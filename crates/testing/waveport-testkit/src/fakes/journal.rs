//! A journal that keeps everything in memory.

use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::journal::{
    Batch, Journal, JournalError, Lease, Recovered, Seq, SessionId, Snapshot,
};

/// What is stored of one session.
#[derive(Clone, Debug, Default)]
struct Stored {
    lease: Lease,
    snapshot: Option<Snapshot>,
    /// The number of the first record in `records`.
    first: u64,
    records: Vec<Bytes>,
}

impl Stored {
    fn next(&self) -> Seq {
        Seq(self.first + u64::try_from(self.records.len()).unwrap_or(u64::MAX))
    }
}

/// A journal that keeps everything in memory. Clones share what is
/// stored, so one can stand for the store and another for a process that
/// comes back to it.
#[derive(Clone, Debug)]
pub struct MemoryJournal {
    /// How long storing takes.
    pub latency: Duration,
    sessions: Arc<Mutex<BTreeMap<SessionId, Stored>>>,
    /// What the next append fails with, once.
    fail_next: Arc<Mutex<Option<PortError>>>,
}

impl Default for MemoryJournal {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryJournal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self {
            latency: Duration::from_millis(2),
            sessions: Arc::default(),
            fail_next: Arc::default(),
        }
    }

    /// Makes the next append fail with `error`, having stored nothing.
    pub fn fail_next_append(&self, error: PortError) {
        *self
            .fail_next
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(error);
    }

    /// Every record stored of `session` that a snapshot has not replaced,
    /// in order.
    #[must_use]
    pub fn records(&self, session: SessionId) -> Vec<Bytes> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&session)
            .map_or_default(|stored| stored.records.clone())
    }

    fn with<R>(
        &self,
        session: SessionId,
        lease: Lease,
        work: impl FnOnce(&mut Stored) -> Result<R, JournalError>,
    ) -> Result<R, JournalError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let stored = sessions.get_mut(&session).ok_or(JournalError::Fenced)?;
        if stored.lease != lease {
            return Err(JournalError::Fenced);
        }
        work(stored)
    }
}

impl Journal for MemoryJournal {
    fn acquire(&self, session: SessionId) -> BoxFuture<'_, Result<Recovered, JournalError>> {
        Box::pin(async move {
            tokio::time::sleep(self.latency).await;
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let stored = sessions.entry(session).or_insert_with(|| Stored {
                first: 1,
                ..Stored::default()
            });
            stored.lease = Lease(stored.lease.0 + 1);
            Ok(Recovered {
                lease: stored.lease,
                snapshot: stored.snapshot.clone(),
                tail: stored.records.clone(),
                next: stored.next(),
            })
        })
    }

    fn append(
        &self,
        session: SessionId,
        lease: Lease,
        batch: Batch,
    ) -> BoxFuture<'_, Result<Seq, JournalError>> {
        Box::pin(async move {
            tokio::time::sleep(self.latency).await;
            let failure = self
                .fail_next
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            self.with(session, lease, |stored| {
                if let Some(error) = failure {
                    return Err(JournalError::Store(error));
                }
                let expected = stored.next();
                if batch.first != expected {
                    return Err(JournalError::OutOfOrder { expected });
                }
                stored.records.extend(batch.records);
                Ok(Seq(stored.next().0 - 1))
            })
        })
    }

    fn snapshot(
        &self,
        session: SessionId,
        lease: Lease,
        snapshot: Snapshot,
    ) -> BoxFuture<'_, Result<(), JournalError>> {
        Box::pin(async move {
            tokio::time::sleep(self.latency).await;
            self.with(session, lease, |stored| {
                let next = stored.next();
                if snapshot.upto >= next || snapshot.upto.0 + 1 < stored.first {
                    // A state as of a record that is not stored, or that
                    // an earlier snapshot has already replaced.
                    return Err(JournalError::OutOfOrder { expected: next });
                }
                let covered = usize::try_from(snapshot.upto.0 + 1 - stored.first)
                    .unwrap_or(usize::MAX)
                    .min(stored.records.len());
                stored.records.drain(..covered);
                stored.first = snapshot.upto.0 + 1;
                stored.snapshot = Some(snapshot);
                Ok(())
            })
        })
    }
}
