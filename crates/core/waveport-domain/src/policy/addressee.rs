//! Whether a user turn is meant for the agent.

use alloc::string::String;
use alloc::vec::Vec;

use crate::ids::ParticipantId;

/// What the kernel knows about a turn that has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddresseeInput<'a> {
    /// Who spoke.
    pub who: ParticipantId,
    /// What they said.
    pub text: &'a str,
    /// How many humans are in the session.
    pub humans: usize,
}

/// Decides whether a user turn is meant for the agent. A turn that is not
/// still enters the conversation, but is owed no reply.
pub trait AddresseePolicy {
    /// Whether the agent should reply to the turn.
    fn addressed(&self, input: &AddresseeInput<'_>) -> bool;
}

/// Every turn is meant for the agent. Right for a conversation between one
/// human and one agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlwaysAddressed;

impl AddresseePolicy for AlwaysAddressed {
    fn addressed(&self, _input: &AddresseeInput<'_>) -> bool {
        true
    }
}

/// A turn is meant for the agent when it contains one of the agent's names
/// as a whole word, in any letter case. For a conversation among several
/// humans in which the agent speaks when spoken to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddressedByName {
    names: Vec<String>,
}

impl AddressedByName {
    /// The policy for an agent that answers to `names`.
    #[must_use]
    pub fn new(names: &[&str]) -> Self {
        Self {
            names: names
                .iter()
                .map(|name| name.trim().to_lowercase())
                .filter(|name| !name.is_empty())
                .collect(),
        }
    }
}

impl AddresseePolicy for AddressedByName {
    fn addressed(&self, input: &AddresseeInput<'_>) -> bool {
        input
            .text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .any(|word| {
                self.names
                    .iter()
                    .any(|name| word.chars().flat_map(char::to_lowercase).eq(name.chars()))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{AddressedByName, AddresseeInput, AddresseePolicy, AlwaysAddressed};
    use crate::ids::ParticipantId;

    fn input(text: &str) -> AddresseeInput<'_> {
        AddresseeInput {
            who: ParticipantId(1),
            text,
            humans: 2,
        }
    }

    #[test]
    fn always_means_always() {
        assert!(AlwaysAddressed.addressed(&input("")));
    }

    #[test]
    fn a_name_counts_only_as_a_whole_word() {
        let policy = AddressedByName::new(&["Ava", " Éloïse "]);
        assert!(policy.addressed(&input("ava what is the total")));
        assert!(policy.addressed(&input("What do you think, AVA?")));
        assert!(policy.addressed(&input("merci ÉLOÏSE")));
        assert!(!policy.addressed(&input("the lava is hot")));
        assert!(!policy.addressed(&input("ben did you send the invoice")));
        assert!(!policy.addressed(&input("")));
    }

    #[test]
    fn no_names_means_never() {
        assert!(!AddressedByName::new(&["  "]).addressed(&input("hello there")));
    }
}
