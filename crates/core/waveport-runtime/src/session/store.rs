//! How a session's facts reach its journal.
//!
//! Every fact the kernel emits is written as a record and queued. One
//! request to the journal is under way at a time, and everything that was
//! queued while it was goes into the next: the journal commits in groups
//! without being asked to. When a request comes back the kernel is told
//! which fact is safe, which is what it waits for before it lets a tool
//! with side effects run.
//!
//! A store that fails for a while is tried again, with the same records:
//! none is skipped and none is sent twice under another number. A journal
//! that says the session has another owner is believed at once.
//!
//! A journal may wait a little for more to store in one go. While the
//! kernel waits for a fact to be safe, the journal is asked not to.
//!
//! The session's state as of one of its facts goes the same way, once
//! every fact up to that one is safe. A state the journal fails to store
//! is let go of: the records it would have replaced are still there, and
//! the next state replaces them.

use core::task::{Context, Poll};
use core::time::Duration;
use std::sync::Arc;

use bytes::Bytes;
use waveport_domain::event::{ControlInput, Event};
use waveport_domain::fact::Stamped;
use waveport_domain::ids::Seq;
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;
use waveport_ports::error::BoxFuture;
use waveport_ports::journal::{Batch, Journal, JournalError, Lease, SessionId, Snapshot};

use crate::codec::{self, CodecError};

/// How long after a failure the journal is asked again. The wait doubles
/// with each failure that follows, up to [`LONGEST_WAIT`].
const FIRST_WAIT: Duration = Duration::from_millis(50);
const LONGEST_WAIT: Duration = Duration::from_secs(2);

type Appending = BoxFuture<'static, Result<Seq, JournalError>>;
type Keeping = BoxFuture<'static, Result<(), JournalError>>;

/// How the states a session gave its journal fared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateStats {
    /// States the journal stored.
    pub stored: u32,
    /// States that were let go of: the journal failed to store them, or
    /// a newer one was ready before they could be sent.
    pub dropped: u32,
}

/// Why a session can write no more facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreBroken {
    /// Another owner has taken the session over.
    Fenced,
    /// The journal and the session disagree about the number of the next
    /// record.
    OutOfOrder,
    /// A fact could not be written as a record.
    Record(CodecError),
    /// The journal failed in a way that trying again does not mend.
    Failed(String),
}

/// A session's way to its journal.
pub(crate) struct Store {
    journal: Arc<dyn Journal>,
    session: SessionId,
    lease: Lease,
    /// Records that no request has carried yet, and the number of the
    /// first of them.
    queued: Vec<Bytes>,
    first: Seq,
    /// The request under way, and what it carries.
    appending: Option<(Appending, Batch)>,
    /// Records the journal failed to store, and when to send them again.
    again: Option<(MonoTime, Batch)>,
    failures: u32,
    /// The number of the last fact that is safe.
    durable: Seq,
    /// The last fact the kernel said it waits for.
    awaited: Seq,
    /// Whether the journal has been asked to hurry with the request
    /// under way.
    hurried: bool,
    /// A state that waits for the facts it includes to be safe.
    state: Option<Snapshot>,
    /// The state the journal is storing.
    keeping: Option<Keeping>,
    states: StateStats,
    broken: Option<StoreBroken>,
}

impl Store {
    /// A store for a session whose next fact has the number `next`.
    pub(crate) fn new(
        journal: Arc<dyn Journal>,
        session: SessionId,
        lease: Lease,
        next: Seq,
    ) -> Self {
        Self {
            journal,
            session,
            lease,
            queued: Vec::new(),
            first: next,
            appending: None,
            again: None,
            failures: 0,
            durable: Seq(next.get().saturating_sub(1)),
            awaited: Seq(0),
            hurried: false,
            state: None,
            keeping: None,
            states: StateStats::default(),
            broken: None,
        }
    }

    /// The kernel waits for the fact numbered `seq` to be safe.
    pub(crate) fn hurry(&mut self, seq: Seq) {
        self.awaited = self.awaited.max(seq);
    }

    /// Has the journal store `state`, the session's state as of the fact
    /// numbered `upto`, once every fact up to that one is safe. A state
    /// that was still waiting gives way to this one.
    pub(crate) fn keep(&mut self, upto: Seq, state: Bytes) {
        if self.broken.is_some() {
            return;
        }
        if self.state.replace(Snapshot { upto, state }).is_some() {
            self.states.dropped += 1;
        }
    }

    /// How the states given to the journal fared.
    pub(crate) const fn states(&self) -> StateStats {
        self.states
    }

    /// Why nothing more can be written, if that is so.
    pub(crate) const fn broken(&self) -> Option<&StoreBroken> {
        self.broken.as_ref()
    }

    /// Whether every fact that was recorded is safe.
    pub(crate) fn is_idle(&self) -> bool {
        self.queued.is_empty() && self.appending.is_none() && self.again.is_none()
    }

    /// When to look at the store again if nothing wakes the session
    /// before.
    pub(crate) fn deadline(&self) -> Option<MonoTime> {
        self.again.as_ref().map(|(at, _)| *at)
    }

    /// Queues a fact for the journal, and returns it as the record that
    /// was queued. `text` is the arena of the step that emitted it.
    pub(crate) fn record(&mut self, stamped: &Stamped, text: &TextArena) -> Option<Bytes> {
        if self.broken.is_some() {
            return None;
        }
        match codec::encode(stamped, text) {
            Ok(record) => {
                self.queued.push(record.clone());
                Some(record)
            }
            Err(error) => {
                self.broken = Some(StoreBroken::Record(error));
                None
            }
        }
    }

    /// Moves the journal on. Adds to `events` what the kernel is to be
    /// told: that facts are safe, or that the session has another owner.
    /// Returns whether anything changed that may have made more work.
    pub(crate) fn poll(
        &mut self,
        cx: &mut Context<'_>,
        now: MonoTime,
        events: &mut Vec<Event>,
    ) -> bool {
        let mut changed = self.poll_records(cx, now, events);
        changed |= self.poll_state(cx, events);
        // The kernel is held up by what the journal has in hand, so the
        // journal is told not to wait for more to store with it.
        if self.appending.is_some() && self.awaited > self.durable && !self.hurried {
            self.hurried = true;
            self.journal.hurry(self.session);
        }
        changed
    }

    /// Hands the journal the state that waits, once the facts it
    /// includes are safe, and sees how the one it is storing fared.
    fn poll_state(&mut self, cx: &mut Context<'_>, events: &mut Vec<Event>) -> bool {
        if self.broken.is_some() {
            return false;
        }
        if self.keeping.is_none()
            && let Some(state) = self.state.take_if(|state| state.upto <= self.durable)
        {
            let (journal, session, lease) = (Arc::clone(&self.journal), self.session, self.lease);
            let keeping = async move { journal.snapshot(session, lease, state).await };
            self.keeping = Some(Box::pin(keeping));
        }
        let Some(keeping) = &mut self.keeping else {
            return false;
        };
        let Poll::Ready(kept) = keeping.as_mut().poll(cx) else {
            return false;
        };
        self.keeping = None;
        match kept {
            Ok(()) => self.states.stored += 1,
            Err(JournalError::Fenced) => {
                self.broken = Some(StoreBroken::Fenced);
                events.push(Event::Control(ControlInput::LeaseLost));
            }
            // The records the state would have replaced are still there.
            Err(JournalError::OutOfOrder { .. } | JournalError::Store(_)) => {
                self.states.dropped += 1;
            }
        }
        true
    }

    fn poll_records(
        &mut self,
        cx: &mut Context<'_>,
        now: MonoTime,
        events: &mut Vec<Event>,
    ) -> bool {
        if self.broken.is_some() {
            return false;
        }
        let mut changed = false;
        loop {
            if self.appending.is_none() {
                let batch = match self.again.take() {
                    Some((at, batch)) if at <= now => batch,
                    Some(waiting) => {
                        self.again = Some(waiting);
                        return changed;
                    }
                    None if self.queued.is_empty() => return changed,
                    None => {
                        let records = core::mem::take(&mut self.queued);
                        let batch = Batch {
                            first: self.first,
                            records,
                        };
                        let count = u64::try_from(batch.records.len()).unwrap_or(u64::MAX);
                        self.first = Seq(self.first.get().saturating_add(count));
                        batch
                    }
                };
                let (journal, session, lease) =
                    (Arc::clone(&self.journal), self.session, self.lease);
                let sent = batch.clone();
                let appending = async move { journal.append(session, lease, sent).await };
                self.appending = Some((Box::pin(appending), batch));
                self.hurried = false;
            }
            let Some((appending, _)) = &mut self.appending else {
                return changed;
            };
            let Poll::Ready(stored) = appending.as_mut().poll(cx) else {
                return changed;
            };
            changed = true;
            let batch = self.appending.take().map(|(_, batch)| batch);
            match stored {
                Ok(upto) => {
                    self.failures = 0;
                    if upto > self.durable {
                        self.durable = upto;
                        events.push(Event::Durable(upto));
                    }
                }
                Err(JournalError::Fenced) => {
                    self.broken = Some(StoreBroken::Fenced);
                    events.push(Event::Control(ControlInput::LeaseLost));
                    return true;
                }
                Err(JournalError::OutOfOrder { .. }) => {
                    self.broken = Some(StoreBroken::OutOfOrder);
                    events.push(Event::Control(ControlInput::LeaseLost));
                    return true;
                }
                Err(JournalError::Store(error)) if error.is_retryable() => {
                    self.failures += 1;
                    let doublings = self.failures.saturating_sub(1).min(16);
                    let wait = FIRST_WAIT.saturating_mul(1 << doublings).min(LONGEST_WAIT);
                    if let Some(batch) = batch {
                        self.again = Some((now.after(wait), batch));
                    }
                }
                Err(JournalError::Store(error)) => {
                    self.broken = Some(StoreBroken::Failed(error.to_string()));
                    events.push(Event::Control(ControlInput::LeaseLost));
                    return true;
                }
            }
        }
    }
}

impl core::fmt::Debug for Store {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Store")
            .field("session", &self.session)
            .field("queued", &self.queued.len())
            .field("durable", &self.durable)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}
