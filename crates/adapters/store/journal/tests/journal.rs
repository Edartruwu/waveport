//! The journal in a file, held to what every journal is held to, and to
//! what only a journal that outlasts its process can be held to.
//!
//! The clock is the real one: the journal writes on a thread of its own,
//! which a clock that a test moves does not move.

#![expect(
    clippy::unwrap_used,
    reason = "a helper that cannot go on has failed the test"
)]

use core::time::Duration;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use waveport_ports::journal::{
    Batch, Journal, JournalError, Lease, Recovered, Seq, SessionId, Snapshot,
};
use waveport_store_journal::{Config, LocalJournal, OpenError, Probe, ResumeToken, Seen};
use waveport_testkit::conformance;

/// How long anything here may take. It all happens on one machine.
const SOON: Duration = Duration::from_secs(10);

/// A place for a test's journal that no other test uses, with nothing
/// there.
fn scratch(test: &str) -> PathBuf {
    let folder = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("journal")
        .join(test);
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).unwrap();
    folder.join("journal.redb")
}

fn open(path: &Path) -> LocalJournal {
    LocalJournal::open(path, Config::default()).unwrap()
}

fn record(n: u64) -> Bytes {
    Bytes::from(format!("record {n}").into_bytes())
}

fn batch(first: u64, count: u64) -> Batch {
    Batch {
        first: Seq(first),
        records: (first..first + count).map(record).collect(),
    }
}

async fn soon<T>(what: impl Future<Output = T>) -> T {
    tokio::time::timeout(SOON, what).await.unwrap()
}

#[tokio::test]
async fn it_keeps_the_rules_of_a_journal() {
    let journal = open(&scratch("rules"));
    conformance::journal_session(&journal, SessionId(7))
        .await
        .unwrap();
    // Another session in the same journal is another session.
    conformance::journal_session(&journal, SessionId(8))
        .await
        .unwrap();
    assert_eq!(
        soon(journal.sessions()).await.unwrap(),
        [SessionId(7), SessionId(8)]
    );
}

#[tokio::test]
async fn it_keeps_the_rules_in_two_phases_as_well() {
    let config = Config {
        two_phase: true,
        ..Config::default()
    };
    let journal = LocalJournal::open(scratch("two-phases"), config).unwrap();
    conformance::journal_session(&journal, SessionId(7))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_session_nobody_has_begun_cannot_be_written_to() {
    let journal = open(&scratch("nobody"));
    assert_eq!(
        soon(journal.append(SessionId(1), Lease(1), batch(1, 1))).await,
        Err(JournalError::Fenced)
    );
    let state = Snapshot {
        upto: Seq(0),
        state: Bytes::from_static(b"nothing yet"),
    };
    assert_eq!(
        soon(journal.snapshot(SessionId(1), Lease(1), state)).await,
        Err(JournalError::Fenced)
    );
    // Nothing was written, and nothing is held of it.
    assert_eq!(soon(journal.sessions()).await.unwrap(), []);
    assert_eq!(journal.stats().commits, 0);
}

#[tokio::test]
async fn what_was_stored_is_there_for_the_next_process() {
    let path = scratch("next-process");
    let session = SessionId(0x5E55);
    let token;
    {
        let journal = open(&path);
        assert_eq!(journal.path(), Some(path.as_path()));
        token = soon(journal.issue(session)).await.unwrap();
        let first = soon(journal.acquire(session)).await.unwrap();
        assert!(first.is_new());
        assert_eq!(first.lease, Lease(1));
        let stored = soon(journal.append(session, first.lease, batch(1, 5))).await;
        assert_eq!(stored, Ok(Seq(5)));
        let state = Snapshot {
            upto: Seq(3),
            state: Bytes::from_static(b"as of record 3"),
        };
        soon(journal.snapshot(session, first.lease, state))
            .await
            .unwrap();
        let stored = soon(journal.append(session, first.lease, batch(6, 2))).await;
        assert_eq!(stored, Ok(Seq(7)));
        // The process ends here. Nothing says goodbye to the journal.
    }

    let journal = open(&path);
    assert_eq!(soon(journal.sessions()).await.unwrap(), [session]);
    let again = soon(journal.acquire(session)).await.unwrap();
    assert_eq!(
        again,
        Recovered {
            lease: Lease(2),
            snapshot: Some(Snapshot {
                upto: Seq(3),
                state: Bytes::from_static(b"as of record 3"),
            }),
            tail: (4..=7).map(record).collect(),
            next: Seq(8),
        }
    );
    // The owner of the earlier life is shut out, and the new one goes on
    // where the records stop.
    assert_eq!(
        soon(journal.append(session, Lease(1), batch(8, 1))).await,
        Err(JournalError::Fenced)
    );
    assert_eq!(
        soon(journal.append(session, again.lease, batch(8, 1))).await,
        Ok(Seq(8))
    );

    // The token that was issued still lets its holder in, and no other
    // does.
    assert_eq!(soon(journal.admit(&token)).await, Ok(true));
    let written = token.to_string();
    let flipped = if written.ends_with('0') { "1" } else { "0" };
    let near: ResumeToken = format!("{}{flipped}", &written[..written.len() - 1])
        .parse()
        .unwrap();
    assert_eq!(soon(journal.admit(&near)).await, Ok(false));
}

#[tokio::test]
async fn a_state_replaces_the_records_it_covers_and_only_those() {
    let path = scratch("covers");
    let session = SessionId(3);
    let journal = open(&path);
    let lease = soon(journal.acquire(session)).await.unwrap().lease;
    // Three groups: records 1 to 4, 5 to 8 and 9 to 10.
    for (first, count) in [(1, 4), (5, 4), (9, 2)] {
        soon(journal.append(session, lease, batch(first, count)))
            .await
            .unwrap();
    }
    let state = |upto: u64| Snapshot {
        upto: Seq(upto),
        state: Bytes::from(format!("as of {upto}").into_bytes()),
    };
    // As of record 6, in the middle of the second group. The first
    // group is let go of. The second is kept whole, and a new owner is
    // given only what follows the state.
    soon(journal.snapshot(session, lease, state(6)))
        .await
        .unwrap();
    let taken = soon(journal.acquire(session)).await.unwrap();
    assert_eq!(taken.snapshot, Some(state(6)));
    assert_eq!(taken.tail, (7..=10).map(record).collect::<Vec<_>>());
    assert_eq!(taken.next, Seq(11));
    let lease = taken.lease;

    // An older state than the one held is not taken in its place, nor
    // one as of a record that is not there.
    for wrong in [4, 11, 99] {
        assert_eq!(
            soon(journal.snapshot(session, lease, state(wrong))).await,
            Err(JournalError::OutOfOrder { expected: Seq(11) }),
            "as of {wrong}"
        );
    }
    // The state as of the last record leaves no record to keep.
    soon(journal.snapshot(session, lease, state(10)))
        .await
        .unwrap();
    drop(journal);
    let journal = open(&path);
    let taken = soon(journal.acquire(session)).await.unwrap();
    assert_eq!(taken.snapshot, Some(state(10)));
    assert_eq!(taken.tail, Vec::<Bytes>::new());
    assert_eq!(taken.next, Seq(11));
}

#[tokio::test]
async fn a_session_that_is_forgotten_is_a_new_one() {
    let path = scratch("forgotten");
    let journal = open(&path);
    let (kept, gone) = (SessionId(1), SessionId(2));
    let mut tokens = Vec::new();
    for session in [kept, gone] {
        let lease = soon(journal.acquire(session)).await.unwrap().lease;
        soon(journal.append(session, lease, batch(1, 6)))
            .await
            .unwrap();
        let state = Snapshot {
            upto: Seq(2),
            state: Bytes::from_static(b"a state"),
        };
        soon(journal.snapshot(session, lease, state)).await.unwrap();
        tokens.push(soon(journal.issue(session)).await.unwrap());
    }
    soon(journal.forget(gone)).await.unwrap();
    // Forgetting what is not there is nothing.
    soon(journal.forget(SessionId(99))).await.unwrap();
    drop(journal);

    let journal = open(&path);
    assert_eq!(soon(journal.sessions()).await.unwrap(), [kept]);
    assert_eq!(soon(journal.admit(&tokens[0])).await, Ok(true));
    assert_eq!(soon(journal.admit(&tokens[1])).await, Ok(false));
    let fresh = soon(journal.acquire(gone)).await.unwrap();
    assert!(fresh.is_new());
    assert_eq!((fresh.lease, fresh.next), (Lease(1), Seq(1)));
    // The other session is as it was.
    let other = soon(journal.acquire(kept)).await.unwrap();
    assert_eq!(other.tail.len(), 4);
    assert_eq!(other.next, Seq(7));
}

#[tokio::test]
async fn a_new_token_takes_the_place_of_the_one_before() {
    let journal = open(&scratch("tokens"));
    let session = SessionId(5);
    // A token can be issued for a session before anything else is
    // stored of it, and the session is still new to whoever takes it.
    let first = soon(journal.issue(session)).await.unwrap();
    assert_eq!(first.session(), session);
    assert!(soon(journal.acquire(session)).await.unwrap().is_new());
    assert_eq!(soon(journal.admit(&first)).await, Ok(true));

    let second = soon(journal.issue(session)).await.unwrap();
    assert_eq!(soon(journal.admit(&first)).await, Ok(false));
    assert_eq!(soon(journal.admit(&second)).await, Ok(true));
    // A token for a session the journal has never heard of.
    let stranger: ResumeToken = format!("wpr1_{:064x}", 9).parse().unwrap();
    assert_eq!(soon(journal.admit(&stranger)).await, Ok(false));
}

#[tokio::test]
async fn one_process_has_the_file_at_a_time() {
    let path = scratch("in-use");
    let journal = open(&path);
    let second = LocalJournal::open(&path, Config::default()).unwrap_err();
    assert!(matches!(second, OpenError::InUse), "{second}");
    assert_eq!(
        second.to_string(),
        "the journal is in use by another process"
    );
    // The one that has it is not disturbed, and when it is gone the
    // file is free.
    let lease = soon(journal.acquire(SessionId(1))).await.unwrap().lease;
    soon(journal.append(SessionId(1), lease, batch(1, 1)))
        .await
        .unwrap();
    drop(journal);
    let journal = open(&path);
    assert_eq!(
        soon(journal.acquire(SessionId(1))).await.unwrap().next,
        Seq(2)
    );
}

#[tokio::test]
async fn a_file_that_is_not_a_journal_is_not_opened() {
    // No directory to make the file in.
    let nowhere = scratch("nowhere")
        .with_file_name("missing")
        .join("journal.redb");
    let refused = LocalJournal::open(&nowhere, Config::default()).unwrap_err();
    assert!(matches!(refused, OpenError::Io(_)), "{refused}");

    // Some other file.
    let path = scratch("other-file");
    std::fs::write(
        &path,
        "This is a text file, and not a journal.\n".repeat(400),
    )
    .unwrap();
    let refused = LocalJournal::open(&path, Config::default()).unwrap_err();
    assert!(matches!(refused, OpenError::Corrupt(_)), "{refused}");
    // It is left as it was.
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("This is a text file")
    );
}

/// What a probe saw of every commit: how many requests it was made of,
/// the records in it, and how many states.
type Commits = Arc<Mutex<Vec<(usize, Vec<Bytes>, usize)>>>;

/// A probe that can hold a commit where it is, and that keeps what
/// every commit held.
struct Gate {
    /// Whether the next commit is to be held.
    armed: Arc<AtomicBool>,
    held: Sender<()>,
    release: Receiver<()>,
    seen: Commits,
}

/// The test's end of a [`Gate`].
struct Keeper {
    armed: Arc<AtomicBool>,
    held: Receiver<()>,
    release: Sender<()>,
    seen: Commits,
}

fn gate() -> (Gate, Keeper) {
    let armed = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let gate = Gate {
        armed: Arc::clone(&armed),
        held: held_tx,
        release: release_rx,
        seen: Arc::clone(&seen),
    };
    let keeper = Keeper {
        armed,
        held: held_rx,
        release: release_tx,
        seen,
    };
    (gate, keeper)
}

impl Probe for Gate {
    fn committing(&mut self, seen: &Seen<'_>) {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.held.send(());
            let _ = self.release.recv_timeout(SOON);
        }
        let _ = seen;
    }

    fn committed(&mut self, seen: &Seen<'_>, _took: Duration) {
        self.seen
            .lock()
            .unwrap()
            .push((seen.requests, seen.records.to_vec(), seen.states));
    }
}

impl Keeper {
    /// Has the next commit held where it is, before anything of it is
    /// stored.
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Waits until a commit is held.
    fn wait(&self) {
        self.held.recv_timeout(SOON).unwrap();
    }

    fn release(&self) {
        self.release.send(()).unwrap();
    }

    fn seen(&self) -> Vec<(usize, Vec<Bytes>, usize)> {
        self.seen.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn what_is_asked_while_a_commit_is_made_is_stored_with_one_sync() {
    let (gate, keeper) = gate();
    let config = Config {
        probe: Some(Box::new(gate)),
        ..Config::default()
    };
    let journal = LocalJournal::open(scratch("groups"), config).unwrap();
    let sessions: Vec<SessionId> = (1..=20).map(SessionId).collect();
    let mut leases = Vec::new();
    for session in &sessions {
        leases.push(soon(journal.acquire(*session)).await.unwrap().lease);
    }
    let before = journal.stats();
    assert_eq!(before.commits, 20, "an owner is not kept waiting");

    // One session's records are being committed, and are held there.
    keeper.arm();
    let first = journal.append(sessions[0], leases[0], batch(1, 2));
    keeper.wait();
    // Meanwhile the other nineteen ask, each for three records, and one
    // of them for a state as well.
    let mut rest = Vec::new();
    for (session, lease) in sessions.iter().zip(&leases).skip(1) {
        rest.push(journal.append(*session, *lease, batch(1, 3)));
    }
    let state = Snapshot {
        upto: Seq(2),
        state: Bytes::from_static(b"as of record 2"),
    };
    let kept = journal.snapshot(sessions[1], leases[1], state);
    keeper.release();

    assert_eq!(soon(first).await, Ok(Seq(2)));
    for stored in rest {
        assert_eq!(soon(stored).await, Ok(Seq(3)));
    }
    soon(kept).await.unwrap();

    // Two commits: the one that was held, and one for everything that
    // was asked while it was.
    let after = journal.stats();
    assert_eq!(after.commits - before.commits, 2);
    assert_eq!(after.requests - before.requests, 21);
    assert_eq!(after.largest, 20);
    assert_eq!(after.failed, 0);
    let seen = keeper.seen();
    let (requests, records, states) = &seen[seen.len() - 1];
    assert_eq!((*requests, records.len(), *states), (20, 57, 1));
    assert_eq!(records[..3], [record(1), record(2), record(3)]);
    let (requests, records, states) = &seen[seen.len() - 2];
    assert_eq!((*requests, records.len(), *states), (1, 2, 0));
}

#[tokio::test]
async fn records_wait_for_more_to_be_stored_with_unless_someone_is_held_up() {
    // A journal that would wait two seconds for a commit to fill.
    let config = Config {
        gather: Duration::from_secs(2),
        ..Config::default()
    };
    let journal = LocalJournal::open(scratch("hurry"), config).unwrap();
    let session = SessionId(1);
    let began = std::time::Instant::now();
    // Whoever takes a session over is waited for by a caller, and is
    // not kept waiting. Nor is a token.
    let lease = soon(journal.acquire(session)).await.unwrap().lease;
    soon(journal.issue(session)).await.unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );

    // Records are. These come right after a commit, and wait for the
    // rest of the two seconds.
    let mut storing = journal.append(session, lease, batch(1, 1));
    let waited = tokio::time::timeout(Duration::from_millis(150), &mut storing).await;
    assert!(waited.is_err(), "stored at once: {waited:?}");
    // Until the session says that it is held up.
    journal.hurry(session);
    assert_eq!(soon(&mut storing).await, Ok(Seq(1)));
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn a_journal_that_is_let_go_of_stores_what_it_was_asked_first() {
    // Also one that would wait two seconds for a commit to fill: with
    // nobody left to ask, nothing more is coming.
    let path = scratch("let-go");
    let config = Config {
        gather: Duration::from_secs(2),
        ..Config::default()
    };
    let journal = LocalJournal::open(&path, config).unwrap();
    let lease = soon(journal.acquire(SessionId(1))).await.unwrap().lease;
    let began = std::time::Instant::now();
    // Nobody waits for the answer, and the records are stored all the
    // same: they were on their way when they were asked for.
    drop(journal.append(SessionId(1), lease, batch(1, 3)));
    drop(journal);
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );

    let journal = open(&path);
    assert_eq!(
        soon(journal.acquire(SessionId(1))).await.unwrap().next,
        Seq(4)
    );
}

#[tokio::test]
async fn a_file_from_a_later_build_is_not_opened() {
    let path = scratch("later");
    drop(open(&path));
    // A later build has been at the file, and says so where every build
    // looks first.
    {
        let db = redb::Database::create(&path).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let meta: redb::TableDefinition<&str, u64> = redb::TableDefinition::new("meta");
            let mut meta = txn.open_table(meta).unwrap();
            meta.insert("format", 2).unwrap();
        }
        txn.commit().unwrap();
    }
    let refused = LocalJournal::open(&path, Config::default()).unwrap_err();
    assert!(
        matches!(refused, OpenError::Newer { format: 2 }),
        "{refused}"
    );
    assert_eq!(
        refused.to_string(),
        "the journal's file is in version 2 of its layout, and this build reads up to version 1"
    );
}

#[tokio::test]
async fn many_sessions_write_at_once_and_each_finds_its_own() {
    let path = scratch("many");
    let journal = Arc::new(open(&path));
    let mut writers = Vec::new();
    for name in 1..=32_u128 {
        let journal = Arc::clone(&journal);
        writers.push(tokio::spawn(async move {
            let session = SessionId(name);
            let lease = journal.acquire(session).await.unwrap().lease;
            let mut next = 1;
            for round in 0..12_u64 {
                let count = 1 + (round + u64::try_from(name).unwrap()) % 4;
                let records = (next..next + count)
                    .map(|n| Bytes::from(format!("session {name} record {n}").into_bytes()))
                    .collect();
                let stored = Batch {
                    first: Seq(next),
                    records,
                };
                next += count;
                assert_eq!(
                    journal.append(session, lease, stored).await,
                    Ok(Seq(next - 1))
                );
                if round == 6 {
                    let state = Snapshot {
                        upto: Seq(next - 2),
                        state: Bytes::from(format!("session {name}").into_bytes()),
                    };
                    journal.snapshot(session, lease, state).await.unwrap();
                }
            }
            next
        }));
    }
    let mut nexts = Vec::new();
    for writer in writers {
        nexts.push(soon(writer).await.unwrap());
    }
    let stats = journal.stats();
    assert_eq!(stats.failed, 0);
    // 32 owners, 384 groups of records and 32 states, in fewer commits
    // than requests: they were stored together.
    assert_eq!(stats.requests, 32 + 384 + 32);
    assert!(stats.commits < stats.requests, "{stats:?}");
    drop(Arc::into_inner(journal).unwrap());

    let journal = open(&path);
    for (name, next) in (1..=32_u128).zip(nexts) {
        let found = soon(journal.acquire(SessionId(name))).await.unwrap();
        assert_eq!(found.next, Seq(next));
        let kept = found.snapshot.unwrap();
        assert_eq!(kept.state, format!("session {name}").as_bytes());
        let mut number = kept.upto.get();
        for record in found.tail {
            number += 1;
            assert_eq!(record, format!("session {name} record {number}").as_bytes());
        }
        assert_eq!(number + 1, next);
    }
}
