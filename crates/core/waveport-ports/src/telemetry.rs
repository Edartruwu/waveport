//! What a session tells those who watch it.
//!
//! The kernel never calls anything. After each step the runtime gives
//! every sink the facts the step recorded and the times at which the
//! parts of a turn were reached, and what a sink makes of them (a trace,
//! a metric, a report) is its own business.

use waveport_domain::fact::Stamped;
use waveport_domain::ids::{GenId, ToolCallId, TurnId};
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;

use crate::journal::SessionId;

/// Watches sessions. It is called on the session's own thread and must
/// not wait: what takes time is handed on to somewhere else.
pub trait EventSink: Send + Sync + 'static {
    /// The facts one step recorded, in order. Their text is in `text`.
    fn facts(&self, session: SessionId, at: MonoTime, facts: &[Stamped], text: &TextArena);

    /// A part of a turn was reached.
    fn stage(&self, session: SessionId, mark: StageMark);
}

/// The time at which a part of a turn was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StageMark {
    /// When.
    pub at: MonoTime,
    /// Which part.
    pub stage: Stage,
}

/// The parts of a turn whose times add up to what a user waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// The final transcript of a turn arrived.
    TranscriptFinal(TurnId),
    /// It was decided that a turn has ended.
    TurnEnded(TurnId),
    /// The model wrote the first text of a response.
    FirstToken(GenId),
    /// Speech synthesis delivered the first audio of a response.
    FirstAudio(GenId),
    /// The first frame of a response was handed to the transport.
    FirstFrameOut(GenId),
    /// A response's volume was lowered, because someone spoke over it.
    Lowered(GenId),
    /// A response was stopped, because someone took the floor.
    Stopped(GenId),
    /// A tool call was sent to its tool.
    ToolSent(ToolCallId),
    /// A tool call came back.
    ToolBack(ToolCallId),
}
