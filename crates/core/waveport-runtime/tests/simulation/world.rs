//! The world a simulated session lives in, and what is checked of it.
//!
//! A [`Plan`] says everything about one run: who calls and what they say,
//! what the line is like, how every part behaves and misbehaves, what the
//! application asks, and when the process dies. [`run`] plays it out on a
//! clock that it controls, life after life, with the real session in the
//! middle and the kernel's monitor watching every step.
//!
//! What is checked, in every run:
//!
//! - the kernel's invariants, after every step, and at the end that
//!   nothing is left unfinished;
//! - that the journal has every fact once, in order, and rebuilds the
//!   session it belongs to;
//! - what goes out on the wire: one response at a time, in order, at the
//!   pace it is heard at, nothing after it was stopped and nothing after
//!   the line closed;
//! - that nothing is in the conversation that the far end did not play;
//! - that a tool that changes something is called once, and not before
//!   the record that allows it is stored.

use core::future::poll_fn;
use core::task::Poll;
use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use tokio::runtime::{Builder, LocalOptions};
use tokio::sync::mpsc;
use tokio::time::Instant;
use waveport_domain::command::{Command, LlmCause, LlmCmd, PlayoutCmd};
use waveport_domain::config::Effect;
use waveport_domain::event::Event;
use waveport_domain::fact::{EpochCause, Fact, Lifecycle, Stamped, ToolOutcome};
use waveport_domain::ids::{GenId, ParticipantId, ToolCallId};
use waveport_domain::kernel::View;
use waveport_domain::out::Out;
use waveport_domain::policy::{AddressedByName, BoxedPolicies};
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;
use waveport_kernel_sim::invariants::Monitor;
use waveport_ports::audio::{Codec, Law, Rate};
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::journal::{Journal, SessionId};
use waveport_ports::stt::{SttFinalize, SttTiming};
use waveport_ports::tool::{ToolExecutor, ToolReply, ToolRequest};
use waveport_ports::transport::{RemoteBuffer, TransportCaps};
use waveport_ports::tts::{Alignment, TtsCaps, TtsInput};
use waveport_runtime::codec;
use waveport_runtime::compute::Compute;
use waveport_runtime::session::{
    Ended, Parts, Request, Session, SessionInput, SessionSpec, StateRule, StoreBroken, Tap,
};
use waveport_sim::{Seed, SimRng};
use waveport_testkit::fakes::{
    Asked, ByLength, Called, FakeTransport, LlmStep, LoudVad, MemoryJournal, ScriptedLlm,
    ScriptedStt, ScriptedTools, ScriptedTts, ScriptedTurns, SentFrame, Spoken, SttScript,
    TransportHandle, TransportLog, TtsScript,
};

pub(crate) const SESSION: SessionId = SessionId(0x51);

/// How long each character of what the agent says takes to say.
pub(crate) const MS_PER_CHAR: u32 = 60;

/// The most lives a session is given.
const MOST_LIVES: usize = 4;

/// Something a caller says, on the clock of the run.
#[derive(Clone, Debug)]
pub(crate) struct Utterance {
    /// Which of the callers: 0 for the first.
    pub who: usize,
    pub from_ms: u64,
    pub to_ms: u64,
    pub text: String,
}

/// What the line to the callers is like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Line {
    /// Plain samples, a far end that says what it played and discards
    /// what it is told to.
    SoundCard,
    /// A telephone line: G.711, and a far end that does neither.
    Telephone,
    /// Opus, and a far end that says what it played and cannot discard.
    Opus,
}

impl Line {
    const fn caps(self) -> TransportCaps {
        match self {
            Self::SoundCard => TransportCaps {
                inbound: Codec::Pcm(Rate::Hz48000),
                outbound: Codec::Pcm(Rate::Hz48000),
                remote_buffer: RemoteBuffer {
                    clearable: true,
                    reports_played: true,
                },
                text: true,
            },
            Self::Telephone => TransportCaps {
                inbound: Codec::G711(Law::Mu),
                outbound: Codec::G711(Law::Mu),
                remote_buffer: RemoteBuffer {
                    clearable: false,
                    reports_played: false,
                },
                text: false,
            },
            Self::Opus => TransportCaps {
                inbound: Codec::Opus,
                outbound: Codec::Opus,
                remote_buffer: RemoteBuffer {
                    clearable: false,
                    reports_played: true,
                },
                text: true,
            },
        }
    }
}

/// Something that happens to a session from outside its parts.
#[derive(Clone, Debug)]
pub(crate) enum Doing {
    /// The application asks something of it.
    Ask(SessionInput),
    /// The next thing it stores is not stored at the first try.
    JournalFails,
    /// Another process takes it over in the journal.
    Taken,
}

pub(crate) type Respond = Arc<dyn Fn(&Asked) -> Vec<LlmStep> + Send + Sync>;
pub(crate) type Answer = Arc<dyn Fn(&Called) -> ToolReply + Send + Sync>;

/// Everything about one run.
#[derive(Clone)]
pub(crate) struct Plan {
    pub seed: u64,
    pub spec: SessionSpec,
    /// The names the agent answers to. With none it takes every turn to
    /// be meant for it.
    pub names: Vec<&'static str>,
    /// The length of speech over the agent's that a model takes to want
    /// the floor, in milliseconds. With none there is no such model.
    pub classifier: Option<u32>,
    pub callers: usize,
    pub line: Line,
    /// How much of a response the far end holds before it plays any.
    pub holds_ms: u64,
    /// How much later than it was sent a packet of a caller may arrive,
    /// and how many of a thousand never do.
    pub jitter_ms: u64,
    pub lost_per_mille: u64,
    pub speech: Vec<Utterance>,
    /// Until when the callers' microphones send.
    pub mics_until_ms: u64,
    /// When the line closes, if it does.
    pub hangs_up_ms: Option<u64>,
    pub doings: Vec<(u64, Doing)>,
    pub stt: SttScript,
    pub llm: Respond,
    pub llm_first_ms: u64,
    pub llm_each_ms: u64,
    pub tts: TtsScript,
    pub tools: Answer,
    pub tools_ms: u64,
    pub journal_ms: u64,
    /// The process dies when the journal holds the record of each of
    /// these numbers, in turn.
    pub dies_after: Vec<u64>,
    /// How long the run goes on.
    pub until_ms: u64,
}

impl Plan {
    /// A calm world for `spec`: one caller on a sound card who says
    /// nothing, parts that answer soon and never fail, and a model that
    /// says "Yes." to everything.
    pub(crate) fn new(seed: u64, mut spec: SessionSpec) -> Self {
        // Three sessions in four have their journal keep their state
        // every few records, so that one that dies is mostly rebuilt
        // from a state and what follows it. The fourth is rebuilt from
        // all its records, as every session was before there were
        // states.
        if !seed.is_multiple_of(4) {
            spec.states = StateRule {
                records: 2 + seed % 7,
                after: Duration::from_secs(60),
            };
        }
        Self {
            seed,
            spec,
            names: Vec::new(),
            classifier: None,
            callers: 1,
            line: Line::SoundCard,
            holds_ms: 0,
            jitter_ms: 0,
            lost_per_mille: 0,
            speech: Vec::new(),
            mics_until_ms: 0,
            hangs_up_ms: None,
            doings: Vec::new(),
            stt: SttScript::new(Vec::new()),
            llm: Arc::new(|_| vec![LlmStep::say("Yes.")]),
            llm_first_ms: 200,
            llm_each_ms: 20,
            tts: TtsScript::default(),
            tools: Arc::new(|_| ToolReply::Completed("done".to_owned())),
            tools_ms: 150,
            journal_ms: 2,
            dies_after: Vec::new(),
            until_ms: 30_000,
        }
    }

    /// Caller `who` says `text` from `from_ms`, at `ms_per_word` a word.
    /// Returns when they have finished.
    pub(crate) fn says(&mut self, who: usize, from_ms: u64, ms_per_word: u64, text: &str) -> u64 {
        let words = u64::try_from(text.split_whitespace().count()).unwrap_or(1);
        let to_ms = from_ms + words.max(1) * ms_per_word;
        self.speech.push(Utterance {
            who,
            from_ms,
            to_ms,
            text: text.to_owned(),
        });
        self.mics_until_ms = self.mics_until_ms.max(to_ms + 3_000);
        to_ms
    }

    fn rng(&self, label: &str) -> SimRng {
        SimRng::new(Seed(self.seed)).fork(label)
    }

    fn policies(&self) -> BoxedPolicies {
        let mut policies = BoxedPolicies::default();
        if !self.names.is_empty() {
            policies.addressee = Box::new(AddressedByName::new(&self.names));
        }
        policies
    }
}

/// What a run did, counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub runs: u64,
    pub steps: u64,
    pub lives: u64,
    pub turns: u64,
    pub dismissed: u64,
    pub spoken: u64,
    pub cut: u64,
    pub barge_ins: u64,
    pub interrupts: u64,
    pub ducks: u64,
    pub unducks: u64,
    pub calls: u64,
    pub sent: u64,
    pub completed: u64,
    pub failed: u64,
    pub in_doubt: u64,
    pub continuations: u64,
    pub delegated: u64,
    pub notices: u64,
    pub closed: u64,
    pub lost: u64,
    pub hung_up: u64,
    pub reopened: u64,
    pub frames: u64,
    /// States the journal stored.
    pub states: u64,
    /// Lives that began from a stored state.
    pub from_state: u64,
}

impl Tally {
    pub(crate) fn add(&mut self, other: &Self) {
        let pairs = [
            (&mut self.runs, other.runs),
            (&mut self.steps, other.steps),
            (&mut self.lives, other.lives),
            (&mut self.turns, other.turns),
            (&mut self.dismissed, other.dismissed),
            (&mut self.spoken, other.spoken),
            (&mut self.cut, other.cut),
            (&mut self.barge_ins, other.barge_ins),
            (&mut self.interrupts, other.interrupts),
            (&mut self.ducks, other.ducks),
            (&mut self.unducks, other.unducks),
            (&mut self.calls, other.calls),
            (&mut self.sent, other.sent),
            (&mut self.completed, other.completed),
            (&mut self.failed, other.failed),
            (&mut self.in_doubt, other.in_doubt),
            (&mut self.continuations, other.continuations),
            (&mut self.delegated, other.delegated),
            (&mut self.notices, other.notices),
            (&mut self.closed, other.closed),
            (&mut self.lost, other.lost),
            (&mut self.hung_up, other.hung_up),
            (&mut self.reopened, other.reopened),
            (&mut self.frames, other.frames),
            (&mut self.states, other.states),
            (&mut self.from_state, other.from_state),
        ];
        for (sum, more) in pairs {
            *sum += more;
        }
    }

    fn step(&mut self, out: &Out) {
        self.steps += 1;
        for stamped in &out.facts {
            match stamped.fact {
                Fact::TurnCommitted { .. } => self.turns += 1,
                Fact::TurnDismissed { .. } => self.dismissed += 1,
                Fact::UtteranceCommitted { truncated, .. } => {
                    self.spoken += 1;
                    self.cut += u64::from(truncated);
                }
                Fact::EpochAdvanced { cause, .. } => {
                    self.barge_ins += u64::from(cause == EpochCause::BargeIn);
                    self.interrupts += u64::from(cause == EpochCause::Interrupted);
                }
                Fact::ToolProposed { .. } => self.calls += 1,
                Fact::ToolDispatched { .. } => self.sent += 1,
                Fact::ToolFinished { outcome, .. } => match outcome {
                    ToolOutcome::Completed => self.completed += 1,
                    ToolOutcome::InDoubt => self.in_doubt += 1,
                    _ => self.failed += 1,
                },
                Fact::NoticeAppended { .. } => self.notices += 1,
                Fact::LifecycleChanged { to, .. } => {
                    self.closed += u64::from(to == Lifecycle::Closed);
                }
                _ => {}
            }
        }
        for command in &out.cmds {
            match command {
                Command::Playout(_, PlayoutCmd::Duck) => self.ducks += 1,
                Command::Playout(_, PlayoutCmd::Unduck) => self.unducks += 1,
                Command::Llm(_, LlmCmd::Start { cause, .. }) => match cause {
                    LlmCause::Continuation { .. } => self.continuations += 1,
                    LlmCause::Delegated { .. } => self.delegated += 1,
                    LlmCause::Reply { .. } => {}
                },
                _ => {}
            }
        }
    }
}

/// What the monitor made of a life, kept where the run can reach it.
struct Watched {
    monitor: Monitor,
    violation: Option<String>,
    tally: Tally,
    /// Every step, written down, when the run is to be explained.
    log: Option<Vec<String>>,
}

struct Watch(Arc<Mutex<Watched>>);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Tap for Watch {
    fn stepped(&mut self, now: MonoTime, event: &Event, out: &Out, view: View<'_>) {
        let mut watched = lock(&self.0);
        watched.tally.step(out);
        if let Some(log) = &mut watched.log {
            let ms = now.as_nanos() / 1_000_000;
            log.push(format!("{ms:>7} ms  {event:?}"));
            for stamped in &out.facts {
                let text = stamped.fact.text().map_or("", |text| out.text.get(text));
                log.push(format!(
                    "              fact #{} {:?} {text:?}",
                    stamped.seq, stamped.fact
                ));
            }
            for command in &out.cmds {
                let text = command.text().map_or("", |text| out.text.get(text));
                log.push(format!("              {command:?} {text:?}"));
            }
        }
        if watched.violation.is_none()
            && let Err(violation) = watched.monitor.observe(now, event, out, view)
        {
            let ms = now.as_nanos() / 1_000_000;
            watched.violation = Some(format!("at {ms} ms, on {event:?}: {violation}"));
        }
    }
}

/// Tools that note when one that changes something is called before the
/// record that allows it is stored.
struct Guarded {
    tools: ScriptedTools,
    journal: MemoryJournal,
    effects: Vec<Effect>,
    early: Arc<Mutex<Vec<ToolCallId>>>,
}

impl ToolExecutor for Guarded {
    fn call<'a>(&'a self, request: ToolRequest<'a>) -> BoxFuture<'a, ToolReply> {
        let effect = self.effects.get(usize::from(request.tool.get()));
        if effect.is_some_and(|effect| *effect != Effect::Pure) {
            let allowed = format!(
                "\"kind\":\"tool_authorized\",\"call\":{}}}",
                request.call.get()
            );
            let stored = self.journal.records(SESSION);
            let is_stored = stored
                .iter()
                .any(|record| String::from_utf8_lossy(record).contains(&allowed));
            if !is_stored {
                lock(&self.early).push(request.call);
            }
        }
        self.tools.call(request)
    }

    fn undo(&self, session: SessionId, call: ToolCallId) -> BoxFuture<'_, bool> {
        self.tools.undo(session, call)
    }
}

/// One life of a session, as it looked from outside.
pub(crate) struct Life {
    pub wire: TransportLog,
    /// How the life ended: by itself, or because the process died, or not
    /// at all.
    pub ended: Option<Ended>,
    pub died: bool,
    /// When it was over, in milliseconds from the start of the run.
    pub over_ms: u64,
}

/// What a run came to.
pub(crate) struct Outcome {
    pub lives: Vec<Life>,
    /// Every fact in the journal, and the text they refer to.
    pub facts: Vec<Stamped>,
    pub text: TextArena,
    pub asked: Vec<Asked>,
    pub called: Vec<Called>,
    pub undone: Vec<ToolCallId>,
    pub tally: Tally,
    /// Every step of the run, when it was to be explained.
    pub log: Vec<String>,
}

impl Outcome {
    pub(crate) fn ended(&self) -> Option<&Ended> {
        self.lives.last().and_then(|life| life.ended.as_ref())
    }

    /// The text a fact refers to.
    pub(crate) fn text_of(&self, fact: &Fact) -> &str {
        fact.text().map_or("", |text| self.text.get(text))
    }

    /// The turns that were committed: who, and what they said.
    pub(crate) fn turns(&self) -> Vec<(ParticipantId, &str, bool)> {
        self.facts
            .iter()
            .filter_map(|stamped| match stamped.fact {
                Fact::TurnCommitted { who, addressed, .. } => {
                    Some((who, self.text_of(&stamped.fact), addressed))
                }
                _ => None,
            })
            .collect()
    }

    /// What the agent said, as far as it was heard, and whether it was
    /// cut off.
    pub(crate) fn utterances(&self) -> Vec<(GenId, &str, bool)> {
        self.facts
            .iter()
            .filter_map(|stamped| match stamped.fact {
                Fact::UtteranceCommitted {
                    generation,
                    truncated,
                    ..
                } => Some((generation, self.text_of(&stamped.fact), truncated)),
                _ => None,
            })
            .collect()
    }
}

/// A run that broke something.
#[derive(Debug)]
pub(crate) struct Broken(pub String);

fn broken<T>(what: impl Into<String>) -> Result<T, Broken> {
    Err(Broken(what.into()))
}

/// Plays `plan` out, on a clock and an executor of its own. With
/// `explain`, every step is written down.
pub(crate) fn run(plan: &Plan, explain: bool) -> Result<Outcome, Broken> {
    let executor = Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build_local(LocalOptions::default())
        .map_err(|error| Broken(format!("no executor: {error}")))?;
    executor.block_on(play(plan, explain))
}

enum Drove {
    Ended(Ended),
    Died,
    Time,
}

/// Runs the session until `until`, or until it ends or its process dies.
async fn drive(
    session: &mut Session<BoxedPolicies>,
    journal: &MemoryJournal,
    until: Instant,
) -> Drove {
    let running = poll_fn(|cx| {
        if journal.has_stopped() {
            return Poll::Ready(Drove::Died);
        }
        match session.poll_run(cx) {
            Poll::Ready(ended) => Poll::Ready(Drove::Ended(ended)),
            Poll::Pending if journal.has_stopped() => Poll::Ready(Drove::Died),
            Poll::Pending => Poll::Pending,
        }
    });
    tokio::time::timeout_at(until, running)
        .await
        .unwrap_or(Drove::Time)
}

/// The facts of a journal, and their text.
fn decoded(records: &[Bytes]) -> Result<(Vec<Stamped>, TextArena), Broken> {
    let mut text = TextArena::default();
    let mut facts = Vec::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        let stamped = codec::decode(record, &mut text)
            .map_err(|error| Broken(format!("record {} cannot be read: {error}", index + 1)))?;
        if stamped.seq.get() != index as u64 + 1 {
            return broken(format!(
                "record {} of the journal is fact #{}",
                index + 1,
                stamped.seq
            ));
        }
        facts.push(stamped);
    }
    Ok((facts, text))
}

/// The response the journal has as being heard and not as ended, and
/// what it has as heard of it.
fn speech_of(facts: &[Stamped], text: &TextArena) -> Option<(GenId, String)> {
    let mut speech: Option<(GenId, String)> = None;
    for stamped in facts {
        match stamped.fact {
            Fact::UtteranceProgress {
                generation, heard, ..
            } => {
                let heard = text.get(heard);
                match &mut speech {
                    Some((of, said)) if *of == generation => said.push_str(heard),
                    _ => speech = Some((generation, heard.to_owned())),
                }
            }
            Fact::UtteranceCommitted { generation, .. }
                if speech.as_ref().is_some_and(|(of, _)| *of == generation) =>
            {
                speech = None;
            }
            _ => {}
        }
    }
    speech
}

/// The session's clock at the start of a life that finds `facts`, in
/// milliseconds: a second after the end of the last turn among them.
fn resumes_at(facts: &[Stamped]) -> u64 {
    let heard = facts.iter().filter_map(|stamped| match stamped.fact {
        Fact::TurnCommitted { span, .. } => Some(span.end.ticks()),
        _ => None,
    });
    heard.max().map_or(0, |ticks| ticks / 48 + 1_000)
}

/// The parts of one life that are made anew for it.
struct Fresh {
    transport: FakeTransport,
    far_end: TransportHandle,
    stt: ScriptedStt,
}

impl Plan {
    /// The line and the recogniser of a life that starts `began_ms` into
    /// the run with the session's clock at `clock_ms`.
    fn fresh(&self, life: usize, began_ms: u64, clock_ms: u64) -> Fresh {
        // What a caller was saying when the process died is not said
        // again: what the next life hears begins with what they say next.
        let speech: Vec<&Utterance> = self
            .speech
            .iter()
            .filter(|said| said.from_ms >= began_ms)
            .collect();
        let closes = self.hangs_up_ms.filter(|at| *at > began_ms);
        let mics_until = closes.map_or(self.mics_until_ms, |at| at.min(self.mics_until_ms));
        let mut far = FakeTransport::with(self.line.caps()).holding(self.holds_ms);
        for caller in 0..self.callers {
            let who = ParticipantId(u16::try_from(caller + 1).unwrap_or(u16::MAX));
            let voice = u8::try_from(caller).unwrap_or(u8::MAX);
            let talks: Vec<(u64, u64)> = speech
                .iter()
                .filter(|said| said.who == caller)
                .map(|said| (said.from_ms - began_ms, said.to_ms - began_ms))
                .collect();
            far = far.join(caller as u64, who).microphone_of(
                who,
                voice,
                0,
                mics_until.saturating_sub(began_ms),
                &talks,
            );
        }
        if self.jitter_ms > 0 || self.lost_per_mille > 0 {
            let mut rng = self.rng(&format!("network {life}"));
            let (jitter, lost) = (self.jitter_ms, self.lost_per_mille);
            far = far.disturbed(move |sent| {
                if rng.chance(lost, 1_000) {
                    return None;
                }
                Some(sent + Duration::from_millis(rng.range(0..jitter + 1)))
            });
        }
        if closes.is_some() {
            far = far.then_closes();
        }
        let (transport, far_end) = far.build();

        let spoken = speech
            .iter()
            .map(|said| {
                let from = said.from_ms - began_ms + clock_ms;
                let to = said.to_ms - began_ms + clock_ms;
                Spoken::new(from, to, &said.text).by(u8::try_from(said.who).unwrap_or(u8::MAX))
            })
            .collect();
        let stt = ScriptedStt::new(SttScript {
            speech: spoken,
            ..self.stt.clone()
        });
        Fresh {
            transport,
            far_end,
            stt,
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one run from its first life to its last, in the order things happen"
)]
async fn play(plan: &Plan, explain: bool) -> Result<Outcome, Broken> {
    let began = Instant::now();
    let since = |at: Instant| u64::try_from(at.duration_since(began).as_millis()).unwrap_or(0);
    let mut journal = MemoryJournal::new();
    journal.latency = Duration::from_millis(plan.journal_ms);
    let respond = Arc::clone(&plan.llm);
    let mut llm = ScriptedLlm::with(move |asked| respond(asked));
    llm.first = Duration::from_millis(plan.llm_first_ms);
    llm.each = Duration::from_millis(plan.llm_each_ms);
    let answer = Arc::clone(&plan.tools);
    let mut tools = ScriptedTools::with(move |called| answer(called));
    tools.latency = Duration::from_millis(plan.tools_ms);
    let early = Arc::new(Mutex::new(Vec::new()));
    let effects: Vec<Effect> = plan
        .spec
        .kernel
        .tools
        .iter()
        .map(|tool| tool.effect)
        .collect();

    let mut doings = plan.doings.clone();
    doings.sort_by_key(|(at, _)| *at);
    let mut doings = doings.into_iter().peekable();
    let mut deaths = plan.dies_after.iter().copied();
    let mut lives = Vec::new();
    let mut tally = Tally::default();
    let mut log = Vec::new();
    let end = began + Duration::from_millis(plan.until_ms);

    let last = loop {
        let life = lives.len();
        journal.go_on();
        // A journal that is to stop at a record it already has stops at
        // the next thing it is asked to store.
        if let Some(last) = deaths.next() {
            journal.stop_after(last);
        }
        let (facts, text) = decoded(&journal.records(SESSION))?;
        tally.from_state += u64::from(journal.kept(SESSION).0.is_some());
        let began_ms = since(Instant::now());
        let fresh = plan.fresh(life, began_ms, resumes_at(&facts));
        let turns = Box::new(ScriptedTurns::always());
        let overlap = plan.classifier.map(|barge_in_ms| {
            Box::new(ByLength { barge_in_ms })
                as Box<dyn waveport_ports::model::InterruptionClassifier>
        });
        let compute = Compute::inline(LoudVad::default(), turns, overlap)
            .map_err(|error| Broken(format!("no listening models: {error:?}")))?;
        let parts = Parts {
            transport: Box::new(fresh.transport),
            stt: Arc::new(fresh.stt),
            llm: Arc::new(llm.clone()),
            tts: Arc::new(ScriptedTts::new(plan.tts.clone())),
            tools: Arc::new(Guarded {
                tools: tools.clone(),
                journal: journal.clone(),
                effects: effects.clone(),
                early: Arc::clone(&early),
            }),
            journal: Arc::new(journal.clone()),
            compute: Arc::new(compute),
            sink: None,
        };
        let mut session = Session::open(plan.spec.clone(), parts, plan.policies())
            .await
            .map_err(|error| Broken(format!("life {life} could not start: {error}")))?;
        let monitor = if facts.is_empty() {
            Monitor::new(session.view())
        } else {
            Monitor::restored(session.view(), speech_of(&facts, &text))
        };
        let watched = Arc::new(Mutex::new(Watched {
            monitor,
            violation: None,
            tally: Tally::default(),
            log: explain.then(Vec::new),
        }));
        session.tap(Box::new(Watch(Arc::clone(&watched))));
        let (ask, inbox) = mpsc::channel(64);
        session.listen_to(inbox);
        if explain {
            log.push(format!(
                "{began_ms:>7} ms  LIFE {life} BEGINS with {} facts",
                facts.len()
            ));
        }

        let ended = loop {
            let next = doings
                .peek()
                .map(|(at, _)| began + Duration::from_millis(*at));
            let until = next.map_or(end, |next| next.min(end));
            match drive(&mut session, &journal, until).await {
                Drove::Ended(ended) => break Some(ended),
                Drove::Died => break None,
                Drove::Time => {}
            }
            if Instant::now() >= end {
                break None;
            }
            while let Some((_, doing)) = doings.next_if(|(at, _)| *at <= since(Instant::now())) {
                match doing {
                    Doing::Ask(input) => {
                        let _ = ask.try_send(Request::Input(input));
                    }
                    Doing::JournalFails => {
                        journal.fail_next_append(PortError::transient("the disk is busy"));
                    }
                    Doing::Taken => {
                        let _ = journal.acquire(SESSION).await;
                    }
                }
            }
        };
        let died = journal.has_stopped();
        {
            let mut watched = lock(&watched);
            tally.add(&watched.tally);
            if let Some(steps) = watched.log.take() {
                log.extend(steps);
            }
            if let Some(violation) = watched.violation.take() {
                return Err(Broken(format!("life {life}: {violation}\n{}", tail(&log))));
            }
        }
        let wire = fresh.far_end.log();
        tally.frames += wire.frames.len() as u64;
        tally.reopened += u64::from(session.stats().hearing.recogniser_failures);
        tally.states += u64::from(session.stats().states.stored);
        if explain {
            log.push(format!(
                "{:>7} ms  LIFE {life} IS OVER: ended {ended:?}, died {died}",
                since(Instant::now())
            ));
        }
        lives.push(Life {
            wire,
            ended: ended.clone(),
            died,
            over_ms: since(Instant::now()),
        });
        if died && lives.len() < MOST_LIVES && Instant::now() < end {
            drop(session);
            continue;
        }
        break (session, watched, ended);
    };
    let (session, watched, ended) = last;
    tally.runs = 1;
    tally.lives = lives.len() as u64;
    tally.lost += u64::from(matches!(ended, Some(Ended::Lost(_))));
    tally.hung_up += u64::from(plan.hangs_up_ms.is_some());

    // What is left of a session that is still running must be nothing.
    let died = lives.last().is_some_and(|life| life.died);
    if ended.is_none() && !died {
        let at_rest = lock(&watched).monitor.at_rest(session.view());
        if let Err(violation) = at_rest {
            return Err(Broken(format!("at the end: {violation}\n{}", tail(&log))));
        }
    }
    if let Some(Ended::Lost(broken_by)) = &ended
        && *broken_by != StoreBroken::Fenced
    {
        return Err(Broken(format!(
            "the session lost its journal: {broken_by:?}"
        )));
    }

    // The journal has every fact once and in order, and what it has is
    // the session: one rebuilt from it holds what this one holds.
    journal.go_on();
    let (facts, text) = decoded(&journal.records(SESSION))?;
    let whole = !died && !matches!(ended, Some(Ended::Lost(_)));
    if whole {
        let held = session.snapshot();
        drop(session);
        let idle = plan.fresh(lives.len(), plan.until_ms, 0);
        let parts = Parts {
            transport: Box::new(idle.transport),
            stt: Arc::new(idle.stt),
            llm: Arc::new(llm.clone()),
            tts: Arc::new(ScriptedTts::new(plan.tts.clone())),
            tools: Arc::new(tools.clone()),
            journal: Arc::new(journal.clone()),
            compute: Arc::new(
                Compute::inline(LoudVad::default(), Box::new(ScriptedTurns::always()), None)
                    .map_err(|error| Broken(format!("no listening models: {error:?}")))?,
            ),
            sink: None,
        };
        let rebuilt = Session::open(plan.spec.clone(), parts, plan.policies())
            .await
            .map_err(|error| {
                Broken(format!("the journal does not rebuild the session: {error}"))
            })?;
        if rebuilt.snapshot() != held {
            return Err(Broken(format!(
                "the journal adds up to\n{:?}\nand the session held\n{held:?}\n{}",
                rebuilt.snapshot(),
                tail(&log)
            )));
        }
    }

    let outcome = Outcome {
        lives,
        facts,
        text,
        asked: llm.asked(),
        called: tools.called(),
        undone: tools.undone(),
        tally,
        log,
    };
    let early = lock(&early).clone();
    checked(plan, &outcome, &early)
        .map_err(|Broken(what)| Broken(format!("{what}\n{}", tail(&outcome.log))))?;
    Ok(outcome)
}

/// The last steps of a run that was written down, or all of them when
/// the run was asked for by its seed.
fn tail(log: &[String]) -> String {
    let alone = std::env::var_os("WAVEPORT_SIM_SEED").is_some();
    let shown = if alone { log.len() } else { 120 };
    let from = log.len().saturating_sub(shown);
    log.get(from..).unwrap_or(&[]).join("\n")
}

/// What must hold of a run beyond the kernel's own invariants.
fn checked(plan: &Plan, outcome: &Outcome, early: &[ToolCallId]) -> Result<(), Broken> {
    let reports = plan.line.caps().remote_buffer.reports_played;
    // How much of each response the far end played, over all lives.
    let mut played: BTreeMap<GenId, u32> = BTreeMap::new();
    for (index, life) in outcome.lives.iter().enumerate() {
        wire_is_sound(&life.wire)
            .map_err(|Broken(what)| Broken(format!("life {index}: {what}")))?;
        let mut tags: Vec<_> = life.wire.frames.iter().map(|frame| frame.tag).collect();
        tags.dedup();
        for tag in tags {
            let heard = if reports {
                life.wire.heard(tag)
            } else {
                life.wire.sent(tag)
            };
            *played.entry(tag.generation).or_default() += heard.as_millis();
        }
    }
    for (generation, text, _) in outcome.utterances() {
        let said = u32::try_from(text.len()).unwrap_or(u32::MAX) * MS_PER_CHAR;
        let heard = played.get(&generation).copied().unwrap_or(0);
        if said > heard + MS_PER_CHAR {
            return broken(format!(
                "the conversation has {text:?} of response {generation:?}, which takes {said} ms \
                 to say, and the far end played {heard} ms of it"
            ));
        }
    }

    if let Some(call) = early.first() {
        return broken(format!(
            "the tool call {call:?} was made before the record that allows it was stored"
        ));
    }
    let mut once: Vec<ToolCallId> = Vec::new();
    for called in &outcome.called {
        let effect = plan.spec.kernel.tools.get(usize::from(called.tool.get()));
        if effect.is_some_and(|tool| tool.effect == Effect::SideEffecting) {
            if once.contains(&called.call) {
                return broken(format!(
                    "the tool call {:?}, which changes something, was made twice",
                    called.call
                ));
            }
            once.push(called.call);
        }
    }
    Ok(())
}

/// What must hold of everything a session sent in one life.
fn wire_is_sound(wire: &TransportLog) -> Result<(), Broken> {
    if wire.late > 0 {
        return broken(format!(
            "{} frames of a response were sent after the far end was told to discard it",
            wire.late
        ));
    }
    if wire.after_close > 0 {
        return broken(format!(
            "{} things were sent after the line had closed",
            wire.after_close
        ));
    }
    // The responses that another has taken over from, and the one being
    // sent: when its first frame went out, and its newest frame.
    let mut over = Vec::new();
    let mut current: Option<(Duration, &SentFrame)> = None;
    for (index, frame) in wire.frames.iter().enumerate() {
        match current {
            Some((began, before)) if before.tag == frame.tag => {
                if before.last {
                    return broken(format!("frame {index} follows the last of its response"));
                }
                if frame.at != before.at.saturating_add(before.length) {
                    return broken(format!(
                        "frame {index} is at {:?} and the one before ends at {:?}",
                        frame.at,
                        before.at.saturating_add(before.length)
                    ));
                }
                // A response is sent at the pace it is heard at: no
                // frame is sent more than a little before it is due.
                let elapsed = frame.when.saturating_sub(began).as_millis();
                if u128::from(frame.at.as_millis()) > elapsed + 60 {
                    return broken(format!(
                        "frame {index} is {} ms into its response and was sent {elapsed} ms \
                         after the first",
                        frame.at.as_millis()
                    ));
                }
                current = Some((began, frame));
            }
            _ => {
                if over.contains(&frame.tag) {
                    return broken(format!(
                        "frame {index} is of a response that another had taken over from"
                    ));
                }
                if let Some((_, before)) = current {
                    over.push(before.tag);
                }
                current = Some((frame.when, frame));
            }
        }
    }
    Ok(())
}

/// A caps for a synthesiser of a simulated world.
pub(crate) fn synthesiser(alignment: Alignment, input: TtsInput) -> TtsCaps {
    TtsCaps {
        alignment,
        input,
        ..TtsScript::default().caps
    }
}

/// A recogniser of a simulated world.
pub(crate) fn recogniser(latency_ms: u64, timing: SttTiming, finalize: SttFinalize) -> SttScript {
    let mut script = SttScript::new(Vec::new());
    script.latency = Duration::from_millis(latency_ms);
    script.caps.timing = timing;
    script.caps.finalize = finalize;
    script
}
