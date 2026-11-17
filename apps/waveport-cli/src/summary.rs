//! What a call came to, in numbers.
//!
//! With `--report <FILE>` the command writes, when the call is over, how
//! it went: each turn of the caller with how long each part of the
//! answer took, and how those lengths are spread over the call. It is
//! made from the same marks as the traces and the metrics, by the same
//! tracker, so the three agree.
//!
//! No word of the call is in the report: what was said is in the
//! journal, and a report is something to pass on.

use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};
use waveport_adapter_kit::turns::{Spread, Stretch, Tracker, What};
use waveport_domain::event::CloseReason;
use waveport_domain::fact::{Fact, Stamped, ToolOutcome};
use waveport_domain::ids::{ToolCallId, TurnId};
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;
use waveport_ports::journal::SessionId;
use waveport_ports::telemetry::{EventSink, Stage, StageMark};
use waveport_runtime::session::{Ended, Rebuilt};
use waveport_runtime::shard::Exit;

/// The version of the report's shape.
const VERSION: u32 = 1;

/// How many turns are told one by one. The turns of a longer call are
/// still counted, and their lengths still spread.
const TURNS: usize = 2_000;

/// A tool call of a turn.
#[derive(Debug)]
struct Called {
    call: ToolCallId,
    tool: String,
    clearing: Option<Duration>,
    calling: Option<Duration>,
    outcome: Option<ToolOutcome>,
}

/// A turn of the caller, and the answer to it.
#[derive(Debug)]
struct Turn {
    id: TurnId,
    ended: MonoTime,
    addressed: bool,
    waited: Option<Duration>,
    deciding: Option<Duration>,
    /// Once for each time the model was asked.
    thinking: Vec<Duration>,
    /// These three are of the first response that was spoken.
    phrasing: Option<Duration>,
    voicing: Option<Duration>,
    sending: Option<Duration>,
    yielding: Option<Duration>,
    tools: Vec<Called>,
    cut_off: bool,
}

#[derive(Debug, Default)]
struct Counts {
    turns: u64,
    dismissed: u64,
    utterances: u64,
    cut_off: u64,
    tool_calls: u64,
    tool_calls_not_completed: u64,
}

#[derive(Debug, Default)]
struct Inner {
    tracker: Tracker,
    turns: Vec<Turn>,
    /// How long each kind of part took, every time.
    parts: BTreeMap<&'static str, Vec<Duration>>,
    counts: Counts,
    /// When the call was last heard of, on its own clock.
    last: MonoTime,
    why: Option<CloseReason>,
}

/// Follows one call, to say afterwards what it came to.
#[derive(Debug)]
pub(crate) struct Summary {
    /// What the tools are called, in the kernel's order.
    tools: Vec<String>,
    inner: Mutex<Inner>,
}

impl Summary {
    pub(crate) fn new(tools: Vec<String>) -> Self {
        Self {
            tools,
            inner: Mutex::default(),
        }
    }

    /// The report of the call `id`, which left as `exit` says.
    pub(crate) fn report(&self, id: SessionId, rebuilt: Option<Rebuilt>, exit: &Exit) -> Value {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let ended = match exit {
            Exit::Ended(Ended::Closed) => "closed",
            Exit::Ended(Ended::Lost(_)) => "lost",
            Exit::Panicked => "panicked",
            Exit::Stopped => "stopped",
        };
        let why = inner.why.map(|why| match why {
            CloseReason::Requested => "requested",
            CloseReason::Abandoned => "abandoned",
            CloseReason::Failure => "failure",
        });
        let rebuilt = rebuilt.map(|from| {
            json!({
                "state": from.state.map(waveport_domain::ids::Seq::get),
                "records": from.records,
            })
        });
        let mut parts = serde_json::Map::new();
        let mut waited = Value::Null;
        for (name, lengths) in &mut inner.parts {
            let spread = spread(lengths);
            if *name == What::Answering(TurnId(0), waveport_domain::ids::GenId(0)).name() {
                waited = spread;
            } else {
                parts.insert((*name).to_owned(), spread);
            }
        }
        let counts = &inner.counts;
        json!({
            "v": VERSION,
            "session": id.to_string(),
            "ended": ended,
            "why": why,
            "rebuilt": rebuilt,
            "length_ms": ms(inner.last.since(MonoTime::ZERO)),
            "counts": {
                "turns": counts.turns,
                "dismissed": counts.dismissed,
                "utterances": counts.utterances,
                "cut_off": counts.cut_off,
                "tool_calls": counts.tool_calls,
                "tool_calls_not_completed": counts.tool_calls_not_completed,
            },
            "waited_ms": waited,
            "parts_ms": parts,
            "turns": inner.turns.iter().map(turn).collect::<Vec<Value>>(),
        })
    }

    /// Takes a part of a turn that has ended.
    fn timed(inner: &mut Inner, stretch: &Stretch) {
        let took = stretch.took();
        inner
            .parts
            .entry(stretch.what.name())
            .or_default()
            .push(took);
        match stretch.what {
            What::Deciding(of) | What::Answering(of, _) => {
                let turns = inner.turns.iter_mut();
                if let Some(turn) = turns.rev().find(|turn| turn.id == of) {
                    if matches!(stretch.what, What::Deciding(_)) {
                        turn.deciding = Some(took);
                    } else {
                        turn.waited = Some(took);
                    }
                }
            }
            // What the agent does is for the turn that ended last. What
            // it does before any turn, a greeting for one, is counted
            // with the parts and told with no turn.
            What::Thinking(_) => {
                if let Some(turn) = inner.turns.last_mut() {
                    turn.thinking.push(took);
                }
            }
            What::Phrasing(_) => first(inner, took, |turn| &mut turn.phrasing),
            What::Voicing(_) => first(inner, took, |turn| &mut turn.voicing),
            What::Sending(_) => first(inner, took, |turn| &mut turn.sending),
            What::Yielding(_) => first(inner, took, |turn| &mut turn.yielding),
            What::Clearing(call) | What::Calling(call) => {
                let turns = inner.turns.iter_mut().rev();
                let mut calls = turns.flat_map(|turn| turn.tools.iter_mut().rev());
                if let Some(called) = calls.find(|called| called.call == call) {
                    if matches!(stretch.what, What::Clearing(_)) {
                        called.clearing = Some(took);
                    } else {
                        called.calling = Some(took);
                    }
                }
            }
        }
    }

    /// Takes a fact.
    fn fact(&self, inner: &mut Inner, fact: &Fact) {
        match *fact {
            Fact::TurnCommitted {
                turn, addressed, ..
            } => {
                inner.counts.turns += 1;
                let turns = inner.turns.iter_mut();
                if let Some(turn) = turns.rev().find(|known| known.id == turn) {
                    turn.addressed = addressed;
                }
            }
            // Keys are a turn, and one that is always for the agent.
            Fact::KeysCommitted { .. } => inner.counts.turns += 1,
            Fact::TurnDismissed { .. } => inner.counts.dismissed += 1,
            Fact::UtteranceCommitted { truncated, .. } => {
                inner.counts.utterances += 1;
                if truncated {
                    inner.counts.cut_off += 1;
                    if let Some(turn) = inner.turns.last_mut() {
                        turn.cut_off = true;
                    }
                }
            }
            Fact::ToolProposed { call, tool, .. } => {
                let name = self.tools.get(usize::from(tool.get()));
                if let Some(turn) = inner.turns.last_mut() {
                    turn.tools.push(Called {
                        call,
                        tool: name.map_or_else(|| format!("tool {}", tool.get()), Clone::clone),
                        clearing: None,
                        calling: None,
                        outcome: None,
                    });
                }
            }
            Fact::ToolFinished { call, outcome, .. } => {
                inner.counts.tool_calls += 1;
                if outcome != ToolOutcome::Completed {
                    inner.counts.tool_calls_not_completed += 1;
                }
                let turns = inner.turns.iter_mut().rev();
                let mut calls = turns.flat_map(|turn| turn.tools.iter_mut().rev());
                if let Some(called) = calls.find(|called| called.call == call) {
                    called.outcome = Some(outcome);
                }
            }
            Fact::LifecycleChanged { reason, .. } => inner.why = reason.or(inner.why),
            _ => {}
        }
    }
}

impl EventSink for Summary {
    fn facts(&self, _: SessionId, at: MonoTime, facts: &[Stamped], _: &TextArena) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.last = inner.last.max(at);
        for stamped in facts {
            self.fact(&mut inner, &stamped.fact);
        }
        // Last: the tracker forgets a call when it is told that the
        // call is over.
        inner.tracker.facts(at, facts);
    }

    fn stage(&self, _: SessionId, mark: StageMark) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.last = inner.last.max(mark.at);
        if let Stage::TurnEnded(turn) = mark.stage
            && inner.turns.len() < TURNS
        {
            inner.turns.push(Turn {
                id: turn,
                ended: mark.at,
                addressed: true,
                waited: None,
                deciding: None,
                thinking: Vec::new(),
                phrasing: None,
                voicing: None,
                sending: None,
                yielding: None,
                tools: Vec::new(),
                cut_off: false,
            });
        }
        // A mark ends two parts of a turn at most.
        let mut found = [None, None];
        let mut next = found.iter_mut();
        inner.tracker.mark(mark, |stretch| {
            if let Some(place) = next.next() {
                *place = Some(stretch);
            }
        });
        for stretch in found.into_iter().flatten() {
            Self::timed(&mut inner, &stretch);
        }
    }
}

/// Sets one of the lengths of the turn that ended last, if it is not set
/// yet.
fn first(inner: &mut Inner, took: Duration, of: impl Fn(&mut Turn) -> &mut Option<Duration>) {
    if let Some(turn) = inner.turns.last_mut() {
        let length = of(turn);
        *length = length.or(Some(took));
    }
}

/// A length of time in milliseconds, to a thousandth.
fn ms(length: Duration) -> f64 {
    (length.as_secs_f64() * 1_000_000.0).round() / 1_000.0
}

fn optional(length: Option<Duration>) -> Value {
    length.map_or(Value::Null, |length| json!(ms(length)))
}

/// How lengths of time are spread, in milliseconds.
fn spread(lengths: &mut [Duration]) -> Value {
    Spread::of(lengths).map_or(Value::Null, |spread| {
        json!({
            "count": spread.count,
            "min": ms(spread.min),
            "p50": ms(spread.p50),
            "p95": ms(spread.p95),
            "p99": ms(spread.p99),
            "max": ms(spread.max),
        })
    })
}

/// A turn, as the report tells it.
fn turn(turn: &Turn) -> Value {
    let tools: Vec<Value> = turn
        .tools
        .iter()
        .map(|called| {
            let outcome = called.outcome.map(|outcome| match outcome {
                ToolOutcome::Completed => "completed",
                ToolOutcome::Failed => "failed",
                ToolOutcome::Cancelled => "cancelled",
                ToolOutcome::InDoubt => "in_doubt",
            });
            json!({
                "tool": called.tool,
                "clearing_ms": optional(called.clearing),
                "calling_ms": optional(called.calling),
                "outcome": outcome,
            })
        })
        .collect();
    json!({
        "turn": turn.id.get(),
        "ended_ms": ms(turn.ended.since(MonoTime::ZERO)),
        "addressed": turn.addressed,
        "waited_ms": optional(turn.waited),
        "deciding_ms": optional(turn.deciding),
        "thinking_ms": turn.thinking.iter().map(|took| ms(*took)).collect::<Vec<f64>>(),
        "phrasing_ms": optional(turn.phrasing),
        "voicing_ms": optional(turn.voicing),
        "sending_ms": optional(turn.sending),
        "yielding_ms": optional(turn.yielding),
        "tools": tools,
        "cut_off": turn.cut_off,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use waveport_domain::event::CloseReason;
    use waveport_domain::fact::{Fact, Lifecycle, Stamped, ToolOutcome};
    use waveport_domain::ids::{
        AgentId, BatchId, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
    };
    use waveport_domain::text::TextArena;
    use waveport_domain::time::{MediaSpan, MediaTime, MonoTime};
    use waveport_ports::journal::SessionId;
    use waveport_ports::telemetry::{EventSink, Stage, StageMark};
    use waveport_runtime::session::{Ended, Rebuilt};
    use waveport_runtime::shard::Exit;

    use super::Summary;

    const QUESTION: &str = "What time is it in Lima?";
    const ANSWER: &str = "It is seven past nine.";

    struct Told {
        summary: Summary,
        text: TextArena,
    }

    impl Told {
        fn new() -> Self {
            Self {
                summary: Summary::new(vec!["get_time".to_owned()]),
                text: TextArena::default(),
            }
        }

        fn mark(&self, at_ms: u64, stage: Stage) {
            let at = MonoTime::from_millis(at_ms);
            self.summary.stage(SessionId(1), StageMark { at, stage });
        }

        fn fact(&self, at_ms: u64, fact: Fact) {
            let stamped = Stamped { seq: Seq(1), fact };
            let at = MonoTime::from_millis(at_ms);
            self.summary.facts(SessionId(1), at, &[stamped], &self.text);
        }

        fn turn(&mut self, at_ms: u64, turn: u32) {
            self.mark(at_ms - 70, Stage::TranscriptFinal(TurnId(turn)));
            self.mark(at_ms, Stage::TurnEnded(TurnId(turn)));
            let fact = Fact::TurnCommitted {
                turn: TurnId(turn),
                who: ParticipantId(1),
                span: MediaSpan::new(MediaTime::from_millis(0), MediaTime::from_millis(900)),
                text: self.text.push(QUESTION),
                addressed: true,
            };
            self.fact(at_ms, fact);
        }

        fn spoken(&self, at_ms: u64, generation: u32) {
            let generation = GenId(generation);
            self.mark(at_ms, Stage::Asked(generation));
            self.mark(at_ms + 200, Stage::FirstToken(generation));
            self.mark(at_ms + 220, Stage::FirstText(generation));
            self.mark(at_ms + 320, Stage::FirstAudio(generation));
            self.mark(at_ms + 325, Stage::FirstFrameOut(generation));
        }

        fn uttered(&mut self, at_ms: u64, generation: u32, truncated: bool) {
            let fact = Fact::UtteranceCommitted {
                generation: GenId(generation),
                agent: AgentId(0),
                answers: None,
                text: self.text.push(ANSWER),
                truncated,
            };
            self.fact(at_ms, fact);
        }
    }

    #[test]
    fn a_call_is_summed_up_turn_by_turn_and_without_its_words() {
        let mut told = Told::new();
        // A greeting, which answers no turn.
        told.mark(100, Stage::FirstText(GenId(1)));
        told.mark(180, Stage::FirstAudio(GenId(1)));
        told.mark(180, Stage::FirstFrameOut(GenId(1)));
        told.uttered(1_000, 1, false);

        // A turn that is answered after a tool was asked.
        let call = ToolCallId(7);
        told.turn(2_470, 1);
        told.mark(2_470, Stage::Asked(GenId(2)));
        told.mark(2_670, Stage::FirstToken(GenId(2)));
        let args = told.text.push("{\"city\":\"Lima\"}");
        told.fact(
            2_670,
            Fact::ToolProposed {
                call,
                batch: BatchId(1),
                generation: GenId(2),
                tool: ToolId(0),
                args,
            },
        );
        told.mark(2_675, Stage::ToolSent(call));
        told.mark(2_825, Stage::ToolBack(call));
        let result = told.text.push("09:07");
        told.fact(
            2_825,
            Fact::ToolFinished {
                call,
                outcome: ToolOutcome::Completed,
                result,
            },
        );
        told.spoken(2_825, 3);

        // The caller speaks over the answer, and is answered again.
        told.mark(4_000, Stage::Lowered(GenId(3)));
        // As a session tells it: the utterance first, then the stop.
        told.uttered(4_240, 3, true);
        told.mark(4_240, Stage::Stopped(GenId(3)));
        told.turn(5_000, 2);
        told.spoken(5_000, 4);
        told.uttered(7_000, 4, false);
        told.fact(
            7_500,
            Fact::LifecycleChanged {
                to: Lifecycle::Closed,
                reason: Some(CloseReason::Requested),
            },
        );

        let rebuilt = Some(Rebuilt {
            state: Some(Seq(12)),
            records: 3,
        });
        let exit = Exit::Ended(Ended::Closed);
        let report = told.summary.report(SessionId(0xCA11), rebuilt, &exit);
        assert_eq!(report["v"], 1);
        assert_eq!(report["session"], "0000000000000000000000000000ca11");
        assert_eq!(report["ended"], "closed");
        assert_eq!(report["why"], "requested");
        assert_eq!(report["rebuilt"], json!({"state": 12, "records": 3}));
        assert_eq!(report["length_ms"], 7_500.0);
        assert_eq!(
            report["counts"],
            json!({
                "turns": 2, "dismissed": 0, "utterances": 3, "cut_off": 1,
                "tool_calls": 1, "tool_calls_not_completed": 0,
            })
        );
        // What the caller waited, turn by turn and over the call.
        assert_eq!(
            report["waited_ms"],
            json!({"count": 2, "min": 325.0, "p50": 325.0, "p95": 680.0, "p99": 680.0, "max": 680.0})
        );
        assert_eq!(
            report["turns"][0],
            json!({
                "turn": 1, "ended_ms": 2_470.0, "addressed": true,
                "waited_ms": 680.0, "deciding_ms": 70.0,
                "thinking_ms": [200.0, 200.0],
                "phrasing_ms": 20.0, "voicing_ms": 100.0, "sending_ms": 5.0,
                "yielding_ms": 240.0,
                "tools": [{
                    "tool": "get_time", "clearing_ms": 5.0, "calling_ms": 150.0,
                    "outcome": "completed",
                }],
                "cut_off": true,
            })
        );
        assert_eq!(report["turns"][1]["waited_ms"], 325.0);
        assert_eq!(report["turns"][1]["thinking_ms"], json!([200.0]));
        assert_eq!(report["turns"][1]["cut_off"], false);
        assert_eq!(report["turns"].as_array().map(Vec::len), Some(2));
        // The greeting is with the parts, and with no turn.
        assert_eq!(report["parts_ms"]["voicing"]["count"], 3);
        assert_eq!(report["parts_ms"]["voicing"]["min"], 80.0);
        assert_eq!(report["parts_ms"]["thinking"]["count"], 3);
        assert_eq!(report["parts_ms"]["yielding"]["max"], 240.0);
        assert!(report["parts_ms"].get("answering").is_none());

        let written = report.to_string();
        for words in [QUESTION, ANSWER, "Lima", "09:07"] {
            assert!(!written.contains(words), "{words:?} is in the report");
        }
    }

    #[test]
    fn a_call_that_came_to_nothing_has_a_report_all_the_same() {
        let told = Told::new();
        let report = told.summary.report(SessionId(2), None, &Exit::Stopped);
        assert_eq!(report["ended"], "stopped");
        assert_eq!(report["why"], json!(null));
        assert_eq!(report["rebuilt"], json!(null));
        assert_eq!(report["waited_ms"], json!(null));
        assert_eq!(report["turns"], json!([]));
        assert_eq!(report["parts_ms"], json!({}));
    }
}
