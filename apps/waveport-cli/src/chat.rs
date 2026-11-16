//! A chat: a call in writing, with whoever sits at the terminal.
//!
//! The session of a chat has no line to anyone. What is typed is asked
//! of it, a line at a time, and what it writes is read from what it
//! tells whoever follows it: the words of an answer as they are written,
//! and then the facts, which say what the answer came to. That is the
//! way any client of a session of text has, and nothing here knows more
//! of the session than a client would.
//!
//! Whoever types at a terminal is taken at their word: a line is said
//! when it is typed, and one typed while the agent writes stops the
//! agent there. Lines that come from a file or from another command wait
//! their turn, each for the answer to the one before.

use core::future::poll_fn;
use core::time::Duration;
use std::collections::VecDeque;
use std::io::{IsTerminal as _, Write as _};

use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::Instant;
use waveport_domain::command::{Activity, ClientEvent};
use waveport_domain::fact::Fact;
use waveport_domain::ids::{GenId, Seq};
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;
use waveport_ports::control::SessionEvent;
use waveport_ports::journal::SessionId;
use waveport_ports::telemetry::EventSink;
use waveport_runtime::codec;
use waveport_runtime::shard::Exit;

use crate::Failure;
use crate::args::Chat;
use crate::call::{self, Outcome, Stack, Told};
use crate::report::Reporter;

/// How long the agent has to have had nothing to do before a line that
/// waits its turn is said, or a chat whose lines have run out is ended.
/// Between the answer of a tool and the answer that is made of it the
/// agent has nothing to do for a moment, which is shorter than this.
const SETTLED: Duration = Duration::from_millis(250);

/// How long a line that nothing came of is waited for before the next is
/// said. Something comes of every line while the chat is under way.
const GIVEN_UP: Duration = Duration::from_secs(30);

/// How long what a chat that is over told last is waited for.
const LAST_TOLD: Duration = Duration::from_secs(2);

/// The lines that are typed, read on a thread of their own. A read that
/// waits for a line cannot be given up, and a thread that waits in one
/// must not be one the command waits for when it ends.
fn typed() -> UnboundedReceiver<String> {
    let (lines, typed) = mpsc::unbounded_channel();
    let read = move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    };
    if let Err(error) = std::thread::Builder::new()
        .name("waveport-typed".to_owned())
        .spawn(read)
    {
        eprintln!("waveport: nothing that is typed can be read: {error}");
    }
    typed
}

/// What of a chat is on the terminal, and where the chat is.
struct Page {
    id: SessionId,
    /// Says the facts that are told as lines.
    reporter: Reporter,
    lines: UnboundedReceiver<String>,
    /// Whether what is typed is on the terminal already, because it was
    /// typed there.
    typed_here: bool,
    /// The response the line that is being written is of.
    open: Option<GenId>,
    /// The response whose words were written last.
    written: Option<GenId>,
    /// The number of the last fact that was told.
    last: Seq,
    /// Lines that were said and are not yet turns of the conversation.
    unheard: usize,
    /// Whether the agent has begun anything since it was last given
    /// something to do: a line, or its greeting.
    begun: bool,
    /// Tool calls that have not come back.
    calls: usize,
    /// What the agent is doing.
    activity: Activity,
}

/// Writes `text` where the chat is shown, at once.
fn show(text: &str) {
    let mut out = std::io::stdout().lock();
    // Whoever read the chat may have gone, which ends nothing here.
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

impl Page {
    /// The page of the chat `id`, which is `greeted` if its agent was
    /// given something to say first.
    fn new(id: SessionId, tools: Vec<String>, typed_here: bool, greeted: bool) -> Self {
        let (tell, lines) = mpsc::unbounded_channel();
        let tell = Box::new(move |line| {
            let _ = tell.send(line);
        });
        Self {
            id,
            reporter: Reporter::new(tell, tools),
            lines,
            typed_here,
            open: None,
            written: None,
            last: Seq(0),
            unheard: 0,
            begun: !greeted,
            calls: 0,
            activity: Activity::Listening,
        }
    }

    /// Ends the line that is being written, if one is.
    fn end_line(&mut self) {
        if self.open.take().is_some() {
            show("\n");
        }
    }

    /// A line of its own.
    fn line(&mut self, line: &str) {
        self.end_line();
        show(&format!("{line}\n"));
    }

    /// Words of the response `generation`, as they are written.
    fn words(&mut self, generation: GenId, text: &str) {
        if self.open != Some(generation) {
            self.end_line();
            show("agent: ");
            self.open = Some(generation);
            self.written = Some(generation);
        }
        show(text);
    }

    /// A fact of the chat, as its journal holds it.
    fn fact(&mut self, record: &[u8]) {
        let mut text = TextArena::default();
        let Ok(stamped) = codec::decode(record, &mut text) else {
            return;
        };
        match stamped.fact {
            Fact::TurnCommitted { text: said, .. } => {
                self.unheard = self.unheard.saturating_sub(1);
                if !self.typed_here {
                    self.line(&format!("you: {}", text.get(said)));
                }
                return;
            }
            // An answer whose words are on the page is not written
            // again: it ends there, and says so if it was stopped.
            Fact::UtteranceCommitted {
                generation,
                truncated,
                ..
            } if self.written == Some(generation) => {
                if truncated && self.open == Some(generation) {
                    show(" [cut off]");
                } else if truncated {
                    self.line("(the agent was cut off)");
                }
                self.end_line();
                return;
            }
            Fact::ToolProposed { .. } => self.calls += 1,
            Fact::ToolFinished { .. } => self.calls = self.calls.saturating_sub(1),
            _ => {}
        }
        self.reporter
            .facts(self.id, MonoTime::ZERO, &[stamped], &text);
        while let Ok(line) = self.lines.try_recv() {
            self.line(&line);
        }
    }

    /// Something the chat told.
    fn told(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Said { generation, text } => self.words(generation, &text),
            SessionEvent::Fact { seq, record } => {
                self.last = seq;
                self.fact(&record);
            }
            SessionEvent::Told(ClientEvent::Agent { activity, .. }) => {
                self.begun |= activity != Activity::Listening;
                self.activity = activity;
            }
            // Nobody speaks in a chat, and facts that are no longer
            // held were told when they were made.
            SessionEvent::Told(_) | SessionEvent::Hearing { .. } | SessionEvent::Gap { .. } => {}
        }
    }

    /// A line is said to the chat.
    fn said(&mut self) {
        self.unheard += 1;
        self.begun = false;
    }

    /// Whether everything that was said has been answered: the agent
    /// began on it, has nothing to do now and waits for nothing.
    fn answered(&self) -> bool {
        self.begun && self.unheard == 0 && self.calls == 0 && self.activity == Activity::Listening
    }
}

/// The next thing a chat tells, or nothing when it tells no more. Never
/// returns for a chat that is not followed.
async fn next(told: &mut Option<Told>) -> Option<SessionEvent> {
    match told {
        Some(told) => poll_fn(|cx| told.as_mut().poll_next(cx)).await,
        None => core::future::pending().await,
    }
}

/// Waits until `at`, or for ever if there is nothing to wait for.
async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => core::future::pending().await,
    }
}

/// Runs a chat with whoever types, and shows it as it goes.
pub(crate) async fn chat(args: Chat) -> Result<Outcome, Failure> {
    let stack = Stack::build(&args.agent, None, args.resume.is_some()).await?;
    for note in &stack.notes {
        println!("note: {note}");
    }
    let id = stack.call_to_run(args.resume.as_ref()).await?;
    let (call, told) = stack.start_written(id, args.resume.is_some()).await?;
    for note in &call.notes {
        println!("note: {note}");
    }
    println!("{}", call.begins());

    let typed_here = std::io::stdin().is_terminal();
    let mut page = Page::new(id, stack.tools(), typed_here, call.greeted);
    let mut told = Some(told);
    let mut typed = Some(typed());
    // The lines that wait their turn.
    let mut waiting: VecDeque<String> = VecDeque::new();
    // Since when the agent has had nothing to do, and until when it is
    // waited for to begin on what it was given.
    let mut idle: Option<Instant> = page.answered().then(Instant::now);
    let mut given_up: Option<Instant> = Some(Instant::now() + GIVEN_UP);
    let mut asked_to_end = false;

    let exit = call.exit();
    tokio::pin!(exit);
    let exit = loop {
        // A line waits for the agent to have answered, and so does the
        // end of a chat whose lines have run out.
        let due = (!asked_to_end && (!waiting.is_empty() || typed.is_none()))
            .then(|| idle.map(|since| since + SETTLED).or(given_up))
            .flatten();
        tokio::select! {
            exit = &mut exit => break exit,
            event = next(&mut told) => match event {
                Some(event) => {
                    page.told(event);
                    idle = match (page.answered(), idle) {
                        (true, None) => Some(Instant::now()),
                        (true, since) => since,
                        (false, _) => None,
                    };
                    if page.begun {
                        given_up = None;
                    }
                }
                // A follower that fell behind is let go, and follows
                // again from where it was. A chat that is over tells
                // nothing more.
                None => told = stack.follow(id, page.last.next()),
            },
            line = async {
                match typed.as_mut() {
                    Some(typed) => typed.recv().await,
                    None => core::future::pending().await,
                }
            } => match line {
                Some(line) if line.trim().is_empty() => {}
                Some(line) if typed_here => {
                    if stack.type_in(id, line) {
                        page.said();
                        idle = None;
                        given_up = Some(Instant::now() + GIVEN_UP);
                    }
                }
                Some(line) => waiting.push_back(line),
                None => typed = None,
            },
            () = until(due) => if let Some(line) = waiting.pop_front() {
                if stack.type_in(id, line) {
                    page.said();
                    given_up = Some(Instant::now() + GIVEN_UP);
                }
                idle = page.answered().then(Instant::now);
            } else {
                asked_to_end = true;
                stack.ask_to_end(id);
            },
            _ = tokio::signal::ctrl_c(), if !asked_to_end => {
                asked_to_end = true;
                page.end_line();
                eprintln!("waveport: ending the chat. Ctrl-C again stops it where it is.");
                stack.ask_to_end(id);
            }
            () = call::last_words(asked_to_end) => break Exit::Stopped,
        }
    };
    // What the chat told last is still on its way, and ends with the
    // chat.
    if told.is_some() && !matches!(exit, Exit::Stopped) {
        let rest = async {
            while let Some(event) = next(&mut told).await {
                page.told(event);
            }
        };
        let _ = tokio::time::timeout(LAST_TOLD, rest).await;
    }
    page.end_line();
    stack.settle(id, &exit, Some("--resume last")).await;
    stack.finish().await;
    Ok(call::outcome(&exit))
}
