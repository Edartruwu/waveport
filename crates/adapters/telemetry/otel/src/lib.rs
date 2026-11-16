//! Waveport OpenTelemetry adapter.
//!
//! Stability: experimental
//!
//! What sessions do, sent to a collector as OpenTelemetry traces and
//! metrics: OTLP over HTTP, as protobuf.
//!
//! **Traces.** There is one trace for each turn of a caller. Its root,
//! `turn`, runs from the final transcript of the turn to the end of the
//! answer to it, and has an event where the transcript came and where
//! the turn was decided to have ended. Under it there is a `response`
//! for each time the model was asked, with an event for each first (the
//! first the model wrote, the first text given to speech synthesis, the
//! first audio, the first frame out) and for being lowered and stopped,
//! and an `execute_tool` for each tool call, with an event where it was
//! allowed, sent and back. What an agent says that answers no turn, a
//! greeting for one, has no trace.
//!
//! **Metrics.** A histogram, in seconds, for each part of a turn that
//! [`waveport_adapter_kit::turns`] times, and counts of turns,
//! utterances, tool calls and sessions. Their names are in [`names`].
//!
//! **What is not sent.** No word of a call: not what was said, not the
//! arguments of a tool call and not what it answered.
//!
//! [`Otel`] is a sink, called on a session's own thread, and does not
//! wait there: a span that ends is put in a queue, and a thread of the
//! exporter's own sends what is in it. A queue that is full loses the
//! span. [`Otel::shutdown`] says how much was sent and how much was not.
//!
//! The headers a collector wants, a key among them, are read from the
//! environment by the exporter, as `OTEL_EXPORTER_OTLP_HEADERS`. There
//! is no setting for them here.

pub mod names;
mod sent;

use core::time::Duration;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider as _, UpDownCounter};
use opentelemetry::trace::{
    Event, Span as _, SpanBuilder, SpanKind, TraceContextExt as _, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::{
    MetricExporter, SpanExporter, WithExportConfig as _, WithHttpConfig as _,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use waveport_adapter_kit::tls;
use waveport_adapter_kit::turns::{Stretch, Tracker, What};
use waveport_domain::event::CloseReason;
use waveport_domain::fact::{Dismissal, Fact, Lifecycle, Stamped, ToolOutcome};
use waveport_domain::ids::{BatchId, GenId, ToolCallId, ToolId, TurnId};
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;
use waveport_ports::error::PortError;
use waveport_ports::journal::SessionId;
use waveport_ports::telemetry::{EventSink, Stage, StageMark};

pub use crate::sent::Sent;
use crate::sent::{Counted, Tally};

/// Into how many parts the sessions are divided, each behind a lock of
/// its own, so that sessions on different threads seldom meet at one.
const PARTS: usize = 16;

/// How many sessions a part holds before it looks for those that were
/// not heard of for a long time.
const CROWDED: usize = 1_024;

/// How long a session is not heard of before it is taken to be gone. A
/// session that ends says so; one whose journal broke under it does not.
const GONE: Duration = Duration::from_secs(3_600);

/// How many responses of one turn are followed.
const RESPONSES: usize = 8;

/// How many tool calls of one session are followed.
const CALLS: usize = 32;

/// How many events a span is given.
const EVENTS: usize = 8;

/// Where the histograms have their bounds, in seconds: close together
/// where a caller notices, wide apart beyond.
const BOUNDS: [f64; 16] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.15, 0.25, 0.35, 0.5, 0.75, 1.0, 1.5, 2.5, 5.0, 10.0,
];

/// How the adapter is set up.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the collector listens for OTLP over HTTP, as
    /// `http://localhost:4318`. Traces go to `/v1/traces` under it and
    /// metrics to `/v1/metrics`.
    pub endpoint: String,
    /// What the sender calls itself: the `service.name` of everything
    /// it sends.
    pub service: String,
    /// What the tools are called, in the kernel's order.
    pub tools: Vec<String>,
    /// How long one request to the collector may take.
    pub timeout: Duration,
    /// How often the metrics are sent.
    pub metrics_every: Duration,
    /// Connect straight to the collector, whatever proxy the
    /// environment names.
    pub direct: bool,
}

impl Config {
    /// The usual settings, for a collector at `endpoint`.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            service: "waveport".to_owned(),
            tools: Vec::new(),
            timeout: Duration::from_secs(10),
            metrics_every: Duration::from_secs(30),
            direct: false,
        }
    }
}

/// The instruments.
struct Measures {
    deciding: Histogram<f64>,
    answering: Histogram<f64>,
    thinking: Histogram<f64>,
    phrasing: Histogram<f64>,
    voicing: Histogram<f64>,
    sending: Histogram<f64>,
    yielding: Histogram<f64>,
    clearing: Histogram<f64>,
    calling: Histogram<f64>,
    turns: Counter<u64>,
    utterances: Counter<u64>,
    tool_calls: Counter<u64>,
    sessions: UpDownCounter<i64>,
    closed: Counter<u64>,
}

impl Measures {
    fn new(meter: &Meter) -> Self {
        let seconds = |name: &'static str, what: &'static str| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_description(what)
                .with_boundaries(BOUNDS.to_vec())
                .build()
        };
        let count = |name: &'static str, what: &'static str| {
            meter.u64_counter(name).with_description(what).build()
        };
        Self {
            deciding: seconds(
                names::DECIDING,
                "From the final transcript of a turn to the decision that it has ended.",
            ),
            answering: seconds(
                names::ANSWERING,
                "From the end of a turn to the first frame of the answer handed to the transport.",
            ),
            thinking: seconds(
                names::THINKING,
                "From asking the model for a response to the first it wrote.",
            ),
            phrasing: seconds(
                names::PHRASING,
                "From the first the model wrote to the first text given to speech synthesis.",
            ),
            voicing: seconds(
                names::VOICING,
                "From the first text given to speech synthesis to its first audio.",
            ),
            sending: seconds(
                names::SENDING,
                "From the first audio of a response to its first frame handed to the transport.",
            ),
            yielding: seconds(
                names::YIELDING,
                "From lowering a response that was spoken over to stopping it.",
            ),
            clearing: seconds(
                names::CLEARING,
                "From the model asking for a tool call to the call being sent.",
            ),
            calling: seconds(
                names::CALLING,
                "From sending a tool call to its coming back.",
            ),
            turns: count(names::TURNS, "Turns of callers, by what became of them."),
            utterances: count(
                names::UTTERANCES,
                "Utterances of agents, by whether they were cut off.",
            ),
            tool_calls: count(
                names::TOOL_CALLS,
                "Tool calls that ended, by tool and by how.",
            ),
            sessions: meter
                .i64_up_down_counter(names::SESSIONS)
                .with_description("Sessions under way.")
                .build(),
            closed: count(names::SESSIONS_CLOSED, "Sessions that closed, by why."),
        }
    }

    /// Counts a part of a turn that has ended.
    fn timed(&self, stretch: &Stretch, tool: Option<&str>) {
        let seconds = stretch.took().as_secs_f64();
        let histogram = match stretch.what {
            What::Deciding(_) => &self.deciding,
            What::Answering(..) => &self.answering,
            What::Thinking(_) => &self.thinking,
            What::Phrasing(_) => &self.phrasing,
            What::Voicing(_) => &self.voicing,
            What::Sending(_) => &self.sending,
            What::Yielding(_) => &self.yielding,
            What::Clearing(_) => &self.clearing,
            What::Calling(_) => &self.calling,
        };
        match tool {
            Some(tool) => {
                let named = KeyValue::new(names::TOOL_NAME, tool.to_owned());
                histogram.record(seconds, &[named]);
            }
            None => histogram.record(seconds, &[]),
        }
    }
}

/// A response of the model that belongs to a turn's trace.
struct Responding {
    generation: GenId,
    began: MonoTime,
    events: Vec<(&'static str, MonoTime)>,
    /// Whether any of it was given to speech synthesis: such a response
    /// ends with its utterance, and one that only asked for tools with
    /// its tools.
    spoken: bool,
    batch: Option<BatchId>,
}

/// The trace of the turn that is being answered.
struct Open {
    turn: TurnId,
    /// Holds the root of the trace.
    cx: Context,
    /// Whether a frame of an answer went out.
    answered: bool,
    responses: Vec<Responding>,
}

/// A tool call that is under way.
struct Called {
    call: ToolCallId,
    tool: ToolId,
    proposed: MonoTime,
    events: Vec<(&'static str, MonoTime)>,
    /// The trace it was asked for in, and that trace's turn.
    within: Option<(TurnId, Context)>,
}

/// What is kept about one session.
struct Watched {
    /// The time of day it was when the session's clock said `since`.
    origin: SystemTime,
    since: MonoTime,
    /// When the session was last heard of.
    seen: SystemTime,
    tracker: Tracker,
    /// The turn whose final transcript came last, and when.
    heard: Option<(TurnId, MonoTime)>,
    open: Option<Open>,
    calls: Vec<Called>,
    reason: Option<CloseReason>,
}

impl Watched {
    fn new(now: SystemTime, at: MonoTime) -> Self {
        Self {
            origin: now,
            since: at,
            seen: now,
            tracker: Tracker::new(),
            heard: None,
            open: None,
            calls: Vec::with_capacity(4),
            reason: None,
        }
    }

    /// The time of day at which the session's clock said `at`.
    fn time(&self, at: MonoTime) -> SystemTime {
        self.origin + at.since(self.since)
    }
}

/// Sends what sessions do to a collector. One serves every session of a
/// process.
pub struct Otel {
    tracer: SdkTracer,
    traces: SdkTracerProvider,
    metrics: SdkMeterProvider,
    measures: Measures,
    tools: Vec<String>,
    tally: Arc<Tally>,
    watched: [Mutex<HashMap<SessionId, Watched>>; PARTS],
}

impl core::fmt::Debug for Otel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Otel")
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl Otel {
    /// Sets the adapter up. Nothing is sent yet, so a collector that is
    /// not there is not found out here.
    ///
    /// # Errors
    ///
    /// The address is not one of a collector, or TLS cannot be set up on
    /// this machine.
    pub fn new(config: Config) -> Result<Self, PortError> {
        let endpoint = config.endpoint.trim_end_matches('/').to_owned();
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return Err(PortError::permanent(format!(
                "{endpoint} is not the address of a collector: it has to begin with http:// or \
                 https://"
            )));
        }
        // A client that blocks may not be made on a thread that runs
        // tasks, and the caller's may be one.
        let (timeout, direct) = (config.timeout, config.direct);
        let client = std::thread::spawn(move || client(timeout, direct))
            .join()
            .map_err(|_| PortError::permanent("no HTTP client for the collector"))??;

        let refused = |error| PortError::permanent(format!("no exporter to {endpoint}: {error}"));
        let tally = Arc::new(Tally::default());
        let spans = SpanExporter::builder()
            .with_http()
            .with_http_client(client.clone())
            .with_endpoint(format!("{endpoint}/v1/traces"))
            .with_timeout(config.timeout)
            .build()
            .map_err(refused)?;
        let metrics = MetricExporter::builder()
            .with_http()
            .with_http_client(client)
            .with_endpoint(format!("{endpoint}/v1/metrics"))
            .with_timeout(config.timeout)
            .build()
            .map_err(refused)?;

        let version = KeyValue::new("service.version", env!("CARGO_PKG_VERSION"));
        let resource = Resource::builder()
            .with_service_name(config.service)
            .with_attribute(version)
            .build();
        let traces = SdkTracerProvider::builder()
            .with_batch_exporter(Counted::new(spans, Arc::clone(&tally)))
            .with_resource(resource.clone())
            .build();
        let reader = PeriodicReader::builder(Counted::new(metrics, Arc::clone(&tally)))
            .with_interval(config.metrics_every)
            .build();
        let metrics = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();
        Ok(Self {
            tracer: traces.tracer(names::SCOPE),
            measures: Measures::new(&metrics.meter(names::SCOPE)),
            traces,
            metrics,
            tools: config.tools,
            tally,
            watched: core::array::from_fn(|_| Mutex::default()),
        })
    }

    /// How much has been sent so far, and how much could not be.
    #[must_use]
    pub fn sent(&self) -> Sent {
        self.tally.sent()
    }

    /// Ends the traces that are open, sends what has not been sent, and
    /// stops. Returns how much was sent in all and how much could not
    /// be.
    ///
    /// It waits for the collector, for as long as a request may take, so
    /// it is for the end of a program and not for a session's thread.
    pub fn shutdown(&self) -> Sent {
        for part in &self.watched {
            let mut part = part.lock().unwrap_or_else(PoisonError::into_inner);
            for (session, mut watched) in part.drain() {
                let seen = watched.seen;
                if let Some(open) = watched.open.take() {
                    self.close(session, &watched, &open, seen);
                }
            }
        }
        // What the providers answer says only whether they stopped in
        // time. Whether the collector took what it was sent is in the
        // tally.
        let _ = self.traces.shutdown();
        let _ = self.metrics.shutdown();
        self.tally.sent()
    }

    /// The part of the sessions that `session` is in.
    fn part(&self, session: SessionId) -> Option<MutexGuard<'_, HashMap<SessionId, Watched>>> {
        let index = usize::try_from(session.0 % (PARTS as u128)).ok()?;
        let part = self.watched.get(index)?;
        Some(part.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// What is kept about `session`, which is begun if it is new.
    fn watched<'a>(
        &self,
        part: &'a mut HashMap<SessionId, Watched>,
        session: SessionId,
        at: MonoTime,
    ) -> &'a mut Watched {
        if !part.contains_key(&session) {
            let now = SystemTime::now();
            if part.len() >= CROWDED {
                let before = part.len();
                part.retain(|_, watched| {
                    now.duration_since(watched.seen)
                        .is_ok_and(|silent| silent < GONE)
                });
                let gone = i64::try_from(before - part.len()).unwrap_or(i64::MAX);
                self.measures.sessions.add(-gone, &[]);
            }
            self.measures.sessions.add(1, &[]);
        }
        let watched = part
            .entry(session)
            .or_insert_with(|| Watched::new(SystemTime::now(), at));
        watched.seen = watched.time(at);
        watched
    }

    /// What the tool `tool` is called.
    fn tool(&self, tool: ToolId) -> String {
        self.tools
            .get(usize::from(tool.get()))
            .cloned()
            .unwrap_or_else(|| format!("tool {}", tool.get()))
    }

    /// Begins the trace of `turn`, which ended at `at`.
    fn begin(&self, session: SessionId, watched: &mut Watched, turn: TurnId, at: MonoTime) {
        if let Some(open) = watched.open.take() {
            self.close(session, watched, &open, watched.time(at));
        }
        let heard = watched.heard.take().filter(|(of, _)| *of == turn);
        let from = heard.map_or(at, |(_, heard)| heard);
        let builder = SpanBuilder::from_name(names::SPAN_TURN)
            .with_kind(SpanKind::Server)
            .with_start_time(watched.time(from))
            .with_attributes([
                KeyValue::new(names::CONVERSATION, session.to_string()),
                KeyValue::new(names::TURN, i64::from(turn.get())),
            ]);
        let root = self.tracer.build_with_context(builder, &Context::new());
        let cx = Context::new().with_span(root);
        if heard.is_some() {
            cx.span().add_event_with_timestamp(
                names::EVENT_TRANSCRIPT,
                watched.time(from),
                Vec::new(),
            );
        }
        cx.span()
            .add_event_with_timestamp(names::EVENT_TURN_ENDED, watched.time(at), Vec::new());
        watched.open = Some(Open {
            turn,
            cx,
            answered: false,
            responses: Vec::with_capacity(2),
        });
    }

    /// Ends a trace at `to`, and with it the responses that have not
    /// ended.
    fn close(&self, session: SessionId, watched: &Watched, open: &Open, to: SystemTime) {
        for response in &open.responses {
            self.responded(session, watched, &open.cx, response, to, None);
        }
        open.cx.span().end_with_timestamp(to);
    }

    /// Sends the span of a response that ended at `to`.
    fn responded(
        &self,
        session: SessionId,
        watched: &Watched,
        cx: &Context,
        response: &Responding,
        to: SystemTime,
        truncated: Option<bool>,
    ) {
        let mut attributes = vec![
            KeyValue::new(names::CONVERSATION, session.to_string()),
            KeyValue::new(names::GENERATION, i64::from(response.generation.get())),
        ];
        if let Some(truncated) = truncated {
            attributes.push(KeyValue::new(names::TRUNCATED, truncated));
        }
        let events = response
            .events
            .iter()
            .map(|(name, at)| Event::new(*name, watched.time(*at), Vec::new(), 0))
            .collect();
        let builder = SpanBuilder::from_name(names::SPAN_RESPONSE)
            .with_kind(SpanKind::Internal)
            .with_start_time(watched.time(response.began))
            .with_attributes(attributes)
            .with_events(events);
        self.tracer
            .build_with_context(builder, cx)
            .end_with_timestamp(to);
    }

    /// Sends the span of a tool call that ended at `to`.
    fn called(
        &self,
        session: SessionId,
        watched: &Watched,
        called: &Called,
        name: String,
        outcome: ToolOutcome,
        to: SystemTime,
    ) {
        let Some((_, cx)) = &called.within else {
            return;
        };
        let events = called
            .events
            .iter()
            .map(|(name, at)| Event::new(*name, watched.time(*at), Vec::new(), 0))
            .collect();
        let builder = SpanBuilder::from_name(format!("{} {name}", names::EXECUTE_TOOL))
            .with_kind(SpanKind::Internal)
            .with_start_time(watched.time(called.proposed))
            .with_attributes([
                KeyValue::new(names::CONVERSATION, session.to_string()),
                KeyValue::new(names::OPERATION, names::EXECUTE_TOOL),
                KeyValue::new(names::TOOL_NAME, name),
                KeyValue::new(names::TOOL_CALL, called.call.get().to_string()),
                KeyValue::new(names::TOOL_OUTCOME, outcome_name(outcome)),
            ])
            .with_events(events);
        self.tracer
            .build_with_context(builder, cx)
            .end_with_timestamp(to);
    }

    /// Takes one fact, recorded at `at`. Returns whether the session is
    /// over.
    fn fact(&self, session: SessionId, watched: &mut Watched, at: MonoTime, fact: &Fact) -> bool {
        let now = watched.time(at);
        match *fact {
            Fact::TurnCommitted {
                turn, addressed, ..
            } => {
                let committed = KeyValue::new(names::TURN_OUTCOME, "committed");
                let said_to = KeyValue::new(names::ADDRESSED, addressed);
                self.measures.turns.add(1, &[committed, said_to.clone()]);
                if let Some(open) = watched.open.take_if(|open| open.turn == turn) {
                    open.cx.span().set_attribute(said_to);
                    if addressed {
                        watched.open = Some(open);
                    } else {
                        // Nobody answers a turn that was said to someone
                        // else.
                        self.close(session, watched, &open, now);
                    }
                }
            }
            Fact::TurnDismissed { reason, .. } => {
                let why = KeyValue::new(names::TURN_OUTCOME, dismissal_name(reason));
                self.measures.turns.add(1, &[why]);
            }
            Fact::UtteranceCommitted {
                generation,
                truncated,
                ..
            } => {
                let cut = KeyValue::new(names::TRUNCATED, truncated);
                self.measures.utterances.add(1, &[cut]);
                self.uttered(session, watched, generation, truncated, at);
            }
            Fact::ToolProposed {
                call,
                batch,
                generation,
                tool,
                ..
            } => {
                let mut within = None;
                if let Some(open) = &mut watched.open {
                    within = Some((open.turn, open.cx.clone()));
                    let responses = open.responses.iter_mut();
                    if let Some(response) = responses.rev().find(|it| it.generation == generation) {
                        response.batch = Some(batch);
                    }
                }
                if watched.calls.len() >= CALLS {
                    watched.calls.remove(0);
                }
                watched.calls.push(Called {
                    call,
                    tool,
                    proposed: at,
                    events: Vec::with_capacity(3),
                    within,
                });
            }
            Fact::ToolAuthorized { call } => {
                note(&mut watched.calls, call, names::EVENT_AUTHORIZED, at);
            }
            Fact::ToolFinished { call, outcome, .. } => {
                if let Some(found) = watched.calls.iter().position(|it| it.call == call) {
                    let called = watched.calls.remove(found);
                    let name = self.tool(called.tool);
                    self.measures.tool_calls.add(
                        1,
                        &[
                            KeyValue::new(names::TOOL_NAME, name.clone()),
                            KeyValue::new(names::TOOL_OUTCOME, outcome_name(outcome)),
                        ],
                    );
                    self.called(session, watched, &called, name, outcome, now);
                }
            }
            Fact::BatchResolved { batch } => {
                // A response that only asked for tools is over when they
                // are. One that also spoke ends with its utterance.
                if let Some(mut open) = watched.open.take() {
                    let over = |it: &Responding| it.batch == Some(batch) && !it.spoken;
                    if let Some(found) = open.responses.iter().position(over) {
                        let response = open.responses.remove(found);
                        self.responded(session, watched, &open.cx, &response, now, None);
                    }
                    watched.open = Some(open);
                }
            }
            Fact::LifecycleChanged { to, reason } => {
                watched.reason = reason.or(watched.reason);
                if to == Lifecycle::Closed {
                    if let Some(open) = watched.open.take() {
                        self.close(session, watched, &open, now);
                    }
                    let why = KeyValue::new(names::CLOSE_REASON, reason_name(watched.reason));
                    self.measures.closed.add(1, &[why]);
                    self.measures.sessions.add(-1, &[]);
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    /// The utterance of a response is recorded: the response is over,
    /// and if nothing else of the answer is under way, so is the trace.
    fn uttered(
        &self,
        session: SessionId,
        watched: &mut Watched,
        generation: GenId,
        truncated: bool,
        at: MonoTime,
    ) {
        let now = watched.time(at);
        let Some(mut open) = watched.open.take() else {
            return;
        };
        let found = open
            .responses
            .iter()
            .position(|it| it.generation == generation);
        if let Some(found) = found {
            let mut response = open.responses.remove(found);
            // A session records the utterance of a response that was cut
            // off before it marks the response as stopped, in the same
            // step. The mark then finds no response, so the event is
            // made here.
            if truncated && response.events.len() < EVENTS {
                response.events.push((names::EVENT_STOPPED, at));
            }
            self.responded(session, watched, &open.cx, &response, now, Some(truncated));
        }
        let waiting = watched
            .calls
            .iter()
            .any(|called| matches!(&called.within, Some((turn, _)) if *turn == open.turn));
        if found.is_some() && open.answered && open.responses.is_empty() && !waiting {
            self.close(session, watched, &open, now);
        } else {
            watched.open = Some(open);
        }
    }

    /// Takes a mark into the trace of the turn that is being answered.
    fn traced(&self, session: SessionId, watched: &mut Watched, mark: StageMark) {
        let at = mark.at;
        let (generation, name) = match mark.stage {
            Stage::TranscriptFinal(turn) => {
                watched.heard = Some((turn, at));
                return;
            }
            Stage::TurnEnded(turn) => {
                self.begin(session, watched, turn, at);
                return;
            }
            Stage::Asked(generation) => {
                if let Some(open) = &mut watched.open
                    && open.responses.len() < RESPONSES
                {
                    open.responses.push(Responding {
                        generation,
                        began: at,
                        events: Vec::with_capacity(EVENTS),
                        spoken: false,
                        batch: None,
                    });
                }
                return;
            }
            Stage::ToolSent(call) => {
                note(&mut watched.calls, call, names::EVENT_SENT, at);
                return;
            }
            Stage::ToolBack(call) => {
                note(&mut watched.calls, call, names::EVENT_BACK, at);
                return;
            }
            Stage::Stored(_) => return,
            Stage::FirstToken(generation) => (generation, names::EVENT_FIRST_TOKEN),
            Stage::FirstText(generation) => (generation, names::EVENT_FIRST_TEXT),
            Stage::FirstAudio(generation) => (generation, names::EVENT_FIRST_AUDIO),
            Stage::FirstFrameOut(generation) => (generation, names::EVENT_FIRST_FRAME),
            Stage::Lowered(generation) => (generation, names::EVENT_LOWERED),
            Stage::Stopped(generation) => (generation, names::EVENT_STOPPED),
        };
        let Some(open) = &mut watched.open else {
            return;
        };
        let responses = open.responses.iter_mut();
        let Some(response) = responses.rev().find(|it| it.generation == generation) else {
            return;
        };
        if response.events.len() < EVENTS {
            response.events.push((name, at));
        }
        match mark.stage {
            Stage::FirstText(_) => response.spoken = true,
            Stage::FirstFrameOut(_) => open.answered = true,
            _ => {}
        }
    }
}

impl EventSink for Otel {
    fn facts(&self, session: SessionId, at: MonoTime, facts: &[Stamped], _: &TextArena) {
        let Some(mut part) = self.part(session) else {
            return;
        };
        let watched = self.watched(&mut part, session, at);
        watched.tracker.facts(at, facts);
        let mut over = false;
        for stamped in facts {
            over |= self.fact(session, watched, at, &stamped.fact);
        }
        if over {
            part.remove(&session);
        }
    }

    fn stage(&self, session: SessionId, mark: StageMark) {
        // What the journal has stored is said after everything else of a
        // session, and nothing is made of it here.
        if matches!(mark.stage, Stage::Stored(_)) {
            return;
        }
        let Some(mut part) = self.part(session) else {
            return;
        };
        let watched = self.watched(&mut part, session, mark.at);

        // A mark ends two parts of a turn at most.
        let mut found = [None, None];
        let mut next = found.iter_mut();
        watched.tracker.mark(mark, |stretch| {
            if let Some(place) = next.next() {
                *place = Some(stretch);
            }
        });
        for stretch in found.into_iter().flatten() {
            let tool = match stretch.what {
                What::Clearing(call) | What::Calling(call) => {
                    let called = watched.calls.iter().find(|it| it.call == call);
                    called.map(|called| self.tool(called.tool))
                }
                _ => None,
            };
            self.measures.timed(&stretch, tool.as_deref());
            if let What::Answering(turn, _) = stretch.what
                && let Some(open) = &watched.open
                && open.turn == turn
            {
                let waited = KeyValue::new(names::WAITED, stretch.took().as_secs_f64());
                open.cx.span().set_attribute(waited);
            }
        }
        self.traced(session, watched, mark);
    }
}

/// Notes that something happened to the tool call `call`.
fn note(calls: &mut [Called], call: ToolCallId, name: &'static str, at: MonoTime) {
    if let Some(called) = calls.iter_mut().rev().find(|it| it.call == call)
        && called.events.len() < EVENTS
    {
        called.events.push((name, at));
    }
}

/// A client for the exporter's own thread.
fn client(timeout: Duration, direct: bool) -> Result<reqwest::blocking::Client, PortError> {
    let tls = tls::client_config(&["h2", "http/1.1"])?;
    let mut builder = reqwest::blocking::Client::builder()
        .tls_backend_preconfigured(tls)
        .connect_timeout(timeout)
        .timeout(timeout)
        .user_agent(concat!("waveport/", env!("CARGO_PKG_VERSION")));
    if direct {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|error| PortError::permanent(format!("no HTTP client for the collector: {error}")))
}

const fn outcome_name(outcome: ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Completed => "completed",
        ToolOutcome::Failed => "failed",
        ToolOutcome::Cancelled => "cancelled",
        ToolOutcome::InDoubt => "in_doubt",
    }
}

const fn dismissal_name(reason: Dismissal) -> &'static str {
    match reason {
        Dismissal::Backchannel => "backchannel",
        Dismissal::Noise => "noise",
        Dismissal::Empty => "empty",
        Dismissal::Closed => "closed",
    }
}

const fn reason_name(reason: Option<CloseReason>) -> &'static str {
    match reason {
        Some(CloseReason::Requested) => "requested",
        Some(CloseReason::Abandoned) => "abandoned",
        Some(CloseReason::Failure) => "failure",
        None => "unknown",
    }
}
