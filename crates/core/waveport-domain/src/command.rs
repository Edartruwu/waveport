//! What the kernel asks for: the only way it affects anything.

use crate::event::TimerId;
use crate::fact::Lifecycle;
use crate::ids::{
    AgentId, BatchId, ContextVersion, GenId, OpTag, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
};
use crate::text::TextRef;
use crate::time::MonoTime;

/// One request from the kernel to the runtime.
///
/// Commands are sent without waiting. A part that cannot take one is treated
/// as failed, so a step never blocks on an adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// To the language model serving a response.
    Llm(OpTag, LlmCmd),
    /// To speech synthesis for a response.
    Tts(OpTag, TtsCmd),
    /// To speech recognition.
    Stt(SttCmd),
    /// To the models that judge turn-taking.
    Sense(SenseCmd),
    /// To playback of a response.
    Playout(OpTag, PlayoutCmd),
    /// To a tool.
    Tool(ToolCallId, ToolCmd),
    /// Set the timer to fire at the given time, replacing an earlier
    /// setting; `None` cancels it.
    SetTimer(TimerId, Option<MonoTime>),
    /// Report with [`Event::Durable`](crate::event::Event::Durable) once the
    /// fact with this sequence number is stored durably.
    AwaitDurable(Seq),
    /// Tell the clients.
    Emit(ClientEvent),
}

impl Command {
    /// The text this command carries, if any, as a reference into the
    /// [`Out`](crate::out::Out) it was emitted in.
    #[must_use]
    pub const fn text(&self) -> Option<TextRef> {
        match self {
            Self::Tts(_, TtsCmd::Speak { text }) => Some(*text),
            Self::Tool(_, ToolCmd::Dispatch { args, .. }) => Some(*args),
            _ => None,
        }
    }
}

/// A request to the language model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlmCmd {
    /// Start a response.
    Start {
        /// The agent that responds.
        agent: AgentId,
        /// The conversation to respond to: every item up to this version.
        context: ContextVersion,
        /// What the response is for.
        cause: LlmCause,
    },
    /// Stop reading the model's output for now: too much text is waiting to
    /// be spoken.
    Pause,
    /// Read the model's output again.
    Resume,
    /// Abandon the response.
    Cancel,
}

/// What a response is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LlmCause {
    /// A user turn.
    Reply {
        /// The turn.
        turn: TurnId,
    },
    /// The results of the tool calls an earlier response asked for.
    Continuation {
        /// Those calls.
        batch: BatchId,
    },
    /// A tool call whose tool is another agent.
    Delegated {
        /// The call.
        call: ToolCallId,
    },
}

/// A request to speech synthesis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtsCmd {
    /// Speak this next.
    Speak {
        /// The text.
        text: TextRef,
    },
    /// No more text follows for this response.
    Finish,
    /// Stop, and discard audio not yet delivered.
    Cancel,
}

/// A request to speech recognition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SttCmd {
    /// Deliver the final transcript of what this participant has said so
    /// far, without waiting for more.
    Finalize {
        /// The participant.
        who: ParticipantId,
    },
    /// This participant will not be listened to any more.
    Close {
        /// The participant.
        who: ParticipantId,
    },
}

/// A request to the models that judge turn-taking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SenseCmd {
    /// Estimate whether the speaker has finished the turn. Answered with
    /// [`Event::Eou`](crate::event::Event::Eou).
    EstimateEou {
        /// The turn.
        turn: TurnId,
        /// Its speaker.
        who: ParticipantId,
    },
    /// Classify speech that overlaps the agent's. Answered with
    /// [`Event::Overlap`](crate::event::Event::Overlap).
    ClassifyOverlap {
        /// The overlapping turn.
        turn: TurnId,
        /// Its speaker.
        who: ParticipantId,
    },
}

/// A request to playback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlayoutCmd {
    /// Audio of this response may be played from now on.
    Start,
    /// Lower the volume: someone is speaking over the agent.
    Duck,
    /// Restore the volume.
    Unduck,
    /// Stop playing and discard what is buffered.
    Stop {
        /// How long the fade-out lasts, in milliseconds.
        fade_ms: u16,
    },
}

/// A request to a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCmd {
    /// Run the call. The call's identity is its idempotency key.
    Dispatch {
        /// Which tool.
        tool: ToolId,
        /// Its arguments.
        args: TextRef,
        /// 0 for the first dispatch; higher when a call is retried after a
        /// recovery.
        attempt: u8,
    },
    /// Stop the call if that is still possible, and undo it if the tool can.
    Cancel,
}

/// News for the clients of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientEvent {
    /// A participant started or stopped speaking.
    UserSpeaking {
        /// Who.
        who: ParticipantId,
        /// The turn the speech belongs to.
        turn: TurnId,
        /// Started or stopped.
        speaking: bool,
    },
    /// What an agent is doing changed.
    Agent {
        /// The agent.
        agent: AgentId,
        /// The response concerned, when there is one.
        generation: Option<GenId>,
        /// What it is doing now.
        activity: Activity,
    },
    /// The session moved to another stage of its life.
    Lifecycle(Lifecycle),
}

/// What an agent is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Activity {
    /// Nothing; it is the user's turn.
    Listening,
    /// Working on a response that has not started to play.
    Thinking,
    /// Speaking.
    Speaking,
}
