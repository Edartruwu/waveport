//! Every path through a small session.
//!
//! The seeded simulation tries long sessions picked at random. This module
//! does the opposite. It takes a session so small that every order in
//! which its events can arrive can be tried, and tries them all: who
//! speaks when, when each transcript and estimate comes in, what the
//! language model writes and which tools it asks for, how far synthesis
//! and playback have got, which tool answers first, which timer fires,
//! when the application interrupts or closes the session, when the line
//! drops and whether it comes back, and when the process dies. A [`Monitor`] checks every step of every path, and at the
//! end of each path the journal is checked against the kernel.
//!
//! A [`Scope`] says how small the session is. Within it nothing is
//! sampled: [`explore`] returns only when no state is left that it has not
//! seen. Two paths that lead to the same state of the kernel, of the
//! world around it and of the monitor are followed once from there.
//!
//! Time passes only when a timer fires, so every order of the events
//! before and after each deadline is tried, and the delays between them
//! are not. Those are the seeded simulation's part.
//!
//! A path that breaks an invariant is reported as a trace, which
//! [`replay`](crate::trace::replay) runs again with [`Scope::config`].

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use waveport_domain::command::{
    Activity, ClientEvent, Command, LlmCause, LlmCmd, PlayoutCmd, SenseCmd, ToolCmd, TtsCmd,
};
use waveport_domain::config::{AgentSpec, Effect, Executor, KernelConfig, ToolSpec};
use waveport_domain::event::{
    CloseReason, ControlInput, Event, Finish, Key, OpEvent, OverlapVerdict, Part, RetryClass,
    TimerId, ToolEvent, VadEdge,
};
use waveport_domain::fact::Lifecycle;
use waveport_domain::ids::{AgentId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId, TurnId};
use waveport_domain::kernel::{Kernel, View};
use waveport_domain::out::Out;
use waveport_domain::policy::DefaultPolicies;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

use crate::check::Report;
use crate::invariants::{Journal, Monitor, Rule, Violation};
use crate::trace::{Entry, Trace};

/// How small the session is: who and what is in it, and how often each
/// kind of thing may happen in it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scope {
    /// How many people are in the session.
    pub people: u8,
    /// The agent's tools that run outside the session, by what a call to
    /// each may do.
    pub tools: Vec<Effect>,
    /// Whether the agent has one more tool, which a second agent answers.
    pub delegate: bool,
    /// Turns the people begin, in all.
    pub turns: u8,
    /// Tool calls the language models ask for, in all.
    pub calls: u8,
    /// Parts that fail, in all: language models, synthesis and tools.
    pub failures: u8,
    /// Timers that fire, in all.
    pub timers: u8,
    /// Times the application interrupts the agent.
    pub interrupts: u8,
    /// Times the application gives the agent something to say.
    pub says: u8,
    /// Times the application asks for the session to close.
    pub closes: u8,
    /// Times the line drops. Each time it comes back at any moment after,
    /// or never.
    pub drops: u8,
    /// Keys the people press, in all. Each is a digit or the key that
    /// ends a run, at any moment.
    pub keys: u8,
    /// Times the process dies.
    pub crashes: u8,
    /// Language models that go on to report their end after they were
    /// told to stop.
    pub late: u8,
    /// The most states to look at. A scope with more is refused: it is
    /// not small.
    pub most_states: u64,
}

impl Scope {
    /// One person, one turn and one tool call: small enough for every
    /// test run.
    #[must_use]
    pub fn small() -> Self {
        Self {
            people: 1,
            tools: vec![Effect::SideEffecting],
            delegate: false,
            turns: 1,
            calls: 1,
            failures: 0,
            timers: 2,
            interrupts: 1,
            says: 0,
            closes: 1,
            drops: 0,
            keys: 0,
            crashes: 0,
            late: 0,
            most_states: 2_000_000,
        }
    }

    /// The configuration of the session's kernel.
    #[must_use]
    pub fn config(&self) -> KernelConfig {
        let mut tools = self
            .tools
            .iter()
            .map(|&effect| ToolSpec::new(effect))
            .collect::<Vec<_>>();
        let mut agents = vec![AgentSpec::default()];
        if self.delegate {
            agents.push(AgentSpec::default());
            tools.push(ToolSpec {
                executor: Executor::Agent(AgentId(1)),
                ..ToolSpec::new(Effect::Pure)
            });
        }
        if let Some(front) = agents.first_mut() {
            front.tools = (0..).map(ToolId).take(tools.len()).collect();
        }
        KernelConfig {
            tools,
            agents,
            ..KernelConfig::default()
        }
    }
}

/// What an exploration covered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Explored {
    /// States that differ from one another.
    pub states: u64,
    /// Steps taken, counting those that led to a state seen before.
    pub steps: u64,
    /// States from which nothing more can happen.
    pub ends: u64,
    /// Those of them in which the session is at rest: nothing is armed
    /// and no adapter has work left. What must hold of a session nobody
    /// is going to wake was checked there.
    pub rests: u64,
    /// The longest path, in steps.
    pub deepest: u32,
}

/// What a person in the session is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Speaker {
    Quiet,
    /// Speaking since this media time.
    Speaking(u64),
    /// Has spoken between these media times, and the transcript of it is
    /// still to come.
    Spoken(u64, u64),
}

/// A language model at work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four things that are each true or not of a model, in any combination"
)]
struct Model {
    tag: OpTag,
    /// Whether it works on a tool call and so speaks to nobody.
    worker: bool,
    wrote: bool,
    /// The tool calls it has asked for.
    asked: u8,
    paused: bool,
    /// Whether it was told to stop and has yet to report its end.
    stopped: bool,
}

/// Synthesis and playback for one response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four things that are each true or not of a voice, in any combination"
)]
struct Voice {
    tag: OpTag,
    /// Bytes of text given to synthesis.
    given: u32,
    /// Whether synthesis was told that no more follows.
    finished: bool,
    /// Bytes of text whose audio has been delivered.
    made: u32,
    /// Whether the last audio has been delivered.
    last: bool,
    /// Whether synthesis was told to stop, or failed.
    silenced: bool,
    playing: bool,
    /// Bytes of text reported as heard.
    heard: u32,
}

/// What may still happen, and how often.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Left {
    turns: u8,
    calls: u8,
    failures: u8,
    timers: u8,
    interrupts: u8,
    says: u8,
    closes: u8,
    drops: u8,
    keys: u8,
    crashes: u8,
    late: u8,
}

/// Everything around the kernel: the people, the adapters and the clock.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct World {
    /// The clock, in nanoseconds.
    now: u64,
    /// The media clock, in ticks. Every edge of speech moves it on.
    media: u64,
    /// How many tools the agent has.
    tools: u16,
    timers: BTreeMap<TimerId, u64>,
    people: Vec<Speaker>,
    /// The turns whose end the kernel asked to have estimated.
    estimates: Vec<TurnId>,
    /// The turns the kernel asked to have classified.
    overlaps: Vec<TurnId>,
    models: Vec<Model>,
    voices: Vec<Voice>,
    /// The calls sent to tools, and whether each was told to stop.
    running: Vec<(ToolCallId, bool)>,
    /// The facts the kernel waits to hear are stored.
    storing: Vec<u64>,
    /// Every fact up to this one was reported as stored.
    vouched: u64,
    /// Whether the process has just come back and nothing has woken the
    /// session.
    asleep: bool,
    /// Whether the line has dropped and is not back.
    gone: bool,
    left: Left,
}

/// One thing that happens.
#[derive(Clone, Debug)]
enum Step {
    Event(MonoTime, Event),
    /// The process dies when the facts up to this one are stored.
    Crash(Seq),
}

/// Half a second of media time.
const EDGE: u64 = 24_000;
/// What the people say, in the order they are in the session.
const WORDS: [&str; 2] = ["where is my order", "and mine"];
/// What a language model that speaks writes: one whole sentence.
const ANSWER: &str = "It shipped. ";
/// What a language model that works on a tool call writes.
const FINDING: &str = "On Monday.";

/// The length of the audio for `bytes` of text.
const fn audio(bytes: u32) -> MediaDur {
    MediaDur::from_ticks(bytes.saturating_mul(2_880))
}

impl World {
    fn new(scope: &Scope, tools: usize) -> Self {
        Self {
            now: 0,
            media: 0,
            tools: u16::try_from(tools).unwrap_or(u16::MAX),
            timers: BTreeMap::new(),
            people: vec![Speaker::Quiet; usize::from(scope.people)],
            estimates: Vec::new(),
            overlaps: Vec::new(),
            models: Vec::new(),
            voices: Vec::new(),
            running: Vec::new(),
            storing: Vec::new(),
            vouched: 0,
            asleep: false,
            gone: false,
            left: Left {
                turns: scope.turns,
                calls: scope.calls,
                failures: scope.failures,
                timers: scope.timers,
                interrupts: scope.interrupts,
                says: scope.says,
                closes: scope.closes,
                drops: scope.drops,
                keys: scope.keys,
                crashes: scope.crashes,
                late: scope.late,
            },
        }
    }

    /// The world after `change`, and `event` arriving in it.
    fn then(&self, change: impl FnOnce(&mut Self), event: Event) -> (Self, Step) {
        let mut next = self.clone();
        change(&mut next);
        let at = MonoTime::from_nanos(next.now);
        (next, Step::Event(at, event))
    }

    fn model(&mut self, index: usize, change: impl FnOnce(&mut Model)) {
        if let Some(model) = self.models.get_mut(index) {
            change(model);
        }
    }

    fn voice(&mut self, index: usize, change: impl FnOnce(&mut Voice)) {
        if let Some(voice) = self.voices.get_mut(index) {
            change(voice);
        }
    }

    /// The voice of the response `tag`, which begins here if this is the
    /// first that is heard of it.
    fn voice_of(&mut self, tag: OpTag) -> Option<&mut Voice> {
        if !self.voices.iter().any(|voice| voice.tag == tag) {
            self.voices.push(Voice {
                tag,
                given: 0,
                finished: false,
                made: 0,
                last: false,
                silenced: false,
                playing: false,
                heard: 0,
            });
        }
        self.voices.iter_mut().find(|voice| voice.tag == tag)
    }

    /// Whether nothing is armed and no adapter has work left: nothing is
    /// going to wake the session.
    fn at_rest(&self) -> bool {
        let idle = self.people.iter().all(|speaker| *speaker == Speaker::Quiet);
        let adapters = self.estimates.is_empty()
            && self.overlaps.is_empty()
            && self.models.is_empty()
            && self.voices.is_empty()
            && self.running.is_empty()
            && self.storing.is_empty();
        self.timers.is_empty() && idle && adapters && !self.asleep
    }

    /// Everything that can happen next. `last` is the newest fact the
    /// kernel has journaled.
    fn moves(&self, last: Seq) -> Vec<(Self, Step)> {
        let mut moves = Vec::new();
        if self.asleep {
            // The process is back, and its client with it.
            let attached = Event::Control(ControlInput::Attached);
            moves.push(self.then(|next| next.asleep = false, attached));
            return moves;
        }
        self.people_move(&mut moves);
        self.models_move(&mut moves);
        self.voices_move(&mut moves);
        self.tools_move(last, &mut moves);
        self.clock_moves(&mut moves);
        self.application_moves(&mut moves);
        if self.left.crashes > 0 {
            // Everything had reached the disk, or only what the kernel
            // was told had.
            let mut stored = vec![last.get()];
            if self.vouched < last.get() {
                stored.push(self.vouched);
            }
            for stored in stored {
                let mut next = Self {
                    now: self.now,
                    media: self.media,
                    tools: self.tools,
                    timers: BTreeMap::new(),
                    people: vec![Speaker::Quiet; self.people.len()],
                    estimates: Vec::new(),
                    overlaps: Vec::new(),
                    models: Vec::new(),
                    voices: Vec::new(),
                    running: Vec::new(),
                    storing: Vec::new(),
                    vouched: stored,
                    asleep: true,
                    // The process that comes back has its line.
                    gone: false,
                    left: self.left,
                };
                next.left.crashes -= 1;
                moves.push((next, Step::Crash(Seq(stored))));
            }
        }
        moves
    }

    fn people_move(&self, moves: &mut Vec<(Self, Step)>) {
        let set = |next: &mut Self, index: usize, speaker: Speaker| {
            if let Some(slot) = next.people.get_mut(index) {
                *slot = speaker;
            }
        };
        for (index, speaker) in self.people.iter().enumerate() {
            let who = ParticipantId(u16::try_from(index + 1).unwrap_or(u16::MAX));
            let edge = |edge, at| Event::Speech {
                who,
                edge,
                at: MediaTime::from_ticks(at),
            };
            if self.left.keys > 0 {
                // A digit, which begins a run or goes on with one, and
                // the key that ends one.
                for key in [Key::One, Key::Hash] {
                    moves.push(self.then(|next| next.left.keys -= 1, Event::Key { who, key }));
                }
            }
            let at = self.media + EDGE;
            match *speaker {
                Speaker::Quiet if self.left.turns > 0 => moves.push(self.then(
                    |next| {
                        next.left.turns -= 1;
                        next.media = at;
                        set(next, index, Speaker::Speaking(at));
                    },
                    edge(VadEdge::Start, at),
                )),
                Speaker::Quiet => {}
                Speaker::Speaking(from) => moves.push(self.then(
                    |next| {
                        next.media = at;
                        set(next, index, Speaker::Spoken(from, at));
                    },
                    edge(VadEdge::End, at),
                )),
                Speaker::Spoken(from, to) => {
                    let span =
                        MediaSpan::new(MediaTime::from_ticks(from), MediaTime::from_ticks(to));
                    let heard = Event::Transcript {
                        who,
                        span,
                        text: (*WORDS.get(index % WORDS.len()).unwrap_or(&"hello")).into(),
                        is_final: true,
                    };
                    moves.push(self.then(|next| set(next, index, Speaker::Quiet), heard));
                }
            }
        }
        for (index, &turn) in self.estimates.iter().enumerate() {
            let sure = Event::Eou { turn, p: Prob::ONE };
            moves.push(self.then(
                |next| {
                    next.estimates.remove(index);
                },
                sure,
            ));
        }
        for (index, &turn) in self.overlaps.iter().enumerate() {
            for verdict in [OverlapVerdict::Backchannel, OverlapVerdict::BargeIn] {
                moves.push(self.then(
                    |next| {
                        next.overlaps.remove(index);
                    },
                    Event::Overlap { turn, verdict },
                ));
            }
        }
    }

    fn models_move(&self, moves: &mut Vec<(Self, Step)>) {
        for (index, model) in self.models.iter().enumerate() {
            let op = |op| Event::Op(model.tag, op);
            let gone = |next: &mut Self| {
                next.models.remove(index);
            };
            if model.stopped {
                // It was told to stop, and reports its end all the same.
                let finish = Finish::Stop;
                moves.push(self.then(gone, op(OpEvent::LlmDone { finish })));
                continue;
            }
            if model.paused {
                continue;
            }
            if !model.wrote {
                let text = if model.worker { FINDING } else { ANSWER };
                moves.push(self.then(
                    |next| next.model(index, |model| model.wrote = true),
                    op(OpEvent::LlmDelta { text: text.into() }),
                ));
            }
            if !model.worker && self.left.calls > 0 && model.asked < 2 {
                for tool in (0..self.tools).map(ToolId) {
                    let args = "{}".into();
                    moves.push(self.then(
                        |next| {
                            next.left.calls -= 1;
                            next.model(index, |model| model.asked += 1);
                        },
                        op(OpEvent::ToolProposed { tool, args }),
                    ));
                }
            }
            let finish = if model.asked > 0 {
                Finish::ToolCalls
            } else {
                Finish::Stop
            };
            moves.push(self.then(gone, op(OpEvent::LlmDone { finish })));
            if self.left.failures > 0 {
                let failed = OpEvent::Failed {
                    part: Part::Think,
                    class: RetryClass::Transient,
                };
                moves.push(self.then(
                    |next| {
                        next.left.failures -= 1;
                        gone(next);
                    },
                    op(failed),
                ));
            }
        }
    }

    fn voices_move(&self, moves: &mut Vec<(Self, Step)>) {
        for (index, voice) in self.voices.iter().enumerate() {
            let op = |op| Event::Op(voice.tag, op);
            let segment = |last| OpEvent::TtsSegment {
                text_end: voice.given,
                audio_end: audio(voice.given),
                last,
            };
            if !voice.silenced && voice.made < voice.given {
                moves.push(self.then(
                    |next| next.voice(index, |voice| voice.made = voice.given),
                    op(segment(false)),
                ));
            } else if !voice.silenced && voice.finished && !voice.last {
                moves.push(self.then(
                    |next| next.voice(index, |voice| voice.last = true),
                    op(segment(true)),
                ));
            }
            if !voice.silenced && !voice.last && voice.given > 0 && self.left.failures > 0 {
                let failed = OpEvent::Failed {
                    part: Part::Speak,
                    class: RetryClass::Transient,
                };
                moves.push(self.then(
                    |next| {
                        next.left.failures -= 1;
                        next.voice(index, |voice| voice.silenced = true);
                    },
                    op(failed),
                ));
            }
            if !voice.playing {
                continue;
            }
            // The listener hears half of what there is, then all of it,
            // and then the end.
            let heard = if voice.heard == 0 && voice.made >= 2 {
                voice.made / 2
            } else {
                voice.made
            };
            let played = |heard_text, drained| OpEvent::Played {
                handed: audio(heard_text),
                acked: audio(heard_text),
                heard_text,
                drained,
            };
            if heard > voice.heard {
                moves.push(self.then(
                    |next| next.voice(index, |voice| voice.heard = heard),
                    op(played(heard, false)),
                ));
            } else if voice.last {
                // The response ends with this, and its voice with it.
                moves.push(self.then(|_| {}, op(played(voice.heard, true))));
            }
        }
    }

    fn tools_move(&self, last: Seq, moves: &mut Vec<(Self, Step)>) {
        for (index, &(call, stopped)) in self.running.iter().enumerate() {
            let gone = |next: &mut Self| {
                next.running.remove(index);
            };
            let result = "ok".into();
            let completed = ToolEvent::Completed { result };
            moves.push(self.then(gone, Event::Tool(call, completed)));
            if self.left.failures > 0 {
                let failed = ToolEvent::Failed {
                    class: RetryClass::Transient,
                    message: "no".into(),
                };
                moves.push(self.then(
                    |next| {
                        next.left.failures -= 1;
                        gone(next);
                    },
                    Event::Tool(call, failed),
                ));
            }
            if stopped {
                moves.push(self.then(gone, Event::Tool(call, ToolEvent::RolledBack)));
            }
        }
        if let Some(&seq) = self.storing.iter().max() {
            // The journal reports once for everything it has stored.
            moves.push(self.then(
                |next| {
                    next.storing.clear();
                    next.vouched = next.vouched.max(seq.min(last.get()));
                },
                Event::Durable(Seq(seq)),
            ));
        }
    }

    /// The timers that are due first fire, in every order.
    fn clock_moves(&self, moves: &mut Vec<(Self, Step)>) {
        let Some(&due) = self.timers.values().min() else {
            return;
        };
        if self.left.timers == 0 {
            return;
        }
        for (&id, _) in self.timers.iter().filter(|&(_, &at)| at == due) {
            moves.push(self.then(
                |next| {
                    next.left.timers -= 1;
                    next.timers.remove(&id);
                    next.now = next.now.max(due);
                },
                Event::Timer(id),
            ));
        }
    }

    fn application_moves(&self, moves: &mut Vec<(Self, Step)>) {
        let control = |input| Event::Control(input);
        if self.left.interrupts > 0 {
            moves.push(self.then(
                |next| next.left.interrupts -= 1,
                control(ControlInput::Interrupt),
            ));
        }
        if self.left.says > 0 {
            let text = "One moment. ".into();
            moves.push(self.then(
                |next| next.left.says -= 1,
                control(ControlInput::Say { text }),
            ));
        }
        if self.left.closes > 0 {
            let reason = CloseReason::Requested;
            moves.push(self.then(
                |next| next.left.closes -= 1,
                control(ControlInput::Close { reason }),
            ));
        }
        if self.gone {
            moves.push(self.then(|next| next.gone = false, control(ControlInput::Attached)));
        } else if self.left.drops > 0 {
            moves.push(self.then(
                |next| {
                    next.left.drops -= 1;
                    next.gone = true;
                },
                control(ControlInput::Detached),
            ));
        }
    }

    /// The adapters take note of what the kernel told them.
    fn react(&mut self, out: &Out) {
        for command in &out.cmds {
            match *command {
                Command::Llm(tag, LlmCmd::Start { cause, .. }) => self.models.push(Model {
                    tag,
                    worker: matches!(cause, LlmCause::Delegated { .. }),
                    wrote: false,
                    asked: 0,
                    paused: false,
                    stopped: false,
                }),
                Command::Llm(tag, llm) => {
                    let Some(index) = self.models.iter().position(|model| model.tag == tag) else {
                        continue;
                    };
                    match llm {
                        LlmCmd::Pause => self.model(index, |model| model.paused = true),
                        LlmCmd::Resume => self.model(index, |model| model.paused = false),
                        LlmCmd::Cancel if self.left.late > 0 => {
                            self.left.late -= 1;
                            self.model(index, |model| model.stopped = true);
                        }
                        LlmCmd::Cancel | LlmCmd::Start { .. } => {
                            self.models.remove(index);
                        }
                    }
                }
                Command::Tts(tag, tts) => {
                    let spoken = command.text().map_or(0, |at| out.text.get(at).len());
                    let Some(voice) = self.voice_of(tag) else {
                        continue;
                    };
                    match tts {
                        TtsCmd::Speak { .. } => {
                            let more = u32::try_from(spoken).unwrap_or(u32::MAX);
                            voice.given = voice.given.saturating_add(more);
                        }
                        TtsCmd::Finish => voice.finished = true,
                        TtsCmd::Cancel => voice.silenced = true,
                    }
                }
                Command::Playout(tag, playout) => {
                    let Some(voice) = self.voice_of(tag) else {
                        continue;
                    };
                    match playout {
                        PlayoutCmd::Start => voice.playing = true,
                        PlayoutCmd::Stop { .. } => voice.playing = false,
                        PlayoutCmd::Duck | PlayoutCmd::Unduck => {}
                    }
                }
                Command::Tool(call, ToolCmd::Dispatch { .. }) => self.running.push((call, false)),
                Command::Tool(call, ToolCmd::Cancel) => {
                    for running in &mut self.running {
                        running.1 |= running.0 == call;
                    }
                }
                Command::SetTimer(id, Some(at)) => {
                    self.timers.insert(id, at.as_nanos());
                }
                Command::SetTimer(id, None) => {
                    self.timers.remove(&id);
                }
                Command::AwaitDurable(seq) => self.storing.push(seq.get()),
                Command::Sense(SenseCmd::EstimateEou { turn, .. }) => self.estimates.push(turn),
                Command::Sense(SenseCmd::ClassifyOverlap { turn, .. }) => self.overlaps.push(turn),
                Command::Emit(ClientEvent::Agent {
                    generation: Some(generation),
                    activity: Activity::Listening,
                    ..
                }) => {
                    // The response is over, and so is the work for it.
                    self.voices
                        .retain(|voice| voice.tag.generation != generation);
                    self.models
                        .retain(|model| model.tag.generation != generation || model.stopped);
                }
                Command::Emit(_) | Command::Stt(_) => {}
            }
        }
    }
}

/// One step of a path, and the steps before it.
struct Hop {
    before: Option<Rc<Self>>,
    entry: Entry,
}

/// A session at one point of one path.
#[derive(Clone)]
struct Node {
    kernel: Kernel,
    monitor: Monitor,
    journal: Journal,
    world: World,
    path: Option<Rc<Hop>>,
    depth: u32,
}

impl Node {
    /// What tells this state from every other.
    fn key(&self) -> (u64, u64) {
        let mut hasher = DefaultHasher::new();
        self.world.hash(&mut hasher);
        self.monitor.hash(&mut hasher);
        (self.kernel.fingerprint(), hasher.finish())
    }

    /// The path that led here, as a trace.
    fn trace(&self) -> Trace {
        let mut entries = Vec::new();
        let mut hop = self.path.as_ref();
        while let Some(at) = hop {
            entries.push(at.entry.clone());
            hop = at.before.as_ref();
        }
        entries.reverse();
        Trace { entries }
    }

    fn hop(&mut self, entry: Entry) {
        self.path = Some(Rc::new(Hop {
            before: self.path.take(),
            entry,
        }));
        self.depth += 1;
    }

    /// The session after `step`, with the world as `world` has it.
    fn take(
        &self,
        world: World,
        step: &Step,
        config: &KernelConfig,
        out: &mut Out,
    ) -> Result<Self, Box<(Self, Violation)>> {
        let mut next = self.clone();
        next.world = world;
        match step {
            Step::Event(at, event) => {
                next.hop(Entry::Event(*at, event.clone()));
                next.kernel.step(*at, event, out);
                next.journal.record(out);
                let view = next.kernel.view();
                if let Err(violation) = next.monitor.observe(*at, event, out, view) {
                    return Err(Box::new((next, violation)));
                }
                next.world.react(out);
            }
            Step::Crash(stored) => {
                next.hop(Entry::Restart(*stored));
                if let Err(violation) = next.journal.check(&next.kernel) {
                    return Err(Box::new((next, violation)));
                }
                next.journal.cut(*stored);
                let rebuilt = next.journal.latest().map(|(snapshot, tail, text)| {
                    Kernel::restore(
                        config.clone(),
                        DefaultPolicies::default(),
                        snapshot,
                        tail,
                        text,
                    )
                });
                match rebuilt {
                    Some(Ok(kernel)) => next.kernel = kernel,
                    Some(Err(error)) => {
                        let violation = Violation {
                            rule: Rule::Restore,
                            what: format!(
                                "the session cannot be rebuilt from its journal: {error}"
                            ),
                        };
                        return Err(Box::new((next, violation)));
                    }
                    None => {}
                }
                next.monitor = Monitor::restored(next.kernel.view(), next.journal.speech());
            }
        }
        Ok(next)
    }
}

/// Tries every path through the session `scope` describes.
///
/// # Errors
///
/// A path breaks an invariant: the report shows the path as a trace and
/// what is wrong at its end. Or the scope has more than
/// [`Scope::most_states`] states.
pub fn explore(scope: &Scope) -> Result<Explored, Report> {
    explore_where(scope, |_| Ok(()))
}

/// Tries every path through the session `scope` describes, and checks
/// `claim` in every state on the way: something more that is to hold of
/// the kernel, said of what its [`View`] shows.
///
/// # Errors
///
/// As [`explore`], or a state does not satisfy `claim`. The report then
/// shows the shortest path this search took to it, which need not be the
/// shortest there is.
pub fn explore_where(
    scope: &Scope,
    claim: impl Fn(View<'_>) -> Result<(), String>,
) -> Result<Explored, Report> {
    let config = scope.config();
    let failed = |node: &Node, violation: &Violation| {
        Report(format!(
            "a path through {scope:?} goes wrong at its end:\n{}{violation}",
            node.trace()
        ))
    };
    let kernel = Kernel::new(config.clone(), DefaultPolicies::default())
        .map_err(|error| Report(format!("the configuration is not valid: {error}")))?;
    let mut root = Node {
        monitor: Monitor::new(kernel.view()),
        journal: Journal::new(kernel.snapshot()),
        world: World::new(scope, config.tools.len()),
        kernel,
        path: None,
        depth: 0,
    };
    let mut out = Out::new();
    // The people are in the session before anything else happens.
    for who in (1..=u16::from(scope.people)).map(ParticipantId) {
        let join = Step::Event(
            MonoTime::from_nanos(0),
            Event::Control(ControlInput::Join { who }),
        );
        root = root
            .take(root.world.clone(), &join, &config, &mut out)
            .map_err(|wrong| failed(&wrong.0, &wrong.1))?;
    }
    root.depth = 0;

    let mut explored = Explored::default();
    let mut seen = BTreeSet::from([root.key()]);
    let mut open = vec![root];
    while let Some(node) = open.pop() {
        explored.states += 1;
        explored.deepest = explored.deepest.max(node.depth);
        if explored.states > scope.most_states {
            return Err(Report(format!(
                "{scope:?} has more than {} states: it is too large to try every path",
                scope.most_states
            )));
        }
        let view = node.kernel.view();
        claim(view).map_err(|what| {
            let rule = Rule::Progress;
            failed(&node, &Violation { rule, what })
        })?;
        let closed = view.lifecycle() == Lifecycle::Closed;
        let moves = if closed {
            Vec::new()
        } else {
            node.world.moves(node.journal.last())
        };
        if moves.is_empty() {
            explored.ends += 1;
            let mut checked = node.journal.check(&node.kernel);
            if closed || node.world.at_rest() {
                explored.rests += 1;
                checked = checked.and_then(|()| node.monitor.at_rest(view));
            }
            checked.map_err(|violation| failed(&node, &violation))?;
            continue;
        }
        for (world, step) in moves {
            explored.steps += 1;
            let next = node
                .take(world, &step, &config, &mut out)
                .map_err(|wrong| failed(&wrong.0, &wrong.1))?;
            if seen.insert(next.key()) {
                open.push(next);
            }
        }
    }
    Ok(explored)
}

#[cfg(test)]
mod tests {
    use waveport_domain::config::Effect;
    use waveport_domain::policy::DefaultPolicies;

    use super::{Scope, explore, explore_where};
    use crate::trace::{Trace, replay};

    /// A session in which nothing can happen.
    fn nothing() -> Scope {
        Scope {
            people: 1,
            tools: Vec::new(),
            delegate: false,
            turns: 0,
            calls: 0,
            failures: 0,
            timers: 0,
            interrupts: 0,
            says: 0,
            closes: 0,
            drops: 0,
            keys: 0,
            crashes: 0,
            late: 0,
            most_states: 100_000,
        }
    }

    #[test]
    fn a_session_in_which_nothing_happens_has_one_state() {
        let explored = explore(&nothing()).unwrap();
        assert_eq!((explored.states, explored.steps), (1, 0));
        assert_eq!((explored.ends, explored.rests), (1, 1));
    }

    #[test]
    fn a_turn_and_its_answer_are_tried_in_every_order() {
        let scope = Scope {
            turns: 1,
            timers: 1,
            ..nothing()
        };
        let explored = explore(&scope).unwrap();
        // Speech, its end, the estimate and the transcript, then the
        // answer written, synthesised and heard: fourteen steps at least
        // on the straight path, and many ways around it.
        assert!(explored.deepest >= 12, "{explored:?}");
        assert!(explored.states >= 50, "{explored:?}");
        assert!(explored.rests >= 1, "{explored:?}");
        assert!(explored.steps > explored.states, "{explored:?}");
        // Nothing is left to chance.
        assert_eq!(explore(&scope), Ok(explored));
    }

    #[test]
    fn more_that_may_happen_is_more_to_try() {
        let one = Scope {
            turns: 1,
            ..nothing()
        };
        let interrupted = Scope {
            interrupts: 1,
            ..one.clone()
        };
        let with_tool = Scope {
            tools: vec![Effect::Pure],
            calls: 1,
            ..one.clone()
        };
        let died = Scope {
            crashes: 1,
            ..one.clone()
        };
        let dropped = Scope {
            drops: 1,
            ..one.clone()
        };
        let base = explore(&one).unwrap().states;
        for scope in [interrupted, with_tool, died, dropped] {
            let explored = explore(&scope).unwrap();
            assert!(explored.states > base, "{scope:?}: {explored:?}");
        }
    }

    #[test]
    fn a_scope_that_is_not_small_is_refused() {
        let scope = Scope {
            turns: 1,
            most_states: 10,
            ..nothing()
        };
        let refused = explore(&scope).unwrap_err().to_string();
        assert!(refused.contains("has more than 10 states"), "{refused}");
    }

    #[test]
    fn a_state_that_breaks_a_claim_is_shown_with_a_path_that_replays() {
        let scope = Scope {
            turns: 1,
            ..nothing()
        };
        // Not true: the agent does answer.
        let never_answers = explore_where(&scope, |view| match view.generations().next() {
            Some(response) => Err(format!("{:?} is under way", response.tag)),
            None => Ok(()),
        });
        let report = never_answers.unwrap_err().to_string();
        assert!(report.contains("is under way"), "{report}");
        // The path is a trace, the report's first and last lines aside,
        // and the kernel can be taken along it again.
        let lines = report.lines().collect::<Vec<_>>();
        let path = lines.get(1..lines.len() - 1).unwrap().join("\n");
        let trace = Trace::parse(&path).unwrap();
        assert!(trace.entries.len() >= 4, "{report}");
        let record = replay(&scope.config(), DefaultPolicies::default, &trace).unwrap();
        assert!(record.contains("    > Llm(OpTag {"), "{record}");
    }
}
