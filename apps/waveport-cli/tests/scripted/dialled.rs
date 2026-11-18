//! A caller on a WebRTC call to an agent that a daemon runs.
//!
//! The daemon runs in the test, with the providers every other call
//! here has. The caller is the command, in a process of its own, with
//! the script of a caller at the agent's own machine: it asks the
//! daemon for a session, offers its call, and is on it.
//!
//! A daemon that runs in a test has the environment of whoever runs
//! the tests, and no test can take a variable out of its own process.
//! So this one is told that its keys are in a variable nobody sets
//! ([`NO_KEY`]): left to itself it would read the keys that are there
//! and send them to the stand-ins.

use core::time::Duration;
use std::path::Path;
use std::time::Instant;

use waveport_adapter_kit::http::{Auth, Body, HttpClient, HttpRequest, HttpSettings};
use waveport_adapter_kit::wav;
use waveportd::{Config, Daemon, Running, Threads};

use super::{Providers, assets, everything, folder, one_question, waveport};

/// An environment variable nobody sets: the stand-ins are asked with no
/// key, whatever keys the environment holds.
const NO_KEY: &str = "WAVEPORT_STAND_IN_KEY";

/// A daemon whose agent `desk` hears and speaks with `providers`, whose
/// agent `clerk` only writes, and which answers calls over WebRTC, at
/// ports of this machine that were free.
async fn daemon(providers: &Providers) -> Running {
    assert!(
        std::env::var_os(NO_KEY).is_none(),
        "{NO_KEY} is set, and is to hold nothing"
    );
    let text = format!(
        "listen = \"127.0.0.1:0\"\n\
         assets = '{}'\n\
         [webrtc]\n\
         udp = \"127.0.0.1:0\"\n\
         tcp = \"127.0.0.1:0\"\n\
         [agents.clerk.llm]\n\
         url = \"{llm}\"\n\
         model = \"a-model\"\n\
         key_env = \"{NO_KEY}\"\n\
         [agents.desk.llm]\n\
         url = \"{llm}\"\n\
         model = \"a-model\"\n\
         key_env = \"{NO_KEY}\"\n\
         [agents.desk.speech.stt]\n\
         provider = \"openai\"\n\
         url = \"{}\"\n\
         key_env = \"{NO_KEY}\"\n\
         [agents.desk.speech.tts]\n\
         provider = \"cartesia\"\n\
         url = \"{}\"\n\
         key_env = \"{NO_KEY}\"\n",
        assets().display(),
        providers.stt.url("/v1/realtime"),
        providers.tts.url("/tts/websocket"),
        llm = providers.llm.url("/v1"),
    );
    let config = Config::from_toml(&text).unwrap();
    // Every provider of every agent is told so, and none is left to
    // the variable it would read by itself.
    assert_eq!(text.matches("key_env").count(), 4);
    assert_eq!(text.matches("url = ").count(), 4);
    let threads = Threads::place(&config).unwrap();
    Daemon::start(&config, &threads)
        .await
        .unwrap_or_else(|failure| panic!("the daemon did not start: {failure}"))
}

/// Checks that `told` tells a call in which the caller asked the time,
/// as a caller who could be anybody is told it: what was said, which
/// tools were used and not what they were given or answered, and how
/// the call ended.
fn tells_the_call(told: &str, all: &str) {
    let lines: Vec<&str> = told.lines().collect();
    let at = |line: &str| {
        lines
            .iter()
            .position(|told| *told == line)
            .unwrap_or_else(|| panic!("no line is {line:?}\n{all}"))
    };
    let begun = at("the call has begun");
    let asked = at("caller: What time is it right now?");
    let looked_up = at("tool: get_time");
    let answered = at("agent: It is ten past three. Goodbye!");
    let hung_up = at("tool: end_call");
    let ended = at("the call ended: it was asked to");
    assert!(
        begun < asked
            && asked < looked_up
            && looked_up < answered
            && answered < hung_up
            && hung_up < ended,
        "{all}"
    );
}

/// Checks that the caller was played the answer, which reached it as
/// Opus: 1.2 s of the tone, give or take what a codec takes to begin
/// and to end a sound, and not before the question had been asked.
fn holds_the_answer(recording: &Path, all: &str) {
    let played = wav::read(&std::fs::read(recording).unwrap()).unwrap();
    assert_eq!(played.hz, 48_000);
    let loud = played.samples.iter().filter(|s| s.abs() > 1_000).count();
    assert!(
        (48_000..=60_000).contains(&loud),
        "{loud} loud samples\n{all}"
    );
    let first = played.samples.iter().position(|s| s.abs() > 1_000).unwrap();
    assert!(
        first > 96_000,
        "the agent spoke after {} ms\n{all}",
        first / 48
    );
}

/// Waits until the daemon at `url` has no session, which is to be
/// soon: three seconds, and not the time a session waits for a caller
/// who may come back.
async fn no_session_is_left(url: &str, all: &str) {
    let http = HttpClient::new(HttpSettings {
        direct: true,
        ..HttpSettings::default()
    })
    .unwrap();
    let sessions = format!("{url}/v1/sessions");
    let began = Instant::now();
    loop {
        let asked = HttpRequest {
            url: &sessions,
            headers: &[],
            auth: Auth::None,
            body: Body::None,
        };
        let listed = http
            .get(asked)
            .await
            .unwrap()
            .body(64 * 1024)
            .await
            .unwrap();
        let listed: serde_json::Value = serde_json::from_slice(&listed).unwrap();
        if listed["sessions"].as_array().is_some_and(Vec::is_empty) {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(3), "{listed}\n{all}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The call in which a caller asks the time, made by the command over
/// WebRTC, with datagrams or over a connection.
async fn a_caller_asks_the_time_of_a_daemon(name: &str, more: &[&str]) {
    let folder = folder(name);
    let script = one_question(&folder);
    let recording = folder.join("agent.wav");
    let providers = Providers::start().await;
    let daemon = daemon(&providers).await;
    let url = format!("http://{}", daemon.address());

    let mut arguments = vec![
        "client",
        "--webrtc",
        "--url",
        &url,
        "--agent",
        "desk",
        "--script",
        script.to_str().unwrap(),
        "--record",
        recording.to_str().unwrap(),
    ];
    arguments.extend_from_slice(more);
    let output = waveport(&arguments).await;
    let told = String::from_utf8_lossy(&output.stdout).into_owned();
    let all = everything(&output, &providers.stt, &providers.llm, &providers.tts);
    assert!(output.status.success(), "{all}");

    tells_the_call(&told, &all);
    // Neither side thought the other had gone.
    let errors = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !errors.contains("gone") && !errors.contains("dropped"),
        "{all}"
    );
    providers.were_asked_as_one_question_asks(&all);
    holds_the_answer(&recording, &all);
    // The session ended the call, and is over itself a moment after
    // its caller was told so. The daemon is then told to stop with
    // nothing left to wait for.
    no_session_is_left(&url, &all).await;
    let left = daemon.drain().await;
    assert_eq!(left, [] as [String; 0], "{all}");
}

#[tokio::test]
async fn a_call_over_webrtc_goes_as_one_at_the_desk_does() {
    a_caller_asks_the_time_of_a_daemon("asks-the-time-over-webrtc", &[]).await;
}

#[tokio::test]
async fn a_call_over_webrtc_is_made_over_a_connection_when_told_to() {
    a_caller_asks_the_time_of_a_daemon("asks-the-time-over-a-connection", &["--tcp"]).await;
}

#[tokio::test]
async fn a_caller_who_hangs_up_is_not_waited_for() {
    let folder = folder("hangs-up-on-a-daemon");
    // A caller who says nothing and leaves: a script that runs out is
    // a caller who hangs up.
    let script = folder.join("leaves.call");
    std::fs::write(&script, "silence 600\n").unwrap();
    let providers = Providers::start().await;
    let daemon = daemon(&providers).await;
    let url = format!("http://{}", daemon.address());

    let arguments = [
        "client",
        "--webrtc",
        "--url",
        &url,
        "--agent",
        "desk",
        "--script",
        script.to_str().unwrap(),
    ];
    let output = waveport(&arguments).await;
    let told = String::from_utf8_lossy(&output.stdout).into_owned();
    let all = everything(&output, &providers.stt, &providers.llm, &providers.tts);
    assert!(output.status.success(), "{all}");
    assert!(told.contains("the call has begun"), "{all}");

    // The caller said that they are gone, on the call and where they
    // offered it. A call that only fell silent would be waited for, for
    // as long as a caller may take to come back: this one's session is
    // over by the time anybody looks.
    no_session_is_left(&url, &all).await;
    let left = daemon.drain().await;
    assert_eq!(left, [] as [String; 0], "{all}");
}

#[tokio::test]
async fn a_caller_whom_a_daemon_turns_away_is_told_why() {
    let folder = folder("turned-away-by-a-daemon");
    let script = one_question(&folder);
    let providers = Providers::start().await;
    let daemon = daemon(&providers).await;
    let url = format!("http://{}", daemon.address());
    let called = |agent: &'static str, url: String| {
        let script = script.to_str().unwrap().to_owned();
        async move {
            let arguments = [
                "client", "--webrtc", "--url", &url, "--agent", agent, "--script", &script,
            ];
            let output = waveport(&arguments).await;
            let told = String::from_utf8_lossy(&output.stdout).into_owned();
            let errors = String::from_utf8_lossy(&output.stderr).into_owned();
            // No call began, and the command says that it failed.
            assert!(!output.status.success(), "{told}{errors}");
            assert!(!told.contains("the call has begun"), "{told}{errors}");
            errors
        }
    };

    // An agent the daemon has none of: no session is made.
    let errors = called("nobody", url.clone()).await;
    assert!(
        errors
            .contains("made no session of the agent nobody: the daemon has no agent of that name"),
        "{errors}"
    );
    // One that only writes: its session is made, and is not called.
    let errors = called("clerk", url.clone()).await;
    assert!(
        errors.contains("no agent took the call at http://127.0.0.1:"),
        "{errors}"
    );
    assert!(errors.contains("neither hears nor speaks"), "{errors}");
    // What a daemon says of what it was sent is told as a reason, and
    // not as it was sent.
    assert!(
        !errors.contains("about:blank") && !errors.contains('{'),
        "{errors}"
    );

    // Where no daemon is, and where none can be.
    let nowhere = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let errors = called("desk", nowhere).await;
    assert!(
        errors.contains("made no session of the agent desk"),
        "{errors}"
    );
    let errors = called("desk", format!("{url}/v1/webrtc")).await;
    assert!(errors.contains("is not where a daemon is"), "{errors}");
    let errors = called("desk", "ws://127.0.0.1:9283".to_owned()).await;
    assert!(errors.contains("is not where a daemon is"), "{errors}");

    // None of them left the daemon anything to wait for but the session
    // of the agent that only writes, which nobody came to.
    let asked = providers.llm.requests();
    assert_eq!(asked.len(), 0, "{asked:?}");
    drop(daemon);
}
