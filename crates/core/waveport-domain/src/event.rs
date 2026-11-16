//! What the kernel is told: everything that can happen to a session.

use crate::ids::{AgentId, GenId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId, TurnId};
use crate::text::Text;
use crate::time::{MediaDur, MediaSpan, MediaTime};

/// One input to [`Kernel::step`](crate::kernel::Kernel::step).
///
/// The variants fall into three groups with different rules for stale
/// input, and the difference is what closes the race where a user's words
/// are flushed by the interruption they caused:
///
/// - **Perception** (`Speech`, `Transcript`, `Eou`, `Overlap`, `TextIn`) is a
///   fact about the world. It is never dropped because the agent was
///   interrupted.
/// - **Results of agent output** (`Op`) are dropped, in one place, when the
///   response they belong to is no longer live.
/// - **Tool results** (`Tool`) are keyed by the call and outlive the response
///   that asked for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A participant started or stopped speaking.
    Speech {
        /// Who.
        who: ParticipantId,
        /// Started or stopped.
        edge: VadEdge,
        /// Where on the participant's track.
        at: MediaTime,
    },
    /// Words recognised in a participant's speech.
    Transcript {
        /// Who spoke.
        who: ParticipantId,
        /// The audio the words were heard in. Transcripts join turns by
        /// this, not by when they arrive.
        span: MediaSpan,
        /// The words. For an interim transcript, everything heard in `span`
        /// so far.
        text: Text,
        /// Whether the recogniser will not revise these words.
        is_final: bool,
    },
    /// An answer to [`SenseCmd::EstimateEou`](crate::command::SenseCmd).
    Eou {
        /// The turn the estimate is for.
        turn: TurnId,
        /// How likely it is that the speaker has finished.
        p: Prob,
    },
    /// An answer to [`SenseCmd::ClassifyOverlap`](crate::command::SenseCmd).
    Overlap {
        /// The turn that overlaps the agent's speech.
        turn: TurnId,
        /// What the overlapping speech is.
        verdict: OverlapVerdict,
    },
    /// A participant sent text instead of speaking.
    TextIn {
        /// Who.
        who: ParticipantId,
        /// What they wrote.
        text: Text,
    },
    /// A result of work on a response, with the tag the work was started
    /// with.
    Op(OpTag, OpEvent),
    /// News about a tool call.
    Tool(ToolCallId, ToolEvent),
    /// A timer set with
    /// [`Command::SetTimer`](crate::command::Command::SetTimer) is due.
    Timer(TimerId),
    /// A request from outside the conversation.
    Control(ControlInput),
    /// Every fact up to and including this one is stored durably.
    Durable(Seq),
}

/// Which way a participant's voice activity changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VadEdge {
    /// Speech began.
    Start,
    /// Speech stopped.
    End,
}

/// What speech that overlaps the agent's own speech turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverlapVerdict {
    /// A listener's noise of agreement. The agent carries on.
    Backchannel,
    /// The speaker wants the floor.
    BargeIn,
    /// Not speech meant for anyone.
    Noise,
}

/// A result of work on a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpEvent {
    /// The language model produced more text.
    LlmDelta {
        /// The new text only.
        text: Text,
    },
    /// The language model asked for a tool call.
    ToolProposed {
        /// Which tool.
        tool: ToolId,
        /// Its arguments, as the model wrote them.
        args: Text,
    },
    /// The language model finished.
    LlmDone {
        /// Why it stopped.
        finish: Finish,
    },
    /// Speech synthesis delivered audio.
    TtsSegment {
        /// Bytes of this response's spoken text synthesised so far.
        text_end: u32,
        /// Length of this response's audio synthesised so far.
        audio_end: MediaDur,
        /// Whether no more audio will follow.
        last: bool,
    },
    /// How far playback of the response has got.
    ///
    /// The runtime delivers the latest position ahead of any other event, so
    /// that an interruption is decided on what was heard by then.
    Played {
        /// Audio handed to the transport.
        handed: MediaDur,
        /// Audio known to have been played to the listener.
        acked: MediaDur,
        /// Bytes of the response's spoken text heard in full.
        heard_text: u32,
        /// Whether all audio of the response has been played.
        drained: bool,
    },
    /// The work failed and will produce nothing more.
    Failed {
        /// Which part failed.
        part: Part,
        /// What kind of failure it was.
        class: RetryClass,
    },
}

/// Why a language model stopped writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Finish {
    /// It was done.
    Stop,
    /// It asked for tool calls and waits for their results.
    ToolCalls,
    /// It reached a length limit.
    Length,
    /// The provider withheld the rest.
    Filtered,
}

/// The parts that work on a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// The language model.
    Think,
    /// Speech synthesis.
    Speak,
    /// Playback to the listener.
    Play,
}

/// What kind of failure an adapter reports, which decides whether trying
/// again can help.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetryClass {
    /// Likely to work if tried again.
    Transient,
    /// The provider asked for fewer requests.
    RateLimited,
    /// The credentials were refused.
    Auth,
    /// Trying again will fail the same way.
    Permanent,
    /// No answer arrived in time.
    Timeout,
}

/// News about a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolEvent {
    /// The tool returned.
    Completed {
        /// What it returned.
        result: Text,
    },
    /// The tool failed.
    Failed {
        /// What kind of failure it was.
        class: RetryClass,
        /// A description for the language model.
        message: Text,
    },
    /// The tool confirmed that it undid a call the kernel cancelled.
    RolledBack,
}

/// The timers a session uses. Setting a timer again replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimerId {
    /// Time to decide whether a turn has ended.
    Eou(TurnId),
    /// A turn has gone on for as long as one may.
    TurnMax(TurnId),
    /// Time to decide about speech that overlaps the agent.
    Overlap(TurnId),
    /// A response has taken as long as one may.
    Gen(GenId),
    /// A tool call has taken as long as its tool allows.
    ToolTimeout(ToolCallId),
    /// A cancelled tool call has had its time to confirm a rollback.
    Rollback(ToolCallId),
    /// Time to record how far playback has got.
    Checkpoint,
    /// The parts that serve a newly activated agent have had their time to
    /// become ready.
    StageAck,
    /// A closing session has had its time to finish what it was doing.
    Drain,
    /// A closing session has had its time to stop.
    Grace,
    /// A line that dropped has had its time to come back.
    Detach,
}

/// A request from outside the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlInput {
    /// Have the agent that holds the floor say exactly this.
    Say {
        /// What to say.
        text: Text,
    },
    /// Stop the agent's speech as a barge-in would.
    Interrupt,
    /// Give the floor to another agent.
    Handoff {
        /// The agent to activate.
        to: AgentId,
        /// What it is told about the conversation so far.
        context: HandoffContext,
    },
    /// End the session.
    Close {
        /// Why.
        reason: CloseReason,
    },
    /// A human joined.
    Join {
        /// Who.
        who: ParticipantId,
    },
    /// A human left.
    Leave {
        /// Who.
        who: ParticipantId,
    },
    /// The line to whoever the session talks to dropped; it may come
    /// back. What the agent was saying ends where it was heard to, and
    /// nothing is said until the line is back. The session waits for that
    /// for [`Timing::detach_wait`](crate::config::Timing::detach_wait),
    /// and then closes as abandoned.
    Detached,
    /// The line is there again. A reply that the drop cut off is owed
    /// again. Said of a line that was never gone, this changes nothing.
    Attached,
    /// The parts that serve the active agent are ready.
    StageReady,
    /// Another owner has taken the session over. Nothing more may be said
    /// or stored.
    LeaseLost,
}

/// What a newly activated agent is told about the conversation so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoffContext {
    /// Everything.
    Full,
    /// This summary.
    Summary(Text),
    /// Nothing.
    None,
}

/// Why a session is closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// The application or the user asked.
    Requested,
    /// Every human left, or the line to them dropped and stayed gone.
    Abandoned,
    /// The session cannot go on.
    Failure,
}
