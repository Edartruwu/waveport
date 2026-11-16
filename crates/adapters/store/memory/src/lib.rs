//! A journal that lasts as long as its process.
//!
//! Stability: experimental
//!
//! Everything a session records is kept, in order, in memory. A session
//! that ends and is started again in the same process goes on where it
//! was. A session whose process dies is gone: this is the journal of a
//! session at a developer's desk and of a test, and of nothing that has
//! to survive.
//!
//! It keeps the rules of the port all the same, so that a session runs
//! with it as it runs with a journal on disk: records are numbered
//! without gaps, only the newest owner of a session can add to it, and
//! what is refused changes nothing.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use waveport_ports::error::BoxFuture;
use waveport_ports::journal::{
    Batch, Journal, JournalError, Lease, Recovered, Seq, SessionId, Snapshot,
};

/// What is kept of one session.
#[derive(Debug)]
struct Stored {
    lease: Lease,
    snapshot: Option<Snapshot>,
    /// The number of the first of `records`.
    first: u64,
    /// The records after the snapshot.
    records: Vec<Bytes>,
}

impl Stored {
    /// The number the next record has to have.
    fn next(&self) -> Seq {
        Seq(self.first + self.records.len() as u64)
    }
}

/// A journal in memory.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    sessions: Mutex<BTreeMap<SessionId, Stored>>,
}

impl MemoryJournal {
    /// A journal that holds nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many records of `session` are held: those after its newest
    /// snapshot.
    #[must_use]
    pub fn held(&self, session: SessionId) -> usize {
        self.lock()
            .get(&session)
            .map_or(0, |stored| stored.records.len())
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<SessionId, Stored>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Does `work` on what is kept of `session`, if `lease` is its
    /// newest.
    fn with<R>(
        &self,
        session: SessionId,
        lease: Lease,
        work: impl FnOnce(&mut Stored) -> Result<R, JournalError>,
    ) -> Result<R, JournalError> {
        let mut sessions = self.lock();
        let stored = sessions.get_mut(&session).ok_or(JournalError::Fenced)?;
        if stored.lease != lease {
            return Err(JournalError::Fenced);
        }
        work(stored)
    }
}

impl Journal for MemoryJournal {
    fn acquire(&self, session: SessionId) -> BoxFuture<'_, Result<Recovered, JournalError>> {
        let mut sessions = self.lock();
        let stored = sessions.entry(session).or_insert_with(|| Stored {
            lease: Lease(0),
            snapshot: None,
            first: 1,
            records: Vec::new(),
        });
        stored.lease = Lease(stored.lease.0 + 1);
        let recovered = Recovered {
            lease: stored.lease,
            snapshot: stored.snapshot.clone(),
            tail: stored.records.clone(),
            next: stored.next(),
        };
        Box::pin(async move { Ok(recovered) })
    }

    fn append(
        &self,
        session: SessionId,
        lease: Lease,
        batch: Batch,
    ) -> BoxFuture<'_, Result<Seq, JournalError>> {
        let answer = self.with(session, lease, |stored| {
            let expected = stored.next();
            if batch.first != expected {
                return Err(JournalError::OutOfOrder { expected });
            }
            stored.records.extend(batch.records);
            Ok(Seq(stored.next().0 - 1))
        });
        Box::pin(async move { answer })
    }

    fn snapshot(
        &self,
        session: SessionId,
        lease: Lease,
        snapshot: Snapshot,
    ) -> BoxFuture<'_, Result<(), JournalError>> {
        let answer = self.with(session, lease, |stored| {
            let next = stored.next();
            // Of a record that is not stored, or of one before the
            // snapshot there is already: neither is the state of this
            // session as it is held.
            if snapshot.upto >= next || snapshot.upto.0 + 1 < stored.first {
                return Err(JournalError::OutOfOrder { expected: next });
            }
            // The records up to it are let go of.
            let covered = usize::try_from(snapshot.upto.0 + 1 - stored.first).unwrap_or(0);
            stored.records.drain(..covered.min(stored.records.len()));
            stored.first = snapshot.upto.0 + 1;
            stored.snapshot = Some(snapshot);
            Ok(())
        });
        Box::pin(async move { answer })
    }
}
