//! The policies of a session, with what its shape decides put in the
//! place of what the caller chose.
//!
//! One policy depends on the shape of a session and not on its agent:
//! how much of what the language model has written is handed on. Speech
//! synthesis sounds best on whole sentences, so a session that speaks
//! waits for one. A reader wants every word as it is written, so a
//! session of text hands on all there is, whatever its caller's policy
//! for sentences would say.

use waveport_domain::policy::{Policies, TextChunker};

/// The caller's policies, as the session's shape leaves them.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Shaped<P> {
    policies: P,
    /// Whether text is handed on as it is written.
    as_written: bool,
}

impl<P> Shaped<P> {
    pub(crate) const fn new(policies: P, as_written: bool) -> Self {
        Self {
            policies,
            as_written,
        }
    }
}

impl<P: Policies> TextChunker for Shaped<P> {
    fn ready(&self, pending: &str, done: bool) -> usize {
        if self.as_written {
            pending.len()
        } else {
            self.policies.chunker().ready(pending, done)
        }
    }
}

impl<P: Policies> Policies for Shaped<P> {
    type Turn = P::Turn;
    type Interruption = P::Interruption;
    type Addressee = P::Addressee;
    type Gate = P::Gate;
    type Chunker = Self;

    fn turn(&self) -> &Self::Turn {
        self.policies.turn()
    }

    fn interruption(&self) -> &Self::Interruption {
        self.policies.interruption()
    }

    fn addressee(&self) -> &Self::Addressee {
        self.policies.addressee()
    }

    fn gate(&self) -> &Self::Gate {
        self.policies.gate()
    }

    fn chunker(&self) -> &Self::Chunker {
        self
    }
}
