//! WebSocket connections that are kept from one piece of work to the
//! next.
//!
//! Making a connection takes three trips to a provider and back, and
//! somebody waits for the work it is made for. So an adapter whose
//! provider takes one piece of work after another on a connection keeps
//! the connection when a piece is done, and the next piece takes it.
//!
//! A kept connection may be gone without this side knowing: nobody reads
//! it while it is kept. So a [`Line`] that began on one remembers what it
//! was told to send, until the provider is heard on it. Should the
//! connection turn out to be gone before that, a new one is made in its
//! place and told the same, and nobody notices.

use core::fmt;
use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::time::Instant;
use waveport_ports::error::PortError;

use crate::secret::Secret;
use crate::ws::{WsConn, WsMessage, WsRequest};

/// How many connections are kept.
const KEPT_AT_MOST: usize = 4;

/// How connections to one address are made, and the ones that are kept.
pub struct Pool {
    url: String,
    /// The header that carries the key, by its name.
    key: Option<(&'static str, Secret)>,
    timeout: Duration,
    keep: Duration,
    /// Connections whose work came to its end, with when that was.
    kept: Mutex<Vec<(WsConn, Instant)>>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("url", &self.url)
            .field("kept", &self.kept())
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// Connections to `url`, each made within `timeout` and kept for
    /// `keep` at most. `key` is the name of the header that carries the
    /// key, and the key.
    #[must_use]
    pub const fn new(
        url: String,
        key: Option<(&'static str, Secret)>,
        timeout: Duration,
        keep: Duration,
    ) -> Self {
        Self {
            url,
            key,
            timeout,
            keep,
            kept: Mutex::new(Vec::new()),
        }
    }

    /// Where the connections are made to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Makes a connection.
    ///
    /// # Errors
    ///
    /// The connection could not be made. The key is not in what the
    /// error says, even when the provider repeated it.
    pub async fn connect(&self) -> Result<WsConn, PortError> {
        let headers: Vec<(&str, &str)> = self
            .key
            .iter()
            .map(|(name, key)| (*name, key.expose()))
            .collect();
        let made = WsConn::connect(WsRequest {
            url: &self.url,
            headers: &headers,
            timeout: self.timeout,
        })
        .await;
        made.map_err(|error| match &self.key {
            Some((_, key)) => PortError {
                message: key.strike(&error.message).into(),
                ..error
            },
            None => error,
        })
    }

    /// Makes a connection now and keeps it, so that the first piece of
    /// work does not wait for one to be made.
    ///
    /// # Errors
    ///
    /// The connection could not be made. Nothing is lost by that but the
    /// time: a line makes its own when none is kept.
    pub async fn warm(&self) -> Result<(), PortError> {
        let conn = self.connect().await?;
        self.keep(conn);
        Ok(())
    }

    /// How many connections are kept.
    #[must_use]
    pub fn kept(&self) -> usize {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// A kept connection that is not too old, the newest there is.
    fn take(&self) -> Option<WsConn> {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        kept.retain(|(_, since)| now.duration_since(*since) < self.keep);
        kept.pop().map(|(conn, _)| conn)
    }

    /// Keeps a connection for the next piece of work.
    fn keep(&self, conn: WsConn) {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.len() == KEPT_AT_MOST {
            kept.remove(0);
        }
        kept.push((conn, Instant::now()));
    }

    /// A connection for one piece of work: a kept one when there is one,
    /// and a new one otherwise.
    ///
    /// # Errors
    ///
    /// None is kept, and one could not be made.
    pub async fn line(self: &Arc<Self>) -> Result<Line, PortError> {
        let (conn, unproven) = match self.take() {
            Some(conn) => (conn, true),
            None => (self.connect().await?, false),
        };
        Ok(Line {
            conn,
            unproven,
            sent: Vec::new(),
            pool: Arc::clone(self),
        })
    }
}

/// The connection of one piece of work.
pub struct Line {
    conn: WsConn,
    /// Whether the connection was kept from earlier work, and the
    /// provider has not been heard on it since.
    unproven: bool,
    /// What was sent on a connection that is not proven, to be sent again
    /// on a new one should this one turn out to be gone.
    sent: Vec<String>,
    pool: Arc<Pool>,
}

impl fmt::Debug for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Line")
            .field("url", &self.pool.url)
            .field("unproven", &self.unproven)
            .finish_non_exhaustive()
    }
}

impl Line {
    /// Sends a message.
    ///
    /// # Errors
    ///
    /// The connection is gone, and, if it was a kept one, another could
    /// not be made in its place.
    pub async fn send(&mut self, message: String) -> Result<(), PortError> {
        if !self.unproven {
            return self.conn.send_text(message).await;
        }
        self.sent.push(message.clone());
        if self.conn.send_text(message).await.is_err() {
            self.renew().await?;
        }
        Ok(())
    }

    /// The next message of the provider, as [`WsConn::next`] gives it.
    /// Dropping this before it is ready loses nothing.
    ///
    /// # Errors
    ///
    /// The connection failed, or the provider closed it with a complaint.
    pub async fn next(&mut self) -> Result<Option<WsMessage>, PortError> {
        self.conn.next().await
    }

    /// Whether the connection was kept from earlier work and the provider
    /// has not been heard on it since. Such a connection may be gone
    /// without this side knowing, and one that ends or fails is worth
    /// [`renew`](Self::renew).
    #[must_use]
    pub const fn is_unproven(&self) -> bool {
        self.unproven
    }

    /// Takes a new connection in place of a kept one that is gone, and
    /// says on it what was said on the old one.
    ///
    /// # Errors
    ///
    /// The new connection could not be made, or failed.
    pub async fn renew(&mut self) -> Result<(), PortError> {
        self.conn = self.pool.connect().await?;
        self.unproven = false;
        for message in core::mem::take(&mut self.sent) {
            self.conn.send_text(message).await?;
        }
        Ok(())
    }

    /// The provider was heard on the connection about this work, so the
    /// connection is not gone.
    pub fn proven(&mut self) {
        self.unproven = false;
        self.sent = Vec::new();
    }

    /// The work is done, and the connection is as good as new: it is
    /// kept for the next.
    pub fn keep(self) {
        self.pool.keep(self.conn);
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::sync::Arc;

    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use waveport_ports::error::RetryClass;

    use super::Pool;
    use crate::secret::Secret;
    use crate::ws::WsMessage;

    /// What a server was sent on one of its connections.
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        /// A connection was made, the nth, with this key in its header.
        Made(usize, Option<String>),
        /// The nth connection was sent this.
        Text(usize, String),
    }

    /// A server that answers every message with `heard: ` and the
    /// message, and that drops each of its first `dropped` connections as
    /// soon as it is made.
    async fn server(dropped: usize) -> (String, mpsc::UnboundedReceiver<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/speak", listener.local_addr().unwrap());
        let (tell, seen) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut number = 0;
            while let Ok((tcp, _)) = listener.accept().await {
                number += 1;
                let tell = tell.clone();
                tokio::spawn(async move {
                    let mut key = None;
                    #[expect(
                        clippy::result_large_err,
                        reason = "the type is the WebSocket library's"
                    )]
                    let read = |request: &Request, response: Response| {
                        key = request
                            .headers()
                            .get("x-key")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        Ok(response)
                    };
                    let Ok(mut stream) = tokio_tungstenite::accept_hdr_async(tcp, read).await
                    else {
                        return;
                    };
                    let _ = tell.send(Seen::Made(number, key));
                    if number <= dropped {
                        return;
                    }
                    while let Some(Ok(message)) = stream.next().await {
                        if let Message::Text(text) = message {
                            let _ = tell.send(Seen::Text(number, text.as_str().to_owned()));
                            let answer = format!("heard: {}", text.as_str());
                            let _ = stream.send(Message::Text(answer.into())).await;
                        }
                    }
                });
            }
        });
        (url, seen)
    }

    fn pool(url: String, keep: Duration) -> Arc<Pool> {
        let key = Some(("x-key", Secret::new("sk-not-a-key")));
        Arc::new(Pool::new(url, key, Duration::from_secs(2), keep))
    }

    fn text(message: Option<WsMessage>) -> String {
        match message {
            Some(WsMessage::Text(text)) => text.as_str().to_owned(),
            other => panic!("no text: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_connection_is_kept_for_the_next_piece_of_work() {
        let (url, mut seen) = server(0).await;
        let pool = pool(url, Duration::from_secs(60));
        assert_eq!(pool.kept(), 0);

        let mut line = pool.line().await.unwrap();
        // A connection that was just made is known to be there.
        assert!(!line.is_unproven());
        line.send("one".to_owned()).await.unwrap();
        assert_eq!(text(line.next().await.unwrap()), "heard: one");
        line.keep();
        assert_eq!(pool.kept(), 1);

        // The next piece of work takes it.
        let mut line = pool.line().await.unwrap();
        assert_eq!(pool.kept(), 0);
        assert!(line.is_unproven());
        line.send("two".to_owned()).await.unwrap();
        assert_eq!(text(line.next().await.unwrap()), "heard: two");
        line.proven();
        assert!(!line.is_unproven());
        // A line that is dropped takes its connection with it.
        drop(line);
        assert_eq!(pool.kept(), 0);

        // One connection was made, with the key in its header.
        let key = Some("sk-not-a-key".to_owned());
        assert_eq!(seen.recv().await, Some(Seen::Made(1, key)));
        assert_eq!(seen.recv().await, Some(Seen::Text(1, "one".to_owned())));
        assert_eq!(seen.recv().await, Some(Seen::Text(1, "two".to_owned())));
        // The key is not shown.
        assert!(!format!("{pool:?}").contains("sk-not-a-key"));
    }

    #[tokio::test]
    async fn a_kept_connection_that_is_gone_is_replaced_and_told_the_same() {
        // The server drops its first connection, which is the one kept.
        let (url, mut seen) = server(1).await;
        let pool = pool(url, Duration::from_secs(60));
        pool.warm().await.unwrap();
        assert_eq!(pool.kept(), 1);
        assert_eq!(
            seen.recv().await,
            Some(Seen::Made(1, Some("sk-not-a-key".to_owned())))
        );
        // Time for the drop to arrive: nobody reads a kept connection.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut line = pool.line().await.unwrap();
        assert!(line.is_unproven());
        // Sending may or may not find out that the connection is gone.
        line.send("one".to_owned()).await.unwrap();
        line.send("two".to_owned()).await.unwrap();
        // Reading does, if sending did not.
        let mut next = line.next().await;
        if line.is_unproven() {
            assert!(matches!(next, Ok(None) | Err(_)), "{next:?}");
            line.renew().await.unwrap();
            next = line.next().await;
        }
        assert!(!line.is_unproven());
        assert_eq!(text(next.unwrap()), "heard: one");
        assert_eq!(text(line.next().await.unwrap()), "heard: two");

        // The second connection was told everything, in order.
        assert_eq!(
            seen.recv().await,
            Some(Seen::Made(2, Some("sk-not-a-key".to_owned())))
        );
        assert_eq!(seen.recv().await, Some(Seen::Text(2, "one".to_owned())));
        assert_eq!(seen.recv().await, Some(Seen::Text(2, "two".to_owned())));
    }

    #[tokio::test]
    async fn a_connection_kept_for_too_long_is_not_used() {
        let (url, mut seen) = server(0).await;
        let pool = pool(url, Duration::from_millis(30));
        pool.warm().await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        let line = pool.line().await.unwrap();
        // It is a new one.
        assert!(!line.is_unproven());
        assert!(matches!(seen.recv().await, Some(Seen::Made(1, _))));
        assert!(matches!(seen.recv().await, Some(Seen::Made(2, _))));
    }

    #[tokio::test]
    async fn only_so_many_connections_are_kept() {
        let (url, _seen) = server(0).await;
        let pool = pool(url, Duration::from_secs(60));
        for _ in 0..6 {
            pool.warm().await.unwrap();
        }
        assert_eq!(pool.kept(), 4);
    }

    #[tokio::test]
    async fn a_connection_that_cannot_be_made_is_not_kept() {
        // Nothing listens there.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/speak", listener.local_addr().unwrap());
        drop(listener);
        let pool = pool(url, Duration::from_secs(60));
        let error = pool.warm().await.unwrap_err();
        assert_eq!(error.class, RetryClass::Transient, "{error}");
        assert!(pool.line().await.is_err());
        assert_eq!(pool.kept(), 0);
    }
}
