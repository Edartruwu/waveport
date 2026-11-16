//! Random sessions from a seed.
//!
//! The scenarios tell six stories along every path. This module tells
//! thousands of stories nobody wrote: a seed decides who is in the
//! session, when they speak and for how long, what the language model
//! writes and which tools it calls, how late every adapter answers and in
//! which ways it misbehaves, and when the process dies. The kernel is
//! stepped through all of it with a [`Monitor`] watching, so every run
//! checks every invariant after every step, and at the end checks that
//! nothing was left unfinished.
//!
//! A [`Journal`] keeps what the kernel journals. Now and then it is
//! checked that the journal alone rebuilds what the kernel holds, and
//! when the process dies the session goes on from the journal: from what
//! of it had reached the disk.
//!
//! A run is a pure function of its profile and its seed. A failure names
//! both, and [`explain`] replays it with a log.

use core::fmt::Write as _;
use core::time::Duration;

use waveport_domain::command::{Command, LlmCause, LlmCmd, PlayoutCmd, SenseCmd, ToolCmd, TtsCmd};
use waveport_domain::config::{AgentSpec, Effect, Executor, Gate, KernelConfig, ToolSpec};
use waveport_domain::event::{
    CloseReason, ControlInput, Event, Finish, HandoffContext, OpEvent, OverlapVerdict, Part,
    RetryClass, TimerId, ToolEvent, VadEdge,
};
use waveport_domain::fact::{Dismissal, EpochCause, Fact, Lifecycle, ToolOutcome};
use waveport_domain::ids::{
    AgentId, Epoch, GenId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId, TurnId,
};
use waveport_domain::kernel::Kernel;
use waveport_domain::out::Out;
use waveport_domain::policy::{AddressedByName, BoxedPolicies};
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};
use waveport_sim::{Schedule, Seed, SeedPlan, SimRng, SimTime};

use crate::check::Report;
use crate::invariants::{Journal, Monitor, Rule, Violation};
use crate::trace::Trace;

type Ms = u64;

/// Milliseconds of audio that synthesis makes of one byte of text.
const AUDIO_PER_BYTE: u32 = 40;
/// How often playback reports its position.
const REPORT_EVERY: Ms = 100;
/// A run that takes more steps than this is taken to never come to rest.
const MOST_STEPS: u64 = 60_000;
/// How many lines of a run's log a failure shows.
const LOG_TAIL: usize = 60;

const WORDS: &[&str] = &[
    "yes", "no", "the", "order", "shipped", "on", "monday", "über", "naïve", "日本", "it",
    "arrives", "agent", "please", "thanks", "ok", "mm-hm", "refund", "where", "is", "my",
];

/// How the world around the kernel behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Every adapter answers, once, soon.
    Calm,
    /// Adapters answer late, twice or never, go on after they were told to
    /// stop, and events that make no sense are thrown in.
    Chaos,
    /// Requests go unanswered and reports are never made.
    Stalls,
    /// Every third report arrives twice.
    Duplicates,
    /// The session is asked to close at some point, the adapters may fall
    /// silent before or after, and one of the two closing timers may be
    /// lost.
    Shutdown,
    /// The process dies, up to three times, and the session goes on from
    /// what of its journal had reached the disk.
    Crashes,
    /// The line drops, up to three times, and is back in time, too late or
    /// never. The session waits for it for a moment, for a few seconds or
    /// not at all.
    Lines,
}

impl Profile {
    /// Every profile.
    ///
    /// A new one is added at the end: the seeds of a sweep are numbered
    /// from its profile's place here.
    pub const ALL: [Self; 7] = [
        Self::Calm,
        Self::Chaos,
        Self::Stalls,
        Self::Duplicates,
        Self::Shutdown,
        Self::Crashes,
        Self::Lines,
    ];

    /// The profile's name, as failures print it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Calm => "calm",
            Self::Chaos => "chaos",
            Self::Stalls => "stalls",
            Self::Duplicates => "duplicates",
            Self::Shutdown => "shutdown",
            Self::Crashes => "crashes",
            Self::Lines => "lines",
        }
    }

    const fn faults(self) -> Faults {
        match self {
            Self::Calm => Faults {
                jitter: 30,
                twice: 0,
                lost: 0,
                stale: 0,
                nonsense: 0,
                late: 3,
            },
            Self::Chaos => Faults {
                jitter: 400,
                twice: 40,
                lost: 40,
                stale: 500,
                nonsense: 40,
                late: 60,
            },
            Self::Stalls => Faults {
                jitter: 60,
                twice: 0,
                lost: 300,
                stale: 0,
                nonsense: 0,
                late: 3,
            },
            Self::Duplicates => Faults {
                jitter: 60,
                twice: 350,
                lost: 0,
                stale: 0,
                nonsense: 0,
                late: 3,
            },
            Self::Shutdown => Faults {
                jitter: 100,
                twice: 20,
                lost: 60,
                stale: 200,
                nonsense: 5,
                late: 10,
            },
            Self::Crashes => Faults {
                jitter: 80,
                twice: 30,
                lost: 30,
                stale: 100,
                nonsense: 4,
                late: 5,
            },
            Self::Lines => Faults {
                jitter: 80,
                twice: 20,
                lost: 20,
                stale: 100,
                nonsense: 3,
                late: 5,
            },
        }
    }
}

/// How much goes wrong. Chances are out of a thousand.
#[derive(Clone, Copy, Debug)]
struct Faults {
    /// Every answer is up to this late.
    jitter: Ms,
    /// A report is delivered twice.
    twice: u64,
    /// A request is never answered, or a report never made.
    lost: u64,
    /// An adapter goes on after it was told to stop.
    stale: u64,
    /// The most events that make no sense in one run.
    nonsense: u64,
    /// A timer fires up to this late.
    late: Ms,
}

/// What the runs of a sweep did, added up. A sweep that never interrupts
/// anybody proves nothing about interruptions, so tests look at this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Runs.
    pub runs: u64,
    /// Steps the kernel took.
    pub steps: u64,
    /// Turns committed.
    pub turns: u64,
    /// Turns dismissed as a backchannel or as noise.
    pub asides: u64,
    /// Turns dismissed for having no words.
    pub empty: u64,
    /// Responses started with a language model.
    pub replies: u64,
    /// Responses that entered the conversation whole.
    pub spoken: u64,
    /// Responses that entered the conversation cut short.
    pub cut: u64,
    /// Responses ended by speech over them.
    pub barge_ins: u64,
    /// Responses replaced before anybody heard them.
    pub superseded: u64,
    /// Responses ended because a part of them failed or stalled.
    pub failures: u64,
    /// Times the language model was told to wait.
    pub pauses: u64,
    /// Sessions that closed.
    pub closed: u64,
    /// Tool calls a language model asked for.
    pub calls: u64,
    /// Tool calls sent to a tool outside the session.
    pub sent: u64,
    /// Tool calls sent again after the process died.
    pub resent: u64,
    /// Tool calls given to another agent to answer.
    pub delegated: u64,
    /// Tool calls that ended with their tool's result.
    pub completed: u64,
    /// Tool calls that failed.
    pub failed: u64,
    /// Tool calls withdrawn before they ran, or undone.
    pub withdrawn: u64,
    /// Tool calls of which nobody knows whether they took effect.
    pub in_doubt: u64,
    /// Responses started for the results of tool calls.
    pub continuations: u64,
    /// Times an agent was given the floor.
    pub handoffs: u64,
    /// Times the process died.
    pub crashes: u64,
    /// Times a line dropped.
    pub drops: u64,
    /// Times a line came back to a session that was waiting for it.
    pub returns: u64,
    /// Responses started because the line was back: for a reply the drop
    /// cut off, or for what was said or done while the line was gone.
    pub resumed: u64,
    /// Sessions that closed because their line stayed gone.
    pub abandoned: u64,
    /// Times the journal was checked against the kernel.
    pub checks: u64,
    /// A fingerprint of everything the runs emitted, for telling whether
    /// two sweeps did the same.
    pub trace: u64,
}

impl Tally {
    fn add(&mut self, other: &Self) {
        self.runs += other.runs;
        self.steps += other.steps;
        self.turns += other.turns;
        self.asides += other.asides;
        self.empty += other.empty;
        self.replies += other.replies;
        self.spoken += other.spoken;
        self.cut += other.cut;
        self.barge_ins += other.barge_ins;
        self.superseded += other.superseded;
        self.failures += other.failures;
        self.pauses += other.pauses;
        self.closed += other.closed;
        self.calls += other.calls;
        self.sent += other.sent;
        self.resent += other.resent;
        self.delegated += other.delegated;
        self.completed += other.completed;
        self.failed += other.failed;
        self.withdrawn += other.withdrawn;
        self.in_doubt += other.in_doubt;
        self.continuations += other.continuations;
        self.handoffs += other.handoffs;
        self.crashes += other.crashes;
        self.drops += other.drops;
        self.returns += other.returns;
        self.resumed += other.resumed;
        self.abandoned += other.abandoned;
        self.checks += other.checks;
        self.trace = mix(self.trace, other.trace);
    }
}

fn mix(hash: u64, value: u64) -> u64 {
    (hash ^ value)
        .wrapping_mul(0x0000_0100_0000_01b3)
        .rotate_left(23)
}

/// Something on its way to the kernel, or due to happen in the world.
#[derive(Debug)]
enum Item {
    /// An adapter's report.
    Report(Event),
    /// Something the application does. It happens even when the adapters
    /// have fallen silent.
    Control(Event),
    /// A timer, with the number of the arming it belongs to.
    Timer(TimerId, u32),
    /// The language model writes its next piece.
    Write(OpTag),
    /// Synthesis delivers audio.
    Segment {
        tag: OpTag,
        text_end: u32,
        audio_ms: u32,
        last: bool,
    },
    /// Playback reports its position.
    Tick(OpTag),
    /// Someone starts to speak.
    Speak { who: ParticipantId, long: bool },
    /// An event that makes no sense.
    Nonsense,
    /// The adapters fall silent.
    Silence,
    /// The journal has stored every fact up to this one, and says so.
    Stored(u64),
    /// The journal keeps a snapshot of the session.
    Snapshot,
    /// The journal is checked against the kernel.
    Check,
    /// The process dies.
    Crash,
}

/// What a language model writes next.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Text(String),
    /// A request for a tool call, with its arguments.
    Call(ToolId, String),
}

/// How a language model's response ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    Done,
    Fails,
    Hangs,
}

/// The adapters' side of one response.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is a separate way for an adapter to behave or misbehave"
)]
struct Job {
    tag: OpTag,
    /// What the model writes, piece by piece.
    pieces: Vec<Piece>,
    next: usize,
    /// Whether it has asked for a tool call.
    called: bool,
    ending: Ending,
    paused: bool,
    /// Whether a piece is held back because the model was told to wait.
    held: bool,
    cancelled: bool,
    /// Whether the adapters go on after they were told to stop.
    deaf: bool,
    /// Whether synthesis never delivers.
    mute: bool,
    /// Whether playback never reports.
    blind: bool,
    /// Bytes of text and milliseconds of audio asked of synthesis.
    asked: (u32, u32),
    /// When the last audio asked for is due.
    ready_at: Ms,
    /// The audio delivered: how much text each stretch ends at, and when.
    marks: Vec<(u32, u32)>,
    playing: bool,
    /// Milliseconds of audio played.
    position: u32,
    reported_at: Ms,
}

impl Job {
    /// Milliseconds of audio delivered.
    fn audio(&self) -> u32 {
        self.marks.last().map_or(0, |mark| mark.1)
    }

    /// Bytes of text whose audio has been played in full.
    fn heard(&self) -> u32 {
        let mut before = (0_u32, 0_u32);
        for &(text, audio) in &self.marks {
            if self.position >= audio {
                before = (text.max(before.0), audio);
                continue;
            }
            let (text, before_text) = (u64::from(text.max(before.0)), u64::from(before.0));
            let part = u64::from(self.position - before.1) * (text - before_text)
                / u64::from((audio - before.1).max(1));
            return u32::try_from(before_text + part).unwrap_or(u32::MAX);
        }
        before.0
    }
}

/// One person, as the adapters that listen to them see them.
#[derive(Clone, Copy, Debug)]
struct Person {
    id: ParticipantId,
    /// Until when they are speaking.
    busy_until: Ms,
    /// When the last report about their voice arrives, so that reports
    /// stay in order.
    voice_at: Ms,
    /// The same for their transcripts.
    words_at: Ms,
}

struct Run {
    kernel: Kernel<BoxedPolicies>,
    monitor: Monitor,
    config: KernelConfig,
    /// Whether the agent answers only to its name.
    named: bool,
    /// What the kernel journaled, as far as it survives.
    journal: Journal,
    /// When each fact of the journal was emitted. The first is fact #1.
    emitted: Vec<Ms>,
    /// How long the journal takes to store a fact.
    lag: Ms,
    /// The highest fact the kernel was told is stored.
    vouched: u64,
    /// The calls that must happen at most once and were sent, in any life
    /// of the session.
    effects: Vec<ToolCallId>,
    out: Out,
    now: Ms,
    queue: Schedule<Item>,
    rng: SimRng,
    faults: Faults,
    people: Vec<Person>,
    jobs: Vec<Job>,
    /// The timers that are armed, with the number of their arming.
    timers: Vec<(TimerId, u32)>,
    armings: u32,
    /// A closing timer that is never delivered.
    lost_timer: Option<TimerId>,
    silent: bool,
    /// The highest turn, response and call numbers seen, for making up
    /// events that almost make sense.
    seen: (u32, u32, u64),
    tally: Tally,
    log: Option<Vec<String>>,
    /// The session as the kernel was given it, when it is being written
    /// down.
    written: Option<Trace>,
}

impl Run {
    fn new(profile: Profile, seed: u64, log: bool) -> Self {
        let mut rng = SimRng::new(Seed(seed));
        let faults = profile.faults();

        let mut config = KernelConfig::default();
        let mut pick = |options: &[u64]| *rng.choose(options).unwrap_or(&1);
        config.limits.participants = usize::try_from(pick(&[2, 8])).unwrap_or(8);
        config.limits.open_turns = usize::try_from(pick(&[1, 2, 8])).unwrap_or(8);
        config.limits.generations = usize::try_from(pick(&[1, 4])).unwrap_or(4);
        config.limits.pending_text = usize::try_from(pick(&[48, 400, 8192])).unwrap_or(8192);
        config.timing.speech_credit =
            MediaDur::from_millis(u32::try_from(pick(&[300, 3_000])).unwrap_or(3_000));
        config.timing.transcript_wait = Duration::from_millis(pick(&[500, 2_000]));
        config.timing.turn_max = Duration::from_millis(pick(&[6_000, 120_000]));
        config.timing.generation_max = Duration::from_millis(pick(&[4_000, 60_000]));
        config.timing.drain = Duration::from_millis(pick(&[500, 5_000]));
        config.timing.grace = Duration::from_millis(pick(&[500, 2_000]));
        config.timing.rollback_wait = Duration::from_millis(pick(&[300, 2_000]));
        config.timing.stage_wait = Duration::from_millis(pick(&[400, 5_000]));
        config.limits.open_calls = usize::try_from(pick(&[1, 3, 16])).unwrap_or(16);
        if profile == Profile::Lines {
            // Asked only here, so that the sessions of the other profiles
            // stay the ones they were.
            config.timing.detach_wait = Duration::from_millis(pick(&[0, 1_000, 5_000, 5_000]));
        }

        // The tools: some run by the application, and perhaps one that a
        // second agent answers.
        for _ in 0..pick(&[0, 1, 2, 4]) {
            let effect = match pick(&[0, 0, 1, 2]) {
                0 => Effect::Pure,
                1 => Effect::Idempotent,
                _ => Effect::SideEffecting,
            };
            let gate = if pick(&[0, 0, 0, 0, 1]) == 1 {
                Gate::AfterOwnUtterance
            } else {
                Gate::None
            };
            config.tools.push(ToolSpec {
                effect,
                gate,
                timeout: Duration::from_millis(pick(&[600, 3_000, 10_000])),
                executor: Executor::External,
            });
        }
        let mut agents = vec![AgentSpec::default()];
        if pick(&[0, 0, 1]) == 1 {
            agents.push(AgentSpec::default());
            config.tools.push(ToolSpec {
                effect: Effect::Pure,
                gate: Gate::None,
                timeout: Duration::from_millis(pick(&[1_500, 30_000])),
                executor: Executor::Agent(AgentId(1)),
            });
        }
        let all = (0..config.tools.len())
            .filter_map(|index| u16::try_from(index).ok().map(ToolId))
            .collect::<Vec<_>>();
        if let Some(front) = agents.first_mut() {
            front.tools.clone_from(&all);
        }
        // Perhaps another agent that can be given the floor.
        if pick(&[0, 0, 1]) == 1 {
            agents.push(AgentSpec { tools: all });
        }
        config.agents = agents;
        let lag = pick(&[1, 10, 40]);

        let named = rng.chance(1, 3);
        // The configuration is valid by construction; a default kernel
        // stands in if that ever stops being true, and the run goes on.
        let kernel = Kernel::new(config.clone(), Self::policies(named)).unwrap_or_default();

        let mut run = Self {
            monitor: Monitor::new(kernel.view()),
            journal: Journal::new(kernel.snapshot()),
            config: kernel.config().clone(),
            kernel,
            named,
            emitted: Vec::new(),
            lag,
            vouched: 0,
            effects: Vec::new(),
            out: Out::new(),
            now: 0,
            queue: Schedule::new(),
            rng,
            faults,
            people: Vec::new(),
            jobs: Vec::new(),
            timers: Vec::new(),
            armings: 0,
            lost_timer: None,
            silent: false,
            seen: (0, 0, 0),
            tally: Tally {
                runs: 1,
                ..Tally::default()
            },
            log: log.then(Vec::new),
            written: None,
        };
        run.plan(profile);
        run
    }

    fn policies(named: bool) -> BoxedPolicies {
        let mut policies = BoxedPolicies::default();
        if named {
            policies.addressee = Box::new(AddressedByName::new(&["agent"]));
        }
        policies
    }

    /// The agents that can be given the floor: those that answer no tool
    /// calls for others.
    fn speakers(&self) -> Vec<AgentId> {
        (0..self.config.agents.len())
            .filter_map(|index| u8::try_from(index).ok().map(AgentId))
            .filter(|&agent| {
                self.config
                    .tools
                    .iter()
                    .all(|tool| tool.executor != Executor::Agent(agent))
            })
            .collect()
    }

    /// Decides what the people and the application do.
    #[expect(
        clippy::too_many_lines,
        reason = "a list of things that may happen, one after the other"
    )]
    fn plan(&mut self, profile: Profile) {
        let horizon = self.rng.range(8_000..40_000);
        let people = self.rng.range(1..4);
        for number in 1..=people {
            let id = ParticipantId(u16::try_from(number).unwrap_or(1));
            let joins = if number == 1 {
                0
            } else {
                self.rng.range(0..horizon / 2)
            };
            self.at(
                joins,
                Item::Control(Event::Control(ControlInput::Join { who: id })),
            );
            if self.rng.chance(1, 4) {
                let leaves = self.rng.range(horizon / 2..horizon);
                self.at(
                    leaves,
                    Item::Control(Event::Control(ControlInput::Leave { who: id })),
                );
            }
            for _ in 0..self.rng.range(1..5) {
                let long = self.rng.chance(2, 3);
                let when = self.rng.range(joins + 100..horizon);
                self.at(when, Item::Speak { who: id, long });
            }
            if self.rng.chance(1, 3) {
                let text = self.sentence().into();
                let when = self.rng.range(joins..horizon);
                self.at(when, Item::Control(Event::TextIn { who: id, text }));
            }
        }
        for _ in 0..self.rng.range(0..3) {
            let text = self.sentence().into();
            let when = self.rng.range(0..horizon);
            self.at(
                when,
                Item::Control(Event::Control(ControlInput::Say { text })),
            );
        }
        if self.rng.chance(1, 3) {
            let when = self.rng.range(0..horizon);
            self.at(when, Item::Control(Event::Control(ControlInput::Interrupt)));
        }
        if self.rng.chance(1, 5) {
            let when = self.rng.range(0..horizon);
            self.at(when, Item::Control(Event::Control(ControlInput::Detached)));
            self.at(
                when + 700,
                Item::Control(Event::Control(ControlInput::Attached)),
            );
        }
        for _ in 0..self.rng.range(0..self.faults.nonsense + 1) {
            let when = self.rng.range(0..horizon + 10_000);
            self.at(when, Item::Nonsense);
        }

        // The floor is handed from agent to agent, and the parts that
        // serve the new one are ready soon, or never.
        let speakers = self.speakers();
        if speakers.len() > 1 {
            for _ in 0..self.rng.range(0..3) {
                let to = *self.rng.choose(&speakers).unwrap_or(&AgentId(0));
                let context = match self.rng.range(0..3) {
                    0 => HandoffContext::Full,
                    1 => HandoffContext::Summary(self.sentence().into()),
                    _ => HandoffContext::None,
                };
                let when = self.rng.range(0..horizon);
                let handoff = ControlInput::Handoff { to, context };
                self.at(when, Item::Control(Event::Control(handoff)));
                let lost = self.faults.lost;
                if !self.fails(lost.max(100)) {
                    let ready = when + self.rng.range(20..900);
                    self.at(
                        ready,
                        Item::Control(Event::Control(ControlInput::StageReady)),
                    );
                }
            }
        }

        // The journal keeps a snapshot now and then, and is checked.
        for _ in 0..self.rng.range(0..4) {
            let when = self.rng.range(0..horizon + 5_000);
            self.at(when, Item::Snapshot);
        }
        for _ in 0..self.rng.range(1..4) {
            let when = self.rng.range(0..horizon + 5_000);
            self.at(when, Item::Check);
        }
        if profile == Profile::Crashes {
            for _ in 0..self.rng.range(1..4) {
                let when = self.rng.range(200..horizon + 3_000);
                self.at(when, Item::Crash);
            }
        }
        if profile == Profile::Lines {
            for _ in 0..self.rng.range(1..4) {
                let when = self.rng.range(200..horizon + 2_000);
                self.at(when, Item::Control(Event::Control(ControlInput::Detached)));
                // Back in time, too late, or never.
                if self.rng.chance(5, 6) {
                    let back = when + self.rng.range(30..2_500);
                    self.at(back, Item::Control(Event::Control(ControlInput::Attached)));
                }
            }
        }

        let closes = match profile {
            Profile::Shutdown => Some(self.rng.range(300..horizon)),
            _ if self.rng.chance(1, 4) => Some(self.rng.range(horizon / 2..horizon + 5_000)),
            _ => None,
        };
        if let Some(when) = closes {
            let reason = *self
                .rng
                .choose(&[
                    CloseReason::Requested,
                    CloseReason::Abandoned,
                    CloseReason::Failure,
                ])
                .unwrap_or(&CloseReason::Requested);
            self.at(
                when,
                Item::Control(Event::Control(ControlInput::Close { reason })),
            );
            if profile == Profile::Shutdown {
                if self.rng.chance(2, 3) {
                    let when = self.rng.range(0..when + 3_000);
                    self.at(when, Item::Silence);
                }
                if self.rng.chance(1, 2) {
                    let lost = if self.rng.chance(1, 2) {
                        TimerId::Drain
                    } else {
                        TimerId::Grace
                    };
                    self.lost_timer = Some(lost);
                }
            }
        }
    }

    fn at(&mut self, when: Ms, item: Item) {
        self.queue.push(SimTime::from_millis(when), item);
    }

    /// A delay within `range`, plus whatever the profile adds.
    fn delay(&mut self, range: core::ops::Range<Ms>) -> Ms {
        self.rng.range(range) + self.rng.range(0..self.faults.jitter + 1)
    }

    fn fails(&mut self, chance: u64) -> bool {
        self.rng.chance(chance, 1_000)
    }

    fn words(&mut self, count: u64) -> String {
        let mut text = String::new();
        for index in 0..count.max(1) {
            if index > 0 {
                text.push(' ');
            }
            text.push_str(self.rng.choose(WORDS).unwrap_or(&"ok"));
        }
        text
    }

    fn sentence(&mut self) -> String {
        let count = self.rng.range(1..9);
        let mut text = self.words(count);
        text.push_str(self.rng.choose(&[". ", "? ", "! ", ".\n"]).unwrap_or(&". "));
        text
    }

    // ---- Stepping the kernel -----------------------------------------------

    fn step(&mut self, event: &Event) -> Result<(), Violation> {
        self.tally.steps += 1;
        let now = MonoTime::from_millis(self.now);
        let line = self.kernel.view().is_attached();
        if let Event::Durable(seq) = event {
            // Whatever the kernel is told is stored has to be: the journal
            // is taken at its word.
            let emitted = u64::try_from(self.emitted.len()).unwrap_or(u64::MAX);
            self.vouched = self.vouched.max(seq.get().min(emitted));
        }
        self.kernel.step(now, event, &mut self.out);
        self.journal.record(&self.out);
        self.emitted.extend(self.out.facts.iter().map(|_| self.now));
        if let Some(written) = &mut self.written {
            written.event(now, event);
        }
        if let Some(log) = &mut self.log {
            log.push(format!("{:>7} ms  <- {event:?}", self.now));
            for stamped in &self.out.facts {
                let text = stamped
                    .fact
                    .text()
                    .map_or("", |text| self.out.text.get(text));
                log.push(format!(
                    "               == #{} {:?} {text:?}",
                    stamped.seq, stamped.fact
                ));
            }
            for command in &self.out.cmds {
                let text = command.text().map_or("", |text| self.out.text.get(text));
                log.push(format!("               -> {command:?} {text:?}"));
            }
        }
        self.monitor
            .observe(now, event, &self.out, self.kernel.view())?;
        for command in &self.out.cmds {
            let Command::Tool(call, ToolCmd::Dispatch { tool, .. }) = *command else {
                continue;
            };
            // What no single life of the session can see: a call that
            // must happen at most once, sent in two of them.
            let once = self
                .config
                .tool(tool)
                .is_some_and(|spec| spec.effect == Effect::SideEffecting);
            if once && self.effects.contains(&call) {
                return Err(Violation {
                    rule: Rule::Tools,
                    what: format!(
                        "call {call} must happen at most once, and was sent in two lives of the \
                         session"
                    ),
                });
            }
            if once {
                self.effects.push(call);
            }
        }

        let emitted = self.out.cmds.len() << 16 | self.out.facts.len() << 8 | self.out.text.len();
        self.tally.trace = mix(self.tally.trace, u64::try_from(emitted).unwrap_or(u64::MAX));
        if self.tally.steps.is_multiple_of(64) {
            self.tally.trace = mix(self.tally.trace, self.kernel.fingerprint());
        }
        for index in 0..self.out.facts.len() {
            if let Some(stamped) = self.out.facts.get(index) {
                let fact = stamped.fact;
                self.count(&fact);
            }
        }
        self.count_line(event, line);
        for index in 0..self.out.cmds.len() {
            if let Some(&command) = self.out.cmds.get(index) {
                self.react(command);
            }
        }
        Ok(())
    }

    /// Counts what the step did about the line, which was there before
    /// the step if `line` says so.
    fn count_line(&mut self, event: &Event, line: bool) {
        let view = self.kernel.view();
        let running = view.lifecycle() == Lifecycle::Running;
        let tally = &mut self.tally;
        match (line, view.is_attached()) {
            (true, false) => tally.drops += 1,
            (false, true) if running => {
                tally.returns += 1;
                let started = |command: &Command| {
                    matches!(command, Command::Llm(_, LlmCmd::Start { cause, .. })
                        if !matches!(cause, LlmCause::Delegated { .. }))
                };
                tally.resumed += u64::from(self.out.cmds.iter().any(started));
            }
            _ => {}
        }
        // Nobody asked a session to close that closes on being told of
        // its line, or by the timer of its wait.
        let told = matches!(
            event,
            Event::Timer(TimerId::Detach) | Event::Control(ControlInput::Detached)
        );
        let closes = |stamped: &waveport_domain::fact::Stamped| {
            matches!(
                stamped.fact,
                Fact::LifecycleChanged {
                    to: Lifecycle::Draining,
                    ..
                }
            )
        };
        tally.abandoned += u64::from(told && self.out.facts.iter().any(closes));
    }

    fn count(&mut self, fact: &Fact) {
        let tally = &mut self.tally;
        match *fact {
            Fact::TurnCommitted { .. } => tally.turns += 1,
            Fact::TurnDismissed { reason, .. } => match reason {
                Dismissal::Backchannel | Dismissal::Noise => tally.asides += 1,
                Dismissal::Empty => tally.empty += 1,
                Dismissal::Closed => {}
            },
            Fact::UtteranceCommitted { truncated, .. } => {
                if truncated {
                    tally.cut += 1;
                } else {
                    tally.spoken += 1;
                }
            }
            Fact::EpochAdvanced { cause, .. } => match cause {
                EpochCause::BargeIn => tally.barge_ins += 1,
                EpochCause::Superseded => tally.superseded += 1,
                EpochCause::Failure => tally.failures += 1,
                _ => {}
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Closed,
                ..
            } => tally.closed += 1,
            Fact::ToolProposed { .. } => tally.calls += 1,
            Fact::ToolDispatched { attempt, .. } if attempt > 0 => tally.resent += 1,
            Fact::ToolFinished { outcome, .. } => match outcome {
                ToolOutcome::Completed => tally.completed += 1,
                ToolOutcome::Failed => tally.failed += 1,
                ToolOutcome::Cancelled => tally.withdrawn += 1,
                ToolOutcome::InDoubt => tally.in_doubt += 1,
            },
            Fact::AgentActivated { .. } => tally.handoffs += 1,
            _ => {}
        }
    }

    // ---- The adapters ------------------------------------------------------

    fn job(&mut self, tag: OpTag) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.tag == tag)
    }

    /// The adapters' side of the response `tag`, begun if it is new.
    fn job_or_new(&mut self, tag: OpTag) -> usize {
        if let Some(index) = self.jobs.iter().position(|job| job.tag == tag) {
            return index;
        }
        self.seen.1 = self.seen.1.max(tag.generation.get());
        let (lost, stale) = (self.faults.lost, self.faults.stale);
        let job = Job {
            tag,
            pieces: Vec::new(),
            next: 0,
            called: false,
            ending: Ending::Done,
            paused: false,
            held: false,
            cancelled: false,
            deaf: self.fails(stale),
            mute: self.fails(lost),
            blind: self.fails(lost),
            asked: (0, 0),
            ready_at: 0,
            marks: Vec::new(),
            playing: false,
            position: 0,
            reported_at: 0,
        };
        // Only the responses under way matter; the list stays short.
        if self.jobs.len() >= 8 {
            self.jobs.remove(0);
        }
        self.jobs.push(job);
        self.jobs.len() - 1
    }

    /// What the language model is going to write for a response, and the
    /// tool calls it is going to ask for. `floor` says that the response
    /// is one that speaks.
    fn write(&mut self, floor: bool) -> (Vec<Piece>, Ending) {
        let mut text = String::new();
        if !self.rng.chance(1, 12) {
            for _ in 0..self.rng.range(1..6) {
                let sentence = self.sentence();
                text.push_str(&sentence);
            }
            if self.rng.chance(1, 5) {
                // It stops in the middle of a sentence.
                let count = self.rng.range(1..5);
                let more = self.words(count);
                text.push_str(&more);
            }
        } else if self.rng.chance(1, 2) {
            text.push_str("  \n");
        }
        let mut pieces = Vec::new();
        let mut piece = String::new();
        let mut size = self.rng.range(1..25);
        for character in text.chars() {
            piece.push(character);
            size -= 1;
            if size == 0 {
                pieces.push(Piece::Text(core::mem::take(&mut piece)));
                size = self.rng.range(1..25);
            }
        }
        if !piece.is_empty() {
            pieces.push(Piece::Text(piece));
        }
        // A model that speaks asks for tools now and then; one that
        // answers a call for another agent has none, and asks anyway once
        // in a while.
        let tools = u64::try_from(self.config.tools.len()).unwrap_or(0);
        let asks = if floor {
            tools > 0 && self.rng.chance(2, 5)
        } else {
            self.rng.chance(1, 10)
        };
        if asks {
            for _ in 0..self.rng.range(1..4) {
                // Now and then a tool that does not exist.
                let tool = ToolId(u16::try_from(self.rng.range(0..tools + 1)).unwrap_or(0));
                let args = format!("{{\"n\":{}}}", self.rng.range(0..100));
                let most = u64::try_from(pieces.len()).unwrap_or(0);
                let at = usize::try_from(self.rng.range(0..most + 1)).unwrap_or(0);
                pieces.insert(at.min(pieces.len()), Piece::Call(tool, args));
            }
        }
        let lost = self.faults.lost;
        let (hangs, fails) = (self.fails(lost), self.fails(lost));
        let ending = if hangs {
            let most = u64::try_from(pieces.len()).unwrap_or(0);
            let keep = usize::try_from(self.rng.range(0..most + 1)).unwrap_or(0);
            pieces.truncate(keep);
            Ending::Hangs
        } else if fails {
            Ending::Fails
        } else {
            Ending::Done
        };
        (pieces, ending)
    }

    /// The world's answer to a command.
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per command, each a few lines"
    )]
    fn react(&mut self, command: Command) {
        let now = self.now;
        let lost = self.faults.lost;
        match command {
            Command::SetTimer(id, Some(at)) => {
                self.timers.retain(|(armed, _)| *armed != id);
                self.armings += 1;
                self.timers.push((id, self.armings));
                if self.lost_timer != Some(id) {
                    let due = at.as_nanos().div_ceil(1_000_000).max(now);
                    let late = self.rng.range(0..self.faults.late + 1);
                    self.at(due + late, Item::Timer(id, self.armings));
                }
            }
            Command::SetTimer(id, None) => self.timers.retain(|(armed, _)| *armed != id),
            Command::Sense(SenseCmd::EstimateEou { turn, .. }) => {
                if !self.fails(lost) {
                    let points = if self.rng.chance(2, 3) {
                        self.rng.range(8_000..10_001)
                    } else {
                        self.rng.range(0..5_000)
                    };
                    let p = Prob::from_basis_points(u16::try_from(points).unwrap_or(0));
                    let when = now + self.delay(20..150);
                    self.at(when, Item::Report(Event::Eou { turn, p }));
                }
            }
            Command::Sense(SenseCmd::ClassifyOverlap { turn, .. }) => {
                if !self.fails(lost) {
                    let verdict = match self.rng.range(0..20) {
                        0..10 => OverlapVerdict::BargeIn,
                        10..17 => OverlapVerdict::Backchannel,
                        _ => OverlapVerdict::Noise,
                    };
                    let when = now + self.delay(60..450);
                    self.at(when, Item::Report(Event::Overlap { turn, verdict }));
                }
            }
            Command::Llm(tag, llm) => {
                let index = self.job_or_new(tag);
                match llm {
                    LlmCmd::Start { cause, .. } => {
                        let floor = match cause {
                            LlmCause::Reply { .. } => true,
                            LlmCause::Continuation { .. } => {
                                self.tally.continuations += 1;
                                true
                            }
                            LlmCause::Delegated { .. } => {
                                self.tally.delegated += 1;
                                false
                            }
                        };
                        if floor {
                            self.tally.replies += 1;
                        }
                        let (pieces, ending) = self.write(floor);
                        if let Some(job) = self.jobs.get_mut(index) {
                            job.pieces = pieces;
                            job.ending = ending;
                        }
                        let when = now + self.delay(80..400);
                        self.at(when, Item::Write(tag));
                    }
                    LlmCmd::Pause => {
                        self.tally.pauses += 1;
                        if let Some(job) = self.jobs.get_mut(index) {
                            job.paused = true;
                        }
                    }
                    LlmCmd::Resume => {
                        let held = self.jobs.get_mut(index).is_some_and(|job| {
                            job.paused = false;
                            core::mem::take(&mut job.held)
                        });
                        if held {
                            let when = now + self.delay(5..60);
                            self.at(when, Item::Write(tag));
                        }
                    }
                    LlmCmd::Cancel => {
                        if let Some(job) = self.jobs.get_mut(index) {
                            job.cancelled = true;
                        }
                    }
                }
            }
            Command::Tts(tag, tts) => {
                let index = self.job_or_new(tag);
                let wait = self.delay(40..200);
                let halves = self.rng.chance(1, 3);
                let Some(job) = self.jobs.get_mut(index) else {
                    return;
                };
                match tts {
                    TtsCmd::Speak { text } => {
                        let bytes = u32::try_from(text.len()).unwrap_or(u32::MAX);
                        let before = job.asked;
                        job.asked = (
                            before.0.saturating_add(bytes),
                            before
                                .1
                                .saturating_add(bytes.saturating_mul(AUDIO_PER_BYTE)),
                        );
                        if job.mute {
                            return;
                        }
                        let ready = job.ready_at.max(now) + wait;
                        job.ready_at = ready;
                        let all = job.asked;
                        if halves {
                            let half = Item::Segment {
                                tag,
                                text_end: before.0 + bytes / 2,
                                audio_ms: before.1 + bytes / 2 * AUDIO_PER_BYTE,
                                last: false,
                            };
                            self.at(ready, half);
                        }
                        let whole = Item::Segment {
                            tag,
                            text_end: all.0,
                            audio_ms: all.1,
                            last: false,
                        };
                        self.at(ready, whole);
                    }
                    TtsCmd::Finish => {
                        if job.mute {
                            return;
                        }
                        let ready = job.ready_at.max(now) + wait / 4;
                        job.ready_at = ready;
                        let end = Item::Segment {
                            tag,
                            text_end: job.asked.0,
                            audio_ms: job.asked.1,
                            last: true,
                        };
                        self.at(ready, end);
                    }
                    TtsCmd::Cancel => job.cancelled = true,
                }
            }
            Command::Playout(tag, playout) => {
                let index = self.job_or_new(tag);
                let Some(job) = self.jobs.get_mut(index) else {
                    return;
                };
                match playout {
                    PlayoutCmd::Start => {
                        job.playing = true;
                        job.reported_at = now;
                        if !job.blind {
                            self.at(now + REPORT_EVERY, Item::Tick(tag));
                        }
                        // The agent speaking is when people speak over it.
                        if self.rng.chance(1, 2)
                            && let Some(&person) = self.rng.choose(&self.people)
                        {
                            let long = self.rng.chance(1, 2);
                            let when = now + self.rng.range(200..4_000);
                            self.at(
                                when,
                                Item::Speak {
                                    who: person.id,
                                    long,
                                },
                            );
                        }
                    }
                    PlayoutCmd::Stop { .. } => job.playing = false,
                    PlayoutCmd::Duck | PlayoutCmd::Unduck => {}
                }
            }
            Command::Emit(waveport_domain::command::ClientEvent::UserSpeaking { turn, .. }) => {
                self.seen.0 = self.seen.0.max(turn.get());
            }
            Command::Tool(call, ToolCmd::Dispatch { .. }) => {
                self.tally.sent += 1;
                self.seen.2 = self.seen.2.max(call.get());
                if self.fails(lost) {
                    return;
                }
                // Most tools answer soon, and some take their time.
                let slow = if self.rng.chance(1, 6) {
                    self.rng.range(0..12_000)
                } else {
                    0
                };
                let when = now + self.delay(20..1_500) + slow;
                let news = if self.rng.chance(1, 6) {
                    ToolEvent::Failed {
                        class: RetryClass::Transient,
                        message: self.words(3).into(),
                    }
                } else {
                    ToolEvent::Completed {
                        result: format!("{{\"call\":{call}}}").into(),
                    }
                };
                self.at(when, Item::Report(Event::Tool(call, news)));
            }
            Command::Tool(call, ToolCmd::Cancel) => {
                // Some tools undo what they did and say so.
                if self.rng.chance(1, 2) && !self.fails(lost) {
                    let when = now + self.delay(20..900);
                    self.at(when, Item::Report(Event::Tool(call, ToolEvent::RolledBack)));
                }
            }
            Command::AwaitDurable(seq) => {
                if !self.fails(lost) {
                    let when = now + self.lag + self.rng.range(0..self.faults.jitter / 8 + 1);
                    self.at(when, Item::Stored(seq.get()));
                }
            }
            Command::Emit(_) | Command::Stt(_) => {}
        }
    }

    // ---- What happens in the world -----------------------------------------

    /// Delivers a report, sometimes twice.
    fn report(&mut self, event: &Event) -> Result<(), Violation> {
        self.step(event)?;
        let twice = self.faults.twice;
        if self.fails(twice) {
            if self.rng.chance(1, 2) {
                self.step(event)?;
            } else {
                let when = self.now + self.rng.range(1..300);
                self.at(when, Item::Report(event.clone()));
            }
        }
        Ok(())
    }

    fn happen(&mut self, item: Item) -> Result<(), Violation> {
        let adapter = !matches!(
            item,
            Item::Control(_)
                | Item::Timer(..)
                | Item::Silence
                | Item::Snapshot
                | Item::Check
                | Item::Crash
        );
        if self.silent && adapter {
            return Ok(());
        }
        match item {
            Item::Stored(seq) => self.report(&Event::Durable(Seq(seq))),
            Item::Snapshot => {
                self.journal.keep(self.kernel.snapshot());
                self.check()
            }
            Item::Check => self.check(),
            Item::Crash => self.crash(),
            Item::Report(event) => self.report(&event),
            Item::Control(event) => {
                if let Event::Control(ControlInput::Join { who }) = &event
                    && self.people.iter().all(|person| person.id != *who)
                {
                    self.people.push(Person {
                        id: *who,
                        busy_until: 0,
                        voice_at: 0,
                        words_at: 0,
                    });
                }
                self.step(&event)
            }
            Item::Timer(id, arming) => {
                let armed = self.timers.contains(&(id, arming));
                self.timers.retain(|timer| *timer != (id, arming));
                let stale = self.faults.stale;
                // A timer that was cancelled or set again may fire anyway.
                if armed || self.fails(stale) {
                    self.step(&Event::Timer(id))
                } else {
                    Ok(())
                }
            }
            Item::Write(tag) => self.written(tag),
            Item::Segment {
                tag,
                text_end,
                audio_ms,
                last,
            } => {
                let Some(job) = self.job(tag) else {
                    return Ok(());
                };
                if job.cancelled && !job.deaf {
                    return Ok(());
                }
                job.marks.push((text_end, audio_ms));
                let segment = OpEvent::TtsSegment {
                    text_end,
                    audio_end: MediaDur::from_millis(audio_ms),
                    last,
                };
                self.report(&Event::Op(tag, segment))
            }
            Item::Tick(tag) => self.played(tag),
            Item::Speak { who, long } => {
                self.speak(who, long);
                Ok(())
            }
            Item::Nonsense => {
                let event = self.nonsense();
                self.step(&event)
            }
            Item::Silence => {
                self.silent = true;
                Ok(())
            }
        }
    }

    /// The language model writes its next piece, or ends.
    fn written(&mut self, tag: OpTag) -> Result<(), Violation> {
        let Some(job) = self.job(tag) else {
            return Ok(());
        };
        if job.cancelled && !job.deaf {
            return Ok(());
        }
        if job.paused && !job.deaf {
            job.held = true;
            return Ok(());
        }
        let op = if let Some(piece) = job.pieces.get(job.next) {
            job.next += 1;
            match piece {
                Piece::Text(text) => OpEvent::LlmDelta {
                    text: text.as_str().into(),
                },
                Piece::Call(tool, args) => {
                    job.called = true;
                    OpEvent::ToolProposed {
                        tool: *tool,
                        args: args.as_str().into(),
                    }
                }
            }
        } else {
            match job.ending {
                Ending::Done => OpEvent::LlmDone {
                    finish: if job.called {
                        Finish::ToolCalls
                    } else {
                        Finish::Stop
                    },
                },
                Ending::Fails => OpEvent::Failed {
                    part: Part::Think,
                    class: RetryClass::Transient,
                },
                Ending::Hangs => return Ok(()),
            }
        };
        if matches!(op, OpEvent::LlmDelta { .. } | OpEvent::ToolProposed { .. }) {
            let when = self.now + self.delay(5..60);
            self.at(when, Item::Write(tag));
        }
        self.report(&Event::Op(tag, op))
    }

    /// Checks that the journal alone adds up to what the kernel holds.
    fn check(&mut self) -> Result<(), Violation> {
        self.tally.checks += 1;
        self.journal.check(&self.kernel)
    }

    /// The process dies. Everything the adapters were doing is gone, and
    /// so is what the journal had not stored. The session is rebuilt from
    /// the rest and wakes when its client is back.
    fn crash(&mut self) -> Result<(), Violation> {
        self.check()?;
        self.tally.crashes += 1;
        let now = self.now;
        let by_now = self
            .emitted
            .iter()
            .take_while(|&&at| at + self.lag <= now)
            .count();
        let stored = u64::try_from(by_now).unwrap_or(u64::MAX).max(self.vouched);
        let total = self.emitted.len();
        self.emitted
            .truncate(usize::try_from(stored).unwrap_or(usize::MAX));
        self.journal.cut(Seq(stored));
        if let Some(log) = &mut self.log {
            log.push(format!(
                "{now:>7} ms  THE PROCESS DIES: {stored} of {total} facts had been stored"
            ));
        }
        if let Some(written) = &mut self.written {
            written.restart(Seq(stored));
        }

        let Some((snapshot, tail, text)) = self.journal.latest() else {
            return Ok(());
        };
        let policies = Self::policies(self.named);
        let rebuilt = Kernel::restore(self.config.clone(), policies, snapshot, tail, text);
        let kernel = rebuilt.map_err(|error| Violation {
            rule: Rule::Restore,
            what: format!("the session cannot be rebuilt from its journal: {error}"),
        })?;
        self.monitor = Monitor::restored(kernel.view(), self.journal.speech());
        self.kernel = kernel;
        self.vouched = stored;

        // What the people and the application are going to do is still
        // going to happen. What the adapters were doing is not.
        self.jobs.clear();
        self.timers.clear();
        let mut kept = Vec::new();
        while let Some((due, item)) = self.queue.pop() {
            let survives = matches!(
                item,
                Item::Control(_)
                    | Item::Speak { .. }
                    | Item::Nonsense
                    | Item::Silence
                    | Item::Snapshot
                    | Item::Check
                    | Item::Crash
            );
            if survives {
                kept.push((due, item));
            }
        }
        for (due, item) in kept {
            self.queue.push(due, item);
        }
        let back = now + self.rng.range(20..1_500);
        self.at(back, Item::Control(Event::Control(ControlInput::Attached)));
        Ok(())
    }

    /// Playback reports how far it has got.
    fn played(&mut self, tag: OpTag) -> Result<(), Violation> {
        let now = self.now;
        let live = self.kernel.view().is_live(tag);
        let Some(job) = self.job(tag) else {
            return Ok(());
        };
        if !((job.playing && live) || job.deaf) {
            return Ok(());
        }
        let elapsed = u32::try_from(now - job.reported_at).unwrap_or(u32::MAX);
        job.reported_at = now;
        let audio = job.audio();
        job.position = job.position.saturating_add(elapsed).min(audio);
        let played = OpEvent::Played {
            handed: MediaDur::from_millis((job.position + 20).min(audio)),
            acked: MediaDur::from_millis(job.position),
            heard_text: job.heard(),
            drained: job.position == audio,
        };
        if job.playing && live {
            self.at(now + REPORT_EVERY, Item::Tick(tag));
        }
        self.report(&Event::Op(tag, played))
    }

    /// Someone says something: their voice starts and stops, perhaps more
    /// than once, and transcripts of it arrive.
    fn speak(&mut self, who: ParticipantId, long: bool) {
        let now = self.now;
        let Some(index) = self.people.iter().position(|person| person.id == who) else {
            return;
        };
        let Some(mut person) = self.people.get(index).copied() else {
            return;
        };
        if person.busy_until > now {
            return;
        }
        let (lost, edge_jitter) = (self.faults.lost, self.faults.jitter / 4 + 1);
        let stretches = if long { self.rng.range(1..4) } else { 1 };
        let mut from = now;
        for stretch in 0..stretches {
            let length = if long {
                self.rng.range(500..2_500)
            } else {
                self.rng.range(150..450)
            };
            let to = from + length;
            let middle = from + length / 2;
            let at = MediaTime::from_millis;
            let started = (from + self.rng.range(0..edge_jitter)).max(person.voice_at);
            self.at(
                started,
                Item::Report(Event::Speech {
                    who,
                    edge: VadEdge::Start,
                    at: at(from),
                }),
            );
            person.voice_at = started;
            // Now and then nobody notices that the last stretch ended.
            let stuck = stretch + 1 == stretches && self.fails(lost / 4);
            if !stuck {
                let stopped = (to + self.rng.range(0..edge_jitter)).max(started);
                self.at(
                    stopped,
                    Item::Report(Event::Speech {
                        who,
                        edge: VadEdge::End,
                        at: at(to),
                    }),
                );
                person.voice_at = stopped;
            }
            if !self.fails(lost) {
                let first = self.words(length / 600);
                let second = self.words(length / 600);
                let transcript = |from: Ms, to: Ms, text: &str, is_final: bool| {
                    Item::Report(Event::Transcript {
                        who,
                        span: MediaSpan::new(at(from), at(to)),
                        text: text.into(),
                        is_final,
                    })
                };
                if self.rng.chance(1, 2) {
                    let when = middle + self.delay(50..200);
                    self.at(when, transcript(from, middle, &first, false));
                }
                let arrives = (to + self.delay(80..500)).max(person.words_at);
                person.words_at = arrives;
                if self.rng.chance(1, 3) {
                    self.at(arrives, transcript(from, middle, &first, true));
                    self.at(arrives, transcript(middle, to, &second, true));
                } else {
                    let both = format!("{first} {second}");
                    self.at(arrives, transcript(from, to, &both, true));
                }
            }
            from = to + self.rng.range(80..1_800);
        }
        person.busy_until = from;
        if let Some(slot) = self.people.get_mut(index) {
            *slot = person;
        }
    }

    /// An event that makes no sense, or almost makes sense.
    #[expect(clippy::too_many_lines, reason = "one arm per kind of event")]
    fn nonsense(&mut self) -> Event {
        let turn =
            TurnId(u32::try_from(self.rng.range(0..u64::from(self.seen.0) + 3)).unwrap_or(0));
        let generation =
            GenId(u32::try_from(self.rng.range(0..u64::from(self.seen.1) + 3)).unwrap_or(0));
        let live = self
            .kernel
            .view()
            .generations()
            .next()
            .map(|running| running.tag);
        let tag = match (live, self.rng.range(0..4)) {
            (Some(tag), 0 | 1) => tag,
            (Some(tag), 2) => OpTag {
                generation: tag.generation,
                epoch: Epoch(tag.epoch.get().wrapping_add(1)),
            },
            _ => OpTag {
                generation,
                epoch: self.kernel.view().epoch(),
            },
        };
        let who = ParticipantId(u16::try_from(self.rng.range(0..6)).unwrap_or(0));
        let any = self.rng.next_u64();
        let small = u32::try_from(self.rng.range(0..5_000)).unwrap_or(0);
        let number = if self.rng.chance(1, 4) {
            u32::MAX
        } else {
            small
        };
        let coin = self.rng.chance(1, 2);
        let call = ToolCallId(self.rng.range(0..self.seen.2 + 3));
        let tools = u64::try_from(self.config.tools.len()).unwrap_or(0);
        let agents = u64::try_from(self.config.agents.len()).unwrap_or(0);
        match self.rng.range(0..16) {
            0 => Event::Eou {
                turn,
                p: Prob::from_basis_points(u16::try_from(any % 20_000).unwrap_or(0)),
            },
            1 => Event::Overlap {
                turn,
                verdict: if coin {
                    OverlapVerdict::BargeIn
                } else {
                    OverlapVerdict::Noise
                },
            },
            2 => Event::Timer(match self.rng.range(0..10) {
                0 => TimerId::Eou(turn),
                1 => TimerId::TurnMax(turn),
                2 => TimerId::Overlap(turn),
                3 => TimerId::Gen(generation),
                4 => TimerId::Checkpoint,
                5 => TimerId::Drain,
                6 => TimerId::Grace,
                7 => TimerId::StageAck,
                8 => TimerId::Rollback(call),
                _ => TimerId::ToolTimeout(call),
            }),
            3 => Event::Op(
                tag,
                OpEvent::LlmDelta {
                    text: self.sentence().into(),
                },
            ),
            4 => Event::Op(
                tag,
                OpEvent::LlmDone {
                    finish: if coin { Finish::Stop } else { Finish::Length },
                },
            ),
            5 => Event::Op(
                tag,
                OpEvent::TtsSegment {
                    text_end: number,
                    audio_end: MediaDur::from_millis(small),
                    last: coin,
                },
            ),
            6 => Event::Op(
                tag,
                OpEvent::Played {
                    handed: MediaDur::from_millis(small),
                    acked: MediaDur::from_millis(small / 2),
                    heard_text: number,
                    drained: coin,
                },
            ),
            7 => Event::Op(
                tag,
                OpEvent::Failed {
                    part: *self
                        .rng
                        .choose(&[Part::Think, Part::Speak, Part::Play])
                        .unwrap_or(&Part::Think),
                    class: RetryClass::Permanent,
                },
            ),
            8 => Event::Speech {
                who,
                edge: if coin { VadEdge::Start } else { VadEdge::End },
                at: MediaTime::from_ticks(if coin { any } else { u64::from(small) }),
            },
            9 => Event::Transcript {
                who,
                span: MediaSpan::new(
                    MediaTime::from_ticks(any >> (any % 64)),
                    MediaTime::from_millis(u64::from(small)),
                ),
                text: self.words(u64::from(small % 4)).into(),
                is_final: coin,
            },
            10 => {
                let text = if coin {
                    self.sentence()
                } else {
                    " \n".to_owned()
                };
                Event::TextIn {
                    who,
                    text: text.into(),
                }
            }
            11 => Event::Tool(
                call,
                match self.rng.range(0..3) {
                    0 => ToolEvent::Completed {
                        result: "{}".into(),
                    },
                    1 => ToolEvent::Failed {
                        class: RetryClass::Permanent,
                        message: "no".into(),
                    },
                    _ => ToolEvent::RolledBack,
                },
            ),
            12 => Event::Durable(Seq(any)),
            13 => Event::Op(
                tag,
                OpEvent::ToolProposed {
                    tool: ToolId(u16::try_from(self.rng.range(0..tools + 2)).unwrap_or(0)),
                    args: "{}".into(),
                },
            ),
            14 => Event::Control(ControlInput::Handoff {
                to: AgentId(u8::try_from(self.rng.range(0..agents + 2)).unwrap_or(0)),
                context: if coin {
                    HandoffContext::Full
                } else {
                    HandoffContext::None
                },
            }),
            _ => Event::Control(match self.rng.range(0..5) {
                0 => ControlInput::Join { who },
                1 => ControlInput::Leave { who },
                2 => ControlInput::StageReady,
                3 => ControlInput::Detached,
                _ => ControlInput::Attached,
            }),
        }
    }

    /// Runs until nothing more is going to happen, and hands what was
    /// written down of the session to `keep`.
    fn finish_with(mut self, keep: impl FnOnce(Trace)) -> Result<Tally, Violation> {
        let outcome = self.run_out();
        keep(self.written.take().unwrap_or_default());
        outcome.map(|()| self.tally)
    }

    /// Runs until nothing more is going to happen.
    fn finish(mut self) -> Result<(Tally, Vec<String>), (Violation, Vec<String>)> {
        let outcome = self.run_out();
        let log = self.log.take().unwrap_or_default();
        match outcome {
            Ok(()) => Ok((self.tally, log)),
            Err(violation) => Err((violation, log)),
        }
    }

    fn run_out(&mut self) -> Result<(), Violation> {
        let outcome = loop {
            let Some((due, item)) = self.queue.pop() else {
                break self
                    .check()
                    .and_then(|()| self.monitor.at_rest(self.kernel.view()));
            };
            self.now = self.now.max(due.as_nanos() / 1_000_000);
            if let Err(violation) = self.happen(item) {
                break Err(violation);
            }
            if self.tally.steps > MOST_STEPS {
                break Err(Violation {
                    rule: Rule::Progress,
                    what: format!("after {MOST_STEPS} steps the session has not come to rest"),
                });
            }
        };
        self.tally.trace = mix(self.tally.trace, self.kernel.fingerprint());
        outcome
    }
}

/// Runs the session that belongs to `seed` under `profile`.
///
/// # Errors
///
/// A step broke an invariant, or the session was left unfinished.
pub fn run(profile: Profile, seed: u64) -> Result<Tally, Violation> {
    Run::new(profile, seed, false)
        .finish()
        .map(|(tally, _)| tally)
        .map_err(|(violation, _)| violation)
}

/// Runs the session that belongs to `seed` again and tells what happened
/// in it: what went wrong, if anything did, and the last steps.
#[must_use]
pub fn explain(profile: Profile, seed: u64) -> String {
    let (verdict, log) = match Run::new(profile, seed, true).finish() {
        Ok((tally, log)) => (format!("nothing went wrong in {} steps", tally.steps), log),
        Err((violation, log)) => (violation.to_string(), log),
    };
    let mut text = format!("profile {}, seed {seed}: {verdict}\n", profile.name());
    let skipped = log.len().saturating_sub(LOG_TAIL);
    if skipped > 0 {
        let _ = writeln!(text, "  ... {skipped} earlier lines");
    }
    for line in log.iter().skip(skipped) {
        let _ = writeln!(text, "  {line}");
    }
    text
}

/// A session written down: everything needed to run a kernel through it
/// again.
#[derive(Debug)]
pub struct Recorded {
    /// The configuration of its kernel.
    pub config: KernelConfig,
    /// Whether its agent answers only to its name.
    named: bool,
    /// What the kernel was given.
    pub trace: Trace,
}

impl Recorded {
    /// The policies of its kernel.
    #[must_use]
    pub fn policies(&self) -> BoxedPolicies {
        Run::policies(self.named)
    }
}

/// Runs the session that belongs to `seed` under `profile` and writes
/// down what its kernel was given.
///
/// # Errors
///
/// A step broke an invariant, or the session was left unfinished.
pub fn record(profile: Profile, seed: u64) -> Result<Recorded, Violation> {
    let mut run = Run::new(profile, seed, false);
    run.written = Some(Trace::default());
    let (config, named) = (run.config.clone(), run.named);
    let mut trace = Trace::default();
    let outcome = run.finish_with(|written| trace = written);
    outcome.map(|_| Recorded {
        config,
        named,
        trace,
    })
}

/// Runs `seeds` sessions under `profile`, each of them twice to see that
/// the kernel does the same both times. The environment can ask for other
/// seeds: see [`waveport_sim::SeedPlan::from_env`].
///
/// # Errors
///
/// A run failed. The report shows which and how to repeat it.
pub fn sweep(profile: Profile, seeds: u64) -> Result<Tally, Report> {
    // Each profile has its own stretch of seeds.
    let start = Profile::ALL
        .iter()
        .position(|other| *other == profile)
        .map_or(0, |index| u64::try_from(index).unwrap_or(0) << 32);
    let default = SeedPlan::Range {
        start,
        count: seeds,
    };
    let plan = SeedPlan::from_env(default).map_err(|error| Report(error.to_string()))?;
    let mut total = Tally::default();
    let swept = waveport_sim::run(plan, |context| {
        let seed = context.seed().0;
        let tally = run(profile, seed).map_err(|violation| violation.to_string())?;
        let again = run(profile, seed).map_err(|violation| violation.to_string())?;
        if tally != again {
            return Err(format!(
                "two runs of the same session differ: {tally:?} and {again:?}"
            ));
        }
        total.add(&tally);
        Ok(())
    });
    match swept {
        Ok(_) => Ok(total),
        Err(failure) => {
            let told = explain(profile, failure.seed.0);
            Err(Report(format!("{failure}\n{told}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Job, Profile, explain, record, run};
    use crate::trace::{Entry, Trace, replay};
    use waveport_domain::ids::{Epoch, GenId, OpTag};

    fn job(marks: &[(u32, u32)], position: u32) -> Job {
        Job {
            tag: OpTag {
                generation: GenId(1),
                epoch: Epoch(0),
            },
            pieces: Vec::new(),
            next: 0,
            called: false,
            ending: super::Ending::Done,
            paused: false,
            held: false,
            cancelled: false,
            deaf: false,
            mute: false,
            blind: false,
            asked: (0, 0),
            ready_at: 0,
            marks: marks.to_vec(),
            playing: true,
            position,
            reported_at: 0,
        }
    }

    #[test]
    fn heard_text_follows_the_audio_played() {
        let marks = [(10, 400), (30, 1_200)];
        assert_eq!(job(&marks, 0).heard(), 0);
        assert_eq!(job(&marks, 200).heard(), 5);
        assert_eq!(job(&marks, 400).heard(), 10);
        assert_eq!(job(&marks, 800).heard(), 20);
        assert_eq!(job(&marks, 1_200).heard(), 30);
        assert_eq!(job(&[], 0).heard(), 0);
        assert_eq!(job(&marks, 1_200).audio(), 1_200);
    }

    #[test]
    fn a_seed_always_gives_the_same_session() {
        for profile in Profile::ALL {
            let first = run(profile, 7).unwrap();
            assert_eq!(run(profile, 7).unwrap(), first, "{}", profile.name());
            assert_ne!(run(profile, 8).unwrap().trace, first.trace);
            assert!(first.steps > 0);
        }
    }

    #[test]
    fn a_run_can_be_explained() {
        let told = explain(Profile::Calm, 7);
        assert!(
            told.starts_with("profile calm, seed 7: nothing went wrong in "),
            "{told}"
        );
        assert!(told.contains(" ms  <- "), "{told}");
        assert!(told.contains(" earlier lines"), "{told}");
    }

    #[test]
    fn a_session_written_down_reads_back_and_replays() {
        let mut restarts = 0;
        for (index, profile) in (0_u64..).zip(Profile::ALL) {
            for seed in 0..25 {
                let seed = (index << 32) + seed;
                let recorded = record(profile, seed).unwrap();
                let text = recorded.trace.to_string();
                assert_eq!(Trace::parse(&text).as_ref(), Ok(&recorded.trace), "{seed}");
                // The kernel does the same without the world that made
                // the session up, and under a monitor that knows nothing
                // of it.
                let replayed = replay(&recorded.config, || recorded.policies(), &recorded.trace);
                let replayed = replayed.unwrap();
                assert_eq!(
                    Trace::parse(&replayed).as_ref(),
                    Ok(&recorded.trace),
                    "{seed}"
                );
                restarts += recorded
                    .trace
                    .entries
                    .iter()
                    .filter(|entry| matches!(entry, Entry::Restart(_)))
                    .count();
            }
        }
        assert!(restarts >= 25, "{restarts}");
    }
}
