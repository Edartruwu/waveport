//! A language model that follows a script.

use core::fmt;
use core::pin::Pin;
use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use waveport_adapter_kit::stream;
use waveport_domain::ids::{ParticipantId, ToolCallId};
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::llm::{Finish, Llm, LlmCaps, LlmEvent, LlmRequest, LlmRx, Message, Usage};

/// One thing a scripted model does in a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmStep {
    /// Writes this, a word at a time.
    Say(String),
    /// Writes exactly this in one piece.
    Delta(String),
    /// Asks for a tool call.
    Call {
        /// The tool's name.
        tool: String,
        /// Its arguments.
        args: String,
    },
    /// Stops for this reason. A response that does not say stops by
    /// itself: to wait for its tool calls if it asked for any, and
    /// otherwise because it is done.
    Stop(Finish),
    /// Fails.
    Fail(PortError),
    /// Writes nothing more and never ends.
    Hang,
}

impl LlmStep {
    /// Writes `text`, a word at a time.
    #[must_use]
    pub fn say(text: &str) -> Self {
        Self::Say(text.to_owned())
    }

    /// Asks for a call of `tool` with `args`.
    #[must_use]
    pub fn call(tool: &str, args: &str) -> Self {
        Self::Call {
            tool: tool.to_owned(),
            args: args.to_owned(),
        }
    }
}

/// A request a scripted model was given, kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The instructions.
    pub instructions: String,
    /// The conversation.
    pub messages: Vec<Said>,
    /// The names of the tools on offer.
    pub tools: Vec<String>,
    /// The limit on what may be written.
    pub max_output_tokens: Option<u32>,
}

impl Asked {
    /// What the user said last.
    #[must_use]
    pub fn last_user(&self) -> Option<&str> {
        self.messages.iter().rev().find_map(|said| match said {
            Said::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }
}

/// An item of a conversation a scripted model was shown, kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Said {
    /// What a user said.
    User {
        /// Who.
        speaker: Option<ParticipantId>,
        /// The words.
        text: String,
    },
    /// What the agent said.
    Assistant {
        /// The words.
        text: String,
    },
    /// A tool call the agent asked for.
    ToolCall {
        /// The call.
        call: ToolCallId,
        /// The tool's name.
        tool: String,
        /// Its arguments.
        args: String,
    },
    /// How a tool call ended.
    ToolResult {
        /// The call.
        call: ToolCallId,
        /// What the model is told.
        text: String,
    },
    /// A note to the model.
    Notice {
        /// The note.
        text: String,
    },
}

impl From<&Message<'_>> for Said {
    fn from(message: &Message<'_>) -> Self {
        match *message {
            Message::User { speaker, text } => Self::User {
                speaker,
                text: text.to_owned(),
            },
            Message::Assistant { text } => Self::Assistant {
                text: text.to_owned(),
            },
            Message::ToolCall { call, tool, args } => Self::ToolCall {
                call,
                tool: tool.to_owned(),
                args: args.to_owned(),
            },
            Message::ToolResult { call, text } => Self::ToolResult {
                call,
                text: text.to_owned(),
            },
            Message::Notice { text } => Self::Notice {
                text: text.to_owned(),
            },
        }
    }
}

type Respond = dyn Fn(&Asked) -> Vec<LlmStep> + Send + Sync;

/// A language model that follows a script.
#[derive(Clone)]
pub struct ScriptedLlm {
    /// What it says it can do.
    pub caps: LlmCaps,
    /// How long it takes to start writing.
    pub first: Duration,
    /// How long it takes over each piece after the first.
    pub each: Duration,
    /// Whether it keeps no record of what it was asked: for a run so
    /// long that the record would be most of what the run holds.
    pub forgets: bool,
    respond: Arc<Respond>,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl fmt::Debug for ScriptedLlm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptedLlm")
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}

impl ScriptedLlm {
    /// A model that answers each request with what `respond` makes of it.
    #[must_use]
    pub fn with(respond: impl Fn(&Asked) -> Vec<LlmStep> + Send + Sync + 'static) -> Self {
        Self {
            caps: LlmCaps {
                tools: true,
                parallel_tools: true,
            },
            first: Duration::from_millis(200),
            each: Duration::from_millis(20),
            forgets: false,
            respond: Arc::new(respond),
            asked: Arc::default(),
        }
    }

    /// A model that answers its first request with the first of
    /// `replies`, its second with the second, and so on. A request there
    /// is no reply for fails.
    #[must_use]
    pub fn replies(replies: Vec<Vec<LlmStep>>) -> Self {
        let replies = Mutex::new(replies.into_iter());
        Self::with(move |_| {
            replies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .next()
                .unwrap_or_else(|| {
                    vec![LlmStep::Fail(PortError::permanent(
                        "the script has no reply for this request",
                    ))]
                })
        })
    }

    /// The requests it was given so far.
    #[must_use]
    pub fn asked(&self) -> Vec<Asked> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Llm for ScriptedLlm {
    fn caps(&self) -> LlmCaps {
        self.caps
    }

    fn start<'a>(
        &'a self,
        request: LlmRequest<'a>,
    ) -> BoxFuture<'a, Result<Pin<Box<dyn LlmRx>>, PortError>> {
        // Read here, before anything waits.
        let asked = Asked {
            instructions: request.instructions.to_owned(),
            messages: request.messages.iter().map(Said::from).collect(),
            tools: request
                .tools
                .iter()
                .map(|tool| tool.name.to_owned())
                .collect(),
            max_output_tokens: request.max_output_tokens,
        };
        let steps = (self.respond)(&asked);
        if !self.forgets {
            self.asked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(asked);
        }
        let (first, each) = (self.first, self.each);
        Box::pin(async move {
            let rx = stream::events(move |mut events| async move {
                let mut wait = first;
                let mut called = false;
                let mut written = 0_u32;
                for step in steps {
                    let finish = match step {
                        LlmStep::Say(text) => {
                            for word in text.split_inclusive(' ') {
                                tokio::time::sleep(core::mem::replace(&mut wait, each)).await;
                                written += 1;
                                let text = word.to_owned();
                                events.send(LlmEvent::Delta { text }).await;
                            }
                            continue;
                        }
                        LlmStep::Delta(text) => {
                            tokio::time::sleep(core::mem::replace(&mut wait, each)).await;
                            written += 1;
                            events.send(LlmEvent::Delta { text }).await;
                            continue;
                        }
                        LlmStep::Call { tool, args } => {
                            tokio::time::sleep(core::mem::replace(&mut wait, each)).await;
                            called = true;
                            events.send(LlmEvent::ToolCall { tool, args }).await;
                            continue;
                        }
                        LlmStep::Fail(error) => return Err(error),
                        LlmStep::Hang => {
                            core::future::pending::<()>().await;
                            continue;
                        }
                        LlmStep::Stop(finish) => finish,
                    };
                    events.send(done(finish, written)).await;
                    return Ok(());
                }
                tokio::time::sleep(wait).await;
                let finish = if called {
                    Finish::ToolCalls
                } else {
                    Finish::Stop
                };
                events.send(done(finish, written)).await;
                Ok(())
            });
            let rx: Pin<Box<dyn LlmRx>> = Box::pin(rx);
            Ok(rx)
        })
    }
}

fn done(finish: Finish, written: u32) -> LlmEvent {
    LlmEvent::Done {
        finish,
        usage: Some(Usage {
            input: 100,
            cached: 0,
            output: written,
        }),
    }
}
