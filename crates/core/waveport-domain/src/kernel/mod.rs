//! The session kernel: one conversation's state and the rules that change
//! it.
//!
//! A kernel is driven by exactly one caller, which feeds it one event at a
//! time:
//!
//! ```
//! use waveport_domain::event::{ControlInput, Event};
//! use waveport_domain::ids::ParticipantId;
//! use waveport_domain::kernel::Kernel;
//! use waveport_domain::out::Out;
//! use waveport_domain::time::MonoTime;
//!
//! let mut kernel: Kernel = Kernel::default();
//! let mut out = Out::new();
//!
//! let joined = Event::Control(ControlInput::Join { who: ParticipantId(1) });
//! kernel.step(MonoTime::from_millis(0), &joined, &mut out);
//!
//! // The caller journals `out.facts`, then carries out `out.cmds`.
//! for command in &out.cmds {
//!     let _ = command;
//! }
//! ```

mod conversation;
mod coordinator;
mod fingerprint;
mod floor;
mod lifecycle;
mod response;
mod tools;
mod view;

use core::fmt;

pub use view::{
    BatchView, CallState, CallView, GenerationRole, GenerationView, ItemView, TurnView, View,
};

use alloc::string::String;
use alloc::vec::Vec;

use self::conversation::Conversation;
use self::coordinator::Step;
use self::floor::OpenTurn;
use self::lifecycle::Closing;
use self::response::Generation;
use self::tools::{Batch, Call};
use crate::bounded::Bounded;
use crate::command::LlmCause;
use crate::config::{AgentSpec, ConfigError, KernelConfig, ToolSpec};
use crate::event::{CloseReason, ControlInput, Event};
use crate::fact::{Fact, Lifecycle, Stamped, ToolOutcome};
use crate::ids::{
    AgentId, BatchId, ContextVersion, Epoch, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
};
use crate::out::Out;
use crate::policy::{DefaultPolicies, Policies};
use crate::text::TextArena;
use crate::time::MonoTime;

/// Everything about a session that a step may change. It is plain data:
/// two kernels with equal state behave the same from then on.
///
/// The first part outlives a crash. It changes only in [`State::apply`],
/// by a fact that is journaled, so the journal can rebuild it, and it is
/// what a [`Snapshot`] holds. The second part, the turns and responses
/// under way and everything that is a matter of time, is lost with the
/// process, and the session carries on without it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct State {
    // ---- Journaled -----------------------------------------------------
    epoch: Epoch,
    lifecycle: Lifecycle,
    /// Why the session is closing, once it is.
    closed_for: Option<CloseReason>,
    /// The agent that holds the floor.
    front: AgentId,
    /// The agent that was asked to take the floor and has not yet.
    handoff: Option<AgentId>,
    /// The sequence number the next fact gets.
    next_seq: Seq,
    /// The lowest turn number no fact has named.
    turn_floor: TurnId,
    /// The lowest response number no fact has named.
    gen_floor: GenId,
    next_call: ToolCallId,
    next_batch: BatchId,
    participants: Bounded<ParticipantId>,
    /// The tool calls that have not ended. What a call is waiting for is
    /// not journaled.
    calls: Bounded<Call>,
    /// The batches that are not resolved. Whether the response that asked
    /// has ended is not journaled.
    batches: Bounded<Batch>,
    conversation: Conversation,
    /// The newest item of the conversation that is owed a response, as the
    /// version of the conversation that holds it, and what the response is
    /// for.
    owed: Option<(ContextVersion, LlmCause)>,
    /// The conversation that has had its response: an agent answered it
    /// and was heard to the end.
    answered: ContextVersion,
    /// The response whose speech is being heard, as far as that has been
    /// recorded, and what was recorded as heard of it.
    speech: Option<(GenId, AgentId)>,
    speech_text: String,

    // ---- Not journaled -------------------------------------------------
    closing: Option<Closing>,
    attached: bool,
    /// Whether this kernel still owns the session.
    leased: bool,
    /// Whether the session was rebuilt from its journal and has not yet
    /// dealt with what was lost.
    recovering: bool,
    /// The number the next turn gets. Never below `turn_floor`.
    next_turn: TurnId,
    /// The number the next response gets. Never below `gen_floor`.
    next_gen: GenId,
    /// Every fact up to this one is stored durably.
    durable: Seq,
    /// The conversation the latest response was started on. A response is
    /// owed while `owed` is newer than this and than `answered`.
    started: ContextVersion,
    /// When an agent that was asked to take the floor has had its time to
    /// become ready.
    stage_until: Option<MonoTime>,
    turns: Bounded<OpenTurn>,
    generations: Bounded<Generation>,
}

impl State {
    fn new(config: &KernelConfig) -> Self {
        let limits = &config.limits;
        Self {
            epoch: Epoch(0),
            lifecycle: Lifecycle::Running,
            closed_for: None,
            front: config.front,
            handoff: None,
            next_seq: Seq(1),
            turn_floor: TurnId(1),
            gen_floor: GenId(1),
            next_call: ToolCallId(1),
            next_batch: BatchId(1),
            participants: Bounded::new(limits.participants),
            calls: Bounded::new(limits.open_calls),
            // A batch that is not resolved has a call that has not ended,
            // or a response that may still ask for one.
            batches: Bounded::new(limits.open_calls.saturating_add(limits.generations)),
            conversation: Conversation::default(),
            owed: None,
            answered: ContextVersion(0),
            speech: None,
            speech_text: String::with_capacity(limits.response_text),
            closing: None,
            attached: true,
            leased: true,
            recovering: false,
            next_turn: TurnId(1),
            next_gen: GenId(1),
            durable: Seq(0),
            started: ContextVersion(0),
            stage_until: None,
            turns: Bounded::new(limits.open_turns),
            generations: Bounded::new(limits.generations),
        }
    }

    /// Makes a fact true. `text` is the arena the fact's text refers to.
    ///
    /// This only sets values. Every decision was taken before the fact was
    /// emitted, so applying the same facts in the same order gives the
    /// same state, in this build or a later one.
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per fact, and each only sets values"
    )]
    fn apply(&mut self, fact: &Fact, text: &TextArena) {
        match *fact {
            Fact::ParticipantJoined { who } => {
                if !self.participants.contains(&who) {
                    // A full table refuses the participant, as it did when
                    // the fact was emitted.
                    let _ = self.participants.push(who);
                }
            }
            Fact::ParticipantLeft { who } => self.participants.retain(|known| *known != who),
            Fact::LifecycleChanged { to, reason } => {
                self.lifecycle = to;
                self.closed_for = reason.or(self.closed_for);
            }
            Fact::EpochAdvanced { to, .. } => self.epoch = to,
            Fact::TurnCommitted {
                turn,
                who,
                text: words,
                addressed,
                ..
            } => {
                self.conversation
                    .push_user_turn(turn, who, text.get(words), addressed);
                if addressed {
                    self.owed = Some((self.conversation.version(), LlmCause::Reply { turn }));
                }
                self.saw_turn(turn);
            }
            Fact::TurnDismissed { turn, .. } => self.saw_turn(turn),
            Fact::NoticeAppended { kind, text: note } => {
                self.conversation.push_notice(kind, text.get(note));
            }
            Fact::UtteranceProgress {
                generation,
                agent,
                heard,
            } => {
                if self.speech != Some((generation, agent)) {
                    self.speech = Some((generation, agent));
                    self.speech_text.clear();
                }
                self.speech_text.push_str(text.get(heard));
                self.saw_generation(generation);
            }
            Fact::UtteranceCommitted {
                generation,
                agent,
                answers,
                text: words,
                truncated,
            } => {
                self.conversation
                    .push_utterance(generation, agent, text.get(words), truncated);
                if let Some(answers) = answers {
                    self.answered = self.answered.max(answers);
                }
                if self.speech.is_some_and(|(of, _)| of == generation) {
                    self.speech = None;
                    self.speech_text.clear();
                }
                self.saw_generation(generation);
            }
            Fact::ToolProposed {
                call,
                batch,
                generation,
                tool,
                args,
            } => {
                self.conversation
                    .push_tool_call(call, tool, generation, text.get(args));
                // The tables had room when the fact was emitted.
                let _ = self
                    .calls
                    .push(Call::proposed(call, tool, batch, generation));
                match self.batch_mut(batch) {
                    Some(open) => open.pending = open.pending.saturating_add(1),
                    None => {
                        let _ = self.batches.push(Batch::begin(batch, generation));
                    }
                }
                self.next_call = self.next_call.max(call.next());
                self.next_batch = self.next_batch.max(batch.next());
                self.saw_generation(generation);
            }
            Fact::ToolAuthorized { call } => {
                if let Some(open) = self.call_mut(call) {
                    open.state = CallState::Authorized;
                }
            }
            Fact::ToolDispatched { call, attempt } => {
                let Some(open) = self.call_mut(call) else {
                    return;
                };
                open.state = CallState::Dispatched;
                open.attempt = attempt;
                let batch = open.batch;
                if let Some(open) = self.batch_mut(batch) {
                    open.ran = true;
                }
            }
            Fact::ToolFinished {
                call,
                outcome,
                result,
            } => {
                self.conversation
                    .push_tool_result(call, outcome, text.get(result));
                let ended = self
                    .calls
                    .iter()
                    .position(|open| open.id == call)
                    .and_then(|index| self.calls.remove(index));
                if let Some(open) = ended.and_then(|ended| self.batch_mut(ended.batch)) {
                    open.pending = open.pending.saturating_sub(1);
                    open.ran |= outcome != ToolOutcome::Cancelled;
                }
            }
            Fact::BatchResolved { batch } => {
                let resolved = self
                    .batches
                    .iter()
                    .position(|open| open.id == batch)
                    .and_then(|index| self.batches.remove(index));
                if resolved.is_some_and(|resolved| resolved.ran) {
                    let cause = LlmCause::Continuation { batch };
                    self.owed = Some((self.conversation.version(), cause));
                }
            }
            Fact::HandoffRequested { to } => self.handoff = Some(to),
            Fact::AgentActivated { agent } => {
                self.front = agent;
                self.handoff = None;
            }
        }
    }

    fn saw_turn(&mut self, turn: TurnId) {
        self.turn_floor = self.turn_floor.max(turn.next());
        self.next_turn = self.next_turn.max(self.turn_floor);
    }

    fn saw_generation(&mut self, generation: GenId) {
        self.gen_floor = self.gen_floor.max(generation.next());
        self.next_gen = self.next_gen.max(self.gen_floor);
    }

    fn call_mut(&mut self, call: ToolCallId) -> Option<&mut Call> {
        self.calls.iter_mut().find(|open| open.id == call)
    }

    fn batch_mut(&mut self, batch: BatchId) -> Option<&mut Batch> {
        self.batches.iter_mut().find(|open| open.id == batch)
    }

    /// Whether a response is owed and none has been started for it.
    fn reply_owed(&self) -> bool {
        self.owed
            .is_some_and(|(needs, _)| needs > self.answered.max(self.started))
    }

    /// The journaled part of the state.
    fn journaled(&self) -> Snapshot {
        Snapshot {
            epoch: self.epoch,
            lifecycle: self.lifecycle,
            closed_for: self.closed_for,
            front: self.front,
            handoff: self.handoff,
            next_seq: self.next_seq,
            turn_floor: self.turn_floor,
            gen_floor: self.gen_floor,
            next_call: self.next_call,
            next_batch: self.next_batch,
            participants: self.participants.to_vec(),
            calls: self.calls.iter().map(Call::journaled).collect(),
            batches: self.batches.iter().map(Batch::journaled).collect(),
            conversation: self.conversation.clone(),
            owed: self.owed,
            answered: self.answered,
            speech: self.speech,
            speech_text: self.speech_text.clone(),
        }
    }

    /// The state of a session that is rebuilt from `snapshot`: what was
    /// journaled, and nothing under way. The error names the limit of
    /// `config` that is too small for what the snapshot holds.
    fn revive(config: &KernelConfig, snapshot: &Snapshot) -> Result<Self, &'static str> {
        let mut state = Self::new(config);
        state.epoch = snapshot.epoch;
        state.lifecycle = snapshot.lifecycle;
        state.closed_for = snapshot.closed_for;
        state.front = snapshot.front;
        state.handoff = snapshot.handoff;
        state.next_seq = snapshot.next_seq;
        state.turn_floor = snapshot.turn_floor;
        state.gen_floor = snapshot.gen_floor;
        state.next_call = snapshot.next_call;
        state.next_batch = snapshot.next_batch;
        for &who in &snapshot.participants {
            state.participants.push(who).map_err(|_| "participants")?;
        }
        for &call in &snapshot.calls {
            state.calls.push(call).map_err(|_| "open_calls")?;
        }
        for &batch in &snapshot.batches {
            state.batches.push(batch).map_err(|_| "open_calls")?;
        }
        state.conversation = snapshot.conversation.clone();
        state.owed = snapshot.owed;
        state.answered = snapshot.answered;
        state.speech = snapshot.speech;
        state.speech_text.push_str(&snapshot.speech_text);
        Ok(state)
    }
}

/// One session's kernel.
///
/// The kernel performs no I/O, reads no clock and never waits. Everything
/// it learns arrives through [`step`](Self::step), and everything it wants
/// done leaves through the [`Out`] passed to it.
///
/// A clone is a second session in the same state, which goes its own way
/// from there: what a test needs to try two futures of one session.
#[derive(Clone)]
pub struct Kernel<P: Policies = DefaultPolicies> {
    config: KernelConfig,
    policies: P,
    state: State,
}

impl<P: Policies + Default> Default for Kernel<P> {
    /// A kernel with the default configuration and policies: one agent, no
    /// tools.
    fn default() -> Self {
        let config = KernelConfig::default();
        let state = State::new(&config);
        Self {
            config,
            policies: P::default(),
            state,
        }
    }
}

impl<P: Policies> Kernel<P> {
    /// A kernel for a new session.
    ///
    /// # Errors
    ///
    /// The configuration does not describe a session that can run.
    pub fn new(config: KernelConfig, policies: P) -> Result<Self, ConfigError> {
        config.validate()?;
        let state = State::new(&config);
        Ok(Self {
            config,
            policies,
            state,
        })
    }

    /// Takes one event at time `now` and leaves in `out` what follows from
    /// it: the commands to carry out and the facts to journal.
    ///
    /// `out` is cleared first. `now` must not go backwards from one step to
    /// the next. The same events at the same times always produce the same
    /// output.
    ///
    /// The first step of a session that was [restored](Self::restore) also
    /// deals with what the crash lost, before it takes its event.
    pub fn step(&mut self, now: MonoTime, event: &Event, out: &mut Out) {
        out.clear();
        if matches!(event, Event::Control(ControlInput::LeaseLost)) {
            // Another owner has the session. From here on this kernel
            // neither says nor records anything.
            self.state.leased = false;
        }
        if !self.state.leased || self.state.lifecycle == Lifecycle::Closed {
            return;
        }
        let mut step = Step {
            now,
            config: &self.config,
            policies: &self.policies,
            state: &mut self.state,
            out,
        };
        if step.state.recovering {
            step.recover();
        }
        step.run(event);
    }

    /// A read-only look at the session.
    #[must_use]
    pub fn view(&self) -> View<'_> {
        View::new(&self.state, &self.config)
    }

    /// The configuration the session was started with.
    #[must_use]
    pub const fn config(&self) -> &KernelConfig {
        &self.config
    }

    /// The policies the session runs with.
    #[must_use]
    pub const fn policies(&self) -> &P {
        &self.policies
    }

    /// What the journal knows of the session, to be stored. Together with
    /// the facts emitted after it, a snapshot rebuilds the session.
    ///
    /// A snapshot holds what the facts so far add up to and nothing else:
    /// a snapshot taken at any earlier time, with the facts emitted since
    /// applied to it, is equal to this one.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.state.journaled()
    }

    /// Rebuilds a session from a snapshot and the facts journaled after it.
    /// `text` is the arena the facts' text refers to.
    ///
    /// What was under way when the snapshot's session stopped is not in
    /// the journal. The first [`step`](Self::step) of the rebuilt session
    /// deals with that: it advances the epoch, records the speech that was
    /// being heard as cut off where it was last known to be, fails the
    /// tool calls that had not been sent, sends again those that may be
    /// repeated, marks the others as in doubt, and leaves a note for the
    /// language model.
    ///
    /// # Errors
    ///
    /// The configuration is unusable or too small for the snapshot, or
    /// `tail` does not continue the snapshot without a gap.
    pub fn restore(
        config: KernelConfig,
        policies: P,
        snapshot: &Snapshot,
        tail: &[Stamped],
        text: &TextArena,
    ) -> Result<Self, RestoreError> {
        config.validate().map_err(RestoreError::Config)?;
        let mut state = State::revive(&config, snapshot).map_err(RestoreError::TooSmall)?;
        for stamped in tail {
            if stamped.seq != state.next_seq {
                return Err(RestoreError::Gap {
                    expected: state.next_seq,
                    found: stamped.seq,
                });
            }
            state.next_seq = state.next_seq.next();
            state.apply(&stamped.fact, text);
        }
        // Everything read back was stored.
        state.durable = Seq(state.next_seq.get().saturating_sub(1));
        // Turns and responses that were under way had numbers of their
        // own, which no fact names. The new ones start above them.
        let limits = &config.limits;
        let skip = |limit: usize| u32::try_from(limit).unwrap_or(u32::MAX);
        state.next_turn = TurnId(
            state
                .turn_floor
                .get()
                .saturating_add(skip(limits.open_turns)),
        );
        state.next_gen = GenId(
            state
                .gen_floor
                .get()
                .saturating_add(skip(limits.generations)),
        );
        state.started = state.answered;
        state.recovering = true;
        Ok(Self {
            config,
            policies,
            state,
        })
    }

    /// Gives back memory the session is not using. For a session that is
    /// going idle; the next step reserves again.
    pub fn park(&mut self) {
        let state = &mut self.state;
        state.participants.park();
        state.turns.park();
        state.generations.park();
        state.calls.park();
        state.batches.park();
        state.conversation.park();
        state.speech_text.shrink_to_fit();
    }

    /// Bytes of heap the kernel holds: its state and its configuration.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        let config = &self.config;
        let state = &self.state;
        let texts = state
            .turns
            .iter()
            .map(|open| open.text.capacity())
            .chain(
                state
                    .generations
                    .iter()
                    .map(|running| running.planned.capacity()),
            )
            .chain(
                config
                    .agents
                    .iter()
                    .map(|agent| agent.tools.capacity().saturating_mul(size_of::<ToolId>())),
            );
        let tables = [
            state.participants.heap_bytes(),
            state.turns.heap_bytes(),
            state.generations.heap_bytes(),
            state.calls.heap_bytes(),
            state.batches.heap_bytes(),
            state.conversation.heap_bytes(),
            state.speech_text.capacity(),
            config
                .agents
                .capacity()
                .saturating_mul(size_of::<AgentSpec>()),
            config
                .tools
                .capacity()
                .saturating_mul(size_of::<ToolSpec>()),
        ];
        texts.chain(tables).fold(0, usize::saturating_add)
    }

    /// A hash of the session's state. Equal states have equal fingerprints
    /// on every platform, so two runs can be compared step by step.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        fingerprint::of(&self.state)
    }
}

impl<P: Policies> fmt::Debug for Kernel<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kernel")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// What the journal knows of a session at one point: everything its facts
/// up to there add up to, and nothing that was only under way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    epoch: Epoch,
    lifecycle: Lifecycle,
    closed_for: Option<CloseReason>,
    front: AgentId,
    handoff: Option<AgentId>,
    next_seq: Seq,
    turn_floor: TurnId,
    gen_floor: GenId,
    next_call: ToolCallId,
    next_batch: BatchId,
    participants: Vec<ParticipantId>,
    calls: Vec<Call>,
    batches: Vec<Batch>,
    conversation: Conversation,
    owed: Option<(ContextVersion, LlmCause)>,
    answered: ContextVersion,
    speech: Option<(GenId, AgentId)>,
    speech_text: String,
}

impl Snapshot {
    /// The sequence number of the first fact that is not in the snapshot.
    #[must_use]
    pub const fn next_seq(&self) -> Seq {
        self.next_seq
    }

    /// Everything the snapshot holds, in plain sight: what it is written
    /// to a journal from. The text is borrowed from the snapshot.
    #[must_use]
    pub fn parts(&self) -> SnapshotParts<'_> {
        SnapshotParts {
            epoch: self.epoch,
            lifecycle: self.lifecycle,
            closed_for: self.closed_for,
            front: self.front,
            handoff: self.handoff,
            next_seq: self.next_seq,
            turn_floor: self.turn_floor,
            gen_floor: self.gen_floor,
            next_call: self.next_call,
            next_batch: self.next_batch,
            participants: self.participants.clone(),
            calls: self
                .calls
                .iter()
                .map(|open| CallView {
                    call: open.id,
                    tool: open.tool,
                    batch: open.batch,
                    by: open.by,
                    state: open.state,
                    attempt: open.attempt,
                })
                .collect(),
            batches: self
                .batches
                .iter()
                .map(|open| BatchView {
                    batch: open.id,
                    by: open.by,
                    pending: usize::from(open.pending),
                    ran: open.ran,
                })
                .collect(),
            items: self.conversation.iter().collect(),
            owed: self.owed,
            answered: self.answered,
            speech: self.speech.map(|(generation, agent)| SpeechHeard {
                generation,
                agent,
                text: &self.speech_text,
            }),
        }
    }

    /// The snapshot that holds what `parts` says: what a snapshot read
    /// back from a journal is made of.
    ///
    /// Nothing is checked here. A snapshot is believed as a fact is: what
    /// it says was decided before it was written. Whether the session's
    /// configuration has room for it is found out by
    /// [`Kernel::restore`].
    #[must_use]
    pub fn from_parts(parts: &SnapshotParts<'_>) -> Self {
        let mut conversation = Conversation::default();
        for item in &parts.items {
            match *item {
                ItemView::UserTurn {
                    turn,
                    who,
                    text,
                    addressed,
                } => conversation.push_user_turn(turn, who, text, addressed),
                ItemView::Utterance {
                    generation,
                    agent,
                    text,
                    truncated,
                } => conversation.push_utterance(generation, agent, text, truncated),
                ItemView::ToolCall {
                    call,
                    tool,
                    generation,
                    args,
                } => conversation.push_tool_call(call, tool, generation, args),
                ItemView::ToolResult {
                    call,
                    outcome,
                    text,
                } => conversation.push_tool_result(call, outcome, text),
                ItemView::Notice { kind, text } => conversation.push_notice(kind, text),
            }
        }
        let call = |open: &CallView| {
            let mut call = Call::proposed(open.call, open.tool, open.batch, open.by);
            call.state = open.state;
            call.attempt = open.attempt;
            // A journal knows nothing of a call that is being stopped.
            call.journaled()
        };
        let batch = |open: &BatchView| {
            let mut batch = Batch::begin(open.batch, open.by);
            batch.pending = u16::try_from(open.pending).unwrap_or(u16::MAX);
            batch.ran = open.ran;
            batch
        };
        Self {
            epoch: parts.epoch,
            lifecycle: parts.lifecycle,
            closed_for: parts.closed_for,
            front: parts.front,
            handoff: parts.handoff,
            next_seq: parts.next_seq,
            turn_floor: parts.turn_floor,
            gen_floor: parts.gen_floor,
            next_call: parts.next_call,
            next_batch: parts.next_batch,
            participants: parts.participants.clone(),
            calls: parts.calls.iter().map(call).collect(),
            batches: parts.batches.iter().map(batch).collect(),
            conversation,
            owed: parts.owed,
            answered: parts.answered,
            speech: parts.speech.map(|heard| (heard.generation, heard.agent)),
            speech_text: parts
                .speech
                .map_or_else(String::new, |heard| String::from(heard.text)),
        }
    }
}

/// A [`Snapshot`] taken apart: every value it holds.
///
/// A snapshot keeps its values to itself, so that only facts can change
/// what a session's journal adds up to. This is the one way round that:
/// for whoever stores a snapshot, and has to write every value of it
/// somewhere and read it back. Whoever builds one of these by hand
/// decides what a session remembers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotParts<'a> {
    /// The epoch.
    pub epoch: Epoch,
    /// The stage of the session's life.
    pub lifecycle: Lifecycle,
    /// Why the session is closing, once it is.
    pub closed_for: Option<CloseReason>,
    /// The agent that holds the floor.
    pub front: AgentId,
    /// The agent that was asked to take the floor and has not yet.
    pub handoff: Option<AgentId>,
    /// The sequence number of the first fact that is not in the snapshot.
    pub next_seq: Seq,
    /// The lowest turn number no fact has named.
    pub turn_floor: TurnId,
    /// The lowest response number no fact has named.
    pub gen_floor: GenId,
    /// The number the next tool call gets.
    pub next_call: ToolCallId,
    /// The number the next batch of tool calls gets.
    pub next_batch: BatchId,
    /// The humans in the session.
    pub participants: Vec<ParticipantId>,
    /// The tool calls that have not ended.
    pub calls: Vec<CallView>,
    /// The batches of tool calls that are not resolved.
    pub batches: Vec<BatchView>,
    /// The conversation, oldest item first.
    pub items: Vec<ItemView<'a>>,
    /// The newest item of the conversation that is owed a response, as
    /// the version of the conversation that holds it, and what the
    /// response is for.
    pub owed: Option<(ContextVersion, LlmCause)>,
    /// The conversation that has had its response.
    pub answered: ContextVersion,
    /// The speech that was being heard, as far as that was recorded.
    pub speech: Option<SpeechHeard<'a>>,
}

/// An agent's speech that was being heard when a snapshot was taken, as
/// far as facts had recorded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeechHeard<'a> {
    /// The response being spoken.
    pub generation: GenId,
    /// Its agent.
    pub agent: AgentId,
    /// What had been recorded as heard of it.
    pub text: &'a str,
}

/// Why a session could not be rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreError {
    /// The configuration is unusable.
    Config(ConfigError),
    /// The named limit of the configuration is too small for what the
    /// snapshot holds.
    TooSmall(&'static str),
    /// The facts do not continue the snapshot.
    Gap {
        /// The sequence number that should have come next.
        expected: Seq,
        /// The one that did.
        found: Seq,
    },
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(f, "unusable configuration: {error}"),
            Self::TooSmall(limit) => {
                write!(f, "the limit `{limit}` is too small for the snapshot")
            }
            Self::Gap { expected, found } => {
                write!(f, "the journal skips from fact {expected} to fact {found}")
            }
        }
    }
}

impl core::error::Error for RestoreError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::TooSmall(_) | Self::Gap { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{CallState, ItemView, Kernel, RestoreError, Snapshot, SpeechHeard};
    use crate::command::LlmCause;
    use crate::config::{ConfigError, KernelConfig};
    use crate::event::{CloseReason, ControlInput, Event};
    use crate::fact::{Dismissal, EpochCause, Fact, Lifecycle, NoticeKind, Stamped, ToolOutcome};
    use crate::ids::{
        AgentId, BatchId, ContextVersion, Epoch, GenId, OpTag, ParticipantId, Seq, ToolCallId,
        ToolId, TurnId,
    };
    use crate::out::Out;
    use crate::policy::{BoxedPolicies, DefaultPolicies};
    use crate::text::TextArena;
    use crate::time::{MediaSpan, MediaTime, MonoTime};

    fn joined(seq: u64) -> Stamped {
        Stamped {
            seq: Seq(seq),
            fact: Fact::ParticipantJoined {
                who: ParticipantId(1),
            },
        }
    }

    #[test]
    fn a_new_session_is_running_and_empty() {
        let kernel: Kernel = Kernel::default();
        let view = kernel.view();
        assert_eq!(view.lifecycle(), Lifecycle::Running);
        assert_eq!(view.epoch(), Epoch(0));
        assert_eq!(view.front(), AgentId(0));
        assert_eq!(view.next_seq(), Seq(1));
        assert!(view.is_attached());
        assert_eq!(view.items().count(), 0);
        assert_eq!(view.turns().count(), 0);
        assert_eq!(view.generations().count(), 0);
        assert_eq!(view.calls().count(), 0);
        assert!(!view.is_live(OpTag {
            generation: GenId(1),
            epoch: Epoch(0),
        }));
    }

    #[test]
    fn a_step_leaves_only_its_own_output() {
        let mut kernel: Kernel = Kernel::default();
        let mut out = Out::new();
        out.facts.push(joined(1));
        let _ = out.text.push("left over from an earlier step");
        let event = Event::Control(ControlInput::Detached);
        kernel.step(MonoTime::from_millis(5), &event, &mut out);
        assert!(out.is_empty());
        assert!(out.text.is_empty());
    }

    #[test]
    fn an_unusable_configuration_is_refused() {
        let config = KernelConfig {
            front: AgentId(4),
            ..KernelConfig::default()
        };
        let error = Kernel::new(config, DefaultPolicies::default()).unwrap_err();
        assert_eq!(error, ConfigError::UnknownAgent(AgentId(4)));
    }

    #[test]
    fn restoring_a_snapshot_gives_an_equal_session() {
        let kernel = Kernel::new(KernelConfig::default(), BoxedPolicies::default()).unwrap();
        let snapshot = kernel.snapshot();
        assert_eq!(snapshot.next_seq(), Seq(1));
        let restored = Kernel::restore(
            KernelConfig::default(),
            BoxedPolicies::default(),
            &snapshot,
            &[],
            &TextArena::default(),
        )
        .unwrap();
        assert_eq!(restored.snapshot(), snapshot);
        assert_eq!(restored.view().next_seq(), Seq(1));
    }

    /// A session that has been through a bit of everything: a turn, a
    /// response that asked for two tools of which one has ended, a note,
    /// a handoff that was asked for, and speech that is being heard.
    fn busy() -> (Vec<Stamped>, TextArena) {
        let mut text = TextArena::default();
        let facts = [
            Fact::ParticipantJoined {
                who: ParticipantId(1),
            },
            Fact::ParticipantJoined {
                who: ParticipantId(2),
            },
            Fact::TurnCommitted {
                turn: TurnId(1),
                who: ParticipantId(1),
                span: MediaSpan::new(MediaTime::from_ticks(48_000), MediaTime::from_ticks(96_000)),
                text: text.push("what time is it, and note that I asked"),
                addressed: true,
            },
            Fact::ToolProposed {
                call: ToolCallId(1),
                batch: BatchId(1),
                generation: GenId(1),
                tool: ToolId(0),
                args: text.push("{}"),
            },
            Fact::ToolProposed {
                call: ToolCallId(2),
                batch: BatchId(1),
                generation: GenId(1),
                tool: ToolId(0),
                args: text.push(""),
            },
            Fact::ToolAuthorized {
                call: ToolCallId(1),
            },
            Fact::ToolAuthorized {
                call: ToolCallId(2),
            },
            Fact::ToolDispatched {
                call: ToolCallId(1),
                attempt: 0,
            },
            Fact::ToolDispatched {
                call: ToolCallId(2),
                attempt: 1,
            },
            Fact::ToolFinished {
                call: ToolCallId(1),
                outcome: ToolOutcome::Completed,
                result: text.push("ten past three"),
            },
            Fact::EpochAdvanced {
                to: Epoch(3),
                cause: EpochCause::BargeIn,
            },
            Fact::UtteranceCommitted {
                generation: GenId(2),
                agent: AgentId(0),
                answers: Some(ContextVersion(1)),
                text: text.push("It is ten"),
                truncated: true,
            },
            Fact::NoticeAppended {
                kind: NoticeKind::Recovered,
                text: text.push("Some work was lost."),
            },
            Fact::TurnDismissed {
                turn: TurnId(4),
                who: ParticipantId(2),
                reason: Dismissal::Backchannel,
            },
            Fact::HandoffRequested { to: AgentId(0) },
            Fact::LifecycleChanged {
                to: Lifecycle::Draining,
                reason: Some(CloseReason::Requested),
            },
            Fact::UtteranceProgress {
                generation: GenId(6),
                agent: AgentId(0),
                heard: text.push("Goodbye, and "),
            },
        ];
        let stamped = facts
            .into_iter()
            .zip(1..)
            .map(|(fact, seq)| Stamped {
                seq: Seq(seq),
                fact,
            })
            .collect();
        (stamped, text)
    }

    fn restored(snapshot: &Snapshot, tail: &[Stamped], text: &TextArena) -> Kernel {
        Kernel::restore(
            KernelConfig::default(),
            DefaultPolicies::default(),
            snapshot,
            tail,
            text,
        )
        .unwrap()
    }

    #[test]
    fn a_snapshot_taken_apart_and_put_together_is_the_same_snapshot() {
        let blank = Kernel::<DefaultPolicies>::default().snapshot();
        assert_eq!(Snapshot::from_parts(&blank.parts()), blank);

        let (facts, text) = busy();
        // After every fact, so that every stage of the session is one
        // the snapshot is taken at.
        for upto in 1..=facts.len() {
            let snapshot = restored(&blank, &facts[..upto], &text).snapshot();
            let parts = snapshot.parts();
            let again = Snapshot::from_parts(&parts);
            assert_eq!(again, snapshot, "after fact {upto}");
            assert_eq!(again.parts(), parts, "after fact {upto}");
            // And a session goes on from it as from the facts.
            let from_parts = restored(&again, &facts[upto..], &text);
            let from_facts = restored(&blank, &facts, &text);
            assert_eq!(from_parts.snapshot(), from_facts.snapshot());
            assert_eq!(from_parts.fingerprint(), from_facts.fingerprint());
        }
    }

    #[test]
    fn the_parts_of_a_snapshot_say_what_the_session_remembers() {
        let blank = Kernel::<DefaultPolicies>::default().snapshot();
        let (facts, text) = busy();
        let snapshot = restored(&blank, &facts, &text).snapshot();
        let parts = snapshot.parts();
        assert_eq!(parts.next_seq, Seq(18));
        assert_eq!(parts.epoch, Epoch(3));
        assert_eq!(parts.lifecycle, Lifecycle::Draining);
        assert_eq!(parts.closed_for, Some(CloseReason::Requested));
        assert_eq!(parts.handoff, Some(AgentId(0)));
        assert_eq!(parts.participants, [ParticipantId(1), ParticipantId(2)]);
        assert_eq!((parts.turn_floor, parts.gen_floor), (TurnId(5), GenId(7)));
        assert_eq!(
            (parts.next_call, parts.next_batch),
            (ToolCallId(3), BatchId(2))
        );
        // The call that has not ended, and its batch.
        assert_eq!(parts.calls.len(), 1);
        assert_eq!(
            (
                parts.calls[0].call,
                parts.calls[0].state,
                parts.calls[0].attempt
            ),
            (ToolCallId(2), CallState::Dispatched, 1)
        );
        assert_eq!(parts.batches.len(), 1);
        assert_eq!((parts.batches[0].pending, parts.batches[0].ran), (1, true));
        assert_eq!(parts.items.len(), 6);
        assert_eq!(
            parts.items[5],
            ItemView::Notice {
                kind: NoticeKind::Recovered,
                text: "Some work was lost.",
            }
        );
        assert_eq!(
            parts.owed,
            Some((ContextVersion(1), LlmCause::Reply { turn: TurnId(1) }))
        );
        assert_eq!(parts.answered, ContextVersion(1));
        assert_eq!(
            parts.speech,
            Some(SpeechHeard {
                generation: GenId(6),
                agent: AgentId(0),
                text: "Goodbye, and ",
            })
        );
    }

    #[test]
    fn a_call_that_is_being_stopped_is_put_together_as_one_that_was_sent() {
        let blank = Kernel::<DefaultPolicies>::default().snapshot();
        let (facts, text) = busy();
        let snapshot = restored(&blank, &facts, &text).snapshot();
        let mut parts = snapshot.parts();
        parts.calls[0].state = CallState::Cancelling;
        assert_eq!(Snapshot::from_parts(&parts), snapshot);
    }

    #[test]
    fn restoring_needs_a_journal_without_gaps() {
        let snapshot = Kernel::<DefaultPolicies>::default().snapshot();
        let restore = |tail: &[Stamped]| {
            Kernel::restore(
                KernelConfig::default(),
                DefaultPolicies::default(),
                &snapshot,
                tail,
                &TextArena::default(),
            )
        };
        let restored = restore(&[joined(1), joined(2)]).unwrap();
        assert_eq!(restored.view().next_seq(), Seq(3));
        assert_eq!(
            restore(&[joined(1), joined(3)]).unwrap_err(),
            RestoreError::Gap {
                expected: Seq(2),
                found: Seq(3),
            }
        );
    }
}
