//! An agent, and what the agents of a process share, as they are written
//! down.
//!
//! An [`AgentSpec`] can be read from TOML:
//!
//! ```toml
//! instructions = "You answer the telephone of a bakery, briefly."
//! greeting = "Good morning. What can I do for you?"
//!
//! [llm]
//! model = "gpt-6-luna"
//!
//! # With this table the agent hears and speaks where a line carries
//! # audio. Without it the agent only reads and writes.
//! [speech]
//! language = "en"
//!
//! [speech.stt]
//! provider = "deepgram"
//!
//! [speech.tts]
//! provider = "cartesia"
//! ```
//!
//! Everything has a default but the choice to speak. A key is named by
//! the environment variable that holds it (`key_env`), and is never
//! written down itself.

use std::path::PathBuf;

use serde::Deserialize;

use crate::Failure;

/// Where OpenAI's language models are.
pub const LLM_URL: &str = "https://api.openai.com/v1";

/// The language model an agent thinks with when it names none.
pub const LLM_MODEL: &str = "gpt-6-luna";

/// The environment variable that holds the key for it.
pub const LLM_KEY_ENV: &str = "OPENAI_API_KEY";

/// The language of an agent that names none.
pub const LANGUAGE: &str = "en";

/// What an agent that speaks is told when it is told nothing else.
pub const INSTRUCTIONS: &str = "You are a friendly assistant on a voice call. What you \
write is spoken aloud, so answer in one or two short sentences of plain speech, with no lists, \
no symbols and no formatting. Use get_time when you are asked the time or the date. When the \
caller says goodbye or asks to end the call, say goodbye and call end_call.";

/// What an agent that writes is told when it is told nothing else.
pub const WRITTEN: &str = "You are a friendly assistant in a chat. Answer in one or two \
short sentences of plain text, with no lists and no formatting. Use get_time when you are asked \
the time or the date. When the user says goodbye or asks to end the chat, say goodbye and call \
end_call.";

/// Where sessions are kept when nothing else is said: in the folder the
/// program is run in.
pub const JOURNAL: &str = ".waveport/journal.redb";

/// Where the models that listen are when nothing else is said: where
/// `cargo xtask fetch-assets` puts them.
pub const ASSETS: &str = "target/assets";

/// How many records of a session are written before all that it has
/// come to is stored in one piece, when nothing else is said.
pub const STATE_EVERY: u64 = 256;

/// What a provider is asked for when a definition says nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usual {
    /// Where the provider listens.
    pub url: &'static str,
    /// The model.
    pub model: &'static str,
    /// The voice, of a provider that speaks.
    pub voice: &'static str,
    /// The environment variable that holds the key.
    pub key_env: &'static str,
}

/// Who recognises speech.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hears {
    /// OpenAI's realtime transcription.
    #[default]
    OpenAi,
    /// Deepgram's live transcription.
    Deepgram,
    /// AssemblyAI's streaming transcription.
    AssemblyAi,
}

impl Hears {
    /// What the provider is asked for when nothing else is said.
    #[must_use]
    pub const fn usual(self) -> Usual {
        match self {
            Self::OpenAi => Usual {
                url: "wss://api.openai.com/v1/realtime",
                model: "gpt-live-transcribe",
                voice: "",
                key_env: "OPENAI_API_KEY",
            },
            Self::Deepgram => Usual {
                url: "wss://api.deepgram.com/v1/listen",
                model: "nova-3",
                voice: "",
                key_env: "DEEPGRAM_API_KEY",
            },
            Self::AssemblyAi => Usual {
                url: "wss://streaming.assemblyai.com/v3/ws",
                model: "universal-3-6-pro",
                voice: "",
                key_env: "ASSEMBLYAI_API_KEY",
            },
        }
    }
}

/// Who synthesises speech.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaks {
    /// Cartesia.
    #[default]
    Cartesia,
    /// ElevenLabs.
    ElevenLabs,
}

impl Speaks {
    /// What the provider is asked for when nothing else is said.
    #[must_use]
    pub const fn usual(self) -> Usual {
        match self {
            Self::Cartesia => Usual {
                url: "wss://api.cartesia.ai/tts/websocket",
                model: "sonic-3.6",
                voice: "228fca29-3a0a-435c-8728-5cb483251068",
                key_env: "CARTESIA_API_KEY",
            },
            Self::ElevenLabs => Usual {
                url: "wss://api.elevenlabs.io/v1/text-to-speech",
                model: "eleven_flash_v2_5",
                voice: "Xb7hH8MSUJpSbSDYk0k2",
                key_env: "ELEVENLABS_API_KEY",
            },
        }
    }
}

/// A provider as an agent uses it: what its definition says, and for the
/// rest what is usual.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    /// Where the provider listens.
    pub url: String,
    /// The model.
    pub model: String,
    /// The voice, of a provider that speaks.
    pub voice: String,
    /// The environment variable that holds the key.
    pub key_env: String,
    /// Whether the address is the provider's own, which gives nothing
    /// away without a key.
    pub own: bool,
}

impl Chosen {
    fn new(
        usual: Usual,
        url: Option<&String>,
        model: Option<&String>,
        key: Option<&String>,
    ) -> Self {
        let or = |said: Option<&String>, usual: &str| {
            said.map_or_else(|| usual.to_owned(), Clone::clone)
        };
        let url = or(url, usual.url);
        Self {
            own: url == usual.url,
            url,
            model: or(model, usual.model),
            voice: usual.voice.to_owned(),
            key_env: or(key, usual.key_env),
        }
    }
}

/// The language model an agent thinks with.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmSpec {
    /// Where it is: OpenAI, or any server that speaks the chat
    /// completions protocol.
    pub url: String,
    /// The model.
    pub model: String,
    /// The environment variable that holds the key for it.
    pub key_env: String,
}

impl Default for LlmSpec {
    fn default() -> Self {
        Self {
            url: LLM_URL.to_owned(),
            model: LLM_MODEL.to_owned(),
            key_env: LLM_KEY_ENV.to_owned(),
        }
    }
}

/// The provider that recognises speech.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SttSpec {
    /// Who it is.
    pub provider: Hears,
    /// Where speech is recognised, if not at the provider itself: a
    /// server that speaks as the provider does.
    pub url: Option<String>,
    /// The model, if not the usual one of the provider.
    pub model: Option<String>,
    /// The environment variable that holds the key, if not the usual
    /// one of the provider.
    pub key_env: Option<String>,
}

impl SttSpec {
    /// The provider as the agent uses it.
    #[must_use]
    pub fn chosen(&self) -> Chosen {
        Chosen::new(
            self.provider.usual(),
            self.url.as_ref(),
            self.model.as_ref(),
            self.key_env.as_ref(),
        )
    }
}

/// The provider that synthesises speech.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtsSpec {
    /// Who it is.
    pub provider: Speaks,
    /// Where speech is synthesised, if not at the provider itself: a
    /// server that speaks as the provider does.
    pub url: Option<String>,
    /// The model, if not the usual one of the provider.
    pub model: Option<String>,
    /// The voice, by the provider's name for it, if not one from the
    /// provider's own examples.
    pub voice: Option<String>,
    /// The environment variable that holds the key, if not the usual
    /// one of the provider.
    pub key_env: Option<String>,
}

impl TtsSpec {
    /// The provider as the agent uses it.
    #[must_use]
    pub fn chosen(&self) -> Chosen {
        let chosen = Chosen::new(
            self.provider.usual(),
            self.url.as_ref(),
            self.model.as_ref(),
            self.key_env.as_ref(),
        );
        Chosen {
            voice: self.voice.clone().unwrap_or(chosen.voice.clone()),
            ..chosen
        }
    }
}

/// What an agent hears and speaks with.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpeechSpec {
    /// The language of its calls, as a tag such as `en` or `es`.
    pub language: String,
    /// Who recognises speech.
    pub stt: SttSpec,
    /// Who synthesises it.
    pub tts: TtsSpec,
}

impl Default for SpeechSpec {
    fn default() -> Self {
        Self {
            language: LANGUAGE.to_owned(),
            stt: SttSpec::default(),
            tts: TtsSpec::default(),
        }
    }
}

/// An agent: what it is told, the model it thinks with, and what it
/// hears and speaks with, if it does either.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    /// What the agent is told before a session. An agent that is told
    /// nothing is told what suits one that speaks ([`INSTRUCTIONS`]) or
    /// one that writes ([`WRITTEN`]), by how the session is run.
    pub instructions: Option<String>,
    /// A file that holds what the agent is told, in place of
    /// `instructions`.
    pub instructions_file: Option<PathBuf>,
    /// Something for the agent to say when a session begins.
    pub greeting: Option<String>,
    /// The language model it thinks with.
    #[serde(default)]
    pub llm: LlmSpec,
    /// What it hears and speaks with. An agent without is one that only
    /// reads and writes.
    pub speech: Option<SpeechSpec>,
}

impl AgentSpec {
    /// Reads an agent from TOML.
    ///
    /// # Errors
    ///
    /// The text is not TOML, holds something an agent's definition does
    /// not have, or says where the instructions are in two ways. What
    /// the text held is not repeated: somebody may have written a key
    /// into it.
    pub fn from_toml(text: &str) -> Result<Self, Failure> {
        read_toml::<Self>(text, "the agent's definition")?.checked()
    }

    /// The agent, if what it says holds together.
    ///
    /// # Errors
    ///
    /// The definition says where the instructions are in two ways.
    pub fn checked(self) -> Result<Self, Failure> {
        if self.instructions.is_some() && self.instructions_file.is_some() {
            return Err(Failure(
                "the agent's definition has both instructions and a file of instructions"
                    .to_owned(),
            ));
        }
        Ok(self)
    }
}

/// Where the sessions of a process are kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Keeps {
    /// In the usual file, [`JOURNAL`]. A process that finds it open in
    /// another does without, unless it has to go on with a session that
    /// is in it.
    #[default]
    Usual,
    /// In this file.
    File(PathBuf),
    /// In memory only: a session that broke off is gone.
    Memory,
}

/// Reads `text` as the TOML of a `T`, which is called `what` in a
/// failure.
///
/// # Errors
///
/// The text is not TOML, or not that of a `T`. The failure says what is
/// wrong and at which line, and does not repeat what the text held:
/// somebody may have written a key into it, and the parser's own message
/// quotes the line.
pub fn read_toml<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T, Failure> {
    toml::from_str(text).map_err(|error| {
        let line = error.span().map_or(0, |span| {
            let before = text.get(..span.start).unwrap_or(text);
            before.bytes().filter(|byte| *byte == b'\n').count() + 1
        });
        Failure(format!(
            "{what} cannot be read, at line {line}: {}",
            error.message()
        ))
    })
}

/// What the settings of a stack are called where they were given, so
/// that a failure about one of them says what to change: an option of a
/// command line, or a line of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Named {
    /// The setting that names the file sessions are kept in.
    pub journal: &'static str,
    /// The setting that keeps sessions in memory.
    pub memory: &'static str,
    /// The setting that says where the models that listen are.
    pub assets: &'static str,
    /// The setting that says where traces and metrics are sent.
    pub otlp: &'static str,
    /// What an agent was defined by, as in "as the command line says".
    pub by: &'static str,
}

/// What the agents of one process share, apart from what is built: where
/// sessions are kept, where the models that listen are, and where traces
/// and metrics go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackSpec {
    /// How many threads sessions run on. Each session stays on the one
    /// it was started on.
    pub shards: usize,
    /// Where sessions are kept.
    pub keeps: Keeps,
    /// How many records of a session are written before all that it has
    /// come to is stored in one piece.
    pub state_every: u64,
    /// A file for the agents to write notes in. With it they have a tool
    /// that adds a line to the file.
    pub notes: Option<PathBuf>,
    /// The OpenTelemetry collector that traces and metrics of the
    /// sessions are sent to, as OTLP over HTTP.
    pub otlp: Option<String>,
    /// Where the models that listen are.
    pub assets: PathBuf,
    /// What these settings are called where they were given.
    pub named: Named,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{AgentSpec, Hears, LLM_KEY_ENV, LLM_MODEL, LLM_URL, Speaks, SpeechSpec};

    #[test]
    fn an_agent_that_says_nothing_writes_with_the_usual_model() {
        let agent = AgentSpec::from_toml("").unwrap();
        assert_eq!(agent, AgentSpec::default());
        assert_eq!(agent.llm.url, LLM_URL);
        assert_eq!(agent.llm.model, LLM_MODEL);
        assert_eq!(agent.llm.key_env, LLM_KEY_ENV);
        assert!(agent.speech.is_none() && agent.instructions.is_none());
    }

    #[test]
    fn an_agent_speaks_when_its_definition_says_so() {
        // The table alone is the choice: the rest is what is usual.
        let agent = AgentSpec::from_toml("[speech]\n").unwrap();
        assert_eq!(agent.speech, Some(SpeechSpec::default()));
        let speech = agent.speech.unwrap();
        assert_eq!(speech.language, "en");
        let hears = speech.stt.chosen();
        assert_eq!(hears.url, "wss://api.openai.com/v1/realtime");
        assert_eq!(hears.key_env, "OPENAI_API_KEY");
        assert!(hears.own);
        assert_eq!(speech.tts.chosen().key_env, "CARTESIA_API_KEY");
    }

    #[test]
    fn a_definition_is_read_as_it_is_written() {
        let agent = AgentSpec::from_toml(
            r#"
            instructions_file = "bakery.txt"
            greeting = "Good morning."

            [llm]
            url = "http://127.0.0.1:8080/v1"
            model = "small"
            key_env = "LOCAL_KEY"

            [speech]
            language = "es"

            [speech.stt]
            provider = "deepgram"
            model = "nova-3-medical"

            [speech.tts]
            provider = "elevenlabs"
            voice = "someone"
            url = "ws://127.0.0.1:9/tts"
            "#,
        )
        .unwrap();
        assert_eq!(agent.instructions_file, Some(PathBuf::from("bakery.txt")));
        assert_eq!(agent.greeting.as_deref(), Some("Good morning."));
        assert_eq!(agent.llm.url, "http://127.0.0.1:8080/v1");
        assert_eq!(agent.llm.key_env, "LOCAL_KEY");
        let speech = agent.speech.unwrap();
        assert_eq!(speech.language, "es");
        assert_eq!(
            (speech.stt.provider, speech.tts.provider),
            (Hears::Deepgram, Speaks::ElevenLabs)
        );

        // Another provider brings its own address, model, voice and the
        // name of the variable that holds its key, and what the
        // definition says stands.
        let hears = speech.stt.chosen();
        assert_eq!(hears.url, "wss://api.deepgram.com/v1/listen");
        assert_eq!(
            (hears.model.as_str(), hears.key_env.as_str()),
            ("nova-3-medical", "DEEPGRAM_API_KEY")
        );
        assert!(hears.own);
        let speaks = speech.tts.chosen();
        assert_eq!(speaks.url, "ws://127.0.0.1:9/tts");
        assert_eq!(speaks.model, "eleven_flash_v2_5");
        assert_eq!(speaks.voice, "someone");
        assert_eq!(speaks.key_env, "ELEVENLABS_API_KEY");
        // An address that is not the provider's own may need no key.
        assert!(!speaks.own);
    }

    #[test]
    fn every_provider_has_what_is_usual_for_it() {
        let third = AgentSpec::from_toml("[speech.stt]\nprovider = \"assemblyai\"\n").unwrap();
        let hears = third.speech.unwrap().stt.chosen();
        assert_eq!(hears.model, "universal-3-6-pro");
        assert_eq!(hears.key_env, "ASSEMBLYAI_API_KEY");
        for provider in [Hears::OpenAi, Hears::Deepgram, Hears::AssemblyAi] {
            let usual = provider.usual();
            assert!(usual.url.starts_with("wss://") && !usual.model.is_empty());
            assert!(usual.key_env.ends_with("_API_KEY"));
        }
        for provider in [Speaks::Cartesia, Speaks::ElevenLabs] {
            let usual = provider.usual();
            assert!(usual.url.starts_with("wss://") && !usual.voice.is_empty());
        }
    }

    #[test]
    fn a_definition_that_holds_something_else_is_refused_and_not_repeated() {
        // Somebody wrote the key where its variable's name belongs.
        let secret = "sk-not-a-real-key-0123456789";
        let text = format!("greeting = \"Hello.\"\n\n[llm]\nkey = \"{secret}\"\n");
        let failure = AgentSpec::from_toml(&text).unwrap_err();
        assert!(failure.0.contains("line 4"), "{failure}");
        assert!(failure.0.contains("unknown field `key`"), "{failure}");
        assert!(!failure.0.contains(secret), "{failure}");

        // A value of the wrong kind is not repeated either.
        let text = format!("[llm]\nkey_env = [\"{secret}\"]\n");
        let failure = AgentSpec::from_toml(&text).unwrap_err();
        assert!(failure.0.contains("line 2"), "{failure}");
        assert!(!failure.0.contains(secret), "{failure}");

        // Nor is text that is not TOML at all.
        let failure = AgentSpec::from_toml(&format!("{secret} {secret}\n")).unwrap_err();
        assert!(!failure.0.contains(secret), "{failure}");

        let unknown = AgentSpec::from_toml("[speech.stt]\nprovider = \"nobody\"\n").unwrap_err();
        assert!(unknown.0.contains("line 2"), "{unknown}");
    }

    #[test]
    fn instructions_come_from_one_place() {
        let both = "instructions = \"Be brief.\"\ninstructions_file = \"brief.txt\"\n";
        let failure = AgentSpec::from_toml(both).unwrap_err();
        assert!(failure.0.contains("both"), "{failure}");
    }
}
