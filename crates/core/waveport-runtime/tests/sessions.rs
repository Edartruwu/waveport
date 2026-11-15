//! Whole conversations, run by the session actor with a fake in every
//! port.
//!
//! The kernel, the audio path, the compute plane and the actor are the
//! real ones. The parts they reach the world through follow scripts, on a
//! clock the test controls, so every run is the same.

#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;
use std::sync::{Arc, Mutex};

use tokio::time::Instant;
use waveport_domain::command::{Activity, ClientEvent, Command};
use waveport_domain::config::{AgentSpec, Effect, KernelConfig, ToolSpec};
use waveport_domain::event::Event;
use waveport_domain::fact::Lifecycle;
use waveport_domain::ids::{ParticipantId, Seq, ToolId};
use waveport_domain::kernel::{ItemView, View};
use waveport_domain::out::Out;
use waveport_domain::policy::DefaultPolicies;
use waveport_domain::time::{MediaDur, MonoTime};
use waveport_ports::audio::{Codec, Law, PcmBytes, Rate};
use waveport_ports::error::PortError;
use waveport_ports::journal::{Journal, SessionId};
use waveport_ports::plan::{Marks, Shortfall};
use waveport_ports::stt::{SttFinalize, SttTiming};
use waveport_ports::tool::ToolReply;
use waveport_ports::transport::{RemoteBuffer, TransportCaps};
use waveport_ports::tts::{Alignment, TtsCaps, TtsInput};
use waveport_runtime::compute::Compute;
use waveport_runtime::context::ToolDef;
use waveport_runtime::session::{
    Ended, Parts, Request, Session, SessionEvent, SessionInput, SessionSpec, StoreBroken, Tap,
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
        }
    }

    fn parts(&self, transport: FakeTransport) -> Parts {
        Parts {
            transport: Box::new(transport),
            stt: Arc::new(self.stt.clone()),
            llm: Arc::new(self.llm.clone()),
            tts: Arc::new(self.tts.clone()),
            tools: Arc::new(self.tools.clone()),
            journal: Arc::new(self.journal.clone()),
            compute: Arc::clone(&self.compute),
            sink: None,
        }
    }
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
        let mut session = Session::open(spec, world.parts(transport), DefaultPolicies::default())
            .await
            .unwrap();
        let watch = Watch::default();
        session.tap(Box::new(watch.clone()));
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
    assert_eq!(running.session.plan().shortfalls, []);
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
    let plan = running.session.plan();
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

    let records = world.journal.records(SESSION);
    assert_eq!(
        kinds(&records)[4..],
        [
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
    let mut running = Running::start(&world, transport).await;
    // The process dies in the middle of the story.
    assert_eq!(running.until(5_000).await, None);
    let before = running.far_end.log();
    let heard_before = before.heard(before.frames[0].tag).as_millis();
    drop(running);
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
    let mut running = Running::start(&world, transport).await;
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
