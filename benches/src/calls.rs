//! Whole calls on the real session actor, for measuring.
//!
//! The kernel, the audio path, the compute plane and the actor are the
//! real ones. The parts they reach the world through are stand-ins that
//! follow a script, as in the runtime's own tests, with two differences
//! that make a long run and a run of many calls possible:
//!
//! - The far end of a call is a [`Line`], which makes the caller's audio
//!   as it goes and keeps nothing of what it is sent but counts. The
//!   transport of the tests writes a whole call down before it begins,
//!   and keeps every frame.
//! - The journal is [`Forgetful`]: it says that it has stored what it is
//!   given, and stores nothing.
//!
//! What a session is seen to do is taken down by a [`Watch`], which is
//! the session's sink and times the parts of every turn with the same
//! tracker that the traces and the reports are made from.

use core::future::{Future, ready};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use tokio::time::{Instant, Sleep};
use waveport_adapter_kit::turns::{Spread, Stretch, Tracker, What};
use waveport_domain::config::{AgentSpec, Effect, KernelConfig, ToolSpec};
use waveport_domain::fact::Stamped;
use waveport_domain::ids::{GenId, ParticipantId, Seq, ToolId};
use waveport_domain::text::TextArena;
use waveport_domain::time::MonoTime;
use waveport_media::codec::Encoder;
use waveport_media::codec::opus::OpusSettings;
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::{BoxFuture, Full, PortError};
use waveport_ports::journal::{
    Batch, Journal, JournalError, Lease, Recovered, SessionId, Snapshot,
};
use waveport_ports::model::TurnModel;
use waveport_ports::telemetry::{EventSink, Stage, StageMark};
use waveport_ports::tool::ToolReply;
use waveport_ports::transport::{
    FrameOut, RemoteBuffer, Transport, TransportCaps, TransportControl, TransportEvent,
};
use waveport_runtime::compute::Compute;
use waveport_runtime::context::ToolDef;
use waveport_runtime::session::{Parts, SessionSpec};
use waveport_testkit::fakes::tts::tone;
use waveport_testkit::fakes::{
    LlmStep, LoudVad, ScriptedLlm, ScriptedStt, ScriptedTools, ScriptedTts, Spoken, SttScript,
    TtsScript,
};

/// The caller.
pub const CALLER: ParticipantId = ParticipantId(1);

/// What the agent is told before a call.
const INSTRUCTIONS: &str = "You are a helpful agent on a call.";

/// What the agent answers a caller who listens: a sentence of a second
/// and a quarter as the stand-in for the synthesiser speaks it.
pub const ANSWER: &str = "It is ten past three.";

/// What the agent answers a caller who is going to speak over it: six
/// seconds and a half, so that there is something to speak over.
pub const STORY: &str = "Of course. Once upon a time there was a ship that sailed all the \
                         seas of the world and never came home.";

/// The length of a packet.
const PACKET: Duration = Duration::from_millis(20);

/// How many responses a line remembers the last frame of.
const REMEMBERED: usize = 64;

/// How long the parts of a call that are not this project's take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delays {
    /// The recogniser, from hearing to saying what it heard.
    pub stt: Duration,
    /// The model, to its first word.
    pub llm_first: Duration,
    /// The model, to each word after that.
    pub llm_each: Duration,
    /// The synthesiser, to its first audio.
    pub tts_first: Duration,
}

impl Delays {
    /// Parts that answer at once. What a caller then waits is this
    /// project's own share.
    pub const NONE: Self = Self {
        stt: Duration::ZERO,
        llm_first: Duration::ZERO,
        llm_each: Duration::ZERO,
        tts_first: Duration::ZERO,
    };

    /// Parts that take about as long as good providers do.
    pub const USUAL: Self = Self {
        stt: Duration::from_millis(50),
        llm_first: Duration::from_millis(200),
        llm_each: Duration::from_millis(20),
        tts_first: Duration::from_millis(100),
    };
}

/// How a call goes: who speaks when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Script {
    /// How many times the caller asks.
    pub questions: u32,
    /// When the first question begins.
    pub first: Duration,
    /// How long a question lasts.
    pub question: Duration,
    /// How long from the beginning of one question to the beginning of
    /// the next.
    pub every: Duration,
    /// How long after the end of each question the caller speaks again,
    /// over the answer, if they do. What they say then is a turn of its
    /// own, and is answered.
    pub over: Option<Duration>,
    /// What the agent answers.
    pub answer: &'static str,
    /// What the far end sends and is sent.
    pub codec: Codec,
    /// How long the providers take.
    pub delays: Delays,
}

impl Script {
    /// A call in which the caller asks `questions` times, every eight
    /// seconds, and listens to each answer.
    #[must_use]
    pub const fn asking(questions: u32, codec: Codec, delays: Delays) -> Self {
        Self {
            questions,
            first: Duration::from_millis(500),
            question: Duration::from_millis(1_200),
            every: Duration::from_secs(8),
            over: None,
            answer: ANSWER,
            codec,
            delays,
        }
    }

    /// The same call, in which the agent answers at length and the
    /// caller speaks over every answer, `after` the end of the question.
    /// The questions are then sixteen seconds apart, so that the answer
    /// to what the caller said over the agent is over before the next.
    #[must_use]
    pub const fn interrupting(mut self, after: Duration) -> Self {
        self.over = Some(after);
        self.answer = STORY;
        self.every = Duration::from_secs(16);
        self
    }

    /// When the caller begins to speak over the answer to question
    /// number `n`, counted from 0, if they do.
    #[must_use]
    pub fn speaks_over(&self, n: u32) -> Option<Duration> {
        let after = self.over?;
        Some(self.first + self.every * n + self.question + after)
    }

    /// When the caller speaks: each stretch as its beginning and its end
    /// since the call began, in milliseconds.
    #[must_use]
    pub fn talks(&self) -> Vec<(u64, u64)> {
        let ms = |length: Duration| u64::try_from(length.as_millis()).unwrap_or(u64::MAX);
        let mut talks = Vec::new();
        for n in 0..u64::from(self.questions) {
            let from = ms(self.first) + n * ms(self.every);
            let to = from + ms(self.question);
            talks.push((from, to));
            if let Some(after) = self.over {
                talks.push((to + ms(after), to + ms(after) + ms(self.question)));
            }
        }
        talks
    }

    /// When the caller leaves: long enough after the last question for
    /// the answer to it to be over.
    #[must_use]
    pub fn length(&self) -> Duration {
        self.first + self.every * self.questions
    }
}

/// A second of the caller's audio as the far end sends it: speaking, and
/// silent.
#[derive(Debug)]
pub struct Packets {
    codec: Codec,
    loud: Vec<Bytes>,
    quiet: Vec<Bytes>,
    step: u32,
}

impl Packets {
    /// The packets of a far end that sends `codec`.
    ///
    /// # Panics
    ///
    /// The codec cannot be set up.
    #[must_use]
    pub fn of(codec: Codec) -> Arc<Self> {
        let rate = codec.clock();
        #[expect(
            clippy::expect_used,
            reason = "a measurement that cannot be set up has nothing to measure"
        )]
        let encoded = |talking: bool| -> (Vec<Bytes>, u32) {
            let mut encoder =
                Encoder::new(codec, rate, OpusSettings::default()).expect("the far end's encoder");
            let step = encoder.timestamp_step(rate.frame());
            let mut packet = vec![0; codec.max_packet()];
            let mut frame = vec![0_i16; rate.frame()];
            let packets = (0..50)
                .map(|n| {
                    for (offset, sample) in frame.iter_mut().enumerate() {
                        *sample = if talking {
                            tone(n * rate.frame() + offset)
                        } else {
                            0
                        };
                    }
                    let len = encoder.encode(&frame, &mut packet).unwrap_or(0);
                    Bytes::copy_from_slice(packet.get(..len).unwrap_or(&[]))
                })
                .collect();
            (packets, step)
        };
        let (loud, step) = encoded(true);
        let (quiet, _) = encoded(false);
        Arc::new(Self {
            codec,
            loud,
            quiet,
            step,
        })
    }
}

/// What a line was sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sent {
    /// How many frames.
    pub frames: u64,
    /// When the last frame of each of the latest responses was handed
    /// over, since the call began.
    pub last: Vec<(GenId, Duration)>,
}

impl Sent {
    /// When the last frame of the response `generation` was handed over.
    #[must_use]
    pub fn last_of(&self, generation: GenId) -> Option<Duration> {
        let found = self.last.iter().rev().find(|(of, _)| *of == generation);
        found.map(|(_, at)| *at)
    }
}

/// The far end of a call: a caller who speaks when the script says, and
/// hangs up when it is over.
#[derive(Debug)]
pub struct Line {
    caps: TransportCaps,
    /// When the call began: when the session first asked what had
    /// happened at the far end. The line's clock is made then and
    /// there, on the thread the session runs on.
    began: Option<(Instant, Pin<Box<Sleep>>)>,
    packets: Arc<Packets>,
    talks: Vec<(u64, u64)>,
    /// The first stretch of `talks` that is not over.
    stretch: usize,
    until: Duration,
    /// The number of the next packet.
    next: u32,
    joined: bool,
    left: bool,
    sent: Arc<Mutex<Sent>>,
}

impl Line {
    /// The far end of a call that goes as `script` says. Returns it, and
    /// where to read what it was sent.
    #[must_use]
    pub fn new(script: &Script, packets: &Arc<Packets>) -> (Self, Arc<Mutex<Sent>>) {
        let sent = Arc::new(Mutex::new(Sent::default()));
        let line = Self {
            caps: TransportCaps {
                inbound: packets.codec,
                outbound: packets.codec,
                // A line that cannot take audio back and does not say
                // what it played, as a telephone line.
                remote_buffer: RemoteBuffer::default(),
                text: false,
            },
            began: None,
            packets: Arc::clone(packets),
            talks: script.talks(),
            stretch: 0,
            until: script.length(),
            next: 0,
            joined: false,
            left: false,
            sent: Arc::clone(&sent),
        };
        (line, sent)
    }

    /// Whether the caller speaks `at_ms` after the call began.
    fn talking(&mut self, at_ms: u64) -> bool {
        while self
            .talks
            .get(self.stretch)
            .is_some_and(|(_, to)| *to <= at_ms)
        {
            self.stretch += 1;
        }
        let now = self.talks.get(self.stretch);
        now.is_some_and(|(from, _)| *from <= at_ms)
    }
}

impl Transport for Line {
    fn caps(&self) -> TransportCaps {
        self.caps
    }

    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>> {
        if !self.joined {
            self.joined = true;
            return Poll::Ready(Some(TransportEvent::Joined { who: CALLER }));
        }
        if self.left {
            // The line is closed, which is what ends the session.
            return Poll::Ready(None);
        }
        let began = self
            .began
            .as_ref()
            .map_or_else(Instant::now, |(began, _)| *began);
        loop {
            let since = Instant::now().saturating_duration_since(began);
            if since >= self.until {
                self.left = true;
                return Poll::Ready(Some(TransportEvent::Left { who: CALLER }));
            }
            let due = PACKET * self.next;
            if since >= due {
                let n = self.next;
                self.next += 1;
                let talking = self.talking(u64::from(n) * 20);
                let of = if talking {
                    &self.packets.loud
                } else {
                    &self.packets.quiet
                };
                let which = usize::try_from(n).unwrap_or(0) % of.len().max(1);
                let payload = of.get(which).cloned().unwrap_or_default();
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "sequence numbers wrap round"
                )]
                let sequence = n as u16;
                return Poll::Ready(Some(TransportEvent::Audio {
                    who: CALLER,
                    sequence,
                    timestamp: n.wrapping_mul(self.packets.step),
                    payload,
                }));
            }
            let wake = began + due.min(self.until);
            let (_, timer) = self
                .began
                .get_or_insert_with(|| (began, Box::pin(tokio::time::sleep_until(wake))));
            timer.as_mut().reset(wake);
            if timer.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
    }

    fn try_send(&mut self, frame: FrameOut<'_>) -> Result<(), Full> {
        let began = self
            .began
            .as_ref()
            .map_or_else(Instant::now, |(began, _)| *began);
        let at = Instant::now().saturating_duration_since(began);
        let mut sent = self.sent.lock().unwrap_or_else(PoisonError::into_inner);
        sent.frames += 1;
        let generation = frame.tag.generation;
        if let Some(known) = sent.last.iter_mut().rev().find(|(of, _)| *of == generation) {
            known.1 = at;
        } else {
            if sent.last.len() >= REMEMBERED {
                sent.last.remove(0);
            }
            sent.last.push((generation, at));
        }
        Ok(())
    }

    fn try_control(&mut self, _: TransportControl) -> Result<(), Full> {
        Ok(())
    }
}

/// A journal that says it has stored what it is given, and stores
/// nothing: for measuring what a session costs when its records cost
/// nothing to keep.
#[derive(Clone, Copy, Debug, Default)]
pub struct Forgetful;

impl Journal for Forgetful {
    fn acquire(&self, _: SessionId) -> BoxFuture<'_, Result<Recovered, JournalError>> {
        Box::pin(ready(Ok(Recovered {
            lease: Lease(1),
            snapshot: None,
            tail: Vec::new(),
            next: Seq(1),
        })))
    }

    fn append(
        &self,
        _: SessionId,
        _: Lease,
        batch: Batch,
    ) -> BoxFuture<'_, Result<Seq, JournalError>> {
        let records = u64::try_from(batch.records.len()).unwrap_or(u64::MAX);
        let upto = Seq(batch.first.get() + records.saturating_sub(1));
        Box::pin(ready(Ok(upto)))
    }

    fn snapshot(
        &self,
        _: SessionId,
        _: Lease,
        _: Snapshot,
    ) -> BoxFuture<'_, Result<(), JournalError>> {
        Box::pin(ready(Ok(())))
    }
}

/// How many kinds of part a turn has.
const KINDS: usize = 9;

/// Where the lengths of a kind of part are kept.
const fn kind(what: &What) -> usize {
    match what {
        What::Deciding(_) => 0,
        What::Answering(..) => 1,
        What::Thinking(_) => 2,
        What::Phrasing(_) => 3,
        What::Voicing(_) => 4,
        What::Sending(_) => 5,
        What::Yielding(_) => 6,
        What::Clearing(_) => 7,
        What::Calling(_) => 8,
    }
}

/// A stand-in for the end-of-turn model that is sure of every turn it
/// is asked about, and keeps nothing of what it was asked: the one in
/// the testkit keeps a list, for a test to read, that grows with every
/// turn of every call.
#[derive(Clone, Copy, Debug)]
struct Sure;

impl TurnModel for Sure {
    fn rate(&self) -> Rate {
        Rate::Hz16000
    }

    fn window(&self) -> usize {
        // Eight seconds, as Smart Turn looks at.
        128_000
    }

    fn max_batch(&self) -> usize {
        8
    }

    fn infer(&mut self, turns: &[&[i16]], out: &mut [f32]) -> Result<(), PortError> {
        for (_, likely) in turns.iter().zip(out) {
            *likely = 0.95;
        }
        Ok(())
    }
}

/// What a watch has seen.
#[derive(Debug, Default)]
struct Seen {
    /// One for each session that is under way. In an ordered map, which
    /// holds nothing once the sessions are over: a hashed one keeps its
    /// room, and after enough sessions have come and gone it takes
    /// twice as much, on some machines and not on others.
    trackers: BTreeMap<SessionId, Tracker>,
    /// How long each kind of part took, every time.
    took: [Vec<Duration>; KINDS],
    /// How many parts of each kind there were, kept or not.
    counted: [u64; KINDS],
    /// When each response was lowered and stopped, on its session's
    /// clock.
    lowered: Vec<(SessionId, GenId, MonoTime)>,
    stopped: Vec<(SessionId, GenId, MonoTime)>,
    /// How many sessions said that they closed.
    closed: u64,
}

/// Takes down what sessions do: the sink of every session of a
/// measurement.
#[derive(Debug)]
pub struct Watch {
    /// Whether every length is kept, or only counted. A run of many
    /// long calls counts, so that what it measures does not grow with
    /// what it is measured by.
    keeps: bool,
    seen: Mutex<Seen>,
}

impl Watch {
    /// A watch that keeps every length it is told.
    #[must_use]
    pub fn keeping() -> Arc<Self> {
        Arc::new(Self {
            keeps: true,
            seen: Mutex::default(),
        })
    }

    /// A watch that only counts.
    #[must_use]
    pub fn counting() -> Arc<Self> {
        Arc::new(Self {
            keeps: false,
            seen: Mutex::default(),
        })
    }

    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How the lengths of the kind of part that `what` is one of are
    /// spread.
    #[must_use]
    pub fn spread(&self, what: &What) -> Option<Spread> {
        let mut seen = self.seen();
        seen.took
            .get_mut(kind(what))
            .and_then(|took| Spread::of(took))
    }

    /// How many parts of the kind that `what` is one of there were.
    #[must_use]
    pub fn counted(&self, what: &What) -> u64 {
        self.seen().counted.get(kind(what)).copied().unwrap_or(0)
    }

    /// When each response was lowered, on its session's clock.
    #[must_use]
    pub fn lowered(&self) -> Vec<(SessionId, GenId, MonoTime)> {
        self.seen().lowered.clone()
    }

    /// When each response was stopped, on its session's clock.
    #[must_use]
    pub fn stopped(&self) -> Vec<(SessionId, GenId, MonoTime)> {
        self.seen().stopped.clone()
    }

    /// How many sessions said that they closed.
    #[must_use]
    pub fn closed(&self) -> u64 {
        self.seen().closed
    }

    /// How many sessions are still followed: those that were heard of
    /// and have not said that they closed.
    #[must_use]
    pub fn followed(&self) -> usize {
        self.seen().trackers.len()
    }
}

impl EventSink for Watch {
    fn facts(&self, session: SessionId, at: MonoTime, facts: &[Stamped], _: &TextArena) {
        use waveport_domain::fact::{Fact, Lifecycle};
        let mut seen = self.seen();
        seen.trackers.entry(session).or_default().facts(at, facts);
        let closes = |stamped: &Stamped| {
            let closed = Lifecycle::Closed;
            matches!(stamped.fact, Fact::LifecycleChanged { to, .. } if to == closed)
        };
        if facts.iter().any(closes) {
            seen.closed += 1;
            seen.trackers.remove(&session);
        }
    }

    fn stage(&self, session: SessionId, mark: StageMark) {
        let mut seen = self.seen();
        let seen = &mut *seen;
        if self.keeps {
            match mark.stage {
                Stage::Lowered(generation) => seen.lowered.push((session, generation, mark.at)),
                Stage::Stopped(generation) => seen.stopped.push((session, generation, mark.at)),
                _ => {}
            }
        }
        if matches!(mark.stage, Stage::Stored(_)) {
            return;
        }
        let keeps = self.keeps;
        let (took, counted) = (&mut seen.took, &mut seen.counted);
        let tracker = seen.trackers.entry(session).or_default();
        tracker.mark(mark, |stretch: Stretch| {
            let kind = kind(&stretch.what);
            if let Some(count) = counted.get_mut(kind) {
                *count += 1;
            }
            if keeps && let Some(took) = took.get_mut(kind) {
                took.push(stretch.took());
            }
        });
    }
}

/// The parts every call of a measurement shares.
#[derive(Clone, Debug)]
pub struct Shared {
    llm: ScriptedLlm,
    tts: ScriptedTts,
    compute: Arc<Compute>,
    packets: Arc<Packets>,
    watch: Arc<Watch>,
    script: Script,
}

impl Shared {
    /// The parts of calls that go as `script` says, watched by `watch`.
    ///
    /// # Panics
    ///
    /// The stand-ins for the models that listen cannot be set up.
    #[must_use]
    pub fn new(script: Script, watch: &Arc<Watch>) -> Self {
        let answer = script.answer;
        let mut llm = ScriptedLlm::with(move |_| vec![LlmStep::say(answer)]);
        llm.first = script.delays.llm_first;
        llm.each = script.delays.llm_each;
        llm.forgets = true;
        let tts = ScriptedTts::new(TtsScript {
            first: script.delays.tts_first,
            forgets: true,
            ..TtsScript::default()
        });
        let turns = Box::new(Sure);
        #[expect(
            clippy::expect_used,
            reason = "a measurement that cannot be set up has nothing to measure"
        )]
        let compute = Compute::inline(LoudVad::default(), turns, None)
            .expect("the stand-ins for the models that listen");
        Self {
            llm,
            tts,
            compute: Arc::new(compute),
            packets: Packets::of(script.codec),
            watch: Arc::clone(watch),
            script,
        }
    }

    /// What a session called `id` is made of, and where to read what its
    /// far end was sent. The call begins when the session first asks its
    /// far end what has happened.
    #[must_use]
    pub fn call(&self, id: SessionId) -> (SessionSpec, Parts, Arc<Mutex<Sent>>) {
        self.call_after(id, Duration::ZERO)
    }

    /// The same, for a call in which everything the caller does is
    /// `later` than the script says: calls that begin together then do
    /// not take their turns together.
    #[must_use]
    pub fn call_after(
        &self,
        id: SessionId,
        later: Duration,
    ) -> (SessionSpec, Parts, Arc<Mutex<Sent>>) {
        let script = Script {
            first: self.script.first + later,
            ..self.script
        };
        let spoken = script.talks().into_iter();
        let spoken = spoken.map(|(from, to)| Spoken::new(from, to, "What time is it right now?"));
        let mut heard = SttScript::new(spoken.collect());
        heard.latency = self.script.delays.stt;
        let (line, sent) = Line::new(&script, &self.packets);
        let spec = SessionSpec {
            kernel: KernelConfig {
                agents: vec![AgentSpec {
                    tools: vec![ToolId(0)],
                }],
                tools: vec![ToolSpec::new(Effect::Pure)],
                ..KernelConfig::default()
            },
            tools: vec![ToolDef {
                name: "get_time".to_owned(),
                description: "The time of day.".to_owned(),
                parameters: "{}".to_owned(),
            }],
            ..SessionSpec::new(id, INSTRUCTIONS)
        };
        let tools = ScriptedTools::with(|_| ToolReply::Completed("15:10".to_owned()));
        let sink: Arc<dyn EventSink> = Arc::clone(&self.watch) as Arc<dyn EventSink>;
        let parts = Parts {
            transport: Box::new(line),
            stt: Arc::new(ScriptedStt::new(heard)),
            llm: Arc::new(self.llm.clone()),
            tts: Arc::new(self.tts.clone()),
            tools: Arc::new(tools),
            journal: Arc::new(Forgetful),
            compute: Arc::clone(&self.compute),
            sink: Some(sink),
        };
        (spec, parts, sent)
    }
}

/// A future that is given to nobody else: what a test that runs calls on
/// its own thread waits for.
pub type Alone<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
