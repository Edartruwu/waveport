//! Sessions written down.
//!
//! A trace is everything one session was given, a line per event: when
//! the event arrived, and what it was.
//!
//! ```text
//! 0 control join 1
//! 100 speech 1 start 4800
//! 1100 speech 1 end 52800
//! 1160 eou 1 10000
//! 1300 transcript 1 4800 52800 final "where is my order"
//! 1420 op 1/0 delta "It shipped on Monday. "
//! 1500 op 1/0 done stop
//! ```
//!
//! A kernel does the same every time it is given the same events at the
//! same times, so a trace is enough to see a session again:
//! [`replay`] steps a kernel through it with a [`Monitor`] watching, and
//! returns the trace with what the kernel emitted written under each
//! line. Such a record is itself a trace, since [`Trace::parse`] skips the
//! lines that are indented. Kept in a file, it shows on the next replay
//! whether the kernel still does what it did.
//!
//! The clock is in milliseconds, with a fraction where an event did not
//! arrive on a whole one. Media time and durations are in ticks. A
//! `restart` line stands for the death of the process: the session goes on
//! from its journal, of which the facts up to the one named had reached
//! the disk.

use core::fmt;
use core::str::FromStr;

use waveport_domain::config::KernelConfig;
use waveport_domain::event::{
    CloseReason, ControlInput, Event, Finish, HandoffContext, OpEvent, OverlapVerdict, Part,
    RetryClass, TimerId, ToolEvent, VadEdge,
};
use waveport_domain::ids::{
    AgentId, Epoch, GenId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId, TurnId,
};
use waveport_domain::kernel::Kernel;
use waveport_domain::out::Out;
use waveport_domain::policy::Policies;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

use crate::check::Report;
use crate::invariants::{Journal, Monitor, Rule, Violation};

/// One line of a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// An event arrived at this time.
    Event(MonoTime, Event),
    /// The process died, and the session was rebuilt from its journal as
    /// far as this fact.
    Restart(Seq),
}

/// What one session was given, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trace {
    /// The lines.
    pub entries: Vec<Entry>,
}

/// A line that is not a line of a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// The number of the line, from 1.
    pub line: usize,
    /// What is wrong with it.
    pub what: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {} is not a line of a trace: {}",
            self.line, self.what
        )
    }
}

impl std::error::Error for ParseError {}

const EDGES: [(&str, VadEdge); 2] = [("start", VadEdge::Start), ("end", VadEdge::End)];
const VERDICTS: [(&str, OverlapVerdict); 3] = [
    ("backchannel", OverlapVerdict::Backchannel),
    ("barge-in", OverlapVerdict::BargeIn),
    ("noise", OverlapVerdict::Noise),
];
const FINISHES: [(&str, Finish); 4] = [
    ("stop", Finish::Stop),
    ("tool-calls", Finish::ToolCalls),
    ("length", Finish::Length),
    ("filtered", Finish::Filtered),
];
const PARTS: [(&str, Part); 3] = [
    ("think", Part::Think),
    ("speak", Part::Speak),
    ("play", Part::Play),
];
const CLASSES: [(&str, RetryClass); 5] = [
    ("transient", RetryClass::Transient),
    ("rate-limited", RetryClass::RateLimited),
    ("auth", RetryClass::Auth),
    ("permanent", RetryClass::Permanent),
    ("timeout", RetryClass::Timeout),
];
const REASONS: [(&str, CloseReason); 3] = [
    ("requested", CloseReason::Requested),
    ("abandoned", CloseReason::Abandoned),
    ("failure", CloseReason::Failure),
];

/// The word for `value` in `table`.
fn word<T: PartialEq + Copy>(table: &[(&'static str, T)], value: T) -> &'static str {
    table
        .iter()
        .find(|(_, of)| *of == value)
        .map_or("?", |(word, _)| word)
}

/// Text between quotes, with the characters that would end the line or
/// the text written as escapes.
struct Quoted<'a>(&'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for character in self.0.chars() {
            match character {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                other if other.is_control() => write!(f, "\\u{{{:x}}}", u32::from(other))?,
                other => write!(f, "{other}")?,
            }
        }
        f.write_str("\"")
    }
}

/// A time on the session's clock, in milliseconds.
struct Clock(MonoTime);

impl fmt::Display for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let nanos = self.0.as_nanos();
        let (millis, rest) = (nanos / 1_000_000, nanos % 1_000_000);
        if rest == 0 {
            write!(f, "{millis}")
        } else {
            let digits = format!("{rest:06}");
            write!(f, "{millis}.{}", digits.trim_end_matches('0'))
        }
    }
}

struct Shown<'a>(&'a Event);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Event::Speech { who, edge, at } => {
                write!(f, "speech {who} {} {}", word(&EDGES, *edge), at.ticks())
            }
            Event::Transcript {
                who,
                span,
                text,
                is_final,
            } => {
                let kind = if *is_final { "final" } else { "interim" };
                let (start, end) = (span.start.ticks(), span.end.ticks());
                let text = Quoted(text.as_str());
                write!(f, "transcript {who} {start} {end} {kind} {text}")
            }
            Event::Eou { turn, p } => write!(f, "eou {turn} {}", p.basis_points()),
            Event::Overlap { turn, verdict } => {
                write!(f, "overlap {turn} {}", word(&VERDICTS, *verdict))
            }
            Event::TextIn { who, text } => write!(f, "text {who} {}", Quoted(text.as_str())),
            Event::Op(tag, op) => {
                write!(f, "op {}/{} ", tag.generation, tag.epoch)?;
                op_shown(f, op)
            }
            Event::Tool(call, news) => {
                write!(f, "tool {call} ")?;
                match news {
                    ToolEvent::Completed { result } => {
                        write!(f, "completed {}", Quoted(result.as_str()))
                    }
                    ToolEvent::Failed { class, message } => {
                        let class = word(&CLASSES, *class);
                        write!(f, "failed {class} {}", Quoted(message.as_str()))
                    }
                    ToolEvent::RolledBack => f.write_str("rolled-back"),
                }
            }
            Event::Timer(timer) => {
                f.write_str("timer ")?;
                match timer {
                    TimerId::Eou(turn) => write!(f, "eou {turn}"),
                    TimerId::TurnMax(turn) => write!(f, "turn-max {turn}"),
                    TimerId::Overlap(turn) => write!(f, "overlap {turn}"),
                    TimerId::Gen(generation) => write!(f, "gen {generation}"),
                    TimerId::ToolTimeout(call) => write!(f, "tool-timeout {call}"),
                    TimerId::Rollback(call) => write!(f, "rollback {call}"),
                    TimerId::Checkpoint => f.write_str("checkpoint"),
                    TimerId::StageAck => f.write_str("stage-ack"),
                    TimerId::Drain => f.write_str("drain"),
                    TimerId::Grace => f.write_str("grace"),
                    TimerId::Detach => f.write_str("detach"),
                }
            }
            Event::Control(input) => {
                f.write_str("control ")?;
                control_shown(f, input)
            }
            Event::Durable(seq) => write!(f, "durable {seq}"),
        }
    }
}

fn op_shown(f: &mut fmt::Formatter<'_>, op: &OpEvent) -> fmt::Result {
    match op {
        OpEvent::LlmDelta { text } => write!(f, "delta {}", Quoted(text.as_str())),
        OpEvent::ToolProposed { tool, args } => {
            write!(f, "propose {tool} {}", Quoted(args.as_str()))
        }
        OpEvent::LlmDone { finish } => write!(f, "done {}", word(&FINISHES, *finish)),
        OpEvent::TtsSegment {
            text_end,
            audio_end,
            last,
        } => {
            let kind = if *last { "last" } else { "more" };
            write!(f, "segment {text_end} {} {kind}", audio_end.ticks())
        }
        OpEvent::Played {
            handed,
            acked,
            heard_text,
            drained,
        } => {
            let kind = if *drained { "drained" } else { "playing" };
            let (handed, acked) = (handed.ticks(), acked.ticks());
            write!(f, "played {handed} {acked} {heard_text} {kind}")
        }
        OpEvent::Failed { part, class } => {
            write!(
                f,
                "failed {} {}",
                word(&PARTS, *part),
                word(&CLASSES, *class)
            )
        }
    }
}

fn control_shown(f: &mut fmt::Formatter<'_>, input: &ControlInput) -> fmt::Result {
    match input {
        ControlInput::Say { text } => write!(f, "say {}", Quoted(text.as_str())),
        ControlInput::Interrupt => f.write_str("interrupt"),
        ControlInput::Handoff { to, context } => {
            write!(f, "handoff {to} ")?;
            match context {
                HandoffContext::Full => f.write_str("full"),
                HandoffContext::Summary(summary) => {
                    write!(f, "summary {}", Quoted(summary.as_str()))
                }
                HandoffContext::None => f.write_str("none"),
            }
        }
        ControlInput::Close { reason } => write!(f, "close {}", word(&REASONS, *reason)),
        ControlInput::Join { who } => write!(f, "join {who}"),
        ControlInput::Leave { who } => write!(f, "leave {who}"),
        ControlInput::Detached => f.write_str("detached"),
        ControlInput::Attached => f.write_str("attached"),
        ControlInput::StageReady => f.write_str("stage-ready"),
        ControlInput::LeaseLost => f.write_str("lease-lost"),
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Event(at, event) => write!(f, "{} {}", Clock(*at), Shown(event)),
            Self::Restart(stored) => write!(f, "restart {stored}"),
        }
    }
}

impl fmt::Display for Trace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for entry in &self.entries {
            writeln!(f, "{entry}")?;
        }
        Ok(())
    }
}

/// The words of one line, read from its beginning.
struct Words<'a> {
    rest: &'a str,
}

impl<'a> Words<'a> {
    fn word(&mut self) -> Result<&'a str, String> {
        let rest = self.rest.trim_start();
        if rest.is_empty() {
            return Err("it ends too soon".to_owned());
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, rest) = rest.split_at(end);
        self.rest = rest;
        Ok(word)
    }

    fn number<T: FromStr>(&mut self) -> Result<T, String> {
        let word = self.word()?;
        word.parse()
            .map_err(|_| format!("{word:?} is not a number that fits"))
    }

    fn one_of<T: Copy>(&mut self, table: &[(&'static str, T)]) -> Result<T, String> {
        let word = self.word()?;
        table
            .iter()
            .find(|(name, _)| *name == word)
            .map(|(_, value)| *value)
            .ok_or_else(|| {
                let names = table.iter().map(|(name, _)| *name).collect::<Vec<_>>();
                format!("{word:?} is none of {}", names.join(", "))
            })
    }

    /// Text between quotes.
    fn text(&mut self) -> Result<String, String> {
        let rest = self.rest.trim_start();
        let Some(quoted) = rest.strip_prefix('"') else {
            return Err(format!("text in quotes was expected at {rest:?}"));
        };
        let mut text = String::new();
        let mut characters = quoted.char_indices();
        while let Some((at, character)) = characters.next() {
            match character {
                '"' => {
                    self.rest = quoted.get(at + 1..).unwrap_or("");
                    return Ok(text);
                }
                '\\' => match characters.next().map(|(_, escaped)| escaped) {
                    Some('n') => text.push('\n'),
                    Some('r') => text.push('\r'),
                    Some('t') => text.push('\t'),
                    Some('u') => {
                        let digits = characters
                            .by_ref()
                            .map(|(_, digit)| digit)
                            .skip_while(|&digit| digit == '{')
                            .take_while(|&digit| digit != '}')
                            .collect::<String>();
                        let escaped = u32::from_str_radix(&digits, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .ok_or_else(|| format!("\\u{{{digits}}} is not a character"))?;
                        text.push(escaped);
                    }
                    Some(escaped @ ('"' | '\\')) => text.push(escaped),
                    other => return Err(format!("\\ is followed by {other:?}")),
                },
                other => text.push(other),
            }
        }
        Err("its text has no closing quote".to_owned())
    }

    fn end(&self) -> Result<(), String> {
        let rest = self.rest.trim();
        if rest.is_empty() {
            Ok(())
        } else {
            Err(format!("{rest:?} is left over"))
        }
    }

    /// A response's tag: its number and its epoch.
    fn tag(&mut self) -> Result<OpTag, String> {
        let word = self.word()?;
        let numbers = word
            .split_once('/')
            .and_then(|(generation, epoch)| Some((generation.parse().ok()?, epoch.parse().ok()?)));
        let Some((generation, epoch)) = numbers else {
            return Err(format!("{word:?} is not a response and its epoch"));
        };
        Ok(OpTag {
            generation: GenId(generation),
            epoch: Epoch(epoch),
        })
    }

    /// A time on the session's clock.
    fn clock(word: &str) -> Result<MonoTime, String> {
        let (millis, fraction) = word.split_once('.').unwrap_or((word, ""));
        let whole = millis.parse::<u64>().ok();
        let digits = fraction.len() <= 6 && fraction.bytes().all(|byte| byte.is_ascii_digit());
        let rest = format!("{fraction:0<6}").parse::<u64>().ok();
        let nanos = whole
            .filter(|_| digits)
            .and_then(|whole| whole.checked_mul(1_000_000)?.checked_add(rest?));
        nanos
            .map(MonoTime::from_nanos)
            .ok_or_else(|| format!("{word:?} is not a time in milliseconds"))
    }

    fn event(&mut self) -> Result<Event, String> {
        let kind = self.word()?;
        Ok(match kind {
            "speech" => Event::Speech {
                who: ParticipantId(self.number()?),
                edge: self.one_of(&EDGES)?,
                at: MediaTime::from_ticks(self.number()?),
            },
            "transcript" => {
                let who = ParticipantId(self.number()?);
                let start = MediaTime::from_ticks(self.number()?);
                let end = MediaTime::from_ticks(self.number()?);
                let is_final = self.one_of(&[("final", true), ("interim", false)])?;
                Event::Transcript {
                    who,
                    span: MediaSpan::new(start, end),
                    text: self.text()?.as_str().into(),
                    is_final,
                }
            }
            "eou" => Event::Eou {
                turn: TurnId(self.number()?),
                p: Prob::from_basis_points(self.number()?),
            },
            "overlap" => Event::Overlap {
                turn: TurnId(self.number()?),
                verdict: self.one_of(&VERDICTS)?,
            },
            "text" => Event::TextIn {
                who: ParticipantId(self.number()?),
                text: self.text()?.as_str().into(),
            },
            "op" => Event::Op(self.tag()?, self.op()?),
            "tool" => Event::Tool(ToolCallId(self.number()?), self.tool()?),
            "timer" => Event::Timer(self.timer()?),
            "control" => Event::Control(self.control()?),
            "durable" => Event::Durable(Seq(self.number()?)),
            other => return Err(format!("{other:?} is no kind of event")),
        })
    }

    fn op(&mut self) -> Result<OpEvent, String> {
        let kind = self.word()?;
        Ok(match kind {
            "delta" => OpEvent::LlmDelta {
                text: self.text()?.as_str().into(),
            },
            "propose" => OpEvent::ToolProposed {
                tool: ToolId(self.number()?),
                args: self.text()?.as_str().into(),
            },
            "done" => OpEvent::LlmDone {
                finish: self.one_of(&FINISHES)?,
            },
            "segment" => OpEvent::TtsSegment {
                text_end: self.number()?,
                audio_end: MediaDur::from_ticks(self.number()?),
                last: self.one_of(&[("last", true), ("more", false)])?,
            },
            "played" => OpEvent::Played {
                handed: MediaDur::from_ticks(self.number()?),
                acked: MediaDur::from_ticks(self.number()?),
                heard_text: self.number()?,
                drained: self.one_of(&[("drained", true), ("playing", false)])?,
            },
            "failed" => OpEvent::Failed {
                part: self.one_of(&PARTS)?,
                class: self.one_of(&CLASSES)?,
            },
            other => return Err(format!("{other:?} is nothing a response is told")),
        })
    }

    fn tool(&mut self) -> Result<ToolEvent, String> {
        let kind = self.word()?;
        Ok(match kind {
            "completed" => ToolEvent::Completed {
                result: self.text()?.as_str().into(),
            },
            "failed" => ToolEvent::Failed {
                class: self.one_of(&CLASSES)?,
                message: self.text()?.as_str().into(),
            },
            "rolled-back" => ToolEvent::RolledBack,
            other => return Err(format!("{other:?} is no news of a tool call")),
        })
    }

    fn timer(&mut self) -> Result<TimerId, String> {
        let kind = self.word()?;
        Ok(match kind {
            "eou" => TimerId::Eou(TurnId(self.number()?)),
            "turn-max" => TimerId::TurnMax(TurnId(self.number()?)),
            "overlap" => TimerId::Overlap(TurnId(self.number()?)),
            "gen" => TimerId::Gen(GenId(self.number()?)),
            "tool-timeout" => TimerId::ToolTimeout(ToolCallId(self.number()?)),
            "rollback" => TimerId::Rollback(ToolCallId(self.number()?)),
            "checkpoint" => TimerId::Checkpoint,
            "stage-ack" => TimerId::StageAck,
            "drain" => TimerId::Drain,
            "grace" => TimerId::Grace,
            "detach" => TimerId::Detach,
            other => return Err(format!("{other:?} is no timer")),
        })
    }

    fn control(&mut self) -> Result<ControlInput, String> {
        let kind = self.word()?;
        Ok(match kind {
            "say" => ControlInput::Say {
                text: self.text()?.as_str().into(),
            },
            "interrupt" => ControlInput::Interrupt,
            "handoff" => {
                let to = AgentId(self.number()?);
                let context = match self.word()? {
                    "full" => HandoffContext::Full,
                    "summary" => HandoffContext::Summary(self.text()?.as_str().into()),
                    "none" => HandoffContext::None,
                    other => return Err(format!("{other:?} is no way to hand a session over")),
                };
                ControlInput::Handoff { to, context }
            }
            "close" => ControlInput::Close {
                reason: self.one_of(&REASONS)?,
            },
            "join" => ControlInput::Join {
                who: ParticipantId(self.number()?),
            },
            "leave" => ControlInput::Leave {
                who: ParticipantId(self.number()?),
            },
            "detached" => ControlInput::Detached,
            "attached" => ControlInput::Attached,
            "stage-ready" => ControlInput::StageReady,
            "lease-lost" => ControlInput::LeaseLost,
            other => return Err(format!("{other:?} is nothing the application says")),
        })
    }

    fn entry(&mut self) -> Result<Entry, String> {
        let first = self.word()?;
        let entry = if first == "restart" {
            Entry::Restart(Seq(self.number()?))
        } else {
            Entry::Event(Self::clock(first)?, self.event()?)
        };
        self.end()?;
        Ok(entry)
    }
}

impl Trace {
    /// Reads a trace. Empty lines, lines that begin with `#` and lines
    /// that are indented are not part of it: the last are what a kernel
    /// emitted, in the record of a replay.
    ///
    /// # Errors
    ///
    /// A line is not a line of a trace.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut entries = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let skipped = line.trim().is_empty()
                || line.starts_with('#')
                || line.starts_with(char::is_whitespace);
            if skipped {
                continue;
            }
            let entry = Words { rest: line }.entry().map_err(|what| ParseError {
                line: index + 1,
                what,
            })?;
            entries.push(entry);
        }
        Ok(Self { entries })
    }

    /// Adds an event that arrived at `at`.
    pub fn event(&mut self, at: MonoTime, event: &Event) {
        self.entries.push(Entry::Event(at, event.clone()));
    }

    /// Adds the death of the process, when the facts up to `stored` had
    /// reached the disk.
    pub fn restart(&mut self, stored: Seq) {
        self.entries.push(Entry::Restart(stored));
    }
}

/// What a step emitted, as the lines written under the line that caused
/// it: the facts, marked `=`, and then the commands, marked `>`.
struct Emitted<'a>(&'a Out);

impl fmt::Display for Emitted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let out = self.0;
        let text = |at: Option<_>| at.map_or("", |at| out.text.get(at));
        for stamped in &out.facts {
            write!(f, "    = #{} {:?}", stamped.seq, stamped.fact)?;
            match text(stamped.fact.text()) {
                "" => writeln!(f)?,
                carried => writeln!(f, " {}", Quoted(carried))?,
            }
        }
        for command in &out.cmds {
            write!(f, "    > {command:?}")?;
            match text(command.text()) {
                "" => writeln!(f)?,
                carried => writeln!(f, " {}", Quoted(carried))?,
            }
        }
        Ok(())
    }
}

/// Steps a new kernel through `trace` with a [`Monitor`] watching, and
/// checks at the end that its journal adds up to what it holds. Returns
/// the trace with what the kernel emitted under each line.
///
/// `policies` makes the kernel's policies, once at the beginning and once
/// more for every restart.
///
/// # Errors
///
/// The configuration is not valid, the session cannot be rebuilt at a
/// restart, or an invariant is broken. The report holds the record up to
/// there.
pub fn replay<P: Policies>(
    config: &KernelConfig,
    policies: impl Fn() -> P,
    trace: &Trace,
) -> Result<String, Report> {
    let mut record = String::new();
    let failed = |record: &str, violation: &Violation| Report(format!("{record}{violation}"));
    let mut kernel = Kernel::new(config.clone(), policies())
        .map_err(|error| Report(format!("the configuration is not valid: {error}")))?;
    let mut monitor = Monitor::new(kernel.view());
    let mut journal = Journal::new(kernel.snapshot());
    let mut out = Out::new();
    for entry in &trace.entries {
        record.push_str(&entry.to_string());
        record.push('\n');
        match entry {
            Entry::Event(at, event) => {
                kernel.step(*at, event, &mut out);
                journal.record(&out);
                record.push_str(&Emitted(&out).to_string());
                monitor
                    .observe(*at, event, &out, kernel.view())
                    .map_err(|violation| failed(&record, &violation))?;
            }
            Entry::Restart(stored) => {
                journal
                    .check(&kernel)
                    .map_err(|violation| failed(&record, &violation))?;
                journal.cut(*stored);
                let rebuilt = journal.latest().map(|(snapshot, tail, text)| {
                    Kernel::restore(config.clone(), policies(), snapshot, tail, text)
                });
                kernel = match rebuilt {
                    Some(Ok(kernel)) => kernel,
                    Some(Err(error)) => {
                        let violation = Violation {
                            rule: Rule::Restore,
                            what: format!(
                                "the session cannot be rebuilt from its journal: {error}"
                            ),
                        };
                        return Err(failed(&record, &violation));
                    }
                    None => continue,
                };
                monitor = Monitor::restored(kernel.view(), journal.speech());
            }
        }
    }
    journal
        .check(&kernel)
        .map_err(|violation| failed(&record, &violation))?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use waveport_domain::config::KernelConfig;
    use waveport_domain::event::{
        CloseReason, ControlInput, Event, Finish, HandoffContext, OpEvent, OverlapVerdict, Part,
        RetryClass, TimerId, ToolEvent, VadEdge,
    };
    use waveport_domain::ids::{
        AgentId, Epoch, GenId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId, TurnId,
    };
    use waveport_domain::policy::DefaultPolicies;
    use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

    use super::{Entry, Trace, replay};

    /// One event of every kind, and of every kind within each kind.
    #[expect(
        clippy::too_many_lines,
        reason = "a list, with one event of every kind in it"
    )]
    fn everything() -> Vec<Event> {
        let who = ParticipantId(2);
        let turn = TurnId(7);
        let call = ToolCallId(u64::MAX);
        let tag = OpTag {
            generation: GenId(3),
            epoch: Epoch(9),
        };
        let at = MediaTime::from_ticks;
        let len = MediaDur::from_ticks;
        let odd = "a \"quoted\" \\ back\nline\ttab\r \u{1} naïve 日本";
        let ops = [
            OpEvent::LlmDelta { text: odd.into() },
            OpEvent::LlmDelta { text: "".into() },
            OpEvent::ToolProposed {
                tool: ToolId(4),
                args: "{\"n\":1}".into(),
            },
            OpEvent::LlmDone {
                finish: Finish::Stop,
            },
            OpEvent::LlmDone {
                finish: Finish::ToolCalls,
            },
            OpEvent::LlmDone {
                finish: Finish::Length,
            },
            OpEvent::LlmDone {
                finish: Finish::Filtered,
            },
            OpEvent::TtsSegment {
                text_end: 12,
                audio_end: len(96_000),
                last: false,
            },
            OpEvent::TtsSegment {
                text_end: u32::MAX,
                audio_end: len(u32::MAX),
                last: true,
            },
            OpEvent::Played {
                handed: len(960),
                acked: len(0),
                heard_text: 5,
                drained: false,
            },
            OpEvent::Played {
                handed: len(960),
                acked: len(960),
                heard_text: 12,
                drained: true,
            },
            OpEvent::Failed {
                part: Part::Think,
                class: RetryClass::Transient,
            },
            OpEvent::Failed {
                part: Part::Speak,
                class: RetryClass::RateLimited,
            },
            OpEvent::Failed {
                part: Part::Play,
                class: RetryClass::Auth,
            },
        ];
        let tools = [
            ToolEvent::Completed { result: odd.into() },
            ToolEvent::Failed {
                class: RetryClass::Permanent,
                message: "no such order".into(),
            },
            ToolEvent::Failed {
                class: RetryClass::Timeout,
                message: "".into(),
            },
            ToolEvent::RolledBack,
        ];
        let timers = [
            TimerId::Eou(turn),
            TimerId::TurnMax(turn),
            TimerId::Overlap(turn),
            TimerId::Gen(tag.generation),
            TimerId::ToolTimeout(call),
            TimerId::Rollback(call),
            TimerId::Checkpoint,
            TimerId::StageAck,
            TimerId::Drain,
            TimerId::Grace,
            TimerId::Detach,
        ];
        let to = AgentId(2);
        let handoff = |context| ControlInput::Handoff { to, context };
        let close = |reason| ControlInput::Close { reason };
        let controls = [
            ControlInput::Say { text: odd.into() },
            ControlInput::Interrupt,
            handoff(HandoffContext::Full),
            handoff(HandoffContext::Summary("Ana wants a refund.".into())),
            handoff(HandoffContext::None),
            close(CloseReason::Requested),
            close(CloseReason::Abandoned),
            close(CloseReason::Failure),
            ControlInput::Join { who },
            ControlInput::Leave { who },
            ControlInput::Detached,
            ControlInput::Attached,
            ControlInput::StageReady,
            ControlInput::LeaseLost,
        ];
        let edge = |edge| Event::Speech {
            who,
            edge,
            at: at(4_800),
        };
        let heard = |is_final| Event::Transcript {
            who,
            span: MediaSpan::new(at(4_800), at(52_800)),
            text: "where is my order".into(),
            is_final,
        };
        let overlap = |verdict| Event::Overlap { turn, verdict };
        let mut events = vec![
            edge(VadEdge::Start),
            edge(VadEdge::End),
            heard(true),
            heard(false),
            Event::Eou {
                turn,
                p: Prob::from_basis_points(7_250),
            },
            overlap(OverlapVerdict::Backchannel),
            overlap(OverlapVerdict::BargeIn),
            overlap(OverlapVerdict::Noise),
            Event::TextIn {
                who,
                text: odd.into(),
            },
            Event::Durable(Seq(41)),
        ];
        events.extend(ops.map(|op| Event::Op(tag, op)));
        events.extend(tools.map(|news| Event::Tool(call, news)));
        events.extend(timers.map(Event::Timer));
        events.extend(controls.map(Event::Control));
        events
    }

    #[test]
    fn every_event_reads_back_as_it_was_written() {
        let mut trace = Trace::default();
        for (step, event) in (0..).zip(everything()) {
            // Whole milliseconds, and times between them.
            let at = MonoTime::from_nanos(step * 1_000_000 + (step % 3) * 1_500);
            trace.event(at, &event);
        }
        trace.restart(Seq(12));
        let text = trace.to_string();
        assert_eq!(Trace::parse(&text), Ok(trace), "{text}");
        assert!(text.contains("\n3 transcript 2 4800 52800 interim \"where is my order\"\n"));
        assert!(text.contains("\n4.0015 eou 7 7250\n"), "{text}");
        assert!(text.ends_with("\nrestart 12\n"));
    }

    #[test]
    fn text_keeps_to_its_line() {
        let odd = Event::TextIn {
            who: ParticipantId(1),
            text: "one\ntwo \"three\" \\ \u{7f}".into(),
        };
        let line = Entry::Event(MonoTime::from_millis(5), odd).to_string();
        assert_eq!(line, r#"5 text 1 "one\ntwo \"three\" \\ \u{7f}""#);
    }

    #[test]
    fn what_is_not_a_trace_is_refused_with_its_line() {
        let refused = |text: &str| {
            let error = Trace::parse(text).unwrap_err();
            (error.line, error.what)
        };
        let (line, what) = refused("0 control attached\n\n12 speech 1 begin 0\n");
        assert_eq!(line, 3);
        assert_eq!(what, "\"begin\" is none of start, end");
        assert_eq!(refused("soon control attached").0, 1);
        assert_eq!(refused("1.0000001 control attached").0, 1);
        assert_eq!(refused("1 control attached now").1, "\"now\" is left over");
        assert_eq!(
            refused("1 text 1 \"open").1,
            "its text has no closing quote"
        );
        assert_eq!(refused("1 text 1 open").0, 1);
        assert_eq!(refused("1 op 1 done stop").0, 1);
        assert_eq!(refused("1 eou 1 70000").0, 1);
        assert_eq!(refused("1 durable").1, "it ends too soon");
        assert_eq!(refused("1 dance").1, "\"dance\" is no kind of event");
    }

    #[test]
    fn a_record_of_a_replay_is_a_trace_and_replays_to_itself() {
        let text = "\
# Ana asks, and the agent answers.
0 control join 1
100 speech 1 start 4800
1100 speech 1 end 52800
1160 eou 1 10000
1300 transcript 1 4800 52800 final \"where is my order\"
1420 op 1/0 delta \"It shipped on Monday. \"
1500 op 1/0 done stop
";
        let trace = Trace::parse(text).unwrap();
        assert_eq!(trace.entries.len(), 7);
        let config = KernelConfig::default();
        let record = replay(&config, DefaultPolicies::default, &trace).unwrap();
        assert!(record.contains("\n    = #2 TurnCommitted {"), "{record}");
        assert!(record.contains("\"where is my order\"\n    > "), "{record}");
        assert!(record.contains("\n    > Tts(OpTag {"), "{record}");

        assert_eq!(Trace::parse(&record), Ok(trace.clone()));
        let again = replay(&config, DefaultPolicies::default, &trace).unwrap();
        assert_eq!(again, record);
    }

    #[test]
    fn a_replay_goes_on_from_the_journal_at_a_restart() {
        let text = "\
0 control join 1
100 text 1 \"where is my order\"
restart 1
200 control attached
";
        let trace = Trace::parse(text).unwrap();
        let config = KernelConfig::default();
        let record = replay(&config, DefaultPolicies::default, &trace).unwrap();
        // The turn never reached the disk, so nothing is owed for it.
        assert!(record.contains("restart 1\n200 control attached\n    = #2 EpochAdvanced"));
        assert!(!record.contains("Recovered"), "{record}");

        let kept = text.replace("restart 1", "restart 2");
        let record = replay(
            &config,
            DefaultPolicies::default,
            &Trace::parse(&kept).unwrap(),
        )
        .unwrap();
        assert!(record.contains("Recovered"), "{record}");
        assert!(record.contains("    > Llm(OpTag {"), "{record}");
    }
}
