//! What the two scenario runners share: a connection that writes down
//! every message with the time it was sent or came, and the speech they
//! send.
//!
//! A recording is JSON lines. Each line has `t` (milliseconds since the
//! connection was asked for, by a clock that only goes forward), `dir`
//! (`out`, `in`, or `note` for what the runner says of itself), the
//! running counts `sent` and `got` (samples of audio sent and received so
//! far on this connection) and `event`. Audio inside an event is replaced
//! by its length in bytes: no sound is kept.

use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use waveport_adapter_kit::secret::Secret;
use waveport_adapter_kit::wav;
use waveport_adapter_kit::ws::{WsConn, WsMessage, WsRequest};

/// A result whose error is a sentence.
pub type Res<T> = Result<T, String>;

/// Speech made by a machine (see `audio/make.sh`), as samples.
///
/// # Errors
///
/// The file is not there: `sh audio/make.sh` makes it.
pub fn speech(name: &str, khz: u32) -> Res<Vec<i16>> {
    let path = root().join(format!("audio/{name}.{khz}k.wav"));
    let bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let wav = wav::read(&bytes).map_err(|error| format!("{}: {error:?}", path.display()))?;
    Ok(wav.samples)
}

/// The same speech as a carrier sends it: G.711 mu-law at 8 kHz.
///
/// # Errors
///
/// The file is not there.
pub fn speech_ulaw(name: &str) -> Res<Vec<u8>> {
    let path = root().join(format!("audio/{name}.8k.ulaw"));
    fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))
}

/// The directory of the spike.
#[must_use]
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Samples as the text a JSON message carries them in.
#[must_use]
pub fn pcm_base64(samples: &[i16]) -> String {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    STANDARD.encode(bytes)
}

/// Bytes as the text a JSON message carries them in.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// How many bytes a base64 text stands for.
#[must_use]
pub fn base64_len(text: &str) -> u64 {
    let padding = text.bytes().rev().take_while(|byte| *byte == b'=').count();
    (text.len() / 4 * 3).saturating_sub(padding) as u64
}

/// One message that came, and when.
#[derive(Clone, Debug)]
pub struct Seen {
    /// Milliseconds since the connection was asked for.
    pub t: u64,
    /// The message, with its audio replaced by lengths.
    pub event: Value,
}

/// Replaces the audio of a message by its length. Returns the number of
/// bytes of audio the message carried.
pub type Strip = fn(&mut Value) -> u64;

/// A connection to a provider that writes down all that passes.
pub struct Run {
    conn: Option<WsConn>,
    began: Instant,
    file: BufWriter<File>,
    strike: Vec<String>,
    strip_out: Strip,
    strip_in: Strip,
    /// Bytes of audio to one sample, sent.
    pub bytes_per_sample_out: u64,
    /// Bytes of audio to one sample, received.
    pub bytes_per_sample_in: u64,
    /// Samples sent so far.
    pub sent: u64,
    /// Samples received so far.
    pub got: u64,
    /// Every message that came.
    pub seen: Vec<Seen>,
    /// Why the connection ended, once it has.
    pub closed: Option<String>,
    /// Whether to print a line for every message.
    pub loud: bool,
    /// Fields of a received message whose text the runner needs and the
    /// recording must not hold (a handle that resumes a session): the
    /// value is given to the runner and struck from all that is written.
    pub hide_fields: Vec<&'static str>,
}

/// What a recording is of.
pub struct Recording<'a> {
    /// `openai` or `gemini`: the directory under `recordings/`.
    pub provider: &'a str,
    /// The file, without `.jsonl`.
    pub name: &'a str,
    /// The model, for the first line.
    pub model: &'a str,
    /// What is asked, for the first line.
    pub question: &'a str,
}

impl Run {
    /// Connects and starts the recording.
    ///
    /// # Errors
    ///
    /// The connection could not be set up, or the file not written.
    pub async fn open(
        recording: Recording<'_>,
        url: &str,
        headers: &[(&str, &str)],
        secrets: &[&Secret],
        strip_out: Strip,
        strip_in: Strip,
    ) -> Res<Self> {
        let dir = root().join("recordings").join(recording.provider);
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let path = dir.join(format!("{}.jsonl", recording.name));
        let file = File::create(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let strike: Vec<String> = secrets.iter().map(|s| s.expose().to_owned()).collect();
        let began = Instant::now();
        let request = WsRequest {
            url,
            headers,
            timeout: Duration::from_secs(20),
        };
        let conn = WsConn::connect(request).await.map_err(|error| {
            let mut text = error.to_string();
            for secret in &strike {
                text = text.replace(secret, "…");
            }
            format!("connect: {text}")
        })?;
        let mut run = Self {
            conn: Some(conn),
            began,
            file: BufWriter::new(file),
            strike,
            strip_out,
            strip_in,
            bytes_per_sample_out: 2,
            bytes_per_sample_in: 2,
            sent: 0,
            got: 0,
            seen: Vec::new(),
            closed: None,
            loud: true,
            hide_fields: Vec::new(),
        };
        run.note(json!({
            "recording": recording.name,
            "provider": recording.provider,
            "model": recording.model,
            "question": recording.question,
            "connected_after_ms": run.now(),
            "unix_time": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }));
        Ok(run)
    }

    /// Milliseconds since the connection was asked for.
    #[must_use]
    pub fn now(&self) -> u64 {
        u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn write(&mut self, dir: &str, event: &Value) {
        let line = json!({
            "t": self.now(),
            "dir": dir,
            "sent": self.sent,
            "got": self.got,
            "event": event,
        });
        let mut text = line.to_string();
        for secret in &self.strike {
            text = text.replace(secret, "…");
        }
        let _ = writeln!(self.file, "{text}");
        let _ = self.file.flush();
        if self.loud {
            let mut shown = brief(event);
            for secret in &self.strike {
                shown = shown.replace(secret, "…");
            }
            println!("{:>7} {} {shown}", self.now(), arrow(dir));
        }
    }

    /// Writes down something the runner does or concludes.
    pub fn note(&mut self, what: Value) {
        self.write("note", &what);
    }

    /// Sends a message.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn send(&mut self, event: Value) -> Res<()> {
        let text = event.to_string();
        let mut shown = event;
        let bytes = (self.strip_out)(&mut shown);
        self.sent += bytes / self.bytes_per_sample_out.max(1);
        self.write("out", &shown);
        let Some(conn) = self.conn.as_mut() else {
            return Err("the connection is closed".to_owned());
        };
        conn.send_text(text).await.map_err(|error| error.to_string())
    }

    /// Sends text that is not JSON, or bytes, as they are.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn send_raw(&mut self, text: Option<&str>, bytes: Option<&[u8]>) -> Res<()> {
        self.write("out", &json!({"raw_text": text, "raw_bytes": bytes.map(<[u8]>::len)}));
        let Some(conn) = self.conn.as_mut() else {
            return Err("the connection is closed".to_owned());
        };
        if let Some(text) = text {
            conn.send_text(text.to_owned()).await.map_err(|error| error.to_string())?;
        }
        if let Some(bytes) = bytes {
            conn.send_binary(bytes.to_vec().into())
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// The next message, if one comes within `patience`.
    pub async fn recv(&mut self, patience: Duration) -> Option<Value> {
        let conn = self.conn.as_mut()?;
        let message = match tokio::time::timeout(patience, conn.next()).await {
            Err(_) => return None,
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None)) => {
                self.end("closed in good order".to_owned());
                return None;
            }
            Ok(Err(error)) => {
                self.end(format!("{:?}: {error}", error.class));
                return None;
            }
        };
        let mut event = match message {
            WsMessage::Text(text) => serde_json::from_str::<Value>(text.as_str())
                .unwrap_or_else(|_| json!({"not_json": text.as_str()})),
            // Gemini sends its JSON as bytes.
            WsMessage::Binary(bytes) => serde_json::from_slice::<Value>(&bytes)
                .unwrap_or_else(|_| json!({"binary_bytes": bytes.len()})),
        };
        let bytes = (self.strip_in)(&mut event);
        self.got += bytes / self.bytes_per_sample_in.max(1);
        let mut hidden = Vec::new();
        find_fields(&event, &self.hide_fields, &mut hidden);
        for value in hidden {
            if !value.is_empty() && !self.strike.contains(&value) {
                self.strike.push(value);
            }
        }
        self.write("in", &event);
        self.seen.push(Seen {
            t: self.now(),
            event: event.clone(),
        });
        Some(event)
    }

    fn end(&mut self, why: String) {
        let mut why = why;
        for secret in &self.strike {
            why = why.replace(secret, "…");
        }
        self.note(json!({"connection_ended": why}));
        self.closed = Some(why);
        self.conn = None;
    }

    /// Reads on for `time`, whatever comes.
    pub async fn listen(&mut self, time: Duration) {
        let until = Instant::now() + time;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() || self.closed.is_some() {
                return;
            }
            if self.recv(left).await.is_none() && self.closed.is_some() {
                return;
            }
        }
    }

    /// Reads on until a message for which `wanted` holds, or `patience`
    /// runs out.
    pub async fn until(
        &mut self,
        patience: Duration,
        wanted: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        let until = Instant::now() + patience;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() || self.closed.is_some() {
                self.note(json!({"gave_up_waiting_after_ms": patience.as_millis() as u64}));
                return None;
            }
            if let Some(event) = self.recv(left).await {
                if wanted(&event) {
                    return Some(event);
                }
            }
        }
    }

    /// Closes the connection and the recording.
    pub async fn close(mut self) {
        if let Some(conn) = self.conn.take() {
            conn.close().await;
        }
        self.note(json!({"closed_by": "the runner"}));
        let _ = self.file.flush();
    }
}

fn find_fields(value: &Value, names: &[&str], out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                match inner.as_str() {
                    Some(text) if names.contains(&key.as_str()) => out.push(text.to_owned()),
                    _ => find_fields(inner, names, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| find_fields(item, names, out)),
        _ => {}
    }
}

fn arrow(dir: &str) -> &'static str {
    match dir {
        "out" => ">",
        "in" => "<",
        _ => "#",
    }
}

/// A message in one line, for the terminal.
#[must_use]
pub fn brief(event: &Value) -> String {
    let mut text = event.to_string();
    if text.len() > 420 {
        let mut cut = 420;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    text
}

/// The text field `name` of a message, or nothing.
#[must_use]
pub fn text_of<'a>(event: &'a Value, name: &str) -> &'a str {
    event.get(name).and_then(Value::as_str).unwrap_or("")
}

/// Silence.
#[must_use]
pub fn silence(samples: usize) -> Vec<i16> {
    vec![0; samples]
}
