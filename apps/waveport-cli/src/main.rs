//! The `waveport` command.
//!
//! Stability: experimental
//!
//! An agent to talk to at this machine, one that takes a call whose
//! caller follows a script, one that takes calls over a WebSocket from
//! the same command run as a client, or one to chat with in writing.
//! The command line defines an agent, and `waveport-assembly` puts it
//! together: the session runtime, the models that listen, a provider for
//! each of hearing, thinking and speaking, a journal and the tools. What
//! is here is the line of each call, what is shown of it, and how it
//! ends. An agent that only writes is the same agent on no line. Nothing
//! here decides anything about a conversation.
//!
//! Calls are kept in a file, so that one that broke off can be gone on
//! with: `--resume last`.
//!
//! Keys are read from the environment, and only the name of a variable
//! is ever written on the command line.

mod args;
mod call;
mod chat;
mod client;
mod report;
mod script;
mod serve;
mod summary;
mod webrtc;

use std::process::ExitCode;

use clap::Parser;
use waveport_assembly::halt::Halt;
use waveport_assembly::spec::Keeps;
use waveport_assembly::{Context, Failure, Kept};
use waveport_transport_local_audio::{Choice, LocalAudio, Options, devices};

use crate::args::{Calls, Cli, Command, Dev, Run, SAID};
use crate::call::Outcome;

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
        Command::Chat(chat) => chat::chat(chat).await,
        Command::Calls(calls) => list_calls(&calls).await,
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
    call::run(
        &dev.agent,
        &dev.aloud,
        transport,
        dev.resume.as_ref(),
        dev.report.as_deref(),
    )
    .await
}

/// A call whose caller follows a script.
async fn scripted(run: Run) -> Result<Outcome, Failure> {
    let steps = script::read(&run.script).map_err(Failure)?;
    let (transport, recording) = LocalAudio::scripted(steps, Options::default());
    let report = run.report.as_deref();
    let resume = run.resume.as_ref();
    let outcome = call::run(&run.agent, &run.aloud, transport, resume, report).await?;
    if let Some(file) = &run.record {
        std::fs::write(file, recording.wav())
            .with(|| format!("the recording cannot be written to {}", file.display()))?;
        let millis = recording.samples().len() / 48;
        println!("recorded {millis} ms to {}", file.display());
    }
    Ok(outcome)
}

/// Lists the calls that broke off and are kept.
async fn list_calls(calls: &Calls) -> Result<Outcome, Failure> {
    let file = &calls.journal;
    // A file that is not there holds nothing, and is not made to say
    // so.
    if !file.exists() {
        println!("no call is kept: there is no {}", file.display());
        return Ok(Outcome::Ended);
    }
    let (kept, _) = Kept::open(&Keeps::File(file.clone()), &SAID, &Halt::Nowhere, true)?;
    let calls = kept.labelled().await?;
    match calls.len() {
        0 => println!("no call is kept in {}", file.display()),
        1 => println!("1 call that broke off is kept in {}", file.display()),
        many => println!("{many} calls that broke off are kept in {}", file.display()),
    }
    for (call, label) in calls {
        // What is kept with a call says whose it is, where somebody
        // said: a daemon keeps the name of the agent.
        match label {
            Some(label) => println!("{call}  {label}"),
            None => println!("{call}"),
        }
    }
    Ok(Outcome::Ended)
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
