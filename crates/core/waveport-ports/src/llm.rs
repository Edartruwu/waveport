//! Language models.
//!
//! A response is one request and a stream of what the model writes. There
//! is no half to send on: a model is slowed down by not being polled, and
//! stopped by being dropped.

use core::pin::Pin;
use core::task::{Context, Poll};

pub use waveport_domain::event::Finish;
use waveport_domain::ids::{ParticipantId, ToolCallId};

use crate::error::{BoxFuture, PortError};

/// A language model.
pub trait Llm: Send + Sync + 'static {
    /// What it can do. The answer does not change.
    fn caps(&self) -> LlmCaps;

    /// Asks for a response. The request is read before the future first
    /// waits, so nothing of it is needed after that.
    fn start<'a>(
        &'a self,
        request: LlmRequest<'a>,
    ) -> BoxFuture<'a, Result<Pin<Box<dyn LlmRx>>, PortError>>;
}

/// A response being written. Polling it is what runs the adapter;
/// dropping it abandons the response.
pub trait LlmRx: Send {
    /// The next event. [`LlmEvent::Done`] is the last one of a response
    /// that came to its end, and `None` follows it. After an error
    /// nothing more follows.
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<LlmEvent, PortError>>>;
}

/// What a model is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LlmRequest<'a> {
    /// Who the agent is and how it behaves.
    pub instructions: &'a str,
    /// The conversation, oldest first.
    pub messages: &'a [Message<'a>],
    /// The tools the model may call.
    pub tools: &'a [ToolDecl<'a>],
    /// The most the model may write, in tokens, when there is a limit.
    pub max_output_tokens: Option<u32>,
}

/// One item of the conversation, as a model is shown it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// What a user said.
    User {
        /// Who, when there is more than one human.
        speaker: Option<ParticipantId>,
        /// The words.
        text: &'a str,
    },
    /// What the agent said, as far as it was heard.
    Assistant {
        /// The words.
        text: &'a str,
    },
    /// A tool call the agent asked for.
    ToolCall {
        /// The call, which its result names.
        call: ToolCallId,
        /// The tool's name.
        tool: &'a str,
        /// Its arguments, as JSON.
        args: &'a str,
    },
    /// How a tool call ended.
    ToolResult {
        /// The call.
        call: ToolCallId,
        /// What the model is told.
        text: &'a str,
    },
    /// A note to the model that nobody said: that it was interrupted,
    /// that the session went on after a failure.
    Notice {
        /// The note.
        text: &'a str,
    },
}

/// A tool a model may call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolDecl<'a> {
    /// Its name.
    pub name: &'a str,
    /// What it does, for the model.
    pub description: &'a str,
    /// Its arguments, as a JSON Schema.
    pub parameters: &'a str,
}

/// Something a model writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmEvent {
    /// More text.
    Delta {
        /// The new text only.
        text: String,
    },
    /// A request for a tool call, whole.
    ToolCall {
        /// The tool's name, as the model wrote it.
        tool: String,
        /// Its arguments, as the model wrote them.
        args: String,
    },
    /// The response is complete.
    Done {
        /// Why the model stopped.
        finish: Finish,
        /// What the response cost, when the provider says.
        usage: Option<Usage>,
    },
}

/// What a response cost, in tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Usage {
    /// Tokens read.
    pub input: u32,
    /// Of those, tokens the provider had kept from an earlier request.
    pub cached: u32,
    /// Tokens written.
    pub output: u32,
}

/// What a language model can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LlmCaps {
    /// Whether it can ask for tool calls.
    pub tools: bool,
    /// Whether it can ask for several tool calls in one response.
    pub parallel_tools: bool,
}
