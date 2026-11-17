//! The kernel's invariants, checked from the outside.
//!
//! A [`Monitor`] is shown every step: the event the kernel was given, what
//! the kernel emitted, and the kernel's state afterwards. From the events
//! and the emissions alone it keeps its own account of the session: who is
//! in it, which turns are open and what was said in them, which responses
//! are under way, what each was given to say and how much of that was
//! heard. A step breaks an invariant when the kernel's emissions or its
//! state disagree with that account.
//!
//! The invariants are those of `docs/architecture.md`, section 13. A
//! monitor checks all of them that can be seen in one life of a session:
//! 1 to 8, the numbering of facts from 10, 11, and progress. Invariant 9, that
//! the journal rebuilds the session, is checked by [`Journal`], which
//! keeps what the kernel journaled; and what must hold from one life of a
//! session to the next is checked by whoever restarts it, with a monitor
//! made by [`Monitor::restored`].

mod calls;

use core::fmt;
use core::hash::{Hash, Hasher};
use core::time::Duration;

use waveport_domain::command::{
    Activity, ClientEvent, Command, LlmCause, LlmCmd, PlayoutCmd, SenseCmd, TtsCmd,
};
use waveport_domain::config::{KernelConfig, Limits};
use waveport_domain::event::{
    CloseReason, ControlInput, Event, Key, OpEvent, Part, TimerId, VadEdge,
};
use waveport_domain::fact::{Dismissal, EpochCause, Fact, Lifecycle, Stamped};
use waveport_domain::ids::{AgentId, Epoch, GenId, OpTag, ParticipantId, Seq, ToolCallId, TurnId};
use waveport_domain::kernel::{Kernel, Snapshot, View};
use waveport_domain::out::Out;
use waveport_domain::policy::{DefaultPolicies, Policies};
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;

use self::calls::{Desk, Recovery};

/// What a broken step broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// Invariant 1: the epoch only moves forward, and nothing is asked of
    /// a response that has ended.
    Liveness,
    /// Invariant 2: everything a person says ends in one turn, committed
    /// with exactly their words or dismissed for a reason that holds, and
    /// every key they press in one run, committed as the keys that were
    /// pressed, when it is over. A key stops what is being heard, and a
    /// person's turns enter the conversation in the order they began.
    Turns,
    /// Invariant 3: a response enters the conversation once, as exactly
    /// the part of its text that was heard.
    SpokenText,
    /// Invariant 4: a tool call that may change something is sent only
    /// once the record that cleared it is stored, at most once, and ends
    /// once, in a way its tool accounts for.
    Tools,
    /// Invariant 5: the results of a batch of tool calls start at most
    /// one response.
    Continuation,
    /// Invariant 6: one response is heard at a time.
    OneVoice,
    /// Invariant 7: a session asked to close is closed in time, in order,
    /// and stays closed.
    Closing,
    /// Invariant 8: every table and every step's output stays within its
    /// bound.
    Bounds,
    /// Invariant 9: a snapshot and the facts journaled since add up to
    /// what the session holds, and a session rebuilt from them begins by
    /// dealing with what was lost.
    Restore,
    /// Invariant 10: facts are numbered without gaps, and a kernel that
    /// lost its lease emits nothing.
    Journal,
    /// Invariant 11: nothing is said into a line that is gone, a reply the
    /// drop cut off is started again when the line is back, and a session
    /// whose line stays gone closes as abandoned when its wait is over
    /// and not before.
    Line,
    /// The floor went from one agent to another in a way it may not.
    Stage,
    /// Nothing more is going to happen, and something is unfinished.
    Progress,
}

impl Rule {
    /// The number of the invariant in the architecture document, if the
    /// rule is one of them.
    #[must_use]
    pub const fn number(self) -> Option<u8> {
        match self {
            Self::Liveness => Some(1),
            Self::Turns => Some(2),
            Self::SpokenText => Some(3),
            Self::Tools => Some(4),
            Self::Continuation => Some(5),
            Self::OneVoice => Some(6),
            Self::Closing => Some(7),
            Self::Bounds => Some(8),
            Self::Restore => Some(9),
            Self::Journal => Some(10),
            Self::Line => Some(11),
            Self::Stage | Self::Progress => None,
        }
    }
}

/// A step that broke a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The rule.
    pub rule: Rule,
    /// What happened.
    pub what: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.rule.number(), self.rule) {
            (Some(number), _) => write!(f, "invariant {number} is broken: {}", self.what),
            (None, Rule::Stage) => write!(f, "the floor changed hands wrongly: {}", self.what),
            (None, _) => write!(f, "the session is stuck: {}", self.what),
        }
    }
}

impl std::error::Error for Violation {}

fn ensure(holds: bool, rule: Rule, what: impl FnOnce() -> String) -> Result<(), Violation> {
    if holds {
        Ok(())
    } else {
        Err(Violation { rule, what: what() })
    }
}

/// Everything a session journaled, kept the way a journal keeps it: the
/// facts in order with their text, and the snapshots taken on the way.
///
/// It checks invariant 9: the first snapshot and every later one, each
/// with the facts journaled since, add up to what the kernel holds.
#[derive(Clone, Debug)]
pub struct Journal {
    /// The snapshots, oldest first. The first is of the session before
    /// its first fact.
    snapshots: Vec<Snapshot>,
    facts: Vec<Stamped>,
    text: TextArena,
}

impl Journal {
    /// The journal of a session that begins as `snapshot` has it.
    #[must_use]
    pub fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshots: vec![snapshot],
            facts: Vec::new(),
            text: TextArena::default(),
        }
    }

    /// Keeps the facts of one step.
    pub fn record(&mut self, out: &Out) {
        for stamped in &out.facts {
            let fact = match stamped.fact.text() {
                Some(at) => stamped.fact.with_text(self.text.push(out.text.get(at))),
                None => stamped.fact,
            };
            self.facts.push(Stamped {
                seq: stamped.seq,
                fact,
            });
        }
    }

    /// Keeps a snapshot, as a journal does now and then so that it need
    /// not be read from its beginning.
    pub fn keep(&mut self, snapshot: Snapshot) {
        self.snapshots.push(snapshot);
    }

    /// The place of the last fact kept.
    #[must_use]
    pub fn last(&self) -> Seq {
        self.facts.last().map_or_else(
            || {
                let first = self.snapshots.first().map(Snapshot::next_seq);
                Seq(first.map_or(0, |next| next.get().saturating_sub(1)))
            },
            |stamped| stamped.seq,
        )
    }

    /// Forgets every fact after `stored` and every snapshot that holds
    /// one: what a crash does to a journal whose last writes had not
    /// reached the disk.
    pub fn cut(&mut self, stored: Seq) {
        self.facts.retain(|stamped| stamped.seq <= stored);
        let first = self.snapshots.first().cloned();
        self.snapshots
            .retain(|snapshot| snapshot.next_seq() <= stored.next());
        if self.snapshots.is_empty() {
            self.snapshots.extend(first);
        }
    }

    /// The facts from `from` on.
    fn since(&self, from: Seq) -> &[Stamped] {
        let start = self
            .facts
            .iter()
            .position(|stamped| stamped.seq >= from)
            .unwrap_or(self.facts.len());
        self.facts.get(start..).unwrap_or(&[])
    }

    /// The newest snapshot, the facts journaled after it, and their text:
    /// what a session is rebuilt from.
    #[must_use]
    pub fn latest(&self) -> Option<(&Snapshot, &[Stamped], &TextArena)> {
        let snapshot = self.snapshots.last()?;
        Some((snapshot, self.since(snapshot.next_seq()), &self.text))
    }

    /// The response whose speech the journal has as being heard and not
    /// as ended, and what it has as heard of it.
    #[must_use]
    pub fn speech(&self) -> Option<(GenId, String)> {
        let mut speech: Option<(GenId, String)> = None;
        for stamped in &self.facts {
            match stamped.fact {
                Fact::UtteranceProgress {
                    generation, heard, ..
                } => {
                    let heard = self.text.get(heard);
                    match &mut speech {
                        Some((of, text)) if *of == generation => text.push_str(heard),
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

    /// Checks that every snapshot kept, with the facts journaled after
    /// it, adds up to what `kernel` holds.
    ///
    /// # Errors
    ///
    /// One does not: invariant 9 is broken.
    pub fn check<P: Policies>(&self, kernel: &Kernel<P>) -> Result<(), Violation> {
        let live = kernel.snapshot();
        for snapshot in &self.snapshots {
            let from = snapshot.next_seq();
            let rebuilt = Kernel::restore(
                kernel.config().clone(),
                DefaultPolicies::default(),
                snapshot,
                self.since(from),
                &self.text,
            );
            let same = rebuilt
                .as_ref()
                .is_ok_and(|rebuilt| rebuilt.snapshot() == live);
            ensure(same, Rule::Restore, || match rebuilt {
                Ok(rebuilt) => format!(
                    "the snapshot before fact #{from} and the facts since add up to\n{:?}\nand \
                     the session holds\n{live:?}",
                    rebuilt.snapshot()
                ),
                Err(error) => format!(
                    "the session cannot be rebuilt from the snapshot before fact #{from}: {error}"
                ),
            })?;
        }
        Ok(())
    }
}

/// An open turn, as the monitor has it.
#[derive(Clone, Debug, Hash)]
struct Heard {
    turn: TurnId,
    who: ParticipantId,
    /// The words of the final transcripts that joined the turn.
    words: String,
    /// Whether the turn's speech was over the agent's.
    over_agent: bool,
    /// The keys of the turn, if it is one of keys that are pressed and
    /// not of speech.
    keys: Option<Run>,
}

/// A run of keys, as the monitor has it. The keys themselves are the
/// words of its turn.
#[derive(Clone, Copy, Debug, Hash)]
struct Run {
    /// When the newest of them was pressed.
    last: MonoTime,
    /// Whether no key can follow: the run is a turn, and waits only for
    /// what whoever pressed it said before.
    over: bool,
}

/// A response under way, as the monitor has it.
#[derive(Clone, Debug, Hash)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one thing seen to happen to a response, and they are checked against each other"
)]
struct Spoken {
    tag: OpTag,
    /// Everything it was given to say: what the model was seen to write,
    /// or what the application asked for.
    script: String,
    /// What was handed to synthesis.
    released: String,
    /// Whether a language model was asked for it.
    asked: bool,
    /// Whether the model was told to wait.
    waiting: bool,
    /// Whether synthesis was told that no more text follows.
    finished: bool,
    /// Whether synthesis delivered its last audio after that.
    synthesized: bool,
    /// Whether it began to play.
    started: bool,
    lowered: bool,
    stopped: bool,
    /// Whether synthesis was told to stop.
    silenced: bool,
    /// Bytes of `released` reported as heard.
    heard: usize,
    /// Bytes of `released` journaled as heard.
    recorded: usize,
    /// Whether it entered the conversation.
    committed: bool,
    /// The tool call it works on, if it does not speak.
    works_on: Option<ToolCallId>,
    /// The agent whose language model was asked for it.
    agent: Option<AgentId>,
    /// Whether its language model has finished, or was told to stop.
    done: bool,
}

impl Spoken {
    fn begin(tag: OpTag, event: &Event) -> Self {
        let script = match event {
            Event::Control(ControlInput::Say { text }) => text.as_str().to_owned(),
            _ => String::new(),
        };
        Self {
            tag,
            script,
            released: String::new(),
            asked: false,
            waiting: false,
            finished: false,
            synthesized: false,
            started: false,
            lowered: false,
            stopped: false,
            silenced: false,
            heard: 0,
            recorded: 0,
            committed: false,
            works_on: None,
            agent: None,
            done: false,
        }
    }

    /// What the listener heard of it: everything when it played to its
    /// end, and otherwise the part playback reported.
    fn audible(&self, whole: bool) -> &str {
        let heard = if whole {
            self.released.as_str()
        } else {
            self.released.get(..self.heard).unwrap_or("")
        };
        heard.trim_end()
    }
}

/// Watches a kernel, one step at a time.
///
/// A clone goes on watching a clone of the kernel. Two monitors hash the
/// same when they have learnt the same of their sessions.
#[derive(Clone, Debug)]
pub struct Monitor {
    config: KernelConfig,
    limits: Limits,
    /// How long a session has to close once asked.
    closing_time: Duration,
    leased: bool,
    last_seq: u64,
    /// The epoch the session was in when the monitor began to watch.
    began: Epoch,
    /// The epoch the session was in before the step being looked at.
    entered: Epoch,
    epoch: Epoch,
    lifecycle: Lifecycle,
    /// When the session must be closed, once it was asked to close.
    close_by: Option<MonoTime>,
    /// When a line that dropped has had its time to come back. `None`
    /// while the line is there.
    gone: Option<MonoTime>,
    /// The response a language model was writing that the line's drop
    /// cuts off, in the step that tells of the drop.
    dropped: Option<GenId>,
    people: Vec<ParticipantId>,
    turns: Vec<Heard>,
    /// The highest turn number seen.
    last_turn: u32,
    responses: Vec<Spoken>,
    /// The highest response number seen.
    last_generation: u32,
    /// The response that is being heard.
    voice: Option<GenId>,
    /// How many items the conversation holds.
    context: u32,
    /// The conversation as of the newest turn meant for the agent.
    owed: u32,
    /// The newest conversation a reply was started on.
    replied: u32,
    /// Words that arrived in this step and must be a turn of their own.
    arrived: Option<(ParticipantId, String)>,
    /// A key that was pressed in this step and has no run to join: the
    /// step begins one with it, unless as many turns were open as may
    /// be, which is the third value.
    pressed: Option<(ParticipantId, Key, bool)>,
    /// A turn of speech that has to end in this step: keys its speaker
    /// pressed before it were committed while another could follow, which
    /// only what was said after them having to enter the conversation
    /// does.
    forced: Option<TurnId>,
    /// The tool calls, their batches, and who has the floor.
    desk: Desk,
}

impl Hash for Monitor {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // The configuration is left out: it does not change.
        let Self {
            config: _,
            limits: _,
            closing_time: _,
            leased,
            last_seq,
            began,
            entered,
            epoch,
            lifecycle,
            close_by,
            gone,
            dropped,
            people,
            turns,
            last_turn,
            responses,
            last_generation,
            voice,
            context,
            owed,
            replied,
            arrived,
            // Neither outlasts a step.
            pressed: _,
            forced: _,
            desk,
        } = self;
        (leased, last_seq, began, entered, epoch, lifecycle, close_by).hash(state);
        (gone, dropped).hash(state);
        (people, turns, last_turn, responses, last_generation, voice).hash(state);
        (context, owed, replied, arrived, desk).hash(state);
    }
}

impl Monitor {
    /// A monitor for a kernel in the state `view` shows, with no turn and
    /// no response under way: a new one.
    #[must_use]
    pub fn new(view: View<'_>) -> Self {
        let config = view.config();
        let context = view.context_version().get();
        Self {
            config: config.clone(),
            desk: Desk::of(view),
            limits: config.limits,
            closing_time: config.timing.drain + config.timing.grace,
            leased: true,
            last_seq: view.next_seq().get().saturating_sub(1),
            began: view.epoch(),
            entered: view.epoch(),
            epoch: view.epoch(),
            lifecycle: view.lifecycle(),
            close_by: None,
            gone: view.gone_until(),
            dropped: None,
            people: view.participants().collect(),
            turns: Vec::new(),
            last_turn: 0,
            responses: Vec::new(),
            last_generation: 0,
            voice: None,
            context,
            owed: 0,
            replied: context,
            arrived: None,
            pressed: None,
            forced: None,
        }
    }

    /// A monitor for a kernel that was rebuilt from its journal and has
    /// not taken a step. `speech` is the response that was being heard
    /// when the session's earlier life ended, and what the journal has as
    /// heard of it. The kernel's first step has to deal with everything
    /// that was under way then.
    #[must_use]
    pub fn restored(view: View<'_>, speech: Option<(GenId, String)>) -> Self {
        let mut monitor = Self::new(view);
        let lost = view.reply_owed() || speech.is_some() || view.calls().next().is_some();
        if view.reply_owed() {
            monitor.owed = monitor.context;
            monitor.replied = monitor.context.saturating_sub(1);
        }
        monitor.desk.recovery = Some(Recovery {
            speech,
            lost,
            noted: false,
        });
        monitor
    }

    /// Looks at one step: the event the kernel was given at `now`, what it
    /// emitted into `out`, and its state afterwards.
    ///
    /// # Errors
    ///
    /// The step broke a rule. The monitor's account is not to be trusted
    /// after that.
    pub fn observe(
        &mut self,
        now: MonoTime,
        event: &Event,
        out: &Out,
        view: View<'_>,
    ) -> Result<(), Violation> {
        let lost = matches!(event, Event::Control(ControlInput::LeaseLost));
        if lost || !self.leased {
            self.leased = false;
            return ensure(out.is_empty(), Rule::Journal, || {
                format!("a kernel without its lease emitted something on {event:?}")
            });
        }
        if self.lifecycle == Lifecycle::Closed {
            return ensure(out.is_empty(), Rule::Closing, || {
                format!("a closed session emitted something on {event:?}")
            });
        }

        let whole = self.delivered(now, event);
        let first = out.facts.first().map(|stamped| {
            let recovery = EpochCause::Recovery;
            matches!(stamped.fact, Fact::EpochAdvanced { cause, .. } if cause == recovery)
        });
        self.recovery_began(first)?;
        self.words_arrived(event, out)?;
        let spoke = self.key_arrived(now, event);
        self.entered = self.epoch;
        // Facts and commands are two lists, so the order between a fact
        // and a command is not known. A turn can begin and end in one
        // step; its beginning is a command and its end a fact, so the
        // beginnings are read first. So are the ends of responses, which
        // decide what may become of the tool calls they asked for.
        for command in &out.cmds {
            self.foreseen(event, command, whole)?;
        }
        for stamped in &out.facts {
            ensure(
                stamped.seq.get() == self.last_seq + 1,
                Rule::Journal,
                || format!("fact #{} follows fact #{}", stamped.seq, self.last_seq),
            )?;
            self.last_seq = stamped.seq.get();
            let text = stamped.fact.text().map_or("", |text| out.text.get(text));
            self.fact(now, event, stamped, text, whole)?;
        }
        for command in &out.cmds {
            let text = command.text().map_or("", |text| out.text.get(text));
            self.command(event, command, text, whole)?;
        }
        self.keys_settled(now, event, view, spoke)?;
        self.settled(now, event, out, view, whole)
    }

    /// Checks what must hold once nothing more is going to happen: every
    /// timer has fired and every adapter has said all it will.
    ///
    /// # Errors
    ///
    /// Something is unfinished.
    pub fn at_rest(&self, view: View<'_>) -> Result<(), Violation> {
        if !self.leased || self.lifecycle == Lifecycle::Closed {
            return Ok(());
        }
        ensure(self.lifecycle == Lifecycle::Running, Rule::Closing, || {
            format!(
                "the session was asked to close and is {:?} for good",
                self.lifecycle
            )
        })?;
        ensure(self.turns.is_empty(), Rule::Turns, || {
            let turns = self
                .turns
                .iter()
                .map(|heard| heard.turn.get())
                .collect::<Vec<_>>();
            format!("the turns {turns:?} never ended")
        })?;
        ensure(self.responses.is_empty(), Rule::Progress, || {
            let tags = self
                .responses
                .iter()
                .map(|spoken| spoken.tag)
                .collect::<Vec<_>>();
            format!("the responses {tags:?} never ended")
        })?;
        ensure(self.gone.is_none(), Rule::Line, || {
            "the line is gone for good, and the session goes on waiting for it".to_owned()
        })?;
        self.desk_at_rest()?;
        ensure(
            self.owed <= self.replied && !view.reply_owed(),
            Rule::Progress,
            || {
                format!(
                    "a turn meant for the agent is item {} of the conversation, and the newest \
                 response was started on item {}",
                    self.owed, self.replied
                )
            },
        )
    }

    /// Takes note of what an event tells about the world. Returns the
    /// response that the event says has played to its end.
    fn delivered(&mut self, now: MonoTime, event: &Event) -> Option<GenId> {
        match event {
            Event::Op(tag, op) => {
                let spoken = self
                    .responses
                    .iter_mut()
                    .find(|spoken| spoken.tag == *tag)?;
                match op {
                    OpEvent::LlmDelta { text } => spoken.script.push_str(text.as_str()),
                    OpEvent::LlmDone { .. }
                    | OpEvent::Failed {
                        part: Part::Think, ..
                    } => spoken.done = true,
                    OpEvent::TtsSegment { last: true, .. } if spoken.finished => {
                        spoken.synthesized = true;
                    }
                    OpEvent::Played {
                        heard_text,
                        drained,
                        ..
                    } => {
                        // Nobody can have heard what was not released.
                        let reported = usize::try_from(*heard_text).unwrap_or(usize::MAX);
                        let mut heard = reported.min(spoken.released.len());
                        while !spoken.released.is_char_boundary(heard) {
                            heard = heard.saturating_sub(1);
                        }
                        spoken.heard = spoken.heard.max(heard);
                        if *drained && spoken.synthesized && spoken.started {
                            return Some(tag.generation);
                        }
                    }
                    _ => {}
                }
            }
            Event::Control(ControlInput::Close { .. }) if self.lifecycle == Lifecycle::Running => {
                self.close_by = Some(now.after(self.closing_time));
            }
            Event::Control(ControlInput::Detached) if self.gone.is_none() => {
                self.gone = Some(now.after(self.config.timing.detach_wait));
                self.dropped = self
                    .responses
                    .iter()
                    .find(|spoken| spoken.asked && spoken.works_on.is_none())
                    .map(|spoken| spoken.tag.generation);
            }
            Event::Control(ControlInput::Attached) => self.gone = None,
            // Nothing is stored that was not emitted.
            Event::Durable(seq) => {
                self.desk.durable = self.desk.durable.max(seq.get().min(self.last_seq));
            }
            _ => {}
        }
        None
    }

    /// Words arrived. They join the speaker's open turn, or the step
    /// commits them as a turn of their own.
    fn words_arrived(&mut self, event: &Event, out: &Out) -> Result<(), Violation> {
        let (who, words, typed) = match event {
            Event::Transcript {
                who,
                text,
                is_final: true,
                ..
            } => (*who, text.as_str().trim(), false),
            Event::TextIn { who, text } => (*who, text.as_str().trim(), true),
            _ => return Ok(()),
        };
        let listening = matches!(self.lifecycle, Lifecycle::Running | Lifecycle::Draining);
        if !listening || !self.people.contains(&who) || words.is_empty() {
            return Ok(());
        }
        let last_turn = self.last_turn;
        let alone = out.facts.iter().any(|stamped| {
            matches!(stamped.fact, Fact::TurnCommitted { turn, who: by, .. }
                if by == who && turn.get() > last_turn)
        });
        if alone {
            self.arrived = Some((who, words.to_owned()));
            return Ok(());
        }
        ensure(!typed, Rule::Turns, || {
            format!("{who} typed {words:?}, and no turn was committed for it")
        })?;
        let spoken = |heard: &&mut Heard| heard.who == who && heard.keys.is_none();
        let Some(heard) = self.turns.iter_mut().find(spoken) else {
            return Err(Violation {
                rule: Rule::Turns,
                what: format!(
                    "{who} said {words:?}; no turn of theirs is open, and none was committed \
                     for the words"
                ),
            });
        };
        if !heard.words.is_empty() {
            heard.words.push(' ');
        }
        heard.words.push_str(words);
        Ok(())
    }

    #[expect(clippy::too_many_lines, reason = "one arm per fact, each a few checks")]
    fn fact(
        &mut self,
        now: MonoTime,
        event: &Event,
        stamped: &Stamped,
        text: &str,
        whole: Option<GenId>,
    ) -> Result<(), Violation> {
        match stamped.fact {
            Fact::ParticipantJoined { who } => {
                ensure(!self.people.contains(&who), Rule::Bounds, || {
                    format!("{who} joined a second time")
                })?;
                self.people.push(who);
            }
            Fact::ParticipantLeft { who } => {
                ensure(self.people.contains(&who), Rule::Bounds, || {
                    format!("{who} left without being in the session")
                })?;
                self.people.retain(|known| *known != who);
            }
            Fact::LifecycleChanged { to, reason } => {
                let next = match self.lifecycle {
                    Lifecycle::Running => Some(Lifecycle::Draining),
                    Lifecycle::Draining => Some(Lifecycle::Closing),
                    Lifecycle::Closing => Some(Lifecycle::Closed),
                    Lifecycle::Closed => None,
                };
                ensure(next == Some(to), Rule::Closing, || {
                    format!("the session went from {:?} to {to:?}", self.lifecycle)
                })?;
                if to == Lifecycle::Draining {
                    self.began_to_close(now, event, reason)?;
                }
                self.lifecycle = to;
            }
            Fact::EpochAdvanced { to, cause } => {
                ensure(to.get() == self.epoch.get() + 1, Rule::Liveness, || {
                    format!("the epoch went from {} to {to}", self.epoch)
                })?;
                self.epoch = to;
                let restored = self.desk.recovering && to.get() == self.began.get() + 1;
                ensure(
                    (cause == EpochCause::Recovery) == restored,
                    Rule::Restore,
                    || format!("the epoch went to {to} for {cause:?}"),
                )?;
                self.floor_cut();
            }
            Fact::TurnCommitted {
                turn,
                who,
                addressed,
                ..
            } => {
                self.context += 1;
                if addressed {
                    self.owed = self.context;
                    self.desk.cause = Some(LlmCause::Reply { turn });
                }
                if let Some(index) = self.turns.iter().position(|heard| heard.turn == turn) {
                    let heard = self.turns.remove(index);
                    let said = heard.keys.is_none() && heard.who == who && heard.words == text;
                    ensure(said, Rule::Turns, || {
                        format!(
                            "turn {turn} was committed as {who} saying {text:?}; what arrived \
                             for it is {} saying {:?}",
                            heard.who, heard.words
                        )
                    })?;
                    self.in_its_order(turn, who, index)?;
                } else {
                    ensure(turn.get() > self.last_turn, Rule::Turns, || {
                        format!("turn {turn} was committed after it had ended")
                    })?;
                    self.last_turn = turn.get();
                    let arrived = self.arrived.take();
                    let accounted = arrived
                        .as_ref()
                        .is_some_and(|(by, words)| *by == who && words == text);
                    ensure(accounted, Rule::Turns, || {
                        format!(
                            "turn {turn} was committed as {who} saying {text:?}, which nobody \
                             was heard to say; what arrived is {arrived:?}"
                        )
                    })?;
                }
            }
            Fact::KeysCommitted { turn, who, .. } => self.keys_committed(event, turn, who, text)?,
            Fact::TurnDismissed { turn, who, reason } => {
                let Some(index) = self.turns.iter().position(|heard| heard.turn == turn) else {
                    // A key may stop the last thing a closing session was
                    // waiting for: its run begins and is dismissed with
                    // the session in one step.
                    let begun = self
                        .pressed
                        .take()
                        .is_some_and(|(by, _, full)| by == who && !full);
                    let closed = reason == Dismissal::Closed
                        && self.lifecycle >= Lifecycle::Closing
                        && turn.get() > self.last_turn;
                    ensure(begun && closed, Rule::Turns, || {
                        format!("turn {turn} was dismissed, and it is not open")
                    })?;
                    self.last_turn = turn.get();
                    return Ok(());
                };
                let heard = self.turns.remove(index);
                // Keys are never nothing, and never an aside.
                let spoken = heard.keys.is_none();
                let holds = heard.who == who
                    && match reason {
                        Dismissal::Empty => spoken && heard.words.is_empty(),
                        Dismissal::Backchannel | Dismissal::Noise => spoken && heard.over_agent,
                        Dismissal::Closed => self.lifecycle >= Lifecycle::Closing,
                    };
                ensure(holds, Rule::Turns, || {
                    format!(
                        "turn {turn}, in which {} said {:?}, was dismissed as {reason:?}",
                        heard.who, heard.words
                    )
                })?;
            }
            Fact::UtteranceProgress { generation, .. } => {
                let Some(spoken) = self.spoken(generation) else {
                    return Err(Violation {
                        rule: Rule::SpokenText,
                        what: format!(
                            "progress was recorded for response {generation}, which is not under way"
                        ),
                    });
                };
                let end = spoken.recorded + text.len();
                let next = spoken.released.get(spoken.recorded..end);
                ensure(
                    next == Some(text) && end <= spoken.heard,
                    Rule::SpokenText,
                    || {
                        format!(
                            "{text:?} was recorded as the next part heard of response {generation}; \
                         it was given {:?} to say, {} bytes of it were heard and {} recorded",
                            spoken.released, spoken.heard, spoken.recorded
                        )
                    },
                )?;
                spoken.recorded = end;
            }
            Fact::UtteranceCommitted {
                generation,
                truncated,
                answers,
                ..
            } => {
                self.context += 1;
                if answers.is_some() && self.dropped == Some(generation) {
                    // The line dropped when all of it had been heard:
                    // nothing of it is owed again.
                    self.dropped = None;
                }
                let Some(spoken) = self.spoken(generation) else {
                    // The one response a session can record without ever
                    // having seen it is the one its earlier life lost.
                    return self.lost_speech(generation, text, truncated, answers.is_some());
                };
                // Speech answers something only when it came from a
                // language model and was heard to the end.
                let may_answer = spoken.asked && !truncated;
                ensure(answers.is_none() || may_answer, Rule::SpokenText, || {
                    format!(
                        "response {generation} is recorded as answering {answers:?}; it was cut \
                         off: {truncated}"
                    )
                })?;
                ensure(!spoken.committed, Rule::SpokenText, || {
                    format!("response {generation} entered the conversation twice")
                })?;
                spoken.committed = true;
                let heard = spoken.audible(whole == Some(generation));
                ensure(!text.is_empty() && text == heard, Rule::SpokenText, || {
                    format!(
                        "response {generation} entered the conversation as {text:?}; what was \
                         heard of it is {heard:?}"
                    )
                })?;
                // Whether more was to come is known for certain in two
                // cases: less was heard than was released, or everything
                // was released and marked as the end.
                let all = spoken.released.trim_end().len();
                let cut = if text.len() < all {
                    Some(true)
                } else if spoken.finished {
                    Some(false)
                } else {
                    None
                };
                ensure(
                    cut.is_none_or(|cut| cut == truncated),
                    Rule::SpokenText,
                    || {
                        format!(
                            "response {generation} entered the conversation as {text:?} with \
                         truncated: {truncated}; it was given {:?} to say",
                            spoken.released
                        )
                    },
                )?;
            }
            Fact::NoticeAppended { kind, .. } => self.noted(event, kind, text)?,
            Fact::ToolProposed {
                call,
                batch,
                generation,
                tool,
                ..
            } => self.call_proposed(event, call, batch, generation, tool, text)?,
            Fact::ToolAuthorized { call } => self.call_cleared(stamped.seq.get(), call)?,
            Fact::ToolDispatched { call, attempt } => self.call_sent(call, attempt)?,
            Fact::ToolFinished { call, outcome, .. } => {
                self.call_ended(event, call, outcome, text)?;
            }
            Fact::BatchResolved { batch } => self.batch_resolved(batch)?,
            Fact::HandoffRequested { to } => self.handoff_asked(event, to)?,
            Fact::AgentActivated { agent } => self.agent_activated(event, agent)?,
        }
        Ok(())
    }

    fn spoken(&mut self, generation: GenId) -> Option<&mut Spoken> {
        self.responses
            .iter_mut()
            .find(|spoken| spoken.tag.generation == generation)
    }

    fn is_open(&self, turn: TurnId) -> bool {
        self.turns.iter().any(|heard| heard.turn == turn)
    }

    /// Where the response a command names is, taking note of it if this is
    /// the first that is seen of it.
    fn response(&mut self, tag: OpTag, event: &Event) -> Result<usize, Violation> {
        let known = self
            .responses
            .iter()
            .position(|spoken| spoken.tag.generation == tag.generation);
        if let Some(index) = known {
            let same = self
                .responses
                .get(index)
                .is_some_and(|spoken| spoken.tag == tag);
            ensure(same, Rule::Liveness, || {
                format!("a command carries {tag:?}, and its response began in another epoch")
            })?;
            return Ok(index);
        }
        ensure(
            tag.generation.get() > self.last_generation,
            Rule::Liveness,
            || format!("a command was emitted for {tag:?}, which has ended"),
        )?;
        // The epoch may have moved since it began, within the step: the
        // facts are read before the commands.
        let current = (self.entered..=self.epoch).contains(&tag.epoch);
        ensure(current, Rule::Liveness, || {
            format!(
                "{tag:?} began in a step that took the session from epoch {} to {}",
                self.entered, self.epoch
            )
        })?;
        ensure(
            self.responses.len() < self.limits.generations,
            Rule::Bounds,
            || {
                format!(
                    "more than {} responses are under way",
                    self.limits.generations
                )
            },
        )?;
        self.last_generation = tag.generation.get();
        self.responses.push(Spoken::begin(tag, event));
        Ok(self.responses.len() - 1)
    }

    /// Takes note of what a step's commands tell that its facts depend
    /// on: the turns they open, the ones they say are spoken over the
    /// agent, and the responses they end.
    fn foreseen(
        &mut self,
        event: &Event,
        command: &Command,
        whole: Option<GenId>,
    ) -> Result<(), Violation> {
        match *command {
            Command::Emit(ClientEvent::Agent {
                generation: Some(generation),
                activity: Activity::Listening,
                ..
            }) => {
                // A response ends by itself in two ways: it is heard to
                // its end, or its language model stops having written
                // nothing. Every other end cuts it off.
                let silent = self
                    .spoken(generation)
                    .is_none_or(|spoken| spoken.script.trim().is_empty());
                let stopped = matches!(event, Event::Op(tag, OpEvent::LlmDone { .. }
                    | OpEvent::Failed { part: Part::Think, .. }) if tag.generation == generation);
                let cut = whole != Some(generation) && !(stopped && silent);
                self.desk.ending.push((generation, cut));
                if !cut {
                    self.asker_done(generation);
                }
            }
            // Its language model writes until the step is over: what it
            // asked for in this step, it asked for in time.
            Command::Llm(tag, LlmCmd::Cancel) => self.desk.stopping.push(tag),
            Command::Emit(ClientEvent::UserSpeaking {
                who,
                turn,
                speaking: true,
            }) if !self.is_open(turn) => {
                // Whether there is room for it, and whether its speaker
                // has no other turn open, is checked when the step is
                // over: a turn that ends in this step is still in the
                // account here.
                let fresh = turn.get() > self.last_turn;
                ensure(fresh && self.people.contains(&who), Rule::Turns, || {
                    format!("turn {turn} began for {who}, and it cannot have")
                })?;
                self.last_turn = turn.get();
                self.turns.push(Heard {
                    turn,
                    who,
                    words: String::new(),
                    over_agent: false,
                    keys: None,
                });
            }
            Command::Sense(SenseCmd::ClassifyOverlap { turn, .. }) => {
                if let Some(heard) = self.turns.iter_mut().find(|heard| heard.turn == turn) {
                    heard.over_agent = true;
                }
            }
            _ => {}
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per command, each a few checks"
    )]
    fn command(
        &mut self,
        event: &Event,
        command: &Command,
        text: &str,
        whole: Option<GenId>,
    ) -> Result<(), Violation> {
        match *command {
            Command::Emit(ClientEvent::UserSpeaking { turn, .. })
            | Command::Sense(SenseCmd::EstimateEou { turn, .. }) => {
                ensure(turn.get() <= self.last_turn, Rule::Turns, || {
                    format!("{command:?} is about turn {turn}, which never began")
                })?;
            }
            Command::Sense(SenseCmd::ClassifyOverlap { turn, .. }) => {
                let known = turn.get() <= self.last_turn;
                ensure(self.voice.is_some() && known, Rule::OneVoice, || {
                    format!("turn {turn} was taken for speech over the agent's, and it is not")
                })?;
            }
            // The agent that worked on a call that ended in this step is
            // gone from the account already.
            Command::Llm(tag, LlmCmd::Cancel) if self.retires(tag) => {}
            Command::Llm(tag, llm) => {
                let index = self.response(tag, event)?;
                let (running, context) = (self.lifecycle == Lifecycle::Running, self.context);
                let Some(spoken) = self.responses.get_mut(index) else {
                    return Ok(());
                };
                let holds = match llm {
                    LlmCmd::Start {
                        agent,
                        context: on,
                        cause,
                    } => {
                        let first = !spoken.asked;
                        if first {
                            // What a language model is asked for, it
                            // writes: nothing was given to it to say, even
                            // in a step that begins with words to say.
                            spoken.script.clear();
                        }
                        spoken.asked = true;
                        spoken.agent = Some(agent);
                        let works_on = self.started(tag, agent, cause)?;
                        if works_on.is_none() {
                            ensure(self.gone.is_none(), Rule::Line, || {
                                format!("{tag:?} was started for {cause:?} while the line is gone")
                            })?;
                            self.replied = self.replied.max(on.get());
                        }
                        if let Some(spoken) = self.responses.get_mut(index) {
                            spoken.works_on = works_on;
                        }
                        first && running && on.get() <= context
                    }
                    LlmCmd::Pause => {
                        let may = spoken.asked && !spoken.waiting;
                        spoken.waiting = true;
                        may
                    }
                    LlmCmd::Resume => {
                        let may = spoken.waiting;
                        spoken.waiting = false;
                        may
                    }
                    LlmCmd::Cancel => {
                        spoken.done = true;
                        spoken.asked
                    }
                };
                ensure(holds, Rule::Liveness, || {
                    format!(
                        "the language model was told {llm:?} for {tag:?}, which makes no sense now"
                    )
                })?;
            }
            Command::Tts(tag, tts) => {
                let index = self.response(tag, event)?;
                let Some(spoken) = self.responses.get_mut(index) else {
                    return Ok(());
                };
                ensure(spoken.works_on.is_none(), Rule::OneVoice, || {
                    format!("{tag:?} works on a tool call, and was given something to say")
                })?;
                match tts {
                    TtsCmd::Speak { .. } => {
                        // White space alone is nothing to say.
                        let open = !spoken.finished && !text.trim().is_empty();
                        spoken.released.push_str(text);
                        let written = spoken.script.starts_with(spoken.released.as_str());
                        ensure(open && written, Rule::SpokenText, || {
                            format!(
                                "{tag:?} has now been given {:?} to say; what was written for \
                                 it is {:?}",
                                spoken.released, spoken.script
                            )
                        })?;
                    }
                    TtsCmd::Finish => {
                        let may = !spoken.finished && !spoken.released.is_empty();
                        spoken.finished = true;
                        ensure(may, Rule::SpokenText, || {
                            format!(
                                "synthesis for {tag:?} was told twice, or with no text, that it is done"
                            )
                        })?;
                    }
                    TtsCmd::Cancel => {
                        spoken.silenced = true;
                        ensure(!spoken.released.is_empty(), Rule::SpokenText, || {
                            format!(
                                "synthesis for {tag:?} was cancelled, and it was never given text"
                            )
                        })?;
                    }
                }
            }
            Command::Playout(tag, playout) => {
                let index = self.response(tag, event)?;
                let generation = tag.generation;
                let voice = self.voice;
                let Some(spoken) = self.responses.get_mut(index) else {
                    return Ok(());
                };
                ensure(spoken.works_on.is_none(), Rule::OneVoice, || {
                    format!("{tag:?} works on a tool call, and was given a voice")
                })?;
                let holds = match playout {
                    PlayoutCmd::Start => {
                        let may = voice.is_none() && !spoken.started && !spoken.released.is_empty();
                        spoken.started = true;
                        self.voice = Some(generation);
                        may
                    }
                    PlayoutCmd::Duck => {
                        let may = voice == Some(generation) && !spoken.lowered;
                        spoken.lowered = true;
                        may
                    }
                    PlayoutCmd::Unduck => {
                        let may = voice == Some(generation) && spoken.lowered;
                        spoken.lowered = false;
                        may
                    }
                    PlayoutCmd::Stop { .. } => {
                        let may = voice == Some(generation) && !spoken.stopped;
                        spoken.stopped = true;
                        may
                    }
                };
                ensure(holds, Rule::OneVoice, || {
                    format!(
                        "playback was told {playout:?} for {tag:?} while {voice:?} is being heard"
                    )
                })?;
            }
            Command::Emit(ClientEvent::Agent {
                generation: Some(generation),
                activity,
                ..
            }) => match activity {
                Activity::Listening => self.ended(generation, whole)?,
                Activity::Thinking | Activity::Speaking => {
                    ensure(self.spoken(generation).is_some(), Rule::Liveness, || {
                        format!(
                            "response {generation} is reported as {activity:?}, and it is not under way"
                        )
                    })?;
                }
            },
            Command::Tool(call, tool) => self.tool_command(call, tool, text)?,
            Command::AwaitDurable(seq) => self.storing_asked(seq.get())?,
            // Timers are only wake-ups, and what a kernel tells the
            // transcriber is advice.
            Command::Emit(_) | Command::SetTimer(..) | Command::Stt(_) => {}
        }
        Ok(())
    }

    /// A response ended.
    fn ended(&mut self, generation: GenId, whole: Option<GenId>) -> Result<(), Violation> {
        let known = self
            .responses
            .iter()
            .position(|spoken| spoken.tag.generation == generation);
        let Some(index) = known else {
            // It began and ended within the step, with no command of its
            // own in between.
            ensure(
                generation.get() > self.last_generation,
                Rule::Liveness,
                || format!("response {generation} ended a second time"),
            )?;
            self.last_generation = generation.get();
            return Ok(());
        };
        let spoken = self.responses.remove(index);
        if self.voice == Some(generation) {
            self.voice = None;
        }
        // No part is left working for a response that is over.
        let whole = whole == Some(generation);
        let writing = spoken.asked && !spoken.done;
        let synthesizing = !spoken.released.is_empty() && !spoken.synthesized && !spoken.silenced;
        let playing = spoken.started && !whole && !spoken.stopped;
        ensure(
            !writing && !synthesizing && !playing,
            Rule::Liveness,
            || {
                format!(
                    "response {generation} ended and left work going on for it: its language \
                     model writing: {writing}, synthesis: {synthesizing}, playback: {playing}"
                )
            },
        )?;
        let heard = spoken.audible(whole);
        ensure(
            spoken.committed || heard.is_empty(),
            Rule::SpokenText,
            || {
                format!(
                    "response {generation} ended, and {heard:?}, which was heard of it, did not \
                 enter the conversation"
                )
            },
        )
    }

    /// Compares the account with the kernel's own state after the step.
    #[expect(
        clippy::too_many_lines,
        reason = "a list of comparisons, one after the other"
    )]
    fn settled(
        &mut self,
        now: MonoTime,
        event: &Event,
        out: &Out,
        view: View<'_>,
        whole: Option<GenId>,
    ) -> Result<(), Violation> {
        ensure(self.arrived.is_none(), Rule::Turns, || {
            format!("the words {:?} ended in no turn", self.arrived)
        })?;
        if let Some(generation) = whole {
            ensure(self.spoken(generation).is_none(), Rule::SpokenText, || {
                format!("response {generation} played to its end and is still under way")
            })?;
        }

        // A response that began without a command of its own.
        for running in view.generations() {
            if self.spoken(running.tag.generation).is_none() {
                self.response(running.tag, event)?;
            }
        }
        ensure(
            view.generations().count() == self.responses.len(),
            Rule::Liveness,
            || {
                let live = view
                    .generations()
                    .map(|running| running.tag)
                    .collect::<Vec<_>>();
                let seen = self
                    .responses
                    .iter()
                    .map(|spoken| spoken.tag)
                    .collect::<Vec<_>>();
                format!(
                    "the kernel has the responses {live:?} under way; its commands say {seen:?}"
                )
            },
        )?;
        for running in view.generations() {
            let Some(spoken) = self
                .responses
                .iter()
                .find(|spoken| spoken.tag == running.tag)
            else {
                continue;
            };
            ensure(
                !spoken.committed && !spoken.stopped,
                Rule::SpokenText,
                || {
                    format!(
                        "{:?} was stopped or entered the conversation, and is still under way",
                        spoken.tag
                    )
                },
            )?;
            let over = view
                .turns()
                .any(|open| open.ducking == Some(running.tag.generation));
            let agrees = running.speaking == spoken.started
                && running.lowered == spoken.lowered
                && running.lowered == over;
            ensure(agrees, Rule::OneVoice, || {
                format!(
                    "{:?} is heard: {}, lowered: {}; the commands say heard: {}, lowered: {}; \
                     a turn is open over it: {over}",
                    running.tag, running.speaking, running.lowered, spoken.started, spoken.lowered
                )
            })?;
        }
        let voices = view
            .generations()
            .filter(|running| running.speaking)
            .count();
        ensure(voices <= 1, Rule::OneVoice, || {
            format!("{voices} responses are being heard at once")
        })?;

        ensure(view.epoch() == self.epoch, Rule::Liveness, || {
            format!(
                "the kernel is in epoch {}; its facts say {}",
                view.epoch(),
                self.epoch
            )
        })?;
        ensure(view.lifecycle() == self.lifecycle, Rule::Closing, || {
            format!(
                "the kernel is {:?}; its facts say {:?}",
                view.lifecycle(),
                self.lifecycle
            )
        })?;
        ensure(
            view.next_seq().get() == self.last_seq + 1,
            Rule::Journal,
            || {
                format!(
                    "the kernel's next fact is #{}; its last was #{}",
                    view.next_seq(),
                    self.last_seq
                )
            },
        )?;
        ensure(
            view.context_version().get() == self.context,
            Rule::SpokenText,
            || {
                format!(
                    "the conversation holds {} items; the facts add up to {}",
                    view.context_version(),
                    self.context
                )
            },
        )?;
        ensure(
            view.participants().eq(self.people.iter().copied()),
            Rule::Bounds,
            || {
                format!(
                    "the kernel's participants differ from what its facts say: {:?}",
                    self.people
                )
            },
        )?;
        let open = view.turns().map(|open| open.turn);
        ensure(
            open.eq(self.turns.iter().map(|heard| heard.turn)),
            Rule::Turns,
            || {
                let open = view.turns().map(|open| open.turn.get()).collect::<Vec<_>>();
                let seen = self
                    .turns
                    .iter()
                    .map(|heard| heard.turn.get())
                    .collect::<Vec<_>>();
                format!("the kernel has the turns {open:?} open; its emissions say {seen:?}")
            },
        )?;

        // Nobody is in two turns of speech at once, and nobody presses two
        // runs of keys at once. What somebody said and what they pressed
        // may both be open.
        let same = |one: &Heard, other: &Heard| match (one.keys, other.keys) {
            (None, None) => true,
            (Some(one), Some(other)) => !one.over && !other.over,
            _ => false,
        };
        let alone = self.turns.iter().enumerate().all(|(index, heard)| {
            self.turns
                .iter()
                .skip(index + 1)
                .all(|other| other.who != heard.who || !same(heard, other))
        });
        ensure(alone, Rule::Turns, || {
            let open = self
                .turns
                .iter()
                .map(|heard| (heard.turn.get(), heard.who, heard.keys.is_some()))
                .collect::<Vec<_>>();
            format!("someone has two turns of one kind open at once: {open:?}")
        })?;

        self.desk_settled(view)?;
        self.line_settled(now, event, view)?;

        let limits = self.limits;
        let within = self.people.len() <= limits.participants
            && self.turns.len() <= limits.open_turns
            && self.responses.len() <= limits.generations
            && self.desk.calls.len() <= limits.open_calls;
        let tables =
            limits.participants + limits.open_turns + limits.generations + limits.open_calls;
        let most = 8 * (tables + 4);
        ensure(
            within && out.cmds.len() <= most && out.facts.len() <= most,
            Rule::Bounds,
            || {
                format!(
                    "{} people, {} turns, {} responses; the step emitted {} commands and {} facts",
                    self.people.len(),
                    self.turns.len(),
                    self.responses.len(),
                    out.cmds.len(),
                    out.facts.len()
                )
            },
        )?;

        let in_time = self
            .close_by
            .is_none_or(|by| now < by || self.lifecycle == Lifecycle::Closed);
        ensure(in_time, Rule::Closing, || {
            format!(
                "the session is still {:?}, {} ms after it was asked to close",
                self.lifecycle,
                self.closing_time.as_millis()
            )
        })
    }
}

impl Monitor {
    /// The session begins to close. It does when it is asked to, for the
    /// reason it was given. By itself it does only when its line has been
    /// gone for as long as it waits for one.
    fn began_to_close(
        &mut self,
        now: MonoTime,
        event: &Event,
        reason: Option<CloseReason>,
    ) -> Result<(), Violation> {
        let asked = matches!(event, Event::Control(ControlInput::Close { reason: why })
            if Some(*why) == reason);
        let waited =
            reason == Some(CloseReason::Abandoned) && self.gone.is_some_and(|until| now >= until);
        ensure(asked || waited, Rule::Line, || {
            format!(
                "the session began to close for {reason:?} on {event:?}; nobody asked it to, \
                 and the wait for its line ends at {:?}",
                self.gone
            )
        })?;
        // What closes by itself has the time a session that was asked
        // has.
        self.close_by.get_or_insert(now.after(self.closing_time));
        Ok(())
    }

    /// Compares what the account says of the line with what the kernel
    /// did about it.
    fn line_settled(
        &mut self,
        now: MonoTime,
        event: &Event,
        view: View<'_>,
    ) -> Result<(), Violation> {
        ensure(view.gone_until() == self.gone, Rule::Line, || {
            format!(
                "the kernel waits for its line until {:?}; what it was told says {:?}",
                view.gone_until(),
                self.gone
            )
        })?;
        if let Some(generation) = self.dropped.take()
            && self.desk.cut_by_line(generation)
            && self.owed > 0
        {
            self.replied = self.replied.min(self.owed - 1);
        }
        let Some(until) = self.gone else {
            return Ok(());
        };
        // Whoever works on a tool call goes on. Nobody else says or
        // prepares anything.
        let speaks = self
            .responses
            .iter()
            .find(|spoken| spoken.works_on.is_none())
            .map(|spoken| spoken.tag);
        ensure(speaks.is_none(), Rule::Line, || {
            format!("{speaks:?} holds the floor while the line is gone")
        })?;
        // The wait is over when the session is told so: by the timer it
        // set, or at once when it waits no time at all.
        let told = matches!(
            event,
            Event::Timer(TimerId::Detach) | Event::Control(ControlInput::Detached)
        );
        let over = told && now >= until;
        ensure(
            !over || self.lifecycle != Lifecycle::Running,
            Rule::Line,
            || {
                format!(
                    "the line has been gone since before {until:?}, the session was told on \
                     {event:?}, and it is still running"
                )
            },
        )
    }
}

impl Monitor {
    /// Whether a run of `pressed` keys is complete without a key that
    /// ends it.
    fn is_run(&self, pressed: usize) -> bool {
        let count = self.config.keys.count;
        count.is_some_and(|count| pressed >= usize::from(count)) || pressed >= self.limits.keys
    }

    /// A key was pressed, or the time in which one could follow may be
    /// over. A key joins the run its presser has under way, or the step
    /// begins one with it. Returns the response that was being heard when
    /// a key was pressed: a key takes the floor.
    fn key_arrived(&mut self, now: MonoTime, event: &Event) -> Option<GenId> {
        let policy = self.config.keys;
        match *event {
            Event::Key { who, key } => {
                let listening = matches!(self.lifecycle, Lifecycle::Running | Lifecycle::Draining);
                if !listening || !self.people.contains(&who) {
                    return None;
                }
                let ends = policy.terminator == Some(key);
                let under_way = self
                    .turns
                    .iter()
                    .position(|heard| heard.who == who && heard.keys.is_some_and(|run| !run.over));
                if let Some(heard) = under_way.and_then(|index| self.turns.get_mut(index)) {
                    if !ends {
                        heard.words.push(key.as_char());
                    }
                    let pressed = heard.words.len();
                    let over = ends || self.is_run(pressed);
                    if let Some(heard) = under_way.and_then(|index| self.turns.get_mut(index)) {
                        heard.keys = Some(Run { last: now, over });
                    }
                } else {
                    let full = self.turns.len() >= self.limits.open_turns;
                    self.pressed = Some((who, key, full));
                }
                self.voice
            }
            Event::Timer(TimerId::Keys(turn)) => {
                let run = self
                    .turns
                    .iter_mut()
                    .filter(|heard| heard.turn == turn)
                    .find_map(|heard| heard.keys.as_mut());
                if let Some(run) = run
                    && now.since(run.last) >= policy.wait
                {
                    run.over = true;
                }
                None
            }
            _ => None,
        }
    }

    /// A turn that was open entered the conversation. What somebody said
    /// and what they pressed enter it in the order they began in.
    fn in_its_order(
        &self,
        turn: TurnId,
        who: ParticipantId,
        index: usize,
    ) -> Result<(), Violation> {
        let before = self
            .turns
            .iter()
            .take(index)
            .find(|heard| heard.who == who)
            .map(|heard| heard.turn);
        ensure(before.is_none(), Rule::Turns, || {
            format!(
                "turn {turn} of {who} entered the conversation before their turn {before:?}, \
                 which began before it"
            )
        })
    }

    /// Keys entered the conversation. They are a run that somebody
    /// pressed, all of it, and one no key can follow.
    fn keys_committed(
        &mut self,
        event: &Event,
        turn: TurnId,
        who: ParticipantId,
        keys: &str,
    ) -> Result<(), Violation> {
        self.context += 1;
        self.owed = self.context;
        self.desk.cause = Some(LlmCause::Reply { turn });
        let Some(index) = self.turns.iter().position(|heard| heard.turn == turn) else {
            // A run of one key begins and is over in one step.
            ensure(turn.get() > self.last_turn, Rule::Turns, || {
                format!("turn {turn} was committed as keys after it had ended")
            })?;
            self.last_turn = turn.get();
            let pressed = self.pressed.take();
            let accounted = pressed.is_some_and(|(by, key, full)| {
                let ends = self.config.keys.terminator == Some(key);
                let alone = keys.chars().eq([key.as_char()]) && (ends || self.is_run(1));
                by == who && alone && !full
            });
            ensure(accounted, Rule::Turns, || {
                format!(
                    "turn {turn} was committed as {who} pressing {keys:?}; what was pressed \
                     and has no run to join is {pressed:?}"
                )
            })?;
            let waits = self.turns.iter().any(|heard| heard.who == who);
            return ensure(!waits, Rule::Turns, || {
                format!(
                    "the keys {keys:?} of {who} entered the conversation before what they began before"
                )
            });
        };
        let heard = self.turns.remove(index);
        let pressed = heard.keys.is_some() && heard.who == who && heard.words == keys;
        ensure(pressed, Rule::Turns, || {
            format!(
                "turn {turn} was committed as {who} pressing {keys:?}; what arrived for it is \
                 {} with {:?}, keys: {}",
                heard.who,
                heard.words,
                heard.keys.is_some()
            )
        })?;
        // Whoever leaves presses no more. And what somebody said after
        // their keys does not enter the conversation before them: when
        // they speak anew and that turn has to end, the keys end with it.
        let left =
            matches!(event, Event::Control(ControlInput::Leave { who: gone }) if *gone == who);
        let over = heard.keys.is_some_and(|run| run.over);
        if !over && !left {
            let anew = matches!(
                event,
                Event::Speech { who: speaker, edge: VadEdge::Start, .. } if *speaker == who
            );
            let after = self
                .turns
                .iter()
                .skip(index)
                .find(|later| later.who == who && later.keys.is_none())
                .map(|later| later.turn);
            ensure(anew && after.is_some(), Rule::Turns, || {
                format!("the keys {keys:?} of {who} were committed while another could follow")
            })?;
            self.forced = after;
        }
        self.in_its_order(turn, who, index)
    }

    /// Compares the account of the keys with what the kernel did about
    /// them in a step. `spoke` is the response that was being heard when
    /// the step's key was pressed.
    fn keys_settled(
        &mut self,
        now: MonoTime,
        event: &Event,
        view: View<'_>,
        spoke: Option<GenId>,
    ) -> Result<(), Violation> {
        // A key that had no run to join began one, unless as many turns
        // are open as may be.
        if let Some((who, key, full)) = self.pressed.take() {
            let begun = view
                .turns()
                .find(|open| open.keys && open.who == who && !self.is_open(open.turn))
                .filter(|_| !full);
            if let Some(open) = begun {
                let fresh = open.turn.get() > self.last_turn;
                ensure(fresh, Rule::Turns, || {
                    format!(
                        "turn {} began for the keys of {who}, and it cannot have",
                        open.turn
                    )
                })?;
                self.last_turn = open.turn.get();
                let ends = self.config.keys.terminator == Some(key);
                self.turns.push(Heard {
                    turn: open.turn,
                    who,
                    words: key.as_char().to_string(),
                    over_agent: false,
                    keys: Some(Run {
                        last: now,
                        over: ends || self.is_run(1),
                    }),
                });
            } else {
                ensure(full, Rule::Turns, || {
                    format!("{who} pressed {key:?} on {event:?}, and no turn began for it")
                })?;
            }
        }
        if let Some(turn) = self.forced.take() {
            ensure(!self.is_open(turn), Rule::Turns, || {
                format!(
                    "keys were committed while another could follow, and turn {turn}, which was \
                     said after them, did not end"
                )
            })?;
        }
        // A key is never a noise of agreement: what was being heard when
        // it was pressed is not heard after it.
        let heard_on = spoke.filter(|_| self.voice == spoke);
        ensure(heard_on.is_none(), Rule::Turns, || {
            format!("a key was pressed over {heard_on:?}, which goes on being heard")
        })?;
        // A run that is over is a turn at once, unless what whoever
        // pressed it began before is still open.
        let listening = matches!(self.lifecycle, Lifecycle::Running | Lifecycle::Draining);
        let waiting = self.turns.iter().enumerate().find(|(index, heard)| {
            heard.keys.is_some_and(|run| run.over)
                && !self
                    .turns
                    .iter()
                    .take(*index)
                    .any(|before| before.who == heard.who)
        });
        let waiting = waiting.map(|(_, heard)| heard.turn).filter(|_| listening);
        ensure(waiting.is_none(), Rule::Turns, || {
            format!(
                "the keys of turn {waiting:?} are over, and they have not entered the conversation"
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use waveport_domain::command::{Command, PlayoutCmd};
    use waveport_domain::event::{
        CloseReason, ControlInput, Event, Finish, Key, OpEvent, TimerId, VadEdge,
    };
    use waveport_domain::fact::{Fact, Stamped};
    use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId, Prob, Seq, TurnId};

    use waveport_domain::config::KernelConfig;
    use waveport_domain::kernel::Kernel;
    use waveport_domain::out::Out;
    use waveport_domain::policy::DefaultPolicies;
    use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

    use super::{Monitor, Rule, Violation};

    const ANA: ParticipantId = ParticipantId(1);
    const REPLY: OpTag = OpTag {
        generation: GenId(1),
        epoch: Epoch(0),
    };

    /// A kernel with a monitor watching it.
    struct Watched {
        kernel: Kernel,
        monitor: Monitor,
        out: Out,
        now_ms: u64,
    }

    impl Watched {
        fn new() -> Self {
            Self::with(KernelConfig::default())
        }

        fn with(config: KernelConfig) -> Self {
            let kernel = Kernel::new(config, DefaultPolicies::default()).unwrap();
            let mut watched = Self {
                monitor: Monitor::new(kernel.view()),
                kernel,
                out: Out::new(),
                now_ms: 0,
            };
            watched
                .step(Event::Control(ControlInput::Join { who: ANA }))
                .unwrap();
            watched
        }

        fn step(&mut self, event: Event) -> Result<(), Violation> {
            self.tampered(event, |_| {})
        }

        /// Steps the kernel and shows the monitor what `tamper` makes of
        /// its emissions.
        #[expect(
            clippy::needless_pass_by_value,
            reason = "each call builds its event in place"
        )]
        fn tampered(
            &mut self,
            event: Event,
            tamper: impl FnOnce(&mut Out),
        ) -> Result<(), Violation> {
            let now = MonoTime::from_millis(self.now_ms);
            self.kernel.step(now, &event, &mut self.out);
            tamper(&mut self.out);
            self.monitor
                .observe(now, &event, &self.out, self.kernel.view())
        }

        fn at(&mut self, millis: u64, event: Event) -> Result<(), Violation> {
            self.now_ms = millis;
            self.step(event)
        }

        /// Ana asks something, up to the last report but one.
        fn asking(&mut self) {
            let at = MediaTime::from_millis;
            let edge = |edge, millis| Event::Speech {
                who: ANA,
                edge,
                at: at(millis),
            };
            self.at(0, edge(VadEdge::Start, 0)).unwrap();
            self.at(900, edge(VadEdge::End, 900)).unwrap();
            self.at(
                960,
                Event::Eou {
                    turn: TurnId(1),
                    p: Prob::ONE,
                },
            )
            .unwrap();
            self.now_ms = 1_100;
        }

        fn words() -> Event {
            Event::Transcript {
                who: ANA,
                span: MediaSpan::new(MediaTime::ZERO, MediaTime::from_millis(900)),
                text: "where is my order".into(),
                is_final: true,
            }
        }

        /// Ana asks, and the agent is heard to say half of its answer.
        fn answering(&mut self) {
            self.asking();
            self.step(Self::words()).unwrap();
            let text = "It shipped on Monday. ";
            self.step(Event::Op(REPLY, OpEvent::LlmDelta { text: text.into() }))
                .unwrap();
            self.step(Event::Op(
                REPLY,
                OpEvent::LlmDone {
                    finish: Finish::Stop,
                },
            ))
            .unwrap();
            for last in [false, true] {
                let segment = OpEvent::TtsSegment {
                    text_end: 22,
                    audio_end: MediaDur::from_millis(1_500),
                    last,
                };
                self.step(Event::Op(REPLY, segment)).unwrap();
            }
            self.step(Self::played(600, 10, false)).unwrap();
        }

        fn played(millis: u32, heard_text: u32, drained: bool) -> Event {
            let position = MediaDur::from_millis(millis);
            let played = OpEvent::Played {
                handed: position,
                acked: position,
                heard_text,
                drained,
            };
            Event::Op(REPLY, played)
        }
    }

    fn rule(checked: Result<(), Violation>) -> Option<Rule> {
        checked.err().map(|violation| violation.rule)
    }

    #[test]
    fn an_honest_session_is_accepted() {
        let mut watched = Watched::new();
        watched.answering();
        assert_eq!(
            watched
                .monitor
                .at_rest(watched.kernel.view())
                .map_err(|v| v.rule),
            Err(Rule::Progress)
        );
        assert_eq!(watched.step(Watched::played(1_500, 22, true)), Ok(()));
        assert_eq!(watched.monitor.at_rest(watched.kernel.view()), Ok(()));
    }

    #[test]
    fn words_that_change_on_the_way_into_the_conversation_are_noticed() {
        let mut watched = Watched::new();
        watched.asking();
        let other = |out: &mut Out| {
            let text = out.text.push("where is my border");
            for stamped in &mut out.facts {
                if let Fact::TurnCommitted { text: words, .. } = &mut stamped.fact {
                    *words = text;
                }
            }
        };
        let checked = watched.tampered(Watched::words(), other);
        assert_eq!(rule(checked.clone()), Some(Rule::Turns), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .starts_with("invariant 2 is broken: turn 1 was committed")
        );
    }

    #[test]
    fn words_that_end_in_no_turn_are_noticed() {
        let mut watched = Watched::new();
        let late = watched.tampered(Watched::words(), |out| out.facts.clear());
        assert_eq!(rule(late), Some(Rule::Turns));

        let mut watched = Watched::new();
        watched.asking();
        watched.step(Watched::words()).unwrap();
        // The turn is committed a second time.
        let again = watched.tampered(Event::Timer(TimerId::StageAck), |out| {
            let text = out.text.push("where is my order");
            let fact = Fact::TurnCommitted {
                turn: TurnId(1),
                who: ANA,
                span: MediaSpan::default(),
                text,
                addressed: true,
            };
            out.facts.push(Stamped { seq: Seq(3), fact });
        });
        assert_eq!(rule(again), Some(Rule::Turns));
    }

    #[test]
    fn text_nobody_heard_is_noticed() {
        let mut watched = Watched::new();
        watched.answering();
        // Interrupted after "It shipped", the response is recorded whole.
        let whole = |out: &mut Out| {
            let text = out.text.push("It shipped on Monday.");
            for stamped in &mut out.facts {
                if let Fact::UtteranceCommitted { text: said, .. } = &mut stamped.fact {
                    *said = text;
                }
            }
        };
        let checked = watched.tampered(Event::Control(ControlInput::Interrupt), whole);
        assert_eq!(rule(checked), Some(Rule::SpokenText));

        // Or not recorded at all.
        let mut watched = Watched::new();
        watched.answering();
        let nothing = |out: &mut Out| {
            out.facts
                .retain(|stamped| !matches!(stamped.fact, Fact::UtteranceCommitted { .. }));
            for (number, stamped) in (3..).zip(&mut out.facts) {
                stamped.seq = Seq(number);
            }
        };
        let checked = watched.tampered(Event::Control(ControlInput::Interrupt), nothing);
        assert_eq!(rule(checked), Some(Rule::SpokenText));
    }

    #[test]
    fn a_response_recorded_as_whole_when_it_was_cut_is_noticed() {
        let mut watched = Watched::new();
        watched.answering();
        let complete = |out: &mut Out| {
            for stamped in &mut out.facts {
                if let Fact::UtteranceCommitted { truncated, .. } = &mut stamped.fact {
                    *truncated = false;
                }
            }
        };
        let checked = watched.tampered(Event::Control(ControlInput::Interrupt), complete);
        assert_eq!(rule(checked), Some(Rule::SpokenText));
    }

    #[test]
    fn a_command_for_a_response_that_has_ended_is_noticed() {
        let mut watched = Watched::new();
        watched.answering();
        watched.step(Watched::played(1_500, 22, true)).unwrap();
        let checked = watched.tampered(Event::Timer(TimerId::StageAck), |out| {
            out.cmds.push(Command::Playout(REPLY, PlayoutCmd::Start));
        });
        assert_eq!(rule(checked), Some(Rule::Liveness));
    }

    #[test]
    fn a_second_voice_is_noticed() {
        let mut watched = Watched::new();
        watched.answering();
        let other = OpTag {
            generation: GenId(2),
            epoch: Epoch(0),
        };
        let checked = watched.tampered(Event::Timer(TimerId::StageAck), |out| {
            out.cmds.push(Command::Playout(other, PlayoutCmd::Start));
        });
        assert_eq!(rule(checked), Some(Rule::OneVoice));
    }

    #[test]
    fn an_epoch_that_does_not_move_is_noticed() {
        let mut watched = Watched::new();
        watched.answering();
        let checked = watched.tampered(Event::Control(ControlInput::Interrupt), |out| {
            out.facts
                .retain(|stamped| !matches!(stamped.fact, Fact::EpochAdvanced { .. }));
        });
        assert_eq!(rule(checked), Some(Rule::Liveness));
    }

    #[test]
    fn a_gap_in_the_numbering_is_noticed() {
        let mut watched = Watched::new();
        watched.asking();
        let checked = watched.tampered(Watched::words(), |out| {
            for stamped in &mut out.facts {
                stamped.seq = Seq(stamped.seq.get() + 1);
            }
        });
        assert_eq!(rule(checked), Some(Rule::Journal));
    }

    #[test]
    fn a_session_that_does_not_close_in_time_is_noticed() {
        // The monitor is told of a request to close that the kernel never
        // got, so the kernel stays as it is.
        let mut watched = Watched::new();
        let asked = Event::Control(ControlInput::Close {
            reason: CloseReason::Requested,
        });
        let quiet = Event::Timer(TimerId::StageAck);
        let now = MonoTime::from_millis(0);
        watched.kernel.step(now, &quiet, &mut watched.out);
        let view = watched.kernel.view();
        assert_eq!(
            watched.monitor.observe(now, &asked, &watched.out, view),
            Ok(())
        );
        assert_eq!(watched.at(6_999, quiet.clone()), Ok(()));
        assert_eq!(rule(watched.at(7_000, quiet)), Some(Rule::Closing));
    }

    #[test]
    fn a_kernel_without_its_lease_must_say_nothing() {
        let mut watched = Watched::new();
        assert_eq!(
            watched.step(Event::Control(ControlInput::LeaseLost)),
            Ok(())
        );
        assert_eq!(watched.step(Watched::words()), Ok(()));
        let checked = watched.tampered(Event::Timer(TimerId::StageAck), |out| {
            out.cmds.push(Command::SetTimer(TimerId::Grace, None));
        });
        assert_eq!(rule(checked), Some(Rule::Journal));
        assert_eq!(watched.monitor.at_rest(watched.kernel.view()), Ok(()));
    }

    impl Watched {
        /// Steps the kernel on `done` and shows the monitor the step as
        /// one on `shown`.
        fn shown_as(&mut self, millis: u64, done: &Event, shown: &Event) -> Result<(), Violation> {
            self.now_ms = millis;
            let now = MonoTime::from_millis(millis);
            self.kernel.step(now, done, &mut self.out);
            self.monitor
                .observe(now, shown, &self.out, self.kernel.view())
        }
    }

    const DROPPED: Event = Event::Control(ControlInput::Detached);
    const BACK: Event = Event::Control(ControlInput::Attached);
    const WAITED: Event = Event::Timer(TimerId::Detach);
    const QUIET: Event = Event::Timer(TimerId::StageAck);

    #[test]
    fn a_line_that_drops_and_returns_is_accepted() {
        let mut watched = Watched::new();
        watched.answering();
        // The answer is cut where it was heard, and given again when the
        // line is back.
        assert_eq!(watched.at(2_000, DROPPED), Ok(()));
        let waiting = watched.monitor.at_rest(watched.kernel.view());
        assert_eq!(waiting.map_err(|v| v.rule), Err(Rule::Line));
        assert_eq!(watched.at(4_000, BACK), Ok(()));
        let again = watched.out.cmds.iter().any(|command| {
            matches!(
                command,
                Command::Llm(_, waveport_domain::command::LlmCmd::Start { .. })
            )
        });
        assert!(again, "{:?}", watched.out.cmds);
    }

    #[test]
    fn a_line_that_stays_gone_is_given_up_when_it_has_been_waited_for() {
        let mut watched = Watched::new();
        assert_eq!(watched.at(1_000, DROPPED), Ok(()));
        assert_eq!(watched.at(30_999, WAITED), Ok(()));
        assert_eq!(watched.at(31_000, WAITED), Ok(()));
        assert_eq!(watched.monitor.at_rest(watched.kernel.view()), Ok(()));
    }

    #[test]
    fn a_line_given_up_before_its_time_is_noticed() {
        let mut watched = Watched::new();
        watched.at(1_000, DROPPED).unwrap();
        // The session closes as abandoned after five seconds of thirty,
        // and nobody asked it to.
        let reason = CloseReason::Abandoned;
        let close = Event::Control(ControlInput::Close { reason });
        let checked = watched.shown_as(6_000, &close, &WAITED);
        assert_eq!(rule(checked.clone()), Some(Rule::Line), "{checked:?}");
        let said = checked.unwrap_err().to_string();
        assert!(
            said.starts_with("invariant 11 is broken: the session began to close"),
            "{said}"
        );
    }

    #[test]
    fn a_session_that_closes_by_itself_with_its_line_there_is_noticed() {
        let mut watched = Watched::new();
        let reason = CloseReason::Abandoned;
        let close = Event::Control(ControlInput::Close { reason });
        let checked = watched.shown_as(6_000, &close, &QUIET);
        assert_eq!(rule(checked), Some(Rule::Line));
        // Asked to, it may, and for the reason it was given only.
        let mut watched = Watched::new();
        assert_eq!(watched.at(6_000, close), Ok(()));
        let mut watched = Watched::new();
        let asked = Event::Control(ControlInput::Close {
            reason: CloseReason::Requested,
        });
        let reason = CloseReason::Abandoned;
        let other = Event::Control(ControlInput::Close { reason });
        assert_eq!(
            rule(watched.shown_as(6_000, &other, &asked)),
            Some(Rule::Line)
        );
    }

    #[test]
    fn a_line_that_is_waited_for_beyond_its_time_is_noticed() {
        let mut watched = Watched::new();
        watched.at(1_000, DROPPED).unwrap();
        // The timer fires and the session goes on as if it had not.
        let checked = watched.shown_as(31_000, &QUIET, &WAITED);
        assert_eq!(rule(checked.clone()), Some(Rule::Line), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("it is still running"),
        );
        // And so when it is told again of a line it has waited for.
        let mut watched = Watched::new();
        watched.at(1_000, DROPPED).unwrap();
        let checked = watched.shown_as(31_000, &QUIET, &DROPPED);
        assert_eq!(rule(checked), Some(Rule::Line));
    }

    #[test]
    fn a_kernel_that_forgets_its_line_is_noticed() {
        // It takes the line for back when nobody said so.
        let mut watched = Watched::new();
        watched.at(1_000, DROPPED).unwrap();
        let checked = watched.shown_as(2_000, &BACK, &QUIET);
        assert_eq!(rule(checked.clone()), Some(Rule::Line), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("the kernel waits for its line until None"),
        );
        // Or it does not notice that the line dropped.
        let mut watched = Watched::new();
        let checked = watched.shown_as(1_000, &QUIET, &DROPPED);
        assert_eq!(rule(checked), Some(Rule::Line));
    }

    #[test]
    fn a_reply_begun_while_the_line_is_gone_is_noticed() {
        let mut watched = Watched::new();
        watched.asking();
        watched.step(DROPPED).unwrap();
        // The kernel takes the line for back, and answers the turn.
        let now = MonoTime::from_millis(watched.now_ms);
        watched.kernel.step(now, &BACK, &mut watched.out);
        let checked = watched.step(Watched::words());
        assert_eq!(rule(checked.clone()), Some(Rule::Line), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("while the line is gone"),
        );
    }

    #[test]
    fn a_reply_that_a_dropped_line_cut_and_nobody_gives_again_is_noticed() {
        let mut watched = Watched::new();
        watched.answering();
        // A kernel that cuts the answer as it does for an interruption,
        // which owes nobody anything, and then waits for its line. To the
        // monitor it is one step, on a line that dropped.
        watched.now_ms = 2_000;
        let now = MonoTime::from_millis(2_000);
        let interrupt = Event::Control(ControlInput::Interrupt);
        watched.kernel.step(now, &interrupt, &mut watched.out);
        let mut waits = Out::new();
        watched.kernel.step(now, &DROPPED, &mut waits);
        assert!(waits.facts.is_empty(), "{:?}", waits.facts);
        watched.out.cmds.extend(waits.cmds.iter().copied());
        let view = watched.kernel.view();
        let cut = watched.monitor.observe(now, &DROPPED, &watched.out, view);
        assert_eq!(cut, Ok(()));
        // The line is back, and the caller is not answered.
        assert_eq!(watched.at(4_000, BACK), Ok(()));
        assert_eq!(watched.out.cmds.len(), 1, "{:?}", watched.out.cmds);
        let stuck = watched.monitor.at_rest(watched.kernel.view()).unwrap_err();
        assert_eq!(stuck.rule, Rule::Progress, "{stuck}");
        assert!(
            stuck.to_string().contains("the session is stuck"),
            "{stuck}"
        );
    }

    fn key(written: char) -> Event {
        Event::Key {
            who: ANA,
            key: Key::from_char(written).unwrap(),
        }
    }

    #[test]
    fn keys_that_are_pressed_and_answered_are_accepted() {
        let mut watched = Watched::new();
        for (at, written) in [(1_000, '4'), (1_250, '0'), (1_500, '7'), (1_750, '1')] {
            assert_eq!(watched.at(at, key(written)), Ok(()));
        }
        // Keys that could go on are a turn that has not ended.
        let open = watched.monitor.at_rest(watched.kernel.view());
        assert_eq!(open.map_err(|v| v.rule), Err(Rule::Turns));
        assert_eq!(watched.at(2_000, key('#')), Ok(()));
        let committed = watched.out.facts.iter().any(
            |stamped| matches!(stamped.fact, Fact::KeysCommitted { turn, .. } if turn == TurnId(1)),
        );
        assert!(committed, "{:?}", watched.out.facts);
        // And so are keys that only time ends, and the one key that is a
        // run by itself.
        assert_eq!(watched.at(9_000, key('5')), Ok(()));
        let waited = Event::Timer(TimerId::Keys(TurnId(2)));
        assert_eq!(watched.at(11_999, waited.clone()), Ok(()));
        assert_eq!(watched.at(12_000, waited), Ok(()));
        assert_eq!(watched.at(13_000, key('#')), Ok(()));
        assert_eq!(watched.monitor.context, 3);
    }

    #[test]
    fn keys_that_are_not_the_keys_that_were_pressed_are_noticed() {
        let mut watched = Watched::new();
        watched.at(1_000, key('4')).unwrap();
        watched.at(1_250, key('0')).unwrap();
        let other = |out: &mut Out| {
            let keys = out.text.push("41");
            for stamped in &mut out.facts {
                if let Fact::KeysCommitted { keys: pressed, .. } = &mut stamped.fact {
                    *pressed = keys;
                }
            }
        };
        watched.now_ms = 1_500;
        let checked = watched.tampered(key('#'), other);
        assert_eq!(rule(checked.clone()), Some(Rule::Turns), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .starts_with("invariant 2 is broken: turn 1 was committed as 1 pressing \"41\"")
        );
    }

    #[test]
    fn keys_that_become_a_turn_while_another_could_follow_are_noticed() {
        let mut watched = Watched::new();
        watched.at(1_000, key('4')).unwrap();
        // The kernel ends the run on a key that does not end one.
        let checked = watched.shown_as(1_250, &key('#'), &key('0'));
        assert_eq!(rule(checked), Some(Rule::Turns));
        // Or before whoever presses has had their time.
        let mut watched = Watched::new();
        watched.at(1_000, key('4')).unwrap();
        let waited = Event::Timer(TimerId::Keys(TurnId(1)));
        let checked = watched.shown_as(2_000, &key('#'), &waited);
        assert_eq!(rule(checked.clone()), Some(Rule::Turns), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("while another could follow")
        );
    }

    #[test]
    fn keys_that_are_over_and_are_no_turn_are_noticed() {
        // The key that ends the run does not.
        let mut watched = Watched::new();
        watched.at(1_000, key('4')).unwrap();
        let checked = watched.shown_as(1_250, &QUIET, &key('#'));
        assert_eq!(rule(checked.clone()), Some(Rule::Turns), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("they have not entered the conversation")
        );
        // Nor does the time in which no key followed.
        let mut watched = Watched::new();
        watched.at(1_000, key('4')).unwrap();
        let waited = Event::Timer(TimerId::Keys(TurnId(1)));
        let checked = watched.shown_as(4_000, &QUIET, &waited);
        assert_eq!(rule(checked), Some(Rule::Turns));
    }

    fn speech(edge: VadEdge, at_ms: u64) -> Event {
        Event::Speech {
            who: ANA,
            edge,
            at: MediaTime::from_millis(at_ms),
        }
    }

    #[test]
    fn keys_that_end_with_what_was_said_after_them_are_accepted() {
        let mut config = KernelConfig::default();
        config.timing.turn_max = Duration::from_secs(6);
        let mut watched = Watched::with(config);
        // Ana presses as she speaks, for as long as a turn may last.
        watched.at(1_000, key('4')).unwrap();
        watched.at(1_200, speech(VadEdge::Start, 1_200)).unwrap();
        watched.at(3_500, key('0')).unwrap();
        watched.at(6_000, key('7')).unwrap();
        watched.at(7_000, speech(VadEdge::End, 7_000)).unwrap();
        let ran_out = Event::Timer(TimerId::TurnMax(TurnId(2)));
        watched.at(7_200, ran_out).unwrap();
        let said = Event::Transcript {
            who: ANA,
            span: MediaSpan::new(MediaTime::from_millis(1_200), MediaTime::from_millis(7_000)),
            text: "the number is".into(),
            is_final: true,
        };
        watched.at(7_300, said).unwrap();
        assert_eq!(watched.monitor.context, 0, "her words wait for her keys");
        // She speaks anew: what she said has to end, and her keys with it.
        assert_eq!(watched.at(7_500, speech(VadEdge::Start, 7_500)), Ok(()));
        assert_eq!(watched.monitor.context, 2);
    }

    #[test]
    fn keys_that_end_for_speech_that_does_not_are_noticed() {
        let mut watched = Watched::new();
        watched.at(1_000, key('4')).unwrap();
        watched.at(1_200, speech(VadEdge::Start, 1_200)).unwrap();
        watched.at(1_800, speech(VadEdge::End, 1_800)).unwrap();
        // The keys become a turn as Ana speaks anew, and what she said
        // after them stays where it was.
        let waited = Event::Timer(TimerId::Keys(TurnId(1)));
        let anew = speech(VadEdge::Start, 4_000);
        let checked = watched.shown_as(4_000, &waited, &anew);
        assert_eq!(rule(checked.clone()), Some(Rule::Turns), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("which was said after them, did not end")
        );
    }

    #[test]
    fn a_key_that_begins_no_turn_is_noticed() {
        let mut watched = Watched::new();
        let checked = watched.shown_as(1_000, &QUIET, &key('4'));
        assert_eq!(rule(checked.clone()), Some(Rule::Turns), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("no turn began for it")
        );
        // A key of somebody who is not there begins none, and need not.
        let mut watched = Watched::new();
        let stranger = Event::Key {
            who: ParticipantId(7),
            key: Key::Four,
        };
        assert_eq!(watched.at(1_000, stranger), Ok(()));
    }

    #[test]
    fn a_key_that_does_not_stop_the_agent_is_noticed() {
        let mut watched = Watched::new();
        watched.answering();
        assert_eq!(watched.step(key('4')), Ok(()));

        let mut watched = Watched::new();
        watched.answering();
        // The agent goes on speaking over the key.
        let now = MonoTime::from_millis(watched.now_ms);
        let pressed = key('4');
        watched.kernel.step(now, &pressed, &mut watched.out);
        watched.out.facts.clear();
        watched
            .out
            .cmds
            .retain(|command| matches!(command, Command::SetTimer(TimerId::Keys(_), _)));
        let view = watched.kernel.view();
        let checked = watched.monitor.observe(now, &pressed, &watched.out, view);
        assert_eq!(rule(checked.clone()), Some(Rule::Turns), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("goes on being heard")
        );
    }

    #[test]
    fn keys_that_enter_the_conversation_before_what_was_said_before_them_are_noticed() {
        let mut watched = Watched::new();
        watched.asking();
        // Ana presses before her words are in, and the keys are over
        // first.
        assert_eq!(watched.step(key('4')), Ok(()));
        assert_eq!(watched.step(key('#')), Ok(()));
        // Honestly, the words go first.
        let mut honest = Watched::new();
        honest.asking();
        honest.step(key('4')).unwrap();
        honest.step(key('#')).unwrap();
        assert_eq!(honest.step(Watched::words()), Ok(()));

        let swapped = |out: &mut Out| {
            assert_eq!(out.facts.len(), 2, "{:?}", out.facts);
            let (first, second) = (out.facts[0], out.facts[1]);
            out.facts[0] = Stamped {
                seq: first.seq,
                fact: second.fact,
            };
            out.facts[1] = Stamped {
                seq: second.seq,
                fact: first.fact,
            };
        };
        let checked = watched.tampered(Watched::words(), swapped);
        assert_eq!(rule(checked.clone()), Some(Rule::Turns), "{checked:?}");
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("which began before it")
        );
    }

    #[test]
    fn a_turn_left_open_is_noticed_at_rest() {
        let mut watched = Watched::new();
        watched.asking();
        let stuck = watched.monitor.at_rest(watched.kernel.view()).unwrap_err();
        assert_eq!(stuck.rule, Rule::Turns);
        assert_eq!(
            stuck.to_string(),
            "invariant 2 is broken: the turns [1] never ended"
        );
        assert_eq!(Rule::Progress.number(), None);
    }
}
