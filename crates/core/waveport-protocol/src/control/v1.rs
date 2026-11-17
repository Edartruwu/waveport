//! Control API, version 1.
//!
//! What an application says to a server that runs sessions, and what it
//! is answered: the bodies of the requests and the answers, as JSON.
//! Which address takes which, and who may ask, is the server's to say.
//! The same bodies are what the gRPC API carries in its own encoding.
//!
//! An application makes a session of an agent ([`CreateSession`]) and is
//! given its name and a client token ([`SessionCreated`]), which it
//! hands to the browser, the telephone bridge or the program that is to
//! be the session's far end. The token lets its holder connect to that
//! session, once, and is no key to anything else: the application's own
//! key never leaves it. The far end then speaks the client protocol
//! ([`client::v1`](crate::client::v1)).
//!
//! An application asks things of a session that is running ([`Input`])
//! and can follow one: what a follower is sent are the messages a server
//! says in the client protocol, the facts with their numbers and the
//! news, and a follower that comes back says which life of the session
//! it was told of and the number of the last fact it was sent.
//!
//! What is asked and cannot be done is answered with a [`Problem`],
//! which has the members of RFC 9457 and one of its own, a code for a
//! program.
//!
//! As in the client protocol, a reader skips a field it does not know
//! and takes a kind it does not know as `Unknown`.

use serde::{Deserialize, Serialize};

use crate::client::v1::{ErrorCode, Refusal, checked_keys, checked_text};

/// The version of the API.
pub const VERSION: u32 = 1;

/// How a session is run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// It hears and speaks.
    Voice,
    /// It reads and writes.
    Text,
    /// A way this version does not have.
    #[serde(other)]
    Unknown,
}

/// Where a session stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// It was made, and its far end has not connected yet.
    Waiting,
    /// It is running.
    Running,
    /// Its line dropped, and it waits for the far end to come back.
    Detached,
    /// It is over.
    Ended,
    /// The process that ran it died, and nobody has gone on with it. It
    /// is kept, and can be gone on with.
    Broken,
    /// A state this version does not have.
    #[serde(other)]
    Unknown,
}

/// The request for a new session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    /// Which agent, by the name the server's configuration gives it.
    pub agent: String,
}

/// The answer to [`CreateSession`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    /// What the session is called.
    pub session: String,
    /// What lets one far end connect to the session, once.
    pub client_token: String,
    /// How long the token can be connected with, in milliseconds.
    pub connect_within_ms: u32,
}

/// What a server says of one session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// What the session is called.
    pub session: String,
    /// Its agent, by the name the server's configuration gives it.
    pub agent: String,
    /// Where it stands.
    pub state: SessionState,
    /// How it is run, once that is known: a session that waits for its
    /// far end is run as the line allows that the far end comes on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// Which of its lives it is in: the number goes up each time it is
    /// rebuilt from what was stored of it, after its process died.
    pub life: u64,
    /// The number the next fact gets that comes about in it.
    pub next_seq: u64,
}

/// What a server says of its sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sessions {
    /// The sessions.
    pub sessions: Vec<SessionInfo>,
}

/// What an agent that takes the floor is told of the conversation so
/// far.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    /// Everything.
    #[default]
    Full,
    /// This summary.
    Summary(String),
    /// Nothing.
    None,
}

/// What an application asks of a session that is running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    /// A participant typed instead of speaking.
    Text {
        /// What they typed.
        text: String,
        /// Who, if not the session's first participant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        who: Option<u16>,
    },
    /// The agent is to say this, whatever it was doing.
    Say {
        /// What to say.
        text: String,
    },
    /// The session is to press these keys of its telephone, one after
    /// the other: characters of [`KEYS`](crate::client::v1::KEYS).
    Press {
        /// The keys.
        keys: String,
    },
    /// The agent is to stop speaking.
    Interrupt,
    /// Another agent is to take the floor.
    Handoff {
        /// The agent.
        to: u8,
        /// What it is told of the conversation so far.
        #[serde(default)]
        context: Context,
    },
    /// The session is to end, once the agent has finished what it is
    /// saying.
    Close,
    /// Something this version does not have.
    #[serde(other)]
    Unknown,
}

impl Input {
    /// Whether what is asked is something that can be.
    ///
    /// # Errors
    ///
    /// What is wrong with it.
    pub fn check(&self) -> Result<(), Refusal> {
        match self {
            Self::Text { text, .. }
            | Self::Say { text }
            | Self::Handoff {
                context: Context::Summary(text),
                ..
            } => checked_text(text),
            Self::Press { keys } => checked_keys(keys),
            Self::Interrupt | Self::Handoff { .. } | Self::Close => Ok(()),
            Self::Unknown => Err(Refusal::Unknown),
        }
    }
}

/// Why something that was asked was not done: the body of an answer
/// that is not a success, of the type `application/problem+json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// The status the answer has in HTTP.
    pub status: u16,
    /// What that status is called.
    pub title: String,
    /// Why, for a program.
    pub code: ErrorCode,
    /// Why, for a person. What it says is not part of the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Problem {
    /// The problem a code stands for, with the status that says it in
    /// HTTP.
    #[must_use]
    pub fn of(code: ErrorCode, detail: impl Into<String>) -> Self {
        let (status, title) = match code {
            ErrorCode::Version | ErrorCode::Malformed | ErrorCode::UnknownMessage => {
                (400, "Bad Request")
            }
            ErrorCode::Token => (401, "Unauthorized"),
            ErrorCode::Forbidden => (403, "Forbidden"),
            ErrorCode::NoSession | ErrorCode::NoAgent => (404, "Not Found"),
            ErrorCode::OutOfPlace => (409, "Conflict"),
            ErrorCode::Unsupported => (422, "Unprocessable Content"),
            ErrorCode::Busy => (503, "Service Unavailable"),
            ErrorCode::Internal | ErrorCode::Unknown => (500, "Internal Server Error"),
        };
        let detail = detail.into();
        Self {
            status,
            title: title.to_owned(),
            code,
            detail: (!detail.is_empty()).then_some(detail),
        }
    }
}
