//! Tool calls and the batches they are asked for in.
//!
//! A response may ask for several tool calls: its batch. Each call goes
//! its own way, `Proposed → Authorized → Dispatched`, and ends once, with a
//! result that enters the conversation. When every call of a batch has
//! ended and no more can be added, the batch is resolved, and that is what
//! one further response, the continuation, is owed for.
//!
//! Calls live outside the epoch. An interruption ends the response that
//! asked for them, and with it the calls that have not been sent; a call
//! that is running keeps running, because its effect may already have
//! happened.

use crate::ids::{BatchId, GenId, Seq, ToolCallId, ToolId};
use crate::kernel::view::CallState;
use crate::time::MonoTime;

/// What the language model is told about a call that the gate refused
/// for good.
pub(super) const NOT_ALLOWED: &str = "the call was not allowed";
/// ... about a call whose tool took too long.
pub(super) const TIMED_OUT: &str = "the tool did not answer in time";
/// ... about a call whose record could not be stored, so that it was
/// never sent.
pub(super) const NOT_RECORDED: &str = "the call could not be recorded, and was not made";
/// ... about a call that was sent and of which nothing more is known.
pub(super) const IN_DOUBT: &str = "the call was made, and whether it took effect is not known";
/// ... about a call that was withdrawn before it was sent.
pub(super) const WITHDRAWN: &str = "the call was withdrawn before it was made";
/// ... about a call that was stopped, or undone by its tool.
pub(super) const UNDONE: &str = "the call was stopped and left no effect";
/// ... about a call no agent could take.
pub(super) const NO_AGENT: &str = "no agent was free to work on the call";
/// ... about a call whose agent failed or wrote nothing.
pub(super) const NO_ANSWER: &str = "the agent working on the call gave no answer";
/// ... about a call that had not been sent when the session was restarted.
pub(super) const LOST: &str = "the session was restarted before the call was made";
/// ... about a call that changes nothing, whose answer was lost with the
/// session and that is not made again.
pub(super) const ANSWER_LOST: &str =
    "the answer to the call was lost when the session was restarted";

/// What the language model is told when the session was restarted and
/// lost what was under way.
pub(super) const RECOVERED: &str = "the session was interrupted by a failure and has been restored; what was under way then was lost";

/// The most times a call is sent again after the session was restarted.
pub(super) const MOST_RETRIES: u8 = 3;

/// A tool call that has not ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Call {
    pub(super) id: ToolCallId,
    pub(super) tool: ToolId,
    pub(super) batch: BatchId,
    /// The response that asked for it.
    pub(super) by: GenId,
    /// `Cancelling` is not journaled: for the journal such a call is
    /// `Dispatched`.
    pub(super) state: CallState,
    /// How many times it was sent before, after the session was restarted.
    pub(super) attempt: u8,
    /// The fact that must be stored durably before the call is sent. Not
    /// journaled.
    pub(super) barrier: Option<Seq>,
    /// When the wait the call is in has lasted too long: for its record
    /// to be stored, for its tool to answer, or for the tool to confirm
    /// that it undid the call. A call that was sent and has no such time
    /// was sent before the session was restarted. Not journaled.
    pub(super) due: Option<MonoTime>,
}

impl Call {
    /// A call as it is when a response has just asked for it.
    pub(super) const fn proposed(id: ToolCallId, tool: ToolId, batch: BatchId, by: GenId) -> Self {
        Self {
            id,
            tool,
            batch,
            by,
            state: CallState::Proposed,
            attempt: 0,
            barrier: None,
            due: None,
        }
    }

    /// Whether the call has been sent to its tool.
    pub(super) const fn is_sent(&self) -> bool {
        matches!(self.state, CallState::Dispatched | CallState::Cancelling)
    }

    /// The call as the journal knows it.
    pub(super) const fn journaled(&self) -> Self {
        Self {
            id: self.id,
            tool: self.tool,
            batch: self.batch,
            by: self.by,
            state: match self.state {
                CallState::Cancelling => CallState::Dispatched,
                state => state,
            },
            attempt: self.attempt,
            barrier: None,
            due: None,
        }
    }
}

/// The calls a response asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Batch {
    pub(super) id: BatchId,
    /// The response that asked.
    pub(super) by: GenId,
    /// Calls of the batch that have not ended.
    pub(super) pending: u16,
    /// Whether any call of the batch was sent, or ended in another way
    /// than by being withdrawn. A batch of which nothing ran is owed no
    /// continuation.
    pub(super) ran: bool,
    /// Whether the response that asked has ended. Not journaled: after a
    /// restart every such response is gone.
    pub(super) over: bool,
}

impl Batch {
    /// The batch of a response that has just asked for its first call.
    pub(super) const fn begin(id: BatchId, by: GenId) -> Self {
        Self {
            id,
            by,
            pending: 1,
            ran: false,
            over: false,
        }
    }

    /// The batch as the journal knows it.
    pub(super) const fn journaled(&self) -> Self {
        Self {
            id: self.id,
            by: self.by,
            pending: self.pending,
            ran: self.ran,
            over: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Batch, Call};
    use crate::ids::{BatchId, GenId, Seq, ToolCallId, ToolId};
    use crate::kernel::view::CallState;
    use crate::time::MonoTime;

    #[test]
    fn the_journal_knows_a_call_without_its_waits() {
        let mut call = Call::proposed(ToolCallId(7), ToolId(1), BatchId(2), GenId(3));
        assert_eq!((call.state, call.attempt), (CallState::Proposed, 0));
        assert!(!call.is_sent());
        assert_eq!(call.journaled(), call);

        call.state = CallState::Cancelling;
        call.barrier = Some(Seq(9));
        call.due = Some(MonoTime::from_millis(40));
        assert!(call.is_sent());
        let journaled = call.journaled();
        assert_eq!(journaled.state, CallState::Dispatched);
        assert_eq!((journaled.barrier, journaled.due), (None, None));
        assert_eq!((journaled.id, journaled.batch), (call.id, call.batch));
    }

    #[test]
    fn the_journal_does_not_know_whether_the_asking_response_ended() {
        let mut batch = Batch::begin(BatchId(1), GenId(4));
        assert_eq!((batch.pending, batch.ran, batch.over), (1, false, false));
        batch.over = true;
        batch.ran = true;
        let journaled = batch.journaled();
        assert!(journaled.ran);
        assert!(!journaled.over);
    }
}
