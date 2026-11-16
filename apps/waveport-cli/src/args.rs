//! What the command is told on its command line.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Where OpenAI transcribes speech as it is spoken.
pub(crate) const STT_URL: &str = "wss://api.openai.com/v1/realtime";

/// Where OpenAI's language models are.
pub(crate) const LLM_URL: &str = "https://api.openai.com/v1";

/// Where Cartesia synthesises speech.
pub(crate) const TTS_URL: &str = "wss://api.cartesia.ai/tts/websocket";

/// What an agent is told when it is told nothing else.
pub(crate) const INSTRUCTIONS: &str = "You are a friendly assistant on a voice call. What you \
write is spoken aloud, so answer in one or two short sentences of plain speech, with no lists, \
no symbols and no formatting. Use get_time when you are asked the time or the date. When the \
caller says goodbye or asks to end the call, say goodbye and call end_call.";

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
    /// List the sound devices of this machine.
    Devices,
}

#[derive(Debug, Args)]
pub(crate) struct Dev {
    #[command(flatten)]
    pub agent: Agent,
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
}

#[derive(Debug, Args)]
pub(crate) struct Run {
    #[command(flatten)]
    pub agent: Agent,
    /// The script the caller follows.
    #[arg(long, value_name = "FILE")]
    pub script: PathBuf,
    /// Where to write what the caller was played, as a WAV file.
    #[arg(long, value_name = "FILE")]
    pub record: Option<PathBuf>,
}

/// The agent, and the providers it is made of.
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
    /// The language of the call, as a tag such as `en` or `es`.
    #[arg(long, value_name = "TAG", default_value = "en")]
    pub language: String,
    /// Where the models that listen are: what `cargo xtask fetch-assets`
    /// fetched.
    #[arg(
        long,
        value_name = "DIR",
        env = "WAVEPORT_ASSETS",
        default_value = "target/assets"
    )]
    pub assets: PathBuf,

    /// Where speech is recognised: OpenAI's realtime transcription, or a
    /// server that speaks as it does.
    #[arg(long, value_name = "URL", default_value = STT_URL)]
    pub stt_url: String,
    /// The model that recognises speech.
    #[arg(long, value_name = "MODEL", default_value = "gpt-live-transcribe")]
    pub stt_model: String,
    /// The environment variable that holds the key for it.
    #[arg(long, value_name = "VARIABLE", default_value = "OPENAI_API_KEY")]
    pub stt_key_env: String,

    /// Where the language model is: OpenAI, or any server that speaks
    /// the chat completions protocol.
    #[arg(long, value_name = "URL", default_value = LLM_URL)]
    pub llm_url: String,
    /// The language model.
    #[arg(long, value_name = "MODEL", default_value = "gpt-6-luna")]
    pub llm_model: String,
    /// The environment variable that holds the key for it.
    #[arg(long, value_name = "VARIABLE", default_value = "OPENAI_API_KEY")]
    pub llm_key_env: String,

    /// Where speech is synthesised: Cartesia, or a server that speaks as
    /// it does.
    #[arg(long, value_name = "URL", default_value = TTS_URL)]
    pub tts_url: String,
    /// The model that synthesises speech.
    #[arg(long, value_name = "MODEL", default_value = "sonic-3.6")]
    pub tts_model: String,
    /// The voice.
    #[arg(
        long,
        value_name = "ID",
        default_value = "228fca29-3a0a-435c-8728-5cb483251068"
    )]
    pub tts_voice: String,
    /// The environment variable that holds the key for it.
    #[arg(long, value_name = "VARIABLE", default_value = "CARTESIA_API_KEY")]
    pub tts_key_env: String,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command, LLM_URL};

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
        assert_eq!(dev.agent.language, "en");
        assert!(!dev.half_duplex && dev.microphone.is_none());
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
