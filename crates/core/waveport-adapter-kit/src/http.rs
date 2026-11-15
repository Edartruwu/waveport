//! HTTP requests whose answers arrive as a stream.
//!
//! A language model is asked with one `POST` and answers piece by piece.
//! This client keeps connections open between requests, since setting
//! one up costs as much time as a model takes to start writing, and
//! hands every failure over as a [`PortError`] whose class says whether
//! trying again can help.

use core::fmt;
use core::time::Duration;

use bytes::Bytes;
use waveport_ports::error::PortError;

use crate::retry::error_of_status;
use crate::secret::Secret;
use crate::tls;

/// The most of an error page that is read.
const MAX_ERROR_BODY: usize = 2_048;

/// How a client is set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpSettings {
    /// How long setting a connection up may take.
    pub connect_timeout: Duration,
    /// How long the server may be silent in the middle of an answer.
    pub read_timeout: Duration,
    /// Whether to go straight to the server whatever the environment says
    /// about proxies: for servers on this machine.
    pub direct: bool,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(60),
            direct: false,
        }
    }
}

/// A request.
#[derive(Clone, Copy, Debug)]
pub struct HttpRequest<'a> {
    /// The address.
    pub url: &'a str,
    /// Headers to send, besides the key.
    pub headers: &'a [(&'a str, &'a str)],
    /// The key, sent as a bearer token. It is struck out of whatever the
    /// server says back in an error.
    pub key: Option<&'a Secret>,
    /// The body, which is JSON.
    pub json: &'a [u8],
}

/// An HTTP client. Clones share their connections.
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient").finish_non_exhaustive()
    }
}

impl HttpClient {
    /// A client.
    ///
    /// # Errors
    ///
    /// TLS cannot be set up on this machine.
    pub fn new(settings: HttpSettings) -> Result<Self, PortError> {
        let tls = tls::client_config(&["h2", "http/1.1"])?;
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .connect_timeout(settings.connect_timeout)
            .read_timeout(settings.read_timeout)
            .tcp_nodelay(true)
            .user_agent(concat!("waveport/", env!("CARGO_PKG_VERSION")));
        if settings.direct {
            builder = builder.no_proxy();
        }
        let inner = builder
            .build()
            .map_err(|error| PortError::permanent(format!("no HTTP client: {error}")))?;
        Ok(Self { inner })
    }

    /// Sends a `POST` and waits for the answer to begin.
    ///
    /// # Errors
    ///
    /// The server could not be reached, or answered with something other
    /// than a success. In that case a little of what it said is in the
    /// error, with the key struck out.
    pub async fn post(&self, request: HttpRequest<'_>) -> Result<HttpResponse, PortError> {
        let mut builder = self
            .inner
            .post(request.url)
            .header("content-type", "application/json")
            .body(request.json.to_vec());
        for (name, value) in request.headers {
            builder = builder.header(*name, *value);
        }
        if let Some(key) = request.key {
            builder = builder.bearer_auth(key.expose());
        }
        let mut response = builder.send().await.map_err(send_error)?;
        let status = response.status();
        if status.is_success() {
            return Ok(HttpResponse { inner: response });
        }

        let wait = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut said = Vec::new();
        while said.len() < MAX_ERROR_BODY {
            match response.chunk().await {
                Ok(Some(chunk)) => said.extend_from_slice(&chunk),
                _ => break,
            }
        }
        let said = String::from_utf8_lossy(&said);
        let said = match request.key {
            Some(key) => key.strike(&said),
            None => said.into_owned(),
        };
        Err(error_of_status(status.as_u16(), wait.as_deref(), &said))
    }
}

/// An answer that is still arriving.
pub struct HttpResponse {
    inner: reqwest::Response,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.inner.status().as_u16())
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    /// The value of a header, when it is there and is text.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner.headers().get(name)?.to_str().ok()
    }

    /// The next piece of the body. `None` means the answer is complete.
    ///
    /// # Errors
    ///
    /// The connection dropped, or the server was silent for too long.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, PortError> {
        self.inner.chunk().await.map_err(send_error)
    }
}

/// A failure to send a request or to read its answer.
fn send_error(error: reqwest::Error) -> PortError {
    // The address can hold a key, and is of no use in a log.
    let error = error.without_url();
    let mut message = error.to_string();
    let mut cause = core::error::Error::source(&error);
    while let Some(inner) = cause {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        cause = inner.source();
    }
    if error.is_timeout() {
        PortError::timeout(message)
    } else if error.is_builder() {
        PortError::permanent(message)
    } else {
        PortError::transient(message)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use waveport_ports::error::RetryClass;

    use super::{HttpClient, HttpRequest, HttpSettings};
    use crate::secret::Secret;
    use crate::sse::SseParser;

    fn client() -> HttpClient {
        HttpClient::new(HttpSettings {
            read_timeout: Duration::from_millis(300),
            direct: true,
            ..HttpSettings::default()
        })
        .unwrap()
    }

    /// Reads one request: its head, and as much body as its
    /// `content-length` says.
    async fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let n = stream.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..n]);
            let text = String::from_utf8_lossy(&request);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let length: usize = text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")?
                            .parse()
                            .ok()
                    })
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + length || n == 0 {
                    return text.into_owned();
                }
            }
        }
    }

    /// A server on this machine that serves each connection with
    /// `serve`. Returns its address.
    async fn server<F, Fut>(serve: F) -> String
    where
        F: Fn(TcpStream, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let request = read_request(&mut stream).await;
                serve(stream, request).await;
            }
        });
        format!("http://{address}/v1/chat/completions")
    }

    #[tokio::test]
    async fn an_answer_arrives_piece_by_piece() {
        let url = server(|mut stream, request| async move {
            assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
            let lower = request.to_ascii_lowercase();
            assert!(
                lower.contains("authorization: bearer sk-test\r\n"),
                "{request}"
            );
            assert!(lower.contains("content-type: application/json\r\n"));
            assert!(lower.contains("x-extra: yes\r\n"));
            assert!(lower.contains("user-agent: waveport/"));
            assert!(request.ends_with("{\"stream\":true}"));

            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      x-request-id: req_1\r\ntransfer-encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            // Three events, cut where no event ends.
            for piece in [
                "data: {\"n\":",
                "1}\n\ndata: {\"n\":2}\n",
                "\ndata: [DONE]\n\n",
            ] {
                let chunk = format!("{:x}\r\n{piece}\r\n", piece.len());
                stream.write_all(chunk.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
        })
        .await;

        let key = Secret::new("sk-test");
        let mut response = client()
            .post(HttpRequest {
                url: &url,
                headers: &[("x-extra", "yes")],
                key: Some(&key),
                json: b"{\"stream\":true}",
            })
            .await
            .unwrap();
        assert_eq!(response.header("x-request-id"), Some("req_1"));
        assert_eq!(response.header("x-none"), None);

        let mut parser = SseParser::new();
        let mut events = Vec::new();
        while let Some(chunk) = response.chunk().await.unwrap() {
            parser
                .push(&chunk, |event| events.push(event.data.to_owned()))
                .unwrap();
        }
        assert_eq!(events, ["{\"n\":1}", "{\"n\":2}", "[DONE]"]);
    }

    #[tokio::test]
    async fn a_refusal_carries_its_status_its_wait_and_no_key() {
        let url = server(|mut stream, request| async move {
            let (status, extra) = if request.contains("\"case\":429") {
                ("429 Too Many Requests", "retry-after: 7\r\n")
            } else if request.contains("\"case\":401") {
                ("401 Unauthorized", "")
            } else {
                ("500 Internal Server Error", "")
            };
            let body = "{\"error\":{\"message\":\"Incorrect API key provided: sk-secret-123.\"}}";
            let response = format!(
                "HTTP/1.1 {status}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        })
        .await;

        let key = Secret::new("sk-secret-123");
        let client = client();
        for (case, class, wait) in [
            (429, RetryClass::RateLimited, Some(Duration::from_secs(7))),
            (401, RetryClass::Auth, None),
            (500, RetryClass::Transient, None),
        ] {
            let body = format!("{{\"case\":{case}}}");
            let error = client
                .post(HttpRequest {
                    url: &url,
                    headers: &[],
                    key: Some(&key),
                    json: body.as_bytes(),
                })
                .await
                .unwrap_err();
            assert_eq!(error.class, class, "{case}: {error}");
            assert_eq!(error.retry_after, wait, "{case}");
            assert!(error.message.contains("Incorrect API key"), "{error}");
            assert!(!error.message.contains("sk-secret-123"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_server_that_is_not_there_is_worth_another_try() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{address}/v1/chat/completions?key=in-the-address");
        // The usual time limits: some systems take a second or two to say
        // that nothing listens, and whether that shows as a refusal or as
        // a timeout is theirs to decide. Either is worth another try.
        let client = HttpClient::new(HttpSettings {
            direct: true,
            ..HttpSettings::default()
        })
        .unwrap();
        let error = client
            .post(HttpRequest {
                url: &url,
                headers: &[],
                key: None,
                json: b"{}",
            })
            .await
            .unwrap_err();
        assert!(error.is_retryable(), "{error}");
        assert!(!error.message.contains("in-the-address"), "{error}");
    }

    #[tokio::test]
    async fn an_answer_that_stops_in_the_middle_is_an_error() {
        let url = server(|mut stream, request| async move {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
            if request.contains("silent") {
                // Says nothing more, and stays.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            // Otherwise: gone, without the end of the body.
        })
        .await;

        let client = client();
        for (body, class) in [
            (&b"{\"then\":\"gone\"}"[..], RetryClass::Transient),
            (&b"{\"then\":\"silent\"}"[..], RetryClass::Timeout),
        ] {
            let mut response = client
                .post(HttpRequest {
                    url: &url,
                    headers: &[],
                    key: None,
                    json: body,
                })
                .await
                .unwrap();
            assert_eq!(response.chunk().await.unwrap().unwrap(), &b"hello"[..]);
            let error = response.chunk().await.unwrap_err();
            assert_eq!(error.class, class, "{error}");
        }
    }
}
