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
mod view;

use core::fmt;

pub use view::{CallState, CallView, GenerationRole, GenerationView, ItemView, TurnView, View};

use self::conversation::Conversation;
use self::coordinator::Step;
use self::floor::OpenTurn;
use self::lifecycle::Closing;
use self::response::Generation;
use crate::bounded::Bounded;
use crate::command::LlmCause;
use crate::config::{AgentSpec, ConfigError, KernelConfig, ToolSpec};
use crate::event::{ControlInput, Event};
use crate::fact::{Fact, Lifecycle, Stamped};
use crate::ids::{AgentId, ContextVersion, Epoch, GenId, ParticipantId, Seq, ToolId, TurnId};
use crate::out::Out;
use crate::policy::{DefaultPolicies, Policies};
use crate::text::TextArena;
use crate::time::MonoTime;

/// Everything about a session that a step may change. It is plain data:
/// two kernels with equal state behave the same from then on.
///
/// Part of it outlives a crash: the conversation, the participants, the
/// epoch and the stage of the session's life. That part changes only in
/// [`State::apply`], by a fact that is journaled, so the journal can
/// rebuild it. The rest, the turns and responses under way, is lost with
/// the process, and the session carries on without it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct State {
    epoch: Epoch,
    lifecycle: Lifecycle,
    closing: Option<Closing>,
    attached: bool,
    /// Whether this kernel still owns the session.
    leased: bool,
    front: AgentId,
    /// The sequence number the next fact gets.
    next_seq: Seq,
    next_turn: TurnId,
    next_gen: GenId,
    participants: Bounded<ParticipantId>,
    turns: Bounded<OpenTurn>,
    generations: Bounded<Generation>,
    conversation: Conversation,
    /// The newest item of the conversation that is owed a response, as the
    /// version of the conversation that holds it, and what the response is
    /// for.
    owed: Option<(ContextVersion, LlmCause)>,
    /// The conversation the latest response was started on. A response is
    /// owed while `owed` is newer than this.
    answered: ContextVersion,
}

impl State {
    fn new(config: &KernelConfig) -> Self {
        let limits = &config.limits;
        Self {
            epoch: Epoch(0),
            lifecycle: Lifecycle::Running,
            closing: None,
            attached: true,
            leased: true,
            front: config.front,
            next_seq: Seq(1),
            next_turn: TurnId(1),
            next_gen: GenId(1),
            participants: Bounded::new(limits.participants),
            turns: Bounded::new(limits.open_turns),
            generations: Bounded::new(limits.generations),
            conversation: Conversation::default(),
            owed: None,
            answered: ContextVersion(0),
        }
    }

    /// Makes a fact true. `text` is the arena the fact's text refers to.
    ///
    /// This only sets values. Every decision was taken before the fact was
    /// emitted, so applying the same facts in the same order gives the
    /// same state, in this build or a later one.
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
            Fact::LifecycleChanged { to, .. } => self.lifecycle = to,
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
                self.next_turn = self.next_turn.max(turn.next());
            }
            Fact::TurnDismissed { turn, .. } => {
                self.next_turn = self.next_turn.max(turn.next());
            }
            Fact::UtteranceCommitted {
                generation,
                agent,
                text: words,
                truncated,
            } => {
                self.conversation
                    .push_utterance(generation, agent, text.get(words), truncated);
                self.next_gen = self.next_gen.max(generation.next());
            }
            // Recorded for recovery, which arrives with the tools.
            Fact::UtteranceProgress { .. }
            | Fact::NoticeAppended { .. }
            | Fact::ToolProposed { .. }
            | Fact::ToolAuthorized { .. }
            | Fact::ToolDispatched { .. }
            | Fact::ToolFinished { .. }
            | Fact::BatchResolved { .. }
            | Fact::HandoffRequested { .. }
            | Fact::AgentActivated { .. } => {}
        }
    }
}

/// One session's kernel.
///
/// The kernel performs no I/O, reads no clock and never waits. Everything
/// it learns arrives through [`step`](Self::step), and everything it wants
/// done leaves through the [`Out`] passed to it.
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

    /// The session's state, to be stored. Together with the facts emitted
    /// after it, a snapshot rebuilds the session.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            state: self.state.clone(),
        }
    }

    /// Rebuilds a session from a snapshot and the facts journaled after it.
    /// `text` is the arena the facts' text refers to.
    ///
    /// # Errors
    ///
    /// The configuration is unusable, or `tail` does not continue the
    /// snapshot without a gap.
    pub fn restore(
        config: KernelConfig,
        policies: P,
        snapshot: &Snapshot,
        tail: &[Stamped],
        text: &TextArena,
    ) -> Result<Self, RestoreError> {
        config.validate().map_err(RestoreError::Config)?;
        let mut state = snapshot.state.clone();
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
        state.conversation.park();
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
            state.conversation.heap_bytes(),
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

/// A session's state at one point in its journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    state: State,
}

impl Snapshot {
    /// The sequence number of the first fact that is not in the snapshot.
    #[must_use]
    pub const fn next_seq(&self) -> Seq {
        self.state.next_seq
    }
}

/// Why a session could not be rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreError {
    /// The configuration is unusable.
    Config(ConfigError),
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
            Self::Gap { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Kernel, RestoreError};
    use crate::config::{ConfigError, KernelConfig};
    use crate::event::{ControlInput, Event};
    use crate::fact::{Fact, Lifecycle, Stamped};
    use crate::ids::{AgentId, Epoch, GenId, OpTag, ParticipantId, Seq};
    use crate::out::Out;
    use crate::policy::{BoxedPolicies, DefaultPolicies};
    use crate::text::TextArena;
    use crate::time::MonoTime;

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
        assert_eq!(restored.fingerprint(), kernel.fingerprint());
        assert_eq!(restored.snapshot(), snapshot);
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
