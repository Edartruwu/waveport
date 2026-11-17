//! A journal in a file on this machine.
//!
//! Stability: experimental
//!
//! What a session records is kept in one file, so that a session whose
//! process died goes on in the next one: `kill -9`, a crash or a power
//! cut loses nothing the journal said was stored.
//!
//! The file is a [redb](https://www.redb.org) database, and its safety
//! is redb's. A group of requests is committed as one transaction with
//! one sync of the file, and a commit that was cut short anywhere leaves
//! the file as it was after the commit before. This crate adds what a
//! journal is on top of that:
//!
//! - **One writer.** A thread of its own does everything that changes
//!   the file. Requests that arrive while it commits are the next group,
//!   so the more sessions write, the more one sync stores.
//! - **Fencing.** A session has one owner. Taking it over gives a lease
//!   that outranks the one before, and what an earlier owner asks for
//!   after that is refused.
//! - **States.** A session's state as of one of its records replaces the
//!   records up to that one.
//! - **Resume tokens.** A [`ResumeToken`] is the right to come back to a
//!   session. The file holds its hash and not the token.
//!
//! One process has the file at a time: a second one that opens it is
//! told that it is in use.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use waveport_ports::journal::{Journal, SessionId};
//! use waveport_store_journal::{Config, LocalJournal};
//!
//! let journal = LocalJournal::open(".waveport/journal.redb", Config::default())?;
//! let session = SessionId(7);
//! let token = journal.issue(session).await?;
//! let recovered = journal.acquire(session).await?;
//! assert!(recovered.is_new());
//!
//! // In the next process, with what the caller kept:
//! assert!(journal.admit(&token).await?);
//! # Ok(()) }
//! ```

mod layout;
mod token;
mod writer;

use core::fmt;
use core::future::Future;
use core::time::Duration;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use bytes::Bytes;
use redb::{Builder, Database};
use tokio::sync::oneshot;
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::journal::{
    Batch, Journal, JournalError, Lease, Recovered, Seq, SessionId, Snapshot,
};

pub use crate::token::{NotAToken, ResumeToken};
pub use crate::writer::Labelled;
use crate::writer::{Request, Settings, Writer, prepare};

/// How a journal works.
pub struct Config {
    /// The least time between the beginnings of two commits. A request
    /// that arrives sooner after a commit began waits for the rest of
    /// it, and is stored together with whatever else arrives meanwhile.
    /// What someone is held up by does not wait: see
    /// [`Journal::hurry`].
    pub gather: Duration,
    /// Commit in two phases, with two syncs of the file, where one is
    /// the usual.
    ///
    /// With one, what tells a whole commit from one that a crash cut
    /// short is a checksum that is not made to withstand an attacker.
    /// Someone who chooses what is recorded, can read the file and can
    /// stop the machine at an instant of their choosing could have a
    /// half-written commit taken for a whole one. Two phases rule that
    /// out, at about twice the time for a commit.
    pub two_phase: bool,
    /// How much memory the file's pages may be kept in.
    pub cache_bytes: usize,
    /// Read the whole file and check it against its checksums each time
    /// it is opened, and not only after a crash. A file that something
    /// else has written to is then refused as corrupt when it is opened,
    /// and not when the damage is met.
    pub check_on_open: bool,
    /// Something to show every commit to.
    pub probe: Option<Box<dyn Probe>>,
}

impl Default for Config {
    /// A commit every 10 ms at most, in one phase, with 16 MiB of
    /// memory, and a file that is checked when it is opened.
    fn default() -> Self {
        Self {
            gather: Duration::from_millis(10),
            two_phase: false,
            cache_bytes: 16 << 20,
            check_on_open: true,
            probe: None,
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("gather", &self.gather)
            .field("two_phase", &self.two_phase)
            .field("cache_bytes", &self.cache_bytes)
            .field("check_on_open", &self.check_on_open)
            .field("probe", &self.probe.is_some())
            .finish()
    }
}

/// What a commit holds, as a [`Probe`] is shown it.
#[derive(Clone, Copy, Debug)]
pub struct Seen<'a> {
    /// How many requests the commit answers.
    pub requests: usize,
    /// The records it stores, in the order they were asked for.
    pub records: &'a [Bytes],
    /// How many states it stores.
    pub states: usize,
}

/// Something that is shown every commit of a journal, on the journal's
/// own thread: what measures the journal, and a test that has to stop a
/// process at a known point of a commit.
///
/// The journal does nothing else while a probe looks, so a probe that
/// takes its time holds every session up.
pub trait Probe: Send + 'static {
    /// A commit is about to be made. Nothing of it is stored yet.
    fn committing(&mut self, seen: &Seen<'_>) {
        let _ = seen;
    }

    /// The commit is stored, after `took`. Nobody has been told yet.
    fn committed(&mut self, seen: &Seen<'_>, took: Duration) {
        let _ = (seen, took);
    }
}

/// What a journal has done since it was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JournalStats {
    /// Commits made: syncs of the file, or twice as many in two phases.
    pub commits: u64,
    /// Requests those commits answered.
    pub requests: u64,
    /// The most requests one commit answered.
    pub largest: usize,
    /// The longest a commit took.
    pub slowest: Duration,
    /// Groups of requests that could not be committed.
    pub failed: u64,
}

/// Where a journal that is not in a file keeps its bytes: for a test
/// that loses or tears what was written, as a machine that loses power
/// does.
///
/// It is a file as a journal needs one: of a length, read and written at
/// any place, and with a way to be told that what was written has to
/// outlast a crash. What was written and not synced may be lost, whole
/// or in part and in any order.
pub trait Storage: fmt::Debug + Send + Sync + 'static {
    /// How many bytes the storage holds.
    ///
    /// # Errors
    ///
    /// The storage failed.
    fn size(&self) -> std::io::Result<u64>;

    /// Fills `out` with what is stored from `offset` on.
    ///
    /// # Errors
    ///
    /// The storage failed, or does not reach that far.
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()>;

    /// Makes the storage `len` long. What it grows by is zeros.
    ///
    /// # Errors
    ///
    /// The storage failed.
    fn set_len(&self, len: u64) -> std::io::Result<()>;

    /// Makes everything that was written outlast a crash.
    ///
    /// # Errors
    ///
    /// The storage failed. What was written may or may not be kept.
    fn sync(&self) -> std::io::Result<()>;

    /// Stores `data` from `offset` on.
    ///
    /// # Errors
    ///
    /// The storage failed.
    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()>;
}

/// A [`Storage`] as the store underneath takes one.
#[derive(Debug)]
struct Backend<S>(S);

impl<S: Storage> redb::StorageBackend for Backend<S> {
    fn len(&self) -> std::io::Result<u64> {
        self.0.size()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.0.read(offset, out)
    }

    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.0.set_len(len)
    }

    fn sync_data(&self) -> std::io::Result<()> {
        self.0.sync()
    }

    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        self.0.write(offset, data)
    }
}

/// Why a journal could not be opened.
#[derive(Debug)]
pub enum OpenError {
    /// Another process has the file open, or another journal of this
    /// one.
    InUse,
    /// The file is not what a journal left there: it is damaged, or it
    /// is another kind of file.
    Corrupt(String),
    /// The file was written by a later build, in a layout this one does
    /// not know.
    Newer {
        /// The version of the layout the file says it is in.
        format: u64,
    },
    /// The file could not be read or written.
    Io(std::io::Error),
    /// Something else went wrong.
    Failed(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InUse => f.write_str("the journal is in use by another process"),
            Self::Corrupt(why) => write!(
                f,
                "the journal's file is damaged or is not a journal: {why}"
            ),
            Self::Newer { format } => write!(
                f,
                "the journal's file is in version {format} of its layout, and this build reads \
                 up to version {}",
                layout::FORMAT
            ),
            Self::Io(error) => write!(
                f,
                "the journal's file could not be read or written: {error}"
            ),
            Self::Failed(why) => write!(f, "the journal could not be opened: {why}"),
        }
    }
}

impl core::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InUse | Self::Corrupt(_) | Self::Newer { .. } | Self::Failed(_) => None,
        }
    }
}

impl From<redb::Error> for OpenError {
    fn from(error: redb::Error) -> Self {
        match error {
            redb::Error::DatabaseAlreadyOpen => Self::InUse,
            redb::Error::Corrupted(why) => Self::Corrupt(why),
            // The store underneath has a layout of its own, and this is
            // a file of an older one.
            redb::Error::UpgradeRequired(version) => Self::Failed(format!(
                "the file is in version {version} of the layout of the store underneath, which \
                 this build no longer reads"
            )),
            // A file that is too short to be a journal, or does not
            // begin as one does.
            redb::Error::Io(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                Self::Corrupt(error.to_string())
            }
            redb::Error::Io(error) => Self::Io(error),
            other => Self::Failed(other.to_string()),
        }
    }
}

/// A journal in a file on this machine.
///
/// Dropping it waits for what was asked of it to be stored, and closes
/// the file.
pub struct LocalJournal {
    /// The way to the writer. `None` once the journal is being dropped.
    requests: Option<Sender<Request>>,
    writer: Option<JoinHandle<()>>,
    stats: Arc<Mutex<JournalStats>>,
    path: Option<PathBuf>,
}

impl fmt::Debug for LocalJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalJournal")
            .field("path", &self.path)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Makes the file at `path`, empty and its owner's alone, if there is no
/// file there. The store underneath takes an empty file for a new one.
#[cfg(unix)]
fn make_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    match made {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

/// How the store underneath is set up for a journal.
fn builder(cache_bytes: usize, check_on_open: bool) -> Builder {
    let mut builder = Database::builder();
    builder
        .set_cache_size(cache_bytes)
        .set_check_integrity_on_open(check_on_open);
    builder
}

impl LocalJournal {
    /// Opens the journal in the file at `path`, and makes the file if it
    /// is not there. The directory it is in has to be.
    ///
    /// The file holds what was said in every session. Where the system
    /// lets a file be its owner's alone, a file that is made here is: on
    /// Unix nobody else can read or write it. A file that was there
    /// already is left as it is.
    ///
    /// A file that was left by a process that died is brought back to
    /// what it last committed, which reads all of it once.
    ///
    /// # Errors
    ///
    /// The file is in use, is not a journal this build can read, or
    /// cannot be read or written.
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self, OpenError> {
        let path = path.as_ref().to_path_buf();
        // Elsewhere a file is its owner's by where it is, not by what
        // it is told: the store underneath makes it.
        #[cfg(unix)]
        make_private(&path).map_err(OpenError::Io)?;
        let (cache_bytes, check_on_open) = (config.cache_bytes, config.check_on_open);
        let file = path.clone();
        let open = move || -> Result<Database, OpenError> {
            let db = builder(cache_bytes, check_on_open)
                .create(&file)
                .map_err(|error| OpenError::from(redb::Error::from(error)))?;
            prepare(&db)?;
            Ok(db)
        };
        let db = open()?;
        Self::start(db, Box::new(open), config, Some(path))
    }

    /// A journal that keeps its bytes in `storage`, which may hold a
    /// journal already. A clone of the storage is the same bytes: the
    /// journal opens them again through one after they failed.
    ///
    /// # Errors
    ///
    /// What the storage holds is not a journal this build can read, or
    /// the storage failed.
    pub fn on(storage: impl Storage + Clone, config: Config) -> Result<Self, OpenError> {
        let (cache_bytes, check_on_open) = (config.cache_bytes, config.check_on_open);
        let open = move || -> Result<Database, OpenError> {
            let db = builder(cache_bytes, check_on_open)
                .create_with_backend(Backend(storage.clone()))
                .map_err(|error| OpenError::from(redb::Error::from(error)))?;
            prepare(&db)?;
            Ok(db)
        };
        let db = open()?;
        Self::start(db, Box::new(open), config, None)
    }

    fn start(
        db: Database,
        reopen: writer::Reopen,
        config: Config,
        path: Option<PathBuf>,
    ) -> Result<Self, OpenError> {
        let stats = Arc::new(Mutex::new(JournalStats::default()));
        let writer = Writer {
            db: Some(db),
            reopen,
            settings: Settings {
                gather: config.gather,
                two_phase: config.two_phase,
            },
            probe: config.probe,
            stats: Arc::clone(&stats),
        };
        let (requests, asked) = mpsc::channel();
        let writer = std::thread::Builder::new()
            .name("waveport-journal".to_owned())
            .spawn(move || writer.run(&asked))
            .map_err(OpenError::Io)?;
        Ok(Self {
            requests: Some(requests),
            writer: Some(writer),
            stats,
            path,
        })
    }

    /// The file the journal is in, if it is in one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// What the journal has done since it was opened.
    #[must_use]
    pub fn stats(&self) -> JournalStats {
        *self.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands the writer a request. It is on its way when this returns,
    /// whether or not anyone waits for the answer. Returns whether there
    /// is a writer to hand it to.
    fn send(&self, request: Request) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|requests| requests.send(request).is_ok())
    }

    /// Issues the token that lets `session` be come back to, in place of
    /// the one issued before. The session may be one the journal holds
    /// nothing of yet.
    ///
    /// As with everything asked of a journal, the request is on its way
    /// when this returns, and the future is only the answer.
    ///
    /// # Errors
    ///
    /// The operating system gave no entropy, or the store failed.
    pub fn issue(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<ResumeToken, JournalError>> + Send + use<> {
        let token = ResumeToken::new(session).map_err(JournalError::Store);
        let stored = token.as_ref().ok().map(|token| {
            let (reply, answer) = oneshot::channel();
            let hash = token.hash();
            let sent = self.send(Request::Issue {
                session,
                hash,
                reply,
            });
            answered(sent, answer)
        });
        async move {
            let token = token?;
            if let Some(stored) = stored {
                stored.await?;
            }
            Ok(token)
        }
    }

    /// Whether `token` is the one that was last issued for its session.
    /// Its holder may then take the session over.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub fn admit(
        &self,
        token: &ResumeToken,
    ) -> impl Future<Output = Result<bool, JournalError>> + Send + use<> {
        let (reply, answer) = oneshot::channel();
        let sent = self.send(Request::Admit {
            session: token.session(),
            hash: token.hash(),
            reply,
        });
        answered(sent, answer)
    }

    /// Lets go of everything that is stored of `session`: its records,
    /// its state, its owner and its token. Whoever asks decides that
    /// nobody goes on with the session: nothing is asked of its owner.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub fn forget(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), JournalError>> + Send + use<> {
        let (reply, answer) = oneshot::channel();
        let sent = self.send(Request::Forget { session, reply });
        answered(sent, answer)
    }

    /// Keeps `label` with `session`, in place of the label before: what
    /// whoever keeps sessions here has to know of one to go on with it,
    /// such as which agent it is a session of. The journal does not
    /// read it, and lets go of it with the session.
    ///
    /// # Errors
    ///
    /// The label is longer than 1,024 bytes, or the store failed.
    pub fn label(
        &self,
        session: SessionId,
        label: &[u8],
    ) -> impl Future<Output = Result<(), JournalError>> + Send + use<> {
        let (reply, answer) = oneshot::channel();
        let sent = self.send(Request::Label {
            session,
            label: label.to_vec(),
            reply,
        });
        answered(sent, answer)
    }

    /// The sessions the journal holds anything of, in the order of their
    /// names, each with the label that is kept with it, if one is.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub fn labelled(
        &self,
    ) -> impl Future<Output = Result<Vec<Labelled>, JournalError>> + Send + use<> {
        let (reply, answer) = oneshot::channel();
        let sent = self.send(Request::Labelled { reply });
        answered(sent, answer)
    }

    /// The sessions the journal holds anything of, in the order of their
    /// names.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub fn sessions(
        &self,
    ) -> impl Future<Output = Result<Vec<SessionId>, JournalError>> + Send + use<> {
        let (reply, answer) = oneshot::channel();
        let sent = self.send(Request::Sessions { reply });
        answered(sent, answer)
    }
}

/// The answer to a request, when there is one. `sent` is whether the
/// request could be handed to the writer.
async fn answered<T: Send + 'static>(
    sent: bool,
    answer: oneshot::Receiver<Result<T, JournalError>>,
) -> Result<T, JournalError> {
    let stopped = || JournalError::Store(PortError::permanent("the journal has stopped"));
    if !sent {
        return Err(stopped());
    }
    answer.await.unwrap_or_else(|_| Err(stopped()))
}

impl Journal for LocalJournal {
    fn acquire(&self, session: SessionId) -> BoxFuture<'_, Result<Recovered, JournalError>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.send(Request::Acquire { session, reply });
        Box::pin(answered(sent, answer))
    }

    fn append(
        &self,
        session: SessionId,
        lease: Lease,
        batch: Batch,
    ) -> BoxFuture<'_, Result<Seq, JournalError>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.send(Request::Append {
            session,
            lease,
            batch,
            reply,
        });
        Box::pin(answered(sent, answer))
    }

    fn snapshot(
        &self,
        session: SessionId,
        lease: Lease,
        snapshot: Snapshot,
    ) -> BoxFuture<'_, Result<(), JournalError>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.send(Request::Snapshot {
            session,
            lease,
            snapshot,
            reply,
        });
        Box::pin(answered(sent, answer))
    }

    fn hurry(&self, _session: SessionId) {
        // One writer serves every session, so whoever is held up, it is
        // the group in hand that goes now.
        let _ = self.send(Request::Hurry);
    }
}

impl Drop for LocalJournal {
    fn drop(&mut self) {
        // With nobody left to ask, the writer stores what it was asked
        // and ends, and the file is closed when it has.
        drop(self.requests.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}
