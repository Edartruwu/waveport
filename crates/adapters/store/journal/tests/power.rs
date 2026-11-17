//! The journal on storage that fails: a write or a sync that is refused,
//! and a machine that loses power in the middle of a commit and keeps
//! only part of what was written since the last sync.
//!
//! The storage is in memory and does what it is told to. It keeps two
//! pictures of itself: what the process reads, which is everything it
//! wrote, and what a power cut would leave, which is what had been synced
//! and, of the rest, whatever the seed of the run decides: any write
//! whole, in part or not at all, in any combination.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use waveport_ports::error::RetryClass;
use waveport_ports::journal::{Batch, Journal, JournalError, Lease, Seq, SessionId, Snapshot};
use waveport_sim::{SeedPlan, SimRng, run};
use waveport_store_journal::{Config, LocalJournal, Probe, ResumeToken, Seen, Storage};

/// How long anything here may take.
const SOON: Duration = Duration::from_secs(10);

/// Something written since the last sync.
#[derive(Debug)]
enum Written {
    Bytes { offset: usize, data: Vec<u8> },
    Len(usize),
}

/// What the storage is told to do wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Fault {
    #[default]
    None,
    /// The next write is refused, and writes nothing.
    Write,
    /// The next sync is refused. What was written stays written, and is
    /// not safe.
    Sync,
    /// The power goes at the sync that many syncs from now, 1 being the
    /// next.
    PowerAt(u64),
}

#[derive(Debug, Default)]
struct Platter {
    /// What the process reads.
    seen: Vec<u8>,
    /// What a power cut leaves.
    safe: Vec<u8>,
    unsynced: Vec<Written>,
    fault: Fault,
    /// Whether the power has gone. Nothing works after that.
    dark: bool,
    /// How many times the storage was synced.
    syncs: u64,
    /// Decides what a power cut keeps.
    rng: Option<SimRng>,
}

/// Storage in memory. A clone is the same storage.
#[derive(Clone, Debug, Default)]
struct Disk(Arc<Mutex<Platter>>);

fn dark() -> io::Error {
    io::Error::other("the power is out")
}

fn apply(image: &mut Vec<u8>, written: &Written, part: core::ops::Range<usize>) {
    match written {
        Written::Len(len) => image.resize(*len, 0),
        Written::Bytes { .. } if part.is_empty() => {}
        Written::Bytes { offset, data } => {
            let end = offset + part.end;
            if image.len() < end {
                image.resize(end, 0);
            }
            image[offset + part.start..end].copy_from_slice(&data[part]);
        }
    }
}

impl Disk {
    /// Storage that holds `image`, all of it safe.
    fn holding(image: Vec<u8>) -> Self {
        let disk = Self::default();
        {
            let mut platter = disk.platter();
            platter.safe.clone_from(&image);
            platter.seen = image;
        }
        disk
    }

    fn platter(&self) -> MutexGuard<'_, Platter> {
        self.0.lock().unwrap()
    }

    fn fault(&self, fault: Fault, rng: Option<SimRng>) {
        let mut platter = self.platter();
        platter.fault = fault;
        if rng.is_some() {
            platter.rng = rng;
        }
    }

    /// What is left after the power went: what the next process finds.
    fn left(&self) -> Vec<u8> {
        self.platter().safe.clone()
    }

    fn is_dark(&self) -> bool {
        self.platter().dark
    }

    fn syncs(&self) -> u64 {
        self.platter().syncs
    }
}

impl Platter {
    /// The power goes. Of what was written since the last sync, each
    /// piece is kept whole, kept in part or dead. Two times in five all
    /// of it had reached the disk and only the answer was missing, and
    /// one time in five none of it had.
    fn cut(&mut self) {
        self.dark = true;
        let mut rng = self
            .rng
            .take()
            .unwrap_or_else(|| SimRng::new(waveport_sim::Seed(0)));
        // 0 for none of it, 1 for all of it, 2 for each piece as it
        // comes.
        let all = [0, 1, 1, 2, 2][usize::try_from(rng.range(0..5)).unwrap()];
        for written in core::mem::take(&mut self.unsynced) {
            let len = match &written {
                Written::Bytes { data, .. } => data.len(),
                Written::Len(_) => 0,
            };
            let at = |rng: &mut SimRng| usize::try_from(rng.range(0..len as u64 + 1)).unwrap();
            let how = if all < 2 { all } else { rng.range(0..4) };
            let part = match how {
                0 => continue,
                1 => 0..len,
                2 => 0..at(&mut rng),
                _ => {
                    let (a, b) = (at(&mut rng), at(&mut rng));
                    a.min(b)..a.max(b)
                }
            };
            apply(&mut self.safe, &written, part);
        }
    }
}

impl Storage for Disk {
    fn size(&self) -> io::Result<u64> {
        let platter = self.platter();
        if platter.dark {
            return Err(dark());
        }
        Ok(platter.seen.len() as u64)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let platter = self.platter();
        if platter.dark {
            return Err(dark());
        }
        let offset = usize::try_from(offset).unwrap();
        let Some(held) = platter.seen.get(offset..offset + out.len()) else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "past the end"));
        };
        out.copy_from_slice(held);
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut platter = self.platter();
        if platter.dark {
            return Err(dark());
        }
        let len = usize::try_from(len).unwrap();
        platter.seen.resize(len, 0);
        platter.unsynced.push(Written::Len(len));
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut platter = self.platter();
        if platter.dark {
            return Err(dark());
        }
        match platter.fault {
            Fault::Sync => {
                platter.fault = Fault::None;
                return Err(io::Error::other("the disk did not answer"));
            }
            Fault::PowerAt(1) => {
                platter.cut();
                return Err(dark());
            }
            Fault::PowerAt(more) => platter.fault = Fault::PowerAt(more - 1),
            Fault::None | Fault::Write => {}
        }
        platter.syncs += 1;
        let platter = &mut *platter;
        for written in platter.unsynced.drain(..) {
            let len = match &written {
                Written::Bytes { data, .. } => data.len(),
                Written::Len(_) => 0,
            };
            apply(&mut platter.safe, &written, 0..len);
        }
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut platter = self.platter();
        if platter.dark {
            return Err(dark());
        }
        if platter.fault == Fault::Write {
            platter.fault = Fault::None;
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "the disk is full",
            ));
        }
        let written = Written::Bytes {
            offset: usize::try_from(offset).unwrap(),
            data: data.to_vec(),
        };
        apply(&mut platter.seen, &written, 0..data.len());
        platter.unsynced.push(written);
        Ok(())
    }
}

fn record(session: SessionId, n: u64) -> Bytes {
    Bytes::from(format!("session {} record {n}", session.0).into_bytes())
}

fn batch(session: SessionId, first: u64, count: u64) -> Batch {
    Batch {
        first: Seq(first),
        records: (first..first + count).map(|n| record(session, n)).collect(),
    }
}

async fn soon<T>(what: impl Future<Output = T>) -> T {
    tokio::time::timeout(SOON, what).await.unwrap()
}

fn class_of<T: core::fmt::Debug>(answer: Result<T, JournalError>) -> RetryClass {
    match answer {
        Err(JournalError::Store(failure)) => failure.class,
        other => panic!("not a failure of the store: {other:?}"),
    }
}

#[tokio::test]
async fn a_write_that_fails_stores_nothing_and_may_be_tried_again() {
    let disk = Disk::default();
    let journal = LocalJournal::on(disk.clone(), Config::default()).unwrap();
    let session = SessionId(1);
    let lease = soon(journal.acquire(session)).await.unwrap().lease;
    soon(journal.append(session, lease, batch(session, 1, 2)))
        .await
        .unwrap();

    // The disk is full for one write.
    disk.fault(Fault::Write, None);
    let failed = soon(journal.append(session, lease, batch(session, 3, 4))).await;
    let failure = failed.clone().unwrap_err().to_string();
    assert_eq!(class_of(failed), RetryClass::Transient, "{failure}");
    assert!(failure.contains("the disk is full"), "{failure}");
    assert_eq!(journal.stats().failed, 1);

    // Nothing of it was stored: the same records go in at the next try,
    // under the same numbers, and the session's owner is still its
    // owner.
    assert_eq!(
        soon(journal.append(session, lease, batch(session, 3, 4))).await,
        Ok(Seq(6))
    );
    let found = soon(journal.acquire(session)).await.unwrap();
    assert_eq!(found.lease, Lease(2));
    assert_eq!(
        found.tail,
        (1..=6).map(|n| record(session, n)).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_sync_that_fails_leaves_records_that_nothing_vouches_for() {
    let disk = Disk::default();
    let journal = LocalJournal::on(disk.clone(), Config::default()).unwrap();
    let (session, other) = (SessionId(1), SessionId(2));
    let lease = soon(journal.acquire(session)).await.unwrap().lease;
    let other_lease = soon(journal.acquire(other)).await.unwrap().lease;
    soon(journal.append(session, lease, batch(session, 1, 2)))
        .await
        .unwrap();

    // Everything is written, and the disk does not confirm it.
    disk.fault(Fault::Sync, None);
    let failed = soon(journal.append(session, lease, batch(session, 3, 4))).await;
    let failure = failed.clone().unwrap_err().to_string();
    // The file has the records, so they cannot be given to it again.
    // The session is told that it cannot go on, and why.
    assert_eq!(class_of(failed), RetryClass::Permanent, "{failure}");
    assert!(
        failure.contains("could not make it safe") && failure.contains("did not answer"),
        "{failure}"
    );

    // Whoever goes on with the session finds them, and the journal goes
    // on for everyone.
    let found = soon(journal.acquire(session)).await.unwrap();
    assert_eq!(found.next, Seq(7));
    assert_eq!(found.tail.len(), 6);
    assert_eq!(
        soon(journal.append(other, other_lease, batch(other, 1, 1))).await,
        Ok(Seq(1))
    );
    assert_eq!(
        soon(journal.append(session, found.lease, batch(session, 7, 1))).await,
        Ok(Seq(7))
    );
}

#[tokio::test]
async fn a_journal_whose_storage_is_gone_says_so_to_everyone() {
    let disk = Disk::default();
    let journal = LocalJournal::on(disk.clone(), Config::default()).unwrap();
    let session = SessionId(1);
    let lease = soon(journal.acquire(session)).await.unwrap().lease;
    disk.fault(Fault::PowerAt(1), None);
    let failed = soon(journal.append(session, lease, batch(session, 1, 1))).await;
    assert!(matches!(failed, Err(JournalError::Store(_))), "{failed:?}");
    assert!(disk.is_dark());
    // Every request after that is answered, and none is left waiting.
    for _ in 0..3 {
        let failed = soon(journal.acquire(session)).await;
        assert!(matches!(failed, Err(JournalError::Store(_))), "{failed:?}");
    }
    assert!(soon(journal.sessions()).await.is_err());
    assert!(soon(journal.issue(session)).await.is_err());
    drop(journal);
}

/// A probe that can hold a commit where it is.
struct Gate {
    armed: Arc<AtomicBool>,
    held: Sender<()>,
    release: Receiver<()>,
}

struct Keeper {
    armed: Arc<AtomicBool>,
    held: Receiver<()>,
    release: Sender<()>,
}

fn gate() -> (Gate, Keeper) {
    let armed = Arc::new(AtomicBool::new(false));
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let gate = Gate {
        armed: Arc::clone(&armed),
        held: held_tx,
        release: release_rx,
    };
    let keeper = Keeper {
        armed,
        held: held_rx,
        release: release_tx,
    };
    (gate, keeper)
}

impl Probe for Gate {
    fn committing(&mut self, _seen: &Seen<'_>) {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.held.send(());
            let _ = self.release.recv_timeout(SOON);
        }
    }
}

/// What a test knows of a session: what the journal said was stored, and
/// what it was asked to store and has not answered for.
#[derive(Clone, Debug)]
struct Known {
    session: SessionId,
    lease: Lease,
    /// The number after the last record the journal said was stored.
    next: u64,
    /// The state the journal said was stored.
    state: Option<u64>,
    /// What was asked in the commit the power went in: the number after
    /// the last record of it, and the state.
    asked_next: u64,
    asked_state: Option<u64>,
    /// Whether the session was being taken over in that commit.
    asked_lease: bool,
    /// The label the journal said was kept with the session, by the
    /// round it was given in, and the one that was asked for.
    label: Option<u64>,
    asked_label: Option<u64>,
}

/// The label a session is given in a round.
fn label_of(session: SessionId, round: u64) -> Vec<u8> {
    format!("session {} in round {round}", session.0).into_bytes()
}

fn state_of(session: SessionId, upto: u64) -> Bytes {
    Bytes::from(format!("session {} as of {upto}", session.0).into_bytes())
}

/// The power goes in the middle of a commit. The next process finds
/// every session with everything the journal had said was stored, and
/// the commit that was cut short either whole, for every session of it,
/// or not at all.
#[expect(
    clippy::too_many_lines,
    reason = "one run from the first commit to what the next process finds"
)]
async fn power_goes(seed: u64, mut rng: SimRng) -> Result<Cut, String> {
    const PLUG: SessionId = SessionId(99);
    let check = |holds: bool, what: String| if holds { Ok(()) } else { Err(what) };

    let disk = Disk::default();
    let (gate, keeper) = gate();
    let config = Config {
        gather: Duration::ZERO,
        two_phase: seed.is_multiple_of(5),
        probe: Some(Box::new(gate)),
        ..Config::default()
    };
    let journal = LocalJournal::on(disk.clone(), config).map_err(|error| error.to_string())?;
    let mut known = Vec::new();
    for name in 1..=3_u128 {
        let session = SessionId(name);
        let lease = soon(journal.acquire(session))
            .await
            .map_err(|e| e.to_string())?
            .lease;
        known.push(Known {
            session,
            lease,
            next: 1,
            state: None,
            asked_next: 1,
            asked_state: None,
            asked_lease: false,
            label: None,
            asked_label: None,
        });
    }
    // The token the journal last said it had issued, and whether one
    // was being issued when the power went.
    let mut token: Option<ResumeToken> = None;
    let mut issuing = false;

    // How many times a commit syncs: once, or twice in two phases.
    let before = disk.syncs();
    soon(journal.issue(PLUG)).await.map_err(|e| e.to_string())?;
    let syncs = disk.syncs() - before;
    check(syncs >= 1, "a commit that did not sync".to_owned())?;

    // The power goes after some rounds, at one of the syncs of the
    // commit that holds what the sessions ask, or, one time in four, of
    // the one that is held before it.
    let cut_in = rng.range(0..6);
    for round in 0..40 {
        if round == cut_in {
            let held = if rng.chance(1, 4) { 0 } else { syncs };
            let at = held + rng.range(1..syncs + 1);
            disk.fault(Fault::PowerAt(at), Some(rng.fork("power")));
        }
        // A commit of its own is held where it is, so that everything
        // asked meanwhile is the commit after it, whole.
        keeper.armed.store(true, Ordering::SeqCst);
        let plug = journal.issue(PLUG);
        keeper
            .held
            .recv_timeout(SOON)
            .map_err(|_| "no commit was held".to_owned())?;

        let mut asked = Vec::new();
        for knows in &mut known {
            knows.asked_next = knows.next;
            knows.asked_state = knows.state;
            knows.asked_lease = false;
            knows.asked_label = knows.label;
            let session = knows.session;
            match rng.range(0..8) {
                // Most of the time, records.
                0..=4 => {
                    let count = rng.range(1..4);
                    knows.asked_next = knows.next + count;
                    let stored =
                        journal.append(session, knows.lease, batch(session, knows.next, count));
                    asked.push(Asked::Records(stored));
                }
                // A state as of a record that is stored.
                5 if knows.next > 1 => {
                    let upto = rng.range(knows.state.unwrap_or(0)..knows.next);
                    knows.asked_state = Some(upto);
                    let state = Snapshot {
                        upto: Seq(upto),
                        state: state_of(session, upto),
                    };
                    asked.push(Asked::State(journal.snapshot(session, knows.lease, state)));
                }
                // Another owner.
                6 => {
                    knows.asked_lease = true;
                    asked.push(Asked::Owner(journal.acquire(session)));
                }
                // Another label.
                7 => {
                    knows.asked_label = Some(round);
                    let kept = journal.label(session, &label_of(session, round));
                    asked.push(Asked::Label(Box::pin(kept)));
                }
                _ => asked.push(Asked::Nothing),
            }
        }
        keeper
            .release
            .send(())
            .map_err(|_| "the journal is gone".to_owned())?;
        let _ = keeper.held.try_recv();

        let plugged = soon(plug).await;
        let mut dead = plugged.is_err();
        issuing = plugged.is_err();
        if let Ok(issued) = plugged {
            token = Some(issued);
        }
        for (knows, asked) in known.iter_mut().zip(asked) {
            match asked {
                Asked::Records(stored) => match soon(stored).await {
                    Ok(upto) => {
                        check(
                            upto.get() + 1 == knows.asked_next,
                            format!("stored up to {upto:?}"),
                        )?;
                        knows.next = knows.asked_next;
                    }
                    Err(JournalError::Store(_)) => dead = true,
                    Err(refused) => return Err(format!("records were refused: {refused}")),
                },
                Asked::State(stored) => match soon(stored).await {
                    Ok(()) => knows.state = knows.asked_state,
                    Err(JournalError::Store(_)) => dead = true,
                    Err(refused) => return Err(format!("a state was refused: {refused}")),
                },
                Asked::Owner(taking) => match soon(taking).await {
                    Ok(got) => {
                        check(
                            got.lease > knows.lease,
                            "a lease that does not outrank".to_owned(),
                        )?;
                        check(
                            got.next == Seq(knows.next),
                            format!("taken at {:?}", got.next),
                        )?;
                        knows.lease = got.lease;
                    }
                    Err(JournalError::Store(_)) => dead = true,
                    Err(refused) => return Err(format!("an owner was refused: {refused}")),
                },
                Asked::Label(kept) => match soon(kept).await {
                    Ok(()) => knows.label = knows.asked_label,
                    Err(JournalError::Store(_)) => dead = true,
                    Err(refused) => return Err(format!("a label was refused: {refused}")),
                },
                Asked::Nothing => {}
            }
        }
        if dead {
            break;
        }
    }
    check(disk.is_dark(), "the power never went".to_owned())?;
    drop(journal);

    // The next process, on what the power cut left.
    let disk = Disk::holding(disk.left());
    let journal = match LocalJournal::on(disk.clone(), Config::default()) {
        Ok(journal) => journal,
        Err(error) => return Err(format!("the journal does not open: {error}")),
    };
    // The commit that was cut short is there whole or not at all: every
    // session of it says the same.
    let labelled = soon(journal.labelled())
        .await
        .map_err(|error| format!("the labels cannot be read: {error}"))?;
    let mut whole: Option<bool> = None;
    for knows in &known {
        let session = knows.session;
        // The label that is kept with it is one it was given, whole.
        let label = labelled.iter().find(|(labelled, _)| *labelled == session);
        let label = label.and_then(|(_, label)| label.clone());
        let label_of = |round: Option<u64>| round.map(|round| label_of(session, round));
        let label_as_before = label == label_of(knows.label);
        let label_as_asked = label == label_of(knows.asked_label);
        check(
            label_as_before || label_as_asked,
            format!(
                "session {} has the label {label:?}: it had that of round {:?}, and asked for \
                 that of round {:?}",
                session.0, knows.label, knows.asked_label
            ),
        )?;
        let found = soon(journal.acquire(session))
            .await
            .map_err(|error| format!("session {} cannot be taken: {error}", session.0))?;
        let next = found.next.get();
        let state = found.snapshot.as_ref().map(|state| state.upto.get());
        let asked_label = knows.asked_label != knows.label;
        let asked = knows.asked_next != knows.next
            || knows.asked_state != knows.state
            || knows.asked_lease
            || asked_label;
        let as_before = next == knows.next && state == knows.state;
        let as_asked = next == knows.asked_next && state == knows.asked_state;
        check(
            as_before || as_asked,
            format!(
                "session {} has {next} and a state as of {state:?}: it had {} and {:?}, and \
                 asked for {} and {:?}",
                session.0, knows.next, knows.state, knows.asked_next, knows.asked_state
            ),
        )?;
        // What its lease is says whether a new owner was stored.
        let owner = found.lease.0 - 1;
        let lease_as_asked = owner == knows.lease.0 + 1;
        check(
            owner == knows.lease.0 || (knows.asked_lease && lease_as_asked),
            format!("session {} was owned with lease {owner}", session.0),
        )?;
        if asked {
            let took = if knows.asked_lease {
                lease_as_asked
            } else if asked_label {
                label_as_asked
            } else {
                as_asked && !as_before
            };
            check(
                whole.is_none_or(|whole| whole == took),
                format!("session {} has a commit in part: {known:?}", session.0),
            )?;
            whole = Some(took);
        }
        // Every record is the one that was stored under its number.
        let first = state.map_or(1, |upto| upto + 1);
        let expected: Vec<Bytes> = (first..next).map(|n| record(session, n)).collect();
        check(
            found.tail == expected,
            format!("session {} has other records", session.0),
        )?;
        if let Some(found) = &found.snapshot {
            check(
                found.state == state_of(session, found.upto.get()),
                format!("session {} has another state", session.0),
            )?;
        }
        // And the session goes on.
        let more = soon(journal.append(session, found.lease, batch(session, next, 2))).await;
        check(
            more == Ok(Seq(next + 1)),
            format!("it does not go on: {more:?}"),
        )?;
    }
    // A token the journal said it had issued is still good, unless one
    // was being issued when the power went, which may have taken its
    // place.
    if let Some(token) = &token {
        let admitted = soon(journal.admit(token))
            .await
            .map_err(|e| e.to_string())?;
        check(
            admitted || issuing,
            "a token that was issued is not admitted".to_owned(),
        )?;
    }
    drop(journal);

    // What was written after the power came back is there for the
    // process after that.
    let journal = LocalJournal::on(
        Disk::holding(disk.platter().seen.clone()),
        Config::default(),
    )
    .map_err(|error| format!("the journal does not open a second time: {error}"))?;
    for knows in &known {
        let found = soon(journal.acquire(knows.session))
            .await
            .map_err(|e| e.to_string())?;
        check(
            found.next.get() >= knows.next + 2,
            "what was added is gone".to_owned(),
        )?;
    }
    // A commit that could not be issued a token was the one that was
    // held, with nothing of a session in it.
    Ok(match whole {
        _ if issuing => Cut::Plug,
        Some(true) => Cut::Whole,
        Some(false) => Cut::Gone,
        None => Cut::Empty,
    })
}

/// Which commit the power went in, and what became of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    /// The one that only issued a token.
    Plug,
    /// One with what sessions asked, and the next process finds all of
    /// it.
    Whole,
    /// One with what sessions asked, and the next process finds none of
    /// it.
    Gone,
    /// One in which no session asked for anything.
    Empty,
}

/// What a session asked of the journal in a round.
enum Asked<'a> {
    Records(waveport_ports::error::BoxFuture<'a, Result<Seq, JournalError>>),
    State(waveport_ports::error::BoxFuture<'a, Result<(), JournalError>>),
    Owner(
        waveport_ports::error::BoxFuture<
            'a,
            Result<waveport_ports::journal::Recovered, JournalError>,
        >,
    ),
    Label(waveport_ports::error::BoxFuture<'a, Result<(), JournalError>>),
    Nothing,
}

#[test]
fn a_power_cut_in_the_middle_of_a_commit_loses_nothing_that_was_stored() {
    let plan = SeedPlan::from_env(SeedPlan::Range {
        start: 0,
        count: 60,
    })
    .unwrap();
    // How often a commit with what sessions asked was cut short and was
    // there whole, how often it was not there at all, and how often the
    // power went in a commit that had nothing of a session in it.
    let (mut whole, mut gone, mut other) = (0_u64, 0_u64, 0_u64);
    let summary = run(plan, |sim| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|error| error.to_string())?;
        let seed = sim.seed().0;
        match runtime.block_on(power_goes(seed, sim.rng.fork("journal")))? {
            Cut::Whole => whole += 1,
            Cut::Gone => gone += 1,
            Cut::Plug | Cut::Empty => other += 1,
        }
        Ok::<(), String>(())
    });
    let seeds = match summary {
        Ok(summary) => summary.seeds,
        Err(failure) => panic!("{failure}"),
    };
    println!(
        "{seeds} power cuts: a commit of sessions was there whole {whole} times and not at all \
         {gone} times, and {other} cuts were in other commits"
    );
    // Both ways out of a cut commit were seen, or the seeds do not try
    // what this is for.
    if seeds >= 50 {
        assert!(whole >= 1 && gone >= 1, "{whole} whole, {gone} gone");
    }
}
