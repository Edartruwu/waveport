//! A read-only look at a session.

use super::State;
use super::response::Playback;
use crate::command::LlmCause;
use crate::config::KernelConfig;
use crate::fact::{Lifecycle, NoticeKind, ToolOutcome};
use crate::ids::{
    AgentId, BatchId, ContextVersion, Epoch, GenId, OpTag, ParticipantId, Seq, ToolCallId, ToolId,
    TurnId,
};
use crate::time::MediaSpan;

/// A read-only look at a session, for the runtime, for policies' callers
/// and for tests. Looking changes nothing.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    state: &'a State,
    config: &'a KernelConfig,
}

impl<'a> View<'a> {
    pub(super) const fn new(state: &'a State, config: &'a KernelConfig) -> Self {
        Self { state, config }
    }

    /// The current epoch.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.state.epoch
    }

    /// The stage of the session's life.
    #[must_use]
    pub const fn lifecycle(&self) -> Lifecycle {
        self.state.lifecycle
    }

    /// Whether a client is connected.
    #[must_use]
    pub const fn is_attached(&self) -> bool {
        self.state.attached
    }

    /// The agent that holds the floor.
    #[must_use]
    pub const fn front(&self) -> AgentId {
        self.state.front
    }

    /// The sequence number the next fact will get.
    #[must_use]
    pub const fn next_seq(&self) -> Seq {
        self.state.next_seq
    }

    /// The version of the conversation: how many times an item has been
    /// appended.
    #[must_use]
    pub fn context_version(&self) -> ContextVersion {
        self.state.conversation.version()
    }

    /// The configuration the session was started with.
    #[must_use]
    pub const fn config(&self) -> &'a KernelConfig {
        self.config
    }

    /// Whether results carrying `tag` are still acted on: its response is
    /// under way and was not ended by an interruption.
    #[must_use]
    pub fn is_live(&self, tag: OpTag) -> bool {
        self.generations().any(|generation| generation.tag == tag)
    }

    /// The responses under way.
    pub fn generations(&self) -> impl Iterator<Item = GenerationView> + use<'a> {
        self.state.generations.iter().map(|running| GenerationView {
            tag: running.tag,
            agent: running.agent,
            role: running.role,
            context: running.context,
            speaking: running.is_audible(),
            lowered: running.playback == Playback::Ducked,
        })
    }

    /// The user turns that have begun and not ended.
    pub fn turns(&self) -> impl Iterator<Item = TurnView> + use<'a> {
        self.state.turns.iter().map(|open| TurnView {
            turn: open.id,
            who: open.who,
            span: open.span,
            speaking: open.speaking,
            ducking: open.overlap.map(|overlap| overlap.generation),
        })
    }

    /// The tool calls that have not ended.
    pub fn calls(&self) -> impl Iterator<Item = CallView> + use<'a> {
        self.state.calls.iter().map(|open| CallView {
            call: open.id,
            tool: open.tool,
            batch: open.batch,
            by: open.by,
            state: open.state,
            attempt: open.attempt,
        })
    }

    /// The humans in the session.
    pub fn participants(&self) -> impl Iterator<Item = ParticipantId> + use<'a> {
        self.state.participants.iter().copied()
    }

    /// The conversation, oldest item first. This is what a language model
    /// is shown.
    pub fn items(&self) -> impl Iterator<Item = ItemView<'a>> + use<'a> {
        self.state.conversation.iter()
    }

    /// The batches of tool calls that are not resolved: a call of theirs
    /// has not ended, or the response that asked may ask for more.
    pub fn batches(&self) -> impl Iterator<Item = BatchView> + use<'a> {
        self.state.batches.iter().map(|open| BatchView {
            batch: open.id,
            by: open.by,
            pending: usize::from(open.pending),
            ran: open.ran,
        })
    }

    /// The agent that was asked to take the floor and has not yet.
    #[must_use]
    pub const fn handoff(&self) -> Option<AgentId> {
        self.state.handoff
    }

    /// Whether a response is owed and has not been started: the
    /// conversation holds a turn meant for the agent, or the results of
    /// tool calls, that no response has been computed from.
    #[must_use]
    pub fn reply_owed(&self) -> bool {
        self.state.reply_owed()
    }

    /// What the response that is owed and has not been started is for.
    #[must_use]
    pub fn owed(&self) -> Option<LlmCause> {
        self.state
            .owed
            .filter(|_| self.state.reply_owed())
            .map(|(_, cause)| cause)
    }
}

/// A response under way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationView {
    /// The tag its commands carry.
    pub tag: OpTag,
    /// The agent responding.
    pub agent: AgentId,
    /// What the response is for.
    pub role: GenerationRole,
    /// The conversation it was computed from.
    pub context: ContextVersion,
    /// Whether its speech is being played.
    pub speaking: bool,
    /// Whether it is played at a lowered volume, because someone is
    /// speaking over it and it is not decided yet who keeps the floor.
    pub lowered: bool,
}

/// What a response is for, which decides what ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GenerationRole {
    /// It speaks to the user. An interruption ends it.
    Floor,
    /// It works on a tool call for another agent and does not speak. It
    /// ends with the call, and an interruption of the floor leaves it alone.
    Delegated(ToolCallId),
}

/// A user turn that has begun and not ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnView {
    /// The turn.
    pub turn: TurnId,
    /// Its speaker.
    pub who: ParticipantId,
    /// The audio it covers so far.
    pub span: MediaSpan,
    /// Whether the speaker is speaking now.
    pub speaking: bool,
    /// The response whose speech this turn overlaps and has lowered, while
    /// that is undecided.
    pub ducking: Option<GenId>,
}

/// A tool call that has not ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallView {
    /// The call.
    pub call: ToolCallId,
    /// The tool.
    pub tool: ToolId,
    /// The calls proposed together with it.
    pub batch: BatchId,
    /// The response that asked for it.
    pub by: GenId,
    /// How far it has got.
    pub state: CallState,
    /// How many times it was sent before, after the session was restarted.
    pub attempt: u8,
}

/// A batch of tool calls that is not resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchView {
    /// The batch.
    pub batch: BatchId,
    /// The response that asked for its calls.
    pub by: GenId,
    /// How many of its calls have not ended.
    pub pending: usize,
    /// Whether any of its calls was sent, or ended in another way than by
    /// being withdrawn. A batch of which nothing ran is owed no response.
    pub ran: bool,
}

/// How far a tool call has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallState {
    /// Asked for by a response, and waiting for its gate.
    Proposed,
    /// Cleared to run, and waiting for its record to be durable.
    Authorized,
    /// Sent to its tool.
    Dispatched,
    /// Sent, then told to stop, and waiting for the tool to confirm that
    /// it left no effect.
    Cancelling,
}

/// One item of the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemView<'a> {
    /// What a user said.
    UserTurn {
        /// The turn.
        turn: TurnId,
        /// Who said it.
        who: ParticipantId,
        /// The words.
        text: &'a str,
        /// Whether it was meant for the agent.
        addressed: bool,
    },
    /// What an agent said, as far as it was heard.
    Utterance {
        /// The response that was spoken.
        generation: GenId,
        /// The agent.
        agent: AgentId,
        /// The words the listener heard.
        text: &'a str,
        /// Whether the agent was cut off.
        truncated: bool,
    },
    /// A tool call an agent asked for.
    ToolCall {
        /// The call.
        call: ToolCallId,
        /// The tool.
        tool: ToolId,
        /// The response that asked.
        generation: GenId,
        /// The arguments.
        args: &'a str,
    },
    /// How a tool call ended.
    ToolResult {
        /// The call.
        call: ToolCallId,
        /// How it ended.
        outcome: ToolOutcome,
        /// What the language model is told.
        text: &'a str,
    },
    /// A note to the language model.
    Notice {
        /// What it is about.
        kind: NoticeKind,
        /// The note.
        text: &'a str,
    },
}
