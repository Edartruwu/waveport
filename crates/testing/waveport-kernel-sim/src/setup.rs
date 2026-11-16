//! How a scenario's session is set up.

use core::time::Duration;

use waveport_domain::config::{AgentSpec, Effect, Executor, KernelConfig, ToolSpec};
use waveport_domain::ids::{AgentId, ParticipantId, ToolId};
use waveport_domain::policy::{AddressedByName, BoxedPolicies};

/// The name the first human in a session goes by.
const CALLER: &str = "the caller";
/// The name the agent that holds the floor goes by.
const FRONT: &str = "the agent";

/// The session a scenario runs in: its tools, its agents and its timing.
///
/// Without anything added it is one caller talking to one agent that has no
/// tools and treats everything said as meant for it.
#[derive(Clone, Debug, Default)]
#[must_use]
pub struct Setup {
    tools: Vec<ToolDef>,
    back_agents: Vec<String>,
    answers_to: Vec<String>,
    drain: Option<Duration>,
    grace: Option<Duration>,
}

#[derive(Clone, Debug)]
struct ToolDef {
    name: String,
    effect: Effect,
    run_by: Option<String>,
}

impl Setup {
    /// One caller, one agent, no tools.
    pub fn new() -> Self {
        Self::default()
    }

    /// The agent can call `name`, a tool the application runs.
    pub fn tool(mut self, name: &str, effect: Effect) -> Self {
        self.tools.push(ToolDef {
            name: name.to_owned(),
            effect,
            run_by: None,
        });
        self
    }

    /// A second agent, which never speaks to the caller.
    pub fn back_agent(mut self, name: &str) -> Self {
        self.back_agents.push(name.to_owned());
        self
    }

    /// The agent can call `tool`, which is run by the back agent `agent`:
    /// what that agent writes is the call's result.
    pub fn delegate(mut self, tool: &str, agent: &str) -> Self {
        self.tools.push(ToolDef {
            name: tool.to_owned(),
            effect: Effect::Pure,
            run_by: Some(agent.to_owned()),
        });
        self
    }

    /// The agent replies only to turns that contain `name`.
    pub fn answers_to(mut self, name: &str) -> Self {
        self.answers_to.push(name.to_owned());
        self
    }

    /// How long a closing session may spend finishing what is under way.
    pub fn drain_ms(mut self, millis: u64) -> Self {
        self.drain = Some(Duration::from_millis(millis));
        self
    }

    /// How long a closing session may then spend stopping.
    pub fn grace_ms(mut self, millis: u64) -> Self {
        self.grace = Some(Duration::from_millis(millis));
        self
    }

    /// The kernel configuration, the policies and the names this setup
    /// describes, or what is wrong with it.
    pub(crate) fn build(self) -> Result<(KernelConfig, BoxedPolicies, Names), String> {
        let mut names = Names {
            people: vec![CALLER.to_owned()],
            agents: vec![FRONT.to_owned()],
            tools: Vec::new(),
        };
        names.agents.extend(self.back_agents.iter().cloned());

        let mut config = KernelConfig::default();
        let mut front = AgentSpec::default();
        for (index, tool) in self.tools.iter().enumerate() {
            if names.tool_id(&tool.name).is_some() {
                return Err(format!("the setup names the tool `{}` twice", tool.name));
            }
            let executor = match &tool.run_by {
                None => Executor::External,
                Some(agent) => Executor::Agent(names.agent_id(agent).ok_or_else(|| {
                    format!(
                        "the tool `{}` is run by `{agent}`, which is not a back agent of the setup",
                        tool.name
                    )
                })?),
            };
            let id = u16::try_from(index).map_err(|_| "the setup has too many tools".to_owned())?;
            front.tools.push(ToolId(id));
            names.tools.push(tool.name.clone());
            config.tools.push(ToolSpec {
                executor,
                ..ToolSpec::new(tool.effect)
            });
        }
        config.agents = vec![front];
        config
            .agents
            .extend(self.back_agents.iter().map(|_| AgentSpec::default()));
        if let Some(drain) = self.drain {
            config.timing.drain = drain;
        }
        if let Some(grace) = self.grace {
            config.timing.grace = grace;
        }
        config
            .validate()
            .map_err(|error| format!("the setup is unusable: {error}"))?;

        let mut policies = BoxedPolicies::default();
        if !self.answers_to.is_empty() {
            let answers_to = self
                .answers_to
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            policies.addressee = Box::new(AddressedByName::new(&answers_to));
        }
        Ok((config, policies, names))
    }
}

/// What people, agents and tools are called in a scenario. The kernel knows
/// them only by number.
#[derive(Clone, Debug, Default)]
pub(crate) struct Names {
    people: Vec<String>,
    agents: Vec<String>,
    tools: Vec<String>,
}

impl Names {
    /// Names for a session whose setup was unusable.
    pub(crate) fn fallback() -> Self {
        Self {
            people: vec![CALLER.to_owned()],
            agents: vec![FRONT.to_owned()],
            tools: Vec::new(),
        }
    }

    /// Adds a human and returns their identity. Identities count from 1.
    pub(crate) fn add_person(&mut self, name: &str) -> ParticipantId {
        self.people.push(name.to_owned());
        ParticipantId(u16::try_from(self.people.len()).unwrap_or(u16::MAX))
    }

    pub(crate) fn person(&self, who: ParticipantId) -> &str {
        usize::from(who.get())
            .checked_sub(1)
            .and_then(|index| self.people.get(index))
            .map_or("someone unknown", String::as_str)
    }

    pub(crate) fn agent(&self, agent: AgentId) -> &str {
        self.agents
            .get(usize::from(agent.get()))
            .map_or("an unknown agent", String::as_str)
    }

    pub(crate) fn agent_id(&self, name: &str) -> Option<AgentId> {
        let index = self.agents.iter().position(|agent| agent == name)?;
        u8::try_from(index).ok().map(AgentId)
    }

    pub(crate) fn agent_count(&self) -> usize {
        self.agents.len()
    }

    pub(crate) fn tool(&self, tool: ToolId) -> &str {
        self.tools
            .get(usize::from(tool.get()))
            .map_or("an unknown tool", String::as_str)
    }

    pub(crate) fn tool_id(&self, name: &str) -> Option<ToolId> {
        let index = self.tools.iter().position(|tool| tool == name)?;
        u16::try_from(index).ok().map(ToolId)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use waveport_domain::config::{Effect, Executor};
    use waveport_domain::ids::{AgentId, ParticipantId, ToolId};
    use waveport_domain::policy::{AddresseeInput, Policies};

    use super::Setup;

    #[test]
    fn a_plain_setup_is_one_caller_and_one_agent() {
        let (config, policies, mut names) = Setup::new().build().unwrap();
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.tools, []);
        assert_eq!(names.person(ParticipantId(1)), "the caller");
        assert_eq!(names.agent(AgentId(0)), "the agent");
        assert_eq!(names.add_person("Ben"), ParticipantId(2));
        assert_eq!(names.person(ParticipantId(2)), "Ben");
        assert_eq!(names.person(ParticipantId(0)), "someone unknown");
        let anything = AddresseeInput {
            who: ParticipantId(1),
            text: "is anyone there",
            humans: 1,
        };
        assert!(policies.addressee().addressed(&anything));
    }

    #[test]
    fn tools_agents_and_timing_reach_the_kernel_configuration() {
        let setup = Setup::new()
            .tool("refund", Effect::SideEffecting)
            .back_agent("researcher")
            .delegate("research", "researcher")
            .answers_to("Ava")
            .drain_ms(2_000)
            .grace_ms(1_000);
        let (config, policies, names) = setup.build().unwrap();
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.agents[0].tools, [ToolId(0), ToolId(1)]);
        assert_eq!(config.agents[1].tools, []);
        assert_eq!(config.tools[0].effect, Effect::SideEffecting);
        assert_eq!(config.tools[1].executor, Executor::Agent(AgentId(1)));
        assert_eq!(config.timing.drain, Duration::from_secs(2));
        assert_eq!(config.timing.grace, Duration::from_secs(1));
        assert_eq!(names.tool_id("research"), Some(ToolId(1)));
        assert_eq!(names.tool(ToolId(0)), "refund");
        assert_eq!(names.agent_id("researcher"), Some(AgentId(1)));
        assert_eq!(names.agent_count(), 2);
        let aside = AddresseeInput {
            who: ParticipantId(1),
            text: "ben did you send it",
            humans: 2,
        };
        assert!(!policies.addressee().addressed(&aside));
    }

    #[test]
    fn a_broken_setup_says_what_is_wrong() {
        let error = Setup::new().delegate("research", "nobody").build().err();
        assert!(error.unwrap().contains("not a back agent"));
        let error = Setup::new()
            .tool("refund", Effect::Pure)
            .tool("refund", Effect::Pure)
            .build()
            .err();
        assert!(error.unwrap().contains("twice"));
    }
}
