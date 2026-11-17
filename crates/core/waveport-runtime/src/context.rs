//! What a language model is shown.
//!
//! The kernel keeps the conversation in the order things happened. A model
//! is asked with a list that follows two more rules, so that every adapter
//! can pass it on as it is:
//!
//! - The result of a tool call follows its call directly. Calls that were
//!   asked for together stay together, and their results follow in the
//!   same order.
//! - Every call has a result. One that is still running has a note in its
//!   place that says so.
//!
//! An agent that works on a tool call for another agent is shown the
//! conversation up to that call, and then what it is asked to do.

use waveport_domain::command::LlmCause;
use waveport_domain::ids::{AgentId, ParticipantId, ToolCallId, ToolId};
use waveport_domain::kernel::{ItemView, View};
use waveport_ports::llm::{Message, ToolDecl};

/// What stands where the result of a call will be, while the call runs.
pub const STILL_RUNNING: &str = "(This call has not finished yet.)";

/// What a language model is shown of keys somebody pressed on their
/// telephone: text of theirs that says it is keys, so that a model does
/// not take a number that was pressed for one that was said.
#[must_use]
pub fn keys_as_text(keys: &str) -> String {
    format!("(pressed on the telephone's keys: {keys})")
}

/// What an agent is, beyond what the kernel knows of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentDef {
    /// What the agent is told before the conversation.
    pub instructions: String,
}

/// What a tool is, beyond what the kernel knows of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDef {
    /// The name a language model calls it by.
    pub name: String,
    /// What it does, for the language model.
    pub description: String,
    /// A JSON schema of its arguments.
    pub parameters: String,
}

/// One thing a language model is shown, kept until the model has been
/// asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    User {
        speaker: Option<ParticipantId>,
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        call: ToolCallId,
        tool: ToolId,
        args: String,
    },
    ToolResult {
        call: ToolCallId,
        text: String,
    },
    Notice {
        text: String,
    },
}

impl Item {
    fn of(item: ItemView<'_>) -> Self {
        match item {
            ItemView::UserTurn { who, text, .. } => Self::User {
                speaker: Some(who),
                text: text.to_owned(),
            },
            ItemView::Keys { who, keys, .. } => Self::User {
                speaker: Some(who),
                text: keys_as_text(keys),
            },
            ItemView::Utterance { text, .. } => Self::Assistant {
                text: text.to_owned(),
            },
            ItemView::ToolCall {
                call, tool, args, ..
            } => Self::ToolCall {
                call,
                tool,
                args: args.to_owned(),
            },
            ItemView::ToolResult { call, text, .. } => Self::ToolResult {
                call,
                text: text.to_owned(),
            },
            ItemView::Notice { text, .. } => Self::Notice {
                text: text.to_owned(),
            },
        }
    }
}

/// What one response is computed from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Asking {
    pub agent: AgentId,
    pub items: Vec<Item>,
    /// The tools the model may call.
    pub tools: Vec<ToolId>,
}

impl Asking {
    /// What the agent `agent` is shown for a response with this `cause`.
    pub(crate) fn new(view: View<'_>, agent: AgentId, cause: LlmCause) -> Self {
        let items: Vec<Item> = view.items().map(Item::of).collect();
        let (items, tools) = match cause {
            LlmCause::Reply { .. } | LlmCause::Continuation { .. } => {
                let tools = view
                    .config()
                    .agent(agent)
                    .map_or_default(|spec| spec.tools.clone());
                (arrange(items), tools)
            }
            // An agent that answers a call calls no tools itself.
            LlmCause::Delegated { call } => (delegated(items, call), Vec::new()),
        };
        Self {
            agent,
            items,
            tools,
        }
    }

    /// What the agent is told before the conversation. `agents` are those
    /// of the session, in the kernel's order.
    pub(crate) fn instructions<'a>(&self, agents: &'a [AgentDef]) -> &'a str {
        agents
            .get(usize::from(self.agent.get()))
            .map_or("", |agent| agent.instructions.as_str())
    }

    /// The conversation, as a model is shown it. `tools` are those of the
    /// session, in the kernel's order.
    pub(crate) fn messages<'a>(&'a self, tools: &'a [ToolDef]) -> Vec<Message<'a>> {
        let name = |tool: ToolId| {
            tools
                .get(usize::from(tool.get()))
                .map_or("", |tool| tool.name.as_str())
        };
        self.items
            .iter()
            .map(|item| match item {
                Item::User { speaker, text } => Message::User {
                    speaker: *speaker,
                    text,
                },
                Item::Assistant { text } => Message::Assistant { text },
                Item::ToolCall { call, tool, args } => Message::ToolCall {
                    call: *call,
                    tool: name(*tool),
                    args,
                },
                Item::ToolResult { call, text } => Message::ToolResult { call: *call, text },
                Item::Notice { text } => Message::Notice { text },
            })
            .collect()
    }

    /// The tools the model may call, as it is told of them.
    pub(crate) fn offered<'a>(&self, tools: &'a [ToolDef]) -> Vec<ToolDecl<'a>> {
        self.tools
            .iter()
            .filter_map(|tool| tools.get(usize::from(tool.get())))
            .map(|tool| ToolDecl {
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.parameters,
            })
            .collect()
    }
}

/// Puts the result of every call directly after the calls it was asked
/// for with, and a note where a result is missing.
fn arrange(items: Vec<Item>) -> Vec<Item> {
    let mut results: Vec<(ToolCallId, String)> = Vec::new();
    let mut rest = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::ToolResult { call, text } => results.push((call, text)),
            other => rest.push(other),
        }
    }
    let mut arranged = Vec::with_capacity(rest.len() + results.len());
    // The calls of the run that is being copied, whose results are due
    // when the run ends.
    let mut run: Vec<ToolCallId> = Vec::new();
    let mut close = |run: &mut Vec<ToolCallId>, arranged: &mut Vec<Item>| {
        for call in run.drain(..) {
            let text = results
                .iter()
                .position(|(of, _)| *of == call)
                .map_or_else(|| STILL_RUNNING.to_owned(), |at| results.swap_remove(at).1);
            arranged.push(Item::ToolResult { call, text });
        }
    };
    for item in rest {
        if let Item::ToolCall { call, .. } = &item {
            run.push(*call);
        } else {
            close(&mut run, &mut arranged);
        }
        arranged.push(item);
    }
    close(&mut run, &mut arranged);
    arranged
}

/// What an agent that answers the call `call` is shown: the conversation
/// before the calls that `call` was asked for with, and then the call's
/// arguments, as something it is asked.
fn delegated(mut items: Vec<Item>, call: ToolCallId) -> Vec<Item> {
    let Some(at) = items
        .iter()
        .position(|item| matches!(item, Item::ToolCall { call: this, .. } if *this == call))
    else {
        return arrange(items);
    };
    let task = match items.get(at) {
        Some(Item::ToolCall { args, .. }) => args.clone(),
        _ => String::new(),
    };
    // The run of calls it stands in begins after the last thing before it
    // that is not a call.
    let begins = items
        .get(..at)
        .and_then(|before| {
            before
                .iter()
                .rposition(|item| !matches!(item, Item::ToolCall { .. }))
        })
        .map_or(0, |last| last + 1);
    items.truncate(begins);
    let mut shown = arrange(items);
    shown.push(Item::User {
        speaker: None,
        text: task,
    });
    shown
}

#[cfg(test)]
mod tests {
    use waveport_domain::ids::{AgentId, ParticipantId, ToolCallId, ToolId};
    use waveport_ports::llm::Message;

    use super::{AgentDef, Asking, Item, STILL_RUNNING, ToolDef, arrange, delegated};

    fn user(text: &str) -> Item {
        Item::User {
            speaker: Some(ParticipantId(1)),
            text: text.to_owned(),
        }
    }

    fn said(text: &str) -> Item {
        Item::Assistant {
            text: text.to_owned(),
        }
    }

    fn call(n: u64, args: &str) -> Item {
        Item::ToolCall {
            call: ToolCallId(n),
            tool: ToolId(0),
            args: args.to_owned(),
        }
    }

    fn result(n: u64, text: &str) -> Item {
        Item::ToolResult {
            call: ToolCallId(n),
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_conversation_without_tools_is_shown_as_it_is() {
        let items = vec![user("hello"), said("Hello."), user("what time is it")];
        assert_eq!(arrange(items.clone()), items);
    }

    #[test]
    fn a_result_follows_its_call_whatever_happened_between() {
        // The caller spoke again while the tool ran.
        let items = vec![
            user("what time is it"),
            call(1, "{}"),
            user("are you there"),
            said("One moment."),
            result(1, "09:15"),
        ];
        assert_eq!(
            arrange(items),
            [
                user("what time is it"),
                call(1, "{}"),
                result(1, "09:15"),
                user("are you there"),
                said("One moment."),
            ]
        );
    }

    #[test]
    fn calls_asked_for_together_stay_together() {
        // The second finished first.
        let items = vec![
            user("time and weather"),
            call(1, "{}"),
            call(2, "{}"),
            result(2, "rain"),
            result(1, "09:15"),
            said("It is 09:15 and raining."),
        ];
        assert_eq!(
            arrange(items),
            [
                user("time and weather"),
                call(1, "{}"),
                call(2, "{}"),
                result(1, "09:15"),
                result(2, "rain"),
                said("It is 09:15 and raining."),
            ]
        );
    }

    #[test]
    fn a_call_that_is_still_running_has_a_note_for_a_result() {
        let items = vec![
            user("time and weather"),
            call(1, "{}"),
            call(2, "{}"),
            result(1, "09:15"),
            user("hello?"),
        ];
        assert_eq!(
            arrange(items),
            [
                user("time and weather"),
                call(1, "{}"),
                call(2, "{}"),
                result(1, "09:15"),
                result(2, STILL_RUNNING),
                user("hello?"),
            ]
        );
        // Also when the calls are the last thing there is.
        assert_eq!(
            arrange(vec![user("time"), call(1, "{}")]),
            [user("time"), call(1, "{}"), result(1, STILL_RUNNING)]
        );
    }

    #[test]
    fn an_agent_that_answers_a_call_sees_what_led_to_it_and_its_task() {
        let items = vec![
            user("book me a table"),
            said("For how many?"),
            user("four"),
            call(7, "{}"),
            call(8, "find a table for four"),
            result(7, "ok"),
        ];
        assert_eq!(
            delegated(items, ToolCallId(8)),
            [
                user("book me a table"),
                said("For how many?"),
                user("four"),
                Item::User {
                    speaker: None,
                    text: "find a table for four".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_request_names_tools_and_carries_the_agents_instructions() {
        let agents = [
            AgentDef {
                instructions: "You answer the telephone.".to_owned(),
            },
            AgentDef {
                instructions: "You find tables.".to_owned(),
            },
        ];
        let tools = [
            ToolDef {
                name: "get_time".to_owned(),
                description: "The time of day.".to_owned(),
                parameters: "{}".to_owned(),
            },
            ToolDef {
                name: "find_table".to_owned(),
                description: "Finds a table.".to_owned(),
                parameters: "{\"type\":\"object\"}".to_owned(),
            },
        ];
        let asking = Asking {
            agent: AgentId(1),
            items: vec![user("four"), call(3, "{}"), result(3, "09:15")],
            tools: vec![ToolId(1)],
        };
        assert_eq!(asking.instructions(&agents), "You find tables.");
        let offered = asking.offered(&tools);
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].name, "find_table");
        assert_eq!(offered[0].parameters, "{\"type\":\"object\"}");
        let messages = asking.messages(&tools);
        assert_eq!(
            messages[1],
            Message::ToolCall {
                call: ToolCallId(3),
                tool: "get_time",
                args: "{}"
            }
        );
        assert_eq!(messages.len(), 3);
    }
}
