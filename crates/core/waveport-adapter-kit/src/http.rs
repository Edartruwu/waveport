//! HTTP requests whose answers arrive as a stream.
//!
//! A language model is asked with one `POST` and answers piece by piece.
//! This client keeps connections open between requests, since setting
//! one up costs as much time as a model takes to start writing, and
//! hands every failure over as a [`PortError`] whose class says whether
//! trying again can help.
//!
//! The same client asks the smaller things: a carrier that is told to
//! place a call, or asked how one stands, and a server that is offered
//! a call and told when it is over. Those say who asks in a way of
//! their own ([`Auth`]), some take a form or a description of a call
//! and not JSON ([`Body`]), and their answers are short enough to be
//! read whole ([`HttpResponse::body`]).

use core::fmt;
use core::fmt::Write as _;
use core::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use reqwest::Method;
use reqwest::header::{AUTHORIZATION, HeaderValue};
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

/// How a request says who sends it. The key goes in a header that is
/// marked as one not to be shown, and is struck out of whatever the
/// server says back in an error.
#[derive(Clone, Copy, Debug)]
pub enum Auth<'a> {
    /// It does not say.
    None,
    /// The key as a bearer token: `Authorization: Bearer`.
    Bearer(&'a Secret),
    /// A name and its key: `Authorization: Basic`. Carriers take the
    /// name of an account and its token that way.
    Basic {
        /// The name, which is no secret.
        user: &'a str,
        /// The key that goes with it.
        key: &'a Secret,
    },
    /// The key as the value of a header of the provider's own, such as
    /// `x-api-key`.
    Header {
        /// The name of the header.
        name: &'a str,
        /// The key.
        key: &'a Secret,
    },
}

impl Auth<'_> {
    /// The header that says who sends the request, if one does.
    fn header(&self) -> Result<Option<(&str, HeaderValue)>, PortError> {
        let (name, value) = match self {
            Self::None => return Ok(None),
            Self::Bearer(key) => (AUTHORIZATION.as_str(), format!("Bearer {}", key.expose())),
            Self::Basic { user, key } => (
                AUTHORIZATION.as_str(),
                format!("Basic {}", basic(user, key)),
            ),
            Self::Header { name, key } => (*name, key.expose().to_owned()),
        };
        // What the value is stays out of the error: it is the key.
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| PortError::permanent("the key cannot be sent in a header"))?;
        // Kept out of whatever prints the request.
        value.set_sensitive(true);
        Ok(Some((name, value)))
    }

    /// `text` without the key, in any shape it was sent in.
    fn strike(&self, text: &str) -> String {
        match self {
            Self::None => text.to_owned(),
            Self::Bearer(key) | Self::Header { key, .. } => key.strike(text),
            Self::Basic { user, key } => {
                // A server that repeats the header repeats the key as it
                // was sent, which is not as it is written.
                let sent = Secret::new(basic(user, key));
                key.strike(&sent.strike(text))
            }
        }
    }
}

/// A name and its key as `Authorization: Basic` has them.
fn basic(user: &str, key: &Secret) -> String {
    STANDARD.encode(format!("{user}:{}", key.expose()))
}

/// What a request carries.
#[derive(Clone, Copy, Debug)]
pub enum Body<'a> {
    /// Nothing.
    None,
    /// JSON.
    Json(&'a [u8]),
    /// Names and their values, as a form sends them
    /// (`application/x-www-form-urlencoded`).
    Form(&'a [(&'a str, &'a str)]),
    /// Bytes of the kind it says they are, as they are: the description
    /// of a call (`application/sdp`), and whatever else is neither JSON
    /// nor a form.
    Of {
        /// What the bytes are, as `Content-Type` says it.
        kind: &'a str,
        /// The bytes.
        bytes: &'a [u8],
    },
}

/// Names and values as a form sends them.
fn form(fields: &[(&str, &str)]) -> Vec<u8> {
    fn written(text: &str, out: &mut String) {
        for byte in text.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                    out.push(char::from(byte));
                }
                b' ' => out.push('+'),
                _ => {
                    let _ = write!(out, "%{byte:02X}");
                }
            }
        }
    }
    let mut out = String::new();
    for (name, value) in fields {
        if !out.is_empty() {
            out.push('&');
        }
        written(name, &mut out);
        out.push('=');
        written(value, &mut out);
    }
    out.into_bytes()
}

/// A request.
#[derive(Clone, Copy, Debug)]
pub struct HttpRequest<'a> {
    /// The address.
    pub url: &'a str,
    /// Headers to send, besides the one that says who sends it.
    pub headers: &'a [(&'a str, &'a str)],
    /// Who sends it.
    pub auth: Auth<'a>,
    /// What it carries.
    pub body: Body<'a>,
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
        self.send(Method::POST, request).await
    }

    /// Sends a `GET` and waits for the answer to begin.
    ///
    /// # Errors
    ///
    /// As [`post`](Self::post). A request that carries something is not
    /// sent: a `GET` carries nothing.
    pub async fn get(&self, request: HttpRequest<'_>) -> Result<HttpResponse, PortError> {
        if !matches!(request.body, Body::None) {
            return Err(PortError::permanent("a GET carries nothing"));
        }
        self.send(Method::GET, request).await
    }

    /// Sends a `DELETE` and waits for the answer to begin.
    ///
    /// # Errors
    ///
    /// As [`post`](Self::post). A request that carries something is not
    /// sent: what is let go of is named by the address.
    pub async fn delete(&self, request: HttpRequest<'_>) -> Result<HttpResponse, PortError> {
        if !matches!(request.body, Body::None) {
            return Err(PortError::permanent("a DELETE carries nothing"));
        }
        self.send(Method::DELETE, request).await
    }

    /// The request as it is sent.
    fn built(
        &self,
        method: Method,
        request: &HttpRequest<'_>,
    ) -> Result<reqwest::Request, PortError> {
        let mut builder = self.inner.request(method, request.url);
        builder = match request.body {
            Body::None => builder,
            Body::Json(json) => builder
                .header("content-type", "application/json")
                .body(json.to_vec()),
            Body::Form(fields) => builder
                .header("content-type", "application/x-www-form-urlencoded")
                .body(form(fields)),
            Body::Of { kind, bytes } => builder.header("content-type", kind).body(bytes.to_vec()),
        };
        for (name, value) in request.headers {
            builder = builder.header(*name, *value);
        }
        if let Some((name, value)) = request.auth.header()? {
            builder = builder.header(name, value);
        }
        builder.build().map_err(send_error)
    }

    async fn send(
        &self,
        method: Method,
        request: HttpRequest<'_>,
    ) -> Result<HttpResponse, PortError> {
        let built = self.built(method, &request)?;
        let mut response = self.inner.execute(built).await.map_err(send_error)?;
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
        let said = request.auth.strike(&String::from_utf8_lossy(&said));
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

    /// The whole body, for an answer that is short: one of `at_most`
    /// bytes or fewer.
    ///
    /// # Errors
    ///
    /// The connection dropped, the server was silent for too long, or
    /// the answer is longer than that. The same request would be
    /// answered at the same length.
    pub async fn body(mut self, at_most: usize) -> Result<Vec<u8>, PortError> {
        let mut body = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            if body.len() + chunk.len() > at_most {
                return Err(PortError::permanent(format!(
                    "the answer is longer than the {at_most} bytes that are read of one"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
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

    use reqwest::Method;

    use super::{Auth, Body, HttpClient, HttpRequest, HttpSettings, form};
    use crate::secret::Secret;
    use crate::sse::SseParser;

    /// A client that waits as long as any client does. A machine that
    /// runs many tests at once can stand still for longer than a test
    /// would think to wait.
    fn client() -> HttpClient {
        HttpClient::new(HttpSettings {
            direct: true,
            ..HttpSettings::default()
        })
        .unwrap()
    }

    /// A client that gives up on silence after a second, for the test
    /// that waits for it to.
    fn impatient() -> HttpClient {
        HttpClient::new(HttpSettings {
            read_timeout: Duration::from_secs(1),
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
                auth: Auth::Bearer(&key),
                body: Body::Json(b"{\"stream\":true}"),
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
                    auth: Auth::Bearer(&key),
                    body: Body::Json(body.as_bytes()),
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
                auth: Auth::None,
                body: Body::Json(b"{}"),
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

        let client = impatient();
        for (body, class) in [
            (&b"{\"then\":\"gone\"}"[..], RetryClass::Transient),
            (&b"{\"then\":\"silent\"}"[..], RetryClass::Timeout),
        ] {
            let mut response = client
                .post(HttpRequest {
                    url: &url,
                    headers: &[],
                    auth: Auth::None,
                    body: Body::Json(body),
                })
                .await
                .unwrap();
            assert_eq!(response.chunk().await.unwrap().unwrap(), &b"hello"[..]);
            let error = response.chunk().await.unwrap_err();
            assert_eq!(error.class, class, "{error}");
        }
    }

    #[test]
    fn a_form_is_written_as_a_browser_writes_one() {
        assert_eq!(form(&[]), b"");
        // A number to call, an address with an address in it, and what
        // is neither a letter nor a digit.
        let fields = [
            ("To", "+15550100"),
            ("Url", "https://voice.example/answer?call=7&again=no"),
            ("Friendly Name", "a b*c-d.e_f~g"),
            ("Said", "¿Qué hora es?"),
            ("Empty", ""),
        ];
        let written = String::from_utf8(form(&fields)).unwrap();
        assert_eq!(
            written,
            "To=%2B15550100\
             &Url=https%3A%2F%2Fvoice.example%2Fanswer%3Fcall%3D7%26again%3Dno\
             &Friendly+Name=a+b*c-d.e_f%7Eg\
             &Said=%C2%BFQu%C3%A9+hora+es%3F\
             &Empty="
        );
    }

    #[test]
    fn whoever_sends_a_request_is_said_in_a_header_that_is_not_shown() {
        let client = client();
        let key = Secret::new("tok-secret-123");
        let cases = [
            (Auth::Bearer(&key), "authorization", "Bearer tok-secret-123"),
            (
                Auth::Basic {
                    user: "an-account",
                    key: &key,
                },
                "authorization",
                // `an-account:tok-secret-123`
                "Basic YW4tYWNjb3VudDp0b2stc2VjcmV0LTEyMw==",
            ),
            (
                Auth::Header {
                    name: "x-api-key",
                    key: &key,
                },
                "x-api-key",
                "tok-secret-123",
            ),
        ];
        for (auth, name, value) in cases {
            let request = HttpRequest {
                url: "http://127.0.0.1:9/",
                headers: &[("x-extra", "yes")],
                auth,
                body: Body::None,
            };
            let built = client.built(Method::GET, &request).unwrap();
            let sent = built.headers().get(name).unwrap();
            assert_eq!(sent.to_str().unwrap(), value, "{name}");
            assert!(sent.is_sensitive(), "{name}");
            // What prints the request does not print the key.
            let shown = format!("{built:?} {request:?}");
            for held in ["tok-secret-123", "YW4tYWNjb3VudDp0b2stc2VjcmV0LTEyMw"] {
                assert!(!shown.contains(held), "{shown}");
            }
            assert!(!built.headers().get("x-extra").unwrap().is_sensitive());
        }
        // Who does not say is not said.
        let request = HttpRequest {
            url: "http://127.0.0.1:9/",
            headers: &[],
            auth: Auth::None,
            body: Body::None,
        };
        let built = client.built(Method::GET, &request).unwrap();
        assert!(built.headers().get("authorization").is_none());
        // A key that cannot stand in a header is not sent, and not
        // repeated.
        let broken = Secret::new("tok\nsecret");
        let request = HttpRequest {
            auth: Auth::Bearer(&broken),
            ..request
        };
        let error = client.built(Method::GET, &request).unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
        assert!(!error.message.contains("secret"), "{error}");
    }

    #[tokio::test]
    async fn a_refusal_repeats_no_key_in_whatever_shape_it_was_sent() {
        let url = server(|mut stream, request| async move {
            // A server that says back what it was sent, as some do.
            let sent: Vec<&str> = request
                .lines()
                .filter(|line| {
                    let line = line.to_ascii_lowercase();
                    line.starts_with("authorization:") || line.starts_with("x-api-key:")
                })
                .collect();
            let body = format!("not allowed: {} (tok-secret-123)", sent.join(" "));
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        })
        .await;
        let key = Secret::new("tok-secret-123");
        let client = client();
        for auth in [
            Auth::Bearer(&key),
            Auth::Basic {
                user: "an-account",
                key: &key,
            },
            Auth::Header {
                name: "x-api-key",
                key: &key,
            },
        ] {
            let error = client
                .post(HttpRequest {
                    url: &url,
                    headers: &[],
                    auth,
                    body: Body::Form(&[("To", "+15550100")]),
                })
                .await
                .unwrap_err();
            assert_eq!(error.class, RetryClass::Auth, "{auth:?}: {error}");
            assert!(error.message.contains("not allowed"), "{error}");
            for held in ["tok-secret-123", "YW4tYWNjb3VudDp0b2stc2VjcmV0LTEyMw"] {
                assert!(!error.message.contains(held), "{auth:?}: {error}");
            }
        }
    }

    #[tokio::test]
    async fn a_form_is_posted_and_a_short_answer_is_read_whole() {
        let url = server(|mut stream, request| async move {
            let lower = request.to_ascii_lowercase();
            let body = if request.starts_with("POST ") {
                assert!(
                    lower.contains("content-type: application/x-www-form-urlencoded\r\n"),
                    "{request}"
                );
                assert!(lower.contains("authorization: basic "), "{request}");
                assert!(request.ends_with("\r\n\r\nTo=%2B15550100&Digits=12%23"));
                "{\"call\":\"made-up\"}"
            } else {
                assert!(
                    request.starts_with("GET /v1/chat/completions HTTP/1.1\r\n"),
                    "{request}"
                );
                // Nothing is carried, and nothing says that something is.
                assert!(!lower.contains("content-type:"), "{request}");
                assert!(request.ends_with("\r\n\r\n"));
                "{\"state\":\"ringing\"}"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        })
        .await;
        let key = Secret::new("tok-secret-123");
        let auth = Auth::Basic {
            user: "an-account",
            key: &key,
        };
        let client = client();
        let placed = client
            .post(HttpRequest {
                url: &url,
                headers: &[],
                auth,
                body: Body::Form(&[("To", "+15550100"), ("Digits", "12#")]),
            })
            .await
            .unwrap();
        assert_eq!(placed.body(64).await.unwrap(), b"{\"call\":\"made-up\"}");

        let asked = HttpRequest {
            url: &url,
            headers: &[],
            auth,
            body: Body::None,
        };
        let stands = client.get(asked).await.unwrap();
        assert_eq!(stands.body(19).await.unwrap(), b"{\"state\":\"ringing\"}");
        // An answer that is longer than is read of one is not read.
        let error = client.get(asked).await.unwrap().body(18).await.unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent, "{error}");
        // And a GET that was to carry something is not sent.
        let carrying = HttpRequest {
            body: Body::Json(b"{}"),
            ..asked
        };
        let error = client.get(carrying).await.unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent, "{error}");
        assert!(error.message.contains("a GET carries nothing"), "{error}");
    }

    #[tokio::test]
    async fn a_description_is_posted_as_what_it_is_and_what_it_made_is_let_go_of() {
        let url = server(|mut stream, request| async move {
            let lower = request.to_ascii_lowercase();
            let (status, more, body) = if request.starts_with("POST ") {
                assert!(
                    lower.contains("content-type: application/sdp\r\n"),
                    "{request}"
                );
                assert!(
                    lower.contains("authorization: bearer tok-secret-123\r\n"),
                    "{request}"
                );
                // The bytes are sent as they are.
                assert!(request.ends_with("\r\n\r\nv=0\r\n"), "{request}");
                ("201 Created", "location: /calls/made-up\r\n", "v=0\r\n")
            } else {
                assert!(
                    request.starts_with("DELETE /v1/chat/completions HTTP/1.1\r\n"),
                    "{request}"
                );
                assert!(!lower.contains("content-type:"), "{request}");
                assert!(request.ends_with("\r\n\r\n"));
                ("200 OK", "", "")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\n{more}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        })
        .await;
        let key = Secret::new("tok-secret-123");
        let client = client();
        let offered = client
            .post(HttpRequest {
                url: &url,
                headers: &[],
                auth: Auth::Bearer(&key),
                body: Body::Of {
                    kind: "application/sdp",
                    bytes: b"v=0\r\n",
                },
            })
            .await
            .unwrap();
        // Where what was made is, is in the head of the answer.
        assert_eq!(offered.header("location"), Some("/calls/made-up"));
        assert_eq!(offered.body(64).await.unwrap(), b"v=0\r\n");

        let gone = HttpRequest {
            url: &url,
            headers: &[],
            auth: Auth::Bearer(&key),
            body: Body::None,
        };
        let answered = client.delete(gone).await.unwrap();
        assert_eq!(answered.body(64).await.unwrap(), b"");
        // And a DELETE that was to carry something is not sent.
        let carrying = HttpRequest {
            body: Body::Json(b"{}"),
            ..gone
        };
        let error = client.delete(carrying).await.unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent, "{error}");
        assert!(
            error.message.contains("a DELETE carries nothing"),
            "{error}"
        );
    }
}
