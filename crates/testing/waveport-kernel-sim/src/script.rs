//! What an agent's language model is going to answer.

use core::time::Duration;

/// One response of a language model, written by the scenario: some text,
/// then any tool calls.
///
/// A script is used up by the next response its agent starts. A response
/// that starts with no script waiting is a failure, which is how a scenario
/// states that an agent responds exactly as often as it says.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub struct Script {
    pub(crate) text: String,
    pub(crate) calls: Vec<(String, String)>,
    pub(crate) think: Option<Duration>,
}

/// A response that says `text`.
pub fn say(text: &str) -> Script {
    Script {
        text: text.to_owned(),
        calls: Vec::new(),
        think: None,
    }
}

impl Script {
    /// After the text, the model also asks for `tool` with these arguments.
    pub fn call(mut self, tool: &str, args: &str) -> Self {
        self.calls.push((tool.to_owned(), args.to_owned()));
        self
    }

    /// The model takes this long before its first word, instead of the
    /// usual 200 ms.
    pub fn after_ms(mut self, millis: u64) -> Self {
        self.think = Some(Duration::from_millis(millis));
        self
    }
}
