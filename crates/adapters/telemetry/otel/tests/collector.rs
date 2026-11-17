//! What a collector is sent.
//!
//! The adapter is told what a session's sink is told, and a server on
//! this machine stands in for the collector: it keeps every request, and
//! the tests read them as the protocol says.

#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::fmt::Write as _;
use core::time::Duration;
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message as _;
use waveport_domain::event::CloseReason;
use waveport_domain::fact::{Fact, Lifecycle, Stamped, ToolOutcome};
use waveport_domain::ids::{
    AgentId, BatchId, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
};
use waveport_domain::text::TextArena;
use waveport_domain::time::{MediaSpan, MediaTime, MonoTime};
use waveport_ports::error::RetryClass;
use waveport_ports::journal::SessionId;
use waveport_ports::telemetry::{EventSink as _, Stage, StageMark};
use waveport_telemetry_otel::{Config, Otel, names};

const SESSION: SessionId = SessionId(0xCA11);
const QUESTION: &str = "What time is it in Lima?";
const ARGS: &str = "{\"city\":\"Lima\"}";
const RESULT: &str = "09:07 in the morning";
const ANSWER: &str = "It is seven past nine.";

/// The requests a collector was sent: where to, and what.
type Got = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// A server that stands in for a collector.
struct Collector {
    address: String,
    got: Got,
}

impl Collector {
    /// A collector that answers every request with `status`.
    fn start(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let got = Got::default();
        let kept = Arc::clone(&got);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let kept = Arc::clone(&kept);
                std::thread::spawn(move || serve(stream, status, &kept));
            }
        });
        Self { address, got }
    }

    /// An adapter that sends to this collector.
    fn adapter(&self) -> Otel {
        Otel::new(Config {
            service: "waveport-test".to_owned(),
            tools: vec!["get_time".to_owned()],
            timeout: Duration::from_secs(5),
            // Only when the adapter stops, so that a test sees one
            // request with everything in it.
            metrics_every: Duration::from_secs(3_600),
            direct: true,
            ..Config::new(format!("{}/", self.address))
        })
        .unwrap()
    }

    /// The bodies of the requests that came to `path`.
    fn bodies(&self, path: &str) -> Vec<Vec<u8>> {
        let got = self.got.lock().unwrap();
        let to_path = got.iter().filter(|(to, _)| to == path);
        to_path.map(|(_, body)| body.clone()).collect()
    }

    /// Every span that was sent, and the name of the service that sent
    /// them.
    fn spans(&self) -> (Vec<Span>, Vec<String>) {
        let mut spans = Vec::new();
        let mut services = Vec::new();
        for body in self.bodies("/v1/traces") {
            let request = ExportTraceServiceRequest::decode(body.as_slice()).unwrap();
            for resource in request.resource_spans {
                let attributes = resource.resource.unwrap().attributes;
                services.push(text(&attributes, "service.name").unwrap());
                for scope in resource.scope_spans {
                    assert_eq!(scope.scope.unwrap().name, names::SCOPE);
                    spans.extend(scope.spans);
                }
            }
        }
        spans.sort_by_key(|span| (span.start_time_unix_nano, span.end_time_unix_nano));
        (spans, services)
    }

    /// The metrics as they were sent last: for each name and set of
    /// attributes, how many values were counted and what they add up
    /// to.
    fn metrics(&self) -> BTreeMap<String, (u64, f64)> {
        let mut found = BTreeMap::new();
        let Some(body) = self.bodies("/v1/metrics").pop() else {
            return found;
        };
        let request = ExportMetricsServiceRequest::decode(body.as_slice()).unwrap();
        for resource in request.resource_metrics {
            for scope in resource.scope_metrics {
                for metric in scope.metrics {
                    match metric.data.unwrap() {
                        metric::Data::Histogram(histogram) => {
                            assert_eq!(metric.unit, "s", "{}", metric.name);
                            for point in histogram.data_points {
                                let key = keyed(&metric.name, &point.attributes);
                                found.insert(key, (point.count, point.sum.unwrap()));
                            }
                        }
                        metric::Data::Sum(sum) => {
                            for point in sum.data_points {
                                let key = keyed(&metric.name, &point.attributes);
                                let Some(number_data_point::Value::AsInt(value)) = point.value
                                else {
                                    panic!("{key} is not a whole number");
                                };
                                #[expect(
                                    clippy::cast_precision_loss,
                                    reason = "the counts of a test are small"
                                )]
                                found.insert(key, (1, value as f64));
                            }
                        }
                        other => panic!("{}: {other:?}", metric.name),
                    }
                }
            }
        }
        found
    }
}

/// Serves one connection: keeps each request and answers it with
/// `status`.
fn serve(mut stream: TcpStream, status: u16, kept: &Mutex<Vec<(String, Vec<u8>)>>) {
    let mut buffer = Vec::new();
    loop {
        let mut more = |buffer: &mut Vec<u8>| {
            let mut chunk = [0; 4096];
            let n = stream.read(&mut chunk).unwrap_or(0);
            buffer.extend_from_slice(&chunk[..n]);
            n > 0
        };
        let head_end = loop {
            let end = buffer.windows(4).position(|four| four == b"\r\n\r\n");
            if let Some(end) = end {
                break end + 4;
            }
            if !more(&mut buffer) {
                return;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: ")?.trim().parse().ok())
            .unwrap_or(0);
        while buffer.len() < head_end + length {
            if !more(&mut buffer) {
                return;
            }
        }
        assert!(
            head.contains("content-type: application/x-protobuf"),
            "{head}"
        );
        let path = head.split(' ').nth(1).unwrap().to_owned();
        let body = buffer[head_end..head_end + length].to_vec();
        buffer.drain(..head_end + length);
        kept.lock().unwrap().push((path, body));
        let answer = format!(
            "HTTP/1.1 {status} It Is\r\ncontent-type: application/x-protobuf\r\n\
             content-length: 0\r\n\r\n"
        );
        if stream.write_all(answer.as_bytes()).is_err() {
            return;
        }
    }
}

/// The attribute `key` as text.
fn text(attributes: &[KeyValue], key: &str) -> Option<String> {
    let found = attributes.iter().find(|attribute| attribute.key == key)?;
    Some(match found.value.clone()?.value? {
        any_value::Value::StringValue(text) => text,
        any_value::Value::BoolValue(yes) => yes.to_string(),
        any_value::Value::IntValue(number) => number.to_string(),
        any_value::Value::DoubleValue(number) => format!("{number:.3}"),
        other => panic!("{key}: {other:?}"),
    })
}

/// A metric's name with its attributes, in the order of their keys.
fn keyed(name: &str, attributes: &[KeyValue]) -> String {
    let mut keys: Vec<&str> = attributes.iter().map(|it| it.key.as_str()).collect();
    keys.sort_unstable();
    let mut key = name.to_owned();
    for attribute in keys {
        let value = text(attributes, attribute).unwrap();
        let _ = write!(key, " {attribute}={value}");
    }
    key
}

/// The names of a span's events, and how long after the span began each
/// was, in milliseconds.
fn events(span: &Span) -> Vec<(&str, u64)> {
    let since = |at: u64| (at - span.start_time_unix_nano) / 1_000_000;
    let events = span.events.iter();
    events
        .map(|event| (event.name.as_str(), since(event.time_unix_nano)))
        .collect()
}

/// How long a span lasted, in milliseconds.
const fn lasted(span: &Span) -> u64 {
    (span.end_time_unix_nano - span.start_time_unix_nano) / 1_000_000
}

/// Tells an adapter what a session's sink is told.
struct Told<'a> {
    otel: &'a Otel,
    session: SessionId,
    text: TextArena,
}

impl<'a> Told<'a> {
    fn new(otel: &'a Otel) -> Self {
        Self {
            otel,
            session: SESSION,
            text: TextArena::default(),
        }
    }

    fn mark(&self, at_ms: u64, stage: Stage) {
        let at = MonoTime::from_millis(at_ms);
        self.otel.stage(self.session, StageMark { at, stage });
    }

    fn fact(&mut self, at_ms: u64, fact: Fact) {
        let stamped = Stamped { seq: Seq(1), fact };
        let at = MonoTime::from_millis(at_ms);
        self.otel.facts(self.session, at, &[stamped], &self.text);
    }

    /// A caller's turn ends at `at_ms`, 70 ms after its final transcript.
    fn turn(&mut self, at_ms: u64, turn: u32, addressed: bool) {
        self.mark(at_ms - 70, Stage::TranscriptFinal(TurnId(turn)));
        self.mark(at_ms, Stage::TurnEnded(TurnId(turn)));
        let fact = Fact::TurnCommitted {
            turn: TurnId(turn),
            who: ParticipantId(1),
            span: MediaSpan::new(MediaTime::from_millis(0), MediaTime::from_millis(900)),
            text: self.text.push(QUESTION),
            addressed,
        };
        self.fact(at_ms, fact);
    }

    /// A response is asked for at `at_ms` and is first heard 325 ms
    /// later.
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

    fn closed(&mut self, at_ms: u64) {
        let fact = Fact::LifecycleChanged {
            to: Lifecycle::Closed,
            reason: Some(CloseReason::Requested),
        };
        self.fact(at_ms, fact);
    }
}

/// A caller asks the time, the model asks a tool for it, and says it
/// when the tool is back. Returns the collector that was told of it.
fn asked_the_time() -> Collector {
    let collector = Collector::start(200);
    let otel = collector.adapter();
    let mut told = Told::new(&otel);
    let (first, call) = (GenId(1), ToolCallId(7));

    told.turn(2_470, 1, true);
    told.mark(2_470, Stage::Asked(first));
    told.mark(2_670, Stage::FirstToken(first));
    let args = told.text.push(ARGS);
    told.fact(
        2_670,
        Fact::ToolProposed {
            call,
            batch: BatchId(1),
            generation: first,
            tool: ToolId(0),
            args,
        },
    );
    told.fact(2_672, Fact::ToolAuthorized { call });
    told.mark(2_675, Stage::Stored(Seq(4)));
    told.mark(2_675, Stage::ToolSent(call));
    told.mark(2_825, Stage::ToolBack(call));
    let result = told.text.push(RESULT);
    told.fact(
        2_825,
        Fact::ToolFinished {
            call,
            outcome: ToolOutcome::Completed,
            result,
        },
    );
    told.fact(2_825, Fact::BatchResolved { batch: BatchId(1) });
    told.spoken(2_825, 2);
    told.uttered(5_000, 2, false);
    told.closed(6_000);
    let sent = otel.shutdown();
    assert_eq!((sent.spans, sent.spans_lost), (4, 0), "{sent:?}");
    assert!(sent.metrics >= 1 && !sent.lost_any(), "{sent:?}");
    collector
}

#[test]
fn a_turn_with_a_tool_call_is_one_trace() {
    let collector = asked_the_time();

    // One trace: the turn, and under it the two responses and the call.
    let (spans, services) = collector.spans();
    assert!(services.iter().all(|service| service == "waveport-test"));
    let names: Vec<&str> = spans.iter().map(|span| span.name.as_str()).collect();
    assert_eq!(
        names,
        ["turn", "response", "execute_tool get_time", "response"]
    );
    let (turn, asked, tool, answered) = (&spans[0], &spans[1], &spans[2], &spans[3]);
    assert_eq!(turn.parent_span_id, Vec::<u8>::new());
    for span in &spans {
        assert_eq!(span.trace_id, turn.trace_id);
        let session = text(&span.attributes, names::CONVERSATION);
        assert_eq!(session.as_deref(), Some("0000000000000000000000000000ca11"));
    }
    for child in [asked, tool, answered] {
        assert_eq!(child.parent_span_id, turn.span_id);
    }

    // The turn runs from its final transcript to the end of the answer.
    assert_eq!(lasted(turn), 2_600);
    assert_eq!(events(turn), [("transcript_final", 0), ("turn_ended", 70)]);
    let of_turn = |key| text(&turn.attributes, key);
    assert_eq!(of_turn(names::TURN).as_deref(), Some("1"));
    assert_eq!(of_turn(names::ADDRESSED).as_deref(), Some("true"));
    assert_eq!(of_turn(names::WAITED).as_deref(), Some("0.680"));

    // The response that only asked for the tool ends when the tool is
    // back, and the one that was spoken with its utterance.
    assert_eq!(
        (lasted(asked), events(asked)),
        (355, vec![("first_token", 200)])
    );
    assert_eq!(lasted(answered), 2_175);
    assert_eq!(
        events(answered),
        [
            ("first_token", 200),
            ("first_text", 220),
            ("first_audio", 320),
            ("first_frame", 325),
        ]
    );
    let cut = text(&answered.attributes, names::TRUNCATED);
    assert_eq!(cut.as_deref(), Some("false"));
    assert_eq!(
        text(&answered.attributes, names::GENERATION).as_deref(),
        Some("2")
    );

    // The call runs from being asked for to being back.
    assert_eq!(lasted(tool), 155);
    assert_eq!(
        events(tool),
        [("authorized", 2), ("sent", 5), ("back", 155)]
    );
    let of_tool = |key| text(&tool.attributes, key);
    assert_eq!(of_tool(names::OPERATION).as_deref(), Some("execute_tool"));
    assert_eq!(of_tool(names::TOOL_NAME).as_deref(), Some("get_time"));
    assert_eq!(of_tool(names::TOOL_CALL).as_deref(), Some("7"));
    assert_eq!(of_tool(names::TOOL_OUTCOME).as_deref(), Some("completed"));
}

#[test]
fn the_parts_of_a_turn_are_counted_and_no_word_of_it_is_sent() {
    let collector = asked_the_time();

    // Each part of the turn is counted once, and the model twice.
    let metrics = collector.metrics();
    let counted = |key: &str| {
        let (count, sum) = metrics
            .get(key)
            .copied()
            .unwrap_or_else(|| panic!("no {key} among {:?}", metrics.keys().collect::<Vec<_>>()));
        (count, (sum * 1_000.0).round())
    };
    assert_eq!(counted(names::DECIDING), (1, 70.0));
    assert_eq!(counted(names::ANSWERING), (1, 680.0));
    assert_eq!(counted(names::THINKING), (2, 400.0));
    assert_eq!(counted(names::PHRASING), (1, 20.0));
    assert_eq!(counted(names::VOICING), (1, 100.0));
    assert_eq!(counted(names::SENDING), (1, 5.0));
    let by_tool = |name: &str| format!("{name} {}=get_time", names::TOOL_NAME);
    assert_eq!(counted(&by_tool(names::CLEARING)), (1, 5.0));
    assert_eq!(counted(&by_tool(names::CALLING)), (1, 150.0));
    let turns = format!(
        "{} {}=true {}=committed",
        names::TURNS,
        names::ADDRESSED,
        names::TURN_OUTCOME
    );
    assert_eq!(counted(&turns), (1, 1_000.0));
    let utterances = format!("{} {}=false", names::UTTERANCES, names::TRUNCATED);
    assert_eq!(counted(&utterances), (1, 1_000.0));
    let calls = format!(
        "{} {}=get_time {}=completed",
        names::TOOL_CALLS,
        names::TOOL_NAME,
        names::TOOL_OUTCOME
    );
    assert_eq!(counted(&calls), (1, 1_000.0));
    let closed = format!(
        "{} {}=requested",
        names::SESSIONS_CLOSED,
        names::CLOSE_REASON
    );
    assert_eq!(counted(&closed), (1, 1_000.0));
    // The session came and went.
    assert_eq!(counted(names::SESSIONS), (1, 0.0));

    // No word of the call was sent.
    let got = collector.got.lock().unwrap();
    assert!(got.len() >= 2);
    for (path, body) in got.iter() {
        for words in [QUESTION, ARGS, RESULT, ANSWER] {
            let found = body.windows(words.len()).any(|at| at == words.as_bytes());
            assert!(!found, "{words:?} was sent to {path}");
        }
    }
}

#[test]
fn an_answer_that_is_spoken_over_ends_the_trace_where_it_was_cut() {
    let collector = Collector::start(200);
    let otel = collector.adapter();
    let mut told = Told::new(&otel);
    told.turn(1_000, 1, true);
    told.spoken(1_000, 1);
    told.mark(3_000, Stage::Lowered(GenId(1)));
    // As a session tells it: what was heard of the answer is recorded,
    // and the answer is marked as stopped after that, in the same step.
    told.uttered(3_240, 1, true);
    told.mark(3_240, Stage::Stopped(GenId(1)));
    // The words that cut it off are the next turn, with a trace of its
    // own, which is still open when the adapter stops.
    told.turn(4_000, 2, true);
    told.mark(4_000, Stage::Asked(GenId(2)));
    told.mark(4_150, Stage::FirstToken(GenId(2)));
    otel.shutdown();

    let (spans, _) = collector.spans();
    let names: Vec<&str> = spans.iter().map(|span| span.name.as_str()).collect();
    assert_eq!(names, ["turn", "response", "turn", "response"]);
    let (first, answer, second, begun) = (&spans[0], &spans[1], &spans[2], &spans[3]);
    assert_eq!(lasted(first), 2_310);
    assert_eq!(
        text(&answer.attributes, names::TRUNCATED).as_deref(),
        Some("true")
    );
    let last: Vec<(&str, u64)> = events(answer).into_iter().skip(4).collect();
    assert_eq!(last, [("lowered", 2_000), ("stopped", 2_240)]);

    // The second trace is another one, and ends where the session was
    // last heard of.
    assert_ne!(second.trace_id, first.trace_id);
    assert_eq!(begun.trace_id, second.trace_id);
    assert_eq!(text(&second.attributes, names::TURN).as_deref(), Some("2"));
    assert_eq!((lasted(second), lasted(begun)), (220, 150));
    assert_eq!(text(&begun.attributes, names::TRUNCATED), None);

    let metrics = collector.metrics();
    let (count, sum) = metrics[names::YIELDING];
    assert_eq!((count, (sum * 1_000.0).round()), (1, 240.0));
    // The session never closed, so it is still counted as under way.
    assert_eq!(metrics[names::SESSIONS], (1, 1.0));
}

#[test]
fn what_answers_no_turn_has_no_trace_and_a_turn_for_someone_else_has_no_answer() {
    let collector = Collector::start(200);
    let otel = collector.adapter();
    let mut told = Told::new(&otel);
    // A greeting: said with no model, to no turn.
    told.mark(100, Stage::FirstText(GenId(1)));
    told.mark(200, Stage::FirstAudio(GenId(1)));
    told.mark(200, Stage::FirstFrameOut(GenId(1)));
    told.uttered(1_500, 1, false);
    // A turn that was said to another person in the room.
    told.turn(3_000, 1, false);
    // A sound that was not a turn.
    told.fact(
        4_000,
        Fact::TurnDismissed {
            turn: TurnId(2),
            who: ParticipantId(1),
            reason: waveport_domain::fact::Dismissal::Backchannel,
        },
    );
    // What the agent says next answers nothing.
    told.spoken(5_000, 2);
    told.uttered(6_000, 2, false);
    told.closed(7_000);
    otel.shutdown();

    let (spans, _) = collector.spans();
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].name, "turn");
    assert_eq!(lasted(&spans[0]), 70);
    let said_to = text(&spans[0].attributes, names::ADDRESSED);
    assert_eq!(said_to.as_deref(), Some("false"));
    assert_eq!(text(&spans[0].attributes, names::WAITED), None);

    let metrics = collector.metrics();
    assert!(!metrics.contains_key(names::ANSWERING), "{metrics:?}");
    assert_eq!(metrics[names::VOICING].0, 2);
    let dismissed = format!("{} {}=backchannel", names::TURNS, names::TURN_OUTCOME);
    assert_eq!(metrics[&dismissed], (1, 1.0));
}

#[test]
fn keys_are_a_turn_with_a_trace_and_no_key_of_it_is_sent() {
    const KEYS: &str = "40719";
    let collector = Collector::start(200);
    let otel = collector.adapter();
    let mut told = Told::new(&otel);
    // Nobody transcribes keys: the turn is over when its keys are.
    told.mark(2_000, Stage::TurnEnded(TurnId(1)));
    let keys = told.text.push(KEYS);
    told.fact(
        2_000,
        Fact::KeysCommitted {
            turn: TurnId(1),
            who: ParticipantId(1),
            keys,
        },
    );
    told.spoken(2_000, 1);
    told.uttered(4_000, 1, false);
    told.closed(5_000);
    otel.shutdown();

    // The turn is traced from where its keys were over to the end of
    // its answer, and is for the agent, as keys always are.
    // The two begin at the same moment, so either may come first.
    let (spans, _) = collector.spans();
    assert_eq!(spans.len(), 2, "{spans:?}");
    let named = |name: &str| spans.iter().find(|span| span.name == name).unwrap();
    let (turn, answered) = (named("turn"), named("response"));
    assert_eq!(answered.parent_span_id, turn.span_id);
    assert_eq!(lasted(turn), 2_000);
    assert_eq!(events(turn), [("turn_ended", 0)]);
    let of_turn = |key| text(&turn.attributes, key);
    assert_eq!(of_turn(names::ADDRESSED).as_deref(), Some("true"));
    assert_eq!(of_turn(names::WAITED).as_deref(), Some("0.325"));

    // It is counted as a turn, and what was waited for its answer as
    // for any other. Nothing was decided about it: nobody spoke.
    let metrics = collector.metrics();
    let turns = format!(
        "{} {}=true {}=committed",
        names::TURNS,
        names::ADDRESSED,
        names::TURN_OUTCOME
    );
    assert_eq!(metrics[&turns], (1, 1.0));
    assert_eq!(metrics[names::ANSWERING].0, 1);
    assert!(!metrics.contains_key(names::DECIDING), "{metrics:?}");

    // No key was sent: an account number is as private as words are.
    let got = collector.got.lock().unwrap();
    assert!(got.len() >= 2);
    for (path, body) in got.iter() {
        let found = body.windows(KEYS.len()).any(|at| at == KEYS.as_bytes());
        assert!(!found, "{KEYS:?} was sent to {path}");
    }
}

#[test]
fn every_session_has_traces_of_its_own() {
    let collector = Collector::start(200);
    let otel = collector.adapter();
    let mut calls: Vec<Told<'_>> = (0..40u128)
        .map(|n| Told {
            session: SessionId(0x100 + n),
            ..Told::new(&otel)
        })
        .collect();
    // The sessions take their turns at the same time.
    for told in &mut calls {
        told.turn(1_000, 1, true);
    }
    for told in &calls {
        told.spoken(1_000, 1);
    }
    for told in &mut calls {
        told.uttered(2_000, 1, false);
        told.closed(3_000);
    }
    let sent = otel.shutdown();
    assert_eq!((sent.spans, sent.spans_lost), (80, 0));

    let (spans, _) = collector.spans();
    let mut traces = BTreeMap::new();
    for span in &spans {
        let session = text(&span.attributes, names::CONVERSATION).unwrap();
        let of_trace: &mut Vec<String> = traces.entry(span.trace_id.clone()).or_default();
        of_trace.push(session);
    }
    assert_eq!(traces.len(), 40);
    for sessions in traces.values() {
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0], sessions[1]);
    }
    let metrics = collector.metrics();
    assert_eq!(metrics[names::ANSWERING].0, 40);
    assert_eq!(metrics[names::SESSIONS], (1, 0.0));
}

#[test]
fn what_cannot_be_sent_is_counted_and_said() {
    // A collector that refuses.
    let collector = Collector::start(503);
    let otel = collector.adapter();
    let mut told = Told::new(&otel);
    told.turn(1_000, 1, true);
    told.spoken(1_000, 1);
    told.uttered(2_000, 1, false);
    let sent = otel.shutdown();
    assert_eq!((sent.spans, sent.spans_lost), (0, 2), "{sent:?}");
    assert!(sent.metrics_lost >= 1 && sent.lost_any(), "{sent:?}");
    assert!(sent.error.is_some(), "{sent:?}");

    // A collector that is not there.
    let nobody = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", nobody.local_addr().unwrap());
    drop(nobody);
    let otel = Otel::new(Config {
        direct: true,
        ..Config::new(address)
    })
    .unwrap();
    let mut told = Told::new(&otel);
    told.turn(1_000, 1, false);
    assert_eq!(otel.sent().spans_lost, 0);
    let sent = otel.shutdown();
    assert_eq!((sent.spans, sent.spans_lost), (0, 1), "{sent:?}");
}

#[test]
fn an_address_that_is_not_a_collectors_is_refused() {
    let Err(error) = Otel::new(Config::new("localhost:4318")).map(|_| ()) else {
        panic!("it has no scheme");
    };
    assert_eq!(error.class, RetryClass::Permanent);
    assert!(error.message.contains("http://"), "{error}");
}
