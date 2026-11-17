//! The agent the command line defines, and a call run on it.

use core::time::Duration;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;
use waveport_assembly::stack::{Agent, Call, Settled, Stack, Told};
use waveport_assembly::{Context, Failure, Resume};
use waveport_domain::ids::{ParticipantId, Seq};
use waveport_ports::control::SessionInput;
use waveport_ports::journal::SessionId;
use waveport_ports::telemetry::EventSink;
use waveport_ports::transport::Transport;
use waveport_runtime::session::{Ended, Rebuilt};
use waveport_runtime::shard::Exit;
use waveport_transport_local_audio::{Health, LocalAudio};

use crate::args::{self, Aloud};
use crate::report::{Reporter, Tell};
use crate::summary::Summary;

pub(crate) use waveport_assembly::stack::name;

/// Who writes to a chat: whoever sits at the terminal.
const WRITER: ParticipantId = ParticipantId(0);

/// How long a call that was asked to end has to do so before the command
/// stops waiting for it.
const LAST_WORDS: Duration = Duration::from_secs(15);

/// How a call went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It ended, and everything it decided was recorded.
    Ended,
    /// It broke off: its journal failed, or it panicked.
    Broken,
}

/// What every call of this command is made of: what calls share, and the
/// one agent the command line defines.
pub(crate) struct Setup {
    stack: Stack,
    agent: Agent,
}

/// What is said of a call when it begins.
pub(crate) fn begins(call: &Call) -> String {
    let records = |count: u64| if count == 1 { "record" } else { "records" };
    match call.rebuilt {
        Some(Rebuilt {
            state: Some(upto),
            records: after,
        }) => format!(
            "the call goes on, from what it had come to by record {} and {after} {} after \
             that",
            upto.get(),
            records(after)
        ),
        Some(Rebuilt {
            state: None,
            records: all,
        }) => format!("the call goes on, from its {all} {}", records(all)),
        None if call.resumed => {
            "the call has begun: nothing of it was stored before it broke off".to_owned()
        }
        None => "the call has begun".to_owned(),
    }
}

impl Setup {
    /// Puts the parts together as `agent` says, with what `aloud` says it
    /// hears and speaks with, if it does either. `resuming` is whether a
    /// call is to be gone on with, which the calls that were kept are
    /// needed for.
    pub(crate) async fn build(
        agent: &args::Agent,
        aloud: Option<&Aloud>,
        resuming: bool,
    ) -> Result<Self, Failure> {
        let stack = Stack::build(&agent.stack(aloud), resuming).await?;
        let agent = stack.agent(&agent.spec(aloud)).await?;
        for note in &agent.notes {
            eprintln!("waveport: {note}");
        }
        Ok(Self { stack, agent })
    }

    /// What calls share: where they run, and where they are kept.
    pub(crate) const fn stack(&self) -> &Stack {
        &self.stack
    }

    /// What whoever ran the command should know before any call.
    pub(crate) fn notes(&self) -> &[String] {
        &self.stack.notes
    }

    /// What the tools are called, in the kernel's order.
    pub(crate) fn tools(&self) -> Vec<String> {
        self.stack.tools()
    }

    /// Something to follow one call with, and say afterwards what it
    /// came to.
    pub(crate) fn summary(&self) -> Arc<Summary> {
        Arc::new(Summary::new(self.stack.tools()))
    }

    /// Sends what is still to be sent of the traces and the metrics, and
    /// says so if any of them did not reach the collector. For the end
    /// of the command.
    pub(crate) async fn finish(&self) {
        if let Some(lost) = self.stack.finish().await {
            eprintln!("waveport: {lost}");
        }
    }

    /// Begins a call called `id` over `transport`, or goes on with it if
    /// `resumed` says that it is one that broke off. Every line about it
    /// is given to `tell`, on the call's own thread.
    pub(crate) async fn start(
        &self,
        id: SessionId,
        transport: Box<dyn Transport>,
        tell: Tell,
        resumed: bool,
        summary: Option<Arc<Summary>>,
    ) -> Result<Call, Failure> {
        // The call is told on the terminal, and to whoever else was
        // asked for.
        let mut sinks: Vec<Arc<dyn EventSink>> =
            vec![Arc::new(Reporter::new(tell, self.stack.tools()))];
        if let Some(summary) = summary {
            sinks.push(summary);
        }
        let (call, _) = self
            .stack
            .start(&self.agent, id, Some(transport), sinks, resumed, false)
            .await?;
        Ok(call)
    }

    /// Begins a chat called `id`, or goes on with it: a call over no
    /// line, with whoever writes to it. Returns it with what it tells
    /// from its beginning here on, which is how what it writes is read.
    pub(crate) async fn start_written(
        &self,
        id: SessionId,
        resumed: bool,
    ) -> Result<(Call, Told), Failure> {
        let (call, told) = self
            .stack
            .start(&self.agent, id, None, Vec::new(), resumed, true)
            .await?;
        let told = told.ok_or_else(|| Failure("the chat tells nothing".to_owned()))?;
        Ok((call, told))
    }

    /// Follows the call `id` from the fact numbered `from`.
    pub(crate) fn follow(&self, id: SessionId, from: Seq) -> Option<Told> {
        self.stack.follow(id, from)
    }

    /// Says to the call `id` what was typed.
    pub(crate) fn type_in(&self, id: SessionId, text: String) -> bool {
        self.stack.ask(id, SessionInput::Text { who: WRITER, text })
    }

    /// The name of the call to run: the one `resume` says to go on with,
    /// or a new one, which is then the one that `--resume last` finds.
    pub(crate) async fn call_to_run(&self, resume: Option<&Resume>) -> Result<SessionId, Failure> {
        self.stack.call_to_run(resume).await
    }

    /// Deals with what is kept of the call `id` once it has left as
    /// `exit` says: a call that is over is let go of, and one that broke
    /// off is there to go on with. `how` is how whoever ran the command
    /// goes on with it, if they can.
    pub(crate) async fn settle(&self, id: SessionId, exit: &Exit, how: Option<&str>) {
        match (self.stack.settle(id, exit).await, how) {
            (Settled::Forgotten(Some(kept)), _) => eprintln!("waveport: {kept}"),
            (Settled::Kept, Some(how)) => {
                eprintln!("waveport: the call is kept: {how} goes on with it");
            }
            _ => {}
        }
    }

    /// Asks the call `id` to end: the agent finishes what it is saying.
    pub(crate) fn ask_to_end(&self, id: SessionId) {
        let _ = self.stack.ask(id, SessionInput::Close);
    }
}

/// How a call went, from how its session left.
pub(crate) fn outcome(exit: &Exit) -> Outcome {
    match exit {
        Exit::Ended(Ended::Closed) => Outcome::Ended,
        Exit::Ended(Ended::Lost(broken)) => {
            eprintln!("waveport: the call broke off: {broken:?}");
            Outcome::Broken
        }
        Exit::Panicked => {
            eprintln!("waveport: the call broke off: its session panicked");
            Outcome::Broken
        }
        Exit::Stopped => {
            eprintln!("waveport: the call was stopped where it was");
            Outcome::Broken
        }
    }
}

/// Runs one call over `transport`, and tells it on the terminal as it
/// goes. The call is a new one, or the one `resume` says to go on with.
pub(crate) async fn run(
    agent: &args::Agent,
    aloud: &Aloud,
    transport: LocalAudio,
    resume: Option<&Resume>,
    report: Option<&Path>,
) -> Result<Outcome, Failure> {
    let health = transport.health();
    let stack = Setup::build(agent, Some(aloud), resume.is_some()).await?;
    for note in stack.notes() {
        println!("note: {note}");
    }
    let id = stack.call_to_run(resume).await?;
    let (lines, mut printed) = mpsc::unbounded_channel();
    let tell = Box::new(move |line| {
        // Nobody reads any more when the command is on its way out.
        let _ = lines.send(line);
    });
    let summary = report.map(|_| stack.summary());
    let resumed = resume.is_some();
    let call = stack
        .start(id, Box::new(transport), tell, resumed, summary.clone())
        .await?;
    for note in &call.notes {
        println!("note: {note}");
    }
    println!("{}", begins(&call));

    let (id, rebuilt) = (call.id, call.rebuilt);
    let exit = call.exit();
    tokio::pin!(exit);
    let mut asked_to_end = false;
    let exit = loop {
        tokio::select! {
            exit = &mut exit => break exit,
            Some(line) = printed.recv() => println!("{line}"),
            _ = tokio::signal::ctrl_c(), if !asked_to_end => {
                asked_to_end = true;
                eprintln!("waveport: ending the call. Ctrl-C again stops it where it is.");
                stack.ask_to_end(id);
            }
            () = last_words(asked_to_end) => break Exit::Stopped,
        }
    };
    while let Ok(line) = printed.try_recv() {
        println!("{line}");
    }
    tell_health(&health);
    stack.settle(id, &exit, Some("--resume last")).await;
    if let (Some(file), Some(summary)) = (report, summary) {
        let mut written = summary.report(id, rebuilt, &exit).to_string();
        written.push('\n');
        std::fs::write(file, written)
            .with(|| format!("the report cannot be written to {}", file.display()))?;
        println!("the report of the call is in {}", file.display());
    }
    stack.finish().await;
    Ok(outcome(&exit))
}

/// Waits until a call that was asked to end has had its time, or until
/// it is told again to stop. Never returns for a call nobody has asked to
/// end.
pub(crate) async fn last_words(asked_to_end: bool) {
    if !asked_to_end {
        return core::future::pending().await;
    }
    tokio::select! {
        () = tokio::time::sleep(LAST_WORDS) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Says what went wrong with the sound devices, if anything did.
pub(crate) fn tell_health(health: &Health) {
    if let Some(failure) = health.failure() {
        eprintln!("waveport: {failure}");
    }
    let (stumbles, lost) = (health.stumbles(), health.lost());
    if stumbles > 0 || lost > 0 {
        eprintln!(
            "waveport: the sound devices fell behind {stumbles} times, and {} ms of the \
             microphone were lost",
            lost / 48
        );
    }
}
