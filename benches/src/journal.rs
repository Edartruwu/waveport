//! Workloads for the journal in a file.
//!
//! Two things are worth knowing about a journal. One is what a session
//! pays to hand its records over, which it does on its own thread for
//! every step that records anything: [`Enqueue`]. The other is how long
//! the records then take to be stored, which is what a tool with an
//! effect waits for before it may be called: [`Committing`]. The second
//! is mostly the disk's doing, so it is measured by the clock and on the
//! machine that asks.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use tokio::runtime::{Builder, Runtime};
use waveport_ports::journal::{Batch, Journal, Lease, Seq, SessionId};
use waveport_store_journal::{Config, LocalJournal, Probe, Seen, Storage};

/// How many records a session hands over at a time here: what a turn
/// with a tool call in it makes in one step.
const RECORDS: u64 = 4;

/// A record about as long as the one a turn of a conversation makes.
#[must_use]
pub fn record(session: SessionId, seq: u64) -> Bytes {
    Bytes::from(format!(
        "{{\"v\":1,\"seq\":{seq},\"fact\":{{\"kind\":\"turn_committed\",\"turn\":{seq},\
         \"who\":1,\"span\":[48000,105600],\"text\":\"session {session} asks what became of \
         the order it placed on Monday, and when it will arrive\",\"addressed\":true}}}}"
    ))
}

/// The records a session hands over in one go, numbered from `first`.
fn batch(session: SessionId, first: u64) -> Batch {
    Batch {
        first: Seq(first),
        records: (first..first + RECORDS)
            .map(|seq| record(session, seq))
            .collect(),
    }
}

/// Bytes in memory where a journal's file would be: a journal on them
/// does everything but wait for a disk.
#[derive(Clone, Debug, Default)]
pub struct Memory(Arc<Mutex<Vec<u8>>>);

impl Memory {
    fn bytes(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where `len` bytes from `offset` are in a file of `size` bytes, if
/// they are in it.
fn span(offset: u64, len: usize, size: usize) -> std::io::Result<core::ops::Range<usize>> {
    let outside = || std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "outside the file");
    let start = usize::try_from(offset).map_err(|_| outside())?;
    let end = start.checked_add(len).ok_or_else(outside)?;
    if end > size {
        return Err(outside());
    }
    Ok(start..end)
}

impl Storage for Memory {
    fn size(&self) -> std::io::Result<u64> {
        Ok(self.bytes().len() as u64)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        let bytes = self.bytes();
        let from = span(offset, out.len(), bytes.len())?;
        out.copy_from_slice(bytes.get(from).unwrap_or_default());
        Ok(())
    }

    fn set_len(&self, len: u64) -> std::io::Result<()> {
        let len = usize::try_from(len).map_err(std::io::Error::other)?;
        self.bytes().resize(len, 0);
        Ok(())
    }

    fn sync(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        let mut bytes = self.bytes();
        let to = span(offset, data.len(), bytes.len())?;
        if let Some(room) = bytes.get_mut(to) {
            room.copy_from_slice(data);
        }
        Ok(())
    }
}

/// Holds one commit of a journal where it is, until it is let go.
struct Hold {
    /// Whether the next commit is the one to hold.
    armed: Arc<AtomicBool>,
    held: Sender<()>,
    go: Receiver<()>,
}

impl Probe for Hold {
    fn committing(&mut self, _seen: &Seen<'_>) {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.held.send(());
            // Until it is told to go on, or nobody is left to tell it.
            let _ = self.go.recv();
        }
    }
}

/// Something that runs a future to its end on this thread.
fn runtime() -> Option<Runtime> {
    Builder::new_current_thread().build().ok()
}

/// A session that is about to hand its journal the records of a step,
/// while the journal is in the middle of a commit. The thread that
/// writes the journal is then busy and stays so: what the session pays
/// is all that happens when the records are handed over.
pub struct Enqueue {
    journal: LocalJournal,
    session: SessionId,
    lease: Lease,
    /// The records to hand over.
    next: Option<Batch>,
    /// Lets the commit that is held go on.
    go: Sender<()>,
}

impl core::fmt::Debug for Enqueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Enqueue").finish_non_exhaustive()
    }
}

impl Enqueue {
    /// A journal in memory with a session that has written to it for a
    /// while, and whose writer is held in a commit.
    ///
    /// # Panics
    ///
    /// The journal cannot be set up, which is a fault of the benchmark.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a benchmark that cannot be set up has nothing to measure"
    )]
    pub fn prepare() -> Box<Self> {
        let armed = Arc::new(AtomicBool::new(false));
        let (held, is_held) = mpsc::channel();
        let (go, may_go) = mpsc::channel();
        let probe = Hold {
            armed: Arc::clone(&armed),
            held,
            go: may_go,
        };
        let config = Config {
            probe: Some(Box::new(probe)),
            ..Config::default()
        };
        let journal = LocalJournal::on(Memory::default(), config).expect("a journal in memory");
        let runtime = runtime().expect("a runtime");
        let session = SessionId(7);
        let lease = runtime
            .block_on(journal.acquire(session))
            .expect("a session of its own")
            .lease;
        let mut first = 1;
        for _ in 0..8 {
            journal.hurry(session);
            runtime
                .block_on(journal.append(session, lease, batch(session, first)))
                .expect("records stored");
            first += RECORDS;
        }
        // The commit that is held: of records nobody waits for here.
        armed.store(true, Ordering::SeqCst);
        drop(journal.append(session, lease, batch(session, first)));
        journal.hurry(session);
        is_held.recv().expect("a commit under way");
        first += RECORDS;
        Box::new(Self {
            next: Some(batch(session, first)),
            journal,
            session,
            lease,
            go,
        })
    }

    /// Hands the records over, which is what is measured. Nothing waits
    /// for them to be stored.
    pub fn hand_over(&mut self) {
        if let Some(batch) = self.next.take() {
            drop(self.journal.append(self.session, self.lease, batch));
        }
    }
}

impl Drop for Enqueue {
    fn drop(&mut self) {
        // The journal waits for its writer when it is dropped, and the
        // writer waits for this.
        let _ = self.go.send(());
    }
}

/// Sessions that each hand their journal the records of a step at the
/// same time, and wait until they are stored: one commit of the journal.
pub struct Committing {
    runtime: Runtime,
    journal: LocalJournal,
    /// Each session, its lease and the number of its next record.
    sessions: Vec<(SessionId, Lease, u64)>,
}

impl core::fmt::Debug for Committing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Committing")
            .field("sessions", &self.sessions.len())
            .finish_non_exhaustive()
    }
}

impl Committing {
    /// `sessions` sessions on a journal in the file at `path`, which is
    /// made anew. `two_phase` is as [`Config::two_phase`].
    ///
    /// # Panics
    ///
    /// The journal cannot be set up.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a benchmark that cannot be set up has nothing to measure"
    )]
    pub fn in_file(path: &Path, sessions: u32, two_phase: bool) -> Self {
        let _ = std::fs::remove_file(path);
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder).expect("a folder for the journal");
        }
        let config = Config {
            two_phase,
            ..Config::default()
        };
        let journal = LocalJournal::open(path, config).expect("a journal in a file");
        Self::on(journal, sessions)
    }

    /// The same on a journal in memory: what a commit costs without the
    /// disk.
    ///
    /// # Panics
    ///
    /// The journal cannot be set up.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a benchmark that cannot be set up has nothing to measure"
    )]
    pub fn in_memory(sessions: u32) -> Self {
        let journal =
            LocalJournal::on(Memory::default(), Config::default()).expect("a journal in memory");
        Self::on(journal, sessions)
    }

    #[expect(
        clippy::expect_used,
        reason = "a benchmark that cannot be set up has nothing to measure"
    )]
    fn on(journal: LocalJournal, sessions: u32) -> Self {
        let runtime = runtime().expect("a runtime");
        let sessions = (1..=u128::from(sessions))
            .map(|name| {
                let session = SessionId(name);
                let taken = runtime
                    .block_on(journal.acquire(session))
                    .expect("a session of its own");
                (session, taken.lease, 1)
            })
            .collect();
        Self {
            runtime,
            journal,
            sessions,
        }
    }

    /// Every session hands its records over and waits for them. Returns
    /// how many were stored.
    ///
    /// # Panics
    ///
    /// The journal refuses or fails, which a benchmark has no use for.
    #[expect(clippy::expect_used, reason = "a journal that fails is not measured")]
    pub fn round(&mut self) -> u64 {
        let mut waiting = Vec::with_capacity(self.sessions.len());
        for (session, lease, next) in &mut self.sessions {
            waiting.push(
                self.journal
                    .append(*session, *lease, batch(*session, *next)),
            );
            *next += RECORDS;
        }
        // Someone waits for every one of them, as a tool with an effect
        // would: the journal does not wait for more to store with them.
        if let Some((session, ..)) = self.sessions.first() {
            self.journal.hurry(*session);
        }
        let mut stored = 0;
        for appended in waiting {
            self.runtime.block_on(appended).expect("records stored");
            stored += RECORDS;
        }
        stored
    }

    /// How many commits the journal has made.
    #[must_use]
    pub fn commits(&self) -> u64 {
        self.journal.stats().commits
    }
}

#[cfg(test)]
mod tests {
    use super::{Committing, Enqueue};

    #[test]
    fn records_are_handed_over_while_a_commit_is_under_way() {
        let mut enqueue = Enqueue::prepare();
        assert!(enqueue.next.is_some());
        enqueue.hand_over();
        assert!(enqueue.next.is_none());
        // Letting go of it lets the commit go on, and the journal end.
        drop(enqueue);
    }

    #[test]
    fn sessions_that_write_at_once_are_stored_in_one_commit() {
        let mut committing = Committing::in_memory(16);
        let before = committing.commits();
        assert_eq!(committing.round(), 64);
        assert_eq!(committing.round(), 64);
        // Two rounds, and not a commit for each session of each.
        let commits = committing.commits() - before;
        assert!((2..=4).contains(&commits), "{commits} commits");
    }

    #[test]
    fn a_journal_in_a_file_is_measured_as_one_in_memory_is() {
        let path = std::env::temp_dir()
            .join(format!("waveport-benches-{}", std::process::id()))
            .join("journal.redb");
        let mut committing = Committing::in_file(&path, 2, true);
        assert_eq!(committing.round(), 8);
        drop(committing);
        let _ = std::fs::remove_file(&path);
    }
}
