//! A whole call, from the command line to the last word.
//!
//! The command itself is run, as anyone would run it, with a caller who
//! follows a script. Everything in it is what a real call uses: the
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
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::fmt::Write as _;
use core::time::Duration;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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

/// Runs the command to its end, while the servers of this test go on
/// answering it.
async fn waveport(arguments: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_waveport"))
        .args(arguments)
        // No key of anyone's is needed for servers on this machine, and
        // none is to reach them.
        .env_remove("OPENAI_API_KEY")
        .env_remove("CARTESIA_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the command, started");
    let began = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if began.elapsed() > Duration::from_secs(90) {
            child.kill().unwrap();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    child.wait_with_output().unwrap()
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

#[tokio::test]
async fn a_caller_asks_the_time_is_told_it_and_the_agent_hangs_up() {
    let folder = folder("asks-the-time");
    let script = one_question(&folder);
    let recording = folder.join("agent.wav");

    let stt = WsServer::start(vec![WsStep::script(HEARD)]).await;
    let llm = HttpServer::start(vec![
        answer(&[], Some("get_time")),
        answer(&ANSWER, Some("end_call")),
        answer(&[], None),
    ])
    .await;
    let tts = WsServer::start(vec![spoken()]).await;

    let (stt_url, llm_url, tts_url) = (
        stt.url("/v1/realtime"),
        llm.url("/v1"),
        tts.url("/tts/websocket"),
    );
    let assets = assets();
    let output = waveport(&[
        "run",
        "--script",
        script.to_str().unwrap(),
        "--record",
        recording.to_str().unwrap(),
        "--assets",
        assets.to_str().unwrap(),
        "--stt-url",
        &stt_url,
        "--llm-url",
        &llm_url,
        "--tts-url",
        &tts_url,
    ])
    .await;
    let told = String::from_utf8_lossy(&output.stdout).into_owned();
    let all = everything(&output, &stt, &llm, &tts);
    assert!(output.status.success(), "{all}");

    // The call, as it was told: what the caller said, the time looked up,
    // the answer, and then the agent ending the call itself, after it had
    // said goodbye.
    let lines: Vec<&str> = told.lines().collect();
    let at = |start: &str| {
        lines
            .iter()
            .position(|line| line.starts_with(start))
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

    // The recogniser was set up as a transcriber, sent audio, and asked
    // once for what it had heard.
    let heard = stt.connections();
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

    // The model was asked twice: with the question and the two tools, and
    // then with the time as well.
    let asked = llm.requests();
    assert_eq!(asked.len(), 2, "{all}");
    assert_eq!(asked[0].line, "POST /v1/chat/completions HTTP/1.1");
    assert!(asked[0].body.contains("What time is it right now?"));
    assert!(asked[0].body.contains("\"get_time\"") && asked[0].body.contains("\"end_call\""));
    assert!(!asked[0].body.contains("time_zone"));
    assert!(asked[1].body.contains("time_zone"), "{}", asked[1].body);

    // The synthesiser was given the answer, whole and once.
    let spoke = tts.connections();
    let given: String = spoke
        .iter()
        .flat_map(waveport_testkit::servers::WsSeen::texts)
        .filter_map(|text| {
            let (_, rest) = text.split_once("\"transcript\":\"")?;
            Some(rest.split_once('"')?.0.to_owned())
        })
        .collect();
    assert_eq!(given, ANSWER.concat(), "{all}");

    // And the caller was played it: 1.2 s of the tone, and nothing else.
    let played = wav::read(&std::fs::read(&recording).unwrap()).unwrap();
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
