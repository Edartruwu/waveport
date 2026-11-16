//! What the kernel decided: the record a session can be rebuilt from.
//!
//! Facts are the only thing that is journaled. They say what became true,
//! not what was received, so a newer build can rebuild a session from the
//! facts of an older one without repeating its decisions.

use crate::event::CloseReason;
use crate::ids::{AgentId, BatchId, Epoch, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId};
use crate::text::TextRef;
use crate::time::MediaSpan;

/// A fact with its position in the session's journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamped {
    /// The position. Facts are numbered from 1 without gaps.
    pub seq: Seq,
    /// What became true.
    pub fact: Fact,
}

/// Something that became true of a session.
///
/// Text is a reference into the [`Out`](crate::out::Out) the fact was
/// emitted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact {
    /// A human joined.
    ParticipantJoined {
        /// Who.
        who: ParticipantId,
    },
    /// A human left.
    ParticipantLeft {
        /// Who.
        who: ParticipantId,
    },
    /// The session moved to another stage of its life.
    LifecycleChanged {
        /// The new stage.
        to: Lifecycle,
        /// Why the session is closing, once it is.
        reason: Option<CloseReason>,
    },
    /// Agent output that held the floor became invalid.
    EpochAdvanced {
        /// The new epoch.
        to: Epoch,
        /// What invalidated the output.
        cause: EpochCause,
    },
    /// A user turn ended and entered the conversation.
    TurnCommitted {
        /// The turn.
        turn: TurnId,
        /// Its speaker.
        who: ParticipantId,
        /// The audio it was spoken in; empty for typed text.
        span: MediaSpan,
        /// What was said.
        text: TextRef,
        /// Whether it was meant for the agent, and so is owed a reply.
        addressed: bool,
    },
    /// A user turn ended without entering the conversation.
    TurnDismissed {
        /// The turn.
        turn: TurnId,
        /// Its speaker.
        who: ParticipantId,
        /// Why.
        reason: Dismissal,
    },
    /// A note to the language model entered the conversation.
    NoticeAppended {
        /// What the note is about.
        kind: NoticeKind,
        /// The note.
        text: TextRef,
    },
    /// More of an agent's speech was heard. Recorded about once a second
    /// while it speaks, so a crash loses at most that much.
    UtteranceProgress {
        /// The response being spoken.
        generation: GenId,
        /// Its agent.
        agent: AgentId,
        /// The text heard since the previous record for this response.
        heard: TextRef,
    },
    /// An agent's speech ended and entered the conversation.
    UtteranceCommitted {
        /// The response that was spoken.
        generation: GenId,
        /// Its agent.
        agent: AgentId,
        /// What the listener heard.
        text: TextRef,
        /// Whether the agent was cut off before it had said everything.
        truncated: bool,
    },
    /// A response asked for a tool call, and the request entered the
    /// conversation.
    ToolProposed {
        /// The call.
        call: ToolCallId,
        /// The calls proposed together with it.
        batch: BatchId,
        /// The response that asked.
        generation: GenId,
        /// Which tool.
        tool: ToolId,
        /// Its arguments.
        args: TextRef,
    },
    /// A call may be dispatched. For a tool with side effects, dispatch
    /// waits until this record is durable.
    ToolAuthorized {
        /// The call.
        call: ToolCallId,
    },
    /// A call was dispatched.
    ToolDispatched {
        /// The call.
        call: ToolCallId,
        /// 0 for the first dispatch.
        attempt: u8,
    },
    /// A call ended, and its result entered the conversation.
    ToolFinished {
        /// The call.
        call: ToolCallId,
        /// How it ended.
        outcome: ToolOutcome,
        /// What the language model is told.
        result: TextRef,
    },
    /// Every call of a batch has ended.
    BatchResolved {
        /// The batch.
        batch: BatchId,
    },
    /// A handoff to another agent was asked for.
    HandoffRequested {
        /// The agent to activate.
        to: AgentId,
    },
    /// An agent took the floor.
    AgentActivated {
        /// The agent.
        agent: AgentId,
    },
}

impl Fact {
    /// The text this fact carries, if any, as a reference into the
    /// [`Out`](crate::out::Out) it was emitted in.
    #[must_use]
    pub const fn text(&self) -> Option<TextRef> {
        match self {
            Self::TurnCommitted { text, .. }
            | Self::NoticeAppended { text, .. }
            | Self::UtteranceCommitted { text, .. } => Some(*text),
            Self::UtteranceProgress { heard, .. } => Some(*heard),
            Self::ToolProposed { args, .. } => Some(*args),
            Self::ToolFinished { result, .. } => Some(*result),
            _ => None,
        }
    }
}

/// The stages of a session's life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lifecycle {
    /// Conversing.
    #[default]
    Running,
    /// Asked to close: finishing what is under way and starting nothing new.
    Draining,
    /// Stopping everything that has not finished.
    Closing,
    /// Over. Nothing more is emitted.
    Closed,
}

/// What made agent output invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EpochCause {
    /// A user spoke over the agent and wanted the floor.
    BargeIn,
    /// The application interrupted the agent.
    Interrupted,
    /// Another agent took the floor.
    Handoff,
    /// The session was rebuilt after a crash.
    Recovery,
    /// The session is closing.
    Closing,
}

/// Why a user turn did not enter the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dismissal {
    /// It was a listener's noise of agreement.
    Backchannel,
    /// It was not speech meant for anyone.
    Noise,
    /// No words were recognised in it.
    Empty,
    /// The session closed before it ended.
    Closed,
}

/// What a note to the language model is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    /// The session was rebuilt after a crash, and some work was lost.
    Recovered,
    /// Another agent took over.
    Handoff,
}

/// How a tool call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolOutcome {
    /// The tool returned a result.
    Completed,
    /// The tool failed, or was never run.
    Failed,
    /// The call was cancelled and left no effect.
    Cancelled,
    /// The call was dispatched, and whether it took effect is unknown.
    InDoubt,
}
