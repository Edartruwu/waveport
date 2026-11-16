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
//! 1 to 8, the numbering of facts from 10, and progress. Invariant 9, that
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
use waveport_domain::event::{ControlInput, Event, OpEvent, Part};
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
    /// with exactly their words or dismissed for a reason that holds.
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
            desk,
        } = self;
        (leased, last_seq, began, entered, epoch, lifecycle, close_by).hash(state);
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
            self.fact(event, stamped, text, whole)?;
        }
        for command in &out.cmds {
            let text = command.text().map_or("", |text| out.text.get(text));
            self.command(event, command, text, whole)?;
        }
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
        let Some(heard) = self.turns.iter_mut().find(|heard| heard.who == who) else {
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
            Fact::LifecycleChanged { to, .. } => {
                let next = match self.lifecycle {
                    Lifecycle::Running => Some(Lifecycle::Draining),
                    Lifecycle::Draining => Some(Lifecycle::Closing),
                    Lifecycle::Closing => Some(Lifecycle::Closed),
                    Lifecycle::Closed => None,
                };
                ensure(next == Some(to), Rule::Closing, || {
                    format!("the session went from {:?} to {to:?}", self.lifecycle)
                })?;
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
                    ensure(heard.who == who && heard.words == text, Rule::Turns, || {
                        format!(
                            "turn {turn} was committed as {who} saying {text:?}; what arrived \
                             for it is {} saying {:?}",
                            heard.who, heard.words
                        )
                    })?;
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
            Fact::TurnDismissed { turn, who, reason } => {
                let Some(index) = self.turns.iter().position(|heard| heard.turn == turn) else {
                    return Err(Violation {
                        rule: Rule::Turns,
                        what: format!("turn {turn} was dismissed, and it is not open"),
                    });
                };
                let heard = self.turns.remove(index);
                let holds = heard.who == who
                    && match reason {
                        Dismissal::Empty => heard.words.is_empty(),
                        Dismissal::Backchannel | Dismissal::Noise => heard.over_agent,
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

        let alone = self.turns.iter().enumerate().all(|(index, heard)| {
            self.turns
                .iter()
                .skip(index + 1)
                .all(|other| other.who != heard.who)
        });
        ensure(alone, Rule::Turns, || {
            let open = self
                .turns
                .iter()
                .map(|heard| (heard.turn.get(), heard.who))
                .collect::<Vec<_>>();
            format!("someone has two turns open at once: {open:?}")
        })?;

        self.desk_settled(view)?;

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

#[cfg(test)]
mod tests {
    use waveport_domain::command::{Command, PlayoutCmd};
    use waveport_domain::event::{
        CloseReason, ControlInput, Event, Finish, OpEvent, TimerId, VadEdge,
    };
    use waveport_domain::fact::{Fact, Stamped};
    use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId, Prob, Seq, TurnId};
    use waveport_domain::kernel::Kernel;
    use waveport_domain::out::Out;
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
            let kernel = Kernel::default();
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
