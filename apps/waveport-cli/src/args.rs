//! What the command is told on its command line.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use waveport_assembly::Resume;
use waveport_assembly::spec::{
    self, ASSETS, AgentSpec, JOURNAL, Keeps, LANGUAGE, LLM_KEY_ENV, LLM_MODEL, LLM_URL, LlmSpec,
    Named, STATE_EVERY, SpeechSpec, StackSpec, SttSpec, TtsSpec,
};

/// Who recognises speech.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Hears {
    /// OpenAI's realtime transcription.
    #[value(name = "openai")]
    OpenAi,
    /// Deepgram's live transcription.
    #[value(name = "deepgram")]
    Deepgram,
    /// AssemblyAI's streaming transcription.
    #[value(name = "assemblyai")]
    AssemblyAi,
}

impl From<Hears> for spec::Hears {
    fn from(hears: Hears) -> Self {
        match hears {
            Hears::OpenAi => Self::OpenAi,
            Hears::Deepgram => Self::Deepgram,
            Hears::AssemblyAi => Self::AssemblyAi,
        }
    }
}

/// Who synthesises speech.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Speaks {
    /// Cartesia.
    #[value(name = "cartesia")]
    Cartesia,
    /// ElevenLabs.
    #[value(name = "elevenlabs")]
    ElevenLabs,
}

impl From<Speaks> for spec::Speaks {
    fn from(speaks: Speaks) -> Self {
        match speaks {
            Speaks::Cartesia => Self::Cartesia,
            Speaks::ElevenLabs => Self::ElevenLabs,
        }
    }
}

/// What the command line calls the settings that a failure may be about.
pub(crate) const SAID: Named = Named {
    journal: "--journal",
    memory: "--no-journal",
    assets: "--assets",
    otlp: "--otlp",
    by: "the command line",
};

/// Where calls are taken over a WebSocket unless told otherwise: on this
/// machine alone.
pub(crate) const LISTEN: &str = "127.0.0.1:9283";

/// Where a client finds an agent that was told nothing about where to
/// take calls.
pub(crate) const CALLS: &str = "ws://127.0.0.1:9283";

/// Voice agents on this machine.
#[derive(Debug, Parser)]
#[command(name = "waveport", version, about, long_about = None)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Talk to an agent through this machine's microphone and loudspeaker.
    ///
    /// Wear headphones: nothing takes the agent's voice out of what the
    /// microphone hears, and an agent that hears itself stops speaking.
    Dev(Dev),
    /// Run a call in which the caller follows a script.
    ///
    /// The script is a text file, one step a line:
    ///
    ///   silence 500         the caller says nothing for 500 ms
    ///   say question.wav    the caller says what the file holds
    ///   listen 800 30000    the caller waits until the agent has spoken
    ///                       and then been silent for 800 ms, for 30 s
    ///                       at most
    ///
    /// Files are named from where the script is. When the script has run
    /// out, the caller hangs up.
    #[command(verbatim_doc_comment)]
    Run(Run),
    /// Take calls over a WebSocket, each from a `waveport client`.
    ///
    /// The agent runs here and whoever calls it is somewhere else: in
    /// another terminal, or on another machine if --exposed says that it
    /// may be. Nothing asks a caller who they are.
    Serve(Serve),
    /// Call an agent that `waveport serve` runs.
    ///
    /// The caller is whoever sits at this machine, at its microphone and
    /// loudspeaker, or follows a script as with `waveport run`.
    Client(Client),
    /// Chat with an agent in writing, at this terminal.
    ///
    /// The agent neither hears nor speaks: it has a language model and
    /// its tools, and nothing else. Each line that is typed is said to
    /// it, and what it writes is shown as it is written. A line typed
    /// while it writes stops it where it is. The chat ends when the
    /// agent ends it, at the end of what is typed (Ctrl-D) or at Ctrl-C.
    ///
    /// What is typed can come from a file or from another command. Each
    /// line then waits for the answer to the one before it.
    ///
    /// A chat is a call in writing: it is kept, gone on with and told as
    /// a call is.
    Chat(Chat),
    /// List the calls that broke off and are kept.
    ///
    /// A call that ended is let go of. One whose command was stopped
    /// where it was, or died, stays in the file calls are kept in until
    /// it is gone on with to its end: `--resume last` goes on with the
    /// one that was begun last, and `--resume FILE` with the one whose
    /// token the file holds.
    Calls(Calls),
    /// List the sound devices of this machine.
    Devices,
}

#[derive(Debug, Args)]
pub(crate) struct Calls {
    /// The file calls are kept in.
    #[arg(long, value_name = "FILE", default_value = JOURNAL)]
    pub journal: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct Dev {
    #[command(flatten)]
    pub agent: Agent,
    #[command(flatten)]
    pub aloud: Aloud,
    /// The microphone, as `waveport devices` names it.
    #[arg(long, value_name = "ID")]
    pub microphone: Option<String>,
    /// The loudspeaker, as `waveport devices` names it.
    #[arg(long, value_name = "ID")]
    pub loudspeaker: Option<String>,
    /// Make the microphone deaf while the agent speaks, for a machine
    /// with no headphones. The agent can then not be spoken over.
    #[arg(long)]
    pub half_duplex: bool,
    /// Go on with a call that broke off: `last` for the one that was
    /// last begun here, or a file that holds the token of another.
    #[arg(long, value_name = "last|FILE", conflicts_with = "no_journal")]
    pub resume: Option<Resume>,
    /// Write what the call came to, in numbers, to this file when it is
    /// over: each turn, and how long each part of the answer to it
    /// took. No word of the call is in it.
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct Run {
    #[command(flatten)]
    pub agent: Agent,
    #[command(flatten)]
    pub aloud: Aloud,
    /// The script the caller follows.
    #[arg(long, value_name = "FILE")]
    pub script: PathBuf,
    /// Where to write what the caller was played, as a WAV file.
    #[arg(long, value_name = "FILE")]
    pub record: Option<PathBuf>,
    /// Go on with a call that broke off: `last` for the one that was
    /// last begun here, or a file that holds the token of another.
    #[arg(long, value_name = "last|FILE", conflicts_with = "no_journal")]
    pub resume: Option<Resume>,
    /// Write what the call came to, in numbers, to this file when it is
    /// over: each turn, and how long each part of the answer to it
    /// took. No word of the call is in it.
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct Serve {
    #[command(flatten)]
    pub agent: Agent,
    #[command(flatten)]
    pub aloud: Aloud,
    /// The address calls are taken at. Port 0 is any port that is free.
    #[arg(long, value_name = "ADDRESS", default_value = LISTEN)]
    pub listen: SocketAddr,
    /// Take calls at an address that other machines can reach.
    #[arg(long)]
    pub exposed: bool,
    /// Stop once this many calls have been taken and have ended.
    #[arg(long, value_name = "NUMBER")]
    pub calls: Option<u32>,
    /// Go on with the calls that broke off when their callers come
    /// back. A caller says which call it was on with the token it was
    /// given as the call began, which `waveport client` does by itself.
    #[arg(long, conflicts_with = "no_journal")]
    pub resume: bool,
}

#[derive(Debug, Args)]
pub(crate) struct Client {
    /// Where the agent takes calls.
    #[arg(long, value_name = "URL", default_value = CALLS)]
    pub url: String,
    /// The microphone, as `waveport devices` names it.
    #[arg(long, value_name = "ID")]
    pub microphone: Option<String>,
    /// The loudspeaker, as `waveport devices` names it.
    #[arg(long, value_name = "ID")]
    pub loudspeaker: Option<String>,
    /// Make the microphone deaf while the agent speaks, for a machine
    /// with no headphones. The agent can then not be spoken over.
    #[arg(long)]
    pub half_duplex: bool,
    /// A script for the caller to follow, in place of the microphone and
    /// the loudspeaker. `waveport run --help` says how one is written.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with_all = ["microphone", "loudspeaker", "half_duplex"]
    )]
    pub script: Option<PathBuf>,
    /// Where to write what the caller was played, as a WAV file.
    #[arg(long, value_name = "FILE", requires = "script")]
    pub record: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct Chat {
    #[command(flatten)]
    pub agent: Agent,
    /// Go on with a chat that broke off: `last` for the one that was
    /// last begun here, or a file that holds the token of another.
    #[arg(long, value_name = "last|FILE", conflicts_with = "no_journal")]
    pub resume: Option<Resume>,
}

/// The agent: what it is told, where its calls are kept, and the model
/// it thinks with. An agent that only writes is made of nothing else.
#[derive(Debug, Args)]
pub(crate) struct Agent {
    /// What the agent is told before the call.
    #[arg(long, value_name = "TEXT", conflicts_with = "instructions_file")]
    pub instructions: Option<String>,
    /// A file that holds what the agent is told before the call.
    #[arg(long, value_name = "FILE")]
    pub instructions_file: Option<PathBuf>,
    /// Something for the agent to say when the call begins.
    #[arg(long, value_name = "TEXT")]
    pub greeting: Option<String>,

    /// The file calls are kept in. A call that broke off, with the
    /// command or with the machine, can be gone on with from it. The
    /// file holds what was said, and is made its owner's alone.
    #[arg(
        long,
        value_name = "FILE",
        help = "The file calls are kept in, from which one that broke off can be gone on with \
                [default: .waveport/journal.redb]"
    )]
    pub journal: Option<PathBuf>,
    /// Keep calls in memory only: one that broke off is gone.
    #[arg(long, conflicts_with = "journal")]
    pub no_journal: bool,
    /// How many records of a call are written before all that the call
    /// has come to is stored in one piece, so that going on with it
    /// need not read every record.
    #[arg(long, value_name = "RECORDS", default_value_t = STATE_EVERY)]
    pub state_every: u64,
    /// A file for the agent to write notes in. With it the agent has a
    /// tool that adds a line to the file.
    #[arg(long, value_name = "FILE")]
    pub notes: Option<PathBuf>,
    /// Send traces and metrics of the calls to the OpenTelemetry
    /// collector at this address, as OTLP over HTTP:
    /// `http://localhost:4318` for one on this machine. No word of a
    /// call is sent. The headers a collector wants, a key among them,
    /// are read from `OTEL_EXPORTER_OTLP_HEADERS`.
    #[arg(long, value_name = "URL")]
    pub otlp: Option<String>,

    /// Where the language model is: OpenAI, or any server that speaks
    /// the chat completions protocol.
    #[arg(long, value_name = "URL", default_value = LLM_URL)]
    pub llm_url: String,
    /// The language model.
    #[arg(long, value_name = "MODEL", default_value = LLM_MODEL)]
    pub llm_model: String,
    /// The environment variable that holds the key for it.
    #[arg(long, value_name = "VARIABLE", default_value = LLM_KEY_ENV)]
    pub llm_key_env: String,
}

/// What an agent that hears and speaks is made of besides: the models
/// that listen, and the providers that recognise and synthesise speech.
#[derive(Debug, Args)]
pub(crate) struct Aloud {
    /// The language of the call, as a tag such as `en` or `es`.
    #[arg(long, value_name = "TAG", default_value = LANGUAGE)]
    pub language: String,
    /// Where the models that listen are: what `cargo xtask fetch-assets`
    /// fetched.
    #[arg(
        long,
        value_name = "DIR",
        env = "WAVEPORT_ASSETS",
        default_value = ASSETS
    )]
    pub assets: PathBuf,

    /// Who recognises speech.
    #[arg(long, value_name = "PROVIDER", value_enum, default_value = "openai")]
    pub stt: Hears,
    /// Where speech is recognised, if not at the provider itself: a
    /// server that speaks as the provider does.
    #[arg(long, value_name = "URL")]
    pub stt_url: Option<String>,
    /// The model that recognises speech.
    #[arg(
        long,
        value_name = "MODEL",
        help = "The model that recognises speech [default: gpt-live-transcribe, nova-3 or \
                universal-3-6-pro, by the provider]"
    )]
    pub stt_model: Option<String>,
    /// The environment variable that holds the key for it.
    #[arg(
        long,
        value_name = "VARIABLE",
        help = "The environment variable that holds the key for it [default: OPENAI_API_KEY, \
                DEEPGRAM_API_KEY or ASSEMBLYAI_API_KEY, by the provider]"
    )]
    pub stt_key_env: Option<String>,

    /// Who synthesises speech.
    #[arg(long, value_name = "PROVIDER", value_enum, default_value = "cartesia")]
    pub tts: Speaks,
    /// Where speech is synthesised, if not at the provider itself: a
    /// server that speaks as the provider does.
    #[arg(long, value_name = "URL")]
    pub tts_url: Option<String>,
    /// The model that synthesises speech.
    #[arg(
        long,
        value_name = "MODEL",
        help = "The model that synthesises speech [default: sonic-3.6 or eleven_flash_v2_5, by \
                the provider]"
    )]
    pub tts_model: Option<String>,
    /// The voice, by the provider's name for it.
    #[arg(
        long,
        value_name = "ID",
        help = "The voice, by the provider's name for it [default: one from the provider's own \
                examples]"
    )]
    pub tts_voice: Option<String>,
    /// The environment variable that holds the key for it.
    #[arg(
        long,
        value_name = "VARIABLE",
        help = "The environment variable that holds the key for it [default: CARTESIA_API_KEY \
                or ELEVENLABS_API_KEY, by the provider]"
    )]
    pub tts_key_env: Option<String>,
}

impl Agent {
    /// The agent as it is defined: what the command line says of it,
    /// with what `aloud` says it hears and speaks with, if it does
    /// either.
    pub(crate) fn spec(&self, aloud: Option<&Aloud>) -> AgentSpec {
        AgentSpec {
            instructions: self.instructions.clone(),
            instructions_file: self.instructions_file.clone(),
            greeting: self.greeting.clone(),
            llm: LlmSpec {
                url: self.llm_url.clone(),
                model: self.llm_model.clone(),
                key_env: self.llm_key_env.clone(),
            },
            speech: aloud.map(Aloud::spec),
        }
    }

    /// What the calls of the command share, as the command line says.
    pub(crate) fn stack(&self, aloud: Option<&Aloud>) -> StackSpec {
        let keeps = match &self.journal {
            _ if self.no_journal => Keeps::Memory,
            Some(file) => Keeps::File(file.clone()),
            None => Keeps::Usual,
        };
        StackSpec {
            shards: 1,
            pinning: None,
            keeps,
            state_every: self.state_every,
            notes: self.notes.clone(),
            otlp: self.otlp.clone(),
            assets: aloud.map_or_else(|| PathBuf::from(ASSETS), |aloud| aloud.assets.clone()),
            named: SAID,
        }
    }
}

impl Aloud {
    /// What the agent hears and speaks with, as it is defined.
    pub(crate) fn spec(&self) -> SpeechSpec {
        SpeechSpec {
            language: self.language.clone(),
            stt: SttSpec {
                provider: self.stt.into(),
                url: self.stt_url.clone(),
                model: self.stt_model.clone(),
                key_env: self.stt_key_env.clone(),
            },
            tts: TtsSpec {
                provider: self.tts.into(),
                url: self.tts_url.clone(),
                model: self.tts_model.clone(),
                voice: self.tts_voice.clone(),
                key_env: self.tts_key_env.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use std::path::PathBuf;

    use waveport_assembly::spec::{Keeps, WRITTEN};

    use super::{Cli, Command, Hears, LLM_URL, Resume, SAID, Speaks};

    #[test]
    fn the_command_line_is_one_clap_can_build() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_call_at_the_desk_needs_nothing_said() {
        let cli = Cli::try_parse_from(["waveport", "dev"]).unwrap();
        let Command::Dev(dev) = cli.command else {
            panic!("dev is dev");
        };
        assert_eq!(dev.agent.llm_url, LLM_URL);
        assert_eq!(dev.aloud.language, "en");
        assert!(!dev.half_duplex && dev.microphone.is_none());
    }

    #[test]
    fn a_provider_is_asked_for_what_is_usual_for_it_unless_told() {
        let agent = |arguments: &[&str]| {
            let all = ["waveport", "dev"].iter().chain(arguments).copied();
            let Command::Dev(dev) = Cli::try_parse_from(all).unwrap().command else {
                panic!("dev is dev");
            };
            dev.aloud
        };
        // Told nothing: the providers of the first voice loop.
        let told_nothing = agent(&[]);
        assert_eq!(
            (told_nothing.stt, told_nothing.tts),
            (Hears::OpenAi, Speaks::Cartesia)
        );
        let hears = told_nothing.spec().stt.chosen();
        assert_eq!(hears.url, "wss://api.openai.com/v1/realtime");
        assert_eq!(hears.key_env, "OPENAI_API_KEY");
        assert!(hears.own);
        assert_eq!(told_nothing.spec().tts.chosen().key_env, "CARTESIA_API_KEY");

        // Another provider brings its own address, model, voice and the
        // name of the variable that holds its key.
        let others = agent(&["--stt", "deepgram", "--tts", "elevenlabs"]);
        let hears = others.spec().stt.chosen();
        assert_eq!(hears.url, "wss://api.deepgram.com/v1/listen");
        assert_eq!(
            (hears.model.as_str(), hears.key_env.as_str()),
            ("nova-3", "DEEPGRAM_API_KEY")
        );
        let speaks = others.spec().tts.chosen();
        assert_eq!(speaks.url, "wss://api.elevenlabs.io/v1/text-to-speech");
        assert_eq!(speaks.model, "eleven_flash_v2_5");
        assert_eq!(speaks.key_env, "ELEVENLABS_API_KEY");
        assert!(speaks.own && !speaks.voice.is_empty());
        let third = agent(&["--stt", "assemblyai"]).spec().stt.chosen();
        assert_eq!(third.model, "universal-3-6-pro");
        assert_eq!(third.key_env, "ASSEMBLYAI_API_KEY");

        // What the command line says stands.
        let told = agent(&[
            "--stt",
            "deepgram",
            "--stt-url",
            "ws://127.0.0.1:9/v1/listen",
            "--stt-model",
            "nova-3-medical",
            "--tts-voice",
            "my-voice",
            "--tts-key-env",
            "MY_KEY",
        ]);
        let hears = told.spec().stt.chosen();
        assert!(!hears.own);
        assert_eq!(hears.model, "nova-3-medical");
        assert_eq!(hears.key_env, "DEEPGRAM_API_KEY");
        let speaks = told.spec().tts.chosen();
        assert_eq!(
            (speaks.voice.as_str(), speaks.key_env.as_str()),
            ("my-voice", "MY_KEY")
        );
        assert_eq!(speaks.model, "sonic-3.6");

        // A provider there is no adapter for is not taken.
        assert!(Cli::try_parse_from(["waveport", "dev", "--stt", "whisper"]).is_err());
    }

    #[test]
    fn a_scripted_call_needs_its_script() {
        assert!(Cli::try_parse_from(["waveport", "run"]).is_err());
        let cli = Cli::try_parse_from([
            "waveport",
            "run",
            "--script",
            "calls/time.call",
            "--llm-url",
            "http://127.0.0.1:9/v1",
        ])
        .unwrap();
        let Command::Run(run) = cli.command else {
            panic!("run is run");
        };
        assert_eq!(run.script.to_str(), Some("calls/time.call"));
        assert_eq!(run.agent.llm_url, "http://127.0.0.1:9/v1");
    }

    #[test]
    fn calls_over_a_websocket_stay_on_this_machine_unless_told() {
        let cli = Cli::try_parse_from(["waveport", "serve"]).unwrap();
        let Command::Serve(serve) = cli.command else {
            panic!("serve is serve");
        };
        assert!(serve.listen.ip().is_loopback() && !serve.exposed);
        assert_eq!(serve.calls, None);
        let cli = Cli::try_parse_from(["waveport", "client"]).unwrap();
        let Command::Client(client) = cli.command else {
            panic!("client is client");
        };
        // A client finds a server that was told nothing, having been told
        // nothing itself.
        assert_eq!(client.url, format!("ws://{}", serve.listen));
        assert!(client.script.is_none());

        // A caller is at the devices or follows a script, not both, and
        // only a scripted one is recorded.
        let both = ["waveport", "client", "--script", "a.call", "--half-duplex"];
        assert!(Cli::try_parse_from(both).is_err());
        assert!(Cli::try_parse_from(["waveport", "client", "--record", "a.wav"]).is_err());
        let scripted = [
            "waveport", "client", "--script", "a.call", "--record", "a.wav",
        ];
        assert!(Cli::try_parse_from(scripted).is_ok());
    }

    #[test]
    fn a_call_is_kept_in_a_file_unless_told_otherwise() {
        let dev = |arguments: &[&str]| {
            let all = ["waveport", "dev"].iter().chain(arguments).copied();
            Cli::try_parse_from(all).map(|cli| match cli.command {
                Command::Dev(dev) => dev,
                _ => panic!("dev is dev"),
            })
        };
        let told_nothing = dev(&[]).unwrap();
        assert!(told_nothing.agent.journal.is_none() && !told_nothing.agent.no_journal);
        assert_eq!(told_nothing.agent.state_every, 256);
        assert!(told_nothing.resume.is_none() && told_nothing.agent.notes.is_none());
        // What the calls share is what the command line says: the usual
        // file, a file that is named, or none.
        let shared = told_nothing.agent.stack(Some(&told_nothing.aloud));
        assert_eq!(shared.keeps, Keeps::Usual);
        assert_eq!(shared.assets, PathBuf::from("target/assets"));
        assert_eq!(shared.named, SAID);
        let named = dev(&["--journal", "calls.redb", "--notes", "notes.txt"]).unwrap();
        let shared = named.agent.stack(Some(&named.aloud));
        assert_eq!(shared.keeps, Keeps::File(PathBuf::from("calls.redb")));
        assert_eq!(shared.notes, Some(PathBuf::from("notes.txt")));
        let nowhere = dev(&["--no-journal"]).unwrap();
        assert_eq!(nowhere.agent.stack(None).keeps, Keeps::Memory);

        // The call that was last begun, or the one a file has the token
        // of. There is nowhere to write a token itself.
        assert_eq!(
            dev(&["--resume", "last"]).unwrap().resume,
            Some(Resume::Last)
        );
        assert_eq!(
            dev(&["--resume", "calls/monday.token"]).unwrap().resume,
            Some(Resume::File(PathBuf::from("calls/monday.token")))
        );

        // A call that is kept nowhere cannot be gone on with, and is not
        // kept in a file as well.
        assert!(dev(&["--no-journal"]).unwrap().agent.no_journal);
        assert!(dev(&["--no-journal", "--resume", "last"]).is_err());
        assert!(dev(&["--no-journal", "--journal", "calls.redb"]).is_err());
        let run = ["waveport", "run", "--script", "a.call", "--resume", "last"];
        assert!(Cli::try_parse_from(run).is_ok());
        // Whoever calls an agent over a WebSocket says which call to go
        // on with, not the agent: the agent is told only that it is to
        // go on with the calls that broke off.
        assert!(Cli::try_parse_from(["waveport", "serve", "--resume", "last"]).is_err());
        let serve = |more: &[&str]| {
            let line = ["waveport", "serve"].iter().chain(more).copied();
            Cli::try_parse_from(line).map(|cli| match cli.command {
                Command::Serve(serve) => serve.resume,
                _ => panic!("serve is serve"),
            })
        };
        assert!(!serve(&[]).unwrap());
        assert!(serve(&["--resume"]).unwrap());
        assert!(serve(&["--resume", "--no-journal"]).is_err());
    }

    #[test]
    fn a_chat_is_told_nothing_of_hearing_or_speaking() {
        let cli = Cli::try_parse_from(["waveport", "chat"]).unwrap();
        let Command::Chat(chat) = cli.command else {
            panic!("chat is chat");
        };
        assert_eq!(chat.agent.llm_url, LLM_URL);
        assert!(chat.resume.is_none() && chat.agent.instructions.is_none());
        // Defined, it is an agent that neither hears nor speaks.
        let defined = chat.agent.spec(None);
        assert!(defined.speech.is_none());
        assert_eq!(defined.llm.model, "gpt-6-luna");
        // What it is told when it is told nothing says nothing of a
        // voice.
        assert!(!WRITTEN.contains("spoken") && !WRITTEN.contains("voice"));

        // It is kept and gone on with as a call is, and takes notes.
        let kept = [
            "waveport",
            "chat",
            "--resume",
            "last",
            "--notes",
            "notes.txt",
        ];
        assert!(Cli::try_parse_from(kept).is_ok());
        let nowhere = ["waveport", "chat", "--no-journal", "--resume", "last"];
        assert!(Cli::try_parse_from(nowhere).is_err());

        // There is no recogniser, synthesiser, model that listens or
        // sound device to say anything about.
        for flag in ["--stt", "--tts", "--assets", "--language", "--microphone"] {
            let told = Cli::try_parse_from(["waveport", "chat", flag, "x"]);
            assert!(told.is_err(), "{flag}");
        }
    }

    #[test]
    fn instructions_come_from_one_place() {
        let both = [
            "waveport",
            "dev",
            "--instructions",
            "Be brief.",
            "--instructions-file",
            "agent.txt",
        ];
        assert!(Cli::try_parse_from(both).is_err());
        // There is nowhere to write a key: only the name of the variable
        // that holds one.
        assert!(Cli::try_parse_from(["waveport", "dev", "--llm-key", "sk-x"]).is_err());
    }
}
