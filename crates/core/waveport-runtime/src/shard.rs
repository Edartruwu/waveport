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
//!
//! The thread of a shard can be kept on a processor of its own
//! ([`RuntimeConfig::pinning`]). It is not, unless whoever runs the
//! process says which.
//!
//! A runtime that is to stop is drained first: it starts no session
//! from then on, and whoever stops it waits for the ones it has. What
//! still runs when the runtime is dropped stops where it is, and what it
//! had stored is in its journal.

use core::fmt;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use tokio::runtime::{Builder, LocalOptions};
use tokio::sync::{Notify, mpsc, oneshot};
use waveport_domain::ids::Seq;
use waveport_domain::policy::Policies;
use waveport_ports::control::{ControlError, EventRx, SessionControl, SessionEvent, SessionInput};
use waveport_ports::journal::{Lease, SessionId};
use waveport_ports::plan::SessionPlan;

use crate::pinning::{self, PinError, Pinning};
use crate::session::{Ended, Parts, Rebuilt, Request, Session, SessionSpec, Standing, StartError};
pub use crate::watch::ShardStats;
use crate::watch::{Counters, Watched};

/// How a runtime is set up.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// The processor each shard's thread is kept on, if they are kept
    /// anywhere. The threads a shard starts for work that blocks are
    /// kept on the processors that are no shard's.
    pub pinning: Option<Pinning>,
}

impl Default for RuntimeConfig {
    /// One shard, runs of 250 µs, 256 requests waiting and followers 256
    /// behind. Its thread is left to the system.
    fn default() -> Self {
        Self {
            shards: 1,
            poll_budget: Duration::from_micros(250),
            inbox: 256,
            follower_room: 256,
            pinning: None,
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
    /// The threads of the shards could not be kept on their processors.
    Pinning(PinError),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => f.write_str("a runtime needs a shard, and room for requests"),
            Self::Thread(error) => write!(f, "a shard could not be started: {error}"),
            Self::Pinning(error) => {
                write!(f, "the shards cannot have processors of their own: {error}")
            }
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
    /// The runtime has stopped, or is being drained and starts nothing
    /// more.
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
    /// The lease it writes its journal with, which says which of its
    /// lives this is: the number goes up each time it is taken over.
    pub lease: Lease,
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

/// The sessions that run here, how each is reached, and where each says
/// where it stands.
type Sessions = BTreeMap<SessionId, (mpsc::Sender<Request>, Arc<Standing>)>;
type Directory = Mutex<Sessions>;

/// Where a session stood when it last ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// The number its next fact gets.
    pub next_seq: Seq,
    /// Whether it has nothing under way: it can be stopped and gone on
    /// with later without anyone noticing more than the wait.
    pub at_rest: bool,
}

/// Shards, and the sessions that run on them.
///
/// Dropping it stops every session where it is.
pub struct Runtime {
    config: RuntimeConfig,
    shards: Vec<Shard>,
    directory: Arc<Directory>,
    /// Whether it starts nothing more.
    draining: AtomicBool,
    /// Told each time a session has gone.
    gone: Arc<Notify>,
}

impl Runtime {
    /// Starts the shards.
    ///
    /// # Errors
    ///
    /// The configuration asks for nothing, a thread could not be
    /// started, or a shard could not be kept on its processor.
    pub fn new(config: RuntimeConfig) -> Result<Self, RuntimeError> {
        if config.shards == 0 || config.inbox == 0 || config.follower_room == 0 {
            return Err(RuntimeError::Config);
        }
        let mut places = Vec::new();
        if let Some(pinning) = &config.pinning {
            if pinning.shards().len() != config.shards {
                return Err(RuntimeError::Pinning(PinError::Count {
                    shards: config.shards,
                    processors: pinning.shards().len(),
                }));
            }
            let rest: Arc<[usize]> = Arc::from(pinning.rest());
            places.extend(pinning.shards().iter().map(|&processor| Place {
                processor,
                rest: Arc::clone(&rest),
            }));
        }
        let mut places = places.into_iter();
        let mut shards = Vec::with_capacity(config.shards);
        for index in 0..config.shards {
            shards.push(start_shard(index, places.next())?);
        }
        Ok(Self {
            config,
            shards,
            directory: Arc::default(),
            draining: AtomicBool::new(false),
            gone: Arc::default(),
        })
    }

    /// Starts no session from now on, and waits until the ones that run
    /// here have ended, for as long as `patience`. Returns how many
    /// still run.
    ///
    /// Nothing is asked of the sessions: whoever drains a runtime says
    /// what is to become of them, before or meanwhile. One that still
    /// runs when the runtime is dropped stops where it is, and can be
    /// gone on with from its journal.
    pub async fn drain(&self, patience: Duration) -> usize {
        self.draining.store(true, Ordering::Relaxed);
        let until = tokio::time::Instant::now() + patience;
        loop {
            // Asked for before the count is read, so that a session
            // that goes in between is not missed.
            let gone = self.gone.notified();
            tokio::pin!(gone);
            gone.as_mut().enable();
            let left = self.sessions();
            if left == 0 || tokio::time::timeout_at(until, gone).await.is_err() {
                return self.sessions();
            }
        }
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
        if self.draining.load(Ordering::Relaxed) {
            return Err(SpawnError::Stopped);
        }
        let Some(shard) = self
            .shards
            .iter()
            .min_by_key(|shard| shard.counters.sessions.load(Ordering::Relaxed))
        else {
            return Err(SpawnError::Stopped);
        };
        let (requests, inbox) = mpsc::channel(self.config.inbox);
        let standing = Arc::<Standing>::default();
        {
            let mut directory = lock(&self.directory);
            if directory.contains_key(&id) {
                return Err(SpawnError::Running);
            }
            directory.insert(id, (requests, Arc::clone(&standing)));
        }
        shard.counters.sessions.fetch_add(1, Ordering::Relaxed);

        let (ready, started) = oneshot::channel();
        let (exited, exit) = oneshot::channel();
        let counters = Arc::clone(&shard.counters);
        let directory = Arc::clone(&self.directory);
        let gone = Arc::clone(&self.gone);
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
                session.report_to(standing);
                let begun = (session.plan().clone(), session.rebuilt(), session.lease());
                let _ = ready.send(Ok(begun));
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
                gone.notify_waiters();
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
            Ok(Ok((plan, rebuilt, lease))) => Ok(Started {
                id,
                plan,
                rebuilt,
                lease,
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

    /// Where the session `session` stood when it last ran, if it runs
    /// here.
    #[must_use]
    pub fn status(&self, session: SessionId) -> Option<Status> {
        let directory = lock(&self.directory);
        let (_, standing) = directory.get(&session)?;
        Some(Status {
            next_seq: standing.next_seq(),
            at_rest: standing.at_rest(),
        })
    }

    /// Whether every session that runs here has nothing under way.
    #[must_use]
    pub fn at_rest(&self) -> bool {
        lock(&self.directory)
            .values()
            .all(|(_, standing)| standing.at_rest())
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
        let (requests, _) = directory.get(&session).ok_or(ControlError::NoSuchSession)?;
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

    fn subscribe_from_state(
        &self,
        session: SessionId,
    ) -> Result<Pin<Box<dyn EventRx>>, ControlError> {
        let (to, following) = mpsc::channel(self.config.follower_room);
        self.reach(session, Request::FollowFromState { to })?;
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

fn lock(directory: &Directory) -> std::sync::MutexGuard<'_, Sessions> {
    directory.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where the thread of a shard is kept, and the threads it starts.
struct Place {
    /// The shard's own processor.
    processor: usize,
    /// The processors that are no shard's.
    rest: Arc<[usize]>,
}

/// Starts the thread of a shard, and waits until its executor is there.
/// With a `place`, the thread is kept there before the executor is
/// built.
fn start_shard(index: usize, place: Option<Place>) -> Result<Shard, RuntimeError> {
    let (jobs, mut waiting) = mpsc::unbounded_channel::<Job>();
    let (built, is_built) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(format!("waveport-shard-{index}"))
        .spawn(move || {
            let mut builder = Builder::new_current_thread();
            builder.enable_all();
            if let Some(Place { processor, rest }) = place {
                // The executor starts threads for work that blocks: a
                // name is looked up on one whenever an adapter connects.
                // A thread begins where its maker is, which would be
                // this shard's processor, so each is sent to the
                // processors that are no shard's as it starts. Nobody
                // can be told if that fails then. So it is tried here
                // first, on the way to where this thread stays.
                let kept = pinning::keep(&rest).and_then(|()| pinning::keep(&[processor]));
                if let Err(error) = kept {
                    let _ = built.send(Err(RuntimeError::Pinning(error)));
                    return;
                }
                builder.on_thread_start(move || {
                    let _ = pinning::keep(&rest);
                });
            }
            let executor = match builder.build_local(LocalOptions::default()) {
                Ok(executor) => {
                    let _ = built.send(Ok(()));
                    executor
                }
                Err(error) => {
                    let _ = built.send(Err(RuntimeError::Thread(error.to_string())));
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
        Ok(Err(error)) => Err(error),
        Err(_) => Err(RuntimeError::Thread(
            "the thread ended before it had begun".to_owned(),
        )),
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use core::time::Duration;
    use std::sync::Arc;

    use super::{Job, Place, Runtime, RuntimeConfig, RuntimeError, start_shard};
    use crate::pinning::{PinError, Pinning, allowed};

    /// Where the thread of `shard` may run, and where a thread may that
    /// it started for work that blocks.
    fn found(shard: &super::Shard) -> (Vec<usize>, Vec<usize>) {
        let (found, told) = std::sync::mpsc::channel();
        let job: Job = Box::new(move || {
            let here = allowed().unwrap();
            tokio::task::spawn_local(async move {
                let there = tokio::task::spawn_blocking(|| allowed().unwrap());
                let _ = found.send((here, there.await.unwrap()));
            });
        });
        assert!(shard.jobs.send(job).is_ok());
        told.recv_timeout(Duration::from_secs(10)).unwrap()
    }

    #[test]
    fn a_shard_stays_on_its_processor_and_what_it_starts_stays_off_it() {
        let all = allowed().unwrap();
        let Some((&processor, others)) = all.split_last().filter(|(_, others)| !others.is_empty())
        else {
            // There is one processor, and nothing to tell apart.
            return;
        };
        let place = Place {
            processor,
            rest: Arc::from(others),
        };
        let shard = start_shard(0, Some(place)).unwrap();
        let (here, there) = found(&shard);
        assert_eq!(here, [processor]);
        assert_eq!(there, others);
        // A shard that was given no place is left to the system, and so
        // is what it starts.
        let shard = start_shard(1, None).unwrap();
        assert_eq!(found(&shard), (all.clone(), all.clone()));
        // Whoever started them is where it was.
        assert_eq!(allowed().unwrap(), all);
    }

    #[test]
    fn a_runtime_is_not_started_with_shards_it_cannot_place() {
        let all = allowed().unwrap();
        let Some((&processor, _)) = all.split_last().filter(|(_, others)| !others.is_empty())
        else {
            return;
        };
        let with = |shards, pinning| RuntimeConfig {
            shards,
            pinning: Some(pinning),
            ..RuntimeConfig::default()
        };
        let pinning = Pinning::here(&[processor]).unwrap();
        // One processor for each shard.
        let refused = Runtime::new(with(2, pinning.clone())).map(|_| ());
        let count = PinError::Count {
            shards: 2,
            processors: 1,
        };
        assert_eq!(refused, Err(RuntimeError::Pinning(count.clone())));
        assert_eq!(
            RuntimeError::Pinning(count).to_string(),
            "the shards cannot have processors of their own: 1 processors were named for 2 \
             shards: each shard has one"
        );
        let runtime = Runtime::new(with(1, pinning)).unwrap();
        assert_eq!(runtime.sessions(), 0);
        // A place the system does not have is said by the shard that
        // was to be there.
        let Some(nowhere) = (0..1024).rev().find(|processor| !all.contains(processor)) else {
            return;
        };
        let place = Place {
            processor: nowhere,
            rest: Arc::from(all.as_slice()),
        };
        match start_shard(0, Some(place)).map(|_| ()) {
            Err(RuntimeError::Pinning(PinError::Refused { processors, .. })) => {
                assert_eq!(processors, [nowhere]);
            }
            other => panic!("{other:?}"),
        }
    }
}
