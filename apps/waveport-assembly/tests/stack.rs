//! An agent on a stack, begun over lines of every kind.
//!
//! The providers of these agents are at addresses where nobody listens:
//! nothing here waits for an answer. What is checked is what is put
//! together, and how a session is run for the line it is given. The
//! models that listen are the real ones, from where
//! `cargo xtask fetch-assets` puts them.

#![expect(
    clippy::unwrap_used,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;
use std::path::{Path, PathBuf};

use waveport_assembly::spec::{AgentSpec, Keeps, Named, StackSpec};
use waveport_assembly::stack::{Settled, Stack, name};
use waveport_ports::control::SessionInput;
use waveport_ports::plan::{SessionPlan, Shortfall};
use waveport_ports::transport::TransportCaps;
use waveport_runtime::session::Ended;
use waveport_runtime::shard::Exit;
use waveport_testkit::fakes::FakeTransport;

/// What a command line calls the settings.
const SAID: Named = Named {
    journal: "--journal",
    memory: "--no-journal",
    assets: "--assets",
    otlp: "--otlp",
    by: "the command line",
};

/// Where the models that listen are.
fn assets() -> PathBuf {
    std::env::var_os("WAVEPORT_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/assets"),
        PathBuf::from,
    )
}

/// A stack that keeps its sessions in memory.
async fn stack(assets: PathBuf) -> Stack {
    let spec = StackSpec {
        keeps: Keeps::Memory,
        state_every: 256,
        notes: None,
        otlp: None,
        assets,
        named: SAID,
    };
    Stack::build(&spec, false).await.unwrap()
}

/// An agent that hears and speaks, with every provider at an address
/// where nobody listens. Such an address needs no key.
const SPEAKS: &str = r#"
instructions = "Say little."

[llm]
url = "http://127.0.0.1:9/v1"

[speech.stt]
url = "ws://127.0.0.1:9/v1/realtime"

[speech.tts]
url = "ws://127.0.0.1:9/tts"
"#;

/// The same agent without a voice.
const WRITES: &str = r#"
[llm]
url = "http://127.0.0.1:9/v1"
"#;

/// A line that carries text and no audio.
const NO_AUDIO: TransportCaps = TransportCaps {
    audio: None,
    text: true,
    keys: None,
};

#[tokio::test]
async fn a_session_is_run_as_its_line_carries() {
    let stack = stack(assets()).await;
    assert!(stack.notes.is_empty(), "{:?}", stack.notes);
    assert_eq!(stack.tools(), ["get_time", "end_call"]);
    let speaks = stack
        .agent(&AgentSpec::from_toml(SPEAKS).unwrap())
        .await
        .unwrap();
    assert!(speaks.speaks());
    // Nobody is where the synthesiser should be, which is said now and
    // stops nothing.
    assert_eq!(speaks.notes.len(), 1, "{:?}", speaks.notes);
    assert!(
        speaks.notes[0].starts_with("speech synthesis could not be reached yet"),
        "{:?}",
        speaks.notes
    );
    let writes = stack
        .agent(&AgentSpec::from_toml(WRITES).unwrap())
        .await
        .unwrap();
    assert!(!writes.speaks() && writes.notes.is_empty());

    // The agent that hears and speaks does, over a line with audio.
    let (line, _far_end) = FakeTransport::sound_card().build();
    let (aloud, told) = stack
        .start(
            &speaks,
            name(),
            Some(Box::new(line)),
            Vec::new(),
            false,
            false,
        )
        .await
        .unwrap();
    assert!(
        matches!(aloud.plan(), SessionPlan::Cascaded(_)),
        "{:?}",
        aloud.plan()
    );
    assert!(told.is_none() && !aloud.greeted && aloud.rebuilt.is_none());

    // The same agent reads and writes over a line that carries no
    // audio, and over no line at all, and so does an agent that has no
    // voice. Whoever asked to follow is given what it tells.
    let (line, _far_end) = FakeTransport::with(NO_AUDIO).build();
    let (in_text, _) = stack
        .start(
            &speaks,
            name(),
            Some(Box::new(line)),
            Vec::new(),
            false,
            false,
        )
        .await
        .unwrap();
    assert_eq!(in_text.plan().shortfalls(), []);
    let (written, told) = stack
        .start(&speaks, name(), None, Vec::new(), false, true)
        .await
        .unwrap();
    let (plain, _) = stack
        .start(&writes, name(), None, Vec::new(), false, false)
        .await
        .unwrap();
    for call in [&in_text, &written, &plain] {
        assert!(
            matches!(call.plan(), SessionPlan::Text(_)),
            "{:?}",
            call.plan()
        );
    }
    assert!(told.is_some());

    // An agent with no voice on a line with audio reads and writes too,
    // and its plan says what it does without.
    let (line, _far_end) = FakeTransport::sound_card().build();
    let (deaf, _) = stack
        .start(
            &writes,
            name(),
            Some(Box::new(line)),
            Vec::new(),
            false,
            false,
        )
        .await
        .unwrap();
    assert!(matches!(deaf.plan(), SessionPlan::Text(_)));
    assert_eq!(deaf.plan().shortfalls(), [Shortfall::AudioUnheard]);
    assert_eq!(deaf.notes.len(), 1);

    // Each is a session of the one runtime: asked to end, it ends, and
    // what was kept of it is let go of.
    for call in [aloud, in_text, written, plain, deaf] {
        let id = call.id;
        assert!(stack.ask(id, SessionInput::Close));
        let exit = tokio::time::timeout(Duration::from_secs(30), call.exit())
            .await
            .unwrap();
        assert_eq!(exit, Exit::Ended(Ended::Closed));
        assert_eq!(stack.settle(id, &exit).await, Settled::Forgotten(None));
        assert!(!stack.ask(id, SessionInput::Close), "it is gone");
    }
    assert_eq!(stack.finish().await, None);
}

#[tokio::test]
async fn an_agent_is_greeted_once_and_told_what_its_file_says() {
    let folder = std::env::temp_dir().join(format!("waveport-assembly-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("told.txt");
    std::fs::write(&file, "Answer in one word.").unwrap();
    let stack = stack(assets()).await;
    let spec = AgentSpec {
        instructions_file: Some(file.clone()),
        greeting: Some("Hello.".to_owned()),
        ..AgentSpec::from_toml(WRITES).unwrap()
    };
    let agent = stack.agent(&spec).await.unwrap();
    let (call, _) = stack
        .start(&agent, name(), None, Vec::new(), false, false)
        .await
        .unwrap();
    assert!(call.greeted);
    assert!(stack.ask(call.id, SessionInput::Close));
    let exit = tokio::time::timeout(Duration::from_secs(30), call.exit())
        .await
        .unwrap();
    assert_eq!(exit, Exit::Ended(Ended::Closed));

    // A file that is not there is said, with its name.
    let missing = AgentSpec {
        instructions_file: Some(folder.join("nowhere.txt")),
        ..AgentSpec::default()
    };
    let Err(failure) = stack.agent(&missing).await.map(|_| ()) else {
        panic!("there is no such file");
    };
    assert!(failure.0.contains("nowhere.txt"), "{failure}");
}

#[tokio::test]
async fn what_cannot_be_had_is_said_as_whoever_gave_the_settings_calls_it() {
    // A provider's own address wants a key, and the variable that is
    // named holds none.
    let stack = stack(assets()).await;
    let keyless = AgentSpec::from_toml("[llm]\nkey_env = \"WAVEPORT_TEST_NO_SUCH_KEY\"\n");
    let Err(failure) = stack.agent(&keyless.unwrap()).await.map(|_| ()) else {
        panic!("there is no key");
    };
    assert!(
        failure.0.contains("WAVEPORT_TEST_NO_SUCH_KEY holds no key"),
        "{failure}"
    );

    // Models that are nowhere are missed by the first agent that hears,
    // and by none that only writes.
    let nowhere = std::env::temp_dir().join("waveport-assembly-no-assets");
    let stack = self::stack(nowhere).await;
    let writes = AgentSpec::from_toml(WRITES).unwrap();
    assert!(stack.agent(&writes).await.is_ok());
    let speaks = AgentSpec::from_toml(SPEAKS).unwrap();
    let Err(failure) = stack.agent(&speaks).await.map(|_| ()) else {
        panic!("there are no models");
    };
    assert!(
        failure.0.contains("cargo xtask fetch-assets")
            && failure.0.contains("say where they are with --assets"),
        "{failure}"
    );

    // A collector at an address that is none.
    let spec = StackSpec {
        keeps: Keeps::Memory,
        state_every: 256,
        notes: None,
        otlp: Some("not an address".to_owned()),
        assets: assets(),
        named: SAID,
    };
    let Err(failure) = Stack::build(&spec, false).await.map(|_| ()) else {
        panic!("it is not an address");
    };
    assert!(failure.0.contains("as --otlp says"), "{failure}");
}
