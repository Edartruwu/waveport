//! Speech synthesis by ElevenLabs.
//!
//! Stability: experimental
//!
//! One WebSocket carries the speech of one response at a time. The text
//! goes up a piece at a time, as the model writes it, and the provider
//! speaks the pieces as one utterance. The audio comes back in pieces,
//! each with the characters that are spoken in it and when, which is
//! what lets a response that is interrupted be cut at the very character
//! the listener heard.
//!
//! **This adapter was written from the provider's documentation as it
//! stood on 15 November 2026, and has not yet been held against the
//! provider.** The tests replay messages in the documented shapes, made
//! up here; none of them was recorded. The tests named `live_*` are the
//! ones that will tell, once there is a key to run them with.
//!
//! What the documentation says, and the adapter is built around:
//!
//! - The voice, the model and the form of the audio are in the address,
//!   so they are the same for everything said on a connection. An
//!   adapter therefore gives audio at one rate, the one it was set up
//!   with ([`Config::rate`]).
//! - The provider calls the speech of one response a context. A context
//!   is begun with a message whose text is one space, and text is added
//!   to it by its name, joined exactly as it is given.
//! - The provider waits until it has a good deal of text before it
//!   speaks, unless it is told to speak what it has. The session hands
//!   text over in whole sentences, so every piece is sent with that
//!   request.
//! - The times of the characters are counted from where the piece of
//!   audio they come with begins, not from the beginning of the speech.
//!   They are brought to the beginning of the speech here.
//! - A context is closed when no more text follows. The provider goes on
//!   with what it was already asked to speak, and then says that the
//!   context is over.
//! - A context that is closed in the middle goes on with what it is
//!   speaking. So a response that is abandoned takes its connection with
//!   it: closing the connection is the one way to be sure that nothing
//!   more is spoken, and nothing more paid for.
//!
//! What the documentation does not say, and is taken here until the
//! provider has been asked:
//!
//! - How a failure is told on the connection. A message that names an
//!   `error` is taken for one, and what it is worth is read from the
//!   name and the words that come with it.
//! - Whether the provider also says that a context is over at other
//!   times than its end. Read before the provider was told that no more
//!   text follows, it is passed over.
//!
//! A connection whose response came to its end is kept, for a while
//! shorter than the provider keeps one that is sent nothing, and the
//! next response takes it. [`ElevenLabsTts::warm`] makes a connection
//! before the first response needs one.
//!
//! The adapter starts no task. The receiving half of a stream is the loop
//! that talks to the provider.

mod wire;

use core::fmt;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::sync::Arc;

use waveport_adapter_kit::pool::{Line, Pool};
use waveport_adapter_kit::secret::Secret;
use waveport_adapter_kit::stream::{self, Commands, Either, Events, Sender};
use waveport_adapter_kit::ws::{CONNECT_TIMEOUT, WsMessage};
use waveport_ports::audio::{Rate, RateSet};
use waveport_ports::error::{BoxFuture, Full, PortError};
use waveport_ports::tts::{
    Alignment, Tts, TtsCaps, TtsConfig, TtsEvent, TtsInput, TtsStream, TtsTx,
};

use crate::wire::{Heard, Speaking};

/// How many pieces of text may wait for the loop.
const ROOM: usize = 64;

/// How the adapter is set up.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the provider listens, up to where the voice is named:
    /// `wss://api.elevenlabs.io/v1/text-to-speech`.
    pub url: String,
    /// The model that speaks.
    pub model: String,
    /// The voice, by the provider's name for it.
    pub voice: String,
    /// The rate of the audio.
    pub rate: Rate,
    /// A language the model is held to, as the provider's code for it.
    /// Only some models can be held to one, and the others refuse a
    /// connection that names one. `None` leaves the model to tell the
    /// language from the text.
    pub language: Option<String>,
    /// Whether the provider rewrites numbers, dates and the like into
    /// words before it speaks them. `None` leaves it at the provider's
    /// own choice, which differs from model to model.
    pub text_normalization: Option<bool>,
    /// The key.
    pub key: Option<Secret>,
    /// How long the provider keeps a connection that is sent nothing, in
    /// seconds, up to 180.
    pub inactivity_timeout_s: u32,
    /// How long making a connection may take.
    pub timeout: Duration,
    /// How long the provider may say nothing once it has all the text of
    /// a response. After that the stream fails.
    pub stall_timeout: Duration,
    /// How long a connection is kept for the next response.
    pub keep: Duration,
    /// The first part of the names this adapter gives its contexts. The
    /// default says when and where the adapter was made.
    pub context_prefix: String,
}

impl Config {
    /// ElevenLabs, with a model and a voice.
    #[must_use]
    pub fn new(model: &str, voice: &str, key: Option<Secret>) -> Self {
        let made = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        Self {
            url: "wss://api.elevenlabs.io/v1/text-to-speech".to_owned(),
            model: model.to_owned(),
            voice: voice.to_owned(),
            rate: Rate::Hz24000,
            language: None,
            text_normalization: None,
            key,
            inactivity_timeout_s: 180,
            timeout: CONNECT_TIMEOUT,
            stall_timeout: Duration::from_secs(10),
            keep: Duration::from_secs(150),
            context_prefix: format!("wp-{:x}-{made:x}", std::process::id()),
        }
    }
}

/// Whether `value` can stand in an address as it is. What an adapter is
/// set up with is put there, and nothing may be smuggled in with it.
fn plain(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The address connections are made to.
fn address(config: &Config) -> Result<String, PortError> {
    for (what, value) in [
        ("voice", Some(config.voice.as_str())),
        ("model", Some(config.model.as_str())),
        ("language", config.language.as_deref()),
    ] {
        if value.is_some_and(|value| !plain(value)) {
            return Err(PortError::permanent(format!(
                "{:?} cannot be the {what} of speech",
                value.unwrap_or_default()
            )));
        }
    }
    let mut url = format!(
        "{}/{}/multi-stream-input?model_id={}&output_format=pcm_{}&inactivity_timeout={}\
         &sync_alignment=true",
        config.url.trim_end_matches('/'),
        config.voice,
        config.model,
        config.rate.hz(),
        config.inactivity_timeout_s
    );
    if let Some(language) = &config.language {
        let _ = write!(url, "&language_code={language}");
    }
    if let Some(rewrite) = config.text_normalization {
        let how = if rewrite { "on" } else { "off" };
        let _ = write!(url, "&apply_text_normalization={how}");
    }
    Ok(url)
}

/// What the streams of one adapter share.
struct Shared {
    config: Config,
    /// How connections are made, and the ones kept for the responses to
    /// come.
    pool: Arc<Pool>,
    /// How many contexts were named.
    contexts: AtomicU64,
}

/// A speech synthesiser at ElevenLabs.
#[derive(Clone)]
pub struct ElevenLabsTts {
    shared: Arc<Shared>,
}

impl fmt::Debug for ElevenLabsTts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ElevenLabsTts")
            .field("url", &self.shared.pool.url())
            .finish_non_exhaustive()
    }
}

impl ElevenLabsTts {
    /// An adapter. No connection is made until a stream is opened, or
    /// [`warm`](Self::warm) is called.
    ///
    /// # Errors
    ///
    /// The voice, the model or the language cannot stand in an address.
    pub fn new(config: Config) -> Result<Self, PortError> {
        let url = address(&config)?;
        let key = config.key.clone().map(|key| ("xi-api-key", key));
        let pool = Pool::new(url, key, config.timeout, config.keep);
        Ok(Self {
            shared: Arc::new(Shared {
                config,
                pool: Arc::new(pool),
                contexts: AtomicU64::new(0),
            }),
        })
    }

    /// Makes a connection now and keeps it, so that the first response
    /// does not wait for one to be made.
    ///
    /// # Errors
    ///
    /// The connection could not be made. Nothing is lost by that but the
    /// time: a stream makes its own when none is kept.
    pub async fn warm(&self) -> Result<(), PortError> {
        self.shared.pool.warm().await
    }

    /// How many connections are kept for the responses to come.
    #[must_use]
    pub fn kept(&self) -> usize {
        self.shared.pool.kept()
    }
}

impl Tts for ElevenLabsTts {
    fn caps(&self) -> TtsCaps {
        TtsCaps {
            rates: RateSet::of(&[self.shared.config.rate]),
            input: TtsInput::Incremental,
            alignment: Alignment::Char,
        }
    }

    /// The language of a stream is not passed on: a connection is made
    /// for one language or for none, and is kept from one stream to the
    /// next ([`Config::language`]).
    fn open(&self, config: TtsConfig) -> BoxFuture<'_, Result<TtsStream, PortError>> {
        Box::pin(async move {
            let shared = Arc::clone(&self.shared);
            if config.rate != shared.config.rate {
                return Err(PortError::permanent(format!(
                    "audio at {} Hz was asked of an adapter that was set up for {} Hz",
                    config.rate.hz(),
                    shared.config.rate.hz()
                )));
            }
            let line = shared.pool.line().await?;
            let number = shared.contexts.fetch_add(1, Ordering::Relaxed) + 1;
            let context = format!("{}-{number}", shared.config.context_prefix);
            let speaking = Speaking::new(context, config.rate, shared.config.key.clone());
            let stall = shared.config.stall_timeout;
            let (commands, events) = stream::pair(ROOM, move |commands, events| {
                run(line, stall, speaking, commands, events)
            });
            Ok(TtsStream {
                tx: Box::new(Tx { commands }),
                rx: Box::pin(events),
            })
        })
    }
}

/// What the sending half hands to the loop.
enum Command {
    /// The next piece of the text.
    Speak(String),
    /// No more text follows.
    Finish,
}

/// The half of a stream that takes text.
struct Tx {
    commands: Sender<Command>,
}

impl TtsTx for Tx {
    fn try_speak(&mut self, text: &str) -> Result<(), Full> {
        if text.is_empty() {
            return Ok(());
        }
        self.commands.try_send(Command::Speak(text.to_owned()))
    }

    fn try_finish(&mut self) -> Result<(), Full> {
        self.commands.try_send(Command::Finish)
    }
}

/// The loop of one stream: text one way, audio and marks the other.
///
/// A sending half that goes away without saying that the text is over is
/// taken to have said so: nothing more can come, and the provider waits
/// to be told.
async fn run(
    mut line: Line,
    stall: Duration,
    mut speaking: Speaking,
    mut commands: Commands<Command>,
    mut events: Events<TtsEvent>,
) -> Result<(), PortError> {
    let mut out = Vec::new();
    let mut fed = true;
    // The provider makes ready for the speech while the text is still
    // being written.
    line.send(speaking.begin()?).await?;
    loop {
        let next = async {
            if fed {
                stream::first(commands.next(), line.next()).await
            } else {
                Either::Right(line.next().await)
            }
        };
        // Once the provider has all the text it has no reason to be
        // quiet for long.
        let next = if speaking.is_finished() {
            match tokio::time::timeout(stall, next).await {
                Ok(next) => next,
                Err(_) => {
                    return Err(PortError::timeout(format!(
                        "the provider said nothing for {} ms with the speech not over",
                        stall.as_millis()
                    )));
                }
            }
        } else {
            next.await
        };
        match next {
            Either::Left(Some(Command::Speak(text))) => {
                if !speaking.is_finished() {
                    line.send(speaking.speak(&text)?).await?;
                }
            }
            Either::Left(command @ (Some(Command::Finish) | None)) => {
                fed = command.is_some();
                if !speaking.is_finished() {
                    line.send(speaking.finish()?).await?;
                }
            }
            Either::Right(Ok(Some(WsMessage::Text(text)))) => {
                let heard = speaking.heard(text.as_str(), &mut out)?;
                if heard != Heard::NotOurs {
                    line.proven();
                }
                for event in out.drain(..) {
                    events.send(event).await;
                }
                if heard == Heard::Done {
                    // The connection is as good as new for the next
                    // response.
                    line.keep();
                    return Ok(());
                }
            }
            Either::Right(Ok(Some(WsMessage::Binary(_)))) => {}
            // A kept connection that the provider had closed meanwhile.
            Either::Right(Ok(None) | Err(_)) if line.is_unproven() && !speaking.was_heard() => {
                line.renew().await?;
            }
            Either::Right(Ok(None)) => {
                return Err(PortError::transient(
                    "the provider closed the connection with the speech not over",
                ));
            }
            Either::Right(Err(error)) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use waveport_adapter_kit::secret::Secret;
    use waveport_ports::audio::Rate;
    use waveport_ports::error::RetryClass;

    use super::{Config, ElevenLabsTts, address};

    #[test]
    fn what_speech_is_set_up_with_is_in_the_address() {
        let config = Config::new("eleven_flash_v2_5", "21m00Tcm4TlvDq8ikWAM", None);
        assert_eq!(
            address(&config).unwrap(),
            "wss://api.elevenlabs.io/v1/text-to-speech/21m00Tcm4TlvDq8ikWAM/multi-stream-input\
             ?model_id=eleven_flash_v2_5&output_format=pcm_24000&inactivity_timeout=180\
             &sync_alignment=true"
        );
        let config = Config {
            url: "ws://127.0.0.1:9/v1/text-to-speech/".to_owned(),
            rate: Rate::Hz16000,
            language: Some("es".to_owned()),
            text_normalization: Some(true),
            inactivity_timeout_s: 60,
            ..config
        };
        assert_eq!(
            address(&config).unwrap(),
            "ws://127.0.0.1:9/v1/text-to-speech/21m00Tcm4TlvDq8ikWAM/multi-stream-input\
             ?model_id=eleven_flash_v2_5&output_format=pcm_16000&inactivity_timeout=60\
             &sync_alignment=true&language_code=es&apply_text_normalization=on"
        );
    }

    #[test]
    fn nothing_is_smuggled_into_the_address() {
        let good = Config::new("eleven_flash_v2_5", "voice-1", None);
        for config in [
            Config {
                voice: "voice-1/../../v1/user".to_owned(),
                ..good.clone()
            },
            Config {
                voice: String::new(),
                ..good.clone()
            },
            Config {
                model: "eleven_flash_v2_5&enable_logging=true".to_owned(),
                ..good.clone()
            },
            Config {
                language: Some("en us".to_owned()),
                ..good.clone()
            },
        ] {
            let error = ElevenLabsTts::new(config).unwrap_err();
            assert_eq!(error.class, RetryClass::Permanent, "{error}");
        }
    }

    #[test]
    fn the_key_is_not_shown() {
        let key = Secret::new("sk-not-a-key");
        let tts = ElevenLabsTts::new(Config::new("eleven_flash_v2_5", "voice-1", Some(key)));
        assert!(!format!("{:?}", tts.unwrap()).contains("sk-not-a-key"));
    }
}
