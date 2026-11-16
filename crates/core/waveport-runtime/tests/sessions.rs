//! Whole conversations, run by the session actor with a fake in every
//! port.
//!
//! The kernel, the audio path, the compute plane and the actor are the
//! real ones. The parts they reach the world through follow scripts, on a
//! clock the test controls, so every run is the same.

#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;
use std::sync::{Arc, Mutex};

use tokio::time::Instant;
use waveport_domain::command::{Activity, ClientEvent, Command};
use waveport_domain::config::{AgentSpec, Effect, KernelConfig, ToolSpec};
use waveport_domain::event::Event;
use waveport_domain::fact::{Lifecycle, Stamped};
use waveport_domain::ids::{GenId, ParticipantId, Seq, ToolId, TurnId};
use waveport_domain::kernel::{ItemView, View};
use waveport_domain::out::Out;
use waveport_domain::policy::DefaultPolicies;
use waveport_domain::text::TextArena;
use waveport_domain::time::{MediaDur, MonoTime};
use waveport_ports::audio::{Codec, Law, PcmBytes, Rate};
use waveport_ports::error::PortError;
use waveport_ports::journal::{Batch, Journal, SessionId, Snapshot};
use waveport_ports::plan::{CascadePlan, Marks, PlanError, SessionPlan, Shortfall};
use waveport_ports::stt::{SttFinalize, SttTiming};
use waveport_ports::telemetry::{EventSink, Stage, StageMark};
use waveport_ports::tool::ToolReply;
use waveport_ports::transport::{AudioCaps, RemoteBuffer, TransportCaps};
use waveport_ports::tts::{Alignment, TtsCaps, TtsInput};
use waveport_runtime::compute::Compute;
use waveport_runtime::context::ToolDef;
use waveport_runtime::session::{
    Brain, Ended, Parts, Rebuilt, Request, Session, SessionEvent, SessionInput, SessionSpec,
    StartError, StateRule, StateStats, StoreBroken, Tap,
};
use waveport_testkit::fakes::{
    FakeTransport, FakeTransportBuilder, LlmStep, LoudVad, MemoryJournal, Said, ScriptedLlm,
    ScriptedStt, ScriptedTools, ScriptedTts, ScriptedTurns, SentFrame, Spoken, SttScript,
    TransportHandle, TtsScript,
};

const CALLER: ParticipantId = ParticipantId(1);
const SESSION: SessionId = SessionId(0x5E55);
const INSTRUCTIONS: &str = "You are a helpful agent on a call.";

/// What a session did, as a tap saw it.
#[derive(Debug, Default)]
struct Seen {
    /// Every command, and when in milliseconds.
    commands: Vec<(u64, String)>,
    /// What the clients were told.
    told: Vec<(u64, ClientEvent)>,
}

#[derive(Clone, Debug, Default)]
struct Watch(Arc<Mutex<Seen>>);

impl Tap for Watch {
    fn stepped(&mut self, now: MonoTime, _: &Event, out: &Out, _: View<'_>) {
        let mut seen = self.0.lock().unwrap();
        let ms = now.as_nanos() / 1_000_000;
        for command in &out.cmds {
            seen.commands.push((ms, format!("{command:?}")));
            if let Command::Emit(event) = command {
                seen.told.push((ms, *event));
            }
        }
    }
}

impl Watch {
    /// When a command whose description contains `what` was first carried
    /// out, in milliseconds.
    fn first(&self, what: &str) -> Option<u64> {
        let seen = self.0.lock().unwrap();
        seen.commands
            .iter()
            .find(|(_, command)| command.contains(what))
            .map(|(at, _)| *at)
    }
}

/// The parts of a session, kept so that the test can look at them after
/// they were given away.
struct World {
    stt: ScriptedStt,
    llm: ScriptedLlm,
    tts: ScriptedTts,
    tools: ScriptedTools,
    journal: MemoryJournal,
    compute: Arc<Compute>,
    sink: Option<Arc<Recorder>>,
}

/// A sink that keeps what it is told.
#[derive(Debug, Default)]
struct Recorder {
    /// The number of every fact, in the order they came.
    facts: Mutex<Vec<u64>>,
    /// Every part of a turn that was reached, and when in milliseconds.
    stages: Mutex<Vec<(u64, Stage)>>,
}

impl EventSink for Recorder {
    fn facts(&self, session: SessionId, _: MonoTime, facts: &[Stamped], _: &TextArena) {
        assert_eq!(session, SESSION);
        let numbers = facts.iter().map(|stamped| stamped.seq.get());
        self.facts.lock().unwrap().extend(numbers);
    }

    fn stage(&self, session: SessionId, mark: StageMark) {
        assert_eq!(session, SESSION);
        let ms = mark.at.as_nanos() / 1_000_000;
        self.stages.lock().unwrap().push((ms, mark.stage));
    }
}

impl World {
    /// A world in which the caller says each of `speech`: a start and an
    /// end in milliseconds, and the words.
    fn new(speech: &[(u64, u64, &str)], llm: ScriptedLlm) -> Self {
        let spoken = speech
            .iter()
            .map(|(from, to, text)| Spoken::new(*from, *to, text))
            .collect();
        let turns = Box::new(ScriptedTurns::always());
        Self {
            stt: ScriptedStt::new(SttScript::new(spoken)),
            llm,
            tts: ScriptedTts::default(),
            tools: ScriptedTools::with(|_| ToolReply::Completed("09:15".to_owned())),
            journal: MemoryJournal::new(),
            compute: Arc::new(Compute::inline(LoudVad::default(), turns, None).unwrap()),
            sink: None,
        }
    }

    fn parts(&self, transport: FakeTransport) -> Parts {
        Parts {
            brain: Brain::Cascaded {
                stt: Arc::new(self.stt.clone()),
                llm: Arc::new(self.llm.clone()),
                tts: Arc::new(self.tts.clone()),
                compute: Arc::clone(&self.compute),
            },
            transport: Some(Box::new(transport)),
            tools: Arc::new(self.tools.clone()),
            journal: Arc::new(self.journal.clone()),
            sink: self.sink.clone().map(|sink| -> Arc<dyn EventSink> { sink }),
            taps: Vec::new(),
        }
    }
}

/// How a session in which speech is recognised and synthesised is run.
fn cascade(session: &Session) -> &CascadePlan {
    let SessionPlan::Cascaded(plan) = session.plan() else {
        panic!("parts that hear and speak are run as a cascade");
    };
    plan
}

/// A session with one agent that has one tool.
fn spec() -> SessionSpec {
    SessionSpec {
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
        ..SessionSpec::new(SESSION, INSTRUCTIONS)
    }
}

/// A session under way, and what watches it.
struct Running {
    session: Session,
    began: Instant,
    watch: Watch,
    far_end: TransportHandle,
}

impl Running {
    async fn start(world: &World, transport: FakeTransportBuilder) -> Self {
        Self::start_as(spec(), world, transport).await
    }

    async fn start_as(spec: SessionSpec, world: &World, transport: FakeTransportBuilder) -> Self {
        let began = Instant::now();
        let (transport, far_end) = transport.build();
        // The session is watched from its first step.
        let watch = Watch::default();
        let mut parts = world.parts(transport);
        parts.taps.push(Box::new(watch.clone()));
        let session = Session::open(spec, parts, DefaultPolicies::default())
            .await
            .unwrap();
        Self {
            session,
            began,
            watch,
            far_end,
        }
    }

    /// Runs the session until `ms` after its start, or until it ends.
    async fn until(&mut self, ms: u64) -> Option<Ended> {
        let at = self.began + Duration::from_millis(ms);
        tokio::time::timeout_at(at, self.session.run()).await.ok()
    }

    /// The conversation as the kernel has it.
    fn conversation(&self) -> Vec<Said> {
        self.session.view().items().map(said).collect()
    }

    /// What the agent said, as far as it was heard, and whether it was
    /// cut off.
    fn utterances(&self) -> Vec<(String, bool)> {
        self.session
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
}

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
            tool: format!("tool {}", tool.get()),
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

fn talks(speech: &[(u64, u64, &str)]) -> Vec<(u64, u64)> {
    speech.iter().map(|(from, to, _)| (*from, *to)).collect()
}

/// The loudest sample of a frame of plain samples.
fn loudest(payload: &[u8]) -> i16 {
    PcmBytes::new(bytes::Bytes::copy_from_slice(payload))
        .unwrap()
        .samples()
        .map(i16::saturating_abs)
        .max()
        .unwrap_or(0)
}

fn ms(frame: &SentFrame) -> u64 {
    u64::try_from(frame.when.as_millis()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_caller_asks_and_hears_the_whole_answer() {
    let speech = [(1_000, 2_200, "what time is it please")];
    let answer = "It is a quarter past nine. Anything else?";
    let world = World::new(
        &speech,
        ScriptedLlm::replies(vec![vec![LlmStep::say(answer)]]),
    );
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 8_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.session.plan().shortfalls(), []);
    assert_eq!(running.until(8_000).await, None, "the session goes on");

    // The model was shown what the caller said, and what it wrote was
    // spoken, a sentence at a time.
    let asked = world.llm.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].instructions, INSTRUCTIONS);
    assert_eq!(asked[0].last_user(), Some("what time is it please"));
    assert_eq!(asked[0].tools, ["get_time"]);
    assert_eq!(
        world.tts.log().spoken,
        [vec![
            "It is a quarter past nine. ".to_owned(),
            "Anything else?".to_owned()
        ]]
    );

    // The caller heard all of it: 41 characters at 60 ms each, in frames
    // of 20 ms that follow one another at the pace they are heard.
    let log = running.far_end.log();
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
        assert!(apart <= Duration::from_millis(21), "frame {n}: {apart:?}");
    }
    assert!(log.frames.last().unwrap().last);
    let quietest = log.frames.iter().map(|frame| loudest(&frame.payload)).min();
    assert!(quietest >= Some(7_000), "{quietest:?}");
    assert_eq!(log.cleared, []);
    assert_eq!(log.late, 0);

    assert_eq!(
        running.conversation(),
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
    assert_eq!(running.utterances(), [(answer.to_owned(), false)]);
    // From the end of the caller's speech to the first audio of the
    // answer: the quiet that ends a turn, the transcript, the model's
    // first sentence and the synthesiser's first audio.
    let waited = ms(&log.frames[0]) - 2_200;
    assert!((500..900).contains(&waited), "{waited} ms");

    // Everything the kernel decided is in the journal, as records.
    let records = world.journal.records(SESSION);
    let stored: Vec<String> = records
        .iter()
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect();
    for kind in [
        "participant_joined",
        "turn_committed",
        "utterance_committed",
    ] {
        assert!(
            stored.iter().any(|record| record.contains(kind)),
            "{kind}: {stored:?}"
        );
    }
    assert_eq!(world.stt.log().finalize_requests, 1);
    assert_eq!(
        running.session.stats(),
        waveport_runtime::session::SessionStats::default()
    );
}

const STORY: &str = "Of course. Once upon a time there was a ship that sailed all the seas \
                     of the world and never came home.";

#[tokio::test(start_paused = true)]
async fn a_caller_who_interrupts_is_answered_on_what_they_heard() {
    let speech = [
        (1_000, 2_200, "tell me a story"),
        (5_000, 5_800, "stop please"),
    ];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say(STORY)],
        vec![LlmStep::say("All right.")],
    ]);
    let world = World::new(&speech, llm);
    // The far end holds 300 ms of audio before it plays any: what it was
    // sent and what the caller heard are not the same.
    let transport = FakeTransport::sound_card()
        .holding(300)
        .join(0, CALLER)
        .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(12_000).await, None);

    let log = running.far_end.log();
    let first = log.frames[0].tag;
    let second = log.frames.last().unwrap().tag;
    assert_ne!(first, second);

    // The agent was lowered as soon as the caller was heard to speak: two
    // frames of the listening model after they began. With no model to
    // say what the speech was, it was stopped when the speech had gone on
    // for half a second.
    let ducked = running.watch.first("Duck").unwrap();
    let stopped = running.watch.first("Stop").unwrap();
    assert!((5_032..=5_100).contains(&ducked), "{ducked}");
    assert!(
        (ducked + 440..=ducked + 520).contains(&stopped),
        "{stopped}"
    );
    let story: Vec<_> = log
        .frames
        .iter()
        .filter(|frame| frame.tag == first)
        .collect();
    for frame in &story {
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
    // Nothing of the story was sent after it was stopped. The fade was
    // its last frame, and then the far end was told to discard.
    let last = story.last().unwrap();
    assert!(ms(last) <= stopped + 20, "{} after {stopped}", ms(last));
    assert!(last.last);
    assert_eq!(log.cleared.len(), 1);
    assert_eq!(log.cleared[0].1, first);
    assert_eq!(log.late, 0);

    // The conversation holds what the caller heard of the story: whole
    // words, and no word whose audio the far end had not played.
    let utterances = running.utterances();
    assert_eq!(utterances.len(), 2);
    let (heard, truncated) = &utterances[0];
    assert!(*truncated);
    assert!(STORY.starts_with(heard.as_str()), "{heard:?}");
    let rest = &STORY[heard.len()..];
    assert!(rest.starts_with(' '), "whole words: {heard:?}");
    let played = log.heard(first).as_millis();
    let sent = log.sent(first).as_millis();
    assert!(sent >= played + 280, "{sent} ms sent, {played} ms played");
    let cut = u32::try_from(heard.len()).unwrap() + 1;
    assert!(
        cut * 60 <= played,
        "{} ms of text, {played} ms played",
        cut * 60
    );
    let next_word = rest[1..].split_inclusive(' ').next().unwrap();
    let next_ms = (cut + u32::try_from(next_word.len()).unwrap()) * 60;
    assert!(
        next_ms + 120 > played,
        "{next_word:?} ends at {next_ms} ms, {played} ms played"
    );
    assert_eq!(utterances[1], ("All right.".to_owned(), false));

    // The second answer was computed from that, not from the story the
    // model wrote.
    let asked = world.llm.asked();
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
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 8_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(8_000).await, None);

    let called = world.tools.called();
    assert_eq!(called.len(), 1);
    assert_eq!(called[0].session, SESSION);
    assert_eq!(called[0].tool, ToolId(0));
    assert_eq!(
        (called[0].args.as_str(), called[0].attempt),
        ("{\"zone\":\"local\"}", 0)
    );
    assert_eq!(world.tools.undone(), []);

    // The model was asked again, with the call and its result.
    let asked = world.llm.asked();
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
    assert_eq!(running.utterances(), [("It is 09:15.".to_owned(), false)]);
    let log = running.far_end.log();
    assert_eq!(log.heard(log.frames[0].tag), MediaDur::from_millis(12 * 60));
}

#[tokio::test(start_paused = true)]
async fn a_sink_is_told_every_fact_and_when_each_part_of_a_turn_was_reached() {
    let speech = [(1_000, 2_200, "what time is it")];
    let mut world = World::new(&speech, calls_a_tool());
    let recorder = Arc::new(Recorder::default());
    world.sink = Some(Arc::clone(&recorder));
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 8_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(8_000).await, None);

    // Every fact, once and in order: the numbers the journal has.
    let stored = u64::try_from(world.journal.records(SESSION).len()).unwrap();
    assert!(stored > 5, "{stored}");
    let facts = recorder.facts.lock().unwrap().clone();
    assert_eq!(facts, (1..=stored).collect::<Vec<u64>>());

    // The parts of the turn, in the order they were reached. The model
    // asked for the time before it said anything, so its first response
    // was never heard, and the second was.
    let (kept, stages): (Vec<_>, Vec<_>) = recorder
        .stages
        .lock()
        .unwrap()
        .iter()
        .partition(|(_, stage)| matches!(stage, Stage::Stored(_)));
    let called = world.tools.called();
    let call = called[0].call;
    let log = running.far_end.log();
    let (first, second) = (GenId(1), GenId(2));
    assert_eq!(log.frames[0].tag.generation, second);
    let reached: Vec<Stage> = stages.iter().map(|(_, stage)| *stage).collect();
    assert_eq!(
        reached,
        [
            Stage::TranscriptFinal(TurnId(1)),
            Stage::TurnEnded(TurnId(1)),
            Stage::Asked(first),
            Stage::FirstToken(first),
            Stage::ToolSent(call),
            Stage::ToolBack(call),
            Stage::Asked(second),
            Stage::FirstToken(second),
            Stage::FirstText(second),
            Stage::FirstAudio(second),
            Stage::FirstFrameOut(second),
        ]
    );
    // Each at the moment it happened: the caller stopped speaking at
    // 2,200 ms, and the first frame of the answer went out when the far
    // end says it came.
    let times: Vec<u64> = stages.iter().map(|(at, _)| *at).collect();
    assert!(times.is_sorted(), "{times:?}");
    assert!((2_200..2_700).contains(&times[0]), "{times:?}");
    assert_eq!(times[10], ms(&log.frames[0]));
    // The model is asked the moment the turn has ended, and again the
    // moment the tool is back.
    assert_eq!(times[2], times[1]);
    assert_eq!(times[6], times[5]);
    // What the caller waited for the answer, and where it went: 200 ms
    // for each response to begin, 150 ms for the tool, 20 ms for the
    // model to write the rest of its first sentence, and 100 ms for the
    // synthesiser.
    assert_eq!(times[3] - times[2], 200);
    assert_eq!(times[5] - times[4], 150);
    assert_eq!(times[7] - times[6], 200);
    assert_eq!(times[8] - times[7], 20);
    assert_eq!(times[9] - times[8], 100);

    // What the journal said it had stored: more each time, and in the
    // end everything.
    let upto: Vec<u64> = kept
        .iter()
        .map(|(_, stage)| match stage {
            Stage::Stored(seq) => seq.get(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(upto.is_sorted_by(|a, b| a < b), "{upto:?}");
    assert_eq!(upto.last(), Some(&stored));
}

#[tokio::test(start_paused = true)]
async fn a_sink_is_told_when_the_agent_was_lowered_and_stopped() {
    let speech = [
        (1_000, 2_200, "tell me a story"),
        (5_000, 5_800, "stop please"),
    ];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say(STORY)],
        vec![LlmStep::say("All right.")],
    ]);
    let mut world = World::new(&speech, llm);
    let recorder = Arc::new(Recorder::default());
    world.sink = Some(Arc::clone(&recorder));
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(12_000).await, None);

    // The marks are made at the commands they are about.
    let stages = recorder.stages.lock().unwrap().clone();
    let at = |wanted: Stage| {
        let found = stages.iter().find(|(_, stage)| *stage == wanted);
        found.map(|(at, _)| *at)
    };
    let story = GenId(1);
    assert_eq!(at(Stage::Lowered(story)), running.watch.first("Duck"));
    assert_eq!(at(Stage::Stopped(story)), running.watch.first("Stop"));
    assert!(at(Stage::Lowered(story)) < at(Stage::Stopped(story)));
    // The answer to the interruption has marks of its own.
    assert!(at(Stage::FirstFrameOut(GenId(2))) > at(Stage::Stopped(story)));
}

#[tokio::test(start_paused = true)]
async fn parts_that_can_do_less_still_hold_a_conversation() {
    // A telephone line that neither says what it played nor takes audio
    // back, a recogniser that decides for itself and gives no times, and
    // a synthesiser that says nothing about where its words are.
    let line = TransportCaps {
        audio: Some(AudioCaps {
            inbound: Codec::G711(Law::Mu),
            outbound: Codec::G711(Law::Mu),
            remote_buffer: RemoteBuffer::default(),
        }),
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
    let mut world = World::new(&speech, llm);
    let mut recogniser = SttScript::new(
        speech
            .iter()
            .map(|(from, to, text)| Spoken::new(*from, *to, text))
            .collect(),
    );
    recogniser.caps.timing = SttTiming::Window;
    recogniser.caps.finalize = SttFinalize::ByProvider;
    world.stt = ScriptedStt::new(recogniser);
    world.tts = ScriptedTts::new(TtsScript {
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
    let mut running = Running::start(&world, transport).await;

    // The plan names every weakness, and is for the line's 8 kHz as far
    // as the synthesiser can go down.
    let plan = cascade(&running.session);
    assert_eq!(
        plan.shortfalls,
        [
            Shortfall::WideTranscriptSpans,
            Shortfall::TranscriptsNotOnRequest,
            Shortfall::CoarseSpokenText,
            Shortfall::PlaybackEstimated,
            Shortfall::CannotClear,
        ]
    );
    assert_eq!(plan.speak.rate, Rate::Hz16000);
    assert_eq!(plan.speak.marks, Marks::Spread);
    assert_eq!(running.until(12_000).await, None);

    // The recogniser was never asked, and the turns were still heard.
    assert_eq!(world.stt.log().finalize_requests, 0);
    let asked = world.llm.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].last_user(), Some("tell me a story"));
    assert_eq!(asked[1].last_user(), Some("stop please"));

    // The story was stopped, and nothing was asked of the line that it
    // cannot do.
    let log = running.far_end.log();
    assert_eq!(log.cleared, []);
    assert_eq!(log.played, [], "the line says nothing");
    // 160 bytes of G.711 for every 20 ms.
    assert!(log.frames.iter().all(|frame| frame.payload.len() == 160));

    // What the caller heard is worked out from the pace, and the cut is
    // by share of characters: a beginning of the story, of about the
    // length that was sent.
    let utterances = running.utterances();
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

#[tokio::test(start_paused = true)]
async fn a_synthesiser_given_text_by_the_piece_that_gives_no_times_says_all_of_it() {
    let story = "Of course. Once upon a time there was a ship. It sailed all the seas of \
                 the world.";
    let speech = [(1_000, 2_200, "tell me a story")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(story)]]);
    let mut world = World::new(&speech, llm);
    // It is given a sentence at a time, and says of none of them how far
    // into it the audio is.
    world.tts = ScriptedTts::new(TtsScript {
        caps: TtsCaps {
            alignment: Alignment::None,
            input: TtsInput::Incremental,
            ..TtsScript::default().caps
        },
        ..TtsScript::default()
    });
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(cascade(&running.session).speak.marks, Marks::Spread);
    assert_eq!(running.until(12_000).await, None);

    // Every sentence was said, not only the first: the ones after it do
    // not wait for the synthesiser to say that it has finished.
    assert_eq!(running.utterances(), [(story.to_owned(), false)]);
    let log = running.far_end.log();
    let length = u32::try_from(story.len()).unwrap() * 60;
    assert_eq!(log.heard(log.frames[0].tag), MediaDur::from_millis(length));
}

#[tokio::test(start_paused = true)]
async fn a_synthesiser_that_marks_only_its_words_says_all_of_it() {
    let story = "Of course. Once upon a time there was a ship. It sailed all the seas of \
                 the world.";
    let speech = [(1_000, 2_200, "tell me a story")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(story)]]);
    let mut world = World::new(&speech, llm);
    // As providers do it: a word is marked where the word ends. The space
    // after the last word of a sentence is never marked, and the sentence
    // after it must not wait for that.
    world.tts = ScriptedTts::new(TtsScript {
        bare_words: true,
        ..TtsScript::default()
    });
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(cascade(&running.session).speak.marks, Marks::Timed);
    assert_eq!(running.until(12_000).await, None);

    assert_eq!(running.utterances(), [(story.to_owned(), false)]);
    // It was given a sentence at a time, each when the one before it had
    // its audio, and none of them late: the story was heard without a
    // break, in the time its characters take.
    let given = world.tts.log().spoken;
    assert_eq!(given.len(), 1);
    assert!(given[0].len() >= 3, "{given:?}");
    let log = running.far_end.log();
    let length = u32::try_from(story.len()).unwrap() * 60;
    assert_eq!(log.heard(log.frames[0].tag), MediaDur::from_millis(length));
    let gaps = log
        .frames
        .windows(2)
        .filter(|pair| ms(&pair[1]) - ms(&pair[0]) > 25)
        .count();
    assert_eq!(gaps, 0);
}

#[tokio::test(start_paused = true)]
async fn a_word_that_is_never_marked_does_not_hold_the_rest_back() {
    let story = "It is 3 o'clock. That is all I know. Goodbye.";
    let speech = [(1_000, 2_200, "what time is it")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(story)]]);
    let mut world = World::new(&speech, llm);
    // The synthesiser says "o'clock" its own way, and marks no such word:
    // the last word of the first sentence is never said to have been
    // reached.
    world.tts = ScriptedTts::new(TtsScript {
        bare_words: true,
        unmarked: Some("clock".to_owned()),
        ..TtsScript::default()
    });
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(12_000).await, None);

    // The rest is said all the same, once the synthesiser has had nothing
    // more to give for a moment.
    assert_eq!(running.utterances(), [(story.to_owned(), false)]);
    let log = running.far_end.log();
    let length = u32::try_from(story.len()).unwrap() * 60;
    assert_eq!(log.heard(log.frames[0].tag), MediaDur::from_millis(length));
}

#[tokio::test(start_paused = true)]
async fn an_application_makes_the_agent_speak_stop_and_leave() {
    let world = World::new(&[], ScriptedLlm::replies(vec![]));
    let transport = FakeTransport::sound_card()
        .holding(100)
        .join(0, CALLER)
        .microphone(CALLER, 0, 20_000, &[]);
    let mut running = Running::start(&world, transport).await;
    let (ask, inbox) = tokio::sync::mpsc::channel(8);
    running.session.listen_to(inbox);

    ask.try_send(Request::Input(SessionInput::Say {
        text: "Welcome. This call may be recorded for training purposes.".to_owned(),
    }))
    .unwrap();
    assert_eq!(running.until(1_500).await, None);
    ask.try_send(SessionInput::Interrupt.into()).unwrap();
    assert_eq!(running.until(3_000).await, None);
    ask.try_send(Request::Input(SessionInput::Say {
        text: "Goodbye.".to_owned(),
    }))
    .unwrap();
    ask.try_send(SessionInput::Close.into()).unwrap();
    // The agent finishes what it is saying, and then the session ends
    // with everything it decided in the journal.
    assert_eq!(running.until(20_000).await, Some(Ended::Closed));
    let took = running.began.elapsed();
    assert!(took < Duration::from_millis(4_000), "{took:?}");

    // Nothing was asked of the model: the application's words were
    // spoken as they were.
    assert_eq!(world.llm.asked(), []);
    let utterances = running.utterances();
    assert_eq!(utterances.len(), 2);
    let (welcome, truncated) = &utterances[0];
    assert!(*truncated);
    assert!(
        welcome.starts_with("Welcome. ") && welcome.len() < 30,
        "{welcome:?}"
    );
    assert_eq!(utterances[1], ("Goodbye.".to_owned(), false));
    let log = running.far_end.log();
    let goodbye = log.frames.last().unwrap().tag;
    assert_eq!(log.heard(goodbye), MediaDur::from_millis(8 * 60));
    assert_eq!(log.cleared.len(), 1);
    assert_eq!(log.late, 0);
    // The far end was told that the session is over, once, and nothing
    // was sent after that.
    assert_eq!(log.goodbyes, 1);
    assert_eq!(log.after_close, 0);
    // And the recogniser was let go of in good order: told that no more
    // audio follows, and read to its end.
    assert_eq!(world.stt.log().let_go, 1);

    let kinds = kinds(&world.journal.records(SESSION));
    assert_eq!(
        kinds,
        [
            "participant_joined",
            "utterance_progress",
            "utterance_committed",
            "epoch_advanced",
            "lifecycle_changed draining",
            "utterance_committed",
            "lifecycle_changed closing",
            "lifecycle_changed closed",
        ]
    );
    assert!(
        running
            .watch
            .0
            .lock()
            .unwrap()
            .told
            .iter()
            .any(|(_, told)| matches!(told, ClientEvent::Lifecycle(Lifecycle::Closed)))
    );
}

/// The kind of each record, and for a change of lifecycle what to.
#[tokio::test(start_paused = true)]
async fn what_is_asked_as_a_caller_joins_finds_the_caller_there() {
    let world = World::new(
        &[],
        ScriptedLlm::replies(vec![vec![LlmStep::say("Hello.")]]),
    );
    let transport = FakeTransport::sound_card().join(0, CALLER);
    let mut running = Running::start(&world, transport).await;
    // The caller is on the line when the session starts, and the text
    // they typed is waiting when the session first runs: it meets both
    // in the same run.
    let (requests, inbox) = tokio::sync::mpsc::channel(8);
    running.session.listen_to(inbox);
    let typed = SessionInput::Text {
        who: CALLER,
        text: "hello".to_owned(),
    };
    requests.send(Request::Input(typed)).await.unwrap();
    assert_eq!(running.until(3_000).await, None);

    // The text is a turn of the caller's, and is answered. Taken before
    // the caller was known to be there, it would have been dropped as
    // the text of a stranger.
    let asked = world.llm.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].last_user(), Some("hello"));
    assert_eq!(running.utterances(), [("Hello.".to_owned(), false)]);
}

#[tokio::test(start_paused = true)]
async fn what_is_typed_into_a_call_stops_what_is_being_said() {
    let speech = [(1_000, 2_200, "tell me a story")];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say(STORY)],
        vec![LlmStep::say("All right.")],
    ]);
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    let (requests, inbox) = tokio::sync::mpsc::channel(8);
    running.session.listen_to(inbox);

    // The agent is in the middle of its story when the caller types.
    assert_eq!(running.until(4_500).await, None);
    assert_eq!(running.utterances(), []);
    let typed = SessionInput::Text {
        who: CALLER,
        text: "stop please".to_owned(),
    };
    requests.send(Request::Input(typed)).await.unwrap();
    assert_eq!(running.until(12_000).await, None);

    // The story stopped when the text came, and not when it would have
    // ended: what is typed takes the floor, as speech does.
    let stopped = running.watch.first("Stop {").unwrap();
    assert!((4_500..4_600).contains(&stopped), "stopped at {stopped} ms");
    let utterances = running.utterances();
    assert_eq!(utterances.len(), 2, "{utterances:?}");
    let (heard, cut) = &utterances[0];
    assert!(*cut, "{heard:?}");
    assert!(
        STORY.starts_with(heard.as_str()) && heard.len() < STORY.len(),
        "{heard:?}"
    );
    assert_eq!(utterances[1], ("All right.".to_owned(), false));
    // The answer is to what was typed, with what was heard of the story
    // before it.
    let asked = world.llm.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[1].last_user(), Some("stop please"));
    assert!(
        asked[1].messages.contains(&Said::Assistant {
            text: heard.clone()
        }),
        "{:?}",
        asked[1].messages
    );
}

fn kinds(records: &[bytes::Bytes]) -> Vec<String> {
    records
        .iter()
        .map(|record| {
            let record: serde_json::Value = serde_json::from_slice(record).unwrap();
            let fact = &record["fact"];
            let kind = fact["kind"].as_str().unwrap();
            match fact["to"].as_str() {
                Some(to) if kind == "lifecycle_changed" => format!("{kind} {to}"),
                _ => kind.to_owned(),
            }
        })
        .collect()
}

/// The numbers of the records, which are to follow one another from 1.
fn numbers(records: &[bytes::Bytes]) -> Vec<u64> {
    records
        .iter()
        .map(|record| {
            let record: serde_json::Value = serde_json::from_slice(record).unwrap();
            record["seq"].as_u64().unwrap()
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_transport_that_closes_ends_the_session() {
    let speech = [(1_000, 2_200, "what time is it please")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(STORY)]]);
    let world = World::new(&speech, llm);
    // The caller hangs up while the agent is speaking.
    let transport = FakeTransport::sound_card()
        .join(0, CALLER)
        .microphone(CALLER, 0, 4_000, &talks(&speech))
        .then_closes();
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(30_000).await, Some(Ended::Closed));
    let took = running.began.elapsed();
    assert!(took < Duration::from_millis(4_100), "{took:?}");

    // Nobody heard the rest of the story, so it is not in the
    // conversation, and none of it was sent to a transport that had
    // closed.
    let utterances = running.utterances();
    assert_eq!(utterances.len(), 1);
    let (heard, truncated) = &utterances[0];
    assert!(*truncated);
    assert!(
        STORY.starts_with(heard.as_str()) && heard.len() < 30,
        "{heard:?}"
    );
    assert_eq!(running.far_end.log().after_close, 0);
    // Nobody is there to be told that the session is over.
    assert_eq!(running.far_end.log().goodbyes, 0);
    // The recogniser of whoever left is let go of all the same.
    assert_eq!(world.stt.log().let_go, 1);

    // The agent is cut off because its line is gone, and the session
    // closes because the line is gone for good.
    let records = world.journal.records(SESSION);
    assert_eq!(
        kinds(&records)[3..],
        [
            "utterance_committed",
            "epoch_advanced",
            "lifecycle_changed draining",
            "lifecycle_changed closing",
            "lifecycle_changed closed",
        ]
    );
    let last = String::from_utf8_lossy(records.last().unwrap()).into_owned();
    assert!(last.contains("\"reason\":\"abandoned\""), "{last}");
}

#[tokio::test(start_paused = true)]
async fn a_line_that_drops_is_waited_for_and_its_caller_is_answered_again() {
    let speech = [(1_000, 2_200, "what time is it please")];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say(STORY)],
        vec![LlmStep::say("It is ten past three.")],
    ]);
    let world = World::new(&speech, llm);
    // The line drops while the agent is speaking, and is back three
    // seconds later.
    let transport = FakeTransport::sound_card()
        .join(0, CALLER)
        .microphone(CALLER, 0, 12_000, &talks(&speech))
        .line_drops(4_000)
        .line_returns(7_000);
    let mut running = Running::start(&world, transport).await;

    // While the line is gone the agent has stopped where it was heard
    // to, and says nothing: no model is asked, and nothing is sent.
    assert_eq!(running.until(6_900).await, None);
    let stopped = running.watch.first("Stop {").unwrap();
    assert!((4_000..4_100).contains(&stopped), "stopped at {stopped} ms");
    let utterances = running.utterances();
    assert_eq!(utterances.len(), 1, "{utterances:?}");
    let (heard, cut) = utterances[0].clone();
    assert!(cut, "{heard:?}");
    assert!(
        STORY.starts_with(heard.as_str()) && heard.len() < 30,
        "{heard:?}"
    );
    assert!(!running.session.view().is_attached());
    assert_eq!(world.llm.asked().len(), 1);
    let sent = running.far_end.log().frames.len();

    // When it is back, the caller is given the reply they did not get.
    assert_eq!(running.until(20_000).await, None);
    assert!(running.session.view().is_attached());
    let utterances = running.utterances();
    assert_eq!(utterances.len(), 2, "{utterances:?}");
    assert_eq!(utterances[1], ("It is ten past three.".to_owned(), false));
    // All that went into the gap is the fade of what was cut off.
    let log = running.far_end.log();
    assert_eq!((log.drops, log.unheard), (1, 1));
    assert!(log.frames[sent..].iter().all(|frame| ms(frame) >= 7_000));
    // The model was asked again for the same turn, and shown what the
    // caller had heard of the first reply.
    let asked = world.llm.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[1].last_user(), Some("what time is it please"));
    assert!(
        asked[1].messages.contains(&Said::Assistant { text: heard }),
        "{:?}",
        asked[1].messages
    );
}

#[tokio::test(start_paused = true)]
async fn a_line_that_stays_gone_ends_the_session_when_it_has_waited() {
    let speech = [(1_000, 2_200, "what time is it please")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(STORY)]]);
    let world = World::new(&speech, llm);
    let transport = FakeTransport::sound_card()
        .join(0, CALLER)
        .microphone(CALLER, 0, 12_000, &talks(&speech))
        .line_drops(4_000);
    let mut spec = spec();
    spec.kernel.timing.detach_wait = Duration::from_secs(5);
    let mut running = Running::start_as(spec, &world, transport).await;

    // Just before the wait is over the session is there, and then it is
    // not.
    assert_eq!(running.until(8_950).await, None);
    assert_eq!(running.until(30_000).await, Some(Ended::Closed));
    let took = running.began.elapsed();
    assert!(
        (Duration::from_millis(9_000)..Duration::from_millis(9_100)).contains(&took),
        "{took:?}"
    );
    let records = world.journal.records(SESSION);
    assert_eq!(
        kinds(&records)[3..],
        [
            "utterance_committed",
            "epoch_advanced",
            "lifecycle_changed draining",
            "lifecycle_changed closing",
            "lifecycle_changed closed",
        ]
    );
    let last = String::from_utf8_lossy(records.last().unwrap()).into_owned();
    assert!(last.contains("\"reason\":\"abandoned\""), "{last}");
    // Nothing was said into the line while it was gone, beyond the fade
    // of what was cut off.
    assert_eq!(running.far_end.log().unheard, 1);
    assert_eq!(world.llm.asked().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_session_whose_transport_closed_says_nothing_more() {
    let speech = [(1_000, 2_200, "what time is it please")];
    // The model tells a story and asks for the time at once. The time is
    // there while the story is told, and is owed to the caller after it.
    let llm = ScriptedLlm::with(|asked| {
        let has_result = asked
            .messages
            .iter()
            .any(|said| matches!(said, Said::ToolResult { .. }));
        if has_result {
            vec![LlmStep::say("It is 09:15.")]
        } else {
            vec![LlmStep::say(STORY), LlmStep::call("get_time", "{}")]
        }
    });
    let world = World::new(&speech, llm);
    // The caller hangs up while the agent is telling the story.
    let transport = FakeTransport::sound_card()
        .join(0, CALLER)
        .microphone(CALLER, 0, 4_000, &talks(&speech))
        .then_closes();
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(30_000).await, Some(Ended::Closed));
    // It closed when the line did, not after the time it would have
    // taken to say what was owed.
    let took = running.began.elapsed();
    assert!(took < Duration::from_millis(4_100), "{took:?}");

    // The tool was called, and the model was not asked what to say of
    // its result: there was nobody left to say it to.
    assert_eq!(world.tools.called().len(), 1);
    assert_eq!(world.llm.asked().len(), 1);
    let utterances = running.utterances();
    assert_eq!(utterances.len(), 1, "{utterances:?}");
    let (heard, truncated) = &utterances[0];
    assert!(*truncated && STORY.starts_with(heard.as_str()), "{heard:?}");
    assert_eq!(running.far_end.log().after_close, 0);
}

#[tokio::test(start_paused = true)]
async fn a_recogniser_that_cannot_be_opened_is_given_up() {
    let speech = [(1_000, 2_200, "what time is it please")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say("It is late.")]]);
    let mut world = World::new(&speech, llm);
    let mut script = SttScript::new(vec![]);
    script.fail_open = Some(PortError::transient("no route"));
    world.stt = ScriptedStt::new(script);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 30_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    // It is tried again after 0.2 s, and then after twice as long each
    // time up to 5 s: six times in all, over 11.2 s. A session that
    // cannot hear is of no use, so then it closes, and says why.
    assert_eq!(running.until(30_000).await, Some(Ended::Closed));
    let took = running.began.elapsed();
    assert!(
        (Duration::from_millis(11_200)..Duration::from_millis(11_300)).contains(&took),
        "{took:?}"
    );
    assert_eq!(running.session.stats().hearing.recogniser_failures, 7);
    assert_eq!(world.stt.log().opened, []);
    assert_eq!(world.llm.asked(), []);

    // The caller was heard to speak and not understood.
    let records = world.journal.records(SESSION);
    assert_eq!(
        kinds(&records),
        [
            "participant_joined",
            "turn_dismissed",
            "lifecycle_changed draining",
            "lifecycle_changed closing",
            "lifecycle_changed closed",
        ]
    );
    let last = String::from_utf8_lossy(records.last().unwrap()).into_owned();
    assert!(last.contains("\"reason\":\"failure\""), "{last}");
}

#[tokio::test(start_paused = true)]
async fn a_recogniser_that_fails_is_opened_again() {
    let speech = [
        (1_000, 2_200, "what time is it please"),
        (6_000, 7_200, "and what day is it"),
    ];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say("It is late.")],
        vec![LlmStep::say("Monday.")],
    ]);
    let mut world = World::new(&speech, llm);
    let mut script = SttScript::new(
        speech
            .iter()
            .map(|(from, to, text)| Spoken::new(*from, *to, text))
            .collect(),
    );
    script.fail_after = Some((8, PortError::transient("reset by peer")));
    world.stt = ScriptedStt::new(script);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(12_000).await, None);

    // The stream broke off two words into the caller's second question.
    // The one opened in its place was given the question from its start,
    // so all of it was understood.
    let heard = world.stt.log();
    assert_eq!(heard.opened.len(), 2);
    assert_eq!(running.session.stats().hearing.recogniser_failures, 1);
    assert_eq!(
        running.conversation(),
        [
            Said::User {
                speaker: Some(CALLER),
                text: "what time is it please".to_owned()
            },
            Said::Assistant {
                text: "It is late.".to_owned()
            },
            Said::User {
                speaker: Some(CALLER),
                text: "and what day is it".to_owned()
            },
            Said::Assistant {
                text: "Monday.".to_owned()
            },
        ]
    );
    // Both streams together were given more audio than the microphone
    // sent: what was heard since the first question, twice.
    let given = heard.audio_ticks / 48;
    assert!((15_500..17_000).contains(&given), "{given} ms");
}

/// The session of [`spec`], with a tool that does something in the world.
fn spec_with_effect() -> SessionSpec {
    let mut spec = spec();
    spec.kernel.tools = vec![ToolSpec::new(Effect::SideEffecting)];
    spec
}

fn calls_a_tool() -> ScriptedLlm {
    ScriptedLlm::with(|asked| {
        let called = asked
            .messages
            .iter()
            .any(|said| matches!(said, Said::ToolResult { .. }));
        if called {
            vec![LlmStep::say("Done.")]
        } else {
            vec![LlmStep::call("get_time", "{}")]
        }
    })
}

#[tokio::test(start_paused = true)]
async fn a_journal_that_fails_for_a_while_loses_nothing() {
    let speech = [(1_000, 2_200, "book it")];
    let world = World::new(&speech, calls_a_tool());
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 8_000, &talks(&speech));
    let mut running = Running::start_as(spec_with_effect(), &world, transport).await;
    // The caller's turn is in the journal. What comes next is not stored
    // at the first try.
    assert_eq!(running.until(2_600).await, None);
    world
        .journal
        .fail_next_append(PortError::transient("disk busy"));
    assert_eq!(running.until(8_000).await, None);

    // The tool was not run before the record that allows it was stored,
    // and that took a second try, 50 ms after the first.
    let waited_from = running.watch.first("AwaitDurable").unwrap();
    let sent = running.watch.first("Dispatch").unwrap();
    assert!(
        (50..70).contains(&(sent - waited_from)),
        "{waited_from} {sent}"
    );
    assert_eq!(world.tools.called().len(), 1);
    assert_eq!(running.utterances(), [("Done.".to_owned(), false)]);

    // No record is missing and none is there twice.
    let records = world.journal.records(SESSION);
    assert_eq!(numbers(&records), [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(
        kinds(&records),
        [
            "participant_joined",
            "turn_committed",
            "tool_proposed",
            "tool_authorized",
            "tool_dispatched",
            "tool_finished",
            "batch_resolved",
            "utterance_committed",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_session_that_another_owner_took_stops_at_once() {
    let speech = [(1_000, 2_200, "tell me a story")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(STORY)]]);
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(3_500).await, None);
    // Another process takes the session over in the journal while the
    // agent is speaking.
    let taken = world.journal.acquire(SESSION).await.unwrap();
    assert_eq!(taken.tail.len(), 2);

    // The session learns of it with the next thing it tries to store,
    // which is how far the story has been heard, a second into it. It
    // stops there: it says nothing more and has the far end discard what
    // it holds.
    assert_eq!(
        running.until(12_000).await,
        Some(Ended::Lost(StoreBroken::Fenced))
    );
    let took = running.began.elapsed();
    assert!(took < Duration::from_millis(3_800), "{took:?}");
    let log = running.far_end.log();
    assert!(log.frames.last().unwrap().last);
    assert_eq!(log.cleared.len(), 1);
    assert_eq!(log.late, 0);
    assert!(log.sent(log.frames[0].tag) < MediaDur::from_millis(1_100));
    // Nothing of its was stored after the other took over.
    assert_eq!(world.journal.records(SESSION).len(), 2);
    // The far end is not told that the session is over, because it is
    // not: the other owner has it.
    assert_eq!(log.goodbyes, 0);
    // Nothing is waited for either: the recogniser is dropped as it is.
    assert_eq!(world.stt.log().let_go, 0);
}

#[tokio::test(start_paused = true)]
async fn a_far_end_that_does_not_answer_a_goodbye_is_not_waited_for() {
    let world = World::new(&[], ScriptedLlm::replies(vec![]));
    let transport = FakeTransport::sound_card()
        .never_lets_go()
        .join(0, CALLER)
        .microphone(CALLER, 0, 20_000, &[]);
    let mut running = Running::start(&world, transport).await;
    let (ask, inbox) = tokio::sync::mpsc::channel(8);
    running.session.listen_to(inbox);
    assert_eq!(running.until(1_000).await, None);
    ask.try_send(SessionInput::Close.into()).unwrap();

    // Everything the session decided is in its journal at once. It is
    // the goodbye that is waited for, and only for half a second.
    assert_eq!(running.until(1_400).await, None);
    assert_eq!(
        kinds(&world.journal.records(SESSION)).last().unwrap(),
        "lifecycle_changed closed"
    );
    assert_eq!(running.until(20_000).await, Some(Ended::Closed));
    let took = running.began.elapsed();
    assert!(
        (Duration::from_millis(1_490)..Duration::from_millis(1_600)).contains(&took),
        "{took:?}"
    );
    assert!(running.far_end.log().goodbyes >= 1);
}

impl World {
    /// The world a process finds that comes back to the session of this
    /// one: the same journal, model and tools, and a caller who says
    /// each of `speech`, on the clock of the session that is resumed.
    fn again(&self, speech: &[(u64, u64, &str)]) -> Self {
        let spoken = speech
            .iter()
            .map(|(from, to, text)| Spoken::new(*from, *to, text))
            .collect();
        let turns = Box::new(ScriptedTurns::always());
        Self {
            stt: ScriptedStt::new(SttScript::new(spoken)),
            llm: self.llm.clone(),
            tts: ScriptedTts::default(),
            tools: self.tools.clone(),
            journal: self.journal.clone(),
            compute: Arc::new(Compute::inline(LoudVad::default(), turns, None).unwrap()),
            sink: self.sink.clone(),
        }
    }
}

/// When the session's clock stands at the start of its next life: a
/// second after the end of the last turn in the journal, in milliseconds.
fn resumes_at(records: &[bytes::Bytes]) -> u64 {
    let heard = records
        .iter()
        .filter_map(|record| {
            let record: serde_json::Value = serde_json::from_slice(record).unwrap();
            record["fact"]["span"][1].as_u64()
        })
        .max()
        .unwrap_or(0);
    heard / 48 + 1_000
}

#[tokio::test(start_paused = true)]
async fn a_session_whose_process_died_is_taken_up_again() {
    // From all its records: no state is stored in so short a session.
    taken_up_again(StateRule::default(), None).await;
}

#[tokio::test(start_paused = true)]
async fn a_session_whose_process_died_is_taken_up_again_from_its_stored_state() {
    // A state is due after every other record, and is taken when the
    // caller's turn ends, which is record 2. What follows is only how
    // far the story was heard, and that is nothing to take a state at.
    let rule = StateRule {
        records: 2,
        after: Duration::MAX,
    };
    taken_up_again(rule, Some(2)).await;
}

/// A session dies in the middle of the agent's answer and is taken up by
/// another process. `stored` is the record its journal holds a state as
/// of when it dies, if `states` makes it store one by then.
async fn taken_up_again(states: StateRule, stored: Option<u64>) {
    let spec = || SessionSpec { states, ..spec() };
    let speech = [(1_000, 2_200, "tell me a story")];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say(STORY)],
        vec![LlmStep::say("Where was I? There was a ship.")],
        vec![LlmStep::say("You are welcome.")],
    ]);
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start_as(spec(), &world, transport).await;
    assert_eq!(running.session.rebuilt(), None);
    // The process dies in the middle of the story.
    assert_eq!(running.until(5_000).await, None);
    let before = running.far_end.log();
    let heard_before = before.heard(before.frames[0].tag).as_millis();
    let fared = running.session.stats().states;
    drop(running);
    // What the next process is given: the state, if one was stored, and
    // the records after it.
    let (kept, after) = world.journal.kept(SESSION);
    assert_eq!(kept.map(|kept| kept.upto.get()), stored);
    assert_eq!(after as u64, 4 - stored.unwrap_or(0));
    assert_eq!(fared.stored, u32::from(stored.is_some()));
    assert_eq!(fared.dropped, 0);
    // What the journal has of the story is how far it had been heard
    // when that was last written down, which is done every second.
    let records = world.journal.records(SESSION);
    assert_eq!(
        kinds(&records),
        [
            "participant_joined",
            "turn_committed",
            "utterance_progress",
            "utterance_progress",
        ]
    );

    // Another process takes the session up, with new connections to
    // everything. The caller says something four seconds later.
    let t0 = resumes_at(&records);
    let speech = [(4_000, 5_000, "thank you")];
    let on_its_clock: Vec<_> = speech
        .iter()
        .map(|(from, to, text)| (from + t0, to + t0, *text))
        .collect();
    let world = world.again(&on_its_clock);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start_as(spec(), &world, transport).await;
    // It says what it was rebuilt from.
    assert_eq!(
        running.session.rebuilt(),
        Some(Rebuilt {
            state: stored.map(Seq),
            records: 4 - stored.unwrap_or(0),
        })
    );
    assert_eq!(running.until(12_000).await, None);

    // The conversation has the story as far as it is known to have been
    // heard, and says that something was lost. The answer that was owed
    // was asked for again at once, and the next turn is answered as any
    // other.
    let conversation = running.conversation();
    assert_eq!(conversation.len(), 6, "{conversation:?}");
    assert!(matches!(conversation[2], Said::Notice { .. }));
    let utterances = running.utterances();
    assert_eq!(
        utterances,
        [
            ("Of course. Once upon a time".to_owned(), true),
            ("Where was I? There was a ship.".to_owned(), false),
            ("You are welcome.".to_owned(), false),
        ]
    );
    // It is not more than the caller heard, and not less by more than a
    // second and a word.
    let kept = u32::try_from(utterances[0].0.len()).unwrap() * 60;
    assert!(
        kept <= heard_before,
        "{kept} ms kept, {heard_before} ms heard"
    );
    assert!(
        heard_before - kept < 1_500,
        "{kept} ms kept, {heard_before} ms heard"
    );

    let asked = world.llm.asked();
    assert_eq!(asked.len(), 3);
    assert_eq!(asked[1].messages, conversation[..3]);
    assert_eq!(asked[2].last_user(), Some("thank you"));
    let log = running.far_end.log();
    let first = ms(&log.frames[0]);
    assert!(
        first < 400,
        "speaking again {first} ms after it was taken up"
    );
    assert_eq!(log.heard(log.frames[0].tag), MediaDur::from_millis(30 * 60));

    // The journal goes on where it stopped.
    let records = world.journal.records(SESSION);
    assert_eq!(numbers(&records), (1..=12).collect::<Vec<u64>>());
    assert_eq!(
        kinds(&records)[4..7],
        ["epoch_advanced", "utterance_committed", "notice_appended"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_was_running_when_the_process_died_is_not_run_twice() {
    let speech = [(1_000, 2_200, "book it")];
    let mut world = World::new(&speech, calls_a_tool());
    // The tool takes two seconds, and the process dies while it runs.
    world.tools.latency = Duration::from_secs(2);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start_as(spec_with_effect(), &world, transport).await;
    assert_eq!(running.until(3_500).await, None);
    assert_eq!(world.tools.called().len(), 1);
    assert_eq!(
        kinds(&world.journal.records(SESSION))[2..],
        ["tool_proposed", "tool_authorized", "tool_dispatched"]
    );
    drop(running);

    let world = world.again(&[]);
    let transport = FakeTransport::sound_card()
        .join(0, CALLER)
        .microphone(CALLER, 0, 12_000, &[]);
    let mut running = Running::start_as(spec_with_effect(), &world, transport).await;
    assert_eq!(running.until(12_000).await, None);

    // The journal says that the call was sent and not what came of it.
    // It is not sent again, because it may have done what it does. The
    // model is told as much, and goes on from there.
    assert_eq!(world.tools.called().len(), 1);
    let records = world.journal.records(SESSION);
    assert_eq!(
        kinds(&records)[5..],
        [
            "epoch_advanced",
            "tool_finished",
            "notice_appended",
            "batch_resolved",
            "utterance_committed",
        ]
    );
    let finished = String::from_utf8_lossy(&records[6]).into_owned();
    assert!(finished.contains("\"outcome\":\"in_doubt\""), "{finished}");
    let asked = world.llm.asked();
    assert_eq!(asked.len(), 2);
    assert!(
        matches!(&asked[1].messages[2], Said::ToolResult { text, .. } if text.contains("not known")),
        "{:?}",
        asked[1].messages
    );
    assert!(matches!(asked[1].messages[3], Said::Notice { .. }));
    assert_eq!(running.utterances(), [("Done.".to_owned(), false)]);
}

/// Everything a follower has been told and not yet taken.
/// A journal that holds `records` of the session and nothing else: what
/// a journal that never stored a state would hold of it.
async fn of_records(records: &[bytes::Bytes]) -> MemoryJournal {
    let journal = MemoryJournal::new();
    let lease = journal.acquire(SESSION).await.unwrap().lease;
    let batch = Batch {
        first: Seq(1),
        records: records.to_vec(),
    };
    journal.append(SESSION, lease, batch).await.unwrap();
    journal
}

#[tokio::test(start_paused = true)]
async fn a_session_has_its_journal_keep_its_state_as_the_conversation_goes() {
    let speech = [
        (1_000, 2_200, "what time is it"),
        (6_000, 7_000, "thank you"),
        (10_000, 11_000, "goodbye"),
    ];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say("It is a quarter past nine.")],
        vec![LlmStep::say("You are welcome.")],
        vec![LlmStep::say("Goodbye!")],
    ]);
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 16_000, &talks(&speech));
    // A state is due after every third record.
    let spec = || SessionSpec {
        states: StateRule {
            records: 3,
            after: Duration::MAX,
        },
        ..spec()
    };
    let mut running = Running::start_as(spec(), &world, transport).await;
    assert_eq!(running.until(16_000).await, None);

    // The states were taken as a turn or an answer entered the
    // conversation, and the newest is as of such a record.
    let records = world.journal.records(SESSION);
    let kinds = kinds(&records);
    let fared = running.session.stats().states;
    assert!(fared.stored >= 2, "{fared:?} {kinds:?}");
    assert_eq!(fared.dropped, 0);
    let (kept, after) = world.journal.kept(SESSION);
    let upto = usize::try_from(kept.unwrap().upto.get()).unwrap();
    assert!(
        ["turn_committed", "utterance_committed"].contains(&kinds[upto - 1].as_str()),
        "a state as of record {upto} of {kinds:?}"
    );
    assert_eq!(upto + after, records.len());
    // No more records wait for a state than make one due, and those that
    // do are after the last time one could be taken.
    assert!(after < 3, "{after} records after the state, of {kinds:?}");

    // Rebuilt from the state and what follows it, the session holds what
    // it holds now, and what it would hold rebuilt from every record.
    let held = running.session.snapshot();
    drop(running);
    let transport = || {
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 1_000, &[])
    };
    let from_state = Running::start_as(spec(), &world.again(&[]), transport()).await;
    assert_eq!(from_state.session.snapshot(), held);
    let mut plain = world.again(&[]);
    plain.journal = of_records(&records).await;
    let from_records = Running::start_as(spec(), &plain, transport()).await;
    assert_eq!(from_records.session.snapshot(), held);
}

#[tokio::test(start_paused = true)]
async fn a_state_is_due_after_a_time_as_well() {
    let speech = [
        (1_000, 2_200, "what time is it"),
        (70_000, 71_000, "thank you"),
    ];
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say("It is a quarter past nine.")],
        vec![LlmStep::say("You are welcome.")],
    ]);
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 80_000, &talks(&speech));
    // As a session is told nothing else: after 256 records or a minute.
    let mut running = Running::start(&world, transport).await;
    assert_eq!(running.until(60_000).await, None);
    assert_eq!(world.journal.kept(SESSION).0, None);
    assert_eq!(running.session.stats().states, StateStats::default());

    // The first turn to end after the minute is the time for one.
    assert_eq!(running.until(80_000).await, None);
    let kinds = kinds(&world.journal.records(SESSION));
    let (state, _) = world.journal.kept(SESSION);
    let upto = usize::try_from(state.unwrap().upto.get()).unwrap();
    assert_eq!(kinds[upto - 1], "turn_committed", "{kinds:?}");
    assert_eq!(
        kinds[..upto]
            .iter()
            .filter(|kind| *kind == "turn_committed")
            .count(),
        2
    );
    assert_eq!(running.session.stats().states.stored, 1);
}

#[tokio::test(start_paused = true)]
async fn a_session_that_never_settles_stores_its_state_all_the_same() {
    // The caller says one thing, and the agent tells a story that is
    // heard for a long time: a record a second, and nothing enters the
    // conversation.
    let speech = [(1_000, 2_200, "tell me a story")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(&STORY.repeat(4))]]);
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 30_000, &talks(&speech));
    let spec = SessionSpec {
        states: StateRule {
            records: 4,
            after: Duration::MAX,
        },
        ..spec()
    };
    let mut running = Running::start_as(spec, &world, transport).await;
    assert_eq!(running.until(20_000).await, None);
    let kinds = kinds(&world.journal.records(SESSION));
    assert!(kinds.len() >= 12, "{kinds:?}");
    // Not at the fourth record, at which nothing had settled, but at
    // the eighth, when twice as many waited.
    let (state, _) = world.journal.kept(SESSION);
    let upto = state.unwrap().upto.get();
    assert_eq!(upto % 8, 0, "a state as of record {upto} of {kinds:?}");
    assert!(running.session.stats().states.stored >= 1);
}

#[tokio::test(start_paused = true)]
async fn a_session_that_hears_and_speaks_is_not_started_without_audio() {
    let world = World::new(&[], ScriptedLlm::replies(vec![]));
    let refused = |parts: Parts| async {
        Session::open(spec(), parts, DefaultPolicies::default())
            .await
            .map(|_| ())
            .unwrap_err()
    };

    // No transport at all.
    let (transport, _far_end) = FakeTransport::sound_card().build();
    let mut parts = world.parts(transport);
    parts.transport = None;
    let error = refused(parts).await;
    assert_eq!(error, StartError::Plan(PlanError::NoAudio));
    assert!(error.to_string().contains("carries audio"), "{error}");

    // A transport for text alone.
    let written = TransportCaps {
        audio: None,
        text: true,
    };
    let (transport, _far_end) = FakeTransport::with(written).build();
    let error = refused(world.parts(transport)).await;
    assert_eq!(error, StartError::Plan(PlanError::NoAudio));

    // Nothing of it reached the journal.
    assert_eq!(world.journal.records(SESSION), Vec::<bytes::Bytes>::new());
}

#[tokio::test(start_paused = true)]
async fn a_state_that_cannot_be_read_is_not_guessed_at() {
    let open = async |journal: MemoryJournal| {
        let mut world = World::new(&[], ScriptedLlm::replies(vec![]));
        world.journal = journal;
        let (transport, _far_end) = FakeTransport::sound_card().build();
        Session::open(spec(), world.parts(transport), DefaultPolicies::default())
            .await
            .map(|_| ())
    };
    let joined = bytes::Bytes::from_static(
        br#"{"v":1,"seq":1,"fact":{"kind":"participant_joined","who":1}}"#,
    );
    let with_state = async |state: &'static [u8]| {
        let journal = of_records(core::slice::from_ref(&joined)).await;
        let lease = journal.acquire(SESSION).await.unwrap().lease;
        let state = Snapshot {
            upto: Seq(1),
            state: bytes::Bytes::from_static(state),
        };
        journal.snapshot(SESSION, lease, state).await.unwrap();
        journal
    };

    let refused = open(with_state(b"not a state").await).await.unwrap_err();
    assert!(matches!(refused, StartError::State(_)), "{refused}");
    assert!(
        refused.to_string().contains("could not be read"),
        "{refused}"
    );

    // A state from a build that is not written yet.
    let refused = open(with_state(br#"{"v":9,"upto":1}"#).await)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("version 9"), "{refused}");

    // A state that is as of another record than the journal says: one
    // of the two is wrong, and nothing says which.
    let moved = br#"{"v":1,"upto":4,"heard_until":0,"epoch":0,"lifecycle":"running","front":0,"turn_floor":1,"gen_floor":1,"next_call":1,"next_batch":1,"participants":[1],"calls":[],"batches":[],"items":[],"answered":0}"#;
    let refused = open(with_state(moved).await).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        "the journal holds a state as of record 1, which says it is as of record 4"
    );
}

#[tokio::test(start_paused = true)]
async fn a_journal_is_asked_to_hurry_only_while_something_waits_for_it() {
    let speech = [(1_000, 2_200, "book it")];
    let transport = || {
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 8_000, &talks(&speech))
    };
    // A tool that changes nothing waits for nothing, and nor does
    // anything else in a conversation.
    let world = World::new(&speech, calls_a_tool());
    let mut running = Running::start(&world, transport()).await;
    assert_eq!(running.until(8_000).await, None);
    assert_eq!(world.tools.called().len(), 1);
    assert_eq!(world.journal.hurried(SESSION), 0);

    // A tool that may change the world waits for the record that allows
    // it, and the journal is told so once.
    let mut world = World::new(&speech, calls_a_tool());
    world.journal.latency = Duration::from_millis(30);
    let mut running = Running::start_as(spec_with_effect(), &world, transport()).await;
    assert_eq!(running.until(8_000).await, None);
    assert_eq!(world.tools.called().len(), 1);
    assert_eq!(world.journal.hurried(SESSION), 1);
}

fn told(following: &mut tokio::sync::mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
    core::iter::from_fn(|| following.try_recv().ok()).collect()
}

/// The numbers of the facts among what a follower was told.
fn facts(events: &[SessionEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Fact { seq, .. } => Some(seq.get()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_follower_is_told_what_happens_and_can_come_back_for_what_it_missed() {
    let speech = [(1_000, 2_200, "what time is it please")];
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say("It is late.")]]);
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 8_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    let (ask, inbox) = tokio::sync::mpsc::channel(8);
    running.session.listen_to(inbox);

    // One follows from the start and keeps up. One has room for two
    // things only.
    let (to, mut first) = tokio::sync::mpsc::channel(64);
    ask.try_send(Request::Follow { from: Seq(1), to }).unwrap();
    let (to, mut slow) = tokio::sync::mpsc::channel(2);
    ask.try_send(Request::Follow { from: Seq(1), to }).unwrap();
    assert_eq!(running.until(8_000).await, None);

    // The first was told every fact, in order and as the journal has it,
    // and the news in between.
    let records = world.journal.records(SESSION);
    let heard = told(&mut first);
    let recorded: Vec<_> = heard
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Fact { record, .. } => Some(record.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(recorded, records);
    assert_eq!(facts(&heard), [1, 2, 3]);
    let news: Vec<_> = heard
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Told(ClientEvent::Agent { activity, .. }) => Some(*activity),
            _ => None,
        })
        .collect();
    assert_eq!(
        news,
        [Activity::Thinking, Activity::Speaking, Activity::Listening]
    );
    assert!(matches!(
        heard[1],
        SessionEvent::Told(ClientEvent::UserSpeaking { speaking: true, .. })
    ));

    // The slow one was told two things and then let go, without holding
    // the session up.
    let heard = told(&mut slow);
    assert_eq!(heard.len(), 2);
    assert_eq!(facts(&heard), [1]);
    assert!(slow.is_closed());

    // It comes back for the rest: the facts after the last it saw.
    let (to, mut back) = tokio::sync::mpsc::channel(64);
    ask.try_send(Request::Follow { from: Seq(2), to }).unwrap();
    assert_eq!(running.until(8_100).await, None);
    assert_eq!(facts(&told(&mut back)), [2, 3]);
    assert!(!back.is_closed());
}

#[tokio::test(start_paused = true)]
async fn a_follower_is_told_the_words_as_they_are_heard_and_as_they_are_said() {
    let speech = [(1_000, 2_200, "what time is it please")];
    let answer = "It is a quarter past nine. Is there anything else?";
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(answer)]]);
    let world = World::new(&speech, llm);
    let transport =
        FakeTransport::sound_card()
            .join(0, CALLER)
            .microphone(CALLER, 0, 12_000, &talks(&speech));
    let mut running = Running::start(&world, transport).await;
    let (to, mut follower) = tokio::sync::mpsc::channel(256);
    running.session.follow(Seq(1), to);
    assert_eq!(running.until(12_000).await, None);
    let heard = told(&mut follower);

    // What the caller was saying, as far as the recogniser had it, came
    // before the turn that it became.
    let committed = heard
        .iter()
        .position(|event| matches!(event, SessionEvent::Fact { seq, .. } if seq.get() == 2))
        .unwrap();
    let so_far: Vec<(usize, &str)> = heard
        .iter()
        .enumerate()
        .filter_map(|(at, event)| match event {
            SessionEvent::Hearing { who, text } => {
                assert_eq!(*who, CALLER);
                Some((at, text.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_ne!(so_far, []);
    for (at, text) in &so_far {
        assert!(*at < committed, "{text:?} came after the turn");
        assert!("what time is it please".starts_with(text), "{text:?}");
    }

    // What the agent said was told as it went to be synthesised, a
    // sentence at a time, and is all of what it said.
    let said: Vec<&str> = heard
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Said { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(said.len(), 2, "{said:?}");
    assert_eq!(said.concat(), answer);
    assert_eq!(running.utterances(), [(answer.to_owned(), false)]);
}

#[tokio::test(start_paused = true)]
async fn a_follower_that_asks_for_more_than_is_held_is_told_where_it_begins() {
    let world = World::new(&[], ScriptedLlm::replies(vec![]));
    let transport = FakeTransport::sound_card()
        .join(0, CALLER)
        .microphone(CALLER, 0, 100, &[]);
    let mut running = Running::start(&world, transport).await;
    let (ask, inbox) = tokio::sync::mpsc::channel(8);
    running.session.listen_to(inbox);
    // The agent says 140 things, each of which is a fact.
    for n in 0..140 {
        let text = "Hi.".to_owned();
        ask.try_send(SessionInput::Say { text }.into()).unwrap();
        assert_eq!(running.until(400 * (n + 1)).await, None);
    }

    let (to, mut late) = tokio::sync::mpsc::channel(256);
    ask.try_send(Request::Follow { from: Seq(1), to }).unwrap();
    assert_eq!(running.until(60_000).await, None);
    let heard = told(&mut late);
    let last = u64::try_from(world.journal.records(SESSION).len()).unwrap();
    assert!(last > 128, "{last}");
    // The last 128 are held.
    let resumes = Seq(last - 127);
    assert_eq!(heard[0], SessionEvent::Gap { resumes });
    assert_eq!(facts(&heard), (last - 127..=last).collect::<Vec<u64>>());
}
