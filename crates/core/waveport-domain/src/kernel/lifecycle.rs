//! The end of a session's life.
//!
//! A session asked to close first drains: it finishes the response under
//! way and starts nothing new. When that is done, or the drain time is up,
//! it stops whatever is left and is closed. Nothing in this depends on an
//! adapter answering: every transition is taken on the clock.

use crate::event::CloseReason;
use crate::time::MonoTime;

/// What a closing session needs to remember.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Closing {
    /// Why the session is closing.
    pub(super) reason: CloseReason,
    /// When the session stops waiting for what is under way.
    pub(super) drain_until: MonoTime,
}
