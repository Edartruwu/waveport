//! A whole voice session, with a fake in every port.
//!
//! This is the proof that the ports are enough: a person talks into a
//! transport, a recogniser hears them, the kernel decides, a model
//! answers, a synthesiser speaks, and the person hears the answer and
//! interrupts it. Nothing here reaches past a port. The kernel and the
//! audio path are the real ones, and the session is run as the planner
//! says to run it.
//!
//! The loop that joins the parts, [`Session`], is the smallest that can.
//! It is not the runtime: it serves one session, in one task, and gives
//! up on anything that fails. The runtime replaces it, and keeps these
//! sessions as tests.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a session that goes wrong fails its test by panicking"
)]

use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use tokio::time::{Instant, Sleep};
use waveport_domain::command::{
    ClientEvent, Command, LlmCmd, PlayoutCmd, SenseCmd, SttCmd, ToolCmd, TtsCmd,
};
use waveport_domain::config::{AgentSpec, Effect, KernelConfig, ToolSpec};
use waveport_domain::event::{
    ControlInput, Event, OpEvent, OverlapVerdict, Part, TimerId, ToolEvent, VadEdge,
};
use waveport_domain::ids::{OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId};
use waveport_domain::kernel::{ItemView, Kernel};
use waveport_domain::out::Out;
use waveport_domain::policy::DefaultPolicies;
use waveport_domain::text::TextArena;
use waveport_domain::time::{MediaDur, MediaTime, MonoTime};
use waveport_media::codec::Encoder;
use waveport_media::codec::opus::OpusSettings;
use waveport_media::ingress::{Ingress, IngressConfig, Packet};
use waveport_media::ledger::{Mark, Spread};
use waveport_media::playout::{Playout, PlayoutConfig, Position};
use waveport_ports::audio::{AudioRef, Codec, Law, PcmBytes, Rate};
use waveport_ports::error::PortError;
use waveport_ports::journal::{Batch, Journal, JournalError, Lease, SessionId};
use waveport_ports::llm::{Llm, LlmEvent, LlmRequest, LlmRx, Message, ToolDecl};
use waveport_ports::plan::{
    AgentNeeds, BrainProfile, CascadePlan, Marks, SessionPlan, Shortfall, plan,
};
use waveport_ports::stt::{Stt, SttConfig, SttEvent, SttFinalize, SttStream, SttTiming};
use waveport_ports::tool::{ToolExecutor, ToolReply, ToolRequest};
use waveport_ports::transport::{
    FrameOut, RemoteBuffer, Transport, TransportCaps, TransportControl, TransportEvent,
};
use waveport_ports::tts::{Alignment, Tts, TtsCaps, TtsConfig, TtsEvent, TtsInput, TtsStream};
use waveport_testkit::fakes::{
    FakeTransport, FakeTransportBuilder, LlmStep, MemoryJournal, Said, ScriptedLlm, ScriptedStt,
    ScriptedTools, ScriptedTts, Spoken, SttScript, TransportHandle, TtsScript,
};

const CALLER: ParticipantId = ParticipantId(1);
const SESSION: SessionId = SessionId(0x5E55);
const INSTRUCTIONS: &str = "You are a helpful agent on a call.";
const TOOL_NAMES: [&str; 1] = ["get_time"];

/// How long the voice-activity detector takes to notice that speech has
/// begun, and how long after its end it says so.
const VAD_ONSET_MS: u64 = 60;
const VAD_HANGOVER_MS: u64 = 200;

type Boxed<T> = Pin<Box<dyn Future<Output = T>>>;

/// The parts of a session.
struct Parts {
    stt: ScriptedStt,
    llm: ScriptedLlm,
    tts: ScriptedTts,
    tools: ScriptedTools,
    journal: MemoryJournal,
}

/// A response's language model.
struct Thinking {
    tag: OpTag,
    state: ThinkState,
    paused: bool,
}

enum ThinkState {
    Starting(Boxed<Result<Pin<Box<dyn LlmRx>>, PortError>>),
    Writing(Pin<Box<dyn LlmRx>>),
    Over,
}

/// A response's speech synthesis.
struct Speaking {
    tag: OpTag,
    state: SpeakState,
    /// Text the kernel released that the stream has not been given.
    unsent: VecDeque<String>,
    /// Whether the kernel has said that no more text follows.
    finish: bool,
    finish_sent: bool,
    /// Bytes of text released so far.
    text: u32,
    /// Bytes of text the audio so far is known to reach.
    marked: u32,
    /// Samples handed to playback so far.
    audio: usize,
    /// Audio that playback had no room for yet, and how much of it has
    /// been handed over.
    held: Option<(PcmBytes, usize)>,
}

impl Speaking {
    /// Gives the stream the text the kernel released, in order, and then
    /// the word that no more follows.
    fn feed(&mut self) {
        let SpeakState::Open(stream) = &mut self.state else {
            return;
        };
        while let Some(text) = self.unsent.front() {
            if stream.tx.try_speak(text).is_err() {
                break;
            }
            self.unsent.pop_front();
        }
        if self.finish && !self.finish_sent && self.unsent.is_empty() {
            self.finish_sent = stream.tx.try_finish().is_ok();
        }
    }
}

enum SpeakState {
    Opening(Boxed<Result<TtsStream, PortError>>),
    Open(TtsStream),
    Over,
}

/// The smallest loop that can run a session through its ports.
struct Session {
    began: Instant,
    kernel: Kernel,
    plan: CascadePlan,
    parts: Parts,

    transport: Pin<Box<FakeTransport>>,
    ingress: Ingress,
    /// When the inbound path is to be looked at again if nothing arrives.
    ingress_due: Option<MonoTime>,
    hearing: Option<SttStream>,
    /// What the voice-activity detector will say, and when.
    edges: VecDeque<(u64, VadEdge)>,
    /// What is said about speech over the agent.
    verdict: OverlapVerdict,
    /// Answers of the listening models that are on their way.
    answers: Vec<(MonoTime, Event)>,

    thinking: Option<Thinking>,
    speaking: Option<Speaking>,
    calls: Vec<(ToolCallId, Boxed<ToolReply>)>,
    undoing: Vec<(ToolCallId, Boxed<bool>)>,
    timers: BTreeMap<TimerId, MonoTime>,

    playout: Playout,
    encoder: Encoder,
    frame: Vec<i16>,
    packet: Vec<u8>,
    scratch: Vec<i16>,
    reported: Option<Position>,

    lease: Lease,
    unsent: Vec<Bytes>,
    /// The number of the first record in `unsent`.
    unsent_first: u64,
    storing: Option<Boxed<Result<Seq, JournalError>>>,
    /// The record the kernel waits to hear is stored.
    awaited: Option<Seq>,

    sleep: Pin<Box<Sleep>>,
    /// What the clients were told, and when in milliseconds.
    told: Vec<(u64, ClientEvent)>,
    /// Every command, as it was carried out.
    commands: Vec<(u64, String)>,
}

impl Session {
    /// A session with these parts, planned for what they say they can do.
    async fn start(
        parts: Parts,
        transport: FakeTransportBuilder,
        speech: &[(u64, u64)],
    ) -> (Self, TransportHandle) {
        // The clock of the far end's script and the session's start
        // together.
        let (transport, handle) = transport.build();
        let began = Instant::now();

        let brain = BrainProfile::Cascaded {
            hear: parts.stt.caps(),
            think: parts.llm.caps(),
            speak: parts.tts.caps(),
        };
        let needs = AgentNeeds {
            tools: TOOL_NAMES.len(),
        };
        let SessionPlan::Cascaded(plan) = plan(&needs, &brain, &transport.caps()).unwrap();

        let config = KernelConfig {
            agents: vec![AgentSpec {
                tools: vec![ToolId(0)],
            }],
            tools: vec![ToolSpec::new(Effect::Pure)],
            ..KernelConfig::default()
        };
        let kernel = Kernel::new(config, DefaultPolicies::default()).unwrap();

        let ingress = Ingress::new(IngressConfig {
            rate: plan.listen.rate,
            ..IngressConfig::new(plan.listen.wire)
        })
        .unwrap();
        // The recogniser is sent what the inbound path hands on.
        assert_eq!(plan.listen.recogniser_rate, plan.listen.rate);
        let hearing = parts
            .stt
            .open(SttConfig::new(plan.listen.recogniser_rate))
            .await
            .unwrap();

        let playout = Playout::new(PlayoutConfig::new(plan.speak.rate));
        let encoder =
            Encoder::new(plan.speak.wire, plan.speak.rate, OpusSettings::default()).unwrap();
        let lease = parts.journal.acquire(SESSION).await.unwrap().lease;

        let mut edges = VecDeque::new();
        for (from, to) in speech {
            edges.push_back((from + VAD_ONSET_MS, VadEdge::Start));
            edges.push_back((to + VAD_HANGOVER_MS, VadEdge::End));
        }
        let session = Self {
            began,
            kernel,
            parts,
            transport: Box::pin(transport),
            ingress,
            ingress_due: None,
            hearing: Some(hearing),
            edges,
            verdict: OverlapVerdict::BargeIn,
            answers: Vec::new(),
            thinking: None,
            speaking: None,
            calls: Vec::new(),
            undoing: Vec::new(),
            timers: BTreeMap::new(),
            playout,
            encoder,
            frame: vec![0; plan.speak.rate.frame()],
            packet: vec![0; plan.speak.wire.max_packet()],
            scratch: vec![0; 4_800],
            reported: None,
            lease,
            unsent: Vec::new(),
            unsent_first: 1,
            storing: None,
            awaited: None,
            sleep: Box::pin(tokio::time::sleep_until(began)),
            told: Vec::new(),
            commands: Vec::new(),
            plan,
        };
        (session, handle)
    }

    fn now(&self) -> MonoTime {
        let nanos = Instant::now()
            .saturating_duration_since(self.began)
            .as_nanos();
        MonoTime::from_nanos(u64::try_from(nanos).unwrap())
    }

    fn now_ms(&self) -> u64 {
        self.now().as_nanos() / 1_000_000
    }

    /// Runs the session until `until_ms` after its start.
    async fn run_until(&mut self, until_ms: u64) {
        let until = MonoTime::from_millis(until_ms);
        let mut spins = 0_u32;
        let mut spun_at = self.now();
        poll_fn(|cx| {
            loop {
                let progressed = self.sweep(cx);
                let now = self.now();
                if now >= until {
                    return Poll::Ready(());
                }
                // A loop that goes round without the clock moving is a
                // fault of the loop, and is to fail rather than hang.
                spins = if now == spun_at { spins + 1 } else { 0 };
                spun_at = now;
                assert!(
                    spins < 10_000,
                    "the session spins at {} ms: progressed {progressed}, timers {:?}, \
                     playout {:?}, ingress {:?}, edges {:?}, answers {:?}",
                    self.now_ms(),
                    self.timers,
                    self.playout.wake(),
                    self.ingress_due,
                    self.edges.front(),
                    self.answers.len(),
                );
                if progressed {
                    continue;
                }
                let next = self.next_deadline().map_or(until, |next| next.min(until));
                let at = self.began + Duration::from_nanos(next.as_nanos());
                self.sleep.as_mut().reset(at);
                if self.sleep.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
            }
        })
        .await;
    }

    /// The next time something is due that no part will wake the session
    /// for.
    fn next_deadline(&self) -> Option<MonoTime> {
        let mut next: Option<MonoTime> = None;
        let mut note = |at: Option<MonoTime>| {
            if let Some(at) = at {
                next = Some(next.map_or(at, |next| next.min(at)));
            }
        };
        note(self.timers.values().min().copied());
        note(self.playout.wake());
        note(self.ingress_due);
        note(self.edges.front().map(|(at, _)| MonoTime::from_millis(*at)));
        note(self.answers.iter().map(|(at, _)| *at).min());
        next
    }

    /// Looks at every part once, in a fixed order, gives the kernel what
    /// happened and carries out what it asks. Returns whether anything
    /// happened.
    fn sweep(&mut self, cx: &mut Context<'_>) -> bool {
        let now = self.now();
        let mut events = Vec::new();

        // How far playback has got comes first, so that whatever else
        // happened is decided on what was heard by then.
        if let Some(position) = self.playout.position(now)
            && self.reported != Some(position)
        {
            self.reported = Some(position);
            let played = OpEvent::Played {
                handed: position.handed,
                acked: position.played,
                heard_text: position.heard_text,
                drained: position.drained,
            };
            events.push(Event::Op(position.tag, played));
        }

        let due: Vec<TimerId> = self
            .timers
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.timers.remove(&id);
            events.push(Event::Timer(id));
        }

        self.poll_transport(cx, now, &mut events);
        while let Some((at, edge)) = self.edges.front().copied()
            && MonoTime::from_millis(at) <= now
        {
            self.edges.pop_front();
            events.push(Event::Speech {
                who: CALLER,
                edge,
                at: MediaTime::from_millis(at),
            });
        }
        let mut index = 0;
        while index < self.answers.len() {
            if self.answers[index].0 <= now {
                events.push(self.answers.remove(index).1);
            } else {
                index += 1;
            }
        }
        self.poll_hearing(cx, &mut events);
        self.poll_thinking(cx, &mut events);
        self.poll_speaking(cx, now, &mut events);
        self.poll_tools(cx, &mut events);
        self.poll_journal(cx, &mut events);

        let mut progressed = !events.is_empty();
        for event in &events {
            self.step(now, event);
        }
        progressed |= self.send_frames(now);
        progressed
    }

    fn poll_transport(&mut self, cx: &mut Context<'_>, now: MonoTime, events: &mut Vec<Event>) {
        while let Poll::Ready(Some(event)) = self.transport.as_mut().poll_event(cx) {
            match event {
                TransportEvent::Joined { who } => {
                    events.push(Event::Control(ControlInput::Join { who }));
                }
                TransportEvent::Left { who } => {
                    events.push(Event::Control(ControlInput::Leave { who }));
                }
                TransportEvent::Audio {
                    sequence,
                    timestamp,
                    payload,
                    ..
                } => {
                    let packet = Packet {
                        sequence,
                        timestamp,
                        payload: &payload,
                    };
                    self.ingress.push(now, packet);
                }
                TransportEvent::Played { tag, played } => self.playout.ack(tag, played),
                TransportEvent::Text { who, text } => events.push(Event::TextIn {
                    who,
                    text: text.into(),
                }),
                TransportEvent::Detached => events.push(Event::Control(ControlInput::Detached)),
                TransportEvent::Attached => events.push(Event::Control(ControlInput::Attached)),
            }
        }
        // What the inbound path hands on goes to the recogniser.
        let rate = self.plan.listen.recogniser_rate;
        let hearing = &mut self.hearing;
        self.ingress_due = self.ingress.poll(now, |heard| {
            if let Some(stream) = hearing {
                let audio = AudioRef {
                    at: heard.at,
                    rate,
                    samples: heard.pcm,
                };
                stream.tx.try_send(audio).expect("the recogniser keeps up");
            }
        });
    }

    fn poll_hearing(&mut self, cx: &mut Context<'_>, events: &mut Vec<Event>) {
        let Some(stream) = &mut self.hearing else {
            return;
        };
        while let Poll::Ready(next) = stream.rx.as_mut().poll_next(cx) {
            match next {
                Some(Ok(SttEvent::Transcript(transcript))) => events.push(Event::Transcript {
                    who: CALLER,
                    span: transcript.span,
                    text: transcript.text.into(),
                    is_final: transcript.is_final,
                }),
                Some(Err(error)) => panic!("the recogniser failed: {error}"),
                None => {
                    self.hearing = None;
                    break;
                }
            }
        }
    }

    fn poll_thinking(&mut self, cx: &mut Context<'_>, events: &mut Vec<Event>) {
        let Some(thinking) = &mut self.thinking else {
            return;
        };
        let tag = thinking.tag;
        if let ThinkState::Starting(starting) = &mut thinking.state
            && let Poll::Ready(started) = starting.as_mut().poll(cx)
        {
            thinking.state = match started {
                Ok(rx) => ThinkState::Writing(rx),
                Err(error) => {
                    let failed = OpEvent::Failed {
                        part: Part::Think,
                        class: error.class,
                    };
                    events.push(Event::Op(tag, failed));
                    ThinkState::Over
                }
            };
        }
        // A model that was told to wait is not read.
        while !thinking.paused
            && let ThinkState::Writing(rx) = &mut thinking.state
            && let Poll::Ready(next) = rx.as_mut().poll_next(cx)
        {
            let event = match next {
                Some(Ok(LlmEvent::Delta { text })) => OpEvent::LlmDelta { text: text.into() },
                Some(Ok(LlmEvent::ToolCall { tool, args })) => {
                    let index = TOOL_NAMES.iter().position(|name| *name == tool).unwrap();
                    OpEvent::ToolProposed {
                        tool: ToolId(u16::try_from(index).unwrap()),
                        args: args.into(),
                    }
                }
                Some(Ok(LlmEvent::Done { finish, .. })) => OpEvent::LlmDone { finish },
                Some(Err(error)) => {
                    thinking.state = ThinkState::Over;
                    OpEvent::Failed {
                        part: Part::Think,
                        class: error.class,
                    }
                }
                None => {
                    thinking.state = ThinkState::Over;
                    break;
                }
            };
            events.push(Event::Op(tag, event));
        }
    }

    fn poll_speaking(&mut self, cx: &mut Context<'_>, now: MonoTime, events: &mut Vec<Event>) {
        let Some(speaking) = &mut self.speaking else {
            return;
        };
        let tag = speaking.tag;
        let rate = self.plan.speak.rate;
        let failed = |error: &PortError| {
            Event::Op(
                tag,
                OpEvent::Failed {
                    part: Part::Speak,
                    class: error.class,
                },
            )
        };
        if let SpeakState::Opening(opening) = &mut speaking.state
            && let Poll::Ready(opened) = opening.as_mut().poll(cx)
        {
            speaking.state = match opened {
                Ok(stream) => SpeakState::Open(stream),
                Err(error) => {
                    events.push(failed(&error));
                    SpeakState::Over
                }
            };
        }
        speaking.feed();
        let SpeakState::Open(stream) = &mut speaking.state else {
            return;
        };

        // Audio comes out, as long as playback has room for it. Audio
        // there is no room for is held, and the stream is not read on.
        loop {
            let taken = hand_on(
                &mut speaking.held,
                &mut self.playout,
                tag,
                now,
                &mut self.scratch,
            );
            if taken > 0 {
                speaking.audio += taken;
                let segment = OpEvent::TtsSegment {
                    text_end: speaking.marked,
                    audio_end: rate.length_of(speaking.audio),
                    last: false,
                };
                events.push(Event::Op(tag, segment));
            }
            if speaking.held.is_some() {
                return;
            }
            match stream.rx.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(TtsEvent::Audio(pcm)))) => speaking.held = Some((pcm, 0)),
                Poll::Ready(Some(Ok(TtsEvent::Mark(mark)))) => {
                    let spread = match self.plan.speak.marks {
                        Marks::Timed => Spread::AtOnce,
                        Marks::Spread => Spread::Evenly,
                    };
                    let mark = Mark {
                        text: mark.text_end,
                        audio: mark.audio_end,
                        spread,
                    };
                    let _ = self.playout.mark(tag, mark);
                    speaking.marked = mark.text;
                }
                Poll::Ready(Some(Ok(TtsEvent::Done))) => {
                    let audio = rate.length_of(speaking.audio);
                    if speaking.marked < speaking.text {
                        // The synthesiser said nothing about where the
                        // rest of the text is: it is in what is left of
                        // the audio, somewhere.
                        let rest = Mark {
                            text: speaking.text,
                            audio,
                            spread: Spread::Evenly,
                        };
                        let _ = self.playout.mark(tag, rest);
                        speaking.marked = speaking.text;
                    }
                    let _ = self.playout.finish(tag);
                    let segment = OpEvent::TtsSegment {
                        text_end: speaking.text,
                        audio_end: audio,
                        last: true,
                    };
                    events.push(Event::Op(tag, segment));
                }
                Poll::Ready(Some(Err(error))) => {
                    events.push(failed(&error));
                    speaking.state = SpeakState::Over;
                    return;
                }
                Poll::Ready(None) => {
                    speaking.state = SpeakState::Over;
                    return;
                }
                Poll::Pending => return,
            }
        }
    }

    fn poll_tools(&mut self, cx: &mut Context<'_>, events: &mut Vec<Event>) {
        let mut index = 0;
        while index < self.calls.len() {
            let Poll::Ready(reply) = self.calls[index].1.as_mut().poll(cx) else {
                index += 1;
                continue;
            };
            let (call, _) = self.calls.remove(index);
            let event = match reply {
                ToolReply::Completed(result) => ToolEvent::Completed {
                    result: result.into(),
                },
                ToolReply::Failed { class, message } => ToolEvent::Failed {
                    class,
                    message: message.into(),
                },
            };
            events.push(Event::Tool(call, event));
        }
        let mut index = 0;
        while index < self.undoing.len() {
            let Poll::Ready(undone) = self.undoing[index].1.as_mut().poll(cx) else {
                index += 1;
                continue;
            };
            let (call, _) = self.undoing.remove(index);
            if undone {
                events.push(Event::Tool(call, ToolEvent::RolledBack));
            }
        }
    }

    fn poll_journal(&mut self, cx: &mut Context<'_>, events: &mut Vec<Event>) {
        if self.storing.is_none() && !self.unsent.is_empty() {
            let batch = Batch {
                first: Seq(self.unsent_first),
                records: core::mem::take(&mut self.unsent),
            };
            self.unsent_first += u64::try_from(batch.records.len()).unwrap();
            let (journal, lease) = (self.parts.journal.clone(), self.lease);
            self.storing = Some(Box::pin(async move {
                journal.append(SESSION, lease, batch).await
            }));
        }
        if let Some(storing) = &mut self.storing
            && let Poll::Ready(stored) = storing.as_mut().poll(cx)
        {
            self.storing = None;
            let stored = stored.expect("the journal stores what it is given");
            if self.awaited.is_some_and(|awaited| awaited <= stored) {
                self.awaited = None;
                events.push(Event::Durable(stored));
            } else {
                // More may be waiting to be stored; look again.
                cx.waker().wake_by_ref();
            }
        }
    }

    /// Hands the frames that are due to the transport.
    fn send_frames(&mut self, now: MonoTime) -> bool {
        let rate = self.plan.speak.rate;
        let mut sent = false;
        while let Some(frame) = self.playout.poll(now, &mut self.frame) {
            let len = self
                .encoder
                .encode(&self.frame, &mut self.packet)
                .expect("a frame can be encoded");
            let out = FrameOut {
                tag: frame.tag,
                at: frame.at,
                length: rate.length_of(frame.samples),
                payload: &self.packet[..len],
                last: frame.last,
            };
            self.transport
                .as_mut()
                .get_mut()
                .try_send(out)
                .expect("the transport keeps up");
            sent = true;
        }
        sent
    }

    /// Gives the kernel one event and carries out what it asks.
    fn step(&mut self, now: MonoTime, event: &Event) {
        let mut out = Out::new();
        self.kernel.step(now, event, &mut out);
        for stamped in &out.facts {
            self.unsent
                .push(Bytes::from(format!("{:?}", stamped.fact).into_bytes()));
        }
        for command in &out.cmds {
            self.carry_out(now, command, &out.text);
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm for each thing the kernel can ask"
    )]
    fn carry_out(&mut self, now: MonoTime, command: &Command, text: &TextArena) {
        self.commands.push((self.now_ms(), format!("{command:?}")));
        match *command {
            Command::Llm(tag, LlmCmd::Start { .. }) => {
                let said: Vec<Said> = self.kernel.view().items().map(said).collect();
                let llm = self.parts.llm.clone();
                let starting = async move {
                    let messages: Vec<Message<'_>> = said.iter().map(message).collect();
                    let tools = TOOL_NAMES.map(|name| ToolDecl {
                        name,
                        description: "",
                        parameters: "{}",
                    });
                    let request = LlmRequest {
                        instructions: INSTRUCTIONS,
                        messages: &messages,
                        tools: &tools,
                        max_output_tokens: None,
                    };
                    llm.start(request).await
                };
                self.thinking = Some(Thinking {
                    tag,
                    state: ThinkState::Starting(Box::pin(starting)),
                    paused: false,
                });
            }
            Command::Llm(tag, LlmCmd::Pause | LlmCmd::Resume) => {
                if let Some(thinking) = &mut self.thinking
                    && thinking.tag == tag
                {
                    thinking.paused = matches!(command, Command::Llm(_, LlmCmd::Pause));
                }
            }
            Command::Llm(tag, LlmCmd::Cancel) => {
                // Dropping the stream is all there is to cancelling it.
                self.thinking.take_if(|thinking| thinking.tag == tag);
            }
            Command::Tts(tag, TtsCmd::Speak { text: piece }) => {
                let piece = text.get(piece).to_owned();
                if self
                    .speaking
                    .as_ref()
                    .is_none_or(|speaking| speaking.tag != tag)
                {
                    let (tts, rate) = (self.parts.tts.clone(), self.plan.speak.rate);
                    let opening = async move { tts.open(TtsConfig::new(rate)).await };
                    self.speaking = Some(Speaking {
                        tag,
                        state: SpeakState::Opening(Box::pin(opening)),
                        unsent: VecDeque::new(),
                        finish: false,
                        finish_sent: false,
                        text: 0,
                        marked: 0,
                        audio: 0,
                        held: None,
                    });
                }
                if let Some(speaking) = &mut self.speaking {
                    speaking.text += u32::try_from(piece.len()).unwrap();
                    speaking.unsent.push_back(piece);
                }
            }
            Command::Tts(tag, TtsCmd::Finish) => {
                if let Some(speaking) = &mut self.speaking
                    && speaking.tag == tag
                {
                    speaking.finish = true;
                }
            }
            Command::Tts(tag, TtsCmd::Cancel) => {
                self.speaking.take_if(|speaking| speaking.tag == tag);
            }
            Command::Stt(SttCmd::Finalize { .. }) => {
                if self.plan.listen.ask_to_finalize
                    && let Some(stream) = &mut self.hearing
                {
                    stream.tx.try_finalize().expect("the recogniser keeps up");
                }
            }
            Command::Stt(SttCmd::Close { .. }) => self.hearing = None,
            Command::Sense(SenseCmd::EstimateEou { turn, .. }) => {
                // The estimator is right: the speaker has finished when
                // no more speech of theirs is to come soon.
                let soon = self.now_ms() + 600;
                let more = self
                    .edges
                    .front()
                    .is_some_and(|(at, edge)| *edge == VadEdge::Start && *at < soon);
                let p = Prob::from_basis_points(if more { 1_000 } else { 9_500 });
                let at = now.after(Duration::from_millis(30));
                self.answers.push((at, Event::Eou { turn, p }));
            }
            Command::Sense(SenseCmd::ClassifyOverlap { turn, .. }) => {
                let verdict = self.verdict;
                let at = now.after(Duration::from_millis(60));
                self.answers.push((at, Event::Overlap { turn, verdict }));
            }
            Command::Playout(tag, PlayoutCmd::Start) => {
                let _ = self.playout.start(tag, now);
            }
            Command::Playout(tag, PlayoutCmd::Duck) => self.playout.duck(tag),
            Command::Playout(tag, PlayoutCmd::Unduck) => self.playout.unduck(tag),
            Command::Playout(tag, PlayoutCmd::Stop { fade_ms }) => {
                self.playout.stop(tag, fade_ms);
                // The fade is the last of the response to go out. Only
                // then is the far end told to let go of what it holds.
                self.send_frames(now);
                if self.plan.playback.clear_on_stop {
                    let clear = TransportControl::Clear { tag };
                    self.transport
                        .as_mut()
                        .get_mut()
                        .try_control(clear)
                        .expect("the transport keeps up");
                }
            }
            Command::Tool(
                call,
                ToolCmd::Dispatch {
                    tool,
                    args,
                    attempt,
                },
            ) => {
                let args = text.get(args).to_owned();
                let tools = self.parts.tools.clone();
                let calling = async move {
                    let request = ToolRequest {
                        session: SESSION,
                        call,
                        tool,
                        args: &args,
                        attempt,
                    };
                    tools.call(request).await
                };
                self.calls.push((call, Box::pin(calling)));
            }
            Command::Tool(call, ToolCmd::Cancel) => {
                self.calls.retain(|(running, _)| *running != call);
                let tools = self.parts.tools.clone();
                self.undoing.push((
                    call,
                    Box::pin(async move { tools.undo(SESSION, call).await }),
                ));
            }
            Command::SetTimer(id, Some(at)) => {
                self.timers.insert(id, at);
            }
            Command::SetTimer(id, None) => {
                self.timers.remove(&id);
            }
            Command::AwaitDurable(seq) => {
                self.awaited = Some(self.awaited.map_or(seq, |awaited| awaited.max(seq)));
            }
            Command::Emit(event) => self.told.push((self.now_ms(), event)),
        }
    }

    /// The conversation as the kernel has it.
    fn conversation(&self) -> Vec<Said> {
        self.kernel.view().items().map(said).collect()
    }

    /// What the agent said, as far as it was heard, and whether it was
    /// cut off.
    fn utterances(&self) -> Vec<(String, bool)> {
        self.kernel
            .view()
            .items()
            .filter_map(|item| match item {
                ItemView::Utterance {
                    text, truncated, ..
                } => Some((text.to_owned(), truncated)),
                _ => None,
            })
            .collect()
    }

    /// When a command whose description contains `what` was first carried
    /// out, in milliseconds.
    fn first(&self, what: &str) -> Option<u64> {
        self.commands
            .iter()
            .find(|(_, command)| command.contains(what))
            .map(|(at, _)| *at)
    }
}

/// Hands held audio to playback, as much of it as there is room for, and
/// returns how many samples that was. What is left stays held.
fn hand_on(
    held: &mut Option<(PcmBytes, usize)>,
    playout: &mut Playout,
    tag: OpTag,
    now: MonoTime,
    scratch: &mut [i16],
) -> usize {
    let Some((pcm, mut done)) = held.take() else {
        return 0;
    };
    let mut handed = 0;
    while done < pcm.len() {
        let n = pcm.copy_to(done, scratch);
        // Audio of a response that was stopped is not wanted.
        let taken = playout.push(tag, now, &scratch[..n]).unwrap_or(n);
        done += taken;
        handed += taken;
        if taken < n {
            *held = Some((pcm, done));
            break;
        }
    }
    handed
}

/// An item of the kernel's conversation, kept.
fn said(item: ItemView<'_>) -> Said {
    match item {
        ItemView::UserTurn { who, text, .. } => Said::User {
            speaker: Some(who),
            text: text.to_owned(),
        },
        ItemView::Utterance { text, .. } => Said::Assistant {
            text: text.to_owned(),
        },
        ItemView::ToolCall {
            call, tool, args, ..
        } => Said::ToolCall {
            call,
            tool: TOOL_NAMES[usize::from(tool.get())].to_owned(),
            args: args.to_owned(),
        },
        ItemView::ToolResult { call, text, .. } => Said::ToolResult {
            call,
            text: text.to_owned(),
        },
        ItemView::Notice { text, .. } => Said::Notice {
            text: text.to_owned(),
        },
    }
}

/// A kept item, as a model is shown it.
fn message(said: &Said) -> Message<'_> {
    match said {
        Said::User { speaker, text } => Message::User {
            speaker: *speaker,
            text,
        },
        Said::Assistant { text } => Message::Assistant { text },
        Said::ToolCall { call, tool, args } => Message::ToolCall {
            call: *call,
            tool,
            args,
        },
        Said::ToolResult { call, text } => Message::ToolResult { call: *call, text },
        Said::Notice { text } => Message::Notice { text },
    }
}

/// The parts of a session in which the caller says each of `speech`: a
/// start and an end in milliseconds, and the words.
fn parts(speech: &[(u64, u64, &str)], llm: ScriptedLlm) -> Parts {
    let spoken = speech
        .iter()
        .map(|(from, to, text)| Spoken::new(*from, *to, text))
        .collect();
    Parts {
        stt: ScriptedStt::new(SttScript::new(spoken)),
        llm,
        tts: ScriptedTts::default(),
        tools: ScriptedTools::with(|_| ToolReply::Completed("09:15".to_owned())),
        journal: MemoryJournal::new(),
    }
}

fn talks(speech: &[(u64, u64, &str)]) -> Vec<(u64, u64)> {
    speech.iter().map(|(from, to, _)| (*from, *to)).collect()
}

/// The loudest sample of a frame of plain samples.
fn loudest(payload: &[u8]) -> i16 {
    PcmBytes::new(Bytes::copy_from_slice(payload))
        .unwrap()
        .samples()
        .map(i16::saturating_abs)
        .max()
        .unwrap_or(0)
}

#[tokio::test(start_paused = true)]
async fn a_caller_asks_and_hears_the_whole_answer() {
    let speech = [(1_000, 2_200, "what time is it please")];
    let answer = "It is a quarter past nine. Anything else?";
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(answer)]]);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 8_000, &talks(&speech));
    let (mut session, far_end) =
        Session::start(parts(&speech, llm), transport, &talks(&speech)).await;
    assert_eq!(
        session.plan.shortfalls,
        [],
        "these parts leave nothing to wish for"
    );
    session.run_until(8_000).await;

    // The model was shown what the caller said, and what it wrote was
    // spoken, a sentence at a time.
    let asked = session.parts.llm.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].instructions, INSTRUCTIONS);
    assert_eq!(asked[0].last_user(), Some("what time is it please"));
    assert_eq!(asked[0].tools, TOOL_NAMES);
    assert_eq!(
        session.parts.tts.log().spoken,
        [vec![
            "It is a quarter past nine. ".to_owned(),
            "Anything else?".to_owned()
        ]]
    );

    // The caller heard all of it: 41 characters at 60 ms each, in frames
    // of 20 ms that follow one another at the pace they are heard.
    let log = far_end.log();
    let tag = log.frames[0].tag;
    let length = MediaDur::from_millis(41 * 60);
    assert_eq!(log.sent(tag), length);
    assert_eq!(log.heard(tag), length);
    assert_eq!(log.frames.len(), 41 * 3);
    for (n, pair) in log.frames.windows(2).enumerate() {
        assert_eq!(
            pair[1].at,
            pair[0].at.saturating_add(pair[0].length),
            "frame {n}"
        );
        let apart = pair[1].when.saturating_sub(pair[0].when);
        assert!(apart <= Duration::from_millis(20), "frame {n}: {apart:?}");
    }
    assert!(log.frames.last().unwrap().last);
    // At full volume: the synthesiser's tone rises to 7,890.
    let quietest = log.frames.iter().map(|frame| loudest(&frame.payload)).min();
    assert!(quietest >= Some(7_000), "{quietest:?}");
    assert_eq!(log.cleared, [], "nothing was taken back");
    // 2,460 ms of audio took 2,460 ms to send, give or take a frame.
    let took = log
        .frames
        .last()
        .unwrap()
        .when
        .saturating_sub(log.frames[0].when);
    assert!(took.abs_diff(Duration::from_millis(2_440)) <= Duration::from_millis(20));

    // And the conversation says so.
    assert_eq!(
        session.conversation(),
        [
            Said::User {
                speaker: Some(CALLER),
                text: "what time is it please".to_owned()
            },
            Said::Assistant {
                text: answer.to_owned()
            },
        ]
    );
    assert_eq!(session.utterances(), [(answer.to_owned(), false)]);

    // From the end of the caller's speech to the first audio of the
    // answer: the detector's hangover, the estimate, the transcript, the
    // model's first sentence and the synthesiser's first audio.
    let first = u64::try_from(log.frames[0].when.as_millis()).unwrap();
    let waited = first - 2_200;
    assert!((500..900).contains(&waited), "{waited} ms");

    // Everything the kernel decided is in the journal.
    let records = session.parts.journal.records(SESSION);
    let stored: Vec<String> = records
        .iter()
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect();
    for fact in ["ParticipantJoined", "TurnCommitted", "UtteranceCommitted"] {
        assert!(
            stored.iter().any(|record| record.starts_with(fact)),
            "{fact}: {stored:?}"
        );
    }
    assert_eq!(session.parts.stt.log().finalize_requests, 1);
}

#[tokio::test(start_paused = true)]
async fn a_caller_who_interrupts_is_answered_on_what_they_heard() {
    let story = "Of course. Once upon a time there was a ship that sailed all the seas \
                 of the world and never came home.";
    let speech = [
        (1_000, 2_200, "tell me a story"),
        (5_000, 5_800, "stop please"),
    ];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say(story)],
        vec![LlmStep::say("All right.")],
    ]);
    // The far end holds 300 ms of audio before it plays any: what it was
    // sent and what the caller heard are not the same.
    let transport = FakeTransport::sound_card()
        .holding(300)
        .join(0, CALLER)
        .microphone(CALLER, 0, 12_000, &talks(&speech));
    let (mut session, far_end) =
        Session::start(parts(&speech, llm), transport, &talks(&speech)).await;
    session.run_until(12_000).await;

    let log = far_end.log();
    let first = log.frames[0].tag;
    let second = log.frames.last().unwrap().tag;
    assert_ne!(first, second);

    // The agent was lowered within a frame or two of the caller being
    // heard, and stopped once it was clear that they meant it.
    let heard_at = 5_000 + VAD_ONSET_MS;
    let ducked = session.first("Duck").unwrap();
    let stopped = session.first("Stop").unwrap();
    assert!((heard_at..=heard_at + 20).contains(&ducked), "{ducked}");
    assert!((ducked..=ducked + 120).contains(&stopped), "{stopped}");
    let ms =
        |frame: &waveport_testkit::fakes::SentFrame| u64::try_from(frame.when.as_millis()).unwrap();
    let story_frames: Vec<_> = log
        .frames
        .iter()
        .filter(|frame| frame.tag == first)
        .collect();
    for frame in &story_frames {
        let loud = loudest(&frame.payload);
        if ms(frame) < ducked {
            assert!(
                loud >= 7_000,
                "full volume before, at {}: {loud}",
                ms(frame)
            );
        } else if ms(frame) > ducked + 20 {
            assert!(
                (1_500..2_200).contains(&loud),
                "a quarter of it after, at {}: {loud}",
                ms(frame)
            );
        }
    }
    // Nothing of the story was sent after it was stopped, and the far
    // end was told to discard what it held.
    let last = story_frames.last().unwrap();
    assert!(ms(last) <= stopped + 20, "{} after {stopped}", ms(last));
    assert_eq!(log.cleared.len(), 1);
    assert_eq!(log.cleared[0].1, first);
    assert!(last.last, "the fade is the last frame");
    assert_eq!(log.late, 0, "and it went out before the far end was told");
    assert!(
        log.sent(first) < MediaDur::from_millis(104 * 60),
        "not all of it"
    );

    // The conversation holds what the caller heard of the story: whole
    // words, and no word whose audio had not been played. A word is
    // heard when the white space after it is.
    let utterances = session.utterances();
    assert_eq!(utterances.len(), 2);
    let (heard, truncated) = &utterances[0];
    assert!(*truncated);
    assert!(story.starts_with(heard.as_str()), "{heard:?}");
    let rest = &story[heard.len()..];
    assert!(rest.starts_with(' '), "whole words: {heard:?}");
    let played = log.heard(first).as_millis();
    let cut = u32::try_from(heard.len()).unwrap() + 1;
    assert!(
        cut * 60 <= played,
        "{} ms of text, {played} ms played",
        cut * 60
    );
    // The next word was not heard in full, as far as the far end had
    // said: it says what it has played every 100 ms, and plays on for a
    // frame after it is told to stop.
    let next_word = rest[1..].split_inclusive(' ').next().unwrap();
    let next_ms = (cut + u32::try_from(next_word.len()).unwrap()) * 60;
    assert!(
        next_ms + 120 > played,
        "{next_word:?} ends at {next_ms} ms, {played} ms played"
    );
    assert_eq!(utterances[1], ("All right.".to_owned(), false));

    // The second answer was computed from that, not from the story the
    // model wrote.
    let asked = session.parts.llm.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[1].last_user(), Some("stop please"));
    let shown: Vec<&str> = asked[1]
        .messages
        .iter()
        .filter_map(|said| match said {
            Said::Assistant { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(shown, [heard.as_str()]);
    assert_eq!(log.heard(second), MediaDur::from_millis(10 * 60));
}

#[tokio::test(start_paused = true)]
async fn a_tool_call_is_run_and_its_result_is_spoken() {
    let speech = [(1_000, 2_200, "what time is it")];
    let llm = ScriptedLlm::with(|asked| {
        let result = asked.messages.iter().find_map(|said| match said {
            Said::ToolResult { text, .. } => Some(text.clone()),
            _ => None,
        });
        match result {
            None => vec![LlmStep::call("get_time", "{\"zone\":\"local\"}")],
            Some(time) => vec![LlmStep::Say(format!("It is {time}."))],
        }
    });
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 8_000, &talks(&speech));
    let (mut session, far_end) =
        Session::start(parts(&speech, llm), transport, &talks(&speech)).await;
    session.run_until(8_000).await;

    let called = session.parts.tools.called();
    assert_eq!(called.len(), 1);
    assert_eq!(called[0].session, SESSION);
    assert_eq!(called[0].tool, ToolId(0));
    assert_eq!(
        (called[0].args.as_str(), called[0].attempt),
        ("{\"zone\":\"local\"}", 0)
    );
    assert_eq!(session.parts.tools.undone(), []);

    // The model was asked again, with the call and its result.
    let asked = session.parts.llm.asked();
    assert_eq!(asked.len(), 2);
    let call = called[0].call;
    assert_eq!(
        asked[1].messages[1..],
        [
            Said::ToolCall {
                call,
                tool: "get_time".to_owned(),
                args: "{\"zone\":\"local\"}".to_owned()
            },
            Said::ToolResult {
                call,
                text: "09:15".to_owned()
            },
        ]
    );
    assert_eq!(session.utterances(), [("It is 09:15.".to_owned(), false)]);
    let log = far_end.log();
    assert_eq!(log.heard(log.frames[0].tag), MediaDur::from_millis(12 * 60));
}

#[tokio::test(start_paused = true)]
async fn parts_that_can_do_less_still_hold_a_conversation() {
    // A telephone line that neither says what it played nor takes audio
    // back, a recogniser that decides for itself and gives no times, and
    // a synthesiser that says nothing about where its words are.
    let line = TransportCaps {
        inbound: Codec::G711(Law::Mu),
        outbound: Codec::G711(Law::Mu),
        remote_buffer: RemoteBuffer::default(),
        text: false,
    };
    let story = "Of course. Once upon a time there was a ship. It sailed all the seas of \
                 the world and never came home.";
    let speech = [
        (1_000, 2_200, "tell me a story"),
        (5_000, 5_800, "stop please"),
    ];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say(story)],
        vec![LlmStep::say("All right.")],
    ]);
    let mut parts = parts(&speech, llm);
    let mut recogniser = SttScript::new(
        speech
            .iter()
            .map(|(from, to, text)| Spoken::new(*from, *to, text))
            .collect(),
    );
    recogniser.caps.timing = SttTiming::Window;
    recogniser.caps.finalize = SttFinalize::ByProvider;
    parts.stt = ScriptedStt::new(recogniser);
    parts.tts = ScriptedTts::new(TtsScript {
        caps: TtsCaps {
            alignment: Alignment::None,
            input: TtsInput::Whole,
            ..TtsScript::default().caps
        },
        ..TtsScript::default()
    });
    let transport =
        FakeTransport::with(line)
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let (mut session, far_end) = Session::start(parts, transport, &talks(&speech)).await;

    // The plan names every weakness, and is for the line's 8 kHz as far
    // as the synthesiser can go down.
    assert_eq!(
        session.plan.shortfalls,
        [
            Shortfall::WideTranscriptSpans,
            Shortfall::TranscriptsNotOnRequest,
            Shortfall::CoarseSpokenText,
            Shortfall::PlaybackEstimated,
            Shortfall::CannotClear,
        ]
    );
    assert_eq!(session.plan.speak.rate, Rate::Hz16000);
    assert_eq!(session.plan.speak.marks, Marks::Spread);
    session.run_until(12_000).await;

    // The recogniser was never asked, and the turns were still heard.
    assert_eq!(session.parts.stt.log().finalize_requests, 0);
    let asked = session.parts.llm.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].last_user(), Some("tell me a story"));
    assert_eq!(asked[1].last_user(), Some("stop please"));

    // The story was stopped, and nothing was asked of the line that it
    // cannot do.
    let log = far_end.log();
    assert_eq!(log.cleared, []);
    assert_eq!(log.played, [], "the line says nothing");
    // 160 bytes of G.711 for every 20 ms.
    assert!(log.frames.iter().all(|frame| frame.payload.len() == 160));

    // What the caller heard is worked out from the pace, and the cut is
    // by share of characters: a beginning of the story, of about the
    // length that was sent.
    let utterances = session.utterances();
    let (heard, truncated) = &utterances[0];
    assert!(*truncated);
    assert!(
        story.starts_with(heard.as_str()) && !heard.is_empty(),
        "{heard:?}"
    );
    let first = log.frames[0].tag;
    let sent = log.sent(first).as_millis();
    let heard_ms = u32::try_from(heard.len()).unwrap() * 60;
    assert!(heard_ms <= sent, "{heard_ms} ms of text, {sent} ms sent");
    assert!(
        sent - heard_ms < 400,
        "{heard_ms} ms of text, {sent} ms sent"
    );
    assert_eq!(utterances[1], ("All right.".to_owned(), false));
}
