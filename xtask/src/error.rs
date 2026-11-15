//! Error type shared by every xtask command.

use std::fmt;

/// A fatal tool error: bad usage, a missing file, a command that could not run.
///
/// A failed gate is not an error. Gates report their violations and turn them
/// into exit code 1 themselves.
#[derive(Debug)]
pub(crate) struct Error(String);

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub(crate) type Result<T> = std::result::Result<T, Error>;

/// Adds a description of what was being attempted to a foreign error.
pub(crate) trait Context<T> {
    fn context(self, what: impl fmt::Display) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| Error::new(format!("{what}: {e}")))
    }
}
