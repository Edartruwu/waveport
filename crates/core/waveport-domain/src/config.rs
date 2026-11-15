//! What a session is set up with.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use crate::ids::{AgentId, ToolId};
use crate::time::MediaDur;

/// Everything the kernel needs to know about a session before it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelConfig {
    /// How large the kernel's tables are.
    pub limits: Limits,
    /// How long the kernel waits for things.
    pub timing: Timing,
    /// The agents. An [`AgentId`] is an index into this list.
    pub agents: Vec<AgentSpec>,
    /// The agent that holds the floor when the session starts.
    pub front: AgentId,
    /// The tools. A [`ToolId`] is an index into this list.
    pub tools: Vec<ToolSpec>,
}

impl Default for KernelConfig {
    /// One agent, no tools, default limits and timing.
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            timing: Timing::default(),
            agents: vec![AgentSpec::default()],
            front: AgentId(0),
            tools: Vec::new(),
        }
    }
}

impl KernelConfig {
    /// The specification of `tool`, if there is such a tool.
    #[must_use]
    pub fn tool(&self, tool: ToolId) -> Option<&ToolSpec> {
        self.tools.get(usize::from(tool.get()))
    }

    /// The specification of `agent`, if there is such an agent.
    #[must_use]
    pub fn agent(&self, agent: AgentId) -> Option<&AgentSpec> {
        self.agents.get(usize::from(agent.get()))
    }

    /// Checks that the configuration describes a session that can run.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let limits = &self.limits;
        for (name, value) in [
            ("participants", limits.participants),
            ("open_turns", limits.open_turns),
            ("generations", limits.generations),
            ("open_calls", limits.open_calls),
            ("pending_text", limits.pending_text),
            ("response_text", limits.response_text),
        ] {
            if value == 0 {
                return Err(ConfigError::ZeroLimit(name));
            }
        }
        if self.agents.is_empty() || self.agents.len() > usize::from(u8::MAX) {
            return Err(ConfigError::AgentCount(self.agents.len()));
        }
        if self.tools.len() > usize::from(u16::MAX) {
            return Err(ConfigError::ToolCount(self.tools.len()));
        }
        if self.agent(self.front).is_none() {
            return Err(ConfigError::UnknownAgent(self.front));
        }
        for agent in &self.agents {
            if let Some(&tool) = agent.tools.iter().find(|&&tool| self.tool(tool).is_none()) {
                return Err(ConfigError::UnknownTool(tool));
            }
        }
        for (index, tool) in self.tools.iter().enumerate() {
            let Executor::Agent(worker) = tool.executor else {
                continue;
            };
            let id = ToolId(u16::try_from(index).unwrap_or(u16::MAX));
            let Some(spec) = self.agent(worker) else {
                return Err(ConfigError::UnknownAgent(worker));
            };
            // An agent that works on a delegated call may not delegate in
            // turn: that keeps the number of responses a call can start at
            // one.
            let delegates = spec.tools.iter().any(|&inner| {
                self.tool(inner)
                    .is_some_and(|inner| matches!(inner.executor, Executor::Agent(_)))
            });
            if delegates || worker == self.front {
                return Err(ConfigError::NestedDelegation(id));
            }
        }
        Ok(())
    }
}

/// How large the kernel's tables are. Each is reserved in full when the
/// session starts and never grows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Limits {
    /// Humans in the session at once.
    pub participants: usize,
    /// User turns open at once.
    pub open_turns: usize,
    /// Responses under way at once.
    pub generations: usize,
    /// Tool calls that have not ended.
    pub open_calls: usize,
    /// Bytes of language-model text waiting to be spoken before the kernel
    /// tells the model to wait. A model that goes on to twice this is
    /// stopped, and what it wrote is said.
    pub pending_text: usize,
    /// Bytes reserved for the text of each response when it starts. A
    /// longer response grows its buffer.
    pub response_text: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            participants: 8,
            open_turns: 8,
            generations: 4,
            open_calls: 16,
            pending_text: 8192,
            response_text: 2048,
        }
    }
}

/// How long the kernel waits for things.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Timing {
    /// How long a turn that has ended waits for its final transcript.
    pub transcript_wait: Duration,
    /// How far before the end of a turn's speech its final transcript may
    /// stop and still count as complete. Recognisers time the last word,
    /// and voice activity ends some time after it.
    pub transcript_slack: MediaDur,
    /// The longest a user turn may last.
    pub turn_max: Duration,
    /// The longest a response may go without producing anything.
    pub generation_max: Duration,
    /// How often the heard part of an agent's speech is recorded.
    pub checkpoint_every: Duration,
    /// How long a cancelled tool call may take to confirm a rollback.
    pub rollback_wait: Duration,
    /// How long a closing session may spend finishing what is under way.
    pub drain: Duration,
    /// How long it may then spend stopping.
    pub grace: Duration,
    /// The fade-out when the agent is interrupted, in milliseconds.
    pub stop_fade_ms: u16,
    /// How much synthesised speech may wait to be played. Text is held back
    /// from synthesis beyond this, which bounds what an interruption wastes.
    pub speech_credit: MediaDur,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            transcript_wait: Duration::from_secs(2),
            transcript_slack: MediaDur::from_millis(600),
            turn_max: Duration::from_secs(120),
            generation_max: Duration::from_secs(60),
            checkpoint_every: Duration::from_secs(1),
            rollback_wait: Duration::from_secs(2),
            drain: Duration::from_secs(5),
            grace: Duration::from_secs(2),
            stop_fade_ms: 8,
            speech_credit: MediaDur::from_millis(3_000),
        }
    }
}

/// An agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AgentSpec {
    /// The tools its language model may call.
    pub tools: Vec<ToolId>,
}

/// A tool, as far as the kernel is concerned. Its name and schema belong to
/// the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ToolSpec {
    /// What running the tool does to the world.
    pub effect: Effect,
    /// What must be true before a call is dispatched.
    pub gate: Gate,
    /// How long a call may take.
    pub timeout: Duration,
    /// What runs the tool.
    pub executor: Executor,
}

impl ToolSpec {
    /// A tool run by the application, with no gate and a ten-second
    /// timeout.
    #[must_use]
    pub const fn new(effect: Effect) -> Self {
        Self {
            effect,
            gate: Gate::None,
            timeout: Duration::from_secs(10),
            executor: Executor::External,
        }
    }
}

/// What running a tool does to the world. It decides what may happen to a
/// call after it has been dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Nothing: the call can be cancelled or repeated freely.
    Pure,
    /// Something, but repeating the call with the same key does it once.
    Idempotent,
    /// Something that must happen at most once.
    SideEffecting,
}

/// A condition on dispatching a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gate {
    /// None.
    None,
    /// The speech of the response that asked for the call has been played
    /// to the end. For a tool such as "end the call".
    AfterOwnUtterance,
}

/// What runs a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Executor {
    /// The application, through the tool port.
    External,
    /// Another agent: the call starts a response of that agent, and what it
    /// writes is the call's result.
    Agent(AgentId),
}

/// Why a configuration cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConfigError {
    /// The named limit is zero.
    ZeroLimit(&'static str),
    /// There must be between 1 and 255 agents.
    AgentCount(usize),
    /// There may be at most 65,535 tools.
    ToolCount(usize),
    /// The configuration names an agent that is not in the list.
    UnknownAgent(AgentId),
    /// An agent lists a tool that is not in the list.
    UnknownTool(ToolId),
    /// The tool is run by the front agent, or by an agent that itself has a
    /// tool run by an agent.
    NestedDelegation(ToolId),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroLimit(name) => write!(f, "the limit `{name}` is zero"),
            Self::AgentCount(count) => {
                write!(f, "a session needs between 1 and 255 agents, not {count}")
            }
            Self::ToolCount(count) => write!(f, "a session has at most 65535 tools, not {count}"),
            Self::UnknownAgent(agent) => write!(f, "there is no agent {agent}"),
            Self::UnknownTool(tool) => write!(f, "there is no tool {tool}"),
            Self::NestedDelegation(tool) => write!(
                f,
                "tool {tool} is run by the front agent or by an agent that delegates in turn"
            ),
        }
    }
}

impl core::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{AgentSpec, ConfigError, Effect, Executor, KernelConfig, ToolSpec};
    use crate::ids::{AgentId, ToolId};

    fn delegating() -> KernelConfig {
        let mut config = KernelConfig::default();
        config.tools.push(ToolSpec {
            executor: Executor::Agent(AgentId(1)),
            ..ToolSpec::new(Effect::Pure)
        });
        config.agents = vec![
            AgentSpec {
                tools: vec![ToolId(0)],
            },
            AgentSpec::default(),
        ];
        config
    }

    #[test]
    fn the_default_is_usable() {
        assert_eq!(KernelConfig::default().validate(), Ok(()));
        assert_eq!(delegating().validate(), Ok(()));
    }

    #[test]
    fn unknown_names_are_refused() {
        let config = KernelConfig {
            front: AgentId(3),
            ..KernelConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(ConfigError::UnknownAgent(AgentId(3)))
        );

        let config = KernelConfig {
            agents: vec![AgentSpec {
                tools: vec![ToolId(9)],
            }],
            ..KernelConfig::default()
        };
        assert_eq!(config.validate(), Err(ConfigError::UnknownTool(ToolId(9))));

        let mut config = delegating();
        config.agents.truncate(1);
        assert_eq!(
            config.validate(),
            Err(ConfigError::UnknownAgent(AgentId(1)))
        );
    }

    #[test]
    fn an_empty_table_is_refused() {
        let mut config = KernelConfig::default();
        config.limits.generations = 0;
        assert_eq!(
            config.validate(),
            Err(ConfigError::ZeroLimit("generations"))
        );
        config = KernelConfig::default();
        config.agents.clear();
        assert_eq!(config.validate(), Err(ConfigError::AgentCount(0)));
    }

    #[test]
    fn delegation_is_one_level_deep() {
        let mut config = delegating();
        if let Some(worker) = config.agents.get_mut(1) {
            worker.tools.push(ToolId(0));
        }
        assert_eq!(
            config.validate(),
            Err(ConfigError::NestedDelegation(ToolId(0)))
        );

        let mut config = delegating();
        config.front = AgentId(1);
        assert_eq!(
            config.validate(),
            Err(ConfigError::NestedDelegation(ToolId(0)))
        );
    }
}
