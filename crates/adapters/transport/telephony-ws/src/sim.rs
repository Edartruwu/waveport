//! A stand-in for a carrier, for tests.
//!
//! It does what a carrier's documentation says the carrier does, and
//! nothing a carrier was seen to do: nothing here was recorded from a
//! call. A test has it bring a call to a stream, speak, press keys and
//! hang up. It holds the audio it is sent, as a carrier does, and plays
//! as much of it as the test says ([`Sim::plays`]), so that what a
//! session is told was heard can be held against what the stand-in
//! played. A test that wants to say each mark back in its own time has
//! it say none by itself ([`Sim::waits_to_be_told`]).
//!
//! What it is sent has to be what the documentation shows and nothing
//! more. A carrier warns of a message it does not know, of a field it
//! does not know and of a stream that is not this one, and plays none of
//! such a message. The stand-in keeps each as a complaint
//! ([`Sim::complaints`]), and a test that sends what a carrier takes has
//! none.
//!
//! Around the stream it does the two things a carrier does first:
//! [`Ringing::form`] is what the carrier posts when it asks what to do
//! with a call, to be signed with [`Carrier::sign`], and
//! [`Answered::read`] is what it makes of the answer.

use std::collections::VecDeque;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;
use waveport_adapter_kit::ws::{WsConn, WsMessage, WsRequest};
use waveport_domain::event::Key;
use waveport_ports::error::PortError;

use crate::Carrier;

/// A call that a carrier has taken and asks about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ringing {
    /// The carrier's name for the call.
    pub call: String,
    /// The carrier's name for the stream it will be connected to.
    pub stream: String,
    /// The carrier's name for the account.
    pub account: String,
    /// The number that calls.
    pub from: String,
    /// The number that is called.
    pub to: String,
}

impl Ringing {
    /// The `n`th call of a test: its names are made up, in the shape a
    /// carrier's have, and its numbers are ones no telephone has.
    #[must_use]
    pub fn made_up(n: u32) -> Self {
        Self {
            call: format!("CA{n:032x}"),
            stream: format!("MZ{n:032x}"),
            account: format!("AC{:032x}", 1),
            from: "+15550100".to_owned(),
            to: "+15550101".to_owned(),
        }
    }

    /// The fields of the form the carrier posts when it asks what to do
    /// with the call, in the order it happens to write them.
    #[must_use]
    pub fn form(&self, carrier: Carrier) -> Vec<(String, String)> {
        match carrier {
            Carrier::Twilio => [
                ("Called", self.to.as_str()),
                ("To", self.to.as_str()),
                ("CallStatus", "ringing"),
                ("From", self.from.as_str()),
                ("Direction", "inbound"),
                ("AccountSid", self.account.as_str()),
                ("ApiVersion", "2010-04-01"),
                ("Caller", self.from.as_str()),
                ("CallSid", self.call.as_str()),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .to_vec(),
        }
    }
}

/// What a carrier makes of the answer to a call: the stream to connect
/// it to, and what the stream is to say as it begins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answered {
    /// The address of the stream.
    pub stream: String,
    /// What the stream says as it begins, in the order of the answer.
    pub parameters: Vec<(String, String)>,
}

impl Answered {
    /// Reads an answer. `None` for one that connects the call to no
    /// stream, or that a carrier could not read.
    #[must_use]
    pub fn read(carrier: Carrier, document: &str) -> Option<Self> {
        match carrier {
            Carrier::Twilio => {
                let (_, connect) = document.split_once("<Connect>")?;
                let (connect, _) = connect.split_once("</Connect>")?;
                let (_, stream) = connect.split_once("<Stream")?;
                let (opening, inside) = stream.split_once('>')?;
                let (inside, _) = inside.split_once("</Stream>")?;
                let url = attribute(opening, "url")?;
                let mut parameters = Vec::new();
                for parameter in inside.split("<Parameter").skip(1) {
                    let (opening, _) = parameter.split_once('>')?;
                    parameters.push((attribute(opening, "name")?, attribute(opening, "value")?));
                }
                Some(Self {
                    stream: url,
                    parameters,
                })
            }
        }
    }
}

/// The value of the attribute `name` among those of an element, as it
/// reads once what stands for other characters is put back.
fn attribute(element: &str, name: &str) -> Option<String> {
    let mut rest = element;
    loop {
        let (before, after) = rest.split_once('=')?;
        let quote = after
            .chars()
            .next()
            .filter(|quote| matches!(quote, '"' | '\''))?;
        let (value, following) = after.get(1..)?.split_once(quote)?;
        if before.trim() == name {
            return unescaped(value);
        }
        rest = following;
    }
}

fn unescaped(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        let (entity, following) = after.split_once(';')?;
        out.push(match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => return None,
        });
        rest = following;
    }
    out.push_str(rest);
    Some(out)
}

/// Something a server sent to the stand-in, which a carrier takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sent {
    /// Audio for the caller, as it was sent.
    Audio(Bytes),
    /// A place in the audio, to be said back when the caller has been
    /// played everything before it.
    Mark(String),
    /// What is held and was not played is to be discarded.
    Clear,
}

/// What a server may send on a stream of Twilio's. A field that the
/// documentation does not show is one Twilio warns of.
#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "lowercase", deny_unknown_fields)]
enum ToTwilio {
    Media {
        #[serde(rename = "streamSid")]
        stream: String,
        media: MediaTo,
    },
    Mark {
        #[serde(rename = "streamSid")]
        stream: String,
        mark: MarkTo,
    },
    Clear {
        #[serde(rename = "streamSid")]
        stream: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaTo {
    payload: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkTo {
    name: String,
}

/// What the stand-in holds and has not played.
#[derive(Debug)]
enum Held {
    Audio(Bytes),
    Mark(String),
}

/// A stand-in for a carrier with one call, at its end of a stream.
#[derive(Debug)]
pub struct Sim {
    carrier: Carrier,
    /// The connection, until either end closes it.
    conn: Option<WsConn>,
    ringing: Ringing,
    /// The number of the next message.
    sequence: u64,
    /// The number of the next piece of the caller's audio.
    chunk: u64,
    /// How many samples the caller has said.
    said: u64,
    /// What it was sent and has not played, in the order it was sent.
    held: VecDeque<Held>,
    /// Whether it says a mark back when the caller has been played
    /// everything before it, as a carrier does.
    by_itself: bool,
    /// The marks it has to say back and has not yet.
    owed: VecDeque<String>,
    /// What it was sent and has not yet told whoever asks.
    taken: Option<Sent>,
    complaints: Vec<String>,
}

impl Sim {
    /// Connects to the stream at `url` as the carrier does once a call
    /// was answered with it, with `headers`, and says nothing yet.
    ///
    /// # Errors
    ///
    /// The stream could not be connected to.
    pub async fn connects(
        carrier: Carrier,
        url: &str,
        headers: &[(&str, &str)],
        ringing: Ringing,
    ) -> Result<Self, PortError> {
        let request = WsRequest {
            headers,
            ..WsRequest::new(url)
        };
        let conn = WsConn::connect(request).await?;
        Ok(Self {
            carrier,
            conn: Some(conn),
            ringing,
            sequence: 1,
            chunk: 1,
            said: 0,
            held: VecDeque::new(),
            by_itself: true,
            owed: VecDeque::new(),
            taken: None,
            complaints: Vec::new(),
        })
    }

    /// Connects to the stream at `url` and begins the call on it, with
    /// `parameters` for what its answer gave the stream to say.
    ///
    /// # Errors
    ///
    /// The stream could not be connected to, or dropped.
    pub async fn brings(
        carrier: Carrier,
        url: &str,
        ringing: Ringing,
        parameters: &[(String, String)],
    ) -> Result<Self, PortError> {
        let mut sim = Self::connects(carrier, url, &[], ringing).await?;
        sim.begins(parameters).await?;
        Ok(sim)
    }

    /// The call it brings.
    #[must_use]
    pub const fn ringing(&self) -> &Ringing {
        &self.ringing
    }

    /// From here on it says no mark back by itself, which no carrier
    /// does: for a test that says each one back in its own time
    /// ([`echoes`](Self::echoes)). What it is sent is held as before.
    pub const fn waits_to_be_told(&mut self) {
        self.by_itself = false;
    }

    /// The caller has been played up to the mark `name`, or it was
    /// thrown away: it is to be said back, unless the test does that
    /// itself.
    fn reached(&mut self, name: String) {
        if self.by_itself {
            self.owed.push_back(name);
        }
    }

    /// Says back the marks that are owed. One that is being said when
    /// whoever waits gives up is on its way, and the rest are said the
    /// next time.
    async fn settles(&mut self) -> Result<(), PortError> {
        while let Some(name) = self.owed.pop_front() {
            self.echoes(&name).await?;
        }
        Ok(())
    }

    fn numbered(&mut self) -> String {
        let number = self.sequence;
        self.sequence += 1;
        number.to_string()
    }

    /// Says that the stream is there, and that the audio of the call
    /// begins.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn begins(&mut self, parameters: &[(String, String)]) -> Result<(), PortError> {
        let Carrier::Twilio = self.carrier;
        let hello = json!({"event": "connected", "protocol": "Call", "version": "1.0.0"});
        self.tells(hello.to_string()).await?;
        let parameters: serde_json::Map<String, serde_json::Value> = parameters
            .iter()
            .map(|(name, value)| (name.clone(), json!(value)))
            .collect();
        let start = json!({
            "event": "start",
            "sequenceNumber": self.numbered(),
            "start": {
                "accountSid": self.ringing.account,
                "streamSid": self.ringing.stream,
                "callSid": self.ringing.call,
                "tracks": ["inbound"],
                "mediaFormat": {"encoding": "audio/x-mulaw", "sampleRate": 8000, "channels": 1},
                "customParameters": parameters,
            },
            "streamSid": self.ringing.stream,
        });
        self.tells(start.to_string()).await
    }

    /// The caller says `audio`, which the carrier sends as one message.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn says(&mut self, audio: &[u8]) -> Result<(), PortError> {
        let Carrier::Twilio = self.carrier;
        let chunk = self.chunk;
        self.chunk += 1;
        // Where it begins, in milliseconds since the stream did.
        let at = self.said / 8;
        self.said += audio.len() as u64;
        let media = json!({
            "event": "media",
            "sequenceNumber": self.numbered(),
            "media": {
                "track": "inbound",
                "chunk": chunk.to_string(),
                "timestamp": at.to_string(),
                "payload": STANDARD.encode(audio),
            },
            "streamSid": self.ringing.stream,
        });
        self.tells(media.to_string()).await
    }

    /// The caller presses a key.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn presses(&mut self, key: Key) -> Result<(), PortError> {
        let Carrier::Twilio = self.carrier;
        let pressed = json!({
            "event": "dtmf",
            "streamSid": self.ringing.stream,
            "sequenceNumber": self.numbered(),
            "dtmf": {"track": "inbound", "digit": key.as_char().to_string()},
        });
        self.tells(pressed.to_string()).await
    }

    /// Says a mark back, whatever is held: as a carrier does when the
    /// caller has been played everything before it.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn echoes(&mut self, name: &str) -> Result<(), PortError> {
        let Carrier::Twilio = self.carrier;
        let mark = json!({
            "event": "mark",
            "sequenceNumber": self.numbered(),
            "streamSid": self.ringing.stream,
            "mark": {"name": name},
        });
        self.tells(mark.to_string()).await
    }

    /// Says `text` as it is, which may be nothing a carrier says.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn tells(&mut self, text: String) -> Result<(), PortError> {
        match self.conn.as_mut() {
            Some(conn) => conn.send_text(text).await,
            None => Err(gone()),
        }
    }

    /// Sends bytes that are no text, which no carrier does.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn tells_bytes(&mut self, bytes: Bytes) -> Result<(), PortError> {
        match self.conn.as_mut() {
            Some(conn) => conn.send_binary(bytes).await,
            None => Err(gone()),
        }
    }

    /// The next thing the server sends that a carrier takes. What it
    /// takes is held as a carrier holds it: audio until it is played, a
    /// mark until the audio before it has been, and a mark with nothing
    /// before it is said back at once. `None` when the server has closed
    /// the stream, which is how a session ends a call.
    ///
    /// Whoever gives up waiting for this loses nothing: a message is
    /// taken whole or not at all.
    ///
    /// # Errors
    ///
    /// The connection dropped.
    pub async fn next(&mut self) -> Result<Option<Sent>, PortError> {
        loop {
            // What a message owes is said back before the message is
            // told of: whoever gave up waiting while that was done
            // finds it here.
            self.settles().await?;
            if let Some(sent) = self.taken.take() {
                return Ok(Some(sent));
            }
            let Some(conn) = self.conn.as_mut() else {
                return Ok(None);
            };
            let text = match conn.next().await {
                Ok(Some(WsMessage::Text(text))) => text,
                Ok(Some(WsMessage::Binary(_))) => {
                    self.complaints.push("a message that is no text".to_owned());
                    continue;
                }
                Ok(None) => {
                    self.conn = None;
                    return Ok(None);
                }
                Err(error) => {
                    self.conn = None;
                    return Err(error);
                }
            };
            let Some(sent) = self.taken(text.as_str()) else {
                continue;
            };
            match &sent {
                Sent::Audio(audio) => self.held.push_back(Held::Audio(audio.clone())),
                Sent::Mark(name) if self.held.is_empty() => self.reached(name.clone()),
                Sent::Mark(name) => self.held.push_back(Held::Mark(name.clone())),
                Sent::Clear => {
                    // Everything goes, and every mark that goes is said
                    // back.
                    for held in core::mem::take(&mut self.held) {
                        if let Held::Mark(name) = held {
                            self.reached(name);
                        }
                    }
                }
            }
            self.taken = Some(sent);
        }
    }

    /// What a message says, if it is one a carrier takes. One that is
    /// not is complained of.
    fn taken(&mut self, text: &str) -> Option<Sent> {
        let Carrier::Twilio = self.carrier;
        let (stream, sent) = match serde_json::from_str::<ToTwilio>(text) {
            Ok(ToTwilio::Media { stream, media }) => {
                let Ok(audio) = STANDARD.decode(media.payload) else {
                    self.complaints
                        .push("audio that is not written in base64".to_owned());
                    return None;
                };
                (stream, Sent::Audio(Bytes::from(audio)))
            }
            Ok(ToTwilio::Mark { stream, mark }) => (stream, Sent::Mark(mark.name)),
            Ok(ToTwilio::Clear { stream }) => (stream, Sent::Clear),
            Err(error) => {
                // What the parser says of it names the field, and holds
                // nothing of the audio.
                self.complaints
                    .push(format!("a message a carrier does not take: {error}"));
                return None;
            }
        };
        if stream != self.ringing.stream {
            self.complaints.push(format!(
                "a message for the stream {stream}, which is not this one"
            ));
            return None;
        }
        Some(sent)
    }

    /// The caller is played up to `samples` samples of what is held, and
    /// every mark that is reached is said back. Returns what was played.
    ///
    /// # Errors
    ///
    /// The connection is gone, so a mark cannot be said back.
    pub async fn plays(&mut self, samples: usize) -> Result<Bytes, PortError> {
        // A sample of a telephone's audio is a byte.
        let mut left = samples;
        let mut played = Vec::new();
        loop {
            match self.held.pop_front() {
                Some(Held::Mark(name)) => self.reached(name),
                Some(Held::Audio(mut audio)) if left > 0 => {
                    let now = audio.split_to(left.min(audio.len()));
                    left -= now.len();
                    played.extend_from_slice(&now);
                    if !audio.is_empty() {
                        self.held.push_front(Held::Audio(audio));
                    }
                }
                Some(audio) => {
                    self.held.push_front(audio);
                    break;
                }
                None => break,
            }
        }
        self.settles().await?;
        Ok(played.into())
    }

    /// How many samples it holds that the caller has not been played.
    #[must_use]
    pub fn holds(&self) -> usize {
        self.held
            .iter()
            .map(|held| match held {
                Held::Audio(audio) => audio.len(),
                Held::Mark(_) => 0,
            })
            .sum()
    }

    /// What it was sent that a carrier warns of and does not take.
    #[must_use]
    pub fn complaints(&self) -> &[String] {
        &self.complaints
    }

    /// The carrier says that the call is over, and leaves the stream
    /// open for the server to close.
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub async fn stops(&mut self) -> Result<(), PortError> {
        let Carrier::Twilio = self.carrier;
        let stop = json!({
            "event": "stop",
            "sequenceNumber": self.numbered(),
            "stop": {"accountSid": self.ringing.account, "callSid": self.ringing.call},
            "streamSid": self.ringing.stream,
        });
        self.tells(stop.to_string()).await
    }

    /// The caller hangs up: the carrier says that the call is over, and
    /// closes the stream.
    ///
    /// # Errors
    ///
    /// The connection was gone already.
    pub async fn hangs_up(&mut self) -> Result<(), PortError> {
        self.stops().await?;
        if let Some(conn) = self.conn.take() {
            conn.close().await;
        }
        Ok(())
    }

    /// The stream is closed without a word, as when a carrier is gone.
    pub async fn drops(&mut self) {
        if let Some(conn) = self.conn.take() {
            conn.close().await;
        }
    }
}

fn gone() -> PortError {
    PortError::transient("the stream is closed")
}

#[cfg(test)]
mod tests {
    use super::{Answered, Ringing};
    use crate::Carrier;

    #[test]
    fn an_answer_is_read_as_a_carrier_reads_it() {
        let document = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Response>\n  <Connect>\n",
            "    <Stream url='wss://voice.example/stream'>\n",
            "      <Parameter value=\"a&lt;b &amp; &quot;c&quot; &apos;d&apos; &gt;\" name=\"note\" />\n",
            "      <Parameter name=\"once\" value=\"a-made-up-ticket\"/>\n",
            "    </Stream>\n  </Connect>\n</Response>\n",
        );
        let answered = Answered::read(Carrier::Twilio, document).unwrap();
        assert_eq!(answered.stream, "wss://voice.example/stream");
        let parameters: Vec<(&str, &str)> = answered
            .parameters
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            parameters,
            [("note", "a<b & \"c\" 'd' >"), ("once", "a-made-up-ticket")]
        );
        for none in [
            "",
            "<Response><Say>Hello.</Say></Response>",
            // A stream that is only started is not one a call is
            // connected to.
            "<Response><Start><Stream url=\"wss://voice.example/stream\"></Stream></Start></Response>",
            "<Response><Connect><Stream></Stream></Connect></Response>",
            "<Response><Connect><Stream url=\"wss://voice.example/&what;\"></Stream></Connect></Response>",
            "<Response><Connect><Stream url=\"wss://voice.example/stream\">",
        ] {
            assert_eq!(Answered::read(Carrier::Twilio, none), None, "{none}");
        }
    }

    #[test]
    fn a_call_that_is_made_up_has_the_shape_of_a_carrier_s() {
        let (first, second) = (Ringing::made_up(1), Ringing::made_up(2));
        assert_ne!(first.call, second.call);
        assert_ne!(first.stream, second.stream);
        for (name, begins) in [
            (&first.call, "CA"),
            (&first.stream, "MZ"),
            (&first.account, "AC"),
        ] {
            assert!(name.starts_with(begins) && name.len() == 34, "{name}");
        }
        let form = first.form(Carrier::Twilio);
        let field = |name: &str| {
            let found = form.iter().find(|(known, _)| known == name);
            found.map(|(_, value)| value.as_str())
        };
        assert_eq!(field("CallSid"), Some(first.call.as_str()));
        assert_eq!(field("AccountSid"), Some(first.account.as_str()));
        assert_eq!(field("From"), Some("+15550100"));
        assert_eq!(field("To"), Some("+15550101"));
    }
}
