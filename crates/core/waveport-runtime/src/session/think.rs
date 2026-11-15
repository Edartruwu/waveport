//! The responses a session's language model is writing.
//!
//! The kernel starts a response with a tag and may hold it back, let it
//! go on, or cancel it. Here that is a stream that is read, not read, or
//! dropped: a model that is not read stops writing soon enough, because
//! nothing takes what it writes, and dropping the stream is all there is
//! to cancelling it.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use waveport_domain::event::{Event, OpEvent, Part, RetryClass};
use waveport_domain::ids::{OpTag, ToolId};
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::llm::{Llm, LlmEvent, LlmRequest, LlmRx};

use super::Defs;
use crate::context::Asking;

type Starting = BoxFuture<'static, Result<Pin<Box<dyn LlmRx>>, PortError>>;

enum State {
    Starting(Starting),
    Writing(Pin<Box<dyn LlmRx>>),
    Over,
}

/// One response being written.
pub(crate) struct Mind {
    pub tag: OpTag,
    state: State,
    /// Whether the kernel has asked for it to be held back.
    pub paused: bool,
    /// The tools it was offered.
    offered: Vec<ToolId>,
    /// Whether nothing of it has arrived yet.
    pub silent: bool,
}

impl Mind {
    /// Asks the model for a response.
    pub(crate) fn start(tag: OpTag, asking: Asking, llm: Arc<dyn Llm>, defs: Arc<Defs>) -> Self {
        let offered = asking.tools.clone();
        let starting = async move {
            let messages = asking.messages(&defs.tools);
            let tools = asking.offered(&defs.tools);
            let request = LlmRequest {
                instructions: asking.instructions(&defs.agents),
                messages: &messages,
                tools: &tools,
                max_output_tokens: None,
            };
            llm.start(request).await
        };
        Self {
            tag,
            state: State::Starting(Box::pin(starting)),
            paused: false,
            offered,
            silent: true,
        }
    }

    /// Whether there is nothing more to come of it.
    pub(crate) const fn is_over(&self) -> bool {
        matches!(self.state, State::Over)
    }

    /// Reads what the model has written, unless the response is held
    /// back, and adds it to `events`. Returns whether anything changed
    /// that may have made more work.
    pub(crate) fn poll(
        &mut self,
        cx: &mut Context<'_>,
        defs: &Defs,
        events: &mut Vec<Event>,
    ) -> bool {
        let tag = self.tag;
        let failed = |class: RetryClass| {
            Event::Op(
                tag,
                OpEvent::Failed {
                    part: Part::Think,
                    class,
                },
            )
        };
        let mut changed = false;
        if let State::Starting(starting) = &mut self.state
            && let Poll::Ready(started) = starting.as_mut().poll(cx)
        {
            changed = true;
            self.state = match started {
                Ok(rx) => State::Writing(rx),
                Err(error) => {
                    events.push(failed(error.class));
                    State::Over
                }
            };
        }
        while !self.paused
            && let State::Writing(rx) = &mut self.state
            && let Poll::Ready(next) = rx.as_mut().poll_next(cx)
        {
            self.silent = false;
            let event = match next {
                Some(Ok(LlmEvent::Delta { text })) => OpEvent::LlmDelta { text: text.into() },
                Some(Ok(LlmEvent::ToolCall { tool, args })) => {
                    // Only a tool that was offered can be asked for.
                    let asked = defs
                        .tools
                        .iter()
                        .position(|known| known.name == tool)
                        .and_then(|index| u16::try_from(index).ok())
                        .map(ToolId)
                        .filter(|tool| self.offered.contains(tool));
                    let Some(tool) = asked else {
                        self.state = State::Over;
                        events.push(failed(RetryClass::Permanent));
                        return true;
                    };
                    OpEvent::ToolProposed {
                        tool,
                        args: args.into(),
                    }
                }
                Some(Ok(LlmEvent::Done { finish, .. })) => {
                    self.state = State::Over;
                    OpEvent::LlmDone { finish }
                }
                Some(Err(error)) => {
                    self.state = State::Over;
                    events.push(failed(error.class));
                    return true;
                }
                // A stream that ends without saying that it is done has
                // broken off.
                None => {
                    self.state = State::Over;
                    events.push(failed(RetryClass::Transient));
                    return true;
                }
            };
            events.push(Event::Op(tag, event));
            changed = true;
        }
        changed
    }
}

impl core::fmt::Debug for Mind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let state = match self.state {
            State::Starting(_) => "starting",
            State::Writing(_) => "writing",
            State::Over => "over",
        };
        f.debug_struct("Mind")
            .field("tag", &self.tag)
            .field("state", &state)
            .field("paused", &self.paused)
            .finish_non_exhaustive()
    }
}
