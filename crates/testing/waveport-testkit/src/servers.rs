//! Servers on this machine that answer as a provider was recorded to.
//!
//! An adapter's tests talk to one of these instead of to its provider.
//! The server follows a script, says what it was sent, and listens on the
//! loopback interface only: nothing leaves the machine, and a test needs
//! no key.
//!
//! These servers keep the machine's time, not a test's: a connection is
//! real, and a clock that a test controls does not move it along.

use core::net::SocketAddr;
use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

/// How an answer that arrives in pieces comes to its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The server says that the answer is complete.
    Complete,
    /// The connection is dropped where the pieces end.
    Cut,
    /// The server says nothing more and keeps the connection open.
    Hang,
}

/// What a server answers one request with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpAnswer {
    /// Success, and a body that arrives in pieces, as an event stream
    /// does.
    Stream {
        /// The pieces, each sent by itself. They may be cut anywhere,
        /// in the middle of a character too.
        pieces: Vec<Vec<u8>>,
        /// How long the server waits before each piece.
        pause: Duration,
        /// What follows the last piece.
        end: End,
    },
    /// Any status, with a body that arrives at once.
    Status {
        /// The status.
        status: u16,
        /// Headers besides the ones every answer has.
        headers: Vec<(String, String)>,
        /// The body.
        body: String,
    },
}

impl HttpAnswer {
    /// An event stream that is sent as it was recorded, a line at a time,
    /// and ends as it should.
    #[must_use]
    pub fn recorded(stream: &str) -> Self {
        Self::Stream {
            pieces: stream
                .split_inclusive('\n')
                .map(|line| line.as_bytes().to_vec())
                .collect(),
            pause: Duration::ZERO,
            end: End::Complete,
        }
    }

    /// The same stream cut into pieces of `size` bytes, wherever that
    /// falls: in the middle of a line, of a word, of a character.
    #[must_use]
    pub fn in_pieces_of(stream: &str, size: usize) -> Self {
        let pieces = stream
            .as_bytes()
            .chunks(size.max(1))
            .map(<[u8]>::to_vec)
            .collect();
        Self::Stream {
            pieces,
            pause: Duration::ZERO,
            end: End::Complete,
        }
    }

    /// A refusal: a status and what the server says about it.
    #[must_use]
    pub fn status(status: u16, body: &str) -> Self {
        Self::Status {
            status,
            headers: Vec::new(),
            body: body.to_owned(),
        }
    }
}

/// A request a server was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpSeen {
    /// The first line: the method and the path.
    pub line: String,
    /// The headers, with their names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: String,
}

impl HttpSeen {
    /// The value of a header, by its name in lower case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }
}

/// An HTTP server on this machine that answers by a script: the first
/// request with the first answer, and so on. Requests beyond the script
/// get the last answer again.
///
/// It stops when it is dropped.
#[derive(Debug)]
pub struct HttpServer {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<HttpSeen>>>,
    serving: JoinHandle<()>,
}

impl HttpServer {
    /// Starts a server.
    ///
    /// # Panics
    ///
    /// The machine gives it no port to listen on.
    #[expect(
        clippy::unwrap_used,
        reason = "a test that cannot be set up has failed"
    )]
    pub async fn start(script: Vec<HttpAnswer>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_server = Arc::clone(&seen);
        let serving = tokio::spawn(async move {
            let mut served = 0_usize;
            while let Ok((mut stream, _)) = listener.accept().await {
                let Some(request) = read_request(&mut stream).await else {
                    continue;
                };
                seen_by_server
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(request);
                let answer = script.get(served).or_else(|| script.last()).cloned();
                served += 1;
                if let Some(answer) = answer {
                    // Each connection is served by itself, so that one
                    // that hangs does not hold up the next.
                    tokio::spawn(async move {
                        let _ = answer_with(&mut stream, answer).await;
                    });
                }
            }
        });
        Self {
            address,
            seen,
            serving,
        }
    }

    /// The address of `path` on this server.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    /// The requests so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<HttpSeen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// Reads one request: its head, and as much body as its `content-length`
/// says. `None` if the connection ended first.
async fn read_request(stream: &mut TcpStream) -> Option<HttpSeen> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    let (head_end, length) = loop {
        let n = stream.read(&mut buffer).await.ok()?;
        if n == 0 {
            return None;
        }
        request.extend_from_slice(buffer.get(..n)?);
        let Some(head_end) = find(&request, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(request.get(..head_end)?).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        break (head_end, length);
    };
    while request.len() < head_end + 4 + length {
        let n = stream.read(&mut buffer).await.ok()?;
        if n == 0 {
            break;
        }
        request.extend_from_slice(buffer.get(..n)?);
    }
    let head = String::from_utf8_lossy(request.get(..head_end)?).into_owned();
    let body = String::from_utf8_lossy(request.get(head_end + 4..)?).into_owned();
    let mut lines = head.lines();
    let line = lines.next()?.to_owned();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    Some(HttpSeen {
        line,
        headers,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn answer_with(stream: &mut TcpStream, answer: HttpAnswer) -> std::io::Result<()> {
    match answer {
        HttpAnswer::Status {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&name);
                head.push_str(": ");
                head.push_str(&value);
                head.push_str("\r\n");
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(body.as_bytes()).await?;
            stream.flush().await
        }
        HttpAnswer::Stream { pieces, pause, end } => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await?;
            stream.flush().await?;
            for piece in pieces {
                if !pause.is_zero() {
                    tokio::time::sleep(pause).await;
                }
                if piece.is_empty() {
                    continue;
                }
                let chunk = format!("{:x}\r\n", piece.len());
                stream.write_all(chunk.as_bytes()).await?;
                stream.write_all(&piece).await?;
                stream.write_all(b"\r\n").await?;
                stream.flush().await?;
            }
            match end {
                End::Complete => {
                    stream.write_all(b"0\r\n\r\n").await?;
                    stream.flush().await
                }
                End::Cut => Ok(()),
                End::Hang => {
                    // Until the other side gives up or the server stops.
                    let mut rest = [0_u8; 64];
                    while matches!(stream.read(&mut rest).await, Ok(n) if n > 0) {}
                    Ok(())
                }
            }
        }
    }
}

/// `text`, with each `{{name}}` in it replaced by the text that `heard`,
/// a JSON object, has under `"name"`. A provider repeats the names its
/// client chose, for a request or for a stream, and a script cannot know
/// them beforehand. A name that `heard` does not have is left as it is.
fn repeating(text: &str, heard: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once("{{") {
        out.push_str(before);
        let Some((name, after)) = after.split_once("}}") else {
            out.push_str("{{");
            rest = after;
            continue;
        };
        let key = format!("\"{name}\":\"");
        let value = heard
            .split_once(&key)
            .and_then(|(_, value)| value.split_once('"'))
            .map(|(value, _)| value);
        if let Some(value) = value {
            out.push_str(value);
        } else {
            out.push_str("{{");
            out.push_str(name);
            out.push_str("}}");
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// How long a server that has closed a connection waits for the other
/// side to answer before it drops the connection.
const CLOSE_WAIT: Duration = Duration::from_millis(500);

/// One thing a WebSocket server does. A script is a list of them, done
/// in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsStep {
    /// Refuses the connection with this status and body, as a provider
    /// refuses a key it does not know. Only as the first step.
    Refuse(u16, String),
    /// Sends a message of text. Each `{{name}}` in it is replaced by the
    /// text that the last message a [`Hear`](Self::Hear) waited for has
    /// under `"name"`: a provider repeats the names its client chose.
    Say(String),
    /// Sends a message of bytes.
    SayBytes(Vec<u8>),
    /// Waits for a message of text that has this in it. Whatever arrives
    /// before it is noted and passed over.
    Hear(String),
    /// Waits for a message of bytes.
    HearBytes,
    /// Waits this long.
    Pause(Duration),
    /// Closes the connection with a code and a reason, and drops it once
    /// the other side has answered, or after half a second.
    Close(u16, String),
    /// Drops the connection without a word.
    Cut,
}

impl WsStep {
    /// A script from its text, which reads as a record of the wire does.
    /// Each line is one step, by how it begins:
    ///
    /// | Line | Step |
    /// |---|---|
    /// | `< text` | say `text` |
    /// | `> text` | wait for a message with `text` in it |
    /// | `>b` | wait for a message of bytes |
    /// | `~ 20` | wait 20 ms |
    /// | `! 1011 reason` | close with a code and a reason |
    /// | `? 401 body` | refuse the connection with a status and a body |
    /// | `x` | drop the connection |
    ///
    /// Empty lines and lines that begin with `#` say nothing.
    ///
    /// # Panics
    ///
    /// A line is none of these.
    #[must_use]
    #[expect(
        clippy::panic,
        reason = "a test whose script cannot be read has failed"
    )]
    pub fn script(text: &str) -> Vec<Self> {
        let number = |text: &str, line: &str| -> (u64, String) {
            let (number, rest) = text.split_once(' ').unwrap_or((text, ""));
            let Ok(number) = number.parse() else {
                panic!("no number in the line {line:?} of a script")
            };
            (number, rest.to_owned())
        };
        let mut steps = Vec::new();
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let step = if let Some(said) = line.strip_prefix("< ") {
                Self::Say(said.to_owned())
            } else if let Some(heard) = line.strip_prefix("> ") {
                Self::Hear(heard.to_owned())
            } else if line == ">b" {
                Self::HearBytes
            } else if let Some(wait) = line.strip_prefix("~ ") {
                Self::Pause(Duration::from_millis(number(wait, line).0))
            } else if let Some(close) = line.strip_prefix("! ") {
                let (code, reason) = number(close, line);
                Self::Close(u16::try_from(code).unwrap_or(1011), reason)
            } else if let Some(refusal) = line.strip_prefix("? ") {
                let (status, body) = number(refusal, line);
                Self::Refuse(u16::try_from(status).unwrap_or(500), body)
            } else if line == "x" {
                Self::Cut
            } else {
                panic!("the line {line:?} of a script is not a step")
            };
            steps.push(step);
        }
        steps
    }
}

/// A message a WebSocket server was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsHeard {
    /// Text.
    Text(String),
    /// Bytes.
    Bytes(Vec<u8>),
}

impl WsHeard {
    /// The text, if it is text.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Bytes(_) => None,
        }
    }
}

/// A connection a WebSocket server was asked for, and what it was sent
/// on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WsSeen {
    /// The path that was asked for, with what follows a `?` in it.
    pub path: String,
    /// The headers of the request, with their names in lower case.
    pub headers: Vec<(String, String)>,
    /// The messages, in order.
    pub messages: Vec<WsHeard>,
}

impl WsSeen {
    /// The value of a header, by its name in lower case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }

    /// The messages of text, in order.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.messages.iter().filter_map(WsHeard::text)
    }
}

/// A WebSocket server on this machine that follows a script: the first
/// connection the first script, and so on. Connections beyond the scripts
/// get the last one again.
///
/// A script that has come to its end without closing the connection
/// leaves it open, and the server goes on noting what it is sent until
/// the other side leaves.
///
/// It stops when it is dropped.
#[derive(Debug)]
pub struct WsServer {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<WsSeen>>>,
    over: watch::Receiver<usize>,
    serving: JoinHandle<()>,
}

impl WsServer {
    /// Starts a server.
    ///
    /// # Panics
    ///
    /// The machine gives it no port to listen on.
    #[expect(
        clippy::unwrap_used,
        reason = "a test that cannot be set up has failed"
    )]
    pub async fn start(scripts: Vec<Vec<WsStep>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_server = Arc::clone(&seen);
        let (ended, over) = watch::channel(0_usize);
        let serving = tokio::spawn(async move {
            let mut served = 0_usize;
            while let Ok((stream, _)) = listener.accept().await {
                let script = scripts.get(served).or_else(|| scripts.last()).cloned();
                let connection = Connection {
                    index: served,
                    seen: Arc::clone(&seen_by_server),
                };
                connection.note(|_| {});
                served += 1;
                let ended = ended.clone();
                tokio::spawn(async move {
                    connection.serve(stream, script.unwrap_or_default()).await;
                    ended.send_modify(|count| *count += 1);
                });
            }
        });
        Self {
            address,
            seen,
            over,
            serving,
        }
    }

    /// The address of `path` on this server.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.address)
    }

    /// The connections so far, in order, with what was sent on each.
    #[must_use]
    pub fn connections(&self) -> Vec<WsSeen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until `count` connections are over, so that all that was
    /// sent on them has been noted, and gives them back.
    pub async fn over(&self, count: usize) -> Vec<WsSeen> {
        let mut over = self.over.clone();
        // An error means the server is gone, and nothing more will come.
        let _ = over.wait_for(|ended| *ended >= count).await;
        self.connections()
    }
}

impl Drop for WsServer {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// One connection to a [`WsServer`], and where what it is sent is noted.
struct Connection {
    index: usize,
    seen: Arc<Mutex<Vec<WsSeen>>>,
}

impl Connection {
    fn note(&self, change: impl FnOnce(&mut WsSeen)) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() <= self.index {
            seen.resize_with(self.index + 1, WsSeen::default);
        }
        if let Some(connection) = seen.get_mut(self.index) {
            change(connection);
        }
    }

    async fn serve(&self, mut stream: TcpStream, script: Vec<WsStep>) {
        if let Some(WsStep::Refuse(status, body)) = script.first() {
            self.refuse(&mut stream, *status, body).await;
            return;
        }

        #[expect(
            clippy::result_large_err,
            reason = "the type of the callback is the library's"
        )]
        let asked = |request: &Request, response: Response| {
            let path = request
                .uri()
                .path_and_query()
                .map_or_else(|| request.uri().path().to_owned(), ToString::to_string);
            let headers = request
                .headers()
                .iter()
                .map(|(name, value)| {
                    let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
                    (name.as_str().to_owned(), value)
                })
                .collect();
            self.note(|seen| {
                seen.path = path;
                seen.headers = headers;
            });
            Ok(response)
        };
        let Ok(mut socket) = tokio_tungstenite::accept_hdr_async(stream, asked).await else {
            return;
        };

        // The last message a step waited for: what is said next may
        // repeat names out of it.
        let mut waited_for = String::new();
        for step in script {
            match step {
                WsStep::Refuse(..) => {}
                WsStep::Say(text) => {
                    let text = repeating(&text, &waited_for);
                    if socket.send(Message::text(text)).await.is_err() {
                        return;
                    }
                }
                WsStep::SayBytes(bytes) => {
                    if socket.send(Message::binary(bytes)).await.is_err() {
                        return;
                    }
                }
                WsStep::Hear(wanted) => loop {
                    match self.heard(&mut socket).await {
                        Some(WsHeard::Text(text)) if text.contains(&wanted) => {
                            waited_for = text;
                            break;
                        }
                        Some(_) => {}
                        None => return,
                    }
                },
                WsStep::HearBytes => loop {
                    match self.heard(&mut socket).await {
                        Some(WsHeard::Bytes(_)) => break,
                        Some(WsHeard::Text(_)) => {}
                        None => return,
                    }
                },
                WsStep::Pause(pause) => tokio::time::sleep(pause).await,
                WsStep::Close(code, reason) => {
                    let frame = CloseFrame {
                        code: code.into(),
                        reason: reason.into(),
                    };
                    let _ = socket.close(Some(frame)).await;
                    // Until the other side has answered the close, but
                    // not for ever: a connection that nobody reads at
                    // the other end never answers.
                    let answered = async { while self.heard(&mut socket).await.is_some() {} };
                    let _ = tokio::time::timeout(CLOSE_WAIT, answered).await;
                    return;
                }
                WsStep::Cut => return,
            }
        }
        while self.heard(&mut socket).await.is_some() {}
    }

    /// Answers a request for a connection with a status, as a server
    /// does that will not have it.
    async fn refuse(&self, stream: &mut TcpStream, status: u16, body: &str) {
        if let Some(request) = read_request(stream).await {
            self.note(|seen| {
                let path = request.line.split(' ').nth(1).unwrap_or_default();
                path.clone_into(&mut seen.path);
                seen.headers = request.headers;
            });
        }
        let refusal = format!(
            "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(refusal.as_bytes()).await;
        let _ = stream.flush().await;
    }

    /// The next message of text or of bytes, noted. `None` when the other
    /// side has left.
    async fn heard(&self, socket: &mut WebSocketStream<TcpStream>) -> Option<WsHeard> {
        loop {
            let heard = match socket.next().await {
                Some(Ok(Message::Text(text))) => WsHeard::Text(text.as_str().to_owned()),
                Some(Ok(Message::Binary(bytes))) => WsHeard::Bytes(bytes.to_vec()),
                Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
                Some(Ok(_)) => continue,
            };
            self.note(|seen| seen.messages.push(heard.clone()));
            return Some(heard);
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    use super::{HttpAnswer, HttpServer, WsHeard, WsServer, WsStep};

    #[test]
    fn a_script_reads_as_a_record_of_the_wire() {
        let script = WsStep::script(
            "# A session.\n\
             > hello\n\
             < {\"type\":\"ready\"}\n\
             \n\
             >b\n\
             ~ 20\n\
             ! 1011 gone away\n\
             x\n",
        );
        assert_eq!(
            script,
            [
                WsStep::Hear("hello".to_owned()),
                WsStep::Say("{\"type\":\"ready\"}".to_owned()),
                WsStep::HearBytes,
                WsStep::Pause(Duration::from_millis(20)),
                WsStep::Close(1011, "gone away".to_owned()),
                WsStep::Cut,
            ]
        );
        assert_eq!(
            WsStep::script("? 401 {\"error\":\"no\"}"),
            [WsStep::Refuse(401, "{\"error\":\"no\"}".to_owned())]
        );
    }

    #[test]
    fn what_is_said_can_repeat_names_out_of_what_was_heard() {
        let heard = r#"{"context_id":"call-7","transcript":"Hi","n":3,"voice":{"id":"v1"}}"#;
        assert_eq!(
            super::repeating(r#"{"done":true,"context_id":"{{context_id}}"}"#, heard),
            r#"{"done":true,"context_id":"call-7"}"#
        );
        assert_eq!(
            super::repeating("{{id}} said {{transcript}}, {{id}}", heard),
            "v1 said Hi, v1"
        );
        // What was not heard, or is not text, stays as it was written,
        // and so does what only looks like a name.
        assert_eq!(
            super::repeating("{{missing}} {{n}}", heard),
            "{{missing}} {{n}}"
        );
        assert_eq!(super::repeating("a {{b", heard), "a {{b");
        assert_eq!(
            super::repeating("{\"a\":{\"b\":1}}", heard),
            "{\"a\":{\"b\":1}}"
        );
    }

    #[tokio::test]
    async fn a_websocket_server_follows_its_script_and_says_what_it_was_sent() {
        let server = WsServer::start(vec![
            WsStep::script(
                "< {\"type\":\"created\"}\n\
                 > \"type\":\"go\"\n\
                 < {\"type\":\"going\"}\n\
                 >b\n\
                 < {\"type\":\"heard\"}\n",
            ),
            WsStep::script("< last words\n! 4002 that is all"),
        ])
        .await;

        let mut request = server
            .url("/v1/live?mode=test")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("x-extra", "Yes".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let mut said = Vec::new();
        socket
            .send(Message::text("{\"type\":\"wait\"}"))
            .await
            .unwrap();
        socket
            .send(Message::text("{\"type\":\"go\"}"))
            .await
            .unwrap();
        socket
            .send(Message::binary(vec![1_u8, 2, 3]))
            .await
            .unwrap();
        for _ in 0..3 {
            let message = socket.next().await.unwrap().unwrap();
            said.push(message.into_text().unwrap().as_str().to_owned());
        }
        assert_eq!(
            said,
            [
                "{\"type\":\"created\"}",
                "{\"type\":\"going\"}",
                "{\"type\":\"heard\"}"
            ]
        );
        // The script is over, and what comes now is still noted.
        socket
            .send(Message::text("{\"type\":\"bye\"}"))
            .await
            .unwrap();
        socket.close(None).await.unwrap();

        let seen = server.over(1).await;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "/v1/live?mode=test");
        assert_eq!(seen[0].header("x-extra"), Some("Yes"));
        assert_eq!(
            seen[0].messages,
            [
                WsHeard::Text("{\"type\":\"wait\"}".to_owned()),
                WsHeard::Text("{\"type\":\"go\"}".to_owned()),
                WsHeard::Bytes(vec![1, 2, 3]),
                WsHeard::Text("{\"type\":\"bye\"}".to_owned()),
            ]
        );
        assert_eq!(seen[0].texts().count(), 3);

        // The second connection gets the second script: a close with a
        // complaint.
        let (mut socket, _) = tokio_tungstenite::connect_async(server.url("/"))
            .await
            .unwrap();
        let first = socket.next().await.unwrap().unwrap();
        assert_eq!(first.into_text().unwrap().as_str(), "last words");
        match socket.next().await.unwrap().unwrap() {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.code, CloseCode::Library(4002));
                assert_eq!(frame.reason.as_str(), "that is all");
            }
            other => panic!("{other:?}"),
        }
        drop(socket);
        assert_eq!(server.over(2).await.len(), 2);
    }

    #[tokio::test]
    async fn a_websocket_server_can_refuse_or_drop_a_connection() {
        let server = WsServer::start(vec![
            WsStep::script("? 401 {\"error\":\"no such key\"}"),
            WsStep::script("< one\nx"),
        ])
        .await;

        let refused = tokio_tungstenite::connect_async(server.url("/v1/live")).await;
        let error = refused.unwrap_err().to_string();
        assert!(error.contains("401"), "{error}");
        let seen = server.over(1).await;
        assert_eq!(seen[0].path, "/v1/live");
        assert!(seen[0].header("sec-websocket-key").is_some());

        let (mut socket, _) = tokio_tungstenite::connect_async(server.url("/"))
            .await
            .unwrap();
        let first = socket.next().await.unwrap().unwrap();
        assert_eq!(first.into_text().unwrap().as_str(), "one");
        // Gone, with no close.
        assert!(matches!(socket.next().await, Some(Err(_)) | None));
    }

    async fn ask(server: &HttpServer, body: &str) -> String {
        let address = server.url("");
        let address = address.strip_prefix("http://").unwrap();
        let mut stream = TcpStream::connect(address).await.unwrap();
        let request = format!(
            "POST /v1/thing HTTP/1.1\r\nHost: {address}\r\nX-Extra: Yes\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        answer
    }

    #[tokio::test]
    async fn a_server_answers_by_its_script_and_says_what_it_was_sent() {
        let server = HttpServer::start(vec![
            HttpAnswer::in_pieces_of("data: one\n\ndata: two\n\n", 7),
            HttpAnswer::status(429, "{\"error\":\"slow down\"}"),
        ])
        .await;

        let first = ask(&server, "{\"n\":1}").await;
        assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "{first}");
        // The body is there, in pieces of seven bytes.
        assert!(first.contains("7\r\ndata: o\r\n"), "{first}");
        assert!(first.ends_with("0\r\n\r\n"), "{first}");

        let second = ask(&server, "{\"n\":2}").await;
        assert!(second.starts_with("HTTP/1.1 429 "), "{second}");
        assert!(second.ends_with("{\"error\":\"slow down\"}"), "{second}");
        // Beyond the script, the last answer again.
        let third = ask(&server, "").await;
        assert!(third.starts_with("HTTP/1.1 429 "), "{third}");

        let seen = server.requests();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].line, "POST /v1/thing HTTP/1.1");
        assert_eq!(seen[0].header("x-extra"), Some("Yes"));
        assert_eq!(seen[0].body, "{\"n\":1}");
        assert_eq!(seen[1].body, "{\"n\":2}");
        assert_eq!(seen[2].body, "");
    }
}
