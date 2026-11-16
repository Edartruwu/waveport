//! The `waveport` command.
//!
//! Stability: experimental
//!
//! An agent to talk to at this machine, one that takes a call whose
//! caller follows a script, or one that takes calls over a WebSocket from
//! the same command run as a client. This is where the parts are put
//! together:
//! the session runtime, the models that listen, a provider for each of
//! hearing, thinking and speaking, a transport, a journal and two tools.
//! Nothing here decides anything about a conversation.
//!
//! Keys are read from the environment, and only the name of a variable
//! is ever written on the command line.

mod args;
mod call;
mod client;
mod report;
mod script;
mod serve;
mod tools;

use core::fmt;
use std::process::ExitCode;

use clap::Parser;
use waveport_transport_local_audio::{Choice, LocalAudio, Options, devices};

use crate::args::{Cli, Command, Dev, Run};
use crate::call::Outcome;

/// Why the command could not do what it was asked, in words for whoever
/// ran it.
#[derive(Debug)]
struct Failure(String);

/// Says what was being done when something failed.
trait Context<T> {
    /// The value, or a failure that begins with what `what` says.
    fn with(self, what: impl FnOnce() -> String) -> Result<T, Failure>;
}

impl<T, E: fmt::Display> Context<T> for Result<T, E> {
    fn with(self, what: impl FnOnce() -> String) -> Result<T, Failure> {
        self.map_err(|error| Failure(format!("{}: {error}", what())))
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let done = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(command(cli.command)),
        Err(error) => Err(Failure(format!("nothing can be run: {error}"))),
    };
    match done {
        Ok(Outcome::Ended) => ExitCode::SUCCESS,
        Ok(Outcome::Broken) => ExitCode::from(2),
        Err(Failure(failure)) => {
            eprintln!("waveport: {failure}");
            ExitCode::FAILURE
        }
    }
}

async fn command(command: Command) -> Result<Outcome, Failure> {
    match command {
        Command::Dev(dev) => at_the_desk(dev).await,
        Command::Run(run) => scripted(run).await,
        Command::Serve(serve) => serve::serve(serve).await,
        Command::Client(client) => client::call(client).await,
        Command::Devices => list_devices(),
    }
}

/// A call with whoever sits at this machine.
async fn at_the_desk(dev: Dev) -> Result<Outcome, Failure> {
    let choice = Choice {
        microphone: dev.microphone,
        loudspeaker: dev.loudspeaker,
    };
    let options = Options {
        half_duplex: dev.half_duplex,
        ..Options::default()
    };
    let transport = LocalAudio::open(&choice, options)
        .with(|| "the sound devices cannot be used (`waveport devices` lists them)".to_owned())?;
    if dev.half_duplex {
        println!("note: the microphone is deaf while the agent speaks");
    } else {
        println!("note: wear headphones, or the agent hears itself and stops speaking");
    }
    call::run(&dev.agent, transport).await
}

/// A call whose caller follows a script.
async fn scripted(run: Run) -> Result<Outcome, Failure> {
    let steps = script::read(&run.script).map_err(Failure)?;
    let (transport, recording) = LocalAudio::scripted(steps, Options::default());
    let outcome = call::run(&run.agent, transport).await?;
    if let Some(file) = &run.record {
        std::fs::write(file, recording.wav())
            .with(|| format!("the recording cannot be written to {}", file.display()))?;
        let millis = recording.samples().len() / 48;
        println!("recorded {millis} ms to {}", file.display());
    }
    Ok(outcome)
}

/// Lists the sound devices of this machine.
fn list_devices() -> Result<Outcome, Failure> {
    let found = devices().with(|| "the sound system cannot be asked".to_owned())?;
    if found.is_empty() {
        println!("this machine has no sound devices");
    }
    for device in found {
        let does = match (device.hears, device.plays) {
            (true, true) => "microphone and loudspeaker",
            (true, false) => "microphone",
            (false, true) => "loudspeaker",
            (false, false) => "neither",
        };
        let default = match (device.default_microphone, device.default_loudspeaker) {
            (true, true) => ", the default for both",
            (true, false) => ", the default microphone",
            (false, true) => ", the default loudspeaker",
            (false, false) => "",
        };
        println!("{}\n    {} ({does}{default})", device.id, device.name);
    }
    Ok(Outcome::Ended)
}
