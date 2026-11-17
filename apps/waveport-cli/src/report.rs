//! The call, as it is told on the terminal.
//!
//! A session tells whoever watches it what it has recorded, on its own
//! thread, and must not be kept waiting. So what is to be printed is put
//! in a queue here, and printed by whoever started the call.

use core::fmt::Write as _;
use core::time::Duration;
use std::sync::{Mutex, PoisonError};

use waveport_domain::event::CloseReason;
use waveport_domain::fact::{Fact, Lifecycle, Stamped, ToolOutcome};
use waveport_domain::ids::{GenId, ToolCallId, ToolId};
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;
use waveport_ports::journal::SessionId;
use waveport_ports::telemetry::{EventSink, Stage, StageMark};

/// How many responses and calls are remembered while they are under way.
const REMEMBERED: usize = 32;

/// What is known of a call that is not yet told.
#[derive(Debug, Default)]
struct Notes {
    /// When the caller's last turn ended.
    turn_ended: Option<MonoTime>,
    /// How long after that the first sound of each response went out.
    first_sound: Vec<(GenId, Duration)>,
    /// The tool calls that were asked for and have not come back.
    calls: Vec<(ToolCallId, ToolId, String)>,
    /// Why the call is ending.
    reason: Option<CloseReason>,
}

/// Where the lines of a call go. It is called on the session's own
/// thread, and must not keep it waiting.
pub(crate) type Tell = Box<dyn Fn(String) + Send + Sync>;

/// Tells a call line by line.
pub(crate) struct Reporter {
    tell: Tell,
    /// What the tools are called, in the kernel's order.
    tools: Vec<String>,
    notes: Mutex<Notes>,
}

impl core::fmt::Debug for Reporter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reporter")
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl Reporter {
    /// A reporter that gives its lines to `tell`.
    pub(crate) fn new(tell: Tell, tools: Vec<String>) -> Self {
        Self {
            tell,
            tools,
            notes: Mutex::default(),
        }
    }

    fn say(&self, line: String) {
        (self.tell)(line);
    }

    fn line(&self, notes: &mut Notes, fact: &Fact, text: &TextArena) -> Option<String> {
        match *fact {
            Fact::TurnCommitted { text: said, .. } => Some(format!("caller: {}", text.get(said))),
            Fact::KeysCommitted { keys, .. } => Some(format!("caller pressed: {}", text.get(keys))),
            Fact::TurnDismissed { reason, .. } => Some(format!(
                "(the caller made a sound that was not a turn: {reason:?})"
            )),
            Fact::UtteranceCommitted {
                generation,
                text: said,
                truncated,
                ..
            } => {
                let mut line = format!("agent: {}", text.get(said));
                if truncated {
                    line.push_str(" [cut off]");
                }
                let found = notes
                    .first_sound
                    .iter()
                    .position(|(of, _)| *of == generation);
                if let Some(found) = found {
                    let (_, after) = notes.first_sound.remove(found);
                    let _ = write!(line, "  [first sound after {} ms]", after.as_millis());
                }
                Some(line)
            }
            Fact::ToolProposed {
                call, tool, args, ..
            } => {
                if notes.calls.len() >= REMEMBERED {
                    notes.calls.remove(0);
                }
                notes.calls.push((call, tool, text.get(args).to_owned()));
                None
            }
            Fact::ToolFinished {
                call,
                outcome,
                result,
            } => {
                let how = match outcome {
                    ToolOutcome::Completed => "",
                    ToolOutcome::Failed => " [failed]",
                    ToolOutcome::Cancelled => " [cancelled]",
                    ToolOutcome::InDoubt => " [in doubt]",
                };
                let result = text.get(result);
                // A call that nobody here saw asked for was asked for
                // before the call broke off and was gone on with.
                let Some(found) = notes.calls.iter().position(|(of, _, _)| *of == call) else {
                    return Some(format!(
                        "tool: a call from before the call broke off -> {result}{how}"
                    ));
                };
                let (_, tool, args) = notes.calls.remove(found);
                let name = self
                    .tools
                    .get(usize::from(tool.0))
                    .map_or("a tool nobody named", String::as_str);
                Some(format!("tool: {name}({args}) -> {result}{how}"))
            }
            Fact::LifecycleChanged { to, reason } => {
                notes.reason = reason.or(notes.reason);
                (to == Lifecycle::Closed).then(|| {
                    let why = match notes.reason {
                        Some(CloseReason::Requested) => "it was asked to",
                        Some(CloseReason::Abandoned) => "the caller left",
                        Some(CloseReason::Failure) => "it could not go on",
                        None => "nobody said why",
                    };
                    format!("the call ended: {why}")
                })
            }
            _ => None,
        }
    }
}

impl EventSink for Reporter {
    fn facts(&self, _session: SessionId, _at: MonoTime, facts: &[Stamped], text: &TextArena) {
        let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        for stamped in facts {
            if let Some(line) = self.line(&mut notes, &stamped.fact, text) {
                self.say(line);
            }
        }
    }

    fn stage(&self, _session: SessionId, mark: StageMark) {
        let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        match mark.stage {
            Stage::TurnEnded(_) => notes.turn_ended = Some(mark.at),
            Stage::FirstFrameOut(generation) => {
                if let Some(ended) = notes.turn_ended {
                    if notes.first_sound.len() >= REMEMBERED {
                        notes.first_sound.remove(0);
                    }
                    notes.first_sound.push((generation, mark.at.since(ended)));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use waveport_domain::event::CloseReason;
    use waveport_domain::fact::{Fact, Lifecycle, Stamped, ToolOutcome};
    use waveport_domain::ids::{
        AgentId, BatchId, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
    };
    use waveport_domain::text::TextArena;
    use waveport_domain::time::{MediaSpan, MediaTime, MonoTime};
    use waveport_ports::journal::SessionId;
    use waveport_ports::telemetry::{EventSink, Stage, StageMark};

    use super::Reporter;

    fn reporter() -> (Reporter, UnboundedReceiver<String>) {
        let (lines, printed) = unbounded_channel();
        let tools = vec!["get_time".to_owned(), "end_call".to_owned()];
        let tell = Box::new(move |line| {
            let _ = lines.send(line);
        });
        (Reporter::new(tell, tools), printed)
    }

    /// Tells the reporter one fact, made with the text it is about.
    fn tell(
        reporter: &Reporter,
        said: &str,
        fact: impl Fn(waveport_domain::text::TextRef) -> Fact,
    ) {
        let mut text = TextArena::default();
        let at = text.push(said);
        let stamped = Stamped {
            seq: Seq(1),
            fact: fact(at),
        };
        reporter.facts(SessionId(1), MonoTime::ZERO, &[stamped], &text);
    }

    fn stage(reporter: &Reporter, at_ms: u64, stage: Stage) {
        let at = MonoTime::from_millis(at_ms);
        reporter.stage(SessionId(1), StageMark { at, stage });
    }

    #[test]
    fn keys_are_told_as_keys() {
        let (reporter, mut printed) = reporter();
        tell(&reporter, "4071", |keys| Fact::KeysCommitted {
            turn: TurnId(2),
            who: ParticipantId(0),
            keys,
        });
        assert_eq!(printed.try_recv().unwrap(), "caller pressed: 4071");
        assert!(printed.try_recv().is_err());
    }

    #[test]
    fn a_call_is_told_as_it_went() {
        let (reporter, mut printed) = reporter();
        let span = MediaSpan::new(MediaTime::from_millis(0), MediaTime::from_millis(900));
        tell(&reporter, "What time is it?", |text| Fact::TurnCommitted {
            turn: TurnId(1),
            who: ParticipantId(0),
            span,
            text,
            addressed: true,
        });
        stage(&reporter, 1_000, Stage::TurnEnded(TurnId(1)));
        tell(&reporter, "{}", |args| Fact::ToolProposed {
            call: ToolCallId(1),
            batch: BatchId(1),
            generation: GenId(1),
            tool: ToolId(0),
            args,
        });
        assert_eq!(printed.try_recv().unwrap(), "caller: What time is it?");
        assert!(printed.try_recv().is_err());

        tell(&reporter, r#"{"time":"09:07"}"#, |result| {
            Fact::ToolFinished {
                call: ToolCallId(1),
                outcome: ToolOutcome::Completed,
                result,
            }
        });
        assert_eq!(
            printed.try_recv().unwrap(),
            r#"tool: get_time({}) -> {"time":"09:07"}"#
        );

        stage(&reporter, 1_640, Stage::FirstFrameOut(GenId(2)));
        tell(&reporter, "It is seven past nine.", |text| {
            Fact::UtteranceCommitted {
                generation: GenId(2),
                agent: AgentId(0),
                answers: None,
                text,
                truncated: false,
            }
        });
        assert_eq!(
            printed.try_recv().unwrap(),
            "agent: It is seven past nine.  [first sound after 640 ms]"
        );

        tell(&reporter, "It is", |text| Fact::UtteranceCommitted {
            generation: GenId(3),
            agent: AgentId(0),
            answers: None,
            text,
            truncated: true,
        });
        assert_eq!(printed.try_recv().unwrap(), "agent: It is [cut off]");
    }

    #[test]
    fn the_end_of_a_call_says_why() {
        let (reporter, mut printed) = reporter();
        let change = |to, reason| {
            let fact = Fact::LifecycleChanged { to, reason };
            tell(&reporter, "", |_| fact);
        };
        change(Lifecycle::Draining, Some(CloseReason::Requested));
        change(Lifecycle::Closing, None);
        assert!(printed.try_recv().is_err());
        change(Lifecycle::Closed, None);
        assert_eq!(
            printed.try_recv().unwrap(),
            "the call ended: it was asked to"
        );
    }

    #[test]
    fn a_tool_that_failed_is_said_to_have() {
        let (reporter, mut printed) = reporter();
        tell(&reporter, "{}", |args| Fact::ToolProposed {
            call: ToolCallId(4),
            batch: BatchId(1),
            generation: GenId(1),
            tool: ToolId(1),
            args,
        });
        tell(&reporter, "No.", |result| Fact::ToolFinished {
            call: ToolCallId(4),
            outcome: ToolOutcome::Failed,
            result,
        });
        assert_eq!(
            printed.try_recv().unwrap(),
            "tool: end_call({}) -> No. [failed]"
        );
    }

    #[test]
    fn a_call_of_a_tool_from_before_the_call_broke_off_is_told_too() {
        // A reporter that began with the process that went on with the
        // call never saw the tool asked for.
        let (reporter, mut printed) = reporter();
        tell(&reporter, "nobody knows whether it took effect", |result| {
            Fact::ToolFinished {
                call: ToolCallId(9),
                outcome: ToolOutcome::InDoubt,
                result,
            }
        });
        assert_eq!(
            printed.try_recv().unwrap(),
            "tool: a call from before the call broke off -> nobody knows whether it took \
             effect [in doubt]"
        );
    }
}
