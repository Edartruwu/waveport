//! What the transports share whose far end speaks client protocol v1: a
//! WebSocket to a client, or the data channel of a WebRTC call.
//!
//! A message of the protocol is a text that holds one JSON object
//! ([`read()`], [`write()`]). A connection begins with what its two ends
//! say of themselves: the client its [`Hello`], and the server, once it
//! has taken the client, its [`Welcome`].

use core::fmt;

use serde::{Deserialize, Serialize};
use waveport_ports::error::PortError;
use waveport_protocol::client::v1::{self as client, AudioFormat, FromClient, FromServer, Resume};

/// What a client says as it comes: its hello, without the version.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Hello {
    /// Its client token, where the server asks for one.
    pub token: Option<String>,
    /// The audio it sends and wants. None for a client that writes.
    pub audio: Option<AudioFormat>,
    /// What it wants to be told beyond the facts, by the names of the
    /// protocol's features.
    pub features: Vec<String>,
    /// Where it was, if it comes back to a session.
    pub resume: Option<Resume>,
}

impl Hello {
    /// The hello of a client that speaks and hears `audio`, or writes
    /// if that is none, and holds no token.
    #[must_use]
    pub const fn new(audio: Option<AudioFormat>) -> Self {
        Self {
            token: None,
            audio,
            features: Vec::new(),
            resume: None,
        }
    }

    /// As the message that is sent.
    #[must_use]
    pub fn said(&self) -> FromClient {
        FromClient::Hello {
            v: client::VERSION,
            token: self.token.clone(),
            audio: self.audio,
            features: self.features.clone(),
            resume: self.resume.clone(),
        }
    }
}

impl fmt::Debug for Hello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Its tokens let their holder into a session, and are not shown.
        f.debug_struct("Hello")
            .field("audio", &self.audio)
            .field("features", &self.features)
            .field("comes_back", &self.resume.is_some())
            .finish_non_exhaustive()
    }
}

/// What a client is told as it is taken. A server is given it by whoever
/// runs the session; the version and the audio are the connection's, and
/// are not in it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Welcome {
    /// What the session is called.
    pub session: String,
    /// Which life of the session this is.
    pub life: u64,
    /// Whether the client came back to a session that was under way.
    pub resumed: bool,
    /// What lets the client come back, if the session is kept where that
    /// can be done.
    pub resume_token: Option<String>,
    /// Which of the features the client asked for it will be told.
    pub features: Vec<String>,
    /// The number the next fact gets that comes about in the session.
    pub next_seq: u64,
    /// How long the session waits for a client whose connection
    /// dropped, in milliseconds.
    pub grace_ms: Option<u32>,
}

impl Welcome {
    /// As the message that is sent on a connection whose audio is
    /// `audio`.
    #[must_use]
    pub fn said(self, audio: Option<AudioFormat>) -> FromServer {
        FromServer::Welcome {
            v: client::VERSION,
            session: self.session,
            life: self.life,
            resumed: self.resumed,
            resume_token: self.resume_token,
            audio,
            features: self.features,
            next_seq: self.next_seq,
            grace_ms: self.grace_ms,
        }
    }
}

impl fmt::Debug for Welcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The resume token is not shown.
        f.debug_struct("Welcome")
            .field("session", &self.session)
            .field("life", &self.life)
            .field("resumed", &self.resumed)
            .field("features", &self.features)
            .field("next_seq", &self.next_seq)
            .field("grace_ms", &self.grace_ms)
            .finish_non_exhaustive()
    }
}

/// A message as the text that is sent.
#[must_use]
pub fn write<M: Serialize>(message: &M) -> String {
    // No message holds anything that cannot be written.
    serde_json::to_string(message).unwrap_or_default()
}

/// A message from the text that arrived.
///
/// # Errors
///
/// The text is not a message of the kind that is read. The error does
/// not repeat it.
pub fn read<'a, M: Deserialize<'a>>(text: &'a str) -> Result<M, PortError> {
    serde_json::from_str(text).map_err(|error| {
        // What arrived is not repeated, and neither is what the reader
        // says of it, which can quote it: a message may hold a token.
        let what = match error.classify() {
            serde_json::error::Category::Data => "it does not say what a message of its kind says",
            serde_json::error::Category::Eof => "it breaks off",
            serde_json::error::Category::Syntax | serde_json::error::Category::Io => {
                "it is not JSON"
            }
        };
        PortError::permanent(format!(
            "what arrived is not a message of {}: {what}",
            client::NAME
        ))
    })
}

#[cfg(test)]
mod tests {
    use waveport_protocol::client::v1::{
        AudioFormat, Codec, FromClient, FromServer, Resume, VERSION,
    };

    use super::{Hello, Welcome, read, write};

    const AUDIO: AudioFormat = AudioFormat {
        codec: Codec::Pcm16,
        rate: 24_000,
    };

    #[test]
    fn a_hello_and_a_welcome_are_said_as_the_protocol_has_them() {
        let hello = Hello {
            token: Some("wpc1_madeup".to_owned()),
            audio: Some(AUDIO),
            features: vec!["said".to_owned()],
            resume: Some(Resume {
                token: "wpr1_madeup".to_owned(),
                life: 2,
                seen: 17,
            }),
        };
        let said = write(&hello.said());
        assert_eq!(
            said,
            r#"{"type":"hello","v":1,"token":"wpc1_madeup","audio":{"codec":"pcm16","rate":24000},"features":["said"],"resume":{"token":"wpr1_madeup","life":2,"seen":17}}"#
        );
        assert_eq!(read::<FromClient>(&said).unwrap(), hello.said());
        assert_eq!(write(&Hello::new(None).said()), r#"{"type":"hello","v":1}"#);

        let welcome = Welcome {
            session: "s-1".to_owned(),
            life: 2,
            resumed: true,
            resume_token: Some("wpr1_madeup".to_owned()),
            features: vec!["said".to_owned()],
            next_seq: 18,
            grace_ms: Some(30_000),
        };
        let FromServer::Welcome { v, audio, .. } = welcome.clone().said(Some(AUDIO)) else {
            panic!("a welcome is said as one");
        };
        assert_eq!((v, audio), (VERSION, Some(AUDIO)));
        assert_eq!(
            write(&welcome.said(None)),
            r#"{"type":"welcome","v":1,"session":"s-1","life":2,"resumed":true,"resume_token":"wpr1_madeup","features":["said"],"next_seq":18,"grace_ms":30000}"#
        );
    }

    #[test]
    fn what_is_shown_of_a_hello_and_a_welcome_holds_no_token() {
        let hello = Hello {
            token: Some("wpc1_madeup".to_owned()),
            resume: Some(Resume {
                token: "wpr1_madeup".to_owned(),
                life: 1,
                seen: 0,
            }),
            ..Hello::new(Some(AUDIO))
        };
        let welcome = Welcome {
            resume_token: Some("wpr1_madeup".to_owned()),
            ..Welcome::default()
        };
        let shown = format!("{hello:?} {welcome:?}");
        assert!(!shown.contains("madeup"), "{shown}");
        assert!(shown.contains("comes_back: true"), "{shown}");
    }

    #[test]
    fn what_cannot_be_read_is_not_repeated() {
        // A hello whose version is a token, half a hello, and no JSON.
        for text in [
            r#"{"type":"hello","v":"wpc1_madeup"}"#,
            r#"{"type":"hello","v":1,"token":"wpc1_madeup""#,
            "wpc1_madeup",
        ] {
            let error = read::<FromClient>(text).unwrap_err().to_string();
            assert!(!error.contains("madeup"), "{error}");
            assert!(error.contains("waveport.client.v1"), "{error}");
        }
    }
}
