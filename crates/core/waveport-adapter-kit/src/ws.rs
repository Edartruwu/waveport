//! WebSocket connections.
//!
//! Most streaming providers speak JSON and audio over a WebSocket. This
//! is the client for them: it connects with a time limit, checks the
//! certificate, answers pings, and hands every failure over as a
//! [`PortError`] whose class says whether trying again can help.
//!
//! A transport is on the other side of such a connection: its far end
//! connects to it. [`WsConn::accept`] takes a connection that was made
//! to a listener on this machine, and what follows is the same.
//!
//! A server that answers HTTP on the same port reads the request itself,
//! and has the connection changed into a WebSocket when it is asked to.
//! [`accept_key`] is what it answers with, and [`WsConn::from_upgraded`]
//! takes the connection from there.

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message, Utf8Bytes};
use waveport_ports::error::PortError;

use crate::retry::{class_of_close, error_of_status};
use crate::tls;

/// How long a connection may take to be set up unless told otherwise.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where to connect, and what to say first.
#[derive(Clone, Copy, Debug)]
pub struct WsRequest<'a> {
    /// The address: `wss://…`, or `ws://…` for a server on this machine.
    pub url: &'a str,
    /// Headers to send with the request, the key among them. A value is
    /// not kept or shown.
    pub headers: &'a [(&'a str, &'a str)],
    /// How long setting the connection up may take in all.
    pub timeout: Duration,
}

impl<'a> WsRequest<'a> {
    /// A request to `url` with the usual time limit and no headers.
    #[must_use]
    pub const fn new(url: &'a str) -> Self {
        Self {
            url,
            headers: &[],
            timeout: CONNECT_TIMEOUT,
        }
    }
}

/// The largest message a connection that was accepted takes: audio comes
/// a fraction of a second at a time, and nothing else is long.
const ACCEPTED_MESSAGE_AT_MOST: usize = 1 << 20;

/// The header a client names the protocols it speaks in, and a server the
/// one it chose.
const PROTOCOL_HEADER: &str = "sec-websocket-protocol";

/// What a server says in `Sec-WebSocket-Accept` to a client that said
/// `key` in `Sec-WebSocket-Key`. It shows the client that the server
/// read its request as one for a WebSocket.
#[must_use]
pub fn accept_key(key: &str) -> String {
    derive_accept_key(key.trim().as_bytes())
}

/// Whether `protocol` is among the protocols a client says it speaks:
/// `offered` is what it sent in `Sec-WebSocket-Protocol`, each header of
/// that name a value.
pub fn offers<'a>(offered: impl IntoIterator<Item = &'a str>, protocol: &str) -> bool {
    offered
        .into_iter()
        .flat_map(|value| value.split(','))
        .any(|offer| offer.trim() == protocol)
}

/// How a connection is read that somebody made to this machine.
fn accepted() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(ACCEPTED_MESSAGE_AT_MOST))
        .max_frame_size(Some(ACCEPTED_MESSAGE_AT_MOST))
}

/// The frame a connection is closed with for a reason.
fn close_frame(code: u16, reason: &str) -> CloseFrame {
    let reason: String = reason.chars().take(100).collect();
    CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    }
}

/// What a client asked for as it connected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WsAsked {
    /// The path it asked for, such as `/v0/session`.
    pub path: String,
    /// What followed the path after a `?`, if anything did.
    pub query: Option<String>,
}

/// A message of a connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsMessage {
    /// Text.
    Text(WsText),
    /// Bytes.
    Binary(Bytes),
}

/// The text of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsText(Utf8Bytes);

impl WsText {
    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for WsText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a connection that was taken over from somebody else is read
/// from and written to.
trait Taken: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Taken for T {}

/// A connection: with or without TLS under it, or one that an HTTP
/// server had before.
enum Io {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    Upgraded(Box<dyn Taken>),
}

impl AsyncRead for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_read(cx, buf),
            Self::Tls(io) => Pin::new(io.as_mut()).poll_read(cx, buf),
            Self::Upgraded(io) => Pin::new(io.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_write(cx, buf),
            Self::Tls(io) => Pin::new(io.as_mut()).poll_write(cx, buf),
            Self::Upgraded(io) => Pin::new(io.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_flush(cx),
            Self::Tls(io) => Pin::new(io.as_mut()).poll_flush(cx),
            Self::Upgraded(io) => Pin::new(io.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_shutdown(cx),
            Self::Tls(io) => Pin::new(io.as_mut()).poll_shutdown(cx),
            Self::Upgraded(io) => Pin::new(io.as_mut()).poll_shutdown(cx),
        }
    }
}

/// A WebSocket connection: to a provider, or from whoever connected to
/// a listener on this machine.
pub struct WsConn {
    stream: WebSocketStream<Io>,
}

impl fmt::Debug for WsConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsConn").finish_non_exhaustive()
    }
}

impl WsConn {
    /// Connects.
    ///
    /// # Errors
    ///
    /// The connection could not be set up. The class says why: a server
    /// that cannot be reached is transient, one that takes too long is a
    /// timeout, and one that refuses the request answers for itself, by
    /// its status.
    pub async fn connect(request: WsRequest<'_>) -> Result<Self, PortError> {
        let timeout = request.timeout;
        match tokio::time::timeout(timeout, Self::connect_now(request)).await {
            Ok(connected) => connected,
            Err(_) => Err(PortError::timeout(format!(
                "the connection was not set up within {} ms",
                timeout.as_millis()
            ))),
        }
    }

    async fn connect_now(request: WsRequest<'_>) -> Result<Self, PortError> {
        let mut handshake = request
            .url
            .into_client_request()
            .map_err(|_| PortError::permanent("the address is not a WebSocket address"))?;
        for (name, value) in request.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| PortError::permanent(format!("{name} is not a header name")))?;
            let mut value = HeaderValue::from_str(value).map_err(|_| {
                PortError::permanent(format!("the value of {name} cannot be sent in a header"))
            })?;
            // Kept out of whatever prints the request.
            value.set_sensitive(true);
            handshake.headers_mut().insert(name, value);
        }
        let uri = handshake.uri();
        let secure = match uri.scheme_str() {
            Some("wss") => true,
            Some("ws") => false,
            _ => {
                return Err(PortError::permanent(
                    "the address is not a WebSocket address",
                ));
            }
        };
        let host = uri
            .host()
            .ok_or_else(|| PortError::permanent("the address names no host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });

        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|error| PortError::transient(format!("{host} cannot be reached: {error}")))?;
        // Messages are small and each one matters now.
        let _ = tcp.set_nodelay(true);
        let io = if secure {
            let config = tls::client_config(&["http/1.1"])?;
            let name = ServerName::try_from(host.clone())
                .map_err(|_| PortError::permanent(format!("{host} is not a host name")))?;
            let stream = TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|error| tls_error(&host, &error))?;
            Io::Tls(Box::new(stream))
        } else {
            Io::Plain(tcp)
        };
        let (stream, _) = tokio_tungstenite::client_async(handshake, io)
            .await
            .map_err(|error| connect_error(&error))?;
        Ok(Self { stream })
    }

    /// Takes a connection that was made to a listener on this machine:
    /// reads the request and answers it.
    ///
    /// `protocol` is the one protocol spoken here, if it has a name. A
    /// client that does not name it among the ones it speaks is refused,
    /// and so is one that asks for a path other than `path`. Without a
    /// name nothing is asked of what the client says it speaks, and
    /// nothing is chosen: a carrier's media stream names no protocol.
    ///
    /// # Errors
    ///
    /// What arrived was no WebSocket request, was refused, or took longer
    /// than `timeout`.
    pub async fn accept(
        tcp: TcpStream,
        path: &str,
        protocol: Option<&str>,
        timeout: Duration,
    ) -> Result<(Self, WsAsked), PortError> {
        let _ = tcp.set_nodelay(true);
        let mut asked = WsAsked::default();
        #[expect(
            clippy::result_large_err,
            reason = "the type of the callback is the library's"
        )]
        let check = |request: &Request, mut response: Response| {
            let uri = request.uri();
            uri.path().clone_into(&mut asked.path);
            asked.query = uri.query().map(str::to_owned);
            if uri.path() != path {
                return Err(refusal(StatusCode::NOT_FOUND, "nothing is served here"));
            }
            let Some(protocol) = protocol else {
                return Ok(response);
            };
            let offered = request
                .headers()
                .get_all(PROTOCOL_HEADER)
                .iter()
                .filter_map(|value| value.to_str().ok());
            if !offers(offered, protocol) {
                return Err(refusal(
                    StatusCode::BAD_REQUEST,
                    &format!("the protocol spoken here is {protocol}"),
                ));
            }
            if let Ok(chosen) = HeaderValue::from_str(protocol) {
                response.headers_mut().insert(PROTOCOL_HEADER, chosen);
            }
            Ok(response)
        };
        let accepting = tokio_tungstenite::accept_hdr_async_with_config(
            Io::Plain(tcp),
            check,
            Some(accepted()),
        );
        let stream = match tokio::time::timeout(timeout, accepting).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                return Err(PortError::permanent(format!(
                    "a connection was not taken: {error}"
                )));
            }
            Err(_) => {
                return Err(PortError::timeout(format!(
                    "a connection did not say what it wanted within {} ms",
                    timeout.as_millis()
                )));
            }
        };
        Ok((Self { stream }, asked))
    }

    /// Takes a connection whose request somebody else read and answered:
    /// an HTTP server that was asked to change the connection into a
    /// WebSocket, checked who asked, and said that it does, with
    /// [`accept_key`]. What comes over `io` from here on are the
    /// messages of a WebSocket, and they are taken as from a connection
    /// that was accepted here: none is longer than one may be.
    ///
    /// `io` is the connection as the server hands it over, with whatever
    /// the server had read of it beyond the request.
    pub async fn from_upgraded<S>(io: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let io = Io::Upgraded(Box::new(io));
        let stream = WebSocketStream::from_raw_socket(io, Role::Server, Some(accepted())).await;
        Self { stream }
    }

    /// Sends text.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn send_text(&mut self, text: String) -> Result<(), PortError> {
        self.send(Message::Text(text.into())).await
    }

    /// Sends bytes.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn send_binary(&mut self, bytes: Bytes) -> Result<(), PortError> {
        self.send(Message::Binary(bytes)).await
    }

    /// Sends a ping, for a provider that closes a connection it hears
    /// nothing on.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn ping(&mut self) -> Result<(), PortError> {
        self.send(Message::Ping(Bytes::new())).await
    }

    async fn send(&mut self, message: Message) -> Result<(), PortError> {
        self.stream
            .send(message)
            .await
            .map_err(|error| stream_error(&error))
    }

    /// The next message. `None` means the provider closed the connection
    /// in good order, having nothing more to say.
    ///
    /// # Errors
    ///
    /// The connection dropped, or the provider closed it with a
    /// complaint.
    pub async fn next(&mut self) -> Result<Option<WsMessage>, PortError> {
        loop {
            let Some(message) = self.stream.next().await else {
                return Err(PortError::transient("the connection dropped"));
            };
            match message {
                Ok(Message::Text(text)) => return Ok(Some(WsMessage::Text(WsText(text)))),
                Ok(Message::Binary(bytes)) => return Ok(Some(WsMessage::Binary(bytes))),
                // Pings are answered by the library as it reads on.
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Ok(Message::Close(Some(frame))) => {
                    return match frame.code {
                        CloseCode::Normal | CloseCode::Away => Ok(None),
                        code => {
                            let code = u16::from(code);
                            let reason: String = frame.reason.as_str().chars().take(200).collect();
                            Err(PortError::new(
                                class_of_close(code),
                                format!("the provider closed the connection ({code}): {reason}"),
                            ))
                        }
                    };
                }
                Ok(Message::Close(None))
                | Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => return Ok(None),
                Err(error) => return Err(stream_error(&error)),
            }
        }
    }

    /// Closes the connection in good order. Dropping it closes it too,
    /// without saying goodbye.
    pub async fn close(mut self) {
        let _ = self.stream.close(None).await;
    }

    /// Closes the connection with a complaint: a code of the protocol,
    /// such as 1008 for something that is not allowed, and a few words.
    pub async fn close_with(mut self, code: u16, reason: &str) {
        let _ = self.stream.close(Some(close_frame(code, reason))).await;
    }

    /// Closes as [`close`](Self::close) does, or with a code and a few
    /// words as [`close_with`](Self::close_with) does, and then reads
    /// what the far end still sends until it has closed too, for
    /// `patience` at most.
    ///
    /// This is for a far end that may be sending when it is told that
    /// the connection is over. A connection that is let go of while
    /// something from the far end is unread is reset, and a reset can
    /// take with it what the far end has not read yet: the very last
    /// things this side said.
    pub async fn close_and_wait(mut self, complaint: Option<(u16, &str)>, patience: Duration) {
        let frame = complaint.map(|(code, reason)| close_frame(code, reason));
        let _ = self.stream.close(frame).await;
        let _ = tokio::time::timeout(patience, async {
            while let Ok(Some(_)) = self.next().await {}
        })
        .await;
    }
}

/// The answer to a request that is not taken.
fn refusal(status: StatusCode, why: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(why.to_owned()));
    *response.status_mut() = status;
    response
}

/// A failure of TLS while connecting. A certificate that does not check
/// out will not check out the next time either.
fn tls_error(host: &str, error: &io::Error) -> PortError {
    let message = format!("TLS with {host} failed: {error}");
    if error.kind() == io::ErrorKind::InvalidData {
        PortError::permanent(message)
    } else {
        PortError::transient(message)
    }
}

/// A failure of the WebSocket handshake.
fn connect_error(error: &WsError) -> PortError {
    match error {
        WsError::Http(response) => {
            let status = response.status().as_u16();
            let wait = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok());
            let said = response
                .body()
                .as_deref()
                .map_or_default(String::from_utf8_lossy);
            error_of_status(status, wait, &said)
        }
        WsError::Url(_) | WsError::HttpFormat(_) => {
            PortError::permanent(format!("the request cannot be sent: {error}"))
        }
        other => stream_error(other),
    }
}

/// A failure of a connection that was set up.
fn stream_error(error: &WsError) -> PortError {
    match error {
        // Something that was sent or received is not what the protocol
        // allows in size or in content; the same again would be the same.
        WsError::Capacity(_) | WsError::Utf8(_) | WsError::AttackAttempt => {
            PortError::permanent(format!("the connection was given up: {error}"))
        }
        _ => PortError::transient(format!("the connection dropped: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use bytes::Bytes;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use waveport_ports::error::RetryClass;

    use super::{ACCEPTED_MESSAGE_AT_MOST, WsConn, WsMessage, WsRequest, accept_key, offers};

    /// A server on this machine that serves one connection with `serve`.
    /// Returns its address.
    async fn server<F, Fut>(serve: F) -> String
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve(stream).await;
        });
        format!("ws://{address}/v1/stream?model=test")
    }

    fn text(message: &WsMessage) -> &str {
        match message {
            WsMessage::Text(text) => text.as_str(),
            WsMessage::Binary(_) => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn text_and_bytes_go_both_ways() {
        let url = server(|stream| async move {
            // The key arrives in a header, and the path is the one asked for.
            #[expect(
                clippy::result_large_err,
                reason = "the type of the callback is the library's"
            )]
            let check = |request: &Request, response: Response| {
                assert_eq!(request.headers()["authorization"], "Bearer sk-test");
                assert_eq!(request.uri().path(), "/v1/stream");
                assert_eq!(request.uri().query(), Some("model=test"));
                Ok(response)
            };
            let mut socket = tokio_tungstenite::accept_hdr_async(stream, check)
                .await
                .unwrap();
            // What arrives is sent back, text in capitals.
            while let Some(Ok(message)) = socket.next().await {
                let reply = match message {
                    Message::Text(text) => Message::text(text.as_str().to_uppercase()),
                    Message::Binary(bytes) => Message::Binary(bytes),
                    _ => continue,
                };
                socket.send(reply).await.unwrap();
            }
        })
        .await;

        let request = WsRequest {
            headers: &[("Authorization", "Bearer sk-test")],
            ..WsRequest::new(&url)
        };
        let mut conn = WsConn::connect(request).await.unwrap();
        conn.send_text("{\"type\":\"hello\"}".to_owned())
            .await
            .unwrap();
        assert_eq!(
            text(&conn.next().await.unwrap().unwrap()),
            "{\"TYPE\":\"HELLO\"}"
        );
        conn.send_binary(Bytes::from_static(&[1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(
            conn.next().await.unwrap(),
            Some(WsMessage::Binary(Bytes::from_static(&[1, 2, 3])))
        );
        conn.ping().await.unwrap();
        conn.close().await;
    }

    /// A listener on this machine, and the address a client reaches it
    /// at.
    async fn listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        (listener, format!("ws://{address}"))
    }

    #[tokio::test]
    async fn a_connection_that_is_accepted_speaks_as_one_that_was_made() {
        let (listener, url) = listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let (mut conn, asked) =
                WsConn::accept(tcp, "/v0/session", Some("talk.v0"), Duration::from_secs(2))
                    .await
                    .unwrap();
            assert_eq!(asked.path, "/v0/session");
            assert_eq!(asked.query.as_deref(), Some("rate=24000"));
            assert_eq!(text(&conn.next().await.unwrap().unwrap()), "hello");
            conn.send_binary(Bytes::from_static(&[7, 8])).await.unwrap();
            // The client says goodbye, in good order.
            assert_eq!(conn.next().await.unwrap(), None);
        });
        let url = format!("{url}/v0/session?rate=24000");
        let request = WsRequest {
            // A client may speak several protocols.
            headers: &[("Sec-WebSocket-Protocol", "other.v9, talk.v0")],
            ..WsRequest::new(&url)
        };
        let mut conn = WsConn::connect(request).await.unwrap();
        conn.send_text("hello".to_owned()).await.unwrap();
        assert_eq!(
            conn.next().await.unwrap(),
            Some(WsMessage::Binary(Bytes::from_static(&[7, 8])))
        );
        conn.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_client_that_asks_for_something_else_is_refused() {
        for (path, protocols, class) in [
            // Another protocol, or none.
            ("/v0/session", Some("other.v9"), RetryClass::Permanent),
            ("/v0/session", None, RetryClass::Permanent),
            // Another path.
            ("/elsewhere", Some("talk.v0"), RetryClass::Permanent),
        ] {
            let (listener, url) = listener().await;
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                WsConn::accept(tcp, "/v0/session", Some("talk.v0"), Duration::from_secs(2))
                    .await
                    .unwrap_err()
            });
            let url = format!("{url}{path}");
            let headers: Vec<(&str, &str)> = protocols
                .iter()
                .map(|offer| ("Sec-WebSocket-Protocol", *offer))
                .collect();
            let request = WsRequest {
                headers: &headers,
                ..WsRequest::new(&url)
            };
            let error = WsConn::connect(request).await.unwrap_err();
            assert_eq!(error.class, class, "{path} {protocols:?}: {error}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_client_that_names_no_protocol_is_taken_where_none_is_asked_for() {
        // A carrier's media stream connects as itself, and says nothing
        // of a protocol.
        let (first, url) = listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = first.accept().await.unwrap();
            let (mut conn, asked) = WsConn::accept(tcp, "/stream", None, Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(asked.query.as_deref(), Some("call=7"));
            assert_eq!(text(&conn.next().await.unwrap().unwrap()), "connected");
            assert_eq!(conn.next().await.unwrap(), None);
        });
        let url_of_stream = format!("{url}/stream?call=7");
        let mut conn = WsConn::connect(WsRequest::new(&url_of_stream))
            .await
            .unwrap();
        conn.send_text("connected".to_owned()).await.unwrap();
        conn.close().await;
        server.await.unwrap();

        // The path is still the one that is served.
        let (second, url) = listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = second.accept().await.unwrap();
            WsConn::accept(tcp, "/stream", None, Duration::from_secs(2))
                .await
                .unwrap_err()
        });
        let elsewhere = format!("{url}/elsewhere");
        let error = WsConn::connect(WsRequest::new(&elsewhere))
            .await
            .unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent, "{error}");
        server.await.unwrap();
    }

    #[test]
    fn a_key_is_answered_as_the_protocol_has_it() {
        // The example of RFC 6455, section 1.3.
        let answer = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), answer);
        // As it stands in a header, with what is around it.
        assert_eq!(accept_key(" dGhlIHNhbXBsZSBub25jZQ== "), answer);
        assert_ne!(accept_key("dGhlIHNhbXBsZSBub25jZQ"), answer);
    }

    #[test]
    fn a_protocol_is_offered_in_one_header_or_in_several() {
        assert!(offers(["waveport.v1"], "waveport.v1"));
        assert!(offers(["other.v9, waveport.v1"], "waveport.v1"));
        assert!(offers(["other.v9", " waveport.v1 "], "waveport.v1"));
        assert!(!offers(["waveport.v10", "xwaveport.v1"], "waveport.v1"));
        assert!(!offers([] as [&str; 0], "waveport.v1"));
        assert!(!offers([""], "waveport.v1"));
    }

    /// Reads the head of a request, and returns what it says in the
    /// header `name`.
    async fn head_says(tcp: &mut TcpStream, name: &str) -> String {
        let mut head = Vec::new();
        let mut buffer = [0; 1024];
        while !head.ends_with(b"\r\n\r\n") {
            let n = tcp.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0, "the request ended early");
            head.extend_from_slice(&buffer[..n]);
        }
        let head = String::from_utf8(head).unwrap();
        let prefix = format!("{name}: ");
        head.lines()
            .find_map(|line| {
                let same = line.len() >= prefix.len()
                    && line[..prefix.len()].eq_ignore_ascii_case(&prefix);
                same.then(|| line[prefix.len()..].to_owned())
            })
            .unwrap()
    }

    #[tokio::test]
    async fn a_connection_that_an_http_server_answered_is_taken_from_there() {
        let (listener, url) = listener().await;
        let server = tokio::spawn(async move {
            // What an HTTP server does: it reads the request, and says
            // that the connection is a WebSocket from here on.
            let (mut tcp, _) = listener.accept().await.unwrap();
            let key = head_says(&mut tcp, "sec-websocket-key").await;
            let answer = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                accept_key(&key)
            );
            tcp.write_all(answer.as_bytes()).await.unwrap();
            let mut conn = WsConn::from_upgraded(tcp).await;
            assert_eq!(text(&conn.next().await.unwrap().unwrap()), "hello");
            conn.send_binary(Bytes::from_static(&[7, 8])).await.unwrap();
            conn.send_text("and words".to_owned()).await.unwrap();
            // The client says goodbye, in good order.
            assert_eq!(conn.next().await.unwrap(), None);
        });
        // A client that holds the server to the protocol: it takes the
        // answer only with the key answered as it has to be.
        let url = format!("{url}/v1/client");
        let mut conn = WsConn::connect(WsRequest::new(&url)).await.unwrap();
        conn.send_text("hello".to_owned()).await.unwrap();
        assert_eq!(
            conn.next().await.unwrap(),
            Some(WsMessage::Binary(Bytes::from_static(&[7, 8])))
        );
        assert_eq!(text(&conn.next().await.unwrap().unwrap()), "and words");
        conn.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_connection_that_was_taken_over_takes_no_message_that_is_too_long() {
        use tokio_tungstenite::WebSocketStream;
        use tokio_tungstenite::tungstenite::protocol::Role;

        // Not a socket: whatever can be read and written will do, which
        // is what an HTTP server hands over.
        let (near, far) = tokio::io::duplex(64 * 1024);
        let mut conn = WsConn::from_upgraded(near).await;
        let client = tokio::spawn(async move {
            let mut client = WebSocketStream::from_raw_socket(far, Role::Client, None).await;
            let fits = vec![1; ACCEPTED_MESSAGE_AT_MOST - 1024];
            client.send(Message::Binary(fits.into())).await.unwrap();
            let too_long = vec![2; ACCEPTED_MESSAGE_AT_MOST + 1];
            // Nobody reads all of it.
            let _ = client.send(Message::Binary(too_long.into())).await;
            client
        });
        let Some(WsMessage::Binary(taken)) = conn.next().await.unwrap() else {
            panic!("expected bytes");
        };
        assert_eq!(taken.len(), ACCEPTED_MESSAGE_AT_MOST - 1024);
        let error = conn.next().await.unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent, "{error}");
        drop(conn);
        let _ = client.await.unwrap();
    }

    #[tokio::test]
    async fn a_close_that_waits_lets_the_far_end_finish_what_it_was_saying() {
        let (listener, url) = listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let (mut conn, _) = WsConn::accept(tcp, "/", Some("talk.v0"), Duration::from_secs(2))
                .await
                .unwrap();
            conn.send_text("the last word".to_owned()).await.unwrap();
            let began = std::time::Instant::now();
            conn.close_and_wait(None, Duration::from_secs(5)).await;
            began.elapsed()
        });
        let request = WsRequest {
            headers: &[("Sec-WebSocket-Protocol", "talk.v0")],
            ..WsRequest::new(&url)
        };
        let mut conn = WsConn::connect(request).await.unwrap();
        // The client is in the middle of saying something, and reads
        // nothing meanwhile. All of it is taken, though the server has
        // said that the connection is over.
        tokio::time::sleep(Duration::from_millis(100)).await;
        for _ in 0..50 {
            conn.send_binary(Bytes::from_static(&[0; 1_920]))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // And what the server said last is still there to be read.
        assert_eq!(text(&conn.next().await.unwrap().unwrap()), "the last word");
        assert_eq!(conn.next().await.unwrap(), None);
        drop(conn);
        // The server waited for the client, and no longer.
        let waited = server.await.unwrap();
        assert!(
            (Duration::from_millis(150)..Duration::from_secs(3)).contains(&waited),
            "{waited:?}"
        );
    }

    #[tokio::test]
    async fn a_close_that_waits_does_not_wait_for_ever() {
        let (listener, url) = listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let (conn, _) = WsConn::accept(tcp, "/", Some("talk.v0"), Duration::from_secs(2))
                .await
                .unwrap();
            let began = std::time::Instant::now();
            let complaint = Some((1008, "that is not allowed"));
            conn.close_and_wait(complaint, Duration::from_millis(150))
                .await;
            began.elapsed()
        });
        let request = WsRequest {
            headers: &[("Sec-WebSocket-Protocol", "talk.v0")],
            ..WsRequest::new(&url)
        };
        // A client that neither reads nor closes.
        let conn = WsConn::connect(request).await.unwrap();
        let waited = server.await.unwrap();
        assert!(
            (Duration::from_millis(140)..Duration::from_secs(2)).contains(&waited),
            "{waited:?}"
        );
        drop(conn);
    }

    #[tokio::test]
    async fn a_client_that_says_nothing_is_not_waited_for() {
        let (listener, url) = listener().await;
        let address = url.trim_start_matches("ws://").to_owned();
        let client = tokio::spawn(async move {
            // Connects, and asks for nothing.
            let stream = TcpStream::connect(address).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(stream);
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let error = WsConn::accept(tcp, "/", Some("talk.v0"), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(error.class, RetryClass::Timeout, "{error}");
        client.abort();
    }

    #[tokio::test]
    async fn a_complaint_made_here_reaches_the_other_side() {
        let (listener, url) = listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let (conn, _) = WsConn::accept(tcp, "/", Some("talk.v0"), Duration::from_secs(2))
                .await
                .unwrap();
            conn.close_with(1008, "that is not allowed").await;
        });
        let request = WsRequest {
            headers: &[("Sec-WebSocket-Protocol", "talk.v0")],
            ..WsRequest::new(&url)
        };
        let mut conn = WsConn::connect(request).await.unwrap();
        let error = conn.next().await.unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent, "{error}");
        assert!(error.message.contains("that is not allowed"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_close_in_good_order_ends_the_stream_and_a_complaint_is_an_error() {
        for (code, expected) in [
            (CloseCode::Normal, None),
            (CloseCode::Policy, Some(RetryClass::Permanent)),
            (CloseCode::Error, Some(RetryClass::Transient)),
            (CloseCode::Library(4_001), Some(RetryClass::Transient)),
        ] {
            let url = server(move |stream| async move {
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                socket.send(Message::text("last words")).await.unwrap();
                let frame = CloseFrame {
                    code,
                    reason: "that is all".into(),
                };
                socket.close(Some(frame)).await.unwrap();
                // Read on until the client has answered the close.
                while socket.next().await.is_some() {}
            })
            .await;
            let mut conn = WsConn::connect(WsRequest::new(&url)).await.unwrap();
            assert_eq!(text(&conn.next().await.unwrap().unwrap()), "last words");
            match (conn.next().await, expected) {
                (Ok(None), None) => {}
                (Err(error), Some(class)) => {
                    assert_eq!(error.class, class, "{code:?}");
                    assert!(error.message.contains("that is all"), "{error}");
                }
                (other, _) => panic!("{code:?}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_connection_that_drops_is_worth_another_try() {
        let url = server(|stream| async move {
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Gone without a word.
            drop(socket);
        })
        .await;
        let mut conn = WsConn::connect(WsRequest::new(&url)).await.unwrap();
        let error = conn.next().await.unwrap_err();
        assert_eq!(error.class, RetryClass::Transient, "{error}");
    }

    #[tokio::test]
    async fn a_server_that_refuses_answers_by_its_status() {
        for (status, class) in [
            ("401 Unauthorized", RetryClass::Auth),
            ("429 Too Many Requests", RetryClass::RateLimited),
            ("503 Service Unavailable", RetryClass::Transient),
            ("404 Not Found", RetryClass::Permanent),
        ] {
            let url = server(move |mut stream| async move {
                let mut request = [0; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                let body = "{\"error\":\"no\"}";
                let response = format!(
                    "HTTP/1.1 {status}\r\nretry-after: 2\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            })
            .await;
            let error = WsConn::connect(WsRequest::new(&url)).await.unwrap_err();
            assert_eq!(error.class, class, "{status}: {error}");
            let waits = matches!(class, RetryClass::RateLimited | RetryClass::Transient);
            assert_eq!(error.retry_after.is_some(), waits, "{status}");
        }
    }

    #[tokio::test]
    async fn a_server_that_is_not_there_is_worth_another_try() {
        // A port nothing listens on: bind one, and let it go.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("ws://{address}/");
        // Some systems take a second or two to say that nothing listens;
        // a refusal and a timeout are both worth another try.
        let error = WsConn::connect(WsRequest::new(&url)).await.unwrap_err();
        assert!(error.is_retryable(), "{error}");
    }

    #[tokio::test]
    async fn a_server_that_does_not_answer_times_out() {
        let url = server(|stream| async move {
            // Takes the connection and says nothing.
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(stream);
        })
        .await;
        let request = WsRequest {
            timeout: Duration::from_millis(50),
            ..WsRequest::new(&url)
        };
        let error = WsConn::connect(request).await.unwrap_err();
        assert_eq!(error.class, RetryClass::Timeout, "{error}");
    }

    #[tokio::test]
    async fn what_is_not_a_websocket_address_is_refused_before_anything_is_sent() {
        for url in [
            "https://example.invalid/",
            "not an address",
            "ws:///nowhere",
        ] {
            let error = WsConn::connect(WsRequest::new(url)).await.unwrap_err();
            assert_eq!(error.class, RetryClass::Permanent, "{url}: {error}");
        }
        let request = WsRequest {
            headers: &[("Authorization", "line\nbreak")],
            ..WsRequest::new("ws://127.0.0.1:1/")
        };
        let error = WsConn::connect(request).await.unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
        assert!(!error.message.contains("line"), "the value is not shown");
    }
}
