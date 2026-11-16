//! The threads sessions run on.
//!
//! A runtime has a fixed number of shards. A shard is one thread with an
//! executor of its own, and a session stays on the shard it was started
//! on for all of its life. Nothing a session does crosses to another
//! thread, except what it gives the listening models and what the world
//! asks of it: there is no lock on its way, and a session on a shard runs
//! as it does alone on a thread in a test.
//!
//! What the sessions of a shard share is the thread's time. Every run of
//! a session is timed, and the runs that took too long are counted.
//!
//! A session that panics ends, and is said to have. The shard and its
//! other sessions go on.

use core::fmt;
use core::pin::Pin;
use core::sync::atomic::Ordering;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use tokio::runtime::{Builder, LocalOptions};
use tokio::sync::{mpsc, oneshot};
use waveport_domain::ids::Seq;
use waveport_domain::policy::Policies;
use waveport_ports::control::{ControlError, EventRx, SessionControl, SessionEvent, SessionInput};
use waveport_ports::journal::SessionId;
use waveport_ports::plan::SessionPlan;

use crate::session::{Ended, Parts, Rebuilt, Request, Session, SessionSpec, StartError};
pub use crate::watch::ShardStats;
use crate::watch::{Counters, Watched};

/// How a runtime is set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// How many shards it has.
    pub shards: usize,
    /// How long a session may run at a time before the run is counted as
    /// one that held the shard up.
    pub poll_budget: Duration,
    /// How many requests can wait for a session.
    pub inbox: usize,
    /// How far behind a follower of a session can be before it is let
    /// go, in things it is told.
    pub follower_room: usize,
}

impl Default for RuntimeConfig {
    /// One shard, runs of 250 µs, 256 requests waiting and followers 256
    /// behind.
    fn default() -> Self {
        Self {
            shards: 1,
            poll_budget: Duration::from_micros(250),
            inbox: 256,
            follower_room: 256,
        }
    }
}

/// Why a runtime could not be set up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    /// It was to have no shards, or no room for requests or followers.
    Config,
    /// A shard's thread or executor could not be started.
    Thread(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => f.write_str("a runtime needs a shard, and room for requests"),
            Self::Thread(error) => write!(f, "a shard could not be started: {error}"),
        }
    }
}

impl core::error::Error for RuntimeError {}

/// Why a session was not started.
#[derive(Clone, Debug, PartialEq)]
pub enum SpawnError {
    /// A session of that name is running here already.
    Running,
    /// It could not be made ready.
    Start(StartError),
    /// It panicked while it was made ready.
    Panicked,
    /// The runtime has stopped.
    Stopped,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("a session of that name is running here already"),
            Self::Start(error) => write!(f, "the session could not be made ready: {error}"),
            Self::Panicked => f.write_str("the session panicked while it was made ready"),
            Self::Stopped => f.write_str("the runtime has stopped"),
        }
    }
}

impl core::error::Error for SpawnError {}

/// How a session on a shard came to its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It ran to its end.
    Ended(Ended),
    /// It panicked. What it had recorded by then is in its journal.
    Panicked,
    /// The runtime stopped while it ran.
    Stopped,
}

/// A session that was started.
#[derive(Debug)]
pub struct Started {
    /// Its name.
    pub id: SessionId,
    /// How it is run with the parts it has.
    pub plan: SessionPlan,
    /// What it was rebuilt from, if it goes on after an earlier life.
    pub rebuilt: Option<Rebuilt>,
    exit: oneshot::Receiver<Exit>,
}

impl Started {
    /// Waits for the session to end, and says how it did.
    pub async fn exit(self) -> Exit {
        self.exit.await.unwrap_or(Exit::Stopped)
    }
}

/// Something for a shard to do, on its own thread.
type Job = Box<dyn FnOnce() + Send>;

struct Shard {
    jobs: mpsc::UnboundedSender<Job>,
    counters: Arc<Counters>,
    thread: Option<JoinHandle<()>>,
}

/// The sessions that run here, and how each is reached.
type Directory = Mutex<BTreeMap<SessionId, mpsc::Sender<Request>>>;

/// Shards, and the sessions that run on them.
///
/// Dropping it stops every session where it is.
pub struct Runtime {
    config: RuntimeConfig,
    shards: Vec<Shard>,
    directory: Arc<Directory>,
}

impl Runtime {
    /// Starts the shards.
    ///
    /// # Errors
    ///
    /// The configuration asks for nothing, or a thread could not be
    /// started.
    pub fn new(config: RuntimeConfig) -> Result<Self, RuntimeError> {
        if config.shards == 0 || config.inbox == 0 || config.follower_room == 0 {
            return Err(RuntimeError::Config);
        }
        let mut shards = Vec::with_capacity(config.shards);
        for index in 0..config.shards {
            shards.push(start_shard(index)?);
        }
        Ok(Self {
            config,
            shards,
            directory: Arc::default(),
        })
    }

    /// Starts a session on the shard that has the fewest, and returns
    /// once it is ready to run.
    ///
    /// # Errors
    ///
    /// A session of that name runs here already, or this one could not
    /// be made ready.
    pub async fn start<P>(
        &self,
        spec: SessionSpec,
        parts: Parts,
        policies: P,
    ) -> Result<Started, SpawnError>
    where
        P: Policies + Send + 'static,
    {
        let id = spec.id;
        let Some(shard) = self
            .shards
            .iter()
            .min_by_key(|shard| shard.counters.sessions.load(Ordering::Relaxed))
        else {
            return Err(SpawnError::Stopped);
        };
        let (requests, inbox) = mpsc::channel(self.config.inbox);
        {
            let mut directory = lock(&self.directory);
            if directory.contains_key(&id) {
                return Err(SpawnError::Running);
            }
            directory.insert(id, requests);
        }
        shard.counters.sessions.fetch_add(1, Ordering::Relaxed);

        let (ready, started) = oneshot::channel();
        let (exited, exit) = oneshot::channel();
        let counters = Arc::clone(&shard.counters);
        let directory = Arc::clone(&self.directory);
        let budget = self.config.poll_budget;
        let job: Job = Box::new(move || {
            let run = async move {
                let mut session = match Session::open(spec, parts, policies).await {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return None;
                    }
                };
                session.listen_to(inbox);
                let _ = ready.send(Ok((session.plan().clone(), session.rebuilt())));
                Some(session.run().await)
            };
            let watched = Watched::new(run, Arc::clone(&counters), budget);
            let running = tokio::task::spawn_local(watched);
            // Whatever becomes of the session, it is taken off the books
            // and whoever waits for it is told.
            tokio::task::spawn_local(async move {
                let exit = match running.await {
                    Ok(Some(ended)) => Some(Exit::Ended(ended)),
                    Ok(None) => None,
                    Err(error) if error.is_panic() => Some(Exit::Panicked),
                    Err(_) => Some(Exit::Stopped),
                };
                lock(&directory).remove(&id);
                counters.sessions.fetch_sub(1, Ordering::Relaxed);
                if let Some(exit) = exit {
                    let _ = exited.send(exit);
                }
            });
        });
        if shard.jobs.send(job).is_err() {
            lock(&self.directory).remove(&id);
            shard.counters.sessions.fetch_sub(1, Ordering::Relaxed);
            return Err(SpawnError::Stopped);
        }

        match started.await {
            Ok(Ok((plan, rebuilt))) => Ok(Started {
                id,
                plan,
                rebuilt,
                exit,
            }),
            Ok(Err(error)) => Err(SpawnError::Start(error)),
            // Nobody said how the start went: it panicked, or the shard
            // stopped under it.
            Err(_) => match exit.await {
                Ok(Exit::Panicked) => Err(SpawnError::Panicked),
                _ => Err(SpawnError::Stopped),
            },
        }
    }

    /// How many sessions run here now.
    #[must_use]
    pub fn sessions(&self) -> usize {
        lock(&self.directory).len()
    }

    /// What has run on each shard.
    #[must_use]
    pub fn stats(&self) -> Vec<ShardStats> {
        self.shards
            .iter()
            .map(|shard| shard.counters.read())
            .collect()
    }

    fn reach(&self, session: SessionId, request: Request) -> Result<(), ControlError> {
        let directory = lock(&self.directory);
        let requests = directory.get(&session).ok_or(ControlError::NoSuchSession)?;
        requests.try_send(request).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ControlError::Behind,
            // It has just ended.
            mpsc::error::TrySendError::Closed(_) => ControlError::NoSuchSession,
        })
    }
}

impl SessionControl for Runtime {
    fn send(&self, session: SessionId, input: SessionInput) -> Result<(), ControlError> {
        self.reach(session, Request::Input(input))
    }

    fn subscribe(
        &self,
        session: SessionId,
        from: Seq,
    ) -> Result<Pin<Box<dyn EventRx>>, ControlError> {
        let (to, following) = mpsc::channel(self.config.follower_room);
        self.reach(session, Request::Follow { from, to })?;
        Ok(Box::pin(Following(following)))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // A shard that is given nothing more to do stops, and with it its
        // sessions.
        let threads: Vec<_> = self
            .shards
            .drain(..)
            .filter_map(|mut shard| shard.thread.take())
            .collect();
        for thread in threads {
            // A shard that panicked has stopped, which is all that is
            // wanted of it here.
            let _ = thread.join();
        }
    }
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime")
            .field("config", &self.config)
            .field("sessions", &self.sessions())
            .finish_non_exhaustive()
    }
}

/// What a follower of a session on a shard is told.
struct Following(mpsc::Receiver<SessionEvent>);

impl EventRx for Following {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<SessionEvent>> {
        self.get_mut().0.poll_recv(cx)
    }
}

fn lock(
    directory: &Directory,
) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, mpsc::Sender<Request>>> {
    directory.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts the thread of a shard, and waits until its executor is there.
fn start_shard(index: usize) -> Result<Shard, RuntimeError> {
    let (jobs, mut waiting) = mpsc::unbounded_channel::<Job>();
    let (built, is_built) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(format!("waveport-shard-{index}"))
        .spawn(move || {
            let executor = Builder::new_current_thread()
                .enable_all()
                .build_local(LocalOptions::default());
            let executor = match executor {
                Ok(executor) => {
                    let _ = built.send(Ok(()));
                    executor
                }
                Err(error) => {
                    let _ = built.send(Err(error.to_string()));
                    return;
                }
            };
            executor.block_on(async move {
                while let Some(job) = waiting.recv().await {
                    job();
                }
            });
        })
        .map_err(|error| RuntimeError::Thread(error.to_string()))?;
    match is_built.recv() {
        Ok(Ok(())) => Ok(Shard {
            jobs,
            counters: Arc::default(),
            thread: Some(thread),
        }),
        Ok(Err(error)) => Err(RuntimeError::Thread(error)),
        Err(_) => Err(RuntimeError::Thread(
            "the thread ended before it had begun".to_owned(),
        )),
    }
}
