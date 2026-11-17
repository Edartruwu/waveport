//! The daemon: its agents, its sessions, and how it starts and stops.

use core::net::SocketAddr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use waveport_assembly::stack::name;
use waveport_assembly::{Agent, Context as _, Failure, ResumeToken, Settled, Stack, Told};
use waveport_ports::control::SessionInput;
use waveport_ports::journal::SessionId;
use waveport_ports::plan::SessionPlan;
use waveport_protocol::client::v1::ErrorCode;
use waveport_protocol::control::v1::{
    Mode, Problem, SessionCreated, SessionInfo, SessionState, Sessions,
};

use crate::auth::{Gate, Keys, Tickets};
use crate::config::{Config, Limits};
use crate::{http, serve};

/// How long the connections that are open have to close once the daemon
/// has said that it is going away.
const LAST_WORDS: Duration = Duration::from_secs(5);

/// Where a session of the daemon stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// It was made, and nobody has connected to it or asked anything of
    /// it. It is nowhere but here, and is let go of at `until`.
    Waiting { until: Instant },
    /// It runs.
    Running { life: u64, mode: Mode },
    /// It does not run, and is kept: it broke off, or the process that
    /// ran it stopped. Whoever comes back to it goes on with it.
    Kept,
}

/// A session of the daemon.
struct Entry {
    /// Its agent, by the name the configuration gives it.
    agent: String,
    phase: Phase,
    /// Held by whoever starts it, so that one does.
    starting: Arc<tokio::sync::Mutex<()>>,
}

/// A session that runs, for whoever asked for it to.
pub(crate) struct Live {
    /// Which of its lives it is in.
    pub(crate) life: u64,
    /// What it tells from its beginning here on, if it was started for
    /// whoever asked and they asked to follow it.
    pub(crate) told: Option<Told>,
}

/// What the requests of a daemon share.
pub(crate) struct Shared {
    pub(crate) limits: Limits,
    /// Whether whoever holds a control key is shown what the tools were
    /// given and what they answered.
    pub(crate) show_tool_text: bool,
    pub(crate) stack: Stack,
    agents: BTreeMap<String, Agent>,
    /// What the tools are called, in the kernel's order.
    pub(crate) tools: Vec<String>,
    pub(crate) keys: Keys,
    pub(crate) tickets: Tickets,
    pub(crate) gate: Gate,
    sessions: Mutex<BTreeMap<SessionId, Entry>>,
    /// Whether the daemon takes nothing new.
    draining: AtomicBool,
    /// Whether the daemon is going away, for whoever follows a session.
    going: watch::Sender<bool>,
}

/// Returns once `flag` is raised, or nobody is left to raise it.
pub(crate) async fn raised(flag: &mut watch::Receiver<bool>) {
    let _ = flag.wait_for(|raised| *raised).await;
}

fn no_session() -> Problem {
    Problem::of(ErrorCode::NoSession, "there is no session of that name")
}

/// What is said to whoever asked when something failed that is the
/// daemon's own. What failed is said where the daemon is run, and not to
/// whoever asked: it may name a file or an address.
pub(crate) fn failed(what: &str, failure: &Failure) -> Problem {
    eprintln!("waveportd: {what}: {failure}");
    Problem::of(ErrorCode::Internal, what)
}

const fn mode_of(plan: &SessionPlan) -> Mode {
    match plan {
        SessionPlan::Cascaded(_) => Mode::Voice,
        SessionPlan::Text(_) => Mode::Text,
    }
}

impl Shared {
    fn sessions(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, Entry>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the daemon takes nothing new.
    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    /// Told when the daemon goes away.
    pub(crate) fn going(&self) -> watch::Receiver<bool> {
        self.going.subscribe()
    }

    fn busy() -> Problem {
        Problem::of(
            ErrorCode::Busy,
            "the daemon is stopping, and takes nothing new",
        )
    }

    /// How long a client token can be connected with.
    fn connect_within(&self) -> Duration {
        Duration::from_millis(u64::from(self.limits.connect_within_ms))
    }

    /// Lets go of the sessions that were made and that nobody came for.
    fn sweep(&self) {
        let now = Instant::now();
        let mut late = Vec::new();
        self.sessions().retain(|id, entry| {
            let stays = !matches!(entry.phase, Phase::Waiting { until } if until <= now);
            if !stays {
                late.push(*id);
            }
            stays
        });
        for id in late {
            self.tickets.revoke(id);
        }
    }

    /// The session `id`, if there is one that somebody can still come
    /// for.
    fn found<T>(&self, id: SessionId, of: impl FnOnce(&mut Entry) -> T) -> Result<T, Problem> {
        let mut sessions = self.sessions();
        let late = sessions.get(&id).is_some_and(
            |entry| matches!(entry.phase, Phase::Waiting { until } if until <= Instant::now()),
        );
        if late {
            sessions.remove(&id);
            drop(sessions);
            self.tickets.revoke(id);
            return Err(no_session());
        }
        sessions.get_mut(&id).map(of).ok_or_else(no_session)
    }

    /// Makes a session of the agent `agent`, and a token for its far end
    /// to connect with.
    pub(crate) fn create(&self, agent: &str) -> Result<SessionCreated, Problem> {
        if self.is_draining() {
            return Err(Self::busy());
        }
        if !self.agents.contains_key(agent) {
            return Err(Problem::of(
                ErrorCode::NoAgent,
                "the daemon has no agent of that name",
            ));
        }
        self.sweep();
        let until = Instant::now() + self.connect_within();
        let id = {
            let mut sessions = self.sessions();
            // A name is the time it was made at, and two sessions may be
            // made at the same time.
            let mut id = name();
            while sessions.contains_key(&id) {
                id = SessionId(id.0.wrapping_add(1));
            }
            let entry = Entry {
                agent: agent.to_owned(),
                phase: Phase::Waiting { until },
                starting: Arc::default(),
            };
            sessions.insert(id, entry);
            id
        };
        self.token_for(id)
    }

    /// A token for one far end to connect to the session `id` with.
    pub(crate) fn mint(&self, id: SessionId) -> Result<SessionCreated, Problem> {
        if self.is_draining() {
            return Err(Self::busy());
        }
        self.found(id, |_| ())?;
        self.token_for(id)
    }

    fn token_for(&self, id: SessionId) -> Result<SessionCreated, Problem> {
        let client_token = self
            .tickets
            .mint(id, self.connect_within())
            .map_err(|failure| failed("no token could be made", &failure))?;
        Ok(SessionCreated {
            session: id.to_string(),
            client_token,
            connect_within_ms: self.limits.connect_within_ms,
        })
    }

    fn info_of(&self, id: SessionId, entry: &Entry) -> SessionInfo {
        let mut info = SessionInfo {
            session: id.to_string(),
            agent: entry.agent.clone(),
            state: SessionState::Waiting,
            mode: None,
            life: None,
            next_seq: None,
        };
        match entry.phase {
            Phase::Waiting { .. } => {}
            Phase::Running { life, mode } => {
                info.state = SessionState::Running;
                info.mode = Some(mode);
                info.life = Some(life);
                info.next_seq = self.stack.status(id).map(|status| status.next_seq.get());
            }
            Phase::Kept => info.state = SessionState::Broken,
        }
        info
    }

    /// What there is to say of the session `id`.
    pub(crate) fn info(&self, id: SessionId) -> Result<SessionInfo, Problem> {
        self.found(id, |entry| self.info_of(id, entry))
    }

    /// The sessions, or the ones that stand as `state` says.
    pub(crate) fn list(&self, state: Option<SessionState>) -> Sessions {
        self.sweep();
        let sessions = self.sessions();
        let sessions = sessions
            .iter()
            .map(|(id, entry)| self.info_of(*id, entry))
            .filter(|info| state.is_none_or(|state| info.state == state))
            .collect();
        Sessions { sessions }
    }

    /// The session `id`, running: it is started if it was made and has
    /// not run, and gone on with if it is kept and does not run. With
    /// `followed`, whoever has it started is given what it tells from
    /// its beginning here on.
    pub(crate) async fn live(
        self: &Arc<Self>,
        id: SessionId,
        followed: bool,
    ) -> Result<Live, Problem> {
        let running = |entry: &mut Entry| match entry.phase {
            Phase::Running { life, .. } => Ok(life),
            Phase::Waiting { .. } => Err((Arc::clone(&entry.starting), entry.agent.clone(), false)),
            Phase::Kept => Err((Arc::clone(&entry.starting), entry.agent.clone(), true)),
        };
        let starting = match self.found(id, running)? {
            Ok(life) => return Ok(Live { life, told: None }),
            Err((starting, ..)) => starting,
        };
        let _one = starting.lock().await;
        // Whoever held it before may have started the session.
        let (name, resumed) = match self.found(id, running)? {
            Ok(life) => return Ok(Live { life, told: None }),
            Err((_, name, resumed)) => (name, resumed),
        };
        if self.is_draining() {
            return Err(Self::busy());
        }
        let Some(agent) = self.agents.get(&name) else {
            return Err(Problem::of(
                ErrorCode::NoAgent,
                "the session is one of an agent the daemon no longer has",
            ));
        };
        let failed = |failure: Failure| failed("the session could not be started", &failure);
        if !resumed {
            // Kept with the session from before its first record, so
            // that whoever goes on with it knows whose it is.
            self.stack.kept.label(id, &name).await.map_err(failed)?;
        }
        let (call, told) = self
            .stack
            .start(agent, id, None, Vec::new(), resumed, followed)
            .await
            .map_err(failed)?;
        let life = call.life();
        let mode = mode_of(call.plan());
        // A session that goes on is followed from its beginning here,
        // which is not its beginning: whoever asked for all of it is
        // told what it has come to instead.
        let told = told.filter(|_| call.rebuilt.is_none());
        let known = self.found(id, |entry| entry.phase = Phase::Running { life, mode });
        if known.is_err() {
            // It was ended while it was being started.
            let _ = self.stack.ask(id, SessionInput::Close);
        }
        // What becomes of it is seen to by whoever waits for it, which
        // does not keep the daemon from stopping.
        let daemon = Arc::downgrade(self);
        tokio::spawn(async move {
            let exit = call.exit().await;
            let Some(daemon) = daemon.upgrade() else {
                return;
            };
            let settled = daemon.stack.settle(id, &exit).await;
            if let Settled::Forgotten(Some(kept)) = &settled {
                eprintln!("waveportd: {kept}");
            }
            let mut sessions = daemon.sessions();
            if settled == Settled::Kept {
                if let Some(entry) = sessions.get_mut(&id) {
                    entry.phase = Phase::Kept;
                }
            } else {
                sessions.remove(&id);
                drop(sessions);
                daemon.tickets.revoke(id);
            }
        });
        Ok(Live { life, told })
    }

    /// Ends the session `id`. Returns whether it is gone already: one
    /// that runs ends once its agent has finished what it is saying.
    pub(crate) async fn end(&self, id: SessionId) -> Result<bool, Problem> {
        let phase = self.found(id, |entry| entry.phase)?;
        self.tickets.revoke(id);
        match phase {
            Phase::Running { .. } => {
                // One that has left meanwhile is ended as well.
                let _ = self.stack.ask(id, SessionInput::Close);
                Ok(false)
            }
            Phase::Waiting { .. } => {
                self.sessions().remove(&id);
                Ok(true)
            }
            Phase::Kept => {
                self.stack
                    .kept
                    .forget(id)
                    .await
                    .map_err(|failure| failed("the session could not be let go of", &failure))?;
                self.sessions().remove(&id);
                Ok(true)
            }
        }
    }

    /// The token that lets the far end of the session `id` come back to
    /// it, in place of the one before.
    pub(crate) async fn credential(&self, id: SessionId) -> Result<ResumeToken, Problem> {
        self.stack
            .kept
            .credential(id)
            .await
            .map_err(|failure| failed("no token could be made", &failure))
    }
}

/// A daemon.
pub struct Daemon;

/// A daemon that runs.
pub struct Running {
    shared: Arc<Shared>,
    address: SocketAddr,
    /// What whoever runs the daemon should know before any session.
    notes: Vec<String>,
    stop: watch::Sender<bool>,
    serving: JoinHandle<()>,
}

impl Daemon {
    /// Starts a daemon as `config` says: puts its agents together, finds
    /// the sessions that are kept, and listens.
    ///
    /// # Errors
    ///
    /// Something `config` names cannot be had: a control key, the file
    /// sessions are kept in, a part of an agent, or the address. A
    /// daemon that is to listen beyond this machine and has no control
    /// key is not started.
    pub async fn start(config: &Config) -> Result<Running, Failure> {
        let keys = Keys::from_env(&config.control.key_env)?;
        if keys.is_empty() && !config.listen.ip().is_loopback() {
            return Err(Failure(format!(
                "the daemon is to listen on {}, which is not this machine alone, and has no \
                 control key: `control.key_env` names the environment variable that holds one",
                config.listen
            )));
        }
        let stack = Stack::build(&config.stack(), true).await?;
        let mut notes = stack.notes.clone();
        let mut agents = BTreeMap::new();
        for (name, spec) in &config.agents {
            let agent = stack
                .agent(spec)
                .await
                .map_err(|Failure(why)| Failure(format!("the agent {name}: {why}")))?;
            notes.extend(agent.notes.iter().map(|note| format!("{name}: {note}")));
            agents.insert(name.clone(), agent);
        }
        // The sessions a process before this one left, each of the
        // agent its label names.
        let mut sessions = BTreeMap::new();
        for (id, label) in stack.kept.labelled().await? {
            let Some(agent) = label else {
                continue;
            };
            let entry = Entry {
                agent,
                phase: Phase::Kept,
                starting: Arc::default(),
            };
            sessions.insert(id, entry);
        }
        match sessions.len() {
            0 => {}
            1 => notes.push(
                "1 session is kept, and is gone on with when its far end comes back".to_owned(),
            ),
            many => notes.push(format!(
                "{many} sessions are kept, and are gone on with when their far ends come back"
            )),
        }
        if keys.is_empty() {
            notes.push(
                "there is no control key: whoever is on this machine may ask everything".to_owned(),
            );
        }
        let listener = TcpListener::bind(config.listen)
            .await
            .with(|| format!("{} cannot be listened on", config.listen))?;
        let address = listener
            .local_addr()
            .with(|| "the address that is listened on cannot be told".to_owned())?;
        let shared = Arc::new(Shared {
            limits: config.limits,
            show_tool_text: config.control.show_tool_text,
            tools: stack.tools(),
            stack,
            agents,
            keys,
            tickets: Tickets::default(),
            gate: Gate::new(address, &config.hosts, &config.allowed_origins),
            sessions: Mutex::new(sessions),
            draining: AtomicBool::new(false),
            going: watch::Sender::new(false),
        });
        let (stop, stopped) = watch::channel(false);
        let router = http::router(&shared);
        let serving = tokio::spawn(serve::serve(listener, router, stopped));
        Ok(Running {
            shared,
            address,
            notes,
            stop,
            serving,
        })
    }
}

impl Running {
    /// The address the daemon listens on.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// What whoever runs the daemon should know before any session.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Stops the daemon: it takes nothing new, lets the answers that are
    /// under way come to their end for as long as the configuration
    /// says, tells whoever follows a session that it is going away, and
    /// stops its sessions where they are. Returns what whoever runs it
    /// should know of how that went.
    ///
    /// A session that is kept in a file is gone on with by the daemon
    /// that is started next, when its far end comes back.
    pub async fn drain(self) -> Vec<String> {
        let Self {
            shared,
            stop,
            mut serving,
            ..
        } = self;
        let mut notes = Vec::new();
        shared.draining.store(true, Ordering::Relaxed);
        // The runtime starts nothing more either. How many sessions it
        // has is not what is waited for: one that rests is not over.
        let running = shared.stack.drain(Duration::ZERO).await;
        let patience = Duration::from_millis(u64::from(shared.limits.drain_ms));
        let until = Instant::now() + patience;
        while !shared.stack.at_rest() && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !shared.stack.at_rest() {
            notes.push(format!(
                "an answer was still under way after {} ms, and was stopped where it was",
                patience.as_millis()
            ));
        }
        if running > 0 {
            let kept = if shared.stack.kept.outlasts_the_program() {
                "what they had come to is kept, and they are gone on with when their far \
                 ends come back"
            } else {
                "they were kept in memory, and are gone"
            };
            notes.push(format!("sessions that were stopped: {running}. {kept}"));
        }
        // Whoever follows a session is told, and the connections close.
        shared.going.send_replace(true);
        stop.send_replace(true);
        if tokio::time::timeout(LAST_WORDS, &mut serving)
            .await
            .is_err()
        {
            serving.abort();
            let _ = serving.await;
            notes.push("a connection did not close, and was dropped".to_owned());
        }
        if let Some(lost) = shared.stack.finish().await {
            notes.push(lost);
        }
        // The sessions stop when the last of what holds them is let go
        // of, which closes the file they are kept in. A request that is
        // still on its way out holds them for a moment.
        let mut shared = Some(shared);
        for _ in 0..50 {
            match shared.take().map(Arc::try_unwrap) {
                Some(Ok(last)) => {
                    // Its threads are waited for, which is not for a
                    // thread that serves.
                    let _ = tokio::task::spawn_blocking(move || drop(last)).await;
                }
                Some(Err(held)) => {
                    shared = Some(held);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                None => break,
            }
        }
        notes
    }
}
