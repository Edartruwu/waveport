//! Whoever asks the daemon: a client of HTTP that says exactly what a
//! test has it say, and reads a stream of events as it arrives.

use core::fmt::Write as _;
use core::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use waveport_protocol::client::v1::FromServer;

/// How long an answer, or the next event of a stream, is waited for.
const PATIENCE: Duration = Duration::from_secs(20);

/// What a request was answered with.
#[derive(Clone, Debug)]
pub(crate) struct Answer {
    pub(crate) status: u16,
    /// The headers, with their names in lower case.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl Answer {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body, which is JSON.
    pub(crate) fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("{error}: {} {:?}", self.status, self.body))
    }

    /// The code of the problem the answer is.
    pub(crate) fn code(&self) -> String {
        assert_eq!(
            self.header("content-type"),
            Some("application/problem+json"),
            "{self:?}"
        );
        let problem = self.json();
        assert_eq!(problem["status"], self.status, "{self:?}");
        problem["code"].as_str().unwrap().to_owned()
    }
}

/// The head of an answer: its status and its headers.
fn head(written: &str) -> (u16, Vec<(String, String)>) {
    let mut lines = written.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("no status in {written:?}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    (status, headers)
}

/// Where the head of an answer ends in `bytes`, if all of it is there.
fn head_ends(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Takes the chunks that are whole out of `raw`, and puts what they
/// hold at the end of `body`. Returns whether the last chunk was among
/// them.
fn unchunk(raw: &mut Vec<u8>, body: &mut Vec<u8>) -> bool {
    loop {
        let Some(line) = raw.windows(2).position(|window| window == b"\r\n") else {
            return false;
        };
        let size = core::str::from_utf8(&raw[..line])
            .ok()
            .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())
            .unwrap_or_else(|| panic!("not the size of a chunk: {:?}", &raw[..line]));
        let begins = line + 2;
        if raw.len() < begins + size + 2 {
            return false;
        }
        body.extend_from_slice(&raw[begins..begins + size]);
        raw.drain(..begins + size + 2);
        if size == 0 {
            return true;
        }
    }
}

/// The request, as it goes over the wire.
fn request(
    address: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> String {
    let says = |name: &str| {
        headers
            .iter()
            .any(|(said, _)| said.eq_ignore_ascii_case(name))
    };
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    if !says("host") {
        let _ = write!(request, "Host: {address}\r\n");
    }
    for (name, value) in headers {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    if let Some(body) = body {
        if !says("content-type") {
            request.push_str("Content-Type: application/json\r\n");
        }
        let _ = write!(request, "Content-Length: {}\r\n", body.len());
    }
    request.push_str("Connection: close\r\n\r\n");
    request.push_str(body.unwrap_or_default());
    request
}

/// Asks the daemon at `address` and returns its answer.
pub(crate) async fn ask(
    address: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Answer {
    let asked = async {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let request = request(address, method, path, headers, body);
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        raw
    };
    let mut raw = tokio::time::timeout(PATIENCE, asked)
        .await
        .unwrap_or_else(|_| panic!("{method} {path} was not answered"));
    let ends = head_ends(&raw).unwrap_or_else(|| panic!("no head in {raw:?}"));
    let (status, headers) = head(core::str::from_utf8(&raw[..ends]).unwrap());
    raw.drain(..ends + 4);
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value == "chunked");
    let mut body = Vec::new();
    if chunked {
        assert!(unchunk(&mut raw, &mut body), "the body was cut");
    } else {
        body = raw;
    }
    Answer {
        status,
        headers,
        body: String::from_utf8(body).unwrap(),
    }
}

/// One event of a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) kind: String,
    pub(crate) id: Option<String>,
    pub(crate) retry: Option<u64>,
    pub(crate) data: String,
}

impl Event {
    /// The message the event is.
    pub(crate) fn message(&self) -> FromServer {
        let message: FromServer = serde_json::from_str(&self.data)
            .unwrap_or_else(|error| panic!("{error}: {:?}", self.data));
        // An event is named as its message is.
        let named = serde_json::to_value(&message).unwrap();
        assert_eq!(named["type"], self.kind.as_str(), "{self:?}");
        message
    }

    pub(crate) fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap()
    }
}

/// A stream of events that is being read.
pub(crate) struct Events {
    stream: TcpStream,
    /// What arrived and is not a whole chunk yet.
    raw: Vec<u8>,
    /// What the chunks held and is not a whole event yet.
    text: Vec<u8>,
    over: bool,
    /// The headers of the answer, with their names in lower case.
    pub(crate) headers: Vec<(String, String)>,
    /// How many comments were sent between the events.
    pub(crate) comments: usize,
}

impl Events {
    /// Asks for the stream at `path`. An answer that is no stream is
    /// returned as it is.
    pub(crate) async fn open(
        address: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<Self, Answer> {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let request = request(address, "GET", path, headers, None);
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        let ends = loop {
            if let Some(ends) = head_ends(&raw) {
                break ends;
            }
            let read = tokio::time::timeout(PATIENCE, stream.read_buf(&mut raw))
                .await
                .unwrap_or_else(|_| panic!("GET {path} was not answered"))
                .unwrap();
            assert_ne!(read, 0, "the connection closed before an answer: {raw:?}");
        };
        let (status, headers) = head(core::str::from_utf8(&raw[..ends]).unwrap());
        raw.drain(..ends + 4);
        let kind = headers
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, kind)| kind.as_str());
        if status != 200 || kind != Some("text/event-stream") {
            let rest = tokio::time::timeout(PATIENCE, stream.read_to_end(&mut raw)).await;
            rest.unwrap_or_else(|_| panic!("GET {path} was not answered to its end"))
                .unwrap();
            return Err(Answer {
                status,
                headers,
                body: String::from_utf8(raw).unwrap(),
            });
        }
        Ok(Self {
            stream,
            raw,
            text: Vec::new(),
            over: false,
            headers,
            comments: 0,
        })
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }

    /// The next event that is whole in what has arrived.
    fn whole(&mut self) -> Option<Event> {
        loop {
            let ends = self.text.windows(2).position(|window| window == b"\n\n")?;
            let written: Vec<u8> = self.text.drain(..ends + 2).collect();
            let written = String::from_utf8(written).unwrap();
            let mut event = Event {
                kind: String::new(),
                id: None,
                retry: None,
                data: String::new(),
            };
            let mut said = false;
            for line in written.lines().filter(|line| !line.is_empty()) {
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "" => self.comments += 1,
                    "event" => value.clone_into(&mut event.kind),
                    "id" => event.id = Some(value.to_owned()),
                    "retry" => event.retry = Some(value.parse().unwrap()),
                    "data" => {
                        if said {
                            event.data.push('\n');
                        }
                        event.data.push_str(value);
                        said = true;
                    }
                    other => panic!("a field no event has: {other:?} in {written:?}"),
                }
            }
            if said {
                return Some(event);
            }
        }
    }

    /// The next event, or `None` when the stream has come to its end.
    /// `within` is how long it is waited for.
    pub(crate) async fn next_within(&mut self, within: Duration) -> Option<Event> {
        // A stream that sends comments and no event says nothing.
        let until = tokio::time::Instant::now() + within;
        loop {
            if let Some(event) = self.whole() {
                return Some(event);
            }
            if self.over {
                assert_eq!(self.text, b"", "the stream was cut in an event");
                return None;
            }
            // A connection that was reset is a stream that was cut.
            let read = tokio::time::timeout_at(until, self.stream.read_buf(&mut self.raw))
                .await
                .unwrap_or_else(|_| panic!("the stream said nothing for {within:?}"))
                .unwrap_or(0);
            let last = unchunk(&mut self.raw, &mut self.text);
            self.over = last || read == 0;
        }
    }

    /// The next event, or `None` when the stream has come to its end.
    pub(crate) async fn next(&mut self) -> Option<Event> {
        self.next_within(PATIENCE).await
    }

    /// Reads the stream for as long as `quiet`, in which it is to send
    /// no event. Returns how many comments it sent meanwhile.
    pub(crate) async fn idles_for(&mut self, quiet: Duration) -> usize {
        let before = self.comments;
        let until = tokio::time::Instant::now() + quiet;
        loop {
            if let Some(event) = self.whole() {
                panic!("the stream was to say nothing, and said {event:?}");
            }
            let read = tokio::time::timeout_at(until, self.stream.read_buf(&mut self.raw)).await;
            match read {
                Err(_) => return self.comments - before,
                Ok(read) => {
                    assert_ne!(read.unwrap_or(0), 0, "the stream ended");
                    assert!(!unchunk(&mut self.raw, &mut self.text), "the stream ended");
                }
            }
        }
    }

    /// The events up to the first that `last` says is the one waited
    /// for, which is the last of them.
    pub(crate) async fn until(&mut self, last: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            let event = self
                .next()
                .await
                .unwrap_or_else(|| panic!("the stream ended after {events:#?}"));
            let found = last(&event);
            events.push(event);
            if found {
                return events;
            }
        }
    }

    /// The events up to the end of the stream.
    pub(crate) async fn rest(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(event) = self.next().await {
            events.push(event);
        }
        events
    }
}
