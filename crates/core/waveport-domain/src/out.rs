//! What one step produces.

use alloc::vec::Vec;

use crate::command::Command;
use crate::fact::Stamped;
use crate::text::TextArena;

/// The output of one [`Kernel::step`](crate::kernel::Kernel::step).
///
/// The caller owns it and passes the same one to every step, so its buffers
/// are reused. A step clears it first; what it holds afterwards is that
/// step's output and nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Out {
    /// Requests to the runtime, in the order they must be carried out.
    pub cmds: Vec<Command>,
    /// Facts to journal, in order.
    pub facts: Vec<Stamped>,
    /// The text the commands and facts refer to.
    pub text: TextArena,
}

impl Out {
    /// An output with room for a busy step, so that steps do not have to
    /// grow it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cmds: Vec::with_capacity(16),
            facts: Vec::with_capacity(8),
            text: TextArena::with_capacity(4096),
        }
    }

    /// Empties the output and keeps its buffers.
    pub fn clear(&mut self) {
        self.cmds.clear();
        self.facts.clear();
        self.text.clear();
    }

    /// Whether the step asked for nothing and decided nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty() && self.facts.is_empty()
    }
}
