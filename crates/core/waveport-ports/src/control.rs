//! How an application drives sessions that are running.
//!
//! The other ports are what a session reaches the world through. This one
//! is what the world reaches a session through: something is asked of it,
//! or someone wants to be told what happens in it.
//!
//! Whoever follows a session is told two kinds of thing. Facts are what
//! the session's journal holds, each with its number, in the form the
//! journal holds them in. The rest is news that is not kept: who is
//! speaking, what an agent is doing. A follower that comes back after a
//! break says the number of the first fact it has not seen, and is given
//! the facts from there, as far back as they are still held.
//!
//! Starting a session is not here. What a session is made of is decided
//! where the parts are put together, and a port cannot name that yet.

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};

use bytes::Bytes;
use waveport_domain::command::ClientEvent;
use waveport_domain::event::HandoffContext;
use waveport_domain::ids::{AgentId, ParticipantId, Seq};

use crate::journal::SessionId;

/// What an application asks of a session that is running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionInput {
    /// A participant typed instead of speaking.
    Text {
        /// Who.
        who: ParticipantId,
        /// What they typed.
        text: String,
    },
    /// The agent is to say this, whatever it was doing.
    Say {
        /// What to say.
        text: String,
    },
    /// The agent is to stop speaking.
    Interrupt,
    /// Another agent is to take the floor.
    Handoff {
        /// The agent.
        to: AgentId,
        /// What it is told of the conversation so far.
        context: HandoffContext,
    },
    /// The session is to end, once the agent has finished what it is
    /// saying.
    Close,
}

/// What a follower of a session is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// A fact was recorded.
    Fact {
        /// Its number. Facts are numbered from 1 without gaps.
        seq: Seq,
        /// The fact, as the journal holds it.
        record: Bytes,
    },
    /// News that is not kept.
    Told(ClientEvent),
    /// The facts asked for are no longer all held. What follows begins
    /// with the fact of this number, and the ones before it have to be
    /// had from somewhere else.
    Gap {
        /// The number of the first fact that follows.
        resumes: Seq,
    },
}

/// Why something could not be asked of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ControlError {
    /// No session of that name is running here.
    NoSuchSession,
    /// The session has too much waiting for it already. The request was
    /// not taken.
    Behind,
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchSession => f.write_str("no session of that name is running here"),
            Self::Behind => f.write_str("the session has too much waiting for it already"),
        }
    }
}

impl core::error::Error for ControlError {}

/// What a session tells one follower, in order.
///
/// It ends when the session does, or when the follower has fallen too far
/// behind. A follower that was cut off for that follows again, from the
/// fact after the last it saw.
pub trait EventRx: Send {
    /// The next thing the follower is told, or `None` when there is
    /// nothing more.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<SessionEvent>>;
}

/// The sessions that are running in one place.
pub trait SessionControl: Send + Sync + 'static {
    /// Asks something of a session. It is taken or refused at once.
    ///
    /// # Errors
    ///
    /// The session is not running here, or has no room for the request.
    fn send(&self, session: SessionId, input: SessionInput) -> Result<(), ControlError>;

    /// Follows a session from the fact numbered `from`. To be told
    /// everything a session has recorded, follow it from fact 1; to be
    /// told only what happens from now on, from a number that no fact
    /// has yet, such as `Seq(u64::MAX)`.
    ///
    /// # Errors
    ///
    /// The session is not running here, or has no room for the request.
    fn subscribe(
        &self,
        session: SessionId,
        from: Seq,
    ) -> Result<Pin<Box<dyn EventRx>>, ControlError>;
}
