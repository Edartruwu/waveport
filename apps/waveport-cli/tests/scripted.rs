//! A whole call, from the command line to the last word.
//!
//! The command itself is run, as anyone would run it, with a caller who
//! follows a script: once with the caller at the agent's own machine,
//! and once with the agent taking calls over a WebSocket and the caller
//! in a process of their own. Everything in it is what a real call uses: the
//! session runtime, the models that listen, the three provider adapters,
//! the audio path and the two tools. Only the providers are not real.
//! They are servers on this machine that speak as the providers do:
//!
//! - Speech recognition answers as OpenAI was recorded answering this
//!   very question, on 15 November 2026.
//! - The language model's answers are made up, in the shape of the ones
//!   that were recorded for the adapter's own tests.
//! - Speech synthesis answers in the shapes Cartesia's documentation
//!   gives, with a tone for a voice.
//!
//! The models that listen have to have been fetched:
//! `cargo xtask fetch-assets`.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::fmt::Write as _;
use core::time::Duration;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use waveport_adapter_kit::pcm::write_base64;
use waveport_adapter_kit::wav;
use waveport_ports::audio::Rate;
use waveport_testkit::servers::{HttpAnswer, HttpServer, WsServer, WsStep};
use waveport_testkit::speech;

/// What the recogniser said when it was sent this question.
const HEARD: &str = include_str!("../../../crates/adapters/stt/openai/tests/fixtures/question.ws");

/// What the agent says once it knows the time.
const ANSWER: [&str; 3] = ["It is", " ten past three.", " Goodbye!"];

/// One piece of an answer of the language model, as it comes off the
/// wire.
fn piece(delta: &str, finish: &str) -> String {
    format!(
        "data: {{\"id\":\"chatcmpl-made-up\",\"object\":\"chat.completion.chunk\",\
         \"created\":1794769429,\"model\":\"a-model\",\"choices\":[{{\"index\":0,\
         \"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
    )
}

/// The piece that asks for the tool `name`, with no arguments.
fn asks_for(name: &str) -> String {
    let call = format!(
        "{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_{name}\",\"type\":\"function\",\
         \"function\":{{\"name\":\"{name}\",\"arguments\":\"{{}}\"}}}}]}}"
    );
    piece(&call, "null")
}

/// An answer of the language model: these words, and then a call of this
/// tool.
fn answer(words: &[&str], tool: Option<&str>) -> HttpAnswer {
    let mut stream = piece("{\"role\":\"assistant\",\"content\":\"\"}", "null");
    for word in words {
        stream.push_str(&piece(&format!("{{\"content\":\"{word}\"}}"), "null"));
    }
    match tool {
        Some(tool) => {
            stream.push_str(&asks_for(tool));
            stream.push_str(&piece("{}", "\"tool_calls\""));
        }
        None => stream.push_str(&piece("{}", "\"stop\"")),
    }
    stream.push_str("data: [DONE]\n\n");
    HttpAnswer::recorded(&stream)
}

/// What the synthesiser says for one response: 1.2 s of a tone at
/// 24 kHz, in three pieces, with where the words are. As the provider
/// does, it speaks each piece of text when it is given it, and does not
/// wait for the rest.
fn spoken() -> Vec<WsStep> {
    let tone: Vec<i16> = (0..Rate::Hz24000.samples_in_millis(400))
        .map(|n| if n % 48 < 24 { 3_000 } else { -3_000 })
        .collect();
    let mut data = String::new();
    write_base64(&tone, &mut data);
    let chunk = format!(
        "< {{\"type\":\"chunk\",\"data\":\"{data}\",\"done\":false,\"status_code\":206,\
         \"step_time\":12,\"context_id\":\"{{{{context_id}}}}\"}}"
    );
    let words = |words: &str, starts: &str, ends: &str| {
        format!(
            "< {{\"type\":\"timestamps\",\"done\":false,\"status_code\":206,\
             \"context_id\":\"{{{{context_id}}}}\",\"word_timestamps\":{{\"words\":[{words}],\
             \"start\":[{starts}],\"end\":[{ends}]}}}}"
        )
    };
    let first = words(
        "\"It\",\"is\",\"ten\",\"past\",\"three.\"",
        "0.0,0.1,0.2,0.4,0.6",
        "0.1,0.2,0.4,0.6,0.8",
    );
    let second = words("\"Goodbye!\"", "0.8", "1.2");
    let done = "< {\"type\":\"done\",\"done\":true,\"status_code\":206,\
                \"context_id\":\"{{context_id}}\"}";
    let script = [
        // The first sentence.
        "> \"transcript\":\"It is",
        &chunk,
        &first,
        &chunk,
        // The second, which is given once the first has its audio.
        "> Goodbye!",
        &chunk,
        &second,
        // And once the adapter has said that no more text follows.
        "> \"continue\":false",
        done,
    ]
    .join("\n");
    WsStep::script(&script)
}

/// A folder for what this test writes, empty.
fn folder(name: &str) -> PathBuf {
    let folder = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).unwrap();
    folder
}

/// Where the models that listen are.
fn assets() -> PathBuf {
    std::env::var_os("WAVEPORT_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/assets"),
        PathBuf::from,
    )
}

/// Starts the command.
fn started(arguments: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_waveport"))
        .args(arguments)
        // No key of anyone's is needed for servers on this machine, and
        // none is to reach them.
        .env_remove("OPENAI_API_KEY")
        .env_remove("CARTESIA_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the command, started")
}

/// Waits for the command to end, for a minute and a half at most, while
/// the servers of this test go on answering it.
async fn ended(child: &mut Child) {
    let began = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if began.elapsed() > Duration::from_secs(90) {
            child.kill().unwrap();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Runs the command to its end.
async fn waveport(arguments: &[&str]) -> Output {
    let mut child = started(arguments);
    ended(&mut child).await;
    child.wait_with_output().unwrap()
}

/// The command taking calls, and what it has said so far.
struct Serving {
    child: Child,
    told: Arc<Mutex<Vec<String>>>,
    errors: Arc<Mutex<Vec<String>>>,
}

/// Keeps every line that comes out of `pipe`, as it comes.
fn lines_of(pipe: impl std::io::Read + Send + 'static) -> Arc<Mutex<Vec<String>>> {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&kept);
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            keeping.lock().unwrap().push(line);
        }
    });
    kept
}

impl Serving {
    /// Starts the command, which is to take calls, and waits until it
    /// says where. Returns it and the address.
    async fn start(arguments: &[&str]) -> (Self, String) {
        let mut child = started(arguments);
        let told = lines_of(child.stdout.take().unwrap());
        let errors = lines_of(child.stderr.take().unwrap());
        let mut serving = Self {
            child,
            told,
            errors,
        };
        let began = Instant::now();
        loop {
            let said = serving.told.lock().unwrap().first().cloned();
            if let Some(line) = said {
                let address = line
                    .strip_prefix("taking calls at ")
                    .unwrap_or_else(|| panic!("the first line was {line:?}"))
                    .to_owned();
                return (serving, address);
            }
            let gone = serving.child.try_wait().unwrap().is_some();
            assert!(
                !gone && began.elapsed() < Duration::from_secs(60),
                "the command takes no calls\n{}",
                serving.all()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Everything the command has said.
    fn all(&self) -> String {
        format!(
            "--- the server told ---\n{}\n--- the server's errors ---\n{}\n",
            self.told.lock().unwrap().join("\n"),
            self.errors.lock().unwrap().join("\n")
        )
    }

    /// Waits for the command to end by itself, and returns how it did.
    async fn ended(&mut self) -> Option<i32> {
        ended(&mut self.child).await;
        let status = self.child.wait().unwrap();
        // What it said last is still on its way through the pipes.
        tokio::time::sleep(Duration::from_millis(100)).await;
        status.code()
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        // A test that failed leaves nothing running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes the script of a caller with one question, who then waits to be
/// told, and the question itself, into `folder`. Returns where the
/// script is.
fn one_question(folder: &Path) -> PathBuf {
    let question = speech::what_time_is_it(Rate::Hz16000);
    std::fs::write(folder.join("question.wav"), wav::write(16_000, &question)).unwrap();
    let script = folder.join("time.call");
    std::fs::write(
        &script,
        "# A caller with one question, who then waits to be told.\n\
         silence 400\n\
         say question.wav\n\
         listen 1500 40000\n",
    )
    .unwrap();
    script
}

/// Everything there is to go by when a call goes wrong on a machine
/// nobody sits at: what the command said, and what each provider was
/// sent, without the audio.
fn everything(output: &Output, stt: &WsServer, llm: &HttpServer, tts: &WsServer) -> String {
    let short = |text: &str| text.chars().take(600).collect::<String>();
    let mut all = format!(
        "--- told ---\n{}--- errors ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for (name, server) in [("recogniser", stt), ("synthesiser", tts)] {
        for (n, connection) in server.connections().iter().enumerate() {
            let _ = writeln!(all, "--- the {name}, connection {n}: {}", connection.path);
            for text in connection.texts() {
                if !text.contains("input_audio_buffer.append") {
                    let _ = writeln!(all, "{}", short(text));
                }
            }
        }
    }
    for (n, request) in llm.requests().iter().enumerate() {
        let _ = writeln!(all, "--- the model, request {n}\n{}", short(&request.body));
    }
    all
}

/// The three providers of a call in which the caller asks the time,
/// each a server on this machine.
struct Providers {
    stt: WsServer,
    llm: HttpServer,
    tts: WsServer,
}

impl Providers {
    async fn start() -> Self {
        Self {
            stt: WsServer::start(vec![WsStep::script(HEARD)]).await,
            llm: HttpServer::start(vec![
                answer(&[], Some("get_time")),
                answer(&ANSWER, Some("end_call")),
                answer(&[], None),
            ])
            .await,
            tts: WsServer::start(vec![spoken()]).await,
        }
    }

    /// What tells the command where its providers and its models are.
    fn arguments(&self) -> Vec<String> {
        vec![
            "--assets".to_owned(),
            assets().to_str().unwrap().to_owned(),
            "--stt-url".to_owned(),
            self.stt.url("/v1/realtime"),
            "--llm-url".to_owned(),
            self.llm.url("/v1"),
            "--tts-url".to_owned(),
            self.tts.url("/tts/websocket"),
        ]
    }

    /// Checks what each provider was asked. `all` is what to show if it
    /// was not what a call with one question asks.
    fn were_asked_as_one_question_asks(&self, all: &str) {
        // The recogniser was set up as a transcriber, sent audio, and
        // asked once for what it had heard.
        let heard = self.stt.connections();
        assert_eq!(heard.len(), 1, "{all}");
        assert!(
            heard[0]
                .path
                .starts_with("/v1/realtime?intent=transcription")
        );
        assert_eq!(heard[0].header("authorization"), None);
        let sent: Vec<&str> = heard[0].texts().collect();
        assert!(sent[0].contains("\"languages\":[\"en\"]"), "{}", sent[0]);
        let count = |what: &str| sent.iter().filter(|text| text.contains(what)).count();
        assert!(count("input_audio_buffer.append") > 20);
        assert_eq!(count("input_audio_buffer.commit"), 1, "{all}");

        // The model was asked twice: with the question and the two
        // tools, and then with the time as well.
        let asked = self.llm.requests();
        assert_eq!(asked.len(), 2, "{all}");
        assert_eq!(asked[0].line, "POST /v1/chat/completions HTTP/1.1");
        assert!(asked[0].body.contains("What time is it right now?"));
        assert!(asked[0].body.contains("\"get_time\"") && asked[0].body.contains("\"end_call\""));
        assert!(!asked[0].body.contains("time_zone"));
        assert!(asked[1].body.contains("time_zone"), "{}", asked[1].body);

        // The synthesiser was given the answer, whole and once.
        let spoke = self.tts.connections();
        let given: String = spoke
            .iter()
            .flat_map(waveport_testkit::servers::WsSeen::texts)
            .filter_map(|text| {
                let (_, rest) = text.split_once("\"transcript\":\"")?;
                Some(rest.split_once('"')?.0.to_owned())
            })
            .collect();
        assert_eq!(given, ANSWER.concat(), "{all}");
    }
}

/// Checks that `told` tells a call in which the caller asked the time:
/// what the caller said, the time looked up, the answer, and then the
/// agent ending the call itself, after it had said goodbye. Every line
/// of the call begins with `before`.
fn tells_the_call(told: &str, before: &str, all: &str) {
    let lines: Vec<&str> = told.lines().collect();
    let at = |start: &str| {
        let start = format!("{before}{start}");
        lines
            .iter()
            .position(|line| line.starts_with(&start))
            .unwrap_or_else(|| panic!("no line begins with {start:?}\n{all}"))
    };
    let asked = at("caller: What time is it right now?");
    let looked_up = at("tool: get_time({}) -> {");
    let answered = at("agent: It is ten past three. Goodbye!");
    let hung_up = at("tool: end_call({}) -> The call is ending.");
    let ended = at("the call ended: it was asked to");
    assert!(
        asked < looked_up && looked_up < answered && answered < hung_up && hung_up < ended,
        "{all}"
    );
    assert!(!lines[answered].contains("[cut off]"), "{all}");
}

/// Checks that the caller was played the answer: 1.2 s of the tone, and
/// nothing else.
fn holds_the_answer(recording: &Path, all: &str) {
    let played = wav::read(&std::fs::read(recording).unwrap()).unwrap();
    assert_eq!(played.hz, 48_000);
    let loud = played.samples.iter().filter(|s| s.abs() > 1_000).count();
    assert!(
        (52_000..=58_000).contains(&loud),
        "{loud} loud samples\n{all}"
    );
    let first = played.samples.iter().position(|s| s.abs() > 1_000).unwrap();
    // Not before the question had been asked: it takes two seconds.
    assert!(
        first > 96_000,
        "the agent spoke after {} ms\n{all}",
        first / 48
    );
}

#[tokio::test]
async fn a_caller_asks_the_time_is_told_it_and_the_agent_hangs_up() {
    let folder = folder("asks-the-time");
    let script = one_question(&folder);
    let recording = folder.join("agent.wav");
    let providers = Providers::start().await;

    let mut arguments = vec![
        "run".to_owned(),
        "--script".to_owned(),
        script.to_str().unwrap().to_owned(),
        "--record".to_owned(),
        recording.to_str().unwrap().to_owned(),
    ];
    arguments.extend(providers.arguments());
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = waveport(&arguments).await;
    let told = String::from_utf8_lossy(&output.stdout).into_owned();
    let all = everything(&output, &providers.stt, &providers.llm, &providers.tts);
    assert!(output.status.success(), "{all}");

    tells_the_call(&told, "", &all);
    providers.were_asked_as_one_question_asks(&all);
    holds_the_answer(&recording, &all);
}

#[tokio::test]
async fn a_call_over_a_websocket_goes_as_one_at_the_desk_does() {
    let folder = folder("asks-the-time-from-elsewhere");
    let script = one_question(&folder);
    let recording = folder.join("agent.wav");
    let providers = Providers::start().await;

    // The agent, taking calls on a port of this machine that was free.
    let mut arguments = vec![
        "serve".to_owned(),
        "--listen".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--calls".to_owned(),
        "1".to_owned(),
    ];
    arguments.extend(providers.arguments());
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let (mut serving, address) = Serving::start(&arguments).await;
    assert!(address.starts_with("ws://127.0.0.1:"), "{address}");

    // The caller, in a process of their own, with the same script as a
    // caller at the agent's own machine.
    let output = waveport(&[
        "client",
        "--url",
        &address,
        "--script",
        script.to_str().unwrap(),
        "--record",
        recording.to_str().unwrap(),
    ])
    .await;
    // The agent was told to take one call, and stops when it is over.
    let code = serving.ended().await;
    let told = String::from_utf8_lossy(&output.stdout).into_owned();
    let all = format!(
        "{}{}",
        serving.all(),
        everything(&output, &providers.stt, &providers.llm, &providers.tts)
    );
    assert!(output.status.success(), "{all}");
    assert_eq!(code, Some(0), "{all}");

    // The caller's side tells the call as the agent's side does, to the
    // last line: it was told how the call ended before the goodbye.
    tells_the_call(&told, "", &all);
    let served = serving.told.lock().unwrap().join("\n");
    tells_the_call(&served, "[1] ", &all);
    assert!(
        served.contains("[1] the call has begun, from 127.0.0.1:"),
        "{all}"
    );
    // Neither side thought the other had gone.
    let errors = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!errors.contains("gone"), "{all}");
    assert!(!served.contains("the caller left"), "{all}");

    providers.were_asked_as_one_question_asks(&all);
    holds_the_answer(&recording, &all);
}

#[tokio::test]
async fn a_caller_who_finds_no_agent_is_told_so() {
    // A port nothing listens on: one is taken, and let go.
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("ws://{}", free.local_addr().unwrap());
    drop(free);
    let folder = folder("no-agent");
    let script = folder.join("short.call");
    std::fs::write(&script, "silence 100\n").unwrap();
    let output = waveport(&[
        "client",
        "--url",
        &address,
        "--script",
        script.to_str().unwrap(),
    ])
    .await;
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{errors}");
    assert!(
        errors.contains("no agent took the call at ws://127.0.0.1:"),
        "{errors}"
    );

    // An agent that may be reached from elsewhere has to be told so.
    let output = waveport(&[
        "serve",
        "--listen",
        "0.0.0.0:0",
        "--stt-url",
        "ws://127.0.0.1:9/v1/realtime",
        "--llm-url",
        "http://127.0.0.1:9/v1",
        "--tts-url",
        "ws://127.0.0.1:9/tts",
        "--assets",
        assets().to_str().unwrap(),
    ])
    .await;
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{errors}");
    assert!(errors.contains("--exposed"), "{errors}");
}

#[tokio::test]
async fn a_call_that_cannot_begin_says_why() {
    let folder = folder("cannot-begin");
    let script = folder.join("empty.call");
    std::fs::write(&script, "silence 100\n").unwrap();

    // The providers themselves want keys, and there are none.
    let output = waveport(&["run", "--script", script.to_str().unwrap()]).await;
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{errors}");
    assert!(errors.contains("OPENAI_API_KEY holds no key"), "{errors}");

    // Models that are nowhere.
    let nowhere = folder.join("no-assets");
    let output = waveport(&[
        "run",
        "--script",
        script.to_str().unwrap(),
        "--assets",
        nowhere.to_str().unwrap(),
        "--stt-url",
        "ws://127.0.0.1:9/v1/realtime",
        "--llm-url",
        "http://127.0.0.1:9/v1",
        "--tts-url",
        "ws://127.0.0.1:9/tts",
    ])
    .await;
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{errors}");
    assert!(errors.contains("cargo xtask fetch-assets"), "{errors}");

    // A script with a step nobody knows.
    std::fs::write(&script, "silence 100\ndance 3\n").unwrap();
    let output = waveport(&["run", "--script", script.to_str().unwrap()]).await;
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{errors}");
    assert!(
        errors.contains("line 2") && errors.contains("\"dance\""),
        "{errors}"
    );
}
