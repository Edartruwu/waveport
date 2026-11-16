//! Tools: what an agent can do besides talk.

use waveport_domain::event::RetryClass;
use waveport_domain::ids::{ToolCallId, ToolId};

use crate::error::BoxFuture;
use crate::journal::SessionId;

/// Runs tool calls.
pub trait ToolExecutor: Send + Sync + 'static {
    /// Runs a call. Dropping the future stops waiting for it, which does
    /// not undo anything the tool has done.
    fn call<'a>(&'a self, request: ToolRequest<'a>) -> BoxFuture<'a, ToolReply>;

    /// Asks the tool to undo a call that was started and may have taken
    /// effect. The answer is whether nothing of it is left: `true` only
    /// when the tool is sure.
    fn undo(&self, session: SessionId, call: ToolCallId) -> BoxFuture<'_, bool>;
}

/// A call to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolRequest<'a> {
    /// The session that asks. With `call` it names the call for good: a
    /// tool that is given the same pair again is being asked to do the
    /// same thing, not another.
    pub session: SessionId,
    /// The call.
    pub call: ToolCallId,
    /// The tool.
    pub tool: ToolId,
    /// The arguments, as the model wrote them.
    pub args: &'a str,
    /// 0 for the first time the call is sent, higher when it is sent again
    /// after the session was restarted.
    pub attempt: u8,
}

/// How a call ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolReply {
    /// The tool returned this, which the model is shown.
    Completed(String),
    /// The tool failed.
    Failed {
        /// What kind of failure it was.
        class: RetryClass,
        /// A description for the model.
        message: String,
    },
}
