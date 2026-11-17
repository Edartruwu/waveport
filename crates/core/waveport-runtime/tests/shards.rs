//! Sessions on shards: started, driven from outside, followed, and ended
//! one at a time.
//!
//! These run on the machine's clock, on the threads a runtime starts, so
//! they are kept short and say nothing of how long anything took.

#![expect(
    clippy::unwrap_used,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::future::poll_fn;
use core::pin::Pin;
use core::time::Duration;
use std::sync::Arc;

use waveport_domain::command::ClientEvent;
use waveport_domain::config::{AgentSpec, Effect, KernelConfig, ToolSpec};
use waveport_domain::fact::Lifecycle;
use waveport_domain::ids::{ParticipantId, Seq, ToolId};
use waveport_domain::policy::DefaultPolicies;
use waveport_ports::control::{ControlError, EventRx, SessionControl, SessionEvent, SessionInput};
use waveport_ports::journal::SessionId;
use waveport_ports::tool::ToolReply;
use waveport_runtime::compute::Compute;
use waveport_runtime::context::ToolDef;
use waveport_runtime::session::{Brain, Ended, Parts, SessionSpec};
use waveport_runtime::shard::{Exit, Runtime, RuntimeConfig, SpawnError};
use waveport_testkit::fakes::{
    FakeTransport, LlmStep, LoudVad, MemoryJournal, ScriptedLlm, ScriptedStt, ScriptedTools,
    ScriptedTts, ScriptedTurns, SttScript,
};

const CALLER: ParticipantId = ParticipantId(1);

/// A session with one agent that has one tool, and the model that calls
/// it for whatever it is asked.
fn spec(id: u128) -> SessionSpec {
    SessionSpec {
        kernel: KernelConfig {
            agents: vec![AgentSpec {
                tools: vec![ToolId(0)],
            }],
            tools: vec![ToolSpec::new(Effect::Pure)],
            ..KernelConfig::default()
        },
        tools: vec![ToolDef {
            name: "look_up".to_owned(),
            description: "Looks something up.".to_owned(),
            parameters: "{}".to_owned(),
        }],
        ..SessionSpec::new(SessionId(id), "You are a helpful agent on a call.")
    }
}

/// The parts of a session whose caller is there and says nothing.
fn parts(compute: &Arc<Compute>, journal: &MemoryJournal, tools: ScriptedTools) -> Parts {
    let (transport, _) = FakeTransport::sound_card().join(0, CALLER).build();
    let llm = ScriptedLlm::with(|_| vec![LlmStep::call("look_up", "{}")]);
    Parts {
        brain: Brain::Cascaded {
            stt: Arc::new(ScriptedStt::new(SttScript::new(vec![]))),
            llm: Arc::new(llm),
            tts: Arc::new(ScriptedTts::default()),
            compute: Arc::clone(compute),
        },
        transport: Some(Box::new(transport)),
        tools: Arc::new(tools),
        journal: Arc::new(journal.clone()),
        sink: None,
        taps: Vec::new(),
    }
}

fn compute() -> Arc<Compute> {
    let turns = Box::new(ScriptedTurns::always());
    Arc::new(Compute::inline(LoudVad::default(), turns, None).unwrap())
}

async fn next(following: &mut Pin<Box<dyn EventRx>>) -> Option<SessionEvent> {
    let told = poll_fn(|cx| following.as_mut().poll_next(cx));
    tokio::time::timeout(Duration::from_secs(20), told)
        .await
        .unwrap()
}

async fn exit(started: waveport_runtime::shard::Started) -> Exit {
    tokio::time::timeout(Duration::from_secs(20), started.exit())
        .await
        .unwrap()
}

#[tokio::test]
async fn sessions_are_started_driven_followed_and_closed() {
    let runtime = Runtime::new(RuntimeConfig {
        shards: 2,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let (compute, journal) = (compute(), MemoryJournal::new());
    let tools = || ScriptedTools::none();
    let (a, b) = (SessionId(0xA), SessionId(0xB));
    let first = runtime
        .start(
            spec(a.0),
            parts(&compute, &journal, tools()),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();
    let second = runtime
        .start(
            spec(b.0),
            parts(&compute, &journal, tools()),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();
    assert_eq!((first.id, second.id), (a, b));
    assert_eq!(runtime.sessions(), 2);
    // One on each shard.
    let stats = runtime.stats();
    assert_eq!((stats[0].sessions, stats[1].sessions), (1, 1));

    // A name is one session's at a time, and nothing can be asked of a
    // session that is not there.
    let again = runtime
        .start(
            spec(a.0),
            parts(&compute, &journal, tools()),
            DefaultPolicies::default(),
        )
        .await;
    assert_eq!(again.unwrap_err(), SpawnError::Running);
    let nobody = SessionId(0xC);
    assert_eq!(
        runtime.send(nobody, SessionInput::Interrupt),
        Err(ControlError::NoSuchSession)
    );
    assert_eq!(
        runtime.subscribe(nobody, Seq(1)).err(),
        Some(ControlError::NoSuchSession)
    );

    // The first is followed from its start, told to say something and
    // to close.
    let mut following = runtime.subscribe(a, Seq(1)).unwrap();
    let text = "Hi.".to_owned();
    runtime.send(a, SessionInput::Say { text }).unwrap();
    runtime.send(a, SessionInput::Close).unwrap();
    assert_eq!(exit(first).await, Exit::Ended(Ended::Closed));

    let mut numbers = Vec::new();
    let mut closed = false;
    while let Some(event) = next(&mut following).await {
        match event {
            SessionEvent::Fact { seq, record } => {
                numbers.push(seq.get());
                assert_eq!(journal.records(a)[numbers.len() - 1], record);
            }
            SessionEvent::Told(ClientEvent::Lifecycle(Lifecycle::Closed)) => closed = true,
            SessionEvent::Told(_) | SessionEvent::Said { .. } | SessionEvent::Hearing { .. } => {}
            SessionEvent::Gap { .. } | SessionEvent::State { .. } => {
                panic!("nothing was asked for that is not held")
            }
        }
    }
    assert!(closed);
    let stored = u64::try_from(journal.records(a).len()).unwrap();
    assert_eq!(numbers, (1..=stored).collect::<Vec<u64>>());
    let said = journal
        .records(a)
        .iter()
        .any(|record| String::from_utf8_lossy(record).contains("\"text\":\"Hi.\""));
    assert!(said);

    // The other went on meanwhile, and is closed in its turn.
    assert_eq!(runtime.sessions(), 1);
    assert_eq!(
        runtime.send(a, SessionInput::Interrupt),
        Err(ControlError::NoSuchSession)
    );
    runtime.send(b, SessionInput::Close).unwrap();
    assert_eq!(exit(second).await, Exit::Ended(Ended::Closed));
    assert_eq!(runtime.sessions(), 0);
    let stats = runtime.stats();
    assert!(
        stats
            .iter()
            .all(|shard| shard.sessions == 0 && shard.polls > 0)
    );
}

#[tokio::test]
async fn a_session_that_panics_takes_nothing_else_with_it() {
    // One shard, so that both sessions are on the same thread.
    let runtime = Runtime::new(RuntimeConfig::default()).unwrap();
    let (compute, journal) = (compute(), MemoryJournal::new());
    let (doomed, other) = (SessionId(0xD), SessionId(0xE));
    let broken = ScriptedTools::with(|_| panic!("the tool has a bug"));
    let first = runtime
        .start(
            spec(doomed.0),
            parts(&compute, &journal, broken),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();
    let sound = ScriptedTools::with(|_| ToolReply::Completed("found".to_owned()));
    let second = runtime
        .start(
            spec(other.0),
            parts(&compute, &journal, sound),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();

    // The caller of the first types something. The model calls the tool,
    // and the tool panics.
    let typed = SessionInput::Text {
        who: CALLER,
        text: "look it up".to_owned(),
    };
    runtime.send(doomed, typed).unwrap();
    assert_eq!(exit(first).await, Exit::Panicked);
    assert_eq!(runtime.sessions(), 1);
    // What it had recorded is in its journal.
    let records = journal.records(doomed);
    let last = String::from_utf8_lossy(records.last().unwrap()).into_owned();
    assert!(last.contains("tool_"), "{last}");

    // The other session, on the same thread, is as it was.
    let mut following = runtime.subscribe(other, Seq(u64::MAX)).unwrap();
    let text = "Still here.".to_owned();
    runtime.send(other, SessionInput::Say { text }).unwrap();
    runtime.send(other, SessionInput::Close).unwrap();
    assert_eq!(exit(second).await, Exit::Ended(Ended::Closed));
    let mut facts = 0;
    while let Some(event) = next(&mut following).await {
        facts += usize::from(matches!(event, SessionEvent::Fact { .. }));
    }
    assert!(facts >= 2, "{facts}");
}

#[tokio::test]
async fn a_runtime_that_is_drained_starts_nothing_and_waits_for_what_it_has() {
    let runtime = Arc::new(Runtime::new(RuntimeConfig::default()).unwrap());
    let (compute, journal) = (compute(), MemoryJournal::new());
    let tools = ScriptedTools::none;
    let (a, b) = (SessionId(0xA), SessionId(0xB));
    let first = runtime
        .start(
            spec(0xA),
            parts(&compute, &journal, tools()),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();
    let second = runtime
        .start(
            spec(0xB),
            parts(&compute, &journal, tools()),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();

    // Each says where it stands: its caller has joined, which is its
    // first fact, and nothing is under way.
    let rests = |id| runtime.status(id).is_some_and(|status| status.at_rest);
    for _ in 0..400 {
        if runtime.at_rest() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(runtime.at_rest());
    assert!(rests(a) && rests(b));
    assert_eq!(runtime.status(a).unwrap().next_seq, Seq(2));
    assert_eq!(runtime.status(SessionId(0xC)), None);
    // One is asked something. From then on it has an answer under way,
    // and the other still rests.
    let typed = SessionInput::Text {
        who: CALLER,
        text: "What is on the list?".to_owned(),
    };
    runtime.send(b, typed).unwrap();
    for _ in 0..400 {
        if !rests(b) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!rests(b) && rests(a));
    assert!(!runtime.at_rest());
    assert!(runtime.status(b).unwrap().next_seq > Seq(2));

    // Nothing ends by itself: when the patience is over, both still run.
    assert_eq!(runtime.drain(Duration::from_millis(50)).await, 2);
    // And nothing is started any more.
    let refused = runtime
        .start(
            spec(0xC),
            parts(&compute, &journal, tools()),
            DefaultPolicies::default(),
        )
        .await;
    assert!(matches!(refused, Err(SpawnError::Stopped)), "{refused:?}");
    assert_eq!(runtime.sessions(), 2);

    // Whoever drains says what becomes of the sessions. These are asked
    // to end while they are waited for, one after the other.
    let waiting = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.drain(Duration::from_secs(20)).await })
    };
    runtime.send(a, SessionInput::Close).unwrap();
    assert_eq!(exit(first).await, Exit::Ended(Ended::Closed));
    assert!(!waiting.is_finished(), "one session still runs");
    runtime.send(b, SessionInput::Close).unwrap();
    assert_eq!(exit(second).await, Exit::Ended(Ended::Closed));
    assert_eq!(waiting.await.unwrap(), 0);
    // A runtime nothing runs on is drained at once.
    assert_eq!(runtime.drain(Duration::from_secs(20)).await, 0);
}

#[tokio::test]
async fn a_session_says_which_of_its_lives_it_is_in_and_is_followed_from_what_it_is() {
    let (compute, journal) = (compute(), MemoryJournal::new());
    let id = SessionId(0xE);
    let runtime = Runtime::new(RuntimeConfig::default()).unwrap();
    let first = runtime
        .start(
            spec(0xE),
            parts(&compute, &journal, ScriptedTools::none()),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();
    assert!(first.rebuilt.is_none());
    let text = "One moment.".to_owned();
    runtime.send(id, SessionInput::Say { text }).unwrap();
    let mut following = runtime.subscribe(id, Seq(1)).unwrap();
    let mut seen = 0;
    while seen < 2 {
        if let Some(SessionEvent::Fact { seq, .. }) = next(&mut following).await {
            seen = seen.max(seq.get());
        }
    }
    // The runtime stops under the session, as a process that dies does.
    let before = first.lease;
    drop(runtime);
    assert_eq!(exit(first).await, Exit::Stopped);

    // The session is gone on with, by another owner: a later life.
    let runtime = Runtime::new(RuntimeConfig::default()).unwrap();
    let second = runtime
        .start(
            spec(0xE),
            parts(&compute, &journal, ScriptedTools::none()),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();
    assert!(second.rebuilt.is_some());
    assert!(second.lease > before, "{:?} after {before:?}", second.lease);

    // Whoever was told of the earlier life follows from what the session
    // is now: its state first, as the journal holds one, and then every
    // fact after it and none before.
    let mut following = runtime.subscribe_from_state(id).unwrap();
    let Some(SessionEvent::State { upto, state }) = next(&mut following).await else {
        panic!("a state comes first");
    };
    // The state is as of the last fact the session has made, which may
    // be one its journal does not have yet.
    assert!(upto.get() >= 1, "{upto}");
    let (snapshot, _) = waveport_runtime::codec::decode_state(&state).unwrap();
    assert_eq!(snapshot.next_seq(), upto.next());
    let text = "Still here.".to_owned();
    runtime.send(id, SessionInput::Say { text }).unwrap();
    runtime.send(id, SessionInput::Close).unwrap();
    assert_eq!(exit(second).await, Exit::Ended(Ended::Closed));
    let mut numbers = Vec::new();
    while let Some(event) = next(&mut following).await {
        match event {
            SessionEvent::Fact { seq, .. } => numbers.push(seq.get()),
            SessionEvent::State { .. } | SessionEvent::Gap { .. } => panic!("{event:?}"),
            _ => {}
        }
    }
    let last = journal.records(id).len() as u64;
    assert_eq!(numbers, (upto.get() + 1..=last).collect::<Vec<u64>>());
    assert!(numbers.len() >= 2, "{numbers:?}");
}

#[tokio::test]
async fn a_session_that_runs_for_too_long_at_a_time_is_counted() {
    let runtime = Runtime::new(RuntimeConfig {
        poll_budget: Duration::from_millis(20),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let (compute, journal) = (compute(), MemoryJournal::new());
    let id = SessionId(0xF);
    // A tool that computes for 60 ms where it should have handed the
    // work to another thread.
    let slow = ScriptedTools::with(|_| {
        std::thread::sleep(Duration::from_millis(60));
        ToolReply::Completed("found".to_owned())
    });
    let started = runtime
        .start(
            spec(id.0),
            parts(&compute, &journal, slow),
            DefaultPolicies::default(),
        )
        .await
        .unwrap();
    let mut following = runtime.subscribe(id, Seq(1)).unwrap();
    let typed = SessionInput::Text {
        who: CALLER,
        text: "look it up".to_owned(),
    };
    runtime.send(id, typed).unwrap();
    // Wait for the tool to have answered.
    loop {
        let event = next(&mut following).await.unwrap();
        if let SessionEvent::Fact { record, .. } = event
            && String::from_utf8_lossy(&record).contains("tool_finished")
        {
            break;
        }
    }
    // The run in which it answered is counted when it is over, which may
    // be a moment after its answer was told.
    let mut stats = runtime.stats()[0];
    for _ in 0..1_000 {
        if stats.over_budget >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        stats = runtime.stats()[0];
    }
    assert!(stats.over_budget >= 1, "{stats:?}");
    assert!(stats.longest >= Duration::from_millis(60), "{stats:?}");

    // Dropping the runtime stops the session where it is.
    drop(runtime);
    assert_eq!(exit(started).await, Exit::Stopped);
}
