//! The providers an agent's definition names, made.
//!
//! This is the one place that knows which adapter a name stands for.

use std::path::Path;
use std::sync::Arc;

use waveport_adapter_kit::secret::Secret;
use waveport_llm_openai_compat::{Config as LlmConfig, Dialect, OpenAiCompat};
use waveport_models_onnx::{Runtime as Models, SileroVad, SmartTurn, TURN_MODEL, VAD_MODEL};
use waveport_ports::stt::Stt;
use waveport_ports::tts::Tts;
use waveport_runtime::compute::Compute;
use waveport_stt_assemblyai::AssemblyAiStt;
use waveport_stt_deepgram::DeepgramStt;
use waveport_stt_openai::OpenAiStt;
use waveport_tts_cartesia::CartesiaTts;
use waveport_tts_elevenlabs::ElevenLabsTts;

use crate::spec::{Chosen, Hears, LLM_URL, LlmSpec, Named, Speaks, SpeechSpec, SttSpec};
use crate::{Context, Failure};

/// The key in the environment variable `variable`, for the provider at
/// `url`. A provider's own address needs one; a server somewhere else
/// may not.
fn key(variable: &str, url: &str, provider: &str) -> Result<Option<Secret>, Failure> {
    match std::env::var(variable) {
        Ok(key) if !key.trim().is_empty() => Ok(Some(Secret::new(key.trim()))),
        _ if url == provider => Err(Failure(format!(
            "the environment variable {variable} holds no key, and {provider} needs one"
        ))),
        _ => Ok(None),
    }
}

/// The key of a provider that a definition chose.
fn key_of(chosen: &Chosen) -> Result<Option<Secret>, Failure> {
    let own = if chosen.own { chosen.url.as_str() } else { "" };
    key(&chosen.key_env, &chosen.url, own)
}

/// The key of the language model.
pub(crate) fn llm_key(spec: &LlmSpec) -> Result<Option<Secret>, Failure> {
    key(&spec.key_env, &spec.url, LLM_URL)
}

/// The language model, asked with `key`.
pub(crate) fn llm(spec: &LlmSpec, key: Option<Secret>) -> Result<OpenAiCompat, Failure> {
    // What only OpenAI itself understands is asked of nobody else.
    let config = if spec.url == LLM_URL {
        LlmConfig::openai(&spec.model, key)
    } else {
        LlmConfig {
            key,
            dialect: Dialect::Generic,
            ..LlmConfig::generic(&spec.url, &spec.model)
        }
    };
    OpenAiCompat::new(config).with(|| "the language model cannot be reached".to_owned())
}

/// The part that recognises speech.
pub(crate) fn recogniser(spec: &SttSpec) -> Result<Arc<dyn Stt>, Failure> {
    let chosen = spec.chosen();
    let key = key_of(&chosen)?;
    let Chosen { url, model, .. } = chosen;
    Ok(match spec.provider {
        Hears::OpenAi => Arc::new(OpenAiStt::new(waveport_stt_openai::Config {
            url,
            ..waveport_stt_openai::Config::new(&model, key)
        })),
        Hears::Deepgram => Arc::new(DeepgramStt::new(waveport_stt_deepgram::Config {
            url,
            ..waveport_stt_deepgram::Config::new(&model, key)
        })),
        Hears::AssemblyAi => Arc::new(AssemblyAiStt::new(waveport_stt_assemblyai::Config {
            url,
            ..waveport_stt_assemblyai::Config::new(&model, key)
        })),
    })
}

/// The part that synthesises speech, with a connection made ahead: the
/// first answer does not then wait for one to be made. If none can be
/// made the session says so itself, at its first answer, and what is
/// returned with the part says so now.
pub(crate) async fn synthesiser(
    speech: &SpeechSpec,
    named: &Named,
) -> Result<(Arc<dyn Tts>, Option<String>), Failure> {
    let chosen = speech.tts.chosen();
    let key = key_of(&chosen)?;
    let Chosen {
        url, model, voice, ..
    } = chosen;
    let (tts, warmed): (Arc<dyn Tts>, _) = match speech.tts.provider {
        Speaks::Cartesia => {
            let tts = Arc::new(CartesiaTts::new(waveport_tts_cartesia::Config {
                url,
                language: speech.language.clone(),
                ..waveport_tts_cartesia::Config::new(&model, &voice, key)
            }));
            let warmed = tts.warm().await;
            (tts, warmed)
        }
        Speaks::ElevenLabs => {
            let config = waveport_tts_elevenlabs::Config {
                url,
                ..waveport_tts_elevenlabs::Config::new(&model, &voice, key)
            };
            let tts = ElevenLabsTts::new(config)
                .with(|| format!("speech cannot be synthesised as {} says", named.by))?;
            let tts = Arc::new(tts);
            let warmed = tts.warm().await;
            (tts, warmed)
        }
    };
    let unreached = warmed
        .err()
        .map(|error| format!("speech synthesis could not be reached yet: {error}"));
    Ok((tts, unreached))
}

/// The models that listen, from the files in `assets`.
pub(crate) fn models(assets: &Path, named: &Named) -> Result<Compute, Failure> {
    let hint = || {
        format!(
            "the models that listen are not in {}: fetch them with `cargo xtask fetch-assets`, \
             or say where they are with {}",
            assets.display(),
            named.assets
        )
    };
    let models = Models::load_from(assets).with(hint)?;
    let speech = SileroVad::load(models, &assets.join(VAD_MODEL)).with(hint)?;
    let turns = SmartTurn::load(models, &assets.join(TURN_MODEL)).with(hint)?;
    Compute::spawn(speech, Box::new(turns), None).with(|| "the models cannot be run".to_owned())
}
