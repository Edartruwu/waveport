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
/// - **Perception** (`Speech`, `Transcript`, `Eou`, `Overlap`, `TextIn`, `Key`)
///   is a fact about the world. It is never dropped because the agent was
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
    /// A participant pressed a key of a telephone.
    Key {
        /// Who.
        who: ParticipantId,
        /// Which key.
        key: Key,
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

/// A key of a telephone: the sixteen that RFC 4733 numbers from 0 to 15.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// 0.
    Zero,
    /// 1.
    One,
    /// 2.
    Two,
    /// 3.
    Three,
    /// 4.
    Four,
    /// 5.
    Five,
    /// 6.
    Six,
    /// 7.
    Seven,
    /// 8.
    Eight,
    /// 9.
    Nine,
    /// The star.
    Star,
    /// The hash, or pound.
    Hash,
    /// A, which few telephones have.
    A,
    /// B.
    B,
    /// C.
    C,
    /// D.
    D,
}

impl Key {
    /// Every key, in the order of their numbers.
    pub const ALL: [Self; 16] = [
        Self::Zero,
        Self::One,
        Self::Two,
        Self::Three,
        Self::Four,
        Self::Five,
        Self::Six,
        Self::Seven,
        Self::Eight,
        Self::Nine,
        Self::Star,
        Self::Hash,
        Self::A,
        Self::B,
        Self::C,
        Self::D,
    ];

    /// The key as it is written: a digit, `*`, `#`, or a capital letter.
    #[must_use]
    pub const fn as_char(self) -> char {
        match self {
            Self::Zero => '0',
            Self::One => '1',
            Self::Two => '2',
            Self::Three => '3',
            Self::Four => '4',
            Self::Five => '5',
            Self::Six => '6',
            Self::Seven => '7',
            Self::Eight => '8',
            Self::Nine => '9',
            Self::Star => '*',
            Self::Hash => '#',
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
        }
    }

    /// The key that is written `written`. The letters are taken in either
    /// case.
    #[must_use]
    pub const fn from_char(written: char) -> Option<Self> {
        Some(match written {
            '0' => Self::Zero,
            '1' => Self::One,
            '2' => Self::Two,
            '3' => Self::Three,
            '4' => Self::Four,
            '5' => Self::Five,
            '6' => Self::Six,
            '7' => Self::Seven,
            '8' => Self::Eight,
            '9' => Self::Nine,
            '*' => Self::Star,
            '#' => Self::Hash,
            'A' | 'a' => Self::A,
            'B' | 'b' => Self::B,
            'C' | 'c' => Self::C,
            'D' | 'd' => Self::D,
            _ => return None,
        })
    }

    /// The number RFC 4733 gives the key: 0 to 9 for the digits, 10 for
    /// the star, 11 for the hash and 12 to 15 for the letters.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The key RFC 4733 numbers `code`.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Zero,
            1 => Self::One,
            2 => Self::Two,
            3 => Self::Three,
            4 => Self::Four,
            5 => Self::Five,
            6 => Self::Six,
            7 => Self::Seven,
            8 => Self::Eight,
            9 => Self::Nine,
            10 => Self::Star,
            11 => Self::Hash,
            12 => Self::A,
            13 => Self::B,
            14 => Self::C,
            15 => Self::D,
            _ => return None,
        })
    }
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
    /// Whoever pressed keys has had their time to press another.
    Keys(TurnId),
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
