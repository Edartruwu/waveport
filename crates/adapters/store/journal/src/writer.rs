//! The thread that writes the journal's file.
//!
//! Everything that changes the file is done here, one group at a time. A
//! group is whatever was asked for while the group before it was being
//! committed, and it is committed as one transaction, with one sync of
//! the file: the more is asked for, the more a sync stores.
//!
//! The writer does not begin a commit sooner than [`Settings::gather`]
//! after the one before, so that under load a commit has had time to
//! fill. A request that someone is held up by is not kept waiting for
//! that: the journal is told with [`Request::Hurry`], and commits what it
//! has at once.
//!
//! A request is refused on what the file holds and on what the requests
//! before it in its group did. If the commit of a group fails, none of
//! that happened, so every request of the group is answered with the
//! failure, also the ones that had been refused.
//!
//! A commit that fails leaves a doubt: the file may hold it all the same,
//! if it was the sync that failed and not a write. So the file is let go
//! of and opened again, which brings it back to what it last committed,
//! and a count that every commit moves on says which that is. If the
//! group is not in the file, its requests may be tried again. If it is,
//! they are answered with a failure that trying again does not mend: the
//! records are there, and nothing vouches for their being safe.

use core::time::Duration;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use redb::{Database, ReadableTable, Table, WriteTransaction};
use tokio::sync::oneshot;
use waveport_ports::error::PortError;
use waveport_ports::journal::{Batch, JournalError, Lease, Recovered, Seq, SessionId, Snapshot};

use crate::layout::{
    BATCHES, COMMITS_KEY, FORMAT, FORMAT_KEY, HEADS, Head, LABEL_MOST, LABELS, META, STATES,
    batch_len, read_batch, unreadable, write_batch,
};
use crate::token::same;
use crate::{JournalStats, OpenError, Probe, Seen};

/// Where the answer to a request goes.
pub(crate) type Reply<T> = oneshot::Sender<Result<T, JournalError>>;

/// Something asked of the writer.
pub(crate) enum Request {
    Acquire {
        session: SessionId,
        reply: Reply<Recovered>,
    },
    Append {
        session: SessionId,
        lease: Lease,
        batch: Batch,
        reply: Reply<Seq>,
    },
    Snapshot {
        session: SessionId,
        lease: Lease,
        snapshot: Snapshot,
        reply: Reply<()>,
    },
    /// Makes `hash` the hash of the session's resume token.
    Issue {
        session: SessionId,
        hash: [u8; 32],
        reply: Reply<()>,
    },
    /// Whether `hash` is the hash of the session's resume token.
    Admit {
        session: SessionId,
        hash: [u8; 32],
        reply: Reply<bool>,
    },
    /// Lets go of everything that is stored of the session.
    Forget {
        session: SessionId,
        reply: Reply<()>,
    },
    /// The sessions the journal holds anything of.
    Sessions { reply: Reply<Vec<SessionId>> },
    /// Keeps `label` with the session, in place of the one before.
    Label {
        session: SessionId,
        label: Vec<u8>,
        reply: Reply<()>,
    },
    /// The sessions the journal holds anything of, each with its label.
    Labelled { reply: Reply<Vec<Labelled>> },
    /// Someone is held up by what was asked: commit without waiting for
    /// more.
    Hurry,
}

/// A session the journal holds something of, and the label that is kept
/// with it, if one is.
pub type Labelled = (SessionId, Option<Vec<u8>>);

/// A request that has been worked out, and the answer it gets if its
/// group is committed.
enum Answer {
    Acquired(Reply<Recovered>, Result<Recovered, JournalError>),
    Appended(Reply<Seq>, Result<Seq, JournalError>),
    Done(Reply<()>, Result<(), JournalError>),
    Admitted(Reply<bool>, Result<bool, JournalError>),
    Sessions(Reply<Vec<SessionId>>, Result<Vec<SessionId>, JournalError>),
    Labelled(Reply<Vec<Labelled>>, Result<Vec<Labelled>, JournalError>),
}

impl Answer {
    /// Gives the answer. Nobody may be waiting for it any more.
    fn send(self) {
        match self {
            Self::Acquired(reply, answer) => drop(reply.send(answer)),
            Self::Appended(reply, answer) => drop(reply.send(answer)),
            Self::Done(reply, answer) => drop(reply.send(answer)),
            Self::Admitted(reply, answer) => drop(reply.send(answer)),
            Self::Sessions(reply, answer) => drop(reply.send(answer)),
            Self::Labelled(reply, answer) => drop(reply.send(answer)),
        }
    }

    /// Answers with a failure of the store instead.
    fn fail(self, failure: &PortError) {
        let failed = || JournalError::Store(failure.clone());
        match self {
            Self::Acquired(reply, _) => drop(reply.send(Err(failed()))),
            Self::Appended(reply, _) => drop(reply.send(Err(failed()))),
            Self::Done(reply, _) => drop(reply.send(Err(failed()))),
            Self::Admitted(reply, _) => drop(reply.send(Err(failed()))),
            Self::Sessions(reply, _) => drop(reply.send(Err(failed()))),
            Self::Labelled(reply, _) => drop(reply.send(Err(failed()))),
        }
    }
}

impl Request {
    /// Answers a request that was not worked out with a failure of the
    /// store.
    fn fail(self, failure: &PortError) {
        let failed = || JournalError::Store(failure.clone());
        match self {
            Self::Acquire { reply, .. } => drop(reply.send(Err(failed()))),
            Self::Append { reply, .. } => drop(reply.send(Err(failed()))),
            Self::Snapshot { reply, .. }
            | Self::Issue { reply, .. }
            | Self::Forget { reply, .. }
            | Self::Label { reply, .. } => {
                drop(reply.send(Err(failed())));
            }
            Self::Admit { reply, .. } => drop(reply.send(Err(failed()))),
            Self::Sessions { reply } => drop(reply.send(Err(failed()))),
            Self::Labelled { reply } => drop(reply.send(Err(failed()))),
            Self::Hurry => {}
        }
    }

    /// Whether whoever asked is waiting, so that the group is not held
    /// back for more to fill it. Records and states are not waited for,
    /// unless the journal is told that they are.
    const fn is_waited_for(&self) -> bool {
        !matches!(self, Self::Append { .. } | Self::Snapshot { .. })
    }
}

/// Why a request does not go through.
enum Stop {
    /// It is refused, and nothing was written for it. The rest of its
    /// group goes on.
    Refused(JournalError),
    /// The store failed. Nothing of the whole group can be believed.
    Broken(PortError),
}

impl<E: Into<redb::Error>> From<E> for Stop {
    fn from(error: E) -> Self {
        Self::Broken(broke(error))
    }
}

/// The failure of the store, as a session is told it. What may pass, a
/// disk that is full or busy, is worth another try. What the file holds
/// is not.
pub(crate) fn broke(error: impl Into<redb::Error>) -> PortError {
    let error = error.into();
    match error {
        redb::Error::Io(_) | redb::Error::PreviousIo => PortError::transient(format!(
            "the journal's file could not be read or written: {error}"
        )),
        _ => PortError::permanent(format!("the journal failed: {error}")),
    }
}

fn refused<T>(why: JournalError) -> Result<T, Stop> {
    Err(Stop::Refused(why))
}

/// The tables of the file, in a transaction.
struct Tables<'txn> {
    meta: Table<'txn, &'static str, u64>,
    heads: Table<'txn, u128, &'static [u8]>,
    batches: Table<'txn, (u128, u64), &'static [u8]>,
    states: Table<'txn, u128, &'static [u8]>,
    labels: Table<'txn, u128, &'static [u8]>,
    /// Whether anything was written.
    wrote: bool,
    /// The records that were written, for a probe, if there is one.
    records: Option<Vec<Bytes>>,
    /// How many states were written.
    stored: usize,
}

impl<'txn> Tables<'txn> {
    fn open(txn: &'txn WriteTransaction, watched: bool) -> Result<Self, PortError> {
        Ok(Self {
            meta: txn.open_table(META).map_err(broke)?,
            heads: txn.open_table(HEADS).map_err(broke)?,
            batches: txn.open_table(BATCHES).map_err(broke)?,
            states: txn.open_table(STATES).map_err(broke)?,
            labels: txn.open_table(LABELS).map_err(broke)?,
            wrote: false,
            records: watched.then(Vec::new),
            stored: 0,
        })
    }

    fn head(&self, session: SessionId) -> Result<Option<Head>, Stop> {
        let Some(row) = self.heads.get(session.0)? else {
            return Ok(None);
        };
        match Head::read(row.value()) {
            Ok(head) => Ok(Some(head)),
            Err(failure) => refused(JournalError::Store(failure)),
        }
    }

    fn put(&mut self, session: SessionId, head: &Head) -> Result<(), Stop> {
        let _ = self.heads.insert(session.0, head.write().as_slice())?;
        self.wrote = true;
        Ok(())
    }

    /// The head of a session, for its owner alone.
    fn owned(&self, session: SessionId, lease: Lease) -> Result<Head, Stop> {
        match self.head(session)? {
            Some(head) if head.lease == lease => Ok(head),
            // A session nobody has taken has no owner either.
            _ => refused(JournalError::Fenced),
        }
    }

    fn acquire(&mut self, session: SessionId) -> Result<Recovered, Stop> {
        let mut head = self.head(session)?.unwrap_or(Head::new());
        // Everything is read before anything is written, so that a
        // session whose rows cannot be read is left as it is.
        let snapshot = match head.state {
            Some(upto) => {
                let Some(row) = self.states.get(session.0)? else {
                    return refused(JournalError::Store(unreadable(
                        "the state of a session is not there",
                    )));
                };
                Some(Snapshot {
                    upto: Seq(upto),
                    state: Bytes::copy_from_slice(row.value()),
                })
            }
            None => None,
        };
        // The records after the state. A group that the state covers
        // only in part is still there whole.
        let after = head.state.unwrap_or(0);
        let mut tail = Vec::new();
        let mut follows = after.saturating_add(1);
        let mut begun = false;
        for row in self.batches.range((session.0, 0)..=(session.0, u64::MAX))? {
            let (key, row) = row?;
            let (_, first) = key.value();
            let gap = if begun {
                first != follows
            } else {
                first > follows
            };
            if gap {
                return refused(JournalError::Store(unreadable(
                    "records of a session are missing",
                )));
            }
            begun = true;
            let count = match batch_len(row.value()) {
                Ok(count) => count,
                Err(failure) => return refused(JournalError::Store(failure)),
            };
            let read = read_batch(row.value(), |place, record| {
                if first.saturating_add(place) > after {
                    tail.push(Bytes::copy_from_slice(record));
                }
            });
            if let Err(failure) = read {
                return refused(JournalError::Store(failure));
            }
            follows = first.saturating_add(count);
        }
        if follows != head.next {
            return refused(JournalError::Store(unreadable(
                "the last records of a session are missing",
            )));
        }
        let Some(lease) = head.lease.0.checked_add(1) else {
            return refused(JournalError::Store(PortError::permanent(
                "the session has been taken over too often",
            )));
        };
        head.lease = Lease(lease);
        self.put(session, &head)?;
        Ok(Recovered {
            lease: head.lease,
            snapshot,
            tail,
            next: Seq(head.next),
        })
    }

    fn append(&mut self, session: SessionId, lease: Lease, batch: &Batch) -> Result<Seq, Stop> {
        let mut head = self.owned(session, lease)?;
        if batch.first.get() != head.next {
            return refused(JournalError::OutOfOrder {
                expected: Seq(head.next),
            });
        }
        let last = Seq(head.next.saturating_sub(1));
        if batch.records.is_empty() {
            return Ok(last);
        }
        let row = match write_batch(&batch.records) {
            Ok(row) => row,
            Err(failure) => return refused(JournalError::Store(failure)),
        };
        let count = u64::try_from(batch.records.len()).unwrap_or(u64::MAX);
        let Some(next) = head.next.checked_add(count) else {
            return refused(JournalError::Store(PortError::permanent(
                "the session has no numbers left for its records",
            )));
        };
        let _ = self
            .batches
            .insert((session.0, head.next), row.as_slice())?;
        head.next = next;
        self.put(session, &head)?;
        if let Some(records) = &mut self.records {
            records.extend(batch.records.iter().cloned());
        }
        Ok(Seq(next - 1))
    }

    fn snapshot(
        &mut self,
        session: SessionId,
        lease: Lease,
        snapshot: &Snapshot,
    ) -> Result<(), Stop> {
        let mut head = self.owned(session, lease)?;
        let upto = snapshot.upto.get();
        // A state as of a record that is not stored, or an older one
        // than the state that is held.
        if upto >= head.next || head.state.is_some_and(|held| upto < held) {
            return refused(JournalError::OutOfOrder {
                expected: Seq(head.next),
            });
        }
        let _ = self.states.insert(session.0, snapshot.state.as_ref())?;
        // The groups the state covers whole are let go of. One that
        // cannot be read is kept: it is not this request that finds out
        // what is wrong with it.
        let covered = (session.0, 0)..=(session.0, upto);
        self.batches.retain_in(covered, |(_, first), row| {
            batch_len(row).map_or(true, |count| {
                first.saturating_add(count) > upto.saturating_add(1)
            })
        })?;
        head.state = Some(upto);
        self.put(session, &head)?;
        self.stored += 1;
        Ok(())
    }

    fn issue(&mut self, session: SessionId, hash: [u8; 32]) -> Result<(), Stop> {
        let mut head = self.head(session)?.unwrap_or(Head::new());
        head.token = Some(hash);
        self.put(session, &head)
    }

    fn admit(&self, session: SessionId, hash: &[u8; 32]) -> Result<bool, Stop> {
        let held = self.head(session)?.and_then(|head| head.token);
        Ok(held.is_some_and(|held| same(&held, hash)))
    }

    fn label(&mut self, session: SessionId, label: &[u8]) -> Result<(), Stop> {
        if label.len() > LABEL_MOST {
            return refused(JournalError::Store(PortError::permanent(format!(
                "a label of {} bytes is longer than the {LABEL_MOST} a session is kept with",
                label.len()
            ))));
        }
        let _ = self.labels.insert(session.0, label)?;
        self.wrote = true;
        Ok(())
    }

    fn labelled(&self) -> Result<Vec<Labelled>, Stop> {
        let mut sessions = Vec::new();
        for row in self.heads.iter()? {
            let (session, _) = row?;
            let session = session.value();
            let label = self
                .labels
                .get(session)?
                .map(|label| label.value().to_vec());
            sessions.push((SessionId(session), label));
        }
        Ok(sessions)
    }

    fn forget(&mut self, session: SessionId) -> Result<(), Stop> {
        let mut held = self.heads.remove(session.0)?.is_some();
        held |= self.states.remove(session.0)?.is_some();
        held |= self.labels.remove(session.0)?.is_some();
        let all = (session.0, 0)..=(session.0, u64::MAX);
        self.batches.retain_in(all, |_, _| {
            held = true;
            false
        })?;
        self.wrote |= held;
        Ok(())
    }

    /// Closes the tables, which has to be done before their transaction
    /// ends, and says what was done in them. If anything was written,
    /// the count of commits is moved on first.
    fn close(mut self) -> Result<Worked, PortError> {
        let mut commit = None;
        if self.wrote {
            let count = self.meta.get(COMMITS_KEY).map_err(broke)?;
            let count = count.map_or(0, |count| count.value()).saturating_add(1);
            let _ = self.meta.insert(COMMITS_KEY, count).map_err(broke)?;
            commit = Some(count);
        }
        Ok(Worked {
            commit,
            records: self.records.unwrap_or_default(),
            stored: self.stored,
        })
    }

    fn sessions(&self) -> Result<Vec<SessionId>, Stop> {
        let mut sessions = Vec::new();
        for row in self.heads.iter()? {
            let (session, _) = row?;
            sessions.push(SessionId(session.value()));
        }
        Ok(sessions)
    }

    /// Works a request out. `Err` is a failure of the store, with the
    /// request it met.
    fn work(&mut self, request: Request) -> Result<Option<Answer>, Box<(PortError, Request)>> {
        /// What a request comes to: its answer, or the failure of the
        /// store.
        fn settle<T>(done: Result<T, Stop>) -> Result<Result<T, JournalError>, PortError> {
            match done {
                Ok(done) => Ok(Ok(done)),
                Err(Stop::Refused(why)) => Ok(Err(why)),
                Err(Stop::Broken(failure)) => Err(failure),
            }
        }
        // The request is taken apart only once it is known to have been
        // worked out, so that one the store failed at can still be
        // answered.
        let answer = match &request {
            Request::Acquire { session, .. } => {
                settle(self.acquire(*session)).map(Outcome::Acquired)
            }
            Request::Append {
                session,
                lease,
                batch,
                ..
            } => settle(self.append(*session, *lease, batch)).map(Outcome::Appended),
            Request::Snapshot {
                session,
                lease,
                snapshot,
                ..
            } => settle(self.snapshot(*session, *lease, snapshot)).map(Outcome::Done),
            Request::Issue { session, hash, .. } => {
                settle(self.issue(*session, *hash)).map(Outcome::Done)
            }
            Request::Admit { session, hash, .. } => {
                settle(self.admit(*session, hash)).map(Outcome::Admitted)
            }
            Request::Forget { session, .. } => settle(self.forget(*session)).map(Outcome::Done),
            Request::Sessions { .. } => settle(self.sessions()).map(Outcome::Sessions),
            Request::Label { session, label, .. } => {
                settle(self.label(*session, label)).map(Outcome::Done)
            }
            Request::Labelled { .. } => settle(self.labelled()).map(Outcome::Labelled),
            Request::Hurry => return Ok(None),
        };
        let worked = match answer {
            Ok(worked) => worked,
            Err(failure) => return Err(Box::new((failure, request))),
        };
        Ok(match (request, worked) {
            (Request::Acquire { reply, .. }, Outcome::Acquired(answer)) => {
                Some(Answer::Acquired(reply, answer))
            }
            (Request::Append { reply, .. }, Outcome::Appended(answer)) => {
                Some(Answer::Appended(reply, answer))
            }
            (
                Request::Snapshot { reply, .. }
                | Request::Issue { reply, .. }
                | Request::Forget { reply, .. }
                | Request::Label { reply, .. },
                Outcome::Done(answer),
            ) => Some(Answer::Done(reply, answer)),
            (Request::Labelled { reply }, Outcome::Labelled(answer)) => {
                Some(Answer::Labelled(reply, answer))
            }
            (Request::Admit { reply, .. }, Outcome::Admitted(answer)) => {
                Some(Answer::Admitted(reply, answer))
            }
            (Request::Sessions { reply }, Outcome::Sessions(answer)) => {
                Some(Answer::Sessions(reply, answer))
            }
            // Each request was worked out as what it is.
            _ => None,
        })
    }
}

/// What a request was worked out to, before it is put with where its
/// answer goes.
enum Outcome {
    Acquired(Result<Recovered, JournalError>),
    Appended(Result<Seq, JournalError>),
    Done(Result<(), JournalError>),
    Admitted(Result<bool, JournalError>),
    Sessions(Result<Vec<SessionId>, JournalError>),
    Labelled(Result<Vec<Labelled>, JournalError>),
}

/// What a group did in the tables.
struct Worked {
    /// The count of commits the file has once this one is made. `None`
    /// if nothing was written, and there is nothing to commit.
    commit: Option<u64>,
    /// The records that were written, if a probe is to be shown them.
    records: Vec<Bytes>,
    /// How many states were written.
    stored: usize,
}

/// How the writer is to work.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    pub gather: Duration,
    pub two_phase: bool,
}

/// Opens the file again after it failed.
pub(crate) type Reopen = Box<dyn FnMut() -> Result<Database, OpenError> + Send>;

/// The writer: the file, and how to write it.
pub(crate) struct Writer {
    /// The file. `None` after it failed, until it is opened again.
    pub db: Option<Database>,
    /// How to open it again.
    pub reopen: Reopen,
    pub settings: Settings,
    pub probe: Option<Box<dyn Probe>>,
    pub stats: Arc<Mutex<JournalStats>>,
}

/// Makes sure the file is one this build can go on with: of no later
/// layout than its own, and with every table there.
pub(crate) fn prepare(db: &Database) -> Result<(), OpenError> {
    let failed = |error: redb::Error| OpenError::from(error);
    let txn = db.begin_write().map_err(|error| failed(error.into()))?;
    let new = {
        let mut meta = txn.open_table(META).map_err(|error| failed(error.into()))?;
        let held = meta
            .get(FORMAT_KEY)
            .map_err(|error| failed(error.into()))?
            .map(|format| format.value());
        match held {
            Some(format) if format > FORMAT => return Err(OpenError::Newer { format }),
            Some(_) => false,
            None => {
                let _ = meta
                    .insert(FORMAT_KEY, FORMAT)
                    .map_err(|error| failed(error.into()))?;
                true
            }
        }
    };
    if !new {
        return txn.abort().map_err(|error| failed(error.into()));
    }
    Tables::open(&txn, false).map_err(|failure| OpenError::Failed(failure.to_string()))?;
    txn.commit().map_err(|error| failed(error.into()))
}

impl Writer {
    /// Serves requests until nobody can ask any more.
    pub(crate) fn run(mut self, requests: &Receiver<Request>) {
        // When the last commit began.
        let mut last: Option<Instant> = None;
        while let Ok(first) = requests.recv() {
            let mut hurried = first.is_waited_for();
            let mut group = vec![first];
            let mut open = true;
            loop {
                // Everything that is waiting already.
                loop {
                    match requests.try_recv() {
                        Ok(request) => {
                            hurried |= request.is_waited_for();
                            group.push(request);
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            open = false;
                            break;
                        }
                    }
                }
                if hurried || !open {
                    break;
                }
                let wait = last.and_then(|last| {
                    (last + self.settings.gather).checked_duration_since(Instant::now())
                });
                let Some(wait) = wait else {
                    break;
                };
                match requests.recv_timeout(wait) {
                    Ok(request) => {
                        hurried |= request.is_waited_for();
                        group.push(request);
                    }
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            }
            group.retain(|request| !matches!(request, Request::Hurry));
            if group.is_empty() {
                continue;
            }
            last = Some(Instant::now());
            self.commit(group);
        }
    }

    /// The file, opened again if it failed.
    fn database(&mut self) -> Result<&Database, PortError> {
        if self.db.is_none() {
            let db = (self.reopen)().map_err(|error| {
                PortError::transient(format!(
                    "the journal's file cannot be opened again: {error}"
                ))
            })?;
            self.db = Some(db);
        }
        self.db
            .as_ref()
            .ok_or_else(|| PortError::transient("the journal's file is not open"))
    }

    /// Works a group out and commits it, and answers every request of
    /// it.
    fn commit(&mut self, group: Vec<Request>) {
        let asked = group.len();
        let two_phase = self.settings.two_phase;
        let watched = self.probe.is_some();
        let begun = self
            .database()
            .and_then(|db| db.begin_write().map_err(broke));
        let mut txn = match begun {
            Ok(txn) => txn,
            Err(failure) => return self.failed(&failure, Vec::new(), group),
        };
        txn.set_two_phase_commit(two_phase);

        let mut answers = Vec::with_capacity(asked);
        let mut tables = match Tables::open(&txn, watched) {
            Ok(tables) => tables,
            Err(failure) => return self.failed(&failure, answers, group),
        };
        let mut rest = group.into_iter();
        while let Some(request) = rest.next() {
            match tables.work(request) {
                Ok(answer) => answers.extend(answer),
                Err(met) => {
                    let (failure, request) = *met;
                    drop(tables);
                    let unanswered = core::iter::once(request).chain(rest).collect();
                    return self.failed(&failure, answers, unanswered);
                }
            }
        }
        let worked = match tables.close() {
            Ok(worked) => worked,
            Err(failure) => return self.failed(&failure, answers, Vec::new()),
        };
        let Some(count) = worked.commit else {
            // Nothing to store: every request was a question or was
            // refused.
            let _ = txn.abort();
            answers.into_iter().for_each(Answer::send);
            return;
        };
        let seen = Seen {
            requests: asked,
            records: &worked.records,
            states: worked.stored,
        };
        if let Some(probe) = &mut self.probe {
            probe.committing(&seen);
        }
        let started = Instant::now();
        if let Err(error) = txn.commit() {
            // The file has been opened again by the time this is known.
            let failure = self.in_doubt(count, &broke(error));
            return self.tell_failed(&failure, answers, Vec::new());
        }
        let took = started.elapsed();
        if let Some(probe) = &mut self.probe {
            probe.committed(&seen, took);
        }
        {
            let mut stats = self.stats.lock().unwrap_or_else(PoisonError::into_inner);
            stats.commits += 1;
            stats.requests += asked as u64;
            stats.largest = stats.largest.max(asked);
            stats.slowest = stats.slowest.max(took);
        }
        answers.into_iter().for_each(Answer::send);
    }

    /// Finds out what became of a commit that failed, which was to be
    /// the `count`th of the file, and gives the failure its requests are
    /// answered with.
    fn in_doubt(&mut self, count: u64, failure: &PortError) -> PortError {
        // Opened again, the file is what it last committed.
        self.db = None;
        let held = self.database().and_then(|db| {
            let txn = db.begin_write().map_err(broke)?;
            let held = {
                let meta = txn.open_table(META).map_err(broke)?;
                let held = meta.get(COMMITS_KEY).map_err(broke)?;
                held.map_or(0, |held| held.value())
            };
            txn.abort().map_err(broke)?;
            Ok(held)
        });
        match held {
            // The file does not have it. Nothing was stored, and what
            // was asked may be asked again.
            Ok(held) if held < count => failure.clone(),
            Ok(_) => PortError::permanent(format!(
                "the journal wrote what it was asked to store and could not make it safe:                  {failure}"
            )),
            Err(unread) => PortError::permanent(format!(
                "the journal failed, and what became of what it was asked to store cannot be                  found out: {failure}; {unread}"
            )),
        }
    }

    /// Answers every request of a group that could not be committed with
    /// the failure. A file that failed is let go of, to be opened again
    /// for the next group: that is what brings it back to what it last
    /// committed.
    fn failed(&mut self, failure: &PortError, answers: Vec<Answer>, unanswered: Vec<Request>) {
        self.db = None;
        self.tell_failed(failure, answers, unanswered);
    }

    /// Answers every request of a group with the failure.
    fn tell_failed(&self, failure: &PortError, answers: Vec<Answer>, unanswered: Vec<Request>) {
        self.stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failed += 1;
        for answer in answers {
            answer.fail(failure);
        }
        for request in unanswered {
            request.fail(failure);
        }
    }
}
