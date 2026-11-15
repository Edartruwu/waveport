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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

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

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::{HttpAnswer, HttpServer};

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
