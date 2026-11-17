//! How an application drives sessions that are running.
//!
//! The other ports are what a session reaches the world through. This one
//! is what the world reaches a session through: something is asked of it,
//! or someone wants to be told what happens in it.
//!
//! Whoever follows a session is told two kinds of thing. Facts are what
//! the session's journal holds, each with its number, in the form the
//! journal holds them in. The rest is news that is not kept: who is
//! speaking, what an agent is doing, the words of a response as they go
//! out, and the words of a speaker as far as they are known. A follower
//! that comes back after a break says the number of the first fact it
//! has not seen, and is given the facts from there, as far back as they
//! are still held. News it missed is not told again: what came of it is
//! in the facts.
//!
//! A fact is told when the session makes it, which is before its journal
//! has stored it. A process that dies in between leaves a journal that
//! never had the fact, and the session that goes on from that journal
//! gives its number to another. Whoever follows a session across the
//! death of its process has to be told where the journal stood, and
//! takes what it was told beyond that as not having happened.
//!
//! A follower that cannot go on from what it holds, because the facts it
//! missed are no longer held or because it was told of an earlier life
//! of the session, follows from the session's state instead: what the
//! facts add up to, as of the last of them, and then what happens from
//! there.
//!
//! Starting a session is not here. What a session is made of is decided
//! where the parts are put together, and a port cannot name that yet.

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};

use bytes::Bytes;
use waveport_domain::command::ClientEvent;
use waveport_domain::event::{HandoffContext, Key};
use waveport_domain::ids::{AgentId, GenId, ParticipantId, Seq};

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
    /// The session is to press these keys of its telephone, one after
    /// the other, for whoever is at the far end: a menu that asks for a
    /// number. They leave as the session's transport has them leave, and
    /// not at all on one that cannot press any, or while the line is
    /// gone. The conversation does not hold them: whoever asks knows what
    /// was asked.
    Press {
        /// The keys.
        keys: Vec<Key>,
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
    /// News that is not kept: text of a response that has gone out. In a
    /// session of text it has gone to the reader, and what is told is
    /// what the response is on record with. In a session that speaks it
    /// has gone to speech synthesis, ahead of what is heard, and what was
    /// heard of the response is the fact that ends it.
    Said {
        /// The response, by the number its facts carry. A response of
        /// which no fact was stored before the process died leaves its
        /// number to another.
        generation: GenId,
        /// The text, which follows what was told of the response before.
        text: String,
    },
    /// News that is not kept: what a participant has said of what they
    /// are saying, as far as the recogniser has it. It may still change.
    /// The words of a turn are the fact that commits it.
    Hearing {
        /// Who.
        who: ParticipantId,
        /// The words so far.
        text: String,
    },
    /// The facts asked for are no longer all held. What follows begins
    /// with the fact of this number, and the ones before it have to be
    /// had from somewhere else.
    Gap {
        /// The number of the first fact that follows.
        resumes: Seq,
    },
    /// What the session's facts add up to, as of one of them. It is the
    /// first thing told to a follower that asked for it
    /// ([`SessionControl::subscribe_from_state`]), and takes the place
    /// of whatever that follower held.
    State {
        /// The number of the last fact the state includes.
        upto: Seq,
        /// The state, as the journal holds one.
        state: Bytes,
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

    /// Follows a session from what it is now. The follower is told the
    /// session's state first ([`SessionEvent::State`]), as of the last
    /// fact there is, and then what happens from there. It is for a
    /// follower that cannot go on from what it holds: one that was told
    /// of a gap, or of an earlier life of the session.
    ///
    /// # Errors
    ///
    /// The session is not running here, or has no room for the request.
    fn subscribe_from_state(
        &self,
        session: SessionId,
    ) -> Result<Pin<Box<dyn EventRx>>, ControlError>;
}
