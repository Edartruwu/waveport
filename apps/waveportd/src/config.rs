//! What a daemon is told: where it listens, who may ask, where sessions
//! are kept, and its agents.

use std::collections::BTreeMap;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use serde::Deserialize;
use waveport_assembly::Failure;
use waveport_assembly::spec::{ASSETS, AgentSpec, Keeps, Named, STATE_EVERY, StackSpec, read_toml};

/// What the settings of a daemon are called in its file.
pub(crate) const NAMED: Named = Named {
    journal: "`journal.file`",
    memory: "leaving `journal.file` out",
    assets: "`assets`",
    otlp: "`telemetry.otlp`",
    by: "the configuration",
};

/// What a daemon is told, as its file has it.
///
/// ```toml
/// listen = "127.0.0.1:7070"
///
/// [control]
/// key_env = ["WAVEPORTD_KEY"]
///
/// [journal]
/// file = "sessions.redb"
///
/// [agents.front-desk]
/// instructions = "You answer for a bakery, briefly."
/// greeting = "Good morning. What can I do for you?"
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The address the daemon listens on. Port 0 takes one that is
    /// free.
    #[serde(default = "listen")]
    pub listen: SocketAddr,
    /// The names the daemon is reached by, beside the address it listens
    /// on: what a request may say in `Host`, as `name` or `name:port`.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Where a page may come from that talks to the daemon from a
    /// browser, as the browser says it in `Origin`:
    /// `https://app.example`.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// How many threads sessions run on.
    #[serde(default = "one")]
    pub shards: usize,
    /// The processor each of those threads is kept on, one for each in
    /// their order, by the number the system has for it (as `taskset
    /// -c` takes it). Every other thread of the daemon is then kept off
    /// those processors. Left out, the system has the threads where it
    /// likes. On Linux only: anywhere else a daemon that is told this
    /// does not start.
    #[serde(default)]
    pub processors: Vec<usize>,
    /// Where the models that listen are, for the agents that hear.
    #[serde(default = "assets")]
    pub assets: PathBuf,
    /// Who may ask everything.
    #[serde(default)]
    pub control: Control,
    /// Where sessions are kept.
    #[serde(default)]
    pub journal: Journal,
    /// Where traces and metrics go.
    #[serde(default)]
    pub telemetry: Telemetry,
    /// How much of what, and for how long.
    #[serde(default)]
    pub limits: Limits,
    /// Whether the daemon serves a page to try its agents on, at
    /// `/playground/`. Left out, a daemon that listens on this machine
    /// alone serves it, and one that listens anywhere else does not.
    #[serde(default)]
    pub playground: Option<bool>,
    /// The calls that carriers bring.
    #[serde(default)]
    pub telephony: Telephony,
    /// The agents, by the names sessions are asked for with.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSpec>,
}

/// Who may ask everything.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    /// The names of the environment variables that hold a control key:
    /// two at most, so that a key can be changed while the other is
    /// still good. A key itself is never written here.
    #[serde(default)]
    pub key_env: Vec<String>,
    /// Whether whoever holds a control key is shown what the tools were
    /// given and what they answered.
    #[serde(default)]
    pub show_tool_text: bool,
}

/// The calls that carriers bring.
///
/// ```toml
/// [telephony]
/// public = "https://voice.example"
///
/// [telephony.twilio]
/// agent = "front-desk"
/// token_env = "TWILIO_AUTH_TOKEN"
/// ```
///
/// Twilio is then told to ask `https://voice.example/v1/telephony/twilio/voice`
/// when a call comes in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telephony {
    /// The address carriers reach the daemon at, as they are told it:
    /// `https://voice.example`, with nothing after the name but a port.
    /// A carrier signs what it asks with the address it asked at, and
    /// is answered with the address of a stream at this one, which it
    /// reaches over TLS. Whatever stands in front of the daemon and
    /// speaks TLS for it passes a WebSocket on.
    #[serde(default)]
    pub public: Option<String>,
    /// The calls that Twilio brings.
    #[serde(default)]
    pub twilio: Option<Twilio>,
}

/// The calls that Twilio brings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Twilio {
    /// The agent that answers, by its name. It hears and speaks.
    pub agent: String,
    /// The name of the environment variable that holds the auth token
    /// of the account, which is what Twilio signs its requests with.
    /// The token itself is never written here.
    pub token_env: String,
    /// The account whose calls are answered, as Twilio names it
    /// (`AC…`). Left out, every call that is signed with the token is.
    #[serde(default)]
    pub account: Option<String>,
    /// Whether a stream is turned away whose request is not signed as
    /// Twilio signs. Left out, a stream is let in by what the answer to
    /// its call gave it to say, which is good once, and one that is not
    /// signed is said once where the daemon runs: how Twilio signs the
    /// request for a stream is not written down in full.
    #[serde(default)]
    pub signed_streams: bool,
}

impl Telephony {
    /// What follows `https://` in the address carriers reach the
    /// daemon at: its name, and its port if it says one.
    #[must_use]
    pub fn authority(&self) -> Option<&str> {
        self.public
            .as_deref()
            .and_then(|public| public.strip_prefix("https://"))
    }

    /// Whether any carrier's calls are answered.
    #[must_use]
    pub const fn answers(&self) -> bool {
        self.twilio.is_some()
    }
}

/// Whether `authority` is a name, or a name and a port, and nothing
/// else. A name is one of letters, digits, dots and strokes, or an
/// address in brackets.
fn is_an_authority(authority: &str) -> bool {
    let (name, port) = match authority.rsplit_once(':') {
        Some((name, port)) if !port.contains(']') => (name, Some(port)),
        _ => (authority, None),
    };
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-');
    let named = name.starts_with(|c: char| c.is_ascii_alphanumeric()) && name.chars().all(plain);
    let numbered = name
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|address| address.parse::<Ipv6Addr>().is_ok());
    let ported = port.is_none_or(|port| port.parse::<u16>().is_ok_and(|port| port > 0));
    (named || numbered) && ported
}

/// Where sessions are kept.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    /// The file. Without one, sessions are kept in memory and are gone
    /// with the daemon.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// How many records of a session are written before all that it has
    /// come to is stored in one piece.
    #[serde(default = "state_every")]
    pub state_every: u64,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            file: None,
            state_every: STATE_EVERY,
        }
    }
}

/// Where traces and metrics go.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    /// The OpenTelemetry collector, which is sent OTLP over HTTP.
    #[serde(default)]
    pub otlp: Option<String>,
}

/// How much of what, and for how long.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// How long a client token can be connected with, in milliseconds,
    /// and how long a session that nobody connects to is kept.
    #[serde(default = "connect_within_ms")]
    pub connect_within_ms: u32,
    /// How long a daemon that is told to stop waits for the answers
    /// that are under way, in milliseconds.
    #[serde(default = "drain_ms")]
    pub drain_ms: u32,
    /// How many bytes the body of a request may have.
    #[serde(default = "body_bytes")]
    pub body_bytes: usize,
    /// How long a request that is not a stream may take, in
    /// milliseconds.
    #[serde(default = "request_ms")]
    pub request_ms: u32,
    /// How often something is sent on a stream of events that has
    /// nothing to say, in milliseconds, so that whatever stands between
    /// the daemon and a client does not take the stream for dead.
    #[serde(default = "keep_alive_ms")]
    pub keep_alive_ms: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect_within_ms: connect_within_ms(),
            drain_ms: drain_ms(),
            body_bytes: body_bytes(),
            request_ms: request_ms(),
            keep_alive_ms: keep_alive_ms(),
        }
    }
}

fn listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 7070))
}

const fn one() -> usize {
    1
}

fn assets() -> PathBuf {
    PathBuf::from(ASSETS)
}

const fn state_every() -> u64 {
    STATE_EVERY
}

const fn connect_within_ms() -> u32 {
    60_000
}

const fn drain_ms() -> u32 {
    10_000
}

const fn body_bytes() -> usize {
    64 * 1024
}

const fn request_ms() -> u32 {
    30_000
}

const fn keep_alive_ms() -> u32 {
    15_000
}

/// Whether `name` can be the name of an agent: a few letters, digits,
/// hyphens and underscores, beginning with a letter or a digit.
fn is_a_name(name: &str) -> bool {
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    name.len() <= 64
        && name.chars().next().is_some_and(plain)
        && name.chars().all(|c| plain(c) || c == '-' || c == '_')
}

impl Config {
    /// Reads a configuration from TOML.
    ///
    /// # Errors
    ///
    /// The text is not TOML, holds something a configuration does not
    /// have, or says something that cannot be. What the text held is not
    /// repeated: somebody may have written a key into it.
    pub fn from_toml(text: &str) -> Result<Self, Failure> {
        read_toml::<Self>(text, "the configuration")?.checked()
    }

    /// Whether the daemon serves the page to try its agents on.
    #[must_use]
    pub fn serves_playground(&self) -> bool {
        self.playground
            .unwrap_or_else(|| self.listen.ip().is_loopback())
    }

    /// The configuration, if what it says holds together.
    ///
    /// # Errors
    ///
    /// There is no agent, an agent has a name that is none, there are
    /// more than two control keys, a number is one that cannot be, or
    /// the shards do not have a processor each.
    pub fn checked(mut self) -> Result<Self, Failure> {
        if self.agents.is_empty() {
            return Err(Failure(
                "the configuration has no agent: there is nobody to answer".to_owned(),
            ));
        }
        if let Some(name) = self.agents.keys().find(|name| !is_a_name(name)) {
            return Err(Failure(format!(
                "{name:?} cannot be the name of an agent: a name is lower-case letters, digits, \
                 hyphens and underscores, and begins with a letter or a digit"
            )));
        }
        let mut agents = BTreeMap::new();
        for (name, agent) in core::mem::take(&mut self.agents) {
            let agent = agent
                .checked()
                .map_err(|Failure(why)| Failure(format!("the agent {name}: {why}")))?;
            agents.insert(name, agent);
        }
        self.agents = agents;
        if self.control.key_env.len() > 2 {
            return Err(Failure(
                "`control.key_env` names more than two variables: one key that is good and one \
                 that takes its place are all there are"
                    .to_owned(),
            ));
        }
        let numbers = [
            self.shards,
            self.limits.body_bytes,
            usize::try_from(self.journal.state_every).unwrap_or(usize::MAX),
            usize::try_from(self.limits.keep_alive_ms).unwrap_or(usize::MAX),
        ];
        if numbers.contains(&0) {
            return Err(Failure(
                "`shards`, `journal.state_every`, `limits.body_bytes` and \
                 `limits.keep_alive_ms` are each one at least"
                    .to_owned(),
            ));
        }
        if !self.processors.is_empty() && self.processors.len() != self.shards {
            return Err(Failure(format!(
                "`processors` names {} for {} `shards`: each shard has one",
                self.processors.len(),
                self.shards
            )));
        }
        let mut named = self.processors.clone();
        named.sort_unstable();
        if let Some(twice) = named.windows(2).find_map(|pair| match pair {
            [one, other] if one == other => Some(*one),
            _ => None,
        }) {
            return Err(Failure(format!(
                "`processors` names processor {twice} for two shards"
            )));
        }
        self.telephony_checked()?;
        Ok(self)
    }

    /// What the configuration says of calls, if it holds together.
    fn telephony_checked(&mut self) -> Result<(), Failure> {
        let telephony = &mut self.telephony;
        if let Some(public) = telephony.public.as_mut() {
            // Written with or without a stroke at its end, it is the
            // same address.
            if public.ends_with('/') {
                public.pop();
            }
        }
        if telephony.public.is_some() && !telephony.authority().is_some_and(is_an_authority) {
            return Err(Failure(
                "`telephony.public` is the address carriers reach the daemon at: `https://`, \
                 its name, a port if it has one, and nothing after that"
                    .to_owned(),
            ));
        }
        let Some(twilio) = &telephony.twilio else {
            return Ok(());
        };
        if telephony.public.is_none() {
            return Err(Failure(
                "`telephony.twilio` answers calls, and `telephony.public` does not say where \
                 Twilio reaches the daemon: it signs what it asks with that address"
                    .to_owned(),
            ));
        }
        if twilio.token_env.trim().is_empty() {
            return Err(Failure(
                "`telephony.twilio.token_env` names the environment variable that holds the \
                 account's auth token"
                    .to_owned(),
            ));
        }
        match self.agents.get(&twilio.agent) {
            None => Err(Failure(format!(
                "`telephony.twilio.agent` names {:?}, and there is no agent of that name",
                twilio.agent
            ))),
            Some(agent) if agent.speech.is_none() => Err(Failure(format!(
                "the agent {} answers calls and neither hears nor speaks: it has no `speech`",
                twilio.agent
            ))),
            Some(_) => Ok(()),
        }
    }

    /// What the agents of the daemon share. Where its threads run is
    /// not in it: that is told by whoever placed them
    /// ([`Threads`](crate::Threads)).
    #[must_use]
    pub fn stack(&self) -> StackSpec {
        StackSpec {
            shards: self.shards,
            pinning: None,
            keeps: self.journal.file.clone().map_or(Keeps::Memory, Keeps::File),
            state_every: self.journal.state_every,
            notes: None,
            otlp: self.telemetry.otlp.clone(),
            assets: self.assets.clone(),
            named: NAMED,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use waveport_assembly::spec::Keeps;

    use super::{Config, Limits, Telephony, is_an_authority};

    #[test]
    fn the_page_is_served_on_this_machine_and_elsewhere_only_if_told_to() {
        let serves = |more: &str| {
            let text = format!("{more}\n[agents.front-desk]\n");
            Config::from_toml(&text).unwrap().serves_playground()
        };
        assert!(serves(""));
        assert!(serves("listen = \"127.0.0.1:0\""));
        assert!(serves("listen = \"[::1]:7070\""));
        assert!(!serves("playground = false"));
        // A daemon that anyone can reach serves it to nobody, unless
        // whoever runs it says so.
        assert!(!serves("listen = \"0.0.0.0:7070\""));
        assert!(!serves("listen = \"192.0.2.7:7070\""));
        assert!(!serves("listen = \"[::]:7070\""));
        assert!(serves("listen = \"0.0.0.0:7070\"\nplayground = true"));
    }

    #[test]
    fn a_configuration_that_names_one_agent_is_enough() {
        let config = Config::from_toml("[agents.front-desk]\n").unwrap();
        assert_eq!(config.listen, SocketAddr::from(([127, 0, 0, 1], 7070)));
        assert_eq!(config.shards, 1);
        // Its threads are left to the system.
        assert_eq!(config.processors, [] as [usize; 0]);
        assert_eq!(config.control.key_env, [] as [String; 0]);
        assert_eq!(config.limits, Limits::default());
        assert_eq!(config.limits.connect_within_ms, 60_000);
        assert_eq!(config.stack().keeps, Keeps::Memory);
        assert_eq!(config.agents.len(), 1);
    }

    #[test]
    fn everything_a_daemon_is_told_can_be_said() {
        let text = r#"
listen = "0.0.0.0:8443"
hosts = ["voice.example"]
allowed_origins = ["https://app.example"]
shards = 4
processors = [6, 7, 2, 3]
assets = "/var/lib/waveport/assets"

[control]
key_env = ["WAVEPORTD_KEY", "WAVEPORTD_KEY_NEXT"]
show_tool_text = true

[journal]
file = "/var/lib/waveport/sessions.redb"
state_every = 64

[telemetry]
otlp = "http://127.0.0.1:4318"

[limits]
connect_within_ms = 30000
drain_ms = 2000
body_bytes = 4096
request_ms = 5000
keep_alive_ms = 20000

[agents.front-desk]
instructions = "You answer for a bakery, briefly."
greeting = "Good morning."

[agents.front-desk.llm]
model = "a-model"

[agents.night_desk.speech]
language = "es"

[telephony]
public = "https://voice.example:8443/"

[telephony.twilio]
agent = "night_desk"
token_env = "A_CARRIER_TOKEN"
account = "AC00000000000000000000000000000001"
signed_streams = true
"#;
        let config = Config::from_toml(text).unwrap();
        // Written with a stroke at its end or without, it is one
        // address.
        let calls = &config.telephony;
        assert_eq!(calls.public.as_deref(), Some("https://voice.example:8443"));
        assert_eq!(calls.authority(), Some("voice.example:8443"));
        assert!(calls.answers());
        let twilio = calls.twilio.as_ref().unwrap();
        assert_eq!(
            (twilio.agent.as_str(), twilio.token_env.as_str()),
            ("night_desk", "A_CARRIER_TOKEN")
        );
        assert_eq!(
            twilio.account.as_deref(),
            Some("AC00000000000000000000000000000001")
        );
        assert!(twilio.signed_streams);
        assert_eq!(config.listen.port(), 8443);
        assert_eq!(config.hosts, ["voice.example"]);
        assert_eq!(config.control.key_env.len(), 2);
        assert!(config.control.show_tool_text);
        assert_eq!(config.limits.drain_ms, 2_000);
        // In the order of the shards.
        assert_eq!(config.processors, [6, 7, 2, 3]);
        let stack = config.stack();
        assert_eq!(stack.shards, 4);
        assert_eq!(stack.pinning, None);
        assert_eq!(
            stack.keeps,
            Keeps::File(PathBuf::from("/var/lib/waveport/sessions.redb"))
        );
        assert_eq!(stack.state_every, 64);
        assert_eq!(stack.otlp.as_deref(), Some("http://127.0.0.1:4318"));
        let names: Vec<&str> = config.agents.keys().map(String::as_str).collect();
        assert_eq!(names, ["front-desk", "night_desk"]);
        assert!(config.agents["night_desk"].speech.is_some());
    }

    #[test]
    fn a_daemon_that_is_told_of_no_calls_answers_none() {
        let config = Config::from_toml("[agents.a]\n").unwrap();
        assert_eq!(config.telephony, Telephony::default());
        assert_eq!(config.telephony.authority(), None);
        assert!(!config.telephony.answers());
        // Where carriers reach a daemon may be said before any carrier
        // is: it answers no calls yet.
        let reached = "[telephony]\npublic = \"https://voice.example\"\n[agents.a]\n";
        let config = Config::from_toml(reached).unwrap();
        assert_eq!(config.telephony.authority(), Some("voice.example"));
        assert!(!config.telephony.answers());
        // What is left out of what is said of a carrier.
        let speaks = "[agents.a.speech]\nlanguage = \"en\"\n";
        let least = format!(
            "{reached}[telephony.twilio]\nagent = \"a\"\ntoken_env = \"A_CARRIER_TOKEN\"\n{speaks}"
        );
        let config = Config::from_toml(&least).unwrap();
        assert!(config.telephony.answers());
        let twilio = config.telephony.twilio.unwrap();
        assert_eq!((twilio.account, twilio.signed_streams), (None, false));
    }

    #[test]
    fn the_address_carriers_reach_a_daemon_at_is_a_name_and_perhaps_a_port() {
        for good in [
            "voice.example",
            "voice.example:8443",
            "a-b.example",
            "localhost",
            "127.0.0.1:8080",
            "voice.example:65535",
            "[::1]",
            "[::1]:8443",
            "[2001:db8::1]:443",
        ] {
            assert!(is_an_authority(good), "{good}");
        }
        for bad in [
            "",
            ":443",
            "voice.example:",
            "voice.example:0",
            "voice.example:65536",
            "voice.example:port",
            "voice.example:1:2",
            "voice.example/calls",
            "voice.example?x",
            "voice.example#x",
            "user@voice.example",
            "voice example",
            ".example",
            "-example",
            "::1",
            "[::1",
            "[]",
            "[]:443",
            "[::1]x",
            "[[::1]]",
            "[voice.example]",
        ] {
            assert!(!is_an_authority(bad), "{bad}");
        }
    }

    #[test]
    fn calls_are_answered_by_an_agent_that_speaks_at_an_address_that_is_said() {
        let speaks = "[agents.a.speech]\nlanguage = \"en\"\n";
        let twilio = "[telephony.twilio]\nagent = \"a\"\ntoken_env = \"A_CARRIER_TOKEN\"\n";
        let at = |public: &str| format!("[telephony]\npublic = \"{public}\"\n");
        let good = format!("{}{twilio}{speaks}", at("https://voice.example"));
        assert!(Config::from_toml(&good).is_ok());
        let nameless = "[telephony.twilio]\nagent = \"a\"\ntoken_env = \" \"\n";
        for (text, why) in [
            // Nowhere, and somewhere a carrier's stream does not reach.
            (
                format!("{twilio}{speaks}"),
                "`telephony.public` does not say",
            ),
            (
                format!("{}{twilio}{speaks}", at("http://voice.example")),
                "`telephony.public` is the address",
            ),
            (
                format!("{}{twilio}{speaks}", at("voice.example")),
                "`telephony.public` is the address",
            ),
            (
                format!("{}{twilio}{speaks}", at("https://voice.example/calls")),
                "`telephony.public` is the address",
            ),
            (
                format!("{}{twilio}{speaks}", at("https://voice.example//")),
                "`telephony.public` is the address",
            ),
            (
                format!("{}[agents.a]\n", at("https://")),
                "`telephony.public` is the address",
            ),
            // An agent that only reads and writes, and one there is
            // none of.
            (
                format!("{}{twilio}[agents.a]\n", at("https://voice.example")),
                "the agent a answers calls and neither hears nor speaks",
            ),
            (
                format!("{}{twilio}[agents.b]\n", at("https://voice.example")),
                "there is no agent of that name",
            ),
            // No variable for what the carrier signs with.
            (
                format!("{}{nameless}{speaks}", at("https://voice.example")),
                "`telephony.twilio.token_env` names",
            ),
        ] {
            let Err(failure) = Config::from_toml(&text) else {
                panic!("{text:?} was taken");
            };
            assert!(failure.0.contains(why), "{text:?}: {failure}");
        }
    }

    #[test]
    fn what_cannot_be_is_refused_and_what_was_written_is_not_repeated() {
        // A made-up key, where the name of its variable belongs and
        // where nothing belongs.
        let secret = "sk-not-a-key";
        for (text, why) in [
            ("", "has no agent"),
            ("[agents.Front]\n", "cannot be the name of an agent"),
            ("[agents.-x]\n", "cannot be the name of an agent"),
            (
                "[control]\nkey_env = [\"A\", \"B\", \"C\"]\n[agents.a]\n",
                "more than two",
            ),
            ("shards = 0\n[agents.a]\n", "one at least"),
            (
                "shards = 2\nprocessors = [3]\n[agents.a]\n",
                "`processors` names 1 for 2 `shards`",
            ),
            (
                "processors = [3, 4]\n[agents.a]\n",
                "`processors` names 2 for 1 `shards`",
            ),
            (
                "shards = 3\nprocessors = [3, 1, 3]\n[agents.a]\n",
                "names processor 3 for two shards",
            ),
            ("processors = [-1]\n[agents.a]\n", "at line 1"),
            ("[limits]\nkeep_alive_ms = 0\n[agents.a]\n", "one at least"),
            (
                "[agents.a]\ninstructions = \"x\"\ninstructions_file = \"y\"\n",
                "the agent a:",
            ),
            (
                "[control]\nkey = \"sk-not-a-key\"\n[agents.a]\n",
                "at line 2",
            ),
            ("[agents.a.llm]\nkey = \"sk-not-a-key\"\n", "at line 2"),
            ("listen = \"sk-not-a-key\"\n[agents.a]\n", "at line 1"),
            (
                "[telephony.twilio]\nagent = \"a\"\ntoken = \"sk-not-a-key\"\n[agents.a]\n",
                "at line 3",
            ),
        ] {
            let Err(failure) = Config::from_toml(text) else {
                panic!("{text:?} was taken");
            };
            assert!(failure.0.contains(why), "{text:?}: {failure}");
            assert!(!failure.0.contains(secret), "{failure}");
        }
    }
}
