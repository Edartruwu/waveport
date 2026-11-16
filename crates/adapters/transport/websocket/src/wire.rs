//! The messages of the protocol, as they are on the wire.
//!
//! Everything that is not audio is a text message that holds one JSON
//! object, and its `type` says what it is. Audio is a binary message that
//! holds samples and nothing else.

use serde::{Deserialize, Serialize};
use waveport_ports::error::PortError;

/// The name of the protocol, which a client gives as it connects.
pub const PROTOCOL: &str = "waveport.ws.v0";

/// The path a client asks for.
pub const PATH: &str = "/v0/session";

/// What a client says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromClient {
    /// The first thing a client says: the rate of its audio, both ways.
    Hello {
        /// Samples a second: 8,000, 16,000, 24,000 or 48,000.
        rate: u32,
    },
    /// How much of a response the client has played so far.
    Played {
        /// The response, by the number its `mark` gave it.
        run: u32,
        /// How many of its samples have been played.
        samples: u32,
    },
    /// Something typed in place of speech.
    Text {
        /// What was typed.
        text: String,
    },
    /// The client is leaving.
    Bye,
}

/// What a server says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromServer {
    /// The answer to `hello`: the session is there.
    Ready {
        /// What the session is called.
        session: String,
        /// The rate of the audio, both ways: the one the client named.
        rate: u32,
    },
    /// Where one response's audio begins, and where it ends.
    Mark {
        /// The number of the response. The audio that follows, up to the
        /// next mark, is its audio.
        run: u32,
        /// Whether this is the end of the response and not its
        /// beginning: all of its audio has been sent.
        #[serde(default, skip_serializing_if = "is_false")]
        end: bool,
    },
    /// What the client holds of this response, and of every one before
    /// it, and has not played is to be discarded.
    Clear {
        /// The number of the response.
        run: u32,
    },
    /// A line about the call, for a person to read. What it says is not
    /// part of the protocol.
    Event {
        /// The line.
        text: String,
    },
    /// The session is over. The connection closes after this.
    Bye {
        /// Why, in a few words.
        reason: String,
    },
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde gives a reference to the field"
)]
const fn is_false(flag: &bool) -> bool {
    !*flag
}

/// A message as the text that is sent.
pub(crate) fn write<M: Serialize>(message: &M) -> String {
    // Neither kind of message holds anything that cannot be written.
    serde_json::to_string(message).unwrap_or_default()
}

/// A message from the text that arrived.
pub(crate) fn read<'a, M: Deserialize<'a>>(text: &'a str) -> Result<M, PortError> {
    serde_json::from_str(text).map_err(|error| {
        let shown: String = text.chars().take(80).collect();
        PortError::permanent(format!("{shown:?} is not a message of {PROTOCOL}: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::{FromClient, FromServer, read, write};

    #[test]
    fn messages_are_written_as_the_protocol_has_them() {
        for (message, text) in [
            (
                FromServer::Ready {
                    session: "s-1".to_owned(),
                    rate: 24_000,
                },
                r#"{"type":"ready","session":"s-1","rate":24000}"#,
            ),
            (
                FromServer::Mark { run: 3, end: false },
                r#"{"type":"mark","run":3}"#,
            ),
            (
                FromServer::Mark { run: 3, end: true },
                r#"{"type":"mark","run":3,"end":true}"#,
            ),
            (FromServer::Clear { run: 3 }, r#"{"type":"clear","run":3}"#),
            (
                FromServer::Event {
                    text: "caller: hello".to_owned(),
                },
                r#"{"type":"event","text":"caller: hello"}"#,
            ),
            (
                FromServer::Bye {
                    reason: "the call ended".to_owned(),
                },
                r#"{"type":"bye","reason":"the call ended"}"#,
            ),
        ] {
            assert_eq!(write(&message), text);
            assert_eq!(read::<FromServer>(text).unwrap(), message);
        }
        for (message, text) in [
            (
                FromClient::Hello { rate: 48_000 },
                r#"{"type":"hello","rate":48000}"#,
            ),
            (
                FromClient::Played {
                    run: 3,
                    samples: 960,
                },
                r#"{"type":"played","run":3,"samples":960}"#,
            ),
            (
                FromClient::Text {
                    text: "hello".to_owned(),
                },
                r#"{"type":"text","text":"hello"}"#,
            ),
            (FromClient::Bye, r#"{"type":"bye"}"#),
        ] {
            assert_eq!(write(&message), text);
            assert_eq!(read::<FromClient>(text).unwrap(), message);
        }
    }

    #[test]
    fn what_is_not_a_message_is_said_to_be_none_and_shown_in_part() {
        for text in [
            "",
            "hello",
            "{}",
            r#"{"type":"dance"}"#,
            r#"{"type":"hello"}"#,
        ] {
            let error = read::<FromClient>(text).unwrap_err();
            assert!(error.message.contains("waveport.ws.v0"), "{error}");
        }
        let long = "x".repeat(10_000);
        let error = read::<FromClient>(&long).unwrap_err();
        assert!(error.message.len() < 300, "{}", error.message.len());
    }
}
