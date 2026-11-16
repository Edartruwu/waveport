//! Speech recognition by Deepgram's live transcription.
//!
//! Stability: experimental
//!
//! One WebSocket for one participant. Audio goes up as plain samples, the
//! provider writes down what it hears as it goes, with the time of every
//! word, and settles a stretch of it for good whenever the speaker
//! pauses. A final transcript of everything so far can also be asked
//! for, which is what a session does when it has decided that a turn is
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
//! - What the session asks for is in the address: the model, the
//!   language, the rate of the audio. A request the provider will not
//!   serve is refused as the connection is made.
//! - Times are counted in the audio that was sent, not on any clock: a
//!   stretch in which nothing was sent does not move them. They are
//!   brought to the session's clock here.
//! - A transcript that is not final is everything heard in a stretch so
//!   far, and the next one replaces it. A final one closes its stretch,
//!   and the next begins where it ended.
//! - The provider answers a request for a final transcript in most
//!   cases, and says that it does not promise to when it has little left
//!   to write down. So a request is answered here: by the transcript
//!   that reaches the end of the audio, whether or not the provider says
//!   it is the answer, and by an empty one when none has come in a short
//!   while.
//! - A connection that is sent nothing for ten seconds is closed. While
//!   no audio is sent the adapter says every few seconds that the
//!   connection is still wanted.
//! - Audio may be sent a quarter faster than it was spoken, and no
//!   faster.
//! - The provider is told when no more audio follows. It then writes
//!   down what is left and closes the connection itself.
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

use crate::wire::Hearing;

/// How many pieces of audio and requests may wait for the loop.
const ROOM: usize = 64;

/// How long the provider has to write down what is left and close the
/// connection, once it has been told that no more audio follows.
const LAST_WORDS: Duration = Duration::from_secs(3);

/// How the adapter is set up.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the provider listens, without what follows a `?`:
    /// `wss://api.deepgram.com/v1/listen`.
    pub url: String,
    /// The model that writes down what is said.
    pub model: String,
    /// The key.
    pub key: Option<Secret>,
    /// Whether the provider writes numbers, dates and the like as they
    /// are written and not as they are said, with punctuation.
    pub smart_format: bool,
    /// How long a pause has to be for the provider to settle what came
    /// before it, in milliseconds. `None` leaves it at the provider's own
    /// choice.
    pub endpointing_ms: Option<u32>,
    /// How long making the connection may take.
    pub timeout: Duration,
    /// How long the provider may take over a final transcript that was
    /// asked for. After that the request is answered with nothing more:
    /// the provider does not promise an answer.
    pub answer_timeout: Duration,
    /// How long the connection may go without anything sent on it before
    /// the provider is told that it is still wanted. The provider closes
    /// one it has heard nothing on for ten seconds.
    pub keep_alive: Duration,
}

impl Config {
    /// Deepgram, with `model`.
    #[must_use]
    pub fn new(model: &str, key: Option<Secret>) -> Self {
        Self {
            url: "wss://api.deepgram.com/v1/listen".to_owned(),
            model: model.to_owned(),
            key,
            smart_format: true,
            endpointing_ms: None,
            timeout: CONNECT_TIMEOUT,
            answer_timeout: Duration::from_secs(2),
            keep_alive: Duration::from_secs(4),
        }
    }
}

/// A speech recogniser at Deepgram.
#[derive(Clone)]
pub struct DeepgramStt {
    config: Config,
}

impl fmt::Debug for DeepgramStt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeepgramStt")
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

impl DeepgramStt {
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
        // A language that is not known is left to the provider, which
        // then listens for several.
        let language = language.unwrap_or("multi");
        for (what, value) in [("model", config.model.as_str()), ("language", language)] {
            if !plain(value) {
                return Err(PortError::permanent(format!(
                    "{value:?} cannot be the {what} of a stream"
                )));
            }
        }
        let mut url = format!(
            "{}?model={}&language={language}&encoding=linear16&sample_rate={}&channels=1\
             &interim_results=true",
            config.url.trim_end_matches('/'),
            config.model,
            rate.hz()
        );
        if config.smart_format {
            url.push_str("&smart_format=true");
        }
        if let Some(pause) = config.endpointing_ms {
            let _ = write!(url, "&endpointing={pause}");
        }
        Ok(url)
    }
}

impl Stt for DeepgramStt {
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
            let token = self
                .config
                .key
                .as_ref()
                .map(|key| format!("Token {}", key.expose()));
            let headers: Vec<(&str, &str)> = token
                .iter()
                .map(|token| ("authorization", token.as_str()))
                .collect();
            let conn = WsConn::connect(WsRequest {
                url: &url,
                headers: &headers,
                timeout: self.config.timeout,
            })
            .await
            .map_err(|error| match &self.config.key {
                // The provider may repeat what it was sent.
                Some(key) => PortError {
                    message: key.strike(&error.message).into(),
                    ..error
                },
                None => error,
            })?;
            let patience = self.config.answer_timeout;
            let keep_alive = self.config.keep_alive;
            let (commands, events) = stream::pair(ROOM, move |commands, events| {
                run(conn, patience, keep_alive, commands, events)
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
        // The provider takes a message with nothing in it for the end.
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
    /// Nothing has been sent for a while.
    Quiet,
}

/// The loop of one stream: audio and requests one way, words the other.
///
/// When the sending half is gone the provider is told that no more audio
/// follows, and what it still writes down is passed on until it closes
/// the connection.
async fn run(
    mut conn: WsConn,
    patience: Duration,
    keep_alive: Duration,
    mut commands: Commands<Command>,
    mut events: Events<SttEvent>,
) -> Result<(), PortError> {
    let mut hearing = Hearing::new();
    let mut out = Vec::new();
    let mut last_sent = Instant::now();
    loop {
        let unanswered = hearing.waiting_since().map(|since| since + patience);
        let quiet = last_sent + keep_alive;
        let wake = unanswered.map_or(quiet, |at| at.min(quiet));
        let next = stream::first(commands.next(), conn.next());
        let next = match tokio::time::timeout_at(wake, next).await {
            Ok(Either::Left(command)) => Next::Asked(command),
            Ok(Either::Right(message)) => Next::Said(message),
            Err(_) if unanswered.is_some_and(|at| at <= Instant::now()) => Next::Unanswered,
            Err(_) => Next::Quiet,
        };
        match next {
            Next::Asked(Some(Command::Audio {
                at,
                length,
                samples,
            })) => {
                hearing.sent(at, length);
                conn.send_binary(samples).await?;
                last_sent = Instant::now();
            }
            Next::Asked(Some(Command::Finalize)) => {
                if hearing.finalize(Instant::now(), &mut out) {
                    conn.send_text(wire::FINALIZE.to_owned()).await?;
                    last_sent = Instant::now();
                }
            }
            Next::Asked(None) => break,
            Next::Said(message) => match message? {
                Some(WsMessage::Text(text)) => hearing.heard(text.as_str(), &mut out)?,
                Some(WsMessage::Binary(_)) => {}
                // The provider has closed in good order, having no more
                // to say. Whoever holds the stream decides what that
                // means to them.
                None => return Ok(()),
            },
            Next::Unanswered => hearing.give_up(&mut out),
            Next::Quiet => {
                conn.send_text(wire::KEEP_ALIVE.to_owned()).await?;
                last_sent = Instant::now();
            }
        }
        for event in out.drain(..) {
            events.send(event).await;
        }
    }

    // No more audio follows. The provider writes down what it has left
    // and closes; a provider that does not is not waited for.
    conn.send_text(wire::CLOSE_STREAM.to_owned()).await?;
    let by = Instant::now() + LAST_WORDS;
    while let Ok(Ok(Some(message))) = tokio::time::timeout_at(by, conn.next()).await {
        if let WsMessage::Text(text) = message {
            hearing.heard(text.as_str(), &mut out)?;
        }
        for event in out.drain(..) {
            events.send(event).await;
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

    use super::{Config, DeepgramStt};

    #[test]
    fn what_a_stream_is_set_up_with_is_in_its_address() {
        let stt = DeepgramStt::new(Config::new("nova-3", None));
        assert_eq!(
            stt.address(Rate::Hz16000, Some("en-US")).unwrap(),
            "wss://api.deepgram.com/v1/listen?model=nova-3&language=en-US&encoding=linear16\
             &sample_rate=16000&channels=1&interim_results=true&smart_format=true"
        );
        // A language nobody named is left to the provider.
        let stt = DeepgramStt::new(Config {
            url: "ws://127.0.0.1:9/v1/listen/".to_owned(),
            smart_format: false,
            endpointing_ms: Some(300),
            ..Config::new("nova-3", None)
        });
        assert_eq!(
            stt.address(Rate::Hz8000, None).unwrap(),
            "ws://127.0.0.1:9/v1/listen?model=nova-3&language=multi&encoding=linear16\
             &sample_rate=8000&channels=1&interim_results=true&endpointing=300"
        );
    }

    #[test]
    fn nothing_is_smuggled_into_the_address() {
        let stt = DeepgramStt::new(Config::new("nova-3", None));
        for language in ["en&callback=https://example.test", "", "en us"] {
            let error = stt.address(Rate::Hz16000, Some(language)).unwrap_err();
            assert_eq!(error.class, RetryClass::Permanent, "{error}");
        }
        let stt = DeepgramStt::new(Config::new("nova-3&mip_opt_out=false", None));
        assert!(stt.address(Rate::Hz16000, Some("en")).is_err());
    }

    #[test]
    fn the_key_is_not_shown() {
        let key = waveport_adapter_kit::secret::Secret::new("dg-not-a-key");
        let stt = DeepgramStt::new(Config::new("nova-3", Some(key)));
        assert!(!format!("{stt:?}").contains("dg-not-a-key"));
    }
}
