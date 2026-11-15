//! Speech recognition by AssemblyAI's streaming transcription.
//!
//! Stability: experimental
//!
//! One WebSocket for one participant. Audio goes up as plain samples, and
//! the provider writes down what it hears turn by turn, with the time of
//! every word: while a turn goes on it says again and again what the
//! turn holds so far, and when it takes the turn to be over it says so
//! once more, for good. It can also be asked to end the turn now, which
//! is what a session does when it has decided for itself that a turn is
//! over.
//!
//! **This adapter was written from the provider's documentation as it
//! stood on 15 November 2026, and has not yet been held against the
//! provider.** The tests replay messages in the documented shapes, made
//! up here; none of them was recorded. The tests named `live_*` are the
//! ones that will tell, once there is a key to run them with.
//!
//! What the documentation says, and the adapter is built around:
//!
//! - What the session asks for is in the address: the model, the rate of
//!   the audio, the language. The provider passes over what it does not
//!   know there, so a model's name written wrongly gets the model the
//!   provider would have chosen itself. Its first message says which
//!   model it began with, and a stream opens only when that is the one
//!   that was asked for.
//! - A key it does not take is not refused as the connection is made:
//!   the connection is made, and then the provider says why not and
//!   closes. A stream opens only when the session has begun.
//! - A message of audio has to hold between 20 ms and a second, and is
//!   best 50 ms or more. Smaller pieces are put together here, which
//!   holds back at most a few hundredths of a second.
//! - Times are milliseconds of the audio that was sent. They are brought
//!   to the session's clock here.
//! - Everything the provider says about a turn replaces what it said
//!   before, until the turn is over.
//! - Asked to end the turn, the provider ends the one it is writing
//!   down. The documentation does not say what it does when it is
//!   writing none, as in silence. So a request is answered here by the
//!   turn that ends next, and by an empty transcript when none has ended
//!   in a short while.
//! - The provider charges for as long as a session is open, and a
//!   session it is not told to end stays open. It is told when no more
//!   audio follows, and what it still writes down is passed on until it
//!   has said that this was all.
//!
//! The adapter starts no task. The receiving half of a stream is the loop
//! that talks to the provider: dropping it closes the connection.

mod wire;

use core::fmt;
use core::fmt::Write as _;
use core::time::Duration;

use bytes::Bytes;
use tokio::time::Instant;
use waveport_adapter_kit::secret::Secret;
use waveport_adapter_kit::stream::{self, Commands, Either, Events, Sender};
use waveport_adapter_kit::ws::{CONNECT_TIMEOUT, WsConn, WsMessage, WsRequest};
use waveport_domain::time::{MediaDur, MediaTime};
use waveport_ports::audio::{AudioRef, Rate, RateSet};
use waveport_ports::error::{BoxFuture, Full, PortError};
use waveport_ports::stt::{
    Stt, SttCaps, SttConfig, SttEvent, SttFinalize, SttStream, SttTiming, SttTx,
};

use crate::wire::{Hearing, Outbox};

/// How many pieces of audio and requests may wait for the loop.
const ROOM: usize = 64;

/// How long the provider has to write down what is left and say that
/// this was all, once it has been told that the session is over.
const LAST_WORDS: Duration = Duration::from_secs(3);

/// How the adapter is set up.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the provider listens, without what follows a `?`:
    /// `wss://streaming.assemblyai.com/v3/ws`.
    pub url: String,
    /// The model that writes down what is said.
    pub model: String,
    /// The key.
    pub key: Option<Secret>,
    /// Whether the language a session names is passed on, for the
    /// provider to lean towards. Only some of its models take one.
    pub steer: bool,
    /// Whether the provider is asked to write a turn out properly at its
    /// end, with capitals and punctuation. Only some of its models have
    /// to be asked; the others do it anyway.
    pub format_turns: bool,
    /// How long a pause has to be for the provider to look whether a turn
    /// is over, in milliseconds. `None` leaves it at the provider's own
    /// choice.
    pub min_turn_silence_ms: Option<u32>,
    /// How long a pause ends a turn whatever was said, in milliseconds.
    /// `None` leaves it at the provider's own choice.
    pub max_turn_silence_ms: Option<u32>,
    /// How long making the connection may take, and then how long the
    /// provider may take to begin the session.
    pub timeout: Duration,
    /// How long the provider may take over a final transcript that was
    /// asked for. After that the request is answered with nothing more.
    pub answer_timeout: Duration,
}

impl Config {
    /// AssemblyAI, with `model`.
    #[must_use]
    pub fn new(model: &str, key: Option<Secret>) -> Self {
        Self {
            url: "wss://streaming.assemblyai.com/v3/ws".to_owned(),
            model: model.to_owned(),
            key,
            steer: true,
            format_turns: false,
            min_turn_silence_ms: None,
            max_turn_silence_ms: None,
            timeout: CONNECT_TIMEOUT,
            answer_timeout: Duration::from_secs(2),
        }
    }
}

/// A speech recogniser at AssemblyAI.
#[derive(Clone)]
pub struct AssemblyAiStt {
    config: Config,
}

impl fmt::Debug for AssemblyAiStt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssemblyAiStt")
            .field("url", &self.config.url)
            .field("model", &self.config.model)
            .finish_non_exhaustive()
    }
}

/// Whether `value` can stand in an address as it is. What a session is
/// set up with is put there, and nothing may be smuggled in with it.
fn plain(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl AssemblyAiStt {
    /// An adapter. No connection is made until a stream is opened.
    #[must_use]
    pub const fn new(config: Config) -> Self {
        Self { config }
    }

    /// How the adapter is set up.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// The address of a stream of audio at `rate` in `language`.
    fn address(&self, rate: Rate, language: Option<&str>) -> Result<String, PortError> {
        let config = &self.config;
        if !plain(&config.model) {
            return Err(PortError::permanent(format!(
                "{:?} cannot be the model of a stream",
                config.model
            )));
        }
        let mut url = format!(
            "{}?sample_rate={}&encoding=pcm_s16le&speech_model={}",
            config.url.trim_end_matches('/'),
            rate.hz(),
            config.model
        );
        if let Some(language) = language.filter(|_| config.steer) {
            // The provider knows a language by its first part: `en`, and
            // not `en-US`. It takes a list of them, written as JSON.
            let code = language
                .split(['-', '_'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !code.chars().all(|c| c.is_ascii_alphabetic()) || code.is_empty() {
                return Err(PortError::permanent(format!(
                    "{language:?} cannot be the language of a stream"
                )));
            }
            let _ = write!(url, "&language_codes=%5B%22{code}%22%5D");
        }
        if config.format_turns {
            url.push_str("&format_turns=true");
        }
        if let Some(pause) = config.min_turn_silence_ms {
            let _ = write!(url, "&min_turn_silence={pause}");
        }
        if let Some(pause) = config.max_turn_silence_ms {
            let _ = write!(url, "&max_turn_silence={pause}");
        }
        Ok(url)
    }

    /// Takes the key out of what a failure says: the provider may repeat
    /// what it was sent.
    fn struck(&self, error: PortError) -> PortError {
        match &self.config.key {
            Some(key) => PortError {
                message: key.strike(&error.message).into(),
                ..error
            },
            None => error,
        }
    }

    /// Makes the connection and waits for the session to begin on it.
    async fn begin(&self, url: &str) -> Result<WsConn, PortError> {
        let key = self.config.key.as_ref().map(Secret::expose);
        let headers: Vec<(&str, &str)> = key.iter().map(|key| ("authorization", *key)).collect();
        let mut conn = WsConn::connect(WsRequest {
            url,
            headers: &headers,
            timeout: self.config.timeout,
        })
        .await?;
        let first = tokio::time::timeout(self.config.timeout, conn.next()).await;
        match first {
            Ok(Ok(Some(WsMessage::Text(text)))) => {
                wire::begun(text.as_str(), &self.config.model)?;
                Ok(conn)
            }
            Ok(Ok(Some(WsMessage::Binary(_)))) => Err(PortError::permanent(
                "the provider did not begin a session with its first message",
            )),
            Ok(Ok(None)) => Err(PortError::transient(
                "the provider closed the connection before a session began",
            )),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(PortError::timeout(
                "the provider did not begin a session in time",
            )),
        }
    }
}

impl Stt for AssemblyAiStt {
    fn caps(&self) -> SttCaps {
        SttCaps {
            rates: RateSet::of(&[Rate::Hz8000, Rate::Hz16000, Rate::Hz24000, Rate::Hz48000]),
            interim: true,
            timing: SttTiming::Words,
            finalize: SttFinalize::Both,
        }
    }

    fn open(&self, config: SttConfig) -> BoxFuture<'_, Result<SttStream, PortError>> {
        Box::pin(async move {
            let url = self.address(config.rate, config.language.as_deref())?;
            let conn = self.begin(&url).await.map_err(|error| self.struck(error))?;
            let patience = self.config.answer_timeout;
            let hearing = Hearing::new(self.config.format_turns);
            let outbox = Outbox::new(config.rate);
            let (commands, events) = stream::pair(ROOM, move |commands, events| {
                run(conn, patience, hearing, outbox, commands, events)
            });
            Ok(SttStream {
                tx: Box::new(Tx { commands }),
                rx: Box::pin(events),
            })
        })
    }
}

/// What the sending half hands to the loop.
enum Command {
    /// A piece of audio: where it is on the media clock, and its samples
    /// as the provider takes them.
    Audio {
        at: MediaTime,
        length: MediaDur,
        samples: Bytes,
    },
    /// A final transcript of everything so far is wanted.
    Finalize,
}

/// The half of a stream that takes audio.
struct Tx {
    commands: Sender<Command>,
}

impl SttTx for Tx {
    fn try_send(&mut self, audio: AudioRef<'_>) -> Result<(), Full> {
        if audio.samples.is_empty() {
            return Ok(());
        }
        if !self.commands.has_room() {
            return Err(Full);
        }
        let samples: Vec<u8> = audio
            .samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        self.commands.try_send(Command::Audio {
            at: audio.at,
            length: audio.length(),
            samples: samples.into(),
        })
    }

    fn try_finalize(&mut self) -> Result<(), Full> {
        self.commands.try_send(Command::Finalize)
    }
}

/// What the loop waits for, whichever comes first.
enum Next {
    Asked(Option<Command>),
    Said(Result<Option<WsMessage>, PortError>),
    /// The provider has been owed an answer for too long.
    Unanswered,
}

/// Sends the audio that is waiting: all that the provider would take
/// with `now`, and otherwise what makes messages of a good size.
async fn send(conn: &mut WsConn, outbox: &mut Outbox, now: bool) -> Result<(), PortError> {
    while let Some(message) = outbox.take(now) {
        conn.send_binary(message).await?;
    }
    Ok(())
}

/// The loop of one stream: audio and requests one way, words the other.
///
/// When the sending half is gone the provider is told that the session
/// is over, and what it still writes down is passed on until it has said
/// that this was all.
async fn run(
    mut conn: WsConn,
    patience: Duration,
    mut hearing: Hearing,
    mut outbox: Outbox,
    mut commands: Commands<Command>,
    mut events: Events<SttEvent>,
) -> Result<(), PortError> {
    let mut out = Vec::new();
    loop {
        let unanswered = hearing.waiting_since().map(|since| since + patience);
        let next = stream::first(commands.next(), conn.next());
        let next = match unanswered {
            Some(at) => tokio::time::timeout_at(at, next).await.ok(),
            None => Some(next.await),
        };
        let next = match next {
            Some(Either::Left(command)) => Next::Asked(command),
            Some(Either::Right(message)) => Next::Said(message),
            None => Next::Unanswered,
        };
        match next {
            Next::Asked(Some(Command::Audio {
                at,
                length,
                samples,
            })) => {
                hearing.sent(at, length);
                outbox.put(&samples);
                send(&mut conn, &mut outbox, false).await?;
            }
            Next::Asked(Some(Command::Finalize)) => {
                // The provider can only end a turn in what it has.
                send(&mut conn, &mut outbox, true).await?;
                if hearing.finalize(Instant::now(), &mut out) {
                    conn.send_text(wire::FORCE_ENDPOINT.to_owned()).await?;
                }
            }
            Next::Asked(None) => break,
            Next::Said(message) => match message? {
                Some(WsMessage::Text(text)) => {
                    hearing.heard(text.as_str(), &mut out)?;
                }
                Some(WsMessage::Binary(_)) => {}
                // The provider has closed in good order, having no more
                // to say. Whoever holds the stream decides what that
                // means to them.
                None => return Ok(()),
            },
            Next::Unanswered => hearing.give_up(&mut out),
        }
        for event in out.drain(..) {
            events.send(event).await;
        }
    }

    // The session is over. The provider writes down what it has left and
    // says that this was all; a provider that does not is not waited for.
    send(&mut conn, &mut outbox, true).await?;
    conn.send_text(wire::TERMINATE.to_owned()).await?;
    let by = Instant::now() + LAST_WORDS;
    while let Ok(Ok(Some(message))) = tokio::time::timeout_at(by, conn.next()).await {
        let mut last = false;
        if let WsMessage::Text(text) = message {
            last = hearing.heard(text.as_str(), &mut out)?;
        }
        for event in out.drain(..) {
            events.send(event).await;
        }
        if last {
            break;
        }
    }
    // Whoever asked last and was not answered is told that this is all.
    hearing.give_up(&mut out);
    for event in out.drain(..) {
        events.send(event).await;
    }
    conn.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use waveport_ports::audio::Rate;
    use waveport_ports::error::RetryClass;

    use super::{AssemblyAiStt, Config};

    #[test]
    fn what_a_stream_is_set_up_with_is_in_its_address() {
        let stt = AssemblyAiStt::new(Config::new("universal-3-6-pro", None));
        // The provider knows a language by its first part, and takes a
        // list: `["en"]`.
        assert_eq!(
            stt.address(Rate::Hz16000, Some("en-US")).unwrap(),
            "wss://streaming.assemblyai.com/v3/ws?sample_rate=16000&encoding=pcm_s16le\
             &speech_model=universal-3-6-pro&language_codes=%5B%22en%22%5D"
        );
        // A language nobody named is left to the provider.
        let stt = AssemblyAiStt::new(Config {
            url: "ws://127.0.0.1:9/v3/ws/".to_owned(),
            format_turns: true,
            min_turn_silence_ms: Some(400),
            max_turn_silence_ms: Some(2_000),
            ..Config::new("universal-streaming-english", None)
        });
        assert_eq!(
            stt.address(Rate::Hz8000, None).unwrap(),
            "ws://127.0.0.1:9/v3/ws?sample_rate=8000&encoding=pcm_s16le\
             &speech_model=universal-streaming-english&format_turns=true&min_turn_silence=400\
             &max_turn_silence=2000"
        );
        // And so is one the model would not take.
        let stt = AssemblyAiStt::new(Config {
            steer: false,
            ..Config::new("universal-streaming-english", None)
        });
        assert_eq!(
            stt.address(Rate::Hz16000, Some("en")).unwrap(),
            "wss://streaming.assemblyai.com/v3/ws?sample_rate=16000&encoding=pcm_s16le\
             &speech_model=universal-streaming-english"
        );
    }

    #[test]
    fn nothing_is_smuggled_into_the_address() {
        let stt = AssemblyAiStt::new(Config::new("universal-3-6-pro", None));
        for language in ["en&token=abc", "", "en us", "-", "e%6E"] {
            let error = stt.address(Rate::Hz16000, Some(language)).unwrap_err();
            assert_eq!(error.class, RetryClass::Permanent, "{error}");
        }
        let stt = AssemblyAiStt::new(Config::new("universal-3-6-pro&redact_pii=false", None));
        assert!(stt.address(Rate::Hz16000, Some("en")).is_err());
    }

    #[test]
    fn the_key_is_not_shown() {
        let key = waveport_adapter_kit::secret::Secret::new("aai-not-a-key");
        let stt = AssemblyAiStt::new(Config::new("universal-3-6-pro", Some(key)));
        assert!(!format!("{stt:?}").contains("aai-not-a-key"));
    }
}
