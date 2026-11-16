//! One session: the kernel, the audio path, and the parts they reach the
//! world through.
//!
//! A session is one task. Each time it runs it looks at every part once,
//! in a fixed order, and collects what happened: how far playback has
//! got, timers that are due, what the transport brought, what the
//! application asked for, what the listening models and the recogniser heard,
//! what the language model wrote, what the synthesiser spoke, what tools
//! answered, and what the journal stored. Then it gives the kernel each
//! of those as an event and carries out what the kernel asks, and hands
//! the transport the frames that are due. When nothing more can move it
//! sleeps until a part wakes it or the next deadline comes.
//!
//! Nothing in a session waits on anything else in it. There is no queue
//! between two of its parts for an interruption to race through: when the
//! kernel says stop, the call that stops playback is an ordinary function
//! call, made before anything else happens.
//!
//! The order in which the parts are looked at is fixed and nothing here
//! is random, so a session on a clock that a test controls does the same
//! thing every time.

mod hear;
mod speak;
mod store;
mod think;

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::{Instant, Sleep};
use waveport_domain::command::{Command, LlmCmd, PlayoutCmd, SenseCmd, SttCmd, ToolCmd, TtsCmd};
use waveport_domain::config::{ConfigError, KernelConfig};
use waveport_domain::event::{CloseReason, ControlInput, Event, TimerId, ToolEvent};
use waveport_domain::fact::{Fact, Lifecycle, Stamped};
use waveport_domain::ids::{ParticipantId, Seq, ToolCallId, ToolId, TurnId};
use waveport_domain::kernel::{Kernel, RestoreError, Snapshot, View};
use waveport_domain::out::Out;
use waveport_domain::policy::{DefaultPolicies, Policies};
use waveport_domain::text::TextArena;
use waveport_domain::time::{MediaTime, MonoTime};
use waveport_media::codec::CodecError as MediaError;
use waveport_media::ingress::Packet;
use waveport_ports::audio::Rate;
pub use waveport_ports::control::{SessionEvent, SessionInput};
use waveport_ports::error::{BoxFuture, Full};
use waveport_ports::journal::{Journal, JournalError, Recovered, SessionId};
use waveport_ports::llm::Llm;
use waveport_ports::plan::{AgentNeeds, BrainProfile, CascadePlan, PlanError, SessionPlan, plan};
use waveport_ports::stt::{Stt, SttRx};
use waveport_ports::telemetry::{EventSink, Stage, StageMark};
use waveport_ports::tool::{ToolExecutor, ToolReply, ToolRequest};
use waveport_ports::transport::{
    FrameOut, Transport, TransportCaps, TransportControl, TransportEvent,
};
use waveport_ports::tts::Tts;

use self::hear::Ear;
pub use self::hear::HearStats;
pub use self::speak::SpeakStats;
use self::speak::{Milestone, Voice};
use self::store::Store;
pub use self::store::{StateStats, StoreBroken};
use self::think::Mind;
use crate::codec::{self, CodecError};
use crate::compute::{Compute, Wants};
use crate::context::{AgentDef, Asking, ToolDef};
use crate::edges::EdgeConfig;

/// How many times a session looks at its parts before it lets other
/// sessions run, when each look finds more to do.
const SWEEPS: usize = 32;

/// How many of the last records a session holds for those who start to
/// follow it, or come back to it.
const RECENT: usize = 128;

/// How far past the end of what an earlier life of the session heard the
/// clock of a resumed session starts.
const RESUME_GAP: Duration = Duration::from_secs(1);

/// How long a session that has closed waits for its transport to have
/// told the far end, and for its recognisers to have been let go of. A
/// far end that does not answer a goodbye is not worth more, and neither
/// is a recogniser: the session is over either way.
const FAREWELL: Duration = Duration::from_millis(500);

/// What a session is, beyond its parts.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSpec {
    /// Its name in the journal.
    pub id: SessionId,
    /// The kernel's configuration.
    pub kernel: KernelConfig,
    /// Its agents, in the kernel's order.
    pub agents: Vec<AgentDef>,
    /// Its tools, in the kernel's order.
    pub tools: Vec<ToolDef>,
    /// The language spoken, if it is known, as the recogniser and the
    /// synthesiser are told it.
    pub language: Option<String>,
    /// How the answers of the model that listens for speech are judged.
    pub edges: EdgeConfig,
    /// When the session's state is stored in its journal.
    pub states: StateRule,
}

/// When a session has its journal store its state, so that the session
/// can be rebuilt from there and the records before it let go of.
///
/// A state is due once this many records were written since the last
/// one, or this long has passed since and anything was recorded. It is
/// taken when a turn or an utterance next enters the conversation, which
/// is when a session holds least that is only under way. A session in
/// which neither happens takes it once twice as many records wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateRule {
    /// How many records are written before a state is due.
    pub records: u64,
    /// How long after the last state the next is due.
    pub after: Duration,
}

impl StateRule {
    /// No state is ever stored: a session is rebuilt from all its
    /// records.
    pub const NEVER: Self = Self {
        records: u64::MAX,
        after: Duration::MAX,
    };
}

impl Default for StateRule {
    /// Every 256 records, or every minute.
    fn default() -> Self {
        Self {
            records: 256,
            after: Duration::from_secs(60),
        }
    }
}

impl SessionSpec {
    /// A session with one agent that has these instructions and no tools.
    #[must_use]
    pub fn new(id: SessionId, instructions: &str) -> Self {
        Self {
            id,
            kernel: KernelConfig::default(),
            agents: vec![AgentDef {
                instructions: instructions.to_owned(),
            }],
            tools: Vec::new(),
            language: None,
            edges: EdgeConfig::default(),
            states: StateRule::default(),
        }
    }
}

/// The parts a session reaches the world through.
pub struct Parts {
    /// The connection to its participants.
    pub transport: Box<dyn Transport>,
    /// The speech recogniser.
    pub stt: Arc<dyn Stt>,
    /// The language model.
    pub llm: Arc<dyn Llm>,
    /// The speech synthesiser.
    pub tts: Arc<dyn Tts>,
    /// What runs its tools.
    pub tools: Arc<dyn ToolExecutor>,
    /// Where its facts are kept.
    pub journal: Arc<dyn Journal>,
    /// The listening models.
    pub compute: Arc<Compute>,
    /// Who is told what happens in it, for timing and for the record.
    pub sink: Option<Arc<dyn EventSink>>,
}

impl fmt::Debug for Parts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parts").finish_non_exhaustive()
    }
}

/// What reaches a session from outside it while it runs.
#[derive(Debug)]
pub enum Request {
    /// Something is asked of it.
    Input(SessionInput),
    /// Someone wants to be told what happens in it, from the fact
    /// numbered `from` on.
    Follow {
        /// The number of the first fact wanted.
        from: Seq,
        /// Where to. A follower that has no room for what it is told is
        /// let go, and comes back for what it missed.
        to: mpsc::Sender<SessionEvent>,
    },
}

impl From<SessionInput> for Request {
    fn from(input: SessionInput) -> Self {
        Self::Input(input)
    }
}

/// Why a session could not be started.
#[derive(Clone, Debug, PartialEq)]
pub enum StartError {
    /// The kernel's configuration is unusable.
    Config(ConfigError),
    /// The agents or tools given are not those of the kernel's
    /// configuration, one for one.
    Defs,
    /// The parts cannot run a session together.
    Plan(PlanError),
    /// The listening models want audio at another rate than the session
    /// hears at.
    ModelRate {
        /// The rate the models take.
        models: Rate,
        /// The rate the session hears at.
        session: Rate,
    },
    /// The codec of the transport cannot be set up.
    Codec(MediaError),
    /// The journal could not be read.
    Journal(JournalError),
    /// A record of the journal could not be read.
    Record(CodecError),
    /// The state the journal holds of the session could not be read.
    State(CodecError),
    /// The state the journal holds says it is as of another record than
    /// the journal says.
    StateMoved {
        /// The record the journal says the state is as of.
        journal: Seq,
        /// The record the state says it is as of.
        state: Seq,
    },
    /// The state and the records of the journal do not make a session.
    Restore(RestoreError),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(f, "the session's configuration is unusable: {error}"),
            Self::Defs => f.write_str(
                "the agents and tools described are not those of the session's configuration",
            ),
            Self::Plan(error) => write!(f, "the parts cannot run a session together: {error}"),
            Self::ModelRate { models, session } => write!(
                f,
                "the listening models take audio at {} Hz and the session hears at {} Hz",
                models.hz(),
                session.hz()
            ),
            Self::Codec(error) => write!(f, "the transport's codec cannot be set up: {error}"),
            Self::Journal(error) => write!(f, "the journal could not be read: {error}"),
            Self::Record(error) => write!(f, "a record of the journal could not be read: {error}"),
            Self::State(error) => write!(
                f,
                "the state the journal holds of the session could not be read: {error}"
            ),
            Self::StateMoved { journal, state } => write!(
                f,
                "the journal holds a state as of record {}, which says it is as of record {}",
                journal.get(),
                state.get()
            ),
            Self::Restore(error) => write!(f, "the journal does not hold a session: {error}"),
        }
    }
}

impl core::error::Error for StartError {}

/// What a session that goes on after an earlier life was rebuilt from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rebuilt {
    /// The record the stored state it began from is as of, if its
    /// journal held a state.
    pub state: Option<Seq>,
    /// How many records after that it read.
    pub records: u64,
}

/// How a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It closed, and everything it decided is in the journal.
    Closed,
    /// It could not write its facts any more, and stopped where it was.
    Lost(StoreBroken),
}

/// Something that is shown every step of a session: the event, what the
/// kernel made of it, and the kernel afterwards. A recorder for replay is
/// one, and so is whatever tells clients what the session does.
pub trait Tap: Send {
    /// The kernel took `event` at `now` and left `out`.
    fn stepped(&mut self, now: MonoTime, event: &Event, out: &Out, view: View<'_>);
}

/// What the agents and tools of a session are called and told.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Defs {
    pub agents: Vec<AgentDef>,
    pub tools: Vec<ToolDef>,
}

/// Counts of what went wrong in a session that it could carry on from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// Participants whose audio could not be set up, or for whom there
    /// was no room.
    pub unheard: u32,
    /// Packets from someone who had not joined.
    pub strays: u64,
    /// What went wrong while hearing, over all participants who have
    /// left.
    pub hearing: HearStats,
    /// What went wrong while speaking.
    pub speaking: SpeakStats,
    /// How the states the session gave its journal fared.
    pub states: StateStats,
}

type Calling = BoxFuture<'static, ToolReply>;
type Undoing = BoxFuture<'static, bool>;

/// One session.
pub struct Session<P: Policies = DefaultPolicies> {
    id: SessionId,
    /// The reading of the clock at which the session's time is `offset`.
    origin: Instant,
    offset: Duration,
    kernel: Kernel<P>,
    out: Out,
    plan: CascadePlan,
    defs: Arc<Defs>,
    edges: EdgeConfig,
    language: Option<String>,
    wants: Wants,

    transport: Box<dyn Transport>,
    transport_open: bool,
    stt: Arc<dyn Stt>,
    llm: Arc<dyn Llm>,
    tools: Arc<dyn ToolExecutor>,
    compute: Arc<Compute>,
    sink: Option<Arc<dyn EventSink>>,

    ears: Vec<Ear>,
    minds: Vec<Mind>,
    voice: Voice,
    calls: Vec<(ToolCallId, Calling)>,
    undoing: Vec<(ToolCallId, Undoing)>,
    timers: Vec<(TimerId, MonoTime)>,
    store: Store,
    inbox: Option<mpsc::Receiver<Request>>,
    /// The last records, with their numbers.
    recent: VecDeque<(Seq, Bytes)>,
    followers: Vec<mpsc::Sender<SessionEvent>>,

    /// What happened, for the kernel to be told, in the order it is told.
    queue: Vec<Event>,
    /// What the session itself has to tell the kernel after a step.
    next: Vec<Event>,
    /// Whether the kernel has been told that the parts of an agent that
    /// is to take the floor are ready.
    stage_reported: bool,
    taps: Vec<Box<dyn Tap>>,
    sleep: Pin<Box<Sleep>>,
    /// Recognisers of participants who are no longer listened to, told
    /// that no more audio follows and read until they end. What they
    /// still write down is for nobody.
    parting: Vec<Pin<Box<dyn SttRx>>>,
    /// Until when the session, which has closed, waits for its transport
    /// to have said so to the far end and for its recognisers to end.
    leaving: Option<MonoTime>,
    /// Whether the transport has said so.
    far_end_told: bool,
    /// When the session's state is stored.
    rule: StateRule,
    /// Where the last audio ends that a turn was spoken in.
    heard_until: MediaTime,
    /// The last fact the newest state includes, and when it was taken.
    state_upto: Seq,
    state_at: MonoTime,
    /// What the session was rebuilt from, if it goes on after an earlier
    /// life.
    rebuilt: Option<Rebuilt>,
    stats: SessionStats,
}

impl<P: Policies> Session<P> {
    /// Takes the session over in its journal and makes it ready to run:
    /// a new one if the journal has nothing of it, and otherwise the one
    /// the journal's records add up to.
    ///
    /// # Errors
    ///
    /// The session cannot be run with these parts, or its journal cannot
    /// be read.
    pub async fn open(spec: SessionSpec, parts: Parts, policies: P) -> Result<Self, StartError> {
        // The session's clock starts when it is asked for, not when its
        // journal has answered.
        let origin = Instant::now();
        spec.kernel.validate().map_err(StartError::Config)?;
        if spec.agents.len() != spec.kernel.agents.len()
            || spec.tools.len() != spec.kernel.tools.len()
        {
            return Err(StartError::Defs);
        }
        let brain = BrainProfile::Cascaded {
            hear: parts.stt.caps(),
            think: parts.llm.caps(),
            speak: parts.tts.caps(),
        };
        let most_tools = spec
            .kernel
            .agents
            .iter()
            .map(|agent| agent.tools.len())
            .max();
        let needs = AgentNeeds {
            tools: most_tools.unwrap_or(0),
        };
        let SessionPlan::Cascaded(plan) =
            plan(&needs, &brain, &parts.transport.caps()).map_err(StartError::Plan)?;
        let wants = parts.compute.wants();
        if wants.rate != plan.listen.rate {
            return Err(StartError::ModelRate {
                models: wants.rate,
                session: plan.listen.rate,
            });
        }
        let voice = Voice::new(plan.speak, plan.playback, parts.tts, spec.language.clone())
            .map_err(StartError::Codec)?;

        let recovered = parts
            .journal
            .acquire(spec.id)
            .await
            .map_err(StartError::Journal)?;
        let mut next = Vec::new();
        let (kernel, earlier) = rebuild(&spec.kernel, policies, &recovered)?;
        let rebuilt = earlier.map(|earlier| earlier.from);
        let heard_until = earlier.map(|earlier| earlier.heard);
        if earlier.is_some() {
            // The first step of a session that was rebuilt deals with
            // what its earlier life left unfinished. This makes sure
            // there is one.
            next.push(Event::Durable(Seq(recovered.next.get().saturating_sub(1))));
        }
        // A session that is rebuilt goes on a little after the end of
        // what its earlier life heard.
        let offset = heard_until.map_or(Duration::ZERO, |heard| since_start(heard) + RESUME_GAP);
        let began = MonoTime::from_nanos(u64::try_from(offset.as_nanos()).unwrap_or(u64::MAX));
        let store = Store::new(parts.journal, spec.id, recovered.lease, recovered.next);
        let recent = last_of(recovered.tail, recovered.next);
        Ok(Self {
            id: spec.id,
            origin,
            offset,
            kernel,
            out: Out::new(),
            plan,
            defs: Arc::new(Defs {
                agents: spec.agents,
                tools: spec.tools,
            }),
            edges: spec.edges,
            language: spec.language,
            wants,
            transport: parts.transport,
            transport_open: true,
            stt: parts.stt,
            llm: parts.llm,
            tools: parts.tools,
            compute: parts.compute,
            sink: parts.sink,
            ears: Vec::new(),
            minds: Vec::new(),
            voice,
            calls: Vec::new(),
            undoing: Vec::new(),
            timers: Vec::new(),
            store,
            inbox: None,
            recent,
            followers: Vec::new(),
            queue: Vec::new(),
            next,
            stage_reported: false,
            taps: Vec::new(),
            sleep: Box::pin(tokio::time::sleep_until(origin)),
            parting: Vec::new(),
            leaving: None,
            far_end_told: false,
            rule: spec.states,
            heard_until: heard_until.unwrap_or(MediaTime::from_ticks(0)),
            state_upto: rebuilt.and_then(|from| from.state).unwrap_or(Seq(0)),
            state_at: began,
            rebuilt,
            stats: SessionStats::default(),
        })
    }

    /// The session's name in the journal.
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// How the session is run with the parts it has, and what it does
    /// less well for them.
    #[must_use]
    pub const fn plan(&self) -> &CascadePlan {
        &self.plan
    }

    /// What the session was rebuilt from, if its journal held anything of
    /// it when it was started.
    #[must_use]
    pub const fn rebuilt(&self) -> Option<Rebuilt> {
        self.rebuilt
    }

    /// A read-only look at the session.
    #[must_use]
    pub fn view(&self) -> View<'_> {
        self.kernel.view()
    }

    /// What the session's facts add up to: what a session rebuilt from
    /// its journal holds, once the journal has every fact.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.kernel.snapshot()
    }

    /// What went wrong so far that the session could carry on from.
    #[must_use]
    pub fn stats(&self) -> SessionStats {
        let mut stats = self.stats;
        stats.speaking = self.voice.stats();
        stats.states = self.store.states();
        for ear in &self.ears {
            let hearing = ear.stats();
            stats.hearing.recogniser_failures += hearing.recogniser_failures;
            stats.hearing.finalize_refused += hearing.finalize_refused;
        }
        stats
    }

    /// Shows `tap` every step from now on.
    pub fn tap(&mut self, tap: Box<dyn Tap>) {
        self.taps.push(tap);
    }

    /// Takes what is asked of the session from `inbox`.
    pub fn listen_to(&mut self, inbox: mpsc::Receiver<Request>) {
        self.inbox = Some(inbox);
    }

    /// Tells `to` what happens in the session from now on, after the
    /// facts from the one numbered `from` that it still holds. If it
    /// holds them no longer, `to` is told first where what follows
    /// begins.
    pub fn follow(&mut self, from: Seq, to: mpsc::Sender<SessionEvent>) {
        if let Some((oldest, _)) = self.recent.front()
            && from < *oldest
            && to.try_send(SessionEvent::Gap { resumes: *oldest }).is_err()
        {
            return;
        }
        for (seq, record) in &self.recent {
            if *seq < from {
                continue;
            }
            let fact = SessionEvent::Fact {
                seq: *seq,
                record: record.clone(),
            };
            if to.try_send(fact).is_err() {
                return;
            }
        }
        self.followers.push(to);
    }

    /// Tells every follower something. One that has no room for it, or
    /// has gone, is let go.
    fn tell(&mut self, event: &SessionEvent) {
        self.followers
            .retain(|to| to.try_send(event.clone()).is_ok());
    }

    /// Runs the session to its end.
    pub async fn run(&mut self) -> Ended {
        core::future::poll_fn(|cx| self.poll_run(cx)).await
    }

    /// The session's time now.
    fn now(&self) -> MonoTime {
        let since = Instant::now().saturating_duration_since(self.origin) + self.offset;
        MonoTime::from_nanos(u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
    }

    /// The reading of the clock at the session's time `at`.
    fn instant(&self, at: MonoTime) -> Instant {
        let since = Duration::from_nanos(at.as_nanos()).saturating_sub(self.offset);
        self.origin + since
    }

    /// Runs the session until it has nothing to do or has ended.
    pub fn poll_run(&mut self, cx: &mut Context<'_>) -> Poll<Ended> {
        for _ in 0..SWEEPS {
            let now = self.now();
            let mut progressed = self.gather(cx, now);
            let mut queue = core::mem::take(&mut self.queue);
            progressed |= !queue.is_empty();
            for event in queue.drain(..) {
                self.step(now, &event);
            }
            self.queue = queue;
            progressed |= self.send_frames(now);
            if let Some(ended) = self.ended() {
                match ended {
                    Ended::Lost(_) => self.fall_silent(now),
                    Ended::Closed if !self.farewell(cx, now) => return Poll::Pending,
                    Ended::Closed => {}
                }
                return Poll::Ready(ended);
            }
            if progressed || !self.next.is_empty() {
                continue;
            }
            let Some(at) = self.deadline() else {
                return Poll::Pending;
            };
            // Everything due by `now` has been done. A deadline that says
            // otherwise is waited for all the same, so that a part that
            // is wrong about its own deadline cannot spin the session.
            let at = at.max(now.after(Duration::from_millis(1)));
            let at = self.instant(at);
            self.sleep.as_mut().reset(at);
            if self.sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
        // There is still more to do. Others get a turn first.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    /// Stops everything the session was doing, where it is. The kernel of
    /// a session that has lost its journal asks for nothing more, not
    /// even for silence, so this is done without it. Tools are not asked
    /// to take anything back: whoever has the session now decides that.
    fn fall_silent(&mut self, now: MonoTime) {
        let fade_ms = self.kernel.config().timing.stop_fade_ms;
        self.voice.halt(fade_ms, now, self.transport.as_mut());
        self.minds.clear();
        self.calls.clear();
        self.undoing.clear();
        self.ears.clear();
        self.parting.clear();
        self.timers.clear();
    }

    /// Has the transport tell the far end that the session, which has
    /// closed, is over, and lets go of the recognisers. Returns whether
    /// that is done with: it is said and they have ended, or it has been
    /// waited for long enough.
    fn farewell(&mut self, cx: &mut Context<'_>, now: MonoTime) -> bool {
        let until = *self.leaving.get_or_insert_with(|| now.after(FAREWELL));
        // Whoever is still listened to is not any more.
        for ear in core::mem::take(&mut self.ears) {
            self.let_go(ear);
        }
        self.far_end_told = self.far_end_told || self.transport.poll_close(cx).is_ready();
        self.part(cx);
        if now >= until || (self.far_end_told && self.parting.is_empty()) {
            return true;
        }
        let at = self.instant(until);
        self.sleep.as_mut().reset(at);
        self.sleep.as_mut().poll(cx).is_ready()
    }

    /// Stops listening to a participant: what went wrong while they were
    /// heard is kept, and their recogniser is told that no more audio
    /// follows.
    fn let_go(&mut self, ear: Ear) {
        let (hearing, left) = ear.leave();
        self.stats.hearing.recogniser_failures += hearing.recogniser_failures;
        self.stats.hearing.finalize_refused += hearing.finalize_refused;
        self.parting.extend(left);
    }

    /// Reads the recognisers that were told that no more audio follows,
    /// and lets go of the ones that have ended.
    fn part(&mut self, cx: &mut Context<'_>) {
        self.parting.retain_mut(|left| {
            loop {
                match left.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(_))) => {}
                    Poll::Ready(Some(Err(_)) | None) => return false,
                    Poll::Pending => return true,
                }
            }
        });
    }

    /// How the session has ended, if it has.
    fn ended(&self) -> Option<Ended> {
        if let Some(broken) = self.store.broken() {
            return Some(Ended::Lost(broken.clone()));
        }
        (self.kernel.view().lifecycle() == Lifecycle::Closed && self.store.is_idle())
            .then_some(Ended::Closed)
    }

    /// The next time something is due that no part will wake the session
    /// for.
    fn deadline(&self) -> Option<MonoTime> {
        let timers = self.timers.iter().map(|(_, at)| *at);
        let ears = self.ears.iter().filter_map(Ear::deadline);
        timers
            .chain(ears)
            .chain(self.voice.deadline())
            .chain(self.store.deadline())
            .min()
    }

    /// Looks at every part once and collects what happened in `queue`.
    /// Returns whether a part changed in a way that may have made more
    /// work, beyond what is in the queue.
    fn gather(&mut self, cx: &mut Context<'_>, now: MonoTime) -> bool {
        let mut changed = false;
        // How far playback has got comes first, so that whatever else
        // happened is decided on what had been heard by then.
        if let Some(position) = self.voice.position(now) {
            self.queue.push(position);
        }
        self.queue.append(&mut self.next);

        // Timers, the earliest first.
        while let Some(index) = self
            .timers
            .iter()
            .enumerate()
            .filter(|(_, (_, at))| *at <= now)
            .min_by_key(|(_, (_, at))| *at)
            .map(|(index, _)| index)
        {
            let (id, _) = self.timers.remove(index);
            self.queue.push(Event::Timer(id));
        }

        // What the transport brought comes before what the application
        // asks. A request is about the session as its world has made it:
        // text typed for someone who is on the line already has to find
        // them there, also when both are met in the same run.
        self.poll_transport(cx, now);

        let mut follows = Vec::new();
        if let Some(inbox) = &mut self.inbox {
            loop {
                match inbox.poll_recv(cx) {
                    Poll::Ready(Some(Request::Input(input))) => self.queue.push(asked(input)),
                    Poll::Ready(Some(Request::Follow { from, to })) => follows.push((from, to)),
                    Poll::Ready(None) => {
                        self.inbox = None;
                        break;
                    }
                    Poll::Pending => break,
                }
            }
        }
        for (from, to) in follows {
            self.follow(from, to);
        }

        for ear in &mut self.ears {
            changed |= ear.poll(cx, now, &mut self.queue);
        }
        self.part(cx);

        for index in 0..self.minds.len() {
            let Some(mind) = self.minds.get_mut(index) else {
                break;
            };
            let was_silent = mind.silent;
            changed |= mind.poll(cx, &self.defs, &mut self.queue);
            if was_silent && !mind.silent {
                let stage = Stage::FirstToken(mind.tag.generation);
                mark(self.sink.as_ref(), self.id, now, stage);
            }
        }
        self.minds.retain(|mind| !mind.is_over());

        let (sink, id) = (self.sink.as_ref(), self.id);
        changed |= self.voice.poll(cx, now, &mut self.queue, &mut |milestone| {
            if let Milestone::FirstAudio(tag) = milestone {
                mark(sink, id, now, Stage::FirstAudio(tag.generation));
            }
        });

        self.poll_tools(cx, now);
        changed |= self.store.poll(cx, now, &mut self.queue);
        changed
    }

    fn poll_transport(&mut self, cx: &mut Context<'_>, now: MonoTime) {
        while self.transport_open {
            let Poll::Ready(event) = self.transport.poll_event(cx) else {
                break;
            };
            match event {
                Some(TransportEvent::Joined { who }) => {
                    self.joined(who);
                    self.queue.push(Event::Control(ControlInput::Join { who }));
                }
                Some(TransportEvent::Left { who }) => {
                    self.queue.push(Event::Control(ControlInput::Leave { who }));
                }
                Some(TransportEvent::Audio {
                    who,
                    sequence,
                    timestamp,
                    payload,
                }) => {
                    let packet = Packet {
                        sequence,
                        timestamp,
                        payload: &payload,
                    };
                    match self.ears.iter_mut().find(|ear| ear.who == who) {
                        Some(ear) => ear.packet(now, packet),
                        None => self.stats.strays += 1,
                    }
                }
                Some(TransportEvent::Played { tag, played }) => self.voice.acked(tag, played),
                Some(TransportEvent::Text { who, text }) => self.queue.push(Event::TextIn {
                    who,
                    text: text.into(),
                }),
                Some(TransportEvent::Detached) => {
                    self.queue.push(Event::Control(ControlInput::Detached));
                }
                Some(TransportEvent::Attached) => {
                    self.queue.push(Event::Control(ControlInput::Attached));
                }
                // The transport is gone, and with it everyone the session
                // could talk to. The session is closed first, so that it
                // starts nothing more, and then what the agent was saying
                // is cut off where it was heard to: nobody hears the rest,
                // and a response begun now would be heard by nobody at
                // all.
                None => {
                    self.transport_open = false;
                    self.transport = Box::new(Gone(self.transport.caps()));
                    self.queue.push(Event::Control(ControlInput::Detached));
                    let reason = CloseReason::Abandoned;
                    self.queue
                        .push(Event::Control(ControlInput::Close { reason }));
                    self.queue.push(Event::Control(ControlInput::Interrupt));
                }
            }
        }
    }

    /// A participant is there: from now on they are heard.
    fn joined(&mut self, who: ParticipantId) {
        if self.ears.iter().any(|ear| ear.who == who) {
            return;
        }
        if self.ears.len() >= self.kernel.config().limits.participants {
            self.stats.unheard += 1;
            return;
        }
        let ear = Ear::new(
            who,
            self.plan.listen,
            MonoTime::ZERO,
            self.compute.listen(),
            self.edges,
            Arc::clone(&self.stt),
            self.language.clone(),
        );
        match ear {
            Ok(ear) => self.ears.push(ear),
            Err(_) => self.stats.unheard += 1,
        }
    }

    fn poll_tools(&mut self, cx: &mut Context<'_>, now: MonoTime) {
        let mut index = 0;
        while let Some((call, calling)) = self.calls.get_mut(index) {
            let Poll::Ready(reply) = calling.as_mut().poll(cx) else {
                index += 1;
                continue;
            };
            let call = *call;
            drop(self.calls.remove(index));
            mark(self.sink.as_ref(), self.id, now, Stage::ToolBack(call));
            let event = match reply {
                ToolReply::Completed(result) => ToolEvent::Completed {
                    result: result.into(),
                },
                ToolReply::Failed { class, message } => ToolEvent::Failed {
                    class,
                    message: message.into(),
                },
            };
            self.queue.push(Event::Tool(call, event));
        }
        let mut index = 0;
        while let Some((call, undoing)) = self.undoing.get_mut(index) {
            let Poll::Ready(undone) = undoing.as_mut().poll(cx) else {
                index += 1;
                continue;
            };
            let call = *call;
            drop(self.undoing.remove(index));
            if undone {
                self.queue.push(Event::Tool(call, ToolEvent::RolledBack));
            }
        }
    }

    /// Hands the frames that are due to the transport.
    fn send_frames(&mut self, now: MonoTime) -> bool {
        let (sink, id) = (self.sink.as_ref(), self.id);
        self.voice
            .send_frames(now, self.transport.as_mut(), &mut |milestone| {
                if let Milestone::FirstFrame(tag) = milestone {
                    mark(sink, id, now, Stage::FirstFrameOut(tag.generation));
                }
            })
    }

    /// Gives the kernel one event and carries out what it asks.
    fn step(&mut self, now: MonoTime, event: &Event) {
        if self.sink.is_some()
            && let Event::Transcript {
                who,
                is_final: true,
                ..
            } = event
            && let Some(turn) = self.kernel.view().turns().find(|turn| turn.who == *who)
        {
            mark(
                self.sink.as_ref(),
                self.id,
                now,
                Stage::TranscriptFinal(turn.turn),
            );
        }
        if let Event::Durable(upto) = event {
            mark(self.sink.as_ref(), self.id, now, Stage::Stored(*upto));
        }

        let mut out = core::mem::take(&mut self.out);
        self.kernel.step(now, event, &mut out);
        for tap in &mut self.taps {
            tap.stepped(now, event, &out, self.kernel.view());
        }
        if !out.facts.is_empty() {
            for stamped in &out.facts {
                if let Some(record) = self.store.record(stamped, &out.text) {
                    if self.recent.len() >= RECENT {
                        self.recent.pop_front();
                    }
                    self.recent.push_back((stamped.seq, record.clone()));
                    let seq = stamped.seq;
                    self.tell(&SessionEvent::Fact { seq, record });
                }
                if let Fact::TurnCommitted { turn, span, .. } = stamped.fact {
                    mark(self.sink.as_ref(), self.id, now, Stage::TurnEnded(turn));
                    self.heard_until = self.heard_until.max(span.end);
                }
            }
            if let Some(sink) = &self.sink {
                sink.facts(self.id, now, &out.facts, &out.text);
            }
            self.keep_state(now, &out.facts);
        }
        for command in &out.cmds {
            self.carry_out(now, command, &out.text);
        }
        self.out = out;

        // An agent that is to take the floor waits for its parts. Every
        // agent of a session shares the session's parts, so they are
        // ready as soon as they are asked for.
        if self.kernel.view().handoff().is_none() {
            self.stage_reported = false;
        } else if !self.stage_reported {
            self.stage_reported = true;
            self.next.push(Event::Control(ControlInput::StageReady));
        }
    }

    fn carry_out(&mut self, now: MonoTime, command: &Command, text: &TextArena) {
        match *command {
            Command::Llm(tag, LlmCmd::Start { agent, cause, .. }) => {
                let asking = Asking::new(self.kernel.view(), agent, cause);
                let mind = Mind::start(tag, asking, Arc::clone(&self.llm), Arc::clone(&self.defs));
                self.minds.retain(|mind| mind.tag != tag);
                self.minds.push(mind);
                mark(
                    self.sink.as_ref(),
                    self.id,
                    now,
                    Stage::Asked(tag.generation),
                );
            }
            Command::Llm(tag, LlmCmd::Pause) => {
                if let Some(mind) = self.minds.iter_mut().find(|mind| mind.tag == tag) {
                    mind.paused = true;
                }
            }
            Command::Llm(tag, LlmCmd::Resume) => {
                if let Some(mind) = self.minds.iter_mut().find(|mind| mind.tag == tag) {
                    mind.paused = false;
                }
            }
            // Dropping the stream is all there is to cancelling it.
            Command::Llm(tag, LlmCmd::Cancel) => self.minds.retain(|mind| mind.tag != tag),
            Command::Tts(tag, TtsCmd::Speak { text: piece }) => {
                if self.voice.speak(tag, text.get(piece)) {
                    let stage = Stage::FirstText(tag.generation);
                    mark(self.sink.as_ref(), self.id, now, stage);
                }
            }
            Command::Tts(tag, TtsCmd::Finish) => self.voice.finish(tag),
            Command::Tts(tag, TtsCmd::Cancel) => self.voice.cancel(tag),
            Command::Stt(SttCmd::Finalize { who }) => {
                if let Some(ear) = self.ears.iter_mut().find(|ear| ear.who == who) {
                    ear.finalize();
                }
            }
            Command::Stt(SttCmd::Close { who }) => {
                if let Some(index) = self.ears.iter().position(|ear| ear.who == who) {
                    let ear = self.ears.remove(index);
                    self.let_go(ear);
                }
            }
            Command::Sense(SenseCmd::EstimateEou { turn, who }) => {
                self.judge(turn, who, self.wants.turn_window, false);
            }
            Command::Sense(SenseCmd::ClassifyOverlap { turn, who }) => {
                if let Some(window) = self.wants.overlap_window {
                    self.judge(turn, who, window, true);
                }
            }
            Command::Playout(tag, PlayoutCmd::Start) => self.voice.start(tag, now),
            Command::Playout(tag, PlayoutCmd::Duck) => {
                self.voice.duck(tag);
                mark(
                    self.sink.as_ref(),
                    self.id,
                    now,
                    Stage::Lowered(tag.generation),
                );
            }
            Command::Playout(tag, PlayoutCmd::Unduck) => self.voice.unduck(tag),
            Command::Playout(tag, PlayoutCmd::Stop { fade_ms }) => {
                self.voice.stop(tag, fade_ms, now, self.transport.as_mut());
                mark(
                    self.sink.as_ref(),
                    self.id,
                    now,
                    Stage::Stopped(tag.generation),
                );
            }
            Command::Tool(
                call,
                ToolCmd::Dispatch {
                    tool,
                    args,
                    attempt,
                },
            ) => self.dispatch(now, call, tool, text.get(args).to_owned(), attempt),
            Command::Tool(call, ToolCmd::Cancel) => {
                // The call is dropped, which stops waiting for it, and the
                // tool is asked to take back what it may have done.
                self.calls.retain(|(running, _)| *running != call);
                let (tools, session) = (Arc::clone(&self.tools), self.id);
                let undoing = async move { tools.undo(session, call).await };
                self.undoing.push((call, Box::pin(undoing)));
            }
            Command::SetTimer(id, at) => {
                self.timers.retain(|(set, _)| *set != id);
                if let Some(at) = at {
                    self.timers.push((id, at));
                }
            }
            Command::Emit(event) => self.tell(&SessionEvent::Told(event)),
            // The store tells the kernel of every fact that is safe. For
            // the one it waits for, the journal is asked not to wait for
            // more to store with it.
            Command::AwaitDurable(seq) => self.store.hurry(seq),
        }
    }

    /// Has the journal store the session's state, if the facts a step
    /// has just left make this the time for it.
    fn keep_state(&mut self, now: MonoTime, facts: &[Stamped]) {
        let Some(last) = facts.last().map(|stamped| stamped.seq) else {
            return;
        };
        let waiting = last.get().saturating_sub(self.state_upto.get());
        let since = Duration::from_nanos(now.as_nanos().saturating_sub(self.state_at.as_nanos()));
        let due = waiting >= self.rule.records || since >= self.rule.after;
        let overdue = waiting >= self.rule.records.saturating_mul(2);
        let settled = facts.iter().any(|stamped| {
            matches!(
                stamped.fact,
                Fact::TurnCommitted { .. } | Fact::UtteranceCommitted { .. }
            )
        });
        if !(overdue || due && settled) {
            return;
        }
        // The kernel has taken every fact it emitted, so what it holds is
        // the state as of the last of them.
        self.state_upto = last;
        self.state_at = now;
        if let Ok(state) = codec::encode_state(&self.kernel.snapshot(), self.heard_until) {
            self.store.keep(last, state);
        }
    }

    /// Sends a call to its tool.
    fn dispatch(
        &mut self,
        now: MonoTime,
        call: ToolCallId,
        tool: ToolId,
        args: String,
        attempt: u8,
    ) {
        let (tools, session) = (Arc::clone(&self.tools), self.id);
        let calling = async move {
            let request = ToolRequest {
                session,
                call,
                tool,
                args: &args,
                attempt,
            };
            tools.call(request).await
        };
        self.calls.push((call, Box::pin(calling)));
        mark(self.sink.as_ref(), self.id, now, Stage::ToolSent(call));
    }

    /// Asks the listening models about a turn.
    fn judge(&mut self, turn: TurnId, who: ParticipantId, window: usize, overlap: bool) {
        let began = self
            .kernel
            .view()
            .turns()
            .find(|open| open.turn == turn)
            .map_or(MediaTime::from_ticks(0), |open| open.span.start);
        if let Some(ear) = self.ears.iter_mut().find(|ear| ear.who == who) {
            ear.judge(turn, began, window, overlap);
        }
    }
}

impl<P: Policies> fmt::Debug for Session<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("ears", &self.ears)
            .field("minds", &self.minds)
            .field("voice", &self.voice)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// What stands in for a transport that has closed: nothing comes from it,
/// and what is sent to it goes nowhere.
struct Gone(TransportCaps);

impl Transport for Gone {
    fn caps(&self) -> TransportCaps {
        self.0
    }

    fn poll_event(&mut self, _: &mut Context<'_>) -> Poll<Option<TransportEvent>> {
        Poll::Ready(None)
    }

    fn try_send(&mut self, _: FrameOut<'_>) -> Result<(), Full> {
        Ok(())
    }

    fn try_control(&mut self, _: TransportControl) -> Result<(), Full> {
        Ok(())
    }
}

/// What an application asked for, as the kernel is told it.
fn asked(input: SessionInput) -> Event {
    match input {
        SessionInput::Text { who, text } => Event::TextIn {
            who,
            text: text.into(),
        },
        SessionInput::Say { text } => Event::Control(ControlInput::Say { text: text.into() }),
        SessionInput::Interrupt => Event::Control(ControlInput::Interrupt),
        SessionInput::Handoff { to, context } => {
            Event::Control(ControlInput::Handoff { to, context })
        }
        SessionInput::Close => Event::Control(ControlInput::Close {
            reason: CloseReason::Requested,
        }),
    }
}

/// Tells whoever times a session that it has reached a stage of a turn.
fn mark(sink: Option<&Arc<dyn EventSink>>, session: SessionId, at: MonoTime, stage: Stage) {
    if let Some(sink) = sink {
        sink.stage(session, StageMark { at, stage });
    }
}

/// What is known of the earlier life of a session that was rebuilt.
#[derive(Clone, Copy)]
struct Earlier {
    /// Where the last audio ends that a turn of the session was spoken
    /// in.
    heard: MediaTime,
    /// What the session was rebuilt from.
    from: Rebuilt,
}

/// The kernel of a session as its journal has it. For a session the
/// journal holds nothing of, a new one. Otherwise the one that the
/// journal's state of the session and the records after it add up to,
/// where the last audio ends that a turn of the session was spoken in,
/// and what it was rebuilt from.
fn rebuild<P: Policies>(
    config: &KernelConfig,
    policies: P,
    recovered: &Recovered,
) -> Result<(Kernel<P>, Option<Earlier>), StartError> {
    if recovered.is_new() {
        let kernel = Kernel::new(config.clone(), policies).map_err(StartError::Config)?;
        return Ok((kernel, None));
    }
    let mut text = TextArena::default();
    let mut facts: Vec<Stamped> = Vec::with_capacity(recovered.tail.len());
    for record in &recovered.tail {
        facts.push(codec::decode(record, &mut text).map_err(StartError::Record)?);
    }
    let (from, heard) = if let Some(stored) = &recovered.snapshot {
        let (snapshot, heard) = codec::decode_state(&stored.state).map_err(StartError::State)?;
        let says = Seq(snapshot.next_seq().get().saturating_sub(1));
        if says != stored.upto {
            return Err(StartError::StateMoved {
                journal: stored.upto,
                state: says,
            });
        }
        (snapshot, heard)
    } else {
        // What a new session would be is what the first record was
        // added to.
        let blank = Kernel::new(config.clone(), DefaultPolicies::default())
            .map_err(StartError::Config)?
            .snapshot();
        (blank, MediaTime::from_ticks(0))
    };
    let kernel = Kernel::restore(config.clone(), policies, &from, &facts, &text)
        .map_err(StartError::Restore)?;
    let earlier = Earlier {
        heard: heard.max(heard_until(&facts)),
        from: Rebuilt {
            state: recovered.snapshot.as_ref().map(|state| state.upto),
            records: facts.len() as u64,
        },
    };
    Ok((kernel, Some(earlier)))
}

/// The last of the records of a journal whose next record has the number
/// `next`, with their numbers: what whoever follows a session that was
/// rebuilt can ask for of its earlier life.
fn last_of(records: Vec<Bytes>, next: Seq) -> VecDeque<(Seq, Bytes)> {
    let held = records.len().min(RECENT);
    let first = next.get().saturating_sub(held as u64);
    let skipped = records.len() - held;
    records
        .into_iter()
        .skip(skipped)
        .zip(first..)
        .map(|(record, seq)| (Seq(seq), record))
        .collect()
}

/// Where the last audio ends that a turn among the facts was spoken in.
fn heard_until(facts: &[Stamped]) -> MediaTime {
    facts
        .iter()
        .filter_map(|stamped| match stamped.fact {
            Fact::TurnCommitted { span, .. } => Some(span.end),
            _ => None,
        })
        .max()
        .unwrap_or(MediaTime::from_ticks(0))
}

/// How long after a session began the audio at `at` was heard.
fn since_start(at: MediaTime) -> Duration {
    // 48,000 ticks in a second.
    Duration::from_nanos(at.ticks().saturating_mul(62_500) / 3)
}
