//! The client's side of the protocol.

use core::time::Duration;

use bytes::Bytes;
use waveport_adapter_kit::ws::{WsConn, WsMessage, WsRequest};
use waveport_ports::audio::Rate;
use waveport_ports::error::PortError;

use crate::wire::{self, FromClient, FromServer, PATH, PROTOCOL};

/// Something a server says or sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// The audio that follows is that of this response.
    Begins(u32),
    /// A piece of the response that began last.
    Audio(Bytes),
    /// All of this response has been sent.
    Ends(u32),
    /// What is held of this response, and of any before it, and has not
    /// been played is to be discarded.
    Clear(u32),
    /// A line about the call, for a person to read.
    Line(String),
    /// The call is over, and why. Nothing follows.
    Bye(String),
}

/// A connection to a session, from the far end's side.
#[derive(Debug)]
pub struct Client {
    conn: WsConn,
    session: String,
}

impl Client {
    /// Connects to the server at `address`, such as
    /// `ws://127.0.0.1:7880`, says hello with audio at `rate`, and waits
    /// for the session to be there.
    ///
    /// # Errors
    ///
    /// The server cannot be reached, does not speak the protocol, turns
    /// the client away, or takes longer than `timeout` over any of it.
    pub async fn connect(address: &str, rate: Rate, timeout: Duration) -> Result<Self, PortError> {
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
        let hello = FromClient::Hello { rate: rate.hz() };
        conn.send_text(wire::write(&hello)).await?;
        let answer = match tokio::time::timeout(timeout, conn.next()).await {
            Ok(answer) => answer?,
            Err(_) => {
                return Err(PortError::timeout(format!(
                    "the server did not answer a hello within {} ms",
                    timeout.as_millis()
                )));
            }
        };
        let Some(WsMessage::Text(text)) = answer else {
            return Err(PortError::permanent(
                "the server did not answer a hello with words",
            ));
        };
        match wire::read(text.as_str())? {
            FromServer::Ready { session, rate: at } if at == rate.hz() => {
                Ok(Self { conn, session })
            }
            FromServer::Ready { rate: at, .. } => Err(PortError::permanent(format!(
                "the server answered with audio at {at} Hz, which was not asked for"
            ))),
            FromServer::Bye { reason } => Err(PortError::permanent(format!(
                "the server turned the client away: {reason}"
            ))),
            other => Err(PortError::permanent(format!(
                "the server answered a hello with {other:?}"
            ))),
        }
    }

    /// What the server calls the session.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Sends what the microphone heard: samples at the rate that was
    /// named, the low byte first.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn audio(&mut self, samples: Bytes) -> Result<(), PortError> {
        if samples.is_empty() {
            return Ok(());
        }
        self.conn.send_binary(samples).await
    }

    /// Says how many samples of response `run` have been played so far.
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

    async fn say(&mut self, message: &FromClient) -> Result<(), PortError> {
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
        let text = match self.conn.next().await? {
            Some(WsMessage::Text(text)) => text,
            Some(WsMessage::Binary(samples)) => return Ok(Some(Heard::Audio(samples))),
            None => return Ok(None),
        };
        let heard = match wire::read(text.as_str())? {
            FromServer::Mark { run, end: false } => Heard::Begins(run),
            FromServer::Mark { run, end: true } => Heard::Ends(run),
            FromServer::Clear { run } => Heard::Clear(run),
            FromServer::Event { text } => Heard::Line(text),
            FromServer::Bye { reason } => Heard::Bye(reason),
            FromServer::Ready { .. } => {
                return Err(PortError::permanent(
                    "the server said that the session is there a second time",
                ));
            }
        };
        Ok(Some(heard))
    }

    /// Says goodbye and closes the connection.
    pub async fn bye(mut self) {
        let _ = self.say(&FromClient::Bye).await;
        self.conn.close().await;
    }
}
