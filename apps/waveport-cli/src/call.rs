//! One call: the parts an agent is made of, put together and run.

use core::time::Duration;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use waveport_adapter_kit::secret::Secret;
use waveport_domain::policy::DefaultPolicies;
use waveport_llm_openai_compat::{Config as LlmConfig, Dialect, OpenAiCompat};
use waveport_models_onnx::{Runtime as Models, SileroVad, SmartTurn, TURN_MODEL, VAD_MODEL};
use waveport_ports::control::{SessionControl, SessionInput};
use waveport_ports::journal::SessionId;
use waveport_runtime::compute::Compute;
use waveport_runtime::session::{Ended, Parts, SessionSpec};
use waveport_runtime::shard::{Exit, Runtime, RuntimeConfig};
use waveport_runtime::tools::Registry;
use waveport_store_memory::MemoryJournal;
use waveport_stt_openai::{Config as SttConfig, OpenAiStt};
use waveport_transport_local_audio::{Health, LocalAudio};
use waveport_tts_cartesia::{CartesiaTts, Config as TtsConfig};

use crate::args::{Agent, INSTRUCTIONS, LLM_URL, STT_URL, TTS_URL};
use crate::report::Reporter;
use crate::{Context, Failure, tools};

/// How long a call that was asked to end has to do so before the command
/// stops waiting for it.
const LAST_WORDS: Duration = Duration::from_secs(15);

/// How a call went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It ended, and everything it decided was recorded.
    Ended,
    /// It broke off: its journal failed, or it panicked.
    Broken,
}

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

/// What the agent is told before the call.
fn instructions(agent: &Agent) -> Result<String, Failure> {
    if let Some(file) = &agent.instructions_file {
        return std::fs::read_to_string(file)
            .with(|| format!("the instructions in {} cannot be read", file.display()));
    }
    Ok(agent
        .instructions
        .clone()
        .unwrap_or_else(|| INSTRUCTIONS.to_owned()))
}

/// The models that listen, from the files in `assets`.
fn models(assets: &Path) -> Result<Compute, Failure> {
    let hint = || {
        format!(
            "the models that listen are not in {}: fetch them with `cargo xtask fetch-assets`, \
             or say where they are with --assets",
            assets.display()
        )
    };
    let models = Models::load_from(assets).with(hint)?;
    let speech = SileroVad::load(models, &assets.join(VAD_MODEL)).with(hint)?;
    let turns = SmartTurn::load(models, &assets.join(TURN_MODEL)).with(hint)?;
    Compute::spawn(speech, Box::new(turns), None).with(|| "the models cannot be run".to_owned())
}

/// A name for this call that no other call has had.
fn name() -> SessionId {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    SessionId(now ^ (u128::from(std::process::id()) << 96))
}

/// Runs one call over `transport`, and tells it on the terminal as it
/// goes.
pub(crate) async fn run(agent: &Agent, transport: LocalAudio) -> Result<Outcome, Failure> {
    let health = transport.health();
    let instructions = instructions(agent)?;
    let stt_key = key(&agent.stt_key_env, &agent.stt_url, STT_URL)?;
    let llm_key = key(&agent.llm_key_env, &agent.llm_url, LLM_URL)?;
    let tts_key = key(&agent.tts_key_env, &agent.tts_url, TTS_URL)?;

    let compute = models(&agent.assets)?;
    let stt = OpenAiStt::new(SttConfig {
        url: agent.stt_url.clone(),
        ..SttConfig::new(&agent.stt_model, stt_key)
    });
    // What only OpenAI itself understands is asked of nobody else.
    let llm = if agent.llm_url == LLM_URL {
        LlmConfig::openai(&agent.llm_model, llm_key)
    } else {
        LlmConfig {
            key: llm_key,
            dialect: Dialect::Generic,
            ..LlmConfig::generic(&agent.llm_url, &agent.llm_model)
        }
    };
    let llm = OpenAiCompat::new(llm).with(|| "the language model cannot be reached".to_owned())?;
    let tts = Arc::new(CartesiaTts::new(TtsConfig {
        url: agent.tts_url.clone(),
        language: agent.language.clone(),
        ..TtsConfig::new(&agent.tts_model, &agent.tts_voice, tts_key)
    }));
    // The first answer does not then wait for a connection to be made.
    // If none can be made the call says so itself, at its first answer.
    if let Err(error) = tts.warm().await {
        eprintln!("waveport: speech synthesis could not be reached yet: {error}");
    }

    let runtime =
        Arc::new(Runtime::new(RuntimeConfig::default()).with(|| "no session can run".to_owned())?);
    let mut registry = Registry::new();
    let sessions: Weak<Runtime> = Arc::downgrade(&runtime);
    tools::add(&mut registry, sessions);
    let id = name();
    let mut spec = SessionSpec::new(id, &instructions);
    spec.language = Some(agent.language.clone());
    registry.offer(&mut spec);

    let (lines, mut printed) = mpsc::unbounded_channel();
    let names = spec.tools.iter().map(|tool| tool.name.clone()).collect();
    let parts = Parts {
        transport: Box::new(transport),
        stt: Arc::new(stt),
        llm: Arc::new(llm),
        tts,
        tools: Arc::new(registry),
        journal: Arc::new(MemoryJournal::new()),
        compute: Arc::new(compute),
        sink: Some(Arc::new(Reporter::new(lines, names))),
    };
    let started = runtime
        .start(spec, parts, DefaultPolicies::default())
        .await
        .with(|| "the call could not be started".to_owned())?;
    for shortfall in &started.plan.shortfalls {
        println!("note: {shortfall}");
    }
    if let Some(greeting) = &agent.greeting {
        let text = greeting.clone();
        runtime
            .send(id, SessionInput::Say { text })
            .with(|| "the greeting could not be said".to_owned())?;
    }
    println!("the call has begun");

    let exit = started.exit();
    tokio::pin!(exit);
    let mut asked_to_end = false;
    let exit = loop {
        tokio::select! {
            exit = &mut exit => break exit,
            Some(line) = printed.recv() => println!("{line}"),
            _ = tokio::signal::ctrl_c(), if !asked_to_end => {
                asked_to_end = true;
                eprintln!("waveport: ending the call. Ctrl-C again stops it where it is.");
                let _ = runtime.send(id, SessionInput::Close);
            }
            () = last_words(asked_to_end) => break Exit::Stopped,
        }
    };
    while let Ok(line) = printed.try_recv() {
        println!("{line}");
    }
    tell_health(&health);
    match exit {
        Exit::Ended(Ended::Closed) => Ok(Outcome::Ended),
        Exit::Ended(Ended::Lost(broken)) => {
            eprintln!("waveport: the call broke off: {broken:?}");
            Ok(Outcome::Broken)
        }
        Exit::Panicked => {
            eprintln!("waveport: the call broke off: its session panicked");
            Ok(Outcome::Broken)
        }
        Exit::Stopped => {
            eprintln!("waveport: the call was stopped where it was");
            Ok(Outcome::Broken)
        }
    }
}

/// Waits until a call that was asked to end has had its time, or until
/// it is told again to stop. Never returns for a call nobody has asked to
/// end.
async fn last_words(asked_to_end: bool) {
    if !asked_to_end {
        return core::future::pending().await;
    }
    tokio::select! {
        () = tokio::time::sleep(LAST_WORDS) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Says what went wrong with the sound devices, if anything did.
fn tell_health(health: &Health) {
    if let Some(failure) = health.failure() {
        eprintln!("waveport: {failure}");
    }
    let (stumbles, lost) = (health.stumbles(), health.lost());
    if stumbles > 0 || lost > 0 {
        eprintln!(
            "waveport: the sound devices fell behind {stumbles} times, and {} ms of the \
             microphone were lost",
            lost / 48
        );
    }
}
