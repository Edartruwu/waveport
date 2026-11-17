//! The far end's side: what a command at a desk or a test connects with.

use core::fmt;
use core::time::Duration;

use bytes::Bytes;
use waveport_adapter_kit::ws::{WsConn, WsMessage, WsRequest};
use waveport_ports::error::PortError;
use waveport_protocol::client::v1::{
    self as client, ByeReason, ErrorCode, FromClient, FromServer, News,
};

use crate::wire::{self, Hello, PATH, PROTOCOL, Welcome};

/// Something a server says or sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// The audio that follows is that of this run.
    Begins {
        /// The number of the run.
        run: u32,
        /// The response it is the audio of.
        generation: u32,
    },
    /// A piece of the run that began last.
    Audio(Bytes),
    /// All of this run has been sent.
    Ends(u32),
    /// What is held of this run, and of any before it, and has not been
    /// played is to be discarded.
    Clear(u32),
    /// Something became true of the session.
    Fact {
        /// Its number.
        seq: u64,
        /// What it is.
        fact: client::Fact,
    },
    /// Something is going on.
    Told(News),
    /// What was held cannot be gone on from. A state follows, and then
    /// the facts from this number on.
    Gap {
        /// The number of the first fact that follows the state.
        resumes: u64,
    },
    /// What the facts up to one of them add up to.
    State(Box<client::State>),
    /// A message of this client was not taken, or the session failed.
    Error {
        /// Why, for a program.
        code: ErrorCode,
        /// Why, for a person.
        message: String,
    },
    /// The server is done with this connection, and why. Nothing
    /// follows.
    Bye(ByeReason),
}

/// Why a client is not connected.
#[derive(Debug)]
pub enum Unconnected {
    /// The server heard the hello and turned the client away.
    Refused {
        /// Why, for a program.
        code: ErrorCode,
        /// Why, for a person.
        message: String,
    },
    /// The server could not be reached, took too long, or did not keep
    /// to the protocol.
    Failed(PortError),
}

impl fmt::Display for Unconnected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { message, .. } => {
                write!(f, "the server turned the client away: {message}")
            }
            Self::Failed(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for Unconnected {}

impl From<PortError> for Unconnected {
    fn from(error: PortError) -> Self {
        Self::Failed(error)
    }
}

/// A connection to a session, from the far end's side.
#[derive(Debug)]
pub struct Client {
    conn: WsConn,
    welcome: Welcome,
}

impl Client {
    /// Connects to the server at `address`, such as
    /// `ws://127.0.0.1:7880`, says `hello`, and waits to be welcomed.
    ///
    /// # Errors
    ///
    /// The server cannot be reached, does not speak the protocol, turns
    /// the client away, or takes longer than `timeout` over any of it.
    pub async fn connect(
        address: &str,
        hello: &Hello,
        timeout: Duration,
    ) -> Result<Self, Unconnected> {
        let address = address.trim_end_matches('/');
        let url = if address.ends_with(PATH) {
            address.to_owned()
        } else {
            format!("{address}{PATH}")
        };
        let request = WsRequest {
            url: &url,
            headers: &[("Sec-WebSocket-Protocol", PROTOCOL)],
            timeout,
        };
        let mut conn = WsConn::connect(request).await?;
        conn.send_text(wire::write(&hello.said())).await?;
        let Ok(answer) = tokio::time::timeout(timeout, conn.next()).await else {
            let error = PortError::timeout(format!(
                "the server did not answer a hello within {} ms",
                timeout.as_millis()
            ));
            return Err(error.into());
        };
        let answer = answer?;
        let Some(WsMessage::Text(text)) = answer else {
            let error = PortError::permanent("the server did not answer a hello with words");
            return Err(error.into());
        };
        let failed = |what: String| Err(Unconnected::Failed(PortError::permanent(what)));
        match wire::read(text.as_str())? {
            FromServer::Welcome {
                v,
                session,
                life,
                resumed,
                resume_token,
                audio,
                features,
                next_seq,
                grace_ms,
            } => {
                if v != client::VERSION {
                    return failed(format!(
                        "the server answered in version {v} of the protocol"
                    ));
                }
                if audio != hello.audio {
                    return failed(format!(
                        "the server answered with {audio:?} for audio, which was not asked for"
                    ));
                }
                let welcome = Welcome {
                    session,
                    life,
                    resumed,
                    resume_token,
                    features,
                    next_seq,
                    grace_ms,
                };
                Ok(Self { conn, welcome })
            }
            FromServer::Error { code, message } => Err(Unconnected::Refused { code, message }),
            FromServer::Bye { reason } => failed(format!(
                "the server answered a hello with goodbye: {reason:?}"
            )),
            _ => failed("the server answered a hello with something else".to_owned()),
        }
    }

    /// What the server said as it took the client.
    #[must_use]
    pub const fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// What the server calls the session.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.welcome.session
    }

    /// Sends what the microphone heard, as the hello said it would be
    /// written: any whole number of samples, the low byte first, or one
    /// Opus packet.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn audio(&mut self, audio: Bytes) -> Result<(), PortError> {
        if audio.is_empty() {
            return Ok(());
        }
        self.conn.send_binary(audio).await
    }

    /// Says how many samples of `run` have been played so far.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn played(&mut self, run: u32, samples: u32) -> Result<(), PortError> {
        self.say(&FromClient::Played { run, samples }).await
    }

    /// Sends something typed in place of speech.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn text(&mut self, text: &str) -> Result<(), PortError> {
        let text = text.to_owned();
        self.say(&FromClient::Text { text }).await
    }

    /// Presses keys of a telephone, one after another.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn dtmf(&mut self, keys: &str) -> Result<(), PortError> {
        let keys = keys.to_owned();
        self.say(&FromClient::Dtmf { keys }).await
    }

    /// Asks that the agent stop speaking.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn interrupt(&mut self) -> Result<(), PortError> {
        self.say(&FromClient::Interrupt).await
    }

    /// Says `message` as it is, whether or not a server takes it.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn say(&mut self, message: &FromClient) -> Result<(), PortError> {
        self.conn.send_text(wire::write(message)).await
    }

    /// The next thing the server says. `None` means the connection was
    /// closed in good order with no goodbye before it.
    ///
    /// # Errors
    ///
    /// The connection dropped, or the server said something that is not
    /// of the protocol.
    pub async fn next(&mut self) -> Result<Option<Heard>, PortError> {
        loop {
            let text = match self.conn.next().await? {
                Some(WsMessage::Text(text)) => text,
                Some(WsMessage::Binary(audio)) => return Ok(Some(Heard::Audio(audio))),
                None => return Ok(None),
            };
            let heard = match wire::read(text.as_str())? {
                FromServer::Mark {
                    run,
                    generation,
                    end: false,
                } => Heard::Begins { run, generation },
                FromServer::Mark { run, end: true, .. } => Heard::Ends(run),
                FromServer::Clear { run } => Heard::Clear(run),
                FromServer::Fact { seq, fact } => Heard::Fact { seq, fact },
                FromServer::Told(news) => Heard::Told(news),
                FromServer::Gap { resumes } => Heard::Gap { resumes },
                FromServer::State(state) => Heard::State(Box::new(state)),
                FromServer::Error { code, message } => Heard::Error { code, message },
                FromServer::Bye { reason } => Heard::Bye(reason),
                FromServer::Welcome { .. } => {
                    return Err(PortError::permanent(
                        "the server welcomed the client a second time",
                    ));
                }
                // A message of a later version: it is passed over.
                FromServer::Unknown => continue,
            };
            return Ok(Some(heard));
        }
    }

    /// Says goodbye, which ends the session, and closes the connection.
    pub async fn bye(mut self) {
        let _ = self.say(&FromClient::Bye).await;
        self.conn.close().await;
    }

    /// Lets go of the connection without a word, as a client does whose
    /// network is gone. The session waits for it to come back.
    pub fn drop_line(self) {
        drop(self.conn);
    }
}
