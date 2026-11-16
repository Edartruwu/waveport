//! Text entering and leaving the kernel.
//!
//! Text that comes in is owned by the event that carries it. Text that goes
//! out is written into one buffer per step, the [`TextArena`], and commands
//! and facts point into it with a [`TextRef`]. Neither direction allocates
//! once the buffers have grown to their working size.

use alloc::string::String;
use core::fmt;

/// Text carried by an event.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Text(String);

impl Text {
    /// The text as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self(String::from(text))
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stretch of a [`TextArena`]. It means nothing without the arena it was
/// handed out by, and nothing after that arena is cleared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextRef {
    start: u32,
    len: u32,
}

impl TextRef {
    /// A reference to no text at all.
    pub const EMPTY: Self = Self { start: 0, len: 0 };

    /// The length of the text in bytes.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len as usize
    }

    /// Whether the text is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// The text one step sends out, back to back in one buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextArena {
    buffer: String,
}

impl TextArena {
    /// An arena that holds `bytes` bytes before it has to grow.
    #[must_use]
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buffer: String::with_capacity(bytes),
        }
    }

    /// Appends `text` and returns where it went. Text that would take the
    /// arena past 4 GiB is not stored, and the reference is empty.
    pub fn push(&mut self, text: &str) -> TextRef {
        let (Ok(start), Ok(len)) = (u32::try_from(self.buffer.len()), u32::try_from(text.len()))
        else {
            return TextRef::EMPTY;
        };
        if start.checked_add(len).is_none() {
            return TextRef::EMPTY;
        }
        self.buffer.push_str(text);
        TextRef { start, len }
    }

    /// The text `at` refers to, or an empty string if `at` did not come from
    /// this arena since it was last cleared.
    #[must_use]
    pub fn get(&self, at: TextRef) -> &str {
        let start = at.start as usize;
        self.buffer.get(start..start + at.len()).unwrap_or("")
    }

    /// Forgets all text and keeps the buffer.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Bytes in use.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the arena holds no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Text, TextArena, TextRef};

    #[test]
    fn the_arena_hands_back_what_was_pushed() {
        let mut arena = TextArena::with_capacity(64);
        let hello = arena.push("hello");
        let world = arena.push("wörld");
        assert_eq!(arena.get(hello), "hello");
        assert_eq!(arena.get(world), "wörld");
        assert_eq!(world.len(), 6);
        assert_eq!(arena.get(TextRef::EMPTY), "");
        assert_eq!(arena.len(), 11);

        arena.clear();
        assert!(arena.is_empty());
        assert_eq!(arena.get(world), "", "a stale reference reads as empty");
    }

    #[test]
    fn event_text_round_trips() {
        let text = Text::from("turn left");
        assert_eq!(text.as_str(), "turn left");
        assert_eq!(alloc::format!("{text}"), "turn left");
    }
}
