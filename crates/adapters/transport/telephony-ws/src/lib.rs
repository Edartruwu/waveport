//! A session whose far end is a telephone call that a carrier streams
//! over a WebSocket.
//!
//! Stability: experimental
//!
//! A carrier that takes a call asks an address of the application what
//! to do with it. The answer connects the call to a WebSocket, and the
//! carrier sends the caller's audio over it and plays what it is sent.
//! This crate is that end of the WebSocket as a transport, and the two
//! things around it that are a carrier's own: the document a call is
//! answered with, and the check that a request is the carrier's.
//!
//! **It has not been held against a carrier yet.** Everything here was
//! written from the carriers' documentation, as read on the day each
//! module names, and is tested against a stand-in that does what the
//! documentation says a carrier does ([`sim`]). The first call through
//! a real carrier is what will show where the two differ.
//!
//! # The carriers
//!
//! | | Audio | What was played | Discarding | Keys out | A request is the carrier's when |
//! |---|---|---|---|---|---|
//! | [`Carrier::Twilio`] | G.711 µ-law at 8 kHz | a mark where a response begins and after every frame of it, which the carrier says back when it has played up to there | `clear` | tones in the audio | it is signed with the account's token (HMAC-SHA1 over the address and the fields of the form) |
//!
//! # What the documentation taught
//!
//! Twilio, read on 17 November 2026:
//!
//! - **The audio is that of a telephone and nothing else.** µ-law at
//!   8 kHz, one channel, both ways, as text messages that hold it in
//!   base64. A stream that says it carries anything else is not taken.
//! - **A mark is how a server learns what was played.** A mark is a name
//!   put after some audio, and the carrier says it back when the caller
//!   has been played everything before it. The transport puts one where
//!   a response begins and one after every frame of it, named by the
//!   response and by how much of it was sent (`r3.160`, and `r3.480.end`
//!   for the last), so a session knows when the caller began to hear a
//!   response and how far, to within a frame.
//! - **A discard empties everything and says back every mark it throws
//!   away.** Those are not audio that was played, so no mark of the
//!   response that was discarded, or of one before it, is believed from
//!   then on. What the caller had heard is what the last mark before the
//!   discard said: a mark that was on its way back when the discard was
//!   sent cannot be told from one that was thrown away, so what was
//!   heard may be a few frames more than is known, and is never less.
//! - **Keys arrive as messages and cannot be sent as such.** A key the
//!   session presses is a tone in its audio ([`KeysOut::Tones`]).
//! - **The address of a stream carries nothing after a `?`.** What a
//!   stream is to say as it begins goes into the answer as parameters,
//!   which the carrier says back in its first message
//!   ([`Started::parameters`]). That is how a server knows which call a
//!   stream is of without trusting the address it was asked at.
//! - **A request is signed, and nothing in it says when.** The
//!   signature is over the address and the fields, with no time and no
//!   number that is used once: a request that was signed once is good
//!   again. Whoever answers calls remembers the calls it answered
//!   ([`Started::call`]), and gives a stream something that is good
//!   once.
//! - **What is not written down.** How the request for a stream is
//!   signed is described in one sentence: a stroke may have to be added
//!   to its address. So [`Carrier::verify`] tries the address in each
//!   way it may have been signed, with and without its port, with and
//!   without that stroke, and for a stream as a `wss` and as an `https`
//!   address. What becomes of a field that a form gives twice is not
//!   written down either. Whether a mark after every frame is more
//!   than the carrier likes is not known.
//! - **Not taken:** a request that was signed with a key of its own in
//!   place of the account's token, which an account can turn on, is
//!   refused and said to be that.
//!
//! # For a server
//!
//! ```text
//! // The carrier asks what to do with a call.
//! carrier.verify(&token, &asked)?;
//! let answer = carrier.answer("wss://voice.example/stream", &[("once", ticket)])?;
//! // ... answer.body is the answer, as answer.content_type ...
//!
//! // The carrier connects, and the server makes a WebSocket of it.
//! let call = Call::of(carrier, conn, STARTS_WITHIN).await?;
//! // call.started() says which call it is, and what the answer gave it.
//! let transport = call.transport(who);
//! ```
//!
//! A call that drops is over: a carrier does not bring a call back to a
//! stream it left, so the transport says that its far end has left and
//! never that the line dropped.
//!
//! [`KeysOut::Tones`]: waveport_ports::transport::KeysOut::Tones

use core::fmt;
use std::collections::BTreeMap;

use bytes::Bytes;
use waveport_adapter_kit::secret::Secret;
use waveport_domain::event::Key;
use waveport_ports::audio::Law;
use waveport_ports::transport::KeysOut;

mod call;
pub mod sim;
mod twilio;

pub use crate::call::{Call, CarrierTransport, STARTS_WITHIN};

/// A carrier whose media streams are spoken here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Carrier {
    /// Twilio's Media Streams, on a call that was connected to one
    /// (`<Connect><Stream>`).
    Twilio,
}

impl Carrier {
    /// Every carrier there is.
    pub const ALL: [Self; 1] = [Self::Twilio];

    /// What it is called in an address and in a configuration.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Twilio => "twilio",
        }
    }

    /// The carrier of that name.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|carrier| carrier.name() == name)
    }

    /// The law its audio is written in, at 8 kHz.
    #[must_use]
    pub const fn law(self) -> Law {
        match self {
            Self::Twilio => Law::Mu,
        }
    }

    /// How a key that the session presses reaches the caller.
    #[must_use]
    pub const fn keys_out(self) -> KeysOut {
        match self {
            Self::Twilio => KeysOut::Tones,
        }
    }

    /// The document a call is answered with, which connects it to the
    /// stream at `stream` and gives the stream `parameters` to say as it
    /// begins.
    ///
    /// # Errors
    ///
    /// The carrier would not take the answer: the stream is not reached
    /// over TLS, its address says something after a `?`, or a parameter
    /// is longer than one may be.
    pub fn answer(self, stream: &str, parameters: &[(&str, &str)]) -> Result<Answer, Unanswerable> {
        match self {
            Self::Twilio => Ok(Answer {
                content_type: "text/xml; charset=utf-8",
                body: twilio::answer(stream, parameters)?,
            }),
        }
    }

    /// Whether a request is the carrier's: one that was made for the
    /// account `secret` is of.
    ///
    /// # Errors
    ///
    /// It is not signed, or not with that secret, or in a way that is
    /// not taken here. The error says which, and nothing of the secret
    /// or of what the request was signed with.
    pub fn verify(self, secret: &Secret, asked: &Asked<'_>) -> Result<(), Unverified> {
        match self {
            Self::Twilio => twilio::verify(secret, asked),
        }
    }

    /// What the carrier signs a request to `url` with, whose form holds
    /// `form`, for the account `secret` is of. For whoever stands in for
    /// a carrier: a test, and [`sim`].
    #[must_use]
    pub fn sign(self, secret: &Secret, url: &str, form: &[(String, String)]) -> String {
        match self {
            Self::Twilio => twilio::signature(secret, url, form),
        }
    }

    /// The header the carrier's signature is in, in lower case.
    #[must_use]
    pub const fn signed_in(self) -> &'static str {
        match self {
            Self::Twilio => "x-twilio-signature",
        }
    }

    pub(crate) fn read(self, text: &str) -> Result<Heard, Malformed> {
        match self {
            Self::Twilio => twilio::read(text),
        }
    }

    pub(crate) fn media(self, stream: &str, audio: &[u8]) -> String {
        match self {
            Self::Twilio => twilio::media(stream, audio),
        }
    }

    pub(crate) fn mark(self, stream: &str, name: &str) -> String {
        match self {
            Self::Twilio => twilio::mark(stream, name),
        }
    }

    pub(crate) fn clear(self, stream: &str) -> String {
        match self {
            Self::Twilio => twilio::clear(stream),
        }
    }
}

impl fmt::Display for Carrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a carrier says as the audio of a call begins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    /// The carrier's name for the stream.
    pub stream: String,
    /// The carrier's name for the call. A call that was answered is not
    /// answered again: a request that was signed once is signed for
    /// good.
    pub call: String,
    /// The account the call is of, where the carrier says.
    pub account: Option<String>,
    /// What the answer gave the stream to say.
    pub parameters: BTreeMap<String, String>,
}

/// The answer to a carrier that asks what to do with a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// What kind of document it is.
    pub content_type: &'static str,
    /// The document.
    pub body: String,
}

/// Why a call cannot be answered with a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unanswerable {
    /// The stream is not reached over TLS (`wss://`).
    NotSecure,
    /// The address of the stream says something after a `?` or a `#`.
    Query,
    /// A parameter has no name, or is longer than one may be.
    Parameter,
    /// Something holds a character a document cannot.
    Unwritable,
}

impl fmt::Display for Unanswerable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotSecure => "a carrier connects a call to a stream over TLS: `wss://`",
            Self::Query => {
                "the address of a stream says nothing after a `?`: a carrier would not ask for \
                 it. What the stream is to say goes into parameters"
            }
            Self::Parameter => "a parameter has a name, and is shorter than 500 characters in all",
            Self::Unwritable => "the answer holds a character a document cannot",
        })
    }
}

impl core::error::Error for Unanswerable {}

/// A request, as it arrived, for whoever checks that it is a carrier's.
#[derive(Clone, Copy, Debug)]
pub struct Asked<'a> {
    /// The address that was asked, as the carrier wrote it: the one the
    /// server is reached at from outside, with what follows its `?`.
    pub url: &'a str,
    /// The headers, with their names in lower case.
    pub headers: &'a [(&'a str, &'a str)],
    /// The fields of the form that was posted, as they read once they
    /// are decoded, in any order. None for a request without a form.
    pub form: &'a [(String, String)],
    /// The body as it arrived.
    pub body: &'a [u8],
}

impl<'a> Asked<'a> {
    /// The header `name`, which is written in lower case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, value)| *value)
    }
}

/// Why a request is not taken for a carrier's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unverified {
    /// It is not signed.
    Unsigned,
    /// It was not signed with the secret, or not as it arrived.
    Mismatch,
    /// Its body is not the one that was signed.
    Body,
    /// It was signed in a way that is not taken here.
    SignedOtherwise,
}

impl fmt::Display for Unverified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsigned => "the request is not signed",
            Self::Mismatch => {
                "the request was not signed with the account's secret, or is not asked at the \
                 address it was signed for"
            }
            Self::Body => "the body of the request is not the one that was signed",
            Self::SignedOtherwise => {
                "the request was signed with a key of its own, which is not taken here: the \
                 account's token is"
            }
        })
    }
}

impl core::error::Error for Unverified {}

/// What a carrier says on its stream, in the words every carrier has
/// for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Heard {
    /// The stream is there, and nothing of the call is known yet.
    Hello,
    /// The audio of the call begins.
    Start(Started),
    /// Audio of the caller, as the carrier writes it.
    Audio(Bytes),
    /// A key the caller pressed.
    Key(Key),
    /// The caller has been played everything that was sent before the
    /// mark of this name.
    Mark(String),
    /// The call is over.
    Stop,
    /// Something that is of no use here.
    Other,
}

/// Why a message of a carrier's is not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Malformed {
    /// It is no message of the carrier's.
    Message,
    /// Its audio cannot be read.
    Audio,
    /// The stream carries audio that is not a telephone's.
    Format,
}

impl Malformed {
    pub(crate) const fn why(self) -> &'static str {
        match self {
            Self::Message => "the carrier said something that is no message of its stream",
            Self::Audio => "the carrier sent audio that cannot be read",
            Self::Format => "the carrier's stream carries audio that is not taken here",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Carrier;

    #[test]
    fn a_carrier_is_known_by_its_name() {
        for carrier in Carrier::ALL {
            assert_eq!(Carrier::named(carrier.name()), Some(carrier));
            assert_eq!(carrier.to_string(), carrier.name());
            // A name is part of an address.
            assert!(carrier.name().bytes().all(|byte| byte.is_ascii_lowercase()));
        }
        assert_eq!(Carrier::named("Twilio"), None);
        assert_eq!(Carrier::named(""), None);
    }
}
