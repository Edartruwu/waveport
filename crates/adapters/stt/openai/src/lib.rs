//! Speech recognition by OpenAI's realtime transcription.
//!
//! Stability: experimental
//!
//! One WebSocket for one participant. Audio goes up as it is heard, the
//! provider writes down what it hears as it goes, and a final transcript
//! comes when it is asked for: the provider is told not to judge for
//! itself where a turn ends, because the session does that.
//!
//! What was learnt from the provider itself, and is held by recorded
//! sessions in this crate's tests:
//!
//! - It takes plain samples at 24,000 Hz and at no other rate.
//! - A session that is refused as it was asked for stays as the provider
//!   makes them: one that cuts the audio where it thinks a speaker has
//!   stopped, and writes nothing down. So a stream is open only once the
//!   provider has said that the session is as asked.
//! - A key the provider does not know is not refused when the connection
//!   is made. The provider takes the connection, says so on it, and
//!   repeats the key as it does. The key is struck from what is passed
//!   on.
//! - What it writes before a final transcript comes a word or so at a
//!   time, about a second behind the audio. Each piece is only what is
//!   new, so the pieces are put together here.
//! - A request for a final transcript of less than a tenth of a second
//!   of audio is refused. Such a request is answered here, with no words.
//! - A final transcript arrives 0.3 to 0.8 s after it is asked for, and
//!   an answer need not come in the order of the requests. The words are
//!   handed on in the order they were asked for.
//! - It gives no times for words. A transcript is of the audio between
//!   two requests.
//! - It works through audio about five times as fast as it is spoken,
//!   and gives up on a request after about six seconds. Half a minute of
//!   audio sent at once is heard. A minute sent at once comes back with
//!   no words in it, and no complaint. Whoever feeds a stream must not
//!   run far ahead of the clock.
//! - A session lasts an hour. The provider then says so and closes, and
//!   the stream ends with a failure that is worth another try.
//! - It charges for the seconds of audio it is sent, silence too.
//!
//! The adapter starts no task. The receiving half of a stream is the loop
//! that talks to the provider: dropping it closes the connection.

mod wire;

use core::fmt;
use core::time::Duration;

use tokio::time::Instant;
use waveport_adapter_kit::pcm::write_base64;
use waveport_adapter_kit::secret::Secret;
use waveport_adapter_kit::stream::{self, Commands, Either, Events, Sender};
use waveport_adapter_kit::ws::{CONNECT_TIMEOUT, WsConn, WsMessage, WsRequest};
use waveport_domain::time::{MediaDur, MediaTime};
use waveport_ports::audio::{AudioRef, Rate, RateSet};
use waveport_ports::error::{BoxFuture, Full, PortError};
use waveport_ports::stt::{
    Stt, SttCaps, SttConfig, SttEvent, SttFinalize, SttStream, SttTiming, SttTx,
};

use crate::wire::{Hearing, Setup};

/// How many pieces of audio and requests may wait for the loop.
const ROOM: usize = 64;

/// How the adapter is set up.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the provider listens, without what follows a `?`:
    /// `wss://api.openai.com/v1/realtime`.
    pub url: String,
    /// The model that writes down what is said.
    pub model: String,
    /// The key.
    pub key: Option<Secret>,
    /// What the audio is of, in a sentence or two, for the model to go
    /// by: the setting, the names it will hear.
    pub prompt: Option<String>,
    /// How long the model waits for more audio before it writes
    /// something down, in the provider's words, from `minimal` to
    /// `xhigh`. Longer is more accurate, and changes when the words that
    /// are not final arrive. It does not change when the final ones do.
    pub delay: Option<String>,
    /// How long making the connection and setting the session up may
    /// take in all.
    pub timeout: Duration,
    /// How long the provider may take over a final transcript that was
    /// asked for. After that the stream fails, and another can be given
    /// the same audio.
    pub answer_timeout: Duration,
}

impl Config {
    /// OpenAI, with `model`.
    #[must_use]
    pub fn new(model: &str, key: Option<Secret>) -> Self {
        Self {
            url: "wss://api.openai.com/v1/realtime".to_owned(),
            model: model.to_owned(),
            key,
            prompt: None,
            delay: None,
            timeout: CONNECT_TIMEOUT,
            answer_timeout: Duration::from_secs(10),
        }
    }
}

/// A speech recogniser at OpenAI.
#[derive(Clone)]
pub struct OpenAiStt {
    config: Config,
    url: String,
}

impl fmt::Debug for OpenAiStt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiStt")
            .field("url", &self.url)
            .field("model", &self.config.model)
            .finish_non_exhaustive()
    }
}

impl OpenAiStt {
    /// An adapter. No connection is made until a stream is opened.
    #[must_use]
    pub fn new(config: Config) -> Self {
        // Without a model in the address: with one, the provider makes a
        // session that converses, and will not make it transcribe.
        let url = format!("{}?intent=transcription", config.url.trim_end_matches('/'));
        Self { config, url }
    }

    async fn connect(&self, language: Option<&str>) -> Result<WsConn, PortError> {
        let asked = wire::setup(&self.config, language)?;
        let key = self.config.key.as_ref();
        let bearer = key.map(|key| format!("Bearer {}", key.expose()));
        let headers: Vec<(&str, &str)> = bearer
            .iter()
            .map(|bearer| ("authorization", bearer.as_str()))
            .collect();
        let by = Instant::now() + self.config.timeout;
        let mut conn = WsConn::connect(WsRequest {
            url: &self.url,
            headers: &headers,
            timeout: self.config.timeout,
        })
        .await?;
        conn.send_text(asked).await?;

        let ready = async {
            loop {
                match conn.next().await? {
                    Some(WsMessage::Text(text)) => match wire::setup_answer(text.as_str(), key) {
                        Setup::Ready => return Ok(()),
                        Setup::Refused(error) => return Err(error),
                        Setup::NotYet => {}
                    },
                    Some(WsMessage::Binary(_)) => {}
                    None => {
                        return Err(PortError::transient(
                            "the provider closed the connection before the session was set up",
                        ));
                    }
                }
            }
        };
        match tokio::time::timeout_at(by, ready).await {
            Ok(Ok(())) => Ok(conn),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(PortError::timeout(format!(
                "the session was not set up within {} ms",
                self.config.timeout.as_millis()
            ))),
        }
    }
}

impl Stt for OpenAiStt {
    fn caps(&self) -> SttCaps {
        SttCaps {
            rates: RateSet::of(&[Rate::Hz24000]),
            interim: true,
            timing: SttTiming::Window,
            finalize: SttFinalize::OnRequest,
        }
    }

    fn open(&self, config: SttConfig) -> BoxFuture<'_, Result<SttStream, PortError>> {
        Box::pin(async move {
            if config.rate.hz() != wire::HZ {
                return Err(PortError::permanent(format!(
                    "the provider takes audio at {} Hz and was to be sent it at {} Hz",
                    wire::HZ,
                    config.rate.hz()
                )));
            }
            let conn = self.connect(config.language.as_deref()).await?;
            let hearing = Hearing::new(self.config.key.clone());
            let patience = self.config.answer_timeout;
            let (commands, events) = stream::pair(ROOM, move |commands, events| {
                run(conn, hearing, patience, commands, events)
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
    /// A piece of audio: where it is on the media clock, and the message
    /// that takes it to the provider.
    Audio {
        at: MediaTime,
        length: MediaDur,
        message: String,
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
        // Asked first: writing audio out as text is not free.
        if !self.commands.has_room() {
            return Err(Full);
        }
        let mut message = String::with_capacity(
            wire::APPEND.len() + audio.samples.len() * 8 / 3 + 4 + wire::APPEND_END.len(),
        );
        message.push_str(wire::APPEND);
        write_base64(audio.samples, &mut message);
        message.push_str(wire::APPEND_END);
        self.commands.try_send(Command::Audio {
            at: audio.at,
            length: audio.length(),
            message,
        })
    }

    fn try_finalize(&mut self) -> Result<(), Full> {
        self.commands.try_send(Command::Finalize)
    }
}

/// The loop of one stream: audio and requests one way, words the other.
///
/// When the sending half is gone the loop waits for the final transcripts
/// that are still owed, and then closes the connection.
async fn run(
    mut conn: WsConn,
    mut hearing: Hearing,
    patience: Duration,
    mut commands: Commands<Command>,
    mut events: Events<SttEvent>,
) -> Result<(), PortError> {
    let mut out = Vec::new();
    let mut fed = true;
    loop {
        let owed_since = hearing.waiting_since();
        if !fed && owed_since.is_none() {
            break;
        }
        let next = async {
            if fed {
                stream::first(commands.next(), conn.next()).await
            } else {
                Either::Right(conn.next().await)
            }
        };
        let next = match owed_since {
            Some(since) => match tokio::time::timeout_at(since + patience, next).await {
                Ok(next) => next,
                Err(_) => {
                    return Err(PortError::timeout(format!(
                        "the provider did not give a final transcript within {} ms of being asked",
                        patience.as_millis()
                    )));
                }
            },
            None => next.await,
        };
        match next {
            Either::Left(Some(Command::Audio {
                at,
                length,
                message,
            })) => {
                hearing.sent(at, length);
                conn.send_text(message).await?;
            }
            Either::Left(Some(Command::Finalize)) => {
                if let Some(commit) = hearing.finalize(Instant::now(), &mut out) {
                    conn.send_text(commit).await?;
                }
            }
            Either::Left(None) => fed = false,
            Either::Right(message) => match message? {
                Some(WsMessage::Text(text)) => hearing.heard(text.as_str(), &mut out)?,
                Some(WsMessage::Binary(_)) => {}
                // The provider has closed in good order, having no more
                // to say. Whoever holds the stream decides what that
                // means to them.
                None => return Ok(()),
            },
        }
        for event in out.drain(..) {
            events.send(event).await;
        }
    }
    conn.close().await;
    Ok(())
}
