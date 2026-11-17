//! The caller's side of a call that goes over a WebSocket: the sound
//! devices of this machine, or a scripted caller, joined to an agent
//! that runs somewhere else.
//!
//! The agent's side tells the call in the facts of client protocol v1,
//! and they are told here line by line, as the agent's side tells them
//! on its own terminal. A line that drops is not the end of the call:
//! the caller comes back with the token it was given, for as long as
//! the call can be expected to wait.

use core::future::poll_fn;
use core::time::Duration;

use tokio::time::Instant;
use waveport_adapter_kit::stream::{Either, first};
use waveport_domain::ids::{Epoch, GenId, OpTag};
use waveport_domain::time::MediaDur;
use waveport_ports::error::PortError;
use waveport_ports::transport::{FrameOut, Transport, TransportControl, TransportEvent};
use waveport_protocol::client::v1::{
    self as protocol, AudioFormat, ByeReason, CloseReason, Fact, Item, Lifecycle, Resume,
    ToolOutcome, features,
};
use waveport_transport_local_audio::{Choice, LocalAudio, Options, RATE};
use waveport_transport_websocket::{Client, Heard, Hello, Unconnected};

use crate::args::Client as Args;
use crate::call::{Outcome, tell_health};
use crate::{Context, Failure, script};

/// How long the agent's server has to answer a call.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// How long an agent's side that takes nothing more is still read for
/// what it had said.
const LAST_WORDS: Duration = Duration::from_secs(1);

/// How long a caller whose line dropped goes on trying to come back. A
/// call waits half a minute for its caller, and an agent that died is
/// started again by whoever runs it.
const COMES_BACK_FOR: Duration = Duration::from_secs(30);

/// How long a caller waits between two tries to come back.
const TRIES_EVERY: Duration = Duration::from_millis(250);

/// How many tool calls are remembered while they are under way.
const REMEMBERED: usize = 32;

/// How a call came to its end, seen from the caller's side, or how its
/// line did.
enum Over {
    /// The agent's side said goodbye: the call is over.
    Ended,
    /// The caller left.
    Left,
    /// The line dropped, and the call may still be there.
    Dropped(String),
    /// The agent's side is gone for good, or wants nothing more of this
    /// caller.
    Gone(String),
}

/// What the devices of this machine know a run of audio by. The agent's
/// side numbers its runs, from nought on each line: a run is known here
/// by its line and its number.
const fn tag(line: u32, run: u32) -> OpTag {
    OpTag {
        generation: GenId(run),
        epoch: Epoch(line),
    }
}

/// What the caller holds of the call: what it comes back with, and what
/// it needs to tell the call line by line.
#[derive(Default)]
struct Held {
    /// What lets the caller come back.
    token: Option<String>,
    /// The life of the call it was told of.
    life: u64,
    /// The number of the last fact it was told.
    seen: u64,
    /// The tool calls that were asked for and have not come back: the
    /// call, the tool, and what it was given.
    calls: Vec<(u64, String, Option<String>)>,
    /// Why the call is ending.
    reason: Option<CloseReason>,
    /// Whether the caller was told how the call ended.
    ended: bool,
}

impl Held {
    /// Takes what the agent's side said as it took the caller.
    fn welcomed(&mut self, far: &Client) {
        let welcome = far.welcome();
        self.token.clone_from(&welcome.resume_token);
        self.life = welcome.life;
    }

    /// Where the caller was, for the agent's side to go on from.
    fn resume(&self) -> Option<Resume> {
        Some(Resume {
            token: self.token.clone()?,
            life: self.life,
            seen: self.seen,
        })
    }

    /// What a turn, a response or the end of a tool call comes to as a
    /// line.
    fn tool(&mut self, call: u64, outcome: ToolOutcome, result: Option<&str>) -> String {
        let how = match outcome {
            ToolOutcome::Completed => "",
            ToolOutcome::Failed => " [failed]",
            ToolOutcome::Cancelled => " [cancelled]",
            ToolOutcome::InDoubt | ToolOutcome::Unknown => " [in doubt]",
        };
        let result = result.map_or_else(String::new, |result| format!(" -> {result}"));
        // A call that nobody here saw asked for was asked for while the
        // line was gone.
        let Some(found) = self.calls.iter().position(|(of, _, _)| *of == call) else {
            return format!("tool: a call from before the line dropped{result}{how}");
        };
        let (_, name, args) = self.calls.remove(found);
        let args = args.map_or_else(String::new, |args| format!("({args})"));
        format!("tool: {name}{args}{result}{how}")
    }

    fn asked_for(&mut self, call: u64, tool: String, args: Option<String>) {
        if self.calls.len() >= REMEMBERED {
            self.calls.remove(0);
        }
        self.calls.push((call, tool, args));
    }

    /// How the call ended, as a line.
    fn ending(&mut self) -> String {
        self.ended = true;
        let why = match self.reason {
            Some(CloseReason::Requested) => "it was asked to",
            Some(CloseReason::Abandoned) => "the caller left",
            Some(CloseReason::Failure) => "it could not go on",
            Some(CloseReason::Unknown) | None => "nobody said why",
        };
        format!("the call ended: {why}")
    }

    /// The line a fact is told as, if it is told.
    fn line(&mut self, fact: Fact) -> Option<String> {
        match fact {
            Fact::TurnCommitted { text, .. } => Some(format!("caller: {text}")),
            Fact::KeysCommitted { keys, .. } => Some(format!("caller pressed: {keys}")),
            Fact::TurnDismissed { reason, .. } => Some(format!(
                "(the caller made a sound that was not a turn: {reason:?})"
            )),
            Fact::UtteranceCommitted {
                text, truncated, ..
            } => Some(said(&text, truncated)),
            Fact::ToolCalled {
                call, tool, args, ..
            } => {
                self.asked_for(call, tool, args);
                None
            }
            Fact::ToolFinished {
                call,
                outcome,
                result,
            } => Some(self.tool(call, outcome, result.as_deref())),
            Fact::LifecycleChanged { to, reason } => {
                self.reason = reason.or(self.reason);
                (to == Lifecycle::Closed).then(|| self.ending())
            }
            _ => None,
        }
    }

    /// The lines a state is told as: the conversation as it stands. It
    /// takes the place of what was told before, of which the call may
    /// hold less, or more.
    fn stands(&mut self, state: protocol::State) -> Vec<String> {
        self.seen = state.upto;
        self.calls.clear();
        self.reason = state.closed_for;
        let mut lines = vec!["the call as it stands:".to_owned()];
        for item in state.items {
            let line = match item {
                Item::Turn { text, .. } => format!("caller: {text}"),
                Item::Keys { keys, .. } => format!("caller pressed: {keys}"),
                Item::Utterance {
                    text, truncated, ..
                } => said(&text, truncated),
                Item::ToolCall {
                    call, tool, args, ..
                } => {
                    self.asked_for(call, tool, args);
                    continue;
                }
                Item::ToolResult {
                    call,
                    outcome,
                    result,
                } => self.tool(call, outcome, result.as_deref()),
                Item::Unknown => continue,
            };
            lines.push(format!("  {line}"));
        }
        if state.lifecycle == Lifecycle::Closed {
            lines.push(self.ending());
        }
        lines
    }
}

/// What an agent said, as a line.
fn said(text: &str, truncated: bool) -> String {
    let mut line = format!("agent: {text}");
    if truncated {
        line.push_str(" [cut off]");
    }
    line
}

/// Calls the agent at `args.url` and stays on the call to its end.
pub(crate) async fn call(args: Args) -> Result<Outcome, Failure> {
    let (mut near, recording) = if let Some(file) = &args.script {
        let steps = script::read(file).map_err(Failure)?;
        let (near, recording) = LocalAudio::scripted(steps, Options::default());
        (near, Some(recording))
    } else {
        let choice = Choice {
            microphone: args.microphone.clone(),
            loudspeaker: args.loudspeaker.clone(),
        };
        let options = Options {
            half_duplex: args.half_duplex,
            ..Options::default()
        };
        let near = LocalAudio::open(&choice, options).with(|| {
            "the sound devices cannot be used (`waveport devices` lists them)".to_owned()
        })?;
        if args.half_duplex {
            println!("note: the microphone is deaf while the agent speaks");
        } else {
            println!("note: wear headphones, or the agent hears itself and stops speaking");
        }
        (near, None)
    };
    let health = near.health();
    let hello = Hello {
        // A caller at a terminal is told what the tools did, as far as
        // the agent's side lets it.
        features: vec![features::TOOLS.to_owned()],
        ..Hello::new(Some(AudioFormat {
            codec: protocol::Codec::Pcm16,
            rate: RATE.hz(),
        }))
    };
    let mut far = Client::connect(&args.url, &hello, ANSWER_WITHIN)
        .await
        .with(|| format!("no agent took the call at {}", args.url))?;
    println!("the call has begun");
    let mut held = Held::default();
    held.welcomed(&far);

    // Each line the call is on has a number: the audio of one is not
    // the audio of another.
    let mut line = 0;
    let over = loop {
        let over = tokio::select! {
            over = stay(&mut near, &mut far, &mut held, line) => over,
            // The connection goes with the command, and the agent's side
            // waits for a caller who does not come back.
            _ = tokio::signal::ctrl_c() => Over::Left,
        };
        let Over::Dropped(why) = over else {
            break over;
        };
        eprintln!("waveport: the line to the agent dropped ({why}): coming back to the call");
        let back = tokio::select! {
            back = comes_back(&mut near, &args.url, &hello, &held) => back,
            _ = tokio::signal::ctrl_c() => Err(Over::Left),
        };
        match back {
            Ok(client) => {
                far = client;
                held.welcomed(&far);
                line += 1;
                println!("the call goes on");
            }
            Err(over) => break over,
        }
    };
    tell_health(&health);
    if let (Some(recording), Some(file)) = (recording, &args.record) {
        std::fs::write(file, recording.wav())
            .with(|| format!("the recording cannot be written to {}", file.display()))?;
        let millis = recording.samples().len() / 48;
        println!("recorded {millis} ms to {}", file.display());
    }
    match over {
        Over::Ended | Over::Left => Ok(Outcome::Ended),
        Over::Dropped(why) | Over::Gone(why) => {
            eprintln!("waveport: the agent's side of the call is gone: {why}");
            Ok(Outcome::Broken)
        }
    }
}

/// Comes back to the call on another connection, for as long as the
/// call can be expected to be there. What the microphone hears
/// meanwhile is heard by nobody.
async fn comes_back(
    near: &mut LocalAudio,
    url: &str,
    hello: &Hello,
    held: &Held,
) -> Result<Client, Over> {
    let Some(resume) = held.resume() else {
        let why = "it gave the caller nothing to come back with";
        return Err(Over::Gone(why.to_owned()));
    };
    let hello = Hello {
        resume: Some(resume),
        ..hello.clone()
    };
    let trying = async {
        let until = Instant::now() + COMES_BACK_FOR;
        loop {
            match Client::connect(url, &hello, ANSWER_WITHIN).await {
                Ok(client) => return Ok(client),
                // It is there, and the call is not, or not for this
                // caller.
                Err(Unconnected::Refused { message, .. }) => return Err(Over::Gone(message)),
                Err(Unconnected::Failed(error)) => {
                    if Instant::now() >= until {
                        return Err(Over::Gone(format!("it did not come back: {error}")));
                    }
                    tokio::time::sleep(TRIES_EVERY).await;
                }
            }
        }
    };
    let leaving = async {
        loop {
            match poll_fn(|cx| near.poll_event(cx)).await {
                Some(TransportEvent::Left { .. }) | None => return,
                Some(_) => {}
            }
        }
    };
    tokio::select! {
        back = trying => back,
        () = leaving => Err(Over::Left),
    }
}

/// What a goodbye of the agent's side means for the call.
fn parted(reason: ByeReason) -> Over {
    match reason {
        ByeReason::Ended => Over::Ended,
        // The call is kept, and its agent will be back.
        ByeReason::GoingAway => Over::Dropped("the agent's side is going away".to_owned()),
        ByeReason::Replaced => Over::Gone("another connection took the call".to_owned()),
        ByeReason::Refused | ByeReason::Unknown => {
            Over::Gone("it wants nothing more of this caller".to_owned())
        }
    }
}

/// Takes one thing the agent's side said that is no audio. Returns how
/// the call or its line ended, if it did.
fn told(held: &mut Held, heard: Heard) -> Option<Over> {
    match heard {
        Heard::Fact { seq, fact } => {
            held.seen = seq;
            if let Some(line) = held.line(fact) {
                println!("{line}");
            }
        }
        Heard::State(state) => {
            for line in held.stands(*state) {
                println!("{line}");
            }
        }
        Heard::Error { message, .. } => {
            eprintln!("waveport: the agent's side did not take something: {message}");
        }
        Heard::Bye(reason) => {
            let over = parted(reason);
            // A call that is over was said to be, whether or not the
            // caller was told how it ended.
            if matches!(over, Over::Ended) && !held.ended {
                println!("{}", held.ending());
            }
            return Some(over);
        }
        // The news was not asked for, and a gap is followed by a state.
        _ => {}
    }
    None
}

/// The agent's side takes nothing more: sending to it failed with
/// `error`. It may have said how the call ended before it stopped
/// listening, so what it sent is read to its end. A call that was said
/// to be over is over, and one that was not may still be there.
async fn last_words(far: &mut Client, held: &mut Held, error: &PortError) -> Over {
    let reading = async {
        loop {
            match far.next().await {
                Ok(Some(heard)) => {
                    if let Some(over) = told(held, heard) {
                        return Some(over);
                    }
                }
                Ok(None) | Err(_) => return None,
            }
        }
    };
    match tokio::time::timeout(LAST_WORDS, reading).await {
        Ok(Some(over)) => over,
        Ok(None) | Err(_) => Over::Dropped(error.to_string()),
    }
}

/// Carries the call on one line: what the microphone hears goes to the
/// agent, what the agent says goes to the loudspeaker, and the agent is
/// told how much of it has been played.
async fn stay(near: &mut LocalAudio, far: &mut Client, held: &mut Held, line: u32) -> Over {
    // The run whose audio arrives, and how many samples of it have.
    let mut open: Option<(u32, usize)> = None;
    loop {
        // What the agent's side says comes before what the microphone
        // hears: a caller who is told that the call is over has nothing
        // more to send.
        let here = poll_fn(|cx| near.poll_event(cx));
        let heard = match first(far.next(), here).await {
            Either::Right(Some(TransportEvent::Audio { payload, .. })) => {
                if let Err(error) = far.audio(payload).await {
                    return last_words(far, held, &error).await;
                }
                continue;
            }
            Either::Right(Some(TransportEvent::Played { tag, played })) => {
                // What was played of a line that is gone is told to
                // nobody: this one knows nothing of its runs.
                if tag.epoch.0 != line {
                    continue;
                }
                let samples = u32::try_from(RATE.samples_in(played)).unwrap_or(u32::MAX);
                if let Err(error) = far.played(tag.generation.0, samples).await {
                    return last_words(far, held, &error).await;
                }
                continue;
            }
            Either::Right(Some(TransportEvent::Left { .. }) | None) => {
                let _ = far.say(&protocol::FromClient::Bye).await;
                return Over::Left;
            }
            Either::Right(Some(_)) => continue,
            Either::Left(Ok(Some(heard))) => heard,
            Either::Left(Ok(None)) => {
                return Over::Dropped("its connection closed with no goodbye".to_owned());
            }
            Either::Left(Err(error)) => return Over::Dropped(error.to_string()),
        };
        match heard {
            Heard::Begins { run, .. } => open = Some((run, 0)),
            Heard::Audio(samples) => {
                // Audio that no run was begun for belongs to none.
                if let Some((run, at)) = &mut open {
                    let length = samples.len() / 2;
                    let frame = FrameOut {
                        tag: tag(line, *run),
                        at: RATE.length_of(*at),
                        length: RATE.length_of(length),
                        payload: &samples,
                        last: false,
                    };
                    // A loudspeaker that is a second behind has no room.
                    // What does not fit is not played, and so is not said
                    // to have been.
                    let _ = near.try_send(frame);
                    *at += length;
                }
            }
            Heard::Ends(run) => {
                let at = match open {
                    Some((of, at)) if of == run => at,
                    _ => 0,
                };
                let end = FrameOut {
                    tag: tag(line, run),
                    at: RATE.length_of(at),
                    length: MediaDur::ZERO,
                    payload: &[],
                    last: true,
                };
                let _ = near.try_send(end);
                open = None;
            }
            Heard::Clear(run) => {
                let _ = near.try_control(TransportControl::Clear {
                    tag: tag(line, run),
                });
                if open.is_some_and(|(of, _)| of <= run) {
                    open = None;
                }
            }
            other => {
                if let Some(over) = told(held, other) {
                    return over;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use waveport_protocol::client::v1::{
        CloseReason, Dismissal, Fact, Item, Lifecycle, State, ToolOutcome,
    };

    use super::Held;

    fn called(call: u64, tool: &str, args: Option<&str>) -> Fact {
        Fact::ToolCalled {
            call,
            tool: tool.to_owned(),
            generation: 1,
            args: args.map(str::to_owned),
        }
    }

    fn finished(call: u64, outcome: ToolOutcome, result: Option<&str>) -> Fact {
        Fact::ToolFinished {
            call,
            outcome,
            result: result.map(str::to_owned),
        }
    }

    #[test]
    fn a_call_is_told_line_by_line_as_at_the_agents_own_terminal() {
        let mut held = Held::default();
        let turn = Fact::TurnCommitted {
            turn: 1,
            who: 0,
            text: "What time is it?".to_owned(),
            addressed: true,
        };
        assert_eq!(held.line(turn).unwrap(), "caller: What time is it?");
        let keys = Fact::KeysCommitted {
            turn: 2,
            who: 0,
            keys: "12#".to_owned(),
        };
        assert_eq!(held.line(keys).unwrap(), "caller pressed: 12#");
        let noise = Fact::TurnDismissed {
            turn: 3,
            who: 0,
            reason: Dismissal::Noise,
        };
        assert!(held.line(noise).unwrap().contains("not a turn: Noise"));
        let answer = |truncated| Fact::UtteranceCommitted {
            generation: 1,
            agent: 0,
            text: "Ten past three.".to_owned(),
            truncated,
        };
        assert_eq!(held.line(answer(false)).unwrap(), "agent: Ten past three.");
        assert_eq!(
            held.line(answer(true)).unwrap(),
            "agent: Ten past three. [cut off]"
        );
        // What is no line: what an agent is being heard to say, and a
        // tool that was sent its call.
        let progress = Fact::UtteranceProgress {
            generation: 1,
            agent: 0,
            heard: "Ten".to_owned(),
        };
        assert_eq!(held.line(progress), None);
        let sent = Fact::ToolSent {
            call: 1,
            attempt: 0,
        };
        assert_eq!(held.line(sent), None);
    }

    #[test]
    fn a_tool_call_is_one_line_when_it_has_come_back() {
        let mut held = Held::default();
        assert_eq!(held.line(called(1, "get_time", Some("{}"))), None);
        assert_eq!(held.line(called(2, "take_note", None)), None);
        assert_eq!(
            held.line(finished(1, ToolOutcome::Completed, Some("15:10")))
                .unwrap(),
            "tool: get_time({}) -> 15:10"
        );
        // A caller that is not shown what tools are given and answer is
        // told that they were called, and how it went.
        assert_eq!(
            held.line(finished(2, ToolOutcome::Failed, None)).unwrap(),
            "tool: take_note [failed]"
        );
        // One that was asked for while nobody was told.
        assert_eq!(
            held.line(finished(3, ToolOutcome::InDoubt, Some("done")))
                .unwrap(),
            "tool: a call from before the line dropped -> done [in doubt]"
        );
        // It is told once.
        assert!(
            held.line(finished(1, ToolOutcome::Completed, None))
                .unwrap()
                .contains("from before the line dropped")
        );
    }

    #[test]
    fn the_end_of_a_call_says_why() {
        let mut held = Held::default();
        let change = |to, reason| Fact::LifecycleChanged { to, reason };
        let requested = Some(CloseReason::Requested);
        assert_eq!(held.line(change(Lifecycle::Draining, requested)), None);
        assert_eq!(held.line(change(Lifecycle::Closing, None)), None);
        assert!(!held.ended);
        assert_eq!(
            held.line(change(Lifecycle::Closed, None)).unwrap(),
            "the call ended: it was asked to"
        );
        assert!(held.ended);
        assert_eq!(Held::default().ending(), "the call ended: nobody said why");
    }

    #[test]
    fn a_state_is_told_as_the_call_stands_and_is_where_the_caller_comes_back_from() {
        let mut held = Held {
            token: Some("wpr1_madeup".to_owned()),
            life: 2,
            seen: 4,
            ..Held::default()
        };
        // A call the caller saw asked for, which the call holds nothing
        // of now.
        held.line(called(9, "get_time", Some("{}")));
        let state = State {
            upto: 11,
            items: vec![
                Item::Turn {
                    turn: 1,
                    who: 0,
                    text: "What time is it?".to_owned(),
                    addressed: true,
                },
                Item::ToolCall {
                    call: 1,
                    tool: "get_time".to_owned(),
                    generation: 1,
                    args: Some("{}".to_owned()),
                },
                Item::ToolResult {
                    call: 1,
                    outcome: ToolOutcome::Completed,
                    result: Some("15:10".to_owned()),
                },
                Item::Utterance {
                    generation: 1,
                    agent: 0,
                    text: "Ten past".to_owned(),
                    truncated: true,
                },
                Item::ToolCall {
                    call: 2,
                    tool: "take_note".to_owned(),
                    generation: 2,
                    args: None,
                },
            ],
            ..State::default()
        };
        assert_eq!(
            held.stands(state),
            [
                "the call as it stands:",
                "  caller: What time is it?",
                "  tool: get_time({}) -> 15:10",
                "  agent: Ten past [cut off]",
            ]
        );
        let resume = held.resume().unwrap();
        assert_eq!((resume.life, resume.seen), (2, 11));
        assert_eq!(resume.token, "wpr1_madeup");
        // The call that was under way when the state was made is told
        // when it comes back, and the one from before is forgotten.
        assert_eq!(
            held.line(finished(2, ToolOutcome::Completed, None))
                .unwrap(),
            "tool: take_note"
        );
        assert!(
            held.line(finished(9, ToolOutcome::Completed, None))
                .unwrap()
                .contains("from before the line dropped")
        );

        // A call that is over says so in its state.
        let over = State {
            upto: 20,
            lifecycle: Lifecycle::Closed,
            closed_for: Some(CloseReason::Abandoned),
            ..State::default()
        };
        assert_eq!(
            held.stands(over),
            ["the call as it stands:", "the call ended: the caller left"]
        );
        // A caller with no token has nothing to come back with.
        assert!(Held::default().resume().is_none());
    }
}
