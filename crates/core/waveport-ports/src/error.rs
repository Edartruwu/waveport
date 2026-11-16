//! How a port fails.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

pub use waveport_domain::event::RetryClass;

/// A future in a box, for the ports that are called a few times in a turn.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A half that takes without waiting has no room.
///
/// What the caller does about it depends on what was refused: audio is
/// dropped, and a command that cannot be delivered means the adapter is
/// not keeping up and is treated as failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Full;

impl fmt::Display for Full {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the port has no room")
    }
}

impl core::error::Error for Full {}

/// A failure of an adapter.
///
/// The class says whether trying again can help, and that is what the
/// runtime acts on. The message is for a person reading a log: it must not
/// hold a key, and nothing decides anything by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortError {
    /// Whether trying again can help.
    pub class: RetryClass,
    /// How long the provider asked to be left alone, when it said.
    pub retry_after: Option<Duration>,
    /// What happened, for a log.
    pub message: Box<str>,
}

impl PortError {
    /// A failure of `class`.
    #[must_use]
    pub fn new(class: RetryClass, message: impl Into<Box<str>>) -> Self {
        Self {
            class,
            retry_after: None,
            message: message.into(),
        }
    }

    /// A failure that is likely not to happen again: a connection that
    /// dropped, a server that had an error of its own.
    #[must_use]
    pub fn transient(message: impl Into<Box<str>>) -> Self {
        Self::new(RetryClass::Transient, message)
    }

    /// A failure that will happen again however often it is tried: a
    /// request the provider cannot make sense of.
    #[must_use]
    pub fn permanent(message: impl Into<Box<str>>) -> Self {
        Self::new(RetryClass::Permanent, message)
    }

    /// The credentials were refused.
    #[must_use]
    pub fn auth(message: impl Into<Box<str>>) -> Self {
        Self::new(RetryClass::Auth, message)
    }

    /// No answer arrived in time.
    #[must_use]
    pub fn timeout(message: impl Into<Box<str>>) -> Self {
        Self::new(RetryClass::Timeout, message)
    }

    /// The provider asked for fewer requests, and perhaps said for how
    /// long.
    #[must_use]
    pub fn rate_limited(retry_after: Option<Duration>, message: impl Into<Box<str>>) -> Self {
        Self {
            class: RetryClass::RateLimited,
            retry_after,
            message: message.into(),
        }
    }

    /// Whether trying the same thing again can help.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.class,
            RetryClass::Transient | RetryClass::RateLimited | RetryClass::Timeout
        )
    }
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = match self.class {
            RetryClass::Transient => "transient",
            RetryClass::RateLimited => "rate limited",
            RetryClass::Auth => "not authorised",
            RetryClass::Permanent => "permanent",
            RetryClass::Timeout => "timed out",
        };
        write!(f, "{class}: {}", self.message)
    }
}

impl core::error::Error for PortError {}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{PortError, RetryClass};

    #[test]
    fn the_class_says_whether_to_try_again() {
        assert!(PortError::transient("the connection dropped").is_retryable());
        assert!(PortError::timeout("no answer").is_retryable());
        assert!(PortError::rate_limited(None, "slow down").is_retryable());
        assert!(!PortError::permanent("no such model").is_retryable());
        assert!(!PortError::auth("the key was refused").is_retryable());
    }

    #[test]
    fn an_error_reads_as_its_class_and_what_happened() {
        let error = PortError::rate_limited(Some(Duration::from_secs(2)), "slow down");
        assert_eq!(error.class, RetryClass::RateLimited);
        assert_eq!(error.retry_after, Some(Duration::from_secs(2)));
        assert_eq!(error.to_string(), "rate limited: slow down");
        assert_eq!(
            PortError::auth("the key was refused").to_string(),
            "not authorised: the key was refused"
        );
    }
}
