//! What the kernel decided: the record a session can be rebuilt from.
//!
//! Facts are the only thing that is journaled. They say what became true,
//! not what was received, so a newer build can rebuild a session from the
//! facts of an older one without repeating its decisions.

use crate::event::CloseReason;
use crate::ids::{
    AgentId, BatchId, ContextVersion, Epoch, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
};
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
    /// Keys of a telephone that a user pressed one after the other entered
    /// the conversation. They are owed a reply, as a turn meant for the
    /// agent is.
    KeysCommitted {
        /// The turn.
        turn: TurnId,
        /// Who pressed them.
        who: ParticipantId,
        /// The keys as they are written, in the order they were pressed:
        /// digits, `*`, `#` and `A` to `D`.
        keys: TextRef,
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
        /// The conversation this speech answers: every item up to this
        /// version has had its response. `None` when it answers nothing:
        /// the agent was cut off, or the text came from the application
        /// and not from a language model.
        answers: Option<ContextVersion>,
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
            | Self::KeysCommitted { keys: text, .. }
            | Self::NoticeAppended { text, .. }
            | Self::UtteranceCommitted { text, .. } => Some(*text),
            Self::UtteranceProgress { heard, .. } => Some(*heard),
            Self::ToolProposed { args, .. } => Some(*args),
            Self::ToolFinished { result, .. } => Some(*result),
            _ => None,
        }
    }

    /// The same fact with its text at `text`. A fact's text lives in the
    /// [`Out`](crate::out::Out) of the step that emitted it; whoever keeps
    /// the fact for longer copies the text somewhere else and points the
    /// fact there. A fact without text is returned as it is.
    #[must_use]
    pub const fn with_text(mut self, text: TextRef) -> Self {
        match &mut self {
            Self::TurnCommitted { text: at, .. }
            | Self::KeysCommitted { keys: at, .. }
            | Self::NoticeAppended { text: at, .. }
            | Self::UtteranceCommitted { text: at, .. }
            | Self::UtteranceProgress { heard: at, .. }
            | Self::ToolProposed { args: at, .. }
            | Self::ToolFinished { result: at, .. } => *at = text,
            _ => {}
        }
        self
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
    /// The application interrupted the agent, or the line to whoever was
    /// listening dropped.
    Interrupted,
    /// Another agent took the floor.
    Handoff,
    /// A newer turn made the response out of date before any of it was
    /// heard.
    Superseded,
    /// A part that was producing the output failed or stalled.
    Failure,
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
    /// Another agent is taking over, and this is what it is told of the
    /// conversation so far in place of the conversation itself. An empty
    /// note means it is told nothing.
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

#[cfg(test)]
mod tests {
    use super::Fact;
    use crate::ids::{AgentId, GenId, ToolCallId};
    use crate::text::TextArena;

    #[test]
    fn a_fact_can_be_pointed_at_a_copy_of_its_text() {
        let mut step = TextArena::default();
        let _ = step.push("something else");
        let said = step.push("Gate twelve.");
        let fact = Fact::UtteranceCommitted {
            generation: GenId(3),
            agent: AgentId(0),
            answers: None,
            text: said,
            truncated: false,
        };

        let mut kept = TextArena::default();
        let copy = kept.push(step.get(said));
        let moved = fact.with_text(copy);
        assert_eq!(moved.text(), Some(copy));
        assert_eq!(kept.get(copy), "Gate twelve.");
        assert_eq!(moved.with_text(said), fact);

        let plain = Fact::ToolAuthorized {
            call: ToolCallId(1),
        };
        assert_eq!(plain.with_text(copy), plain);
        assert_eq!(plain.text(), None);
    }
}
