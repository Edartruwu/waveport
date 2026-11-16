//! Speech synthesis by Cartesia.
//!
//! Stability: experimental
//!
//! One WebSocket carries the speech of one response at a time. The text
//! goes up a piece at a time, as the model writes it, and the provider
//! speaks the pieces as one utterance. The audio comes back in pieces,
//! with the time each word ends at, which is what lets a response that is
//! interrupted be cut at the word the listener heard.
//!
//! **This adapter was written from the provider's documentation as it
//! stood on 15 November 2026, and has not yet been held against the
//! provider.** The tests replay messages in the documented shapes, made
//! up here; none of them was recorded. The tests named `live_*` are the
//! ones that will tell, once there is a key to run them with.
//!
//! What the documentation says, and the adapter is built around:
//!
//! - The provider calls the speech of one response a context. Text is
//!   added to a context by its name, and joined exactly as it is given:
//!   the spaces between the pieces have to be in the pieces.
//! - The provider waits for more text before it speaks, for up to three
//!   seconds, unless it is told not to. The session already hands text
//!   over in whole sentences, so the adapter asks for no wait
//!   ([`Config::max_buffer_delay_ms`]).
//! - A context ends when it is told that no more text follows, and the
//!   provider says `done` when all its audio has been sent.
//! - A context that is cancelled goes on with the piece it is speaking.
//!   So a response that is abandoned takes its connection with it:
//!   closing the connection is the one way to be sure that nothing more
//!   is spoken, and nothing more paid for.
//!
//! Making a connection takes three trips to the provider and back, and a
//! response is waited for by somebody. So a connection whose response
//! came to its end is kept, for a few minutes, and the next response
//! takes it. A kept connection that turns out to be gone is replaced
//! without anybody noticing, as long as the provider had said nothing
//! yet. [`CartesiaTts::warm`] makes a connection before the first
//! response needs one.
//!
//! The adapter starts no task. The receiving half of a stream is the loop
//! that talks to the provider.

mod wire;

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::sync::Arc;

use waveport_adapter_kit::pool::{Line, Pool};
use waveport_adapter_kit::secret::Secret;
use waveport_adapter_kit::stream::{self, Commands, Either, Events, Sender};
use waveport_adapter_kit::ws::{CONNECT_TIMEOUT, WsMessage};
use waveport_ports::audio::RateSet;
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
    /// Where the provider listens, without what follows a `?`:
    /// `wss://api.cartesia.ai/tts/websocket`.
    pub url: String,
    /// The version of the provider's API that is spoken, which the
    /// provider names by a date.
    pub version: String,
    /// The model that speaks.
    pub model: String,
    /// The voice, by the provider's name for it.
    pub voice: String,
    /// The language of the text when a stream is not told one, as the
    /// provider's code for it.
    pub language: String,
    /// The key.
    pub key: Option<Secret>,
    /// How long the provider may wait for more text before it speaks
    /// what it has, in milliseconds, up to 5,000. Nought, the default
    /// here, is for text that is handed over in whole sentences.
    pub max_buffer_delay_ms: u32,
    /// How long making a connection may take.
    pub timeout: Duration,
    /// How long the provider may say nothing once it has all the text of
    /// a response. After that the stream fails.
    pub stall_timeout: Duration,
    /// How long a connection is kept for the next response.
    pub keep: Duration,
    /// The first part of the names this adapter gives its contexts. A
    /// name has to be one the provider has not seen, so the default says
    /// when and where the adapter was made.
    pub context_prefix: String,
}

impl Config {
    /// Cartesia, with a model and a voice.
    #[must_use]
    pub fn new(model: &str, voice: &str, key: Option<Secret>) -> Self {
        let made = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        Self {
            url: "wss://api.cartesia.ai/tts/websocket".to_owned(),
            version: "2026-08-14".to_owned(),
            model: model.to_owned(),
            voice: voice.to_owned(),
            language: "en".to_owned(),
            key,
            max_buffer_delay_ms: 0,
            timeout: CONNECT_TIMEOUT,
            stall_timeout: Duration::from_secs(10),
            keep: Duration::from_secs(180),
            context_prefix: format!("wp-{:x}-{made:x}", std::process::id()),
        }
    }
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

/// A speech synthesiser at Cartesia.
#[derive(Clone)]
pub struct CartesiaTts {
    shared: Arc<Shared>,
}

impl fmt::Debug for CartesiaTts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CartesiaTts")
            .field("url", &self.shared.pool.url())
            .field("model", &self.shared.config.model)
            .field("voice", &self.shared.config.voice)
            .finish_non_exhaustive()
    }
}

impl CartesiaTts {
    /// An adapter. No connection is made until a stream is opened, or
    /// [`warm`](Self::warm) is called.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let url = format!(
            "{}?cartesia_version={}",
            config.url.trim_end_matches('/'),
            config.version
        );
        let key = config.key.clone().map(|key| ("x-api-key", key));
        let pool = Pool::new(url, key, config.timeout, config.keep);
        Self {
            shared: Arc::new(Shared {
                config,
                pool: Arc::new(pool),
                contexts: AtomicU64::new(0),
            }),
        }
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

impl Tts for CartesiaTts {
    fn caps(&self) -> TtsCaps {
        TtsCaps {
            rates: RateSet::ALL,
            input: TtsInput::Incremental,
            alignment: Alignment::Word,
        }
    }

    fn open(&self, config: TtsConfig) -> BoxFuture<'_, Result<TtsStream, PortError>> {
        Box::pin(async move {
            let shared = Arc::clone(&self.shared);
            let line = shared.pool.line().await?;
            let number = shared.contexts.fetch_add(1, Ordering::Relaxed) + 1;
            let context = format!("{}-{number}", shared.config.context_prefix);
            let speaking = Speaking::new(
                &shared.config,
                context,
                config.rate,
                config.language.as_deref(),
            );
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
