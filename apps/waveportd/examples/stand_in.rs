//! The daemon with stand-ins for its providers.
//!
//! ```sh
//! cargo xtask fetch-assets
//! cargo run -p waveportd --example stand_in -- --listen 127.0.0.1:8080
//! ```
//!
//! runs a daemon whose one agent, `desk`, hears and speaks with nobody's
//! key and nothing beyond this machine. The recogniser, the language
//! model and the synthesiser are servers this program starts, which say
//! what they are scripted to (`waveport_testkit::stand_in`). The daemon
//! is the real one, with the models that listen: it hears when somebody
//! speaks and when they have finished.
//!
//! Every session goes the same way. The agent greets whoever comes on
//! its line with "Good morning.". Whatever is then said or written, the
//! recogniser hears "What time is it right now?", the agent answers "It
//! is ten past three. Goodbye!", and ends the call. Its voice is a
//! tone. The stand-ins have answers for as many sessions as
//! `--sessions` says, one after another, and none after that.
//!
//! It is for trying the page the daemon serves without a key, and for
//! the tests of the JavaScript client, which run a browser against it.
//! `--question` writes the question as a WAV file, which a browser can
//! be given in place of a microphone: three seconds of nothing, in
//! which the agent greets, and then the question.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use waveport_adapter_kit::wav;
use waveport_assembly::{Context as _, Failure};
use waveport_ports::audio::Rate;
use waveport_testkit::servers::{HttpServer, WsServer, WsStep};
use waveport_testkit::speech;
use waveport_testkit::stand_in::{GREETING, HEARD, SAID, answer, greeted, spoken};
use waveportd::{Config, Daemon, Threads};

/// An environment variable nobody sets: the stand-ins are asked with no
/// key, whatever keys the environment holds.
const NO_KEY: &str = "WAVEPORT_STAND_IN_KEY";

/// Runs the daemon with stand-ins for its providers.
#[derive(Debug, Parser)]
#[command(name = "stand_in")]
struct Cli {
    /// The address the daemon listens on. Port 0 takes one that is free.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    /// How many sessions the stand-ins have answers for.
    #[arg(long, default_value_t = 8)]
    sessions: usize,
    /// Writes the question the recogniser hears to this file, as a WAV
    /// at 48 kHz: the time a greeting takes of nothing, the question,
    /// and a moment of nothing after it.
    #[arg(long, value_name = "FILE")]
    question: Option<PathBuf>,
    /// More of the daemon's configuration, as the lines of TOML that
    /// come before its agents.
    #[arg(long, value_name = "TOML", default_value = "")]
    more: String,
}

/// Where the models that listen are.
fn assets() -> PathBuf {
    std::env::var_os("WAVEPORT_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/assets"),
        PathBuf::from,
    )
}

/// The stand-ins, which answer for as long as they are held.
struct Providers {
    stt: WsServer,
    llm: HttpServer,
    tts: WsServer,
}

impl Providers {
    async fn start(sessions: usize) -> Self {
        // A session asks the model once: the answer comes with the call
        // of the tool that ends the session, and nothing is asked after
        // that.
        let answers = (0..sessions)
            .map(|_| answer(&SAID, Some("end_call")))
            .collect();
        // The daemon keeps its connection to the synthesiser from one
        // session to the next, and may open another: each connection has
        // a voice for every session, which greets and then answers.
        let voices: Vec<WsStep> = (0..sessions)
            .flat_map(|_| [greeted(), spoken()].concat())
            .collect();
        Self {
            stt: WsServer::start(vec![WsStep::script(HEARD); sessions]).await,
            llm: HttpServer::start(answers).await,
            tts: WsServer::start(vec![voices; sessions]).await,
        }
    }

    /// The configuration of a daemon whose agent is made of these.
    fn config(&self, cli: &Cli) -> String {
        format!(
            "listen = \"{}\"\n\
             assets = '{}'\n\
             {}\n\
             [agents.desk]\n\
             greeting = \"{GREETING}\"\n\
             [agents.desk.llm]\n\
             url = \"{}\"\n\
             model = \"a-model\"\n\
             key_env = \"{NO_KEY}\"\n\
             [agents.desk.speech.stt]\n\
             provider = \"openai\"\n\
             url = \"{}\"\n\
             key_env = \"{NO_KEY}\"\n\
             [agents.desk.speech.tts]\n\
             provider = \"cartesia\"\n\
             url = \"{}\"\n\
             key_env = \"{NO_KEY}\"\n",
            cli.listen,
            assets().display(),
            cli.more,
            self.llm.url("/v1"),
            self.stt.url("/v1/realtime"),
            self.tts.url("/tts/websocket"),
        )
    }
}

/// Returns when the program is told to stop.
async fn told_to_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut asked) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = asked.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn run(cli: &Cli) -> Result<ExitCode, Failure> {
    if let Some(file) = &cli.question {
        let rate = Rate::Hz48000;
        // The agent greets for less than half a second once its far
        // end is on the line. Whoever asks waits it out, as a person
        // does, and a browser that is given this for a microphone has
        // the time it takes to connect in it too.
        let question = speech::framed(rate, 3_000, &speech::what_time_is_it(rate), 600);
        std::fs::write(file, wav::write(rate.hz(), &question))
            .with(|| format!("{} cannot be written", file.display()))?;
    }
    let providers = Providers::start(cli.sessions).await;
    let config = Config::from_toml(&providers.config(cli))?;
    // Nothing is kept on a processor of its own here.
    let threads = Threads::place(&config)?;
    let running = Daemon::start(&config, &threads).await?;
    for note in running.notes() {
        eprintln!("waveportd: {note}");
    }
    println!("waveportd: listening on http://{}", running.address());
    told_to_stop().await;
    for note in running.drain().await {
        eprintln!("waveportd: {note}");
    }
    eprintln!("waveportd: stopped");
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build();
    let done = match runtime {
        Ok(runtime) => {
            let done = runtime.block_on(run(&cli));
            runtime.shutdown_background();
            done
        }
        Err(error) => Err(Failure(format!("nothing can be run: {error}"))),
    };
    match done {
        Ok(code) => code,
        Err(Failure(failure)) => {
            eprintln!("stand_in: {failure}");
            ExitCode::FAILURE
        }
    }
}
