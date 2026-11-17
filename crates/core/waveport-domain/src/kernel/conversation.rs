//! The conversation: what was said and done, in the order it became true.
//!
//! Items are only ever appended, and only by applying a fact, so the
//! conversation a language model is shown is exactly what the journal can
//! rebuild.

use alloc::string::String;
use alloc::vec::Vec;

use super::view::ItemView;
use crate::fact::{NoticeKind, ToolOutcome};
use crate::ids::{AgentId, ContextVersion, GenId, ParticipantId, ToolCallId, ToolId, TurnId};

/// A stretch of the conversation's text buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Span {
    start: u32,
    len: u32,
}

/// One item, with its text held in the conversation's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Item {
    UserTurn {
        turn: TurnId,
        who: ParticipantId,
        text: Span,
        addressed: bool,
    },
    Keys {
        turn: TurnId,
        who: ParticipantId,
        keys: Span,
    },
    Utterance {
        generation: GenId,
        agent: AgentId,
        text: Span,
        truncated: bool,
    },
    ToolCall {
        call: ToolCallId,
        tool: ToolId,
        generation: GenId,
        args: Span,
    },
    ToolResult {
        call: ToolCallId,
        outcome: ToolOutcome,
        text: Span,
    },
    Notice {
        kind: NoticeKind,
        text: Span,
    },
}

/// The items of a conversation and their text, back to back in one buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct Conversation {
    items: Vec<Item>,
    text: String,
}

impl Conversation {
    /// How many items have been appended.
    pub(super) fn version(&self) -> ContextVersion {
        ContextVersion(u32::try_from(self.items.len()).unwrap_or(u32::MAX))
    }

    /// Stores `text` and returns where. Text past 4 GiB is not stored.
    fn keep(&mut self, text: &str) -> Span {
        let (Ok(start), Ok(len)) = (u32::try_from(self.text.len()), u32::try_from(text.len()))
        else {
            return Span { start: 0, len: 0 };
        };
        if start.checked_add(len).is_none() {
            return Span { start: 0, len: 0 };
        }
        self.text.push_str(text);
        Span { start, len }
    }

    fn text_of(&self, span: Span) -> &str {
        let start = span.start as usize;
        self.text
            .get(start..start + span.len as usize)
            .unwrap_or("")
    }

    /// Appends what a user said.
    pub(super) fn push_user_turn(
        &mut self,
        turn: TurnId,
        who: ParticipantId,
        text: &str,
        addressed: bool,
    ) {
        let text = self.keep(text);
        self.items.push(Item::UserTurn {
            turn,
            who,
            text,
            addressed,
        });
    }

    /// Appends the keys a user pressed.
    pub(super) fn push_keys(&mut self, turn: TurnId, who: ParticipantId, keys: &str) {
        let keys = self.keep(keys);
        self.items.push(Item::Keys { turn, who, keys });
    }

    /// Appends what an agent said, as far as it was heard.
    pub(super) fn push_utterance(
        &mut self,
        generation: GenId,
        agent: AgentId,
        text: &str,
        truncated: bool,
    ) {
        let text = self.keep(text);
        self.items.push(Item::Utterance {
            generation,
            agent,
            text,
            truncated,
        });
    }

    /// Appends a tool call an agent asked for.
    pub(super) fn push_tool_call(
        &mut self,
        call: ToolCallId,
        tool: ToolId,
        generation: GenId,
        args: &str,
    ) {
        let args = self.keep(args);
        self.items.push(Item::ToolCall {
            call,
            tool,
            generation,
            args,
        });
    }

    /// Appends how a tool call ended.
    pub(super) fn push_tool_result(&mut self, call: ToolCallId, outcome: ToolOutcome, text: &str) {
        let text = self.keep(text);
        self.items.push(Item::ToolResult {
            call,
            outcome,
            text,
        });
    }

    /// Appends a note to the language model.
    pub(super) fn push_notice(&mut self, kind: NoticeKind, text: &str) {
        let text = self.keep(text);
        self.items.push(Item::Notice { kind, text });
    }

    /// The arguments of a tool call, as the conversation holds them.
    pub(super) fn call_args(&self, call: ToolCallId) -> &str {
        self.items
            .iter()
            .rev()
            .find_map(|item| match *item {
                Item::ToolCall { call: of, args, .. } if of == call => Some(self.text_of(args)),
                _ => None,
            })
            .unwrap_or("")
    }

    /// The items, oldest first.
    pub(super) fn iter(&self) -> impl Iterator<Item = ItemView<'_>> {
        self.items.iter().map(|item| match *item {
            Item::UserTurn {
                turn,
                who,
                text,
                addressed,
            } => ItemView::UserTurn {
                turn,
                who,
                text: self.text_of(text),
                addressed,
            },
            Item::Keys { turn, who, keys } => ItemView::Keys {
                turn,
                who,
                keys: self.text_of(keys),
            },
            Item::Utterance {
                generation,
                agent,
                text,
                truncated,
            } => ItemView::Utterance {
                generation,
                agent,
                text: self.text_of(text),
                truncated,
            },
            Item::ToolCall {
                call,
                tool,
                generation,
                args,
            } => ItemView::ToolCall {
                call,
                tool,
                generation,
                args: self.text_of(args),
            },
            Item::ToolResult {
                call,
                outcome,
                text,
            } => ItemView::ToolResult {
                call,
                outcome,
                text: self.text_of(text),
            },
            Item::Notice { kind, text } => ItemView::Notice {
                kind,
                text: self.text_of(text),
            },
        })
    }

    /// Gives back reserved space that is not in use.
    pub(super) fn park(&mut self) {
        self.items.shrink_to_fit();
        self.text.shrink_to_fit();
    }

    /// Bytes of heap the conversation holds.
    pub(super) fn heap_bytes(&self) -> usize {
        self.items
            .capacity()
            .saturating_mul(size_of::<Item>())
            .saturating_add(self.text.capacity())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::Conversation;
    use crate::ids::{AgentId, ContextVersion, GenId, ParticipantId, TurnId};
    use crate::kernel::view::ItemView;

    #[test]
    fn items_come_back_in_order_with_their_text() {
        let mut conversation = Conversation::default();
        assert_eq!(conversation.version(), ContextVersion(0));
        conversation.push_user_turn(TurnId(1), ParticipantId(1), "where is my order", true);
        conversation.push_utterance(GenId(1), AgentId(0), "Your order shipped", true);
        conversation.push_user_turn(TurnId(2), ParticipantId(2), "", false);
        assert_eq!(conversation.version(), ContextVersion(3));

        let items = conversation.iter().collect::<Vec<_>>();
        assert_eq!(
            items,
            [
                ItemView::UserTurn {
                    turn: TurnId(1),
                    who: ParticipantId(1),
                    text: "where is my order",
                    addressed: true,
                },
                ItemView::Utterance {
                    generation: GenId(1),
                    agent: AgentId(0),
                    text: "Your order shipped",
                    truncated: true,
                },
                ItemView::UserTurn {
                    turn: TurnId(2),
                    who: ParticipantId(2),
                    text: "",
                    addressed: false,
                },
            ]
        );
    }

    #[test]
    fn tool_calls_keep_their_arguments_and_results() {
        use crate::fact::{NoticeKind, ToolOutcome};
        use crate::ids::{ToolCallId, ToolId};

        let mut conversation = Conversation::default();
        conversation.push_tool_call(ToolCallId(1), ToolId(0), GenId(1), r#"{"which":"last"}"#);
        conversation.push_tool_call(ToolCallId(2), ToolId(1), GenId(1), "{}");
        conversation.push_tool_result(ToolCallId(2), ToolOutcome::Completed, "done");
        conversation.push_notice(NoticeKind::Recovered, "the session was resumed");
        assert_eq!(conversation.call_args(ToolCallId(1)), r#"{"which":"last"}"#);
        assert_eq!(conversation.call_args(ToolCallId(2)), "{}");
        assert_eq!(conversation.call_args(ToolCallId(3)), "");
        let items = conversation.iter().collect::<Vec<_>>();
        assert_eq!(
            items[2..],
            [
                ItemView::ToolResult {
                    call: ToolCallId(2),
                    outcome: ToolOutcome::Completed,
                    text: "done",
                },
                ItemView::Notice {
                    kind: NoticeKind::Recovered,
                    text: "the session was resumed",
                },
            ]
        );
    }

    #[test]
    fn parking_keeps_the_items() {
        let mut conversation = Conversation::default();
        conversation.push_user_turn(TurnId(1), ParticipantId(1), "hello", true);
        let before = conversation.clone();
        conversation.park();
        assert_eq!(conversation, before);
        assert!(conversation.heap_bytes() >= 5);
    }
}
