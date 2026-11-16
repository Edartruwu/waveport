//! A journal that keeps everything in memory.

use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Every record, from the one numbered 1. A journal would let go of
    /// those a snapshot covers. This one keeps them for a test to look
    /// at, and gives a new owner only the ones after the snapshot.
    records: Vec<Bytes>,
    /// How many times the session asked not to be kept waiting.
    hurried: u64,
}

impl Stored {
    fn next(&self) -> Seq {
        Seq(1 + u64::try_from(self.records.len()).unwrap_or(u64::MAX))
    }

    /// How many records the snapshot covers.
    fn covered(&self) -> usize {
        let upto = self.snapshot.as_ref().map_or(0, |snapshot| snapshot.upto.0);
        usize::try_from(upto)
            .unwrap_or(usize::MAX)
            .min(self.records.len())
    }

    /// The records after the snapshot.
    fn tail(&self) -> Vec<Bytes> {
        self.records.iter().skip(self.covered()).cloned().collect()
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
    /// The number of the last record that is stored before the journal
    /// stops, if it is to stop.
    stops_after: Arc<Mutex<Option<u64>>>,
    stopped: Arc<AtomicBool>,
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
            stops_after: Arc::default(),
            stopped: Arc::default(),
        }
    }

    /// Makes the journal stop once it holds the record numbered `last`
    /// of a session: what a process that dies does to its journal. The
    /// records up to that one are stored, also when they are the
    /// beginning of a batch, and nothing after it is. A request that
    /// meets the end is never answered.
    pub fn stop_after(&self, last: u64) {
        *self
            .stops_after
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(last);
    }

    /// Whether the journal has stopped.
    #[must_use]
    pub fn has_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Lets a journal that was to stop, or has stopped, go on: what it
    /// stored is what the next process finds.
    pub fn go_on(&self) {
        *self
            .stops_after
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.stopped.store(false, Ordering::Relaxed);
    }

    /// Makes the next append fail with `error`, having stored nothing.
    pub fn fail_next_append(&self, error: PortError) {
        *self
            .fail_next
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(error);
    }

    /// Every record ever stored of `session`, in order: also the ones a
    /// snapshot has replaced since, which a journal that is not for tests
    /// would have let go of.
    #[must_use]
    pub fn records(&self, session: SessionId) -> Vec<Bytes> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&session)
            .map_or_default(|stored| stored.records.clone())
    }

    /// The newest snapshot stored of `session`, and how many records
    /// follow it: what a new owner of the session is given.
    #[must_use]
    pub fn kept(&self, session: SessionId) -> (Option<Snapshot>, usize) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&session)
            .map_or((None, 0), |stored| {
                (stored.snapshot.clone(), stored.tail().len())
            })
    }

    /// How many times `session` asked not to be kept waiting.
    #[must_use]
    pub fn hurried(&self, session: SessionId) -> u64 {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&session)
            .map_or(0, |stored| stored.hurried)
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
            let stored = sessions.entry(session).or_default();
            stored.lease = Lease(stored.lease.0 + 1);
            Ok(Recovered {
                lease: stored.lease,
                snapshot: stored.snapshot.clone(),
                tail: stored.tail(),
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
            let stops_after = *self
                .stops_after
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let answer = self.with(session, lease, |stored| {
                if let Some(error) = failure {
                    return Err(JournalError::Store(error));
                }
                let expected = stored.next();
                if batch.first != expected {
                    return Err(JournalError::OutOfOrder { expected });
                }
                // A journal that stops stores what comes before its end.
                let room = stops_after.map_or(usize::MAX, |last| {
                    usize::try_from(last.saturating_sub(expected.0.saturating_sub(1)))
                        .unwrap_or(usize::MAX)
                });
                let whole = batch.records.len() <= room;
                stored.records.extend(batch.records.into_iter().take(room));
                if !whole {
                    self.stopped.store(true, Ordering::Relaxed);
                }
                Ok(Seq(stored.next().0 - 1))
            });
            if self.has_stopped() {
                core::future::pending::<()>().await;
            }
            answer
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
                let held = stored.snapshot.as_ref().map_or(0, |held| held.upto.0);
                if snapshot.upto >= next || snapshot.upto.0 < held {
                    // A state as of a record that is not stored, or that
                    // an earlier snapshot has already replaced.
                    return Err(JournalError::OutOfOrder { expected: next });
                }
                stored.snapshot = Some(snapshot);
                Ok(())
            })
        })
    }

    fn hurry(&self, session: SessionId) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(stored) = sessions.get_mut(&session) {
            stored.hurried += 1;
        }
    }
}
