//! The conversation: what was said and done, in the order it became true.
//!
//! Items are only ever appended, and only by applying a fact, so the
//! conversation a language model is shown is exactly what the journal can
//! rebuild.

use alloc::string::String;
use alloc::vec::Vec;

use super::view::ItemView;
use crate::ids::{AgentId, ContextVersion, GenId, ParticipantId, TurnId};

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
    Utterance {
        generation: GenId,
        agent: AgentId,
        text: Span,
        truncated: bool,
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
    fn parking_keeps_the_items() {
        let mut conversation = Conversation::default();
        conversation.push_user_turn(TurnId(1), ParticipantId(1), "hello", true);
        let before = conversation.clone();
        conversation.park();
        assert_eq!(conversation, before);
        assert!(conversation.heap_bytes() >= 5);
    }
}
