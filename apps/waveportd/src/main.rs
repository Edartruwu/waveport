//! The `waveportd` command.
//!
//! Stability: experimental
//!
//! `waveportd --config waveportd.toml` runs the agents the file defines
//! and listens for whoever asks for a session of one. The file names
//! the environment variables that hold keys, and holds no key itself.
//! Paths in it are from where the command is run.
//!
//! The daemon stops when it is told to (Ctrl-C, or `SIGTERM`): it takes
//! nothing new, lets the answers that are under way come to their end,
//! and tells whoever follows a session that it is going away. Told
//! twice, it stops where it is.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use waveport_assembly::{Context as _, Failure};
use waveportd::{Config, Daemon};

/// Runs agents that are reached over HTTP.
#[derive(Debug, Parser)]
#[command(name = "waveportd", version)]
struct Cli {
    /// The file that says where the daemon listens, who may ask, where
    /// sessions are kept, and what its agents are.
    #[arg(long, short, value_name = "FILE")]
    config: PathBuf,
    /// Reads the file, says what is wrong with it if anything is, and
    /// stops.
    #[arg(long)]
    check: bool,
}

/// Returns when the daemon is told to stop.
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

async fn run(config: &Config) -> Result<ExitCode, Failure> {
    let running = Daemon::start(config).await?;
    for note in running.notes() {
        eprintln!("waveportd: {note}");
    }
    println!("waveportd: listening on http://{}", running.address());
    told_to_stop().await;
    eprintln!("waveportd: stopping. Told again, it stops where it is.");
    tokio::select! {
        notes = running.drain() => {
            for note in notes {
                eprintln!("waveportd: {note}");
            }
            eprintln!("waveportd: stopped");
            Ok(ExitCode::SUCCESS)
        }
        () = told_to_stop() => {
            eprintln!("waveportd: stopped where it was");
            Ok(ExitCode::from(130))
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = std::fs::read_to_string(&cli.config)
        .with(|| format!("{} cannot be read", cli.config.display()))
        .and_then(|text| Config::from_toml(&text));
    let done = config.and_then(|config| {
        if cli.check {
            println!(
                "waveportd: {} holds together: {} agents, listening on {}",
                cli.config.display(),
                config.agents.len(),
                config.listen
            );
            return Ok(ExitCode::SUCCESS);
        }
        // The threads here serve connections. Sessions run on threads
        // of their own.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("waveportd-io")
            .enable_all()
            .build()
            .with(|| "nothing can be run".to_owned())?;
        let done = runtime.block_on(run(&config));
        // Whatever still waits for something is not waited for.
        runtime.shutdown_background();
        done
    });
    match done {
        Ok(code) => code,
        Err(Failure(failure)) => {
            eprintln!("waveportd: {failure}");
            ExitCode::FAILURE
        }
    }
}
