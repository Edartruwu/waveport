//! What an agent is made of, and a session put together from it.
//!
//! Stability: experimental
//!
//! An agent is one value, an [`AgentSpec`]: what it is told, the model it
//! thinks with, and what it hears and speaks with if it does either. A
//! [`Stack`] is what the agents of one process share: the session
//! runtime, the models that listen, the tools, the journal and what
//! sends traces and metrics. A session is an agent on a stack, begun
//! over a line or over none, and the stack picks how it is run by what
//! the line carries: an agent on a line with audio hears and speaks, and
//! the same agent on a line without, or on none, reads and writes.
//!
//! Whatever runs agents puts them together here: the `waveport` command
//! from what its command line says, and a server from a file. So one
//! definition is one agent, however it is reached.
//!
//! Nothing here decides anything about a conversation, and nothing here
//! prints: what whoever runs the program should know is returned as
//! notes, in words for them.
//!
//! A key is never part of a definition. A definition names the
//! environment variable that holds one, and [`AgentSpec::from_toml`]
//! refuses a definition that holds anything it does not know, without
//! repeating what it held.

pub mod halt;
pub mod kept;
mod providers;
pub mod spec;
pub mod stack;
pub mod tools;

use core::fmt;

pub use waveport_runtime::pinning;
pub use waveport_store_journal::ResumeToken;

pub use crate::kept::{Kept, Resume};
pub use crate::spec::{AgentSpec, Keeps, Named, StackSpec};
pub use crate::stack::{Agent, Call, Settled, Stack, Told};

/// Why something could not be done, in words for whoever runs the
/// program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure(pub String);

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::error::Error for Failure {}

/// Says what was being done when something failed.
pub trait Context<T> {
    /// The value, or a failure that begins with what `what` says.
    ///
    /// # Errors
    ///
    /// The failure there was, after what `what` says.
    fn with(self, what: impl FnOnce() -> String) -> Result<T, Failure>;
}

impl<T, E: fmt::Display> Context<T> for Result<T, E> {
    fn with(self, what: impl FnOnce() -> String) -> Result<T, Failure> {
        self.map_err(|error| Failure(format!("{}: {error}", what())))
    }
}
