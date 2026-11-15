//! The journal: where a session's facts are kept so that it can go on
//! after its process died.
//!
//! A journal stores bytes. What a record means is the runtime's business;
//! the journal's is that records are numbered without gaps, that only the
//! session's current owner can add any, and that what it said was stored
//! is still there after a crash.

use core::fmt;

use bytes::Bytes;
pub use waveport_domain::ids::Seq;

use crate::error::{BoxFuture, PortError};

/// A session's identity, for as long as it is remembered anywhere.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u128);

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({self})")
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Which owner of a session is writing. Each time a session is taken
/// over the number goes up, and what an earlier owner writes after that
/// is refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lease(pub u64);

/// A store for the facts of sessions.
pub trait Journal: Send + Sync + 'static {
    /// Takes the session over: returns what is stored of it and a lease
    /// that outranks every lease given before. A session nothing is
    /// stored of is a new one.
    fn acquire(&self, session: SessionId) -> BoxFuture<'_, Result<Recovered, JournalError>>;

    /// Adds records. The future is done when they are stored durably, and
    /// gives the number of the last one.
    fn append(
        &self,
        session: SessionId,
        lease: Lease,
        batch: Batch,
    ) -> BoxFuture<'_, Result<Seq, JournalError>>;

    /// Stores the session's state as of a record, so that recovery can
    /// start there and not at the first record. Records up to that one may
    /// then be forgotten.
    fn snapshot(
        &self,
        session: SessionId,
        lease: Lease,
        snapshot: Snapshot,
    ) -> BoxFuture<'_, Result<(), JournalError>>;

    /// Says that the session is waiting for what it has handed over to be
    /// stored: a tool with side effects is held back until it is.
    ///
    /// A journal may wait a little for more to store in one go, which
    /// costs a session nothing as long as nothing waits for the answer.
    /// This asks it not to. A journal that never waits has nothing to do,
    /// which is what this does unless it is implemented.
    fn hurry(&self, session: SessionId) {
        let _ = session;
    }
}

/// Records to add, one for each fact, numbered from `first` without gaps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    /// The number of the first record.
    pub first: Seq,
    /// The records.
    pub records: Vec<Bytes>,
}

/// A session's state as of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The last record the state includes.
    pub upto: Seq,
    /// The state.
    pub state: Bytes,
}

/// What is stored of a session, and the right to add to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// The lease to write with.
    pub lease: Lease,
    /// The newest snapshot, if there is one.
    pub snapshot: Option<Snapshot>,
    /// The records after the snapshot, in order.
    pub tail: Vec<Bytes>,
    /// The number the next record has to have.
    pub next: Seq,
}

impl Recovered {
    /// Whether nothing was stored: the session is new.
    #[must_use]
    pub fn is_new(&self) -> bool {
        self.snapshot.is_none() && self.tail.is_empty()
    }
}

/// Why a journal refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalError {
    /// The session has a newer owner. Nothing more may be written.
    Fenced,
    /// The records do not follow what is stored.
    OutOfOrder {
        /// The number the next record has to have.
        expected: Seq,
    },
    /// The store failed.
    Store(PortError),
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fenced => f.write_str("the session has a newer owner"),
            Self::OutOfOrder { expected } => {
                write!(f, "the next record has to be number {}", expected.0)
            }
            Self::Store(error) => write!(f, "the store failed: {error}"),
        }
    }
}

impl core::error::Error for JournalError {}

#[cfg(test)]
mod tests {
    use super::{JournalError, Lease, Recovered, Seq, SessionId};

    #[test]
    fn a_session_id_reads_as_32_hex_digits() {
        let id = SessionId(0xABC);
        assert_eq!(id.to_string().len(), 32);
        assert!(id.to_string().ends_with("abc"));
        assert_eq!(format!("{id:?}"), format!("SessionId({id})"));
    }

    #[test]
    fn nothing_stored_is_a_new_session() {
        let recovered = Recovered {
            lease: Lease(1),
            snapshot: None,
            tail: Vec::new(),
            next: Seq(1),
        };
        assert!(recovered.is_new());
        assert_eq!(
            JournalError::OutOfOrder { expected: Seq(7) }.to_string(),
            "the next record has to be number 7"
        );
    }
}
