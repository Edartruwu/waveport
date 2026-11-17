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
//!
//! `resumed` kills the command in the middle of such a call, and goes on
//! with the call in another process. `watched` has such a call traced,
//! counted and summed up. `written` chats with the agent in writing,
//! which needs the language model alone.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

mod resumed;
mod watched;
mod written;

use core::fmt::Write as _;
use core::time::Duration;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use waveport_adapter_kit::pcm::write_base64;
use waveport_adapter_kit::wav;
use waveport_adapter_kit::ws::{WsConn, WsMessage};
use waveport_ports::audio::Rate;
use waveport_protocol::client::v1::{
    self as protocol, AudioFormat, ByeReason, Codec, Cut, ErrorCode, Fact, FromClient, FromServer,
    Lifecycle, Resume,
};
use waveport_testkit::servers::{HttpAnswer, HttpServer, WsHeard, WsSeen, WsServer, WsStep};
use waveport_testkit::speech;
use waveport_transport_websocket::{Client, Heard, Hello, PATH, PROTOCOL, Unconnected};

/// What the recogniser said when it was sent this question.
const HEARD: &str =
    include_str!("../../../../crates/adapters/stt/openai/tests/fixtures/question.ws");

/// What a stand-in for Deepgram says in a call with one question:
/// nothing until it is asked what it has heard, and then the question
/// with where its words were. Made up, in the shapes of what the
/// provider says.
const HEARD_BY_DEEPGRAM: &str = "> \"type\":\"Finalize\"\n\
    < {\"type\":\"Results\",\"channel_index\":[0,1],\"duration\":2.5,\"start\":0.0,\
    \"is_final\":true,\"speech_final\":true,\"from_finalize\":true,\"channel\":\
    {\"alternatives\":[{\"transcript\":\"What time is it right now?\",\"confidence\":0.99,\
    \"words\":[{\"word\":\"what\",\"start\":0.6,\"end\":0.8,\"confidence\":0.99},\
    {\"word\":\"now\",\"start\":1.7,\"end\":2.0,\"confidence\":0.99}]}]}}\n\
    > \"type\":\"CloseStream\"\n\
    < {\"type\":\"Metadata\",\"request_id\":\"made-up\",\"duration\":9.0,\"channels\":1}\n\
    ! 1000 ";

/// The same of a stand-in for AssemblyAI, which begins a session, ends
/// the turn when it is asked to, and says when the session is over.
const HEARD_BY_ASSEMBLYAI: &str = "< {\"type\":\"Begin\",\"id\":\"made-up\",\
    \"expires_at\":1772570132,\"configuration\":{\"model\":\"universal-3-6-pro\"}}\n\
    > \"type\":\"ForceEndpoint\"\n\
    < {\"turn_order\":0,\"turn_is_formatted\":true,\"end_of_turn\":true,\
    \"transcript\":\"What time is it right now?\",\"end_of_turn_confidence\":1,\
    \"words\":[{\"start\":600,\"end\":800,\"text\":\"What\",\"confidence\":0.99,\
    \"word_is_final\":true},{\"start\":1700,\"end\":2000,\"text\":\"now?\",\
    \"confidence\":0.99,\"word_is_final\":true}],\"utterance\":\"\",\"type\":\"Turn\"}\n\
    > \"type\":\"Terminate\"\n\
    < {\"type\":\"Termination\",\"audio_duration_seconds\":9,\
    \"session_duration_seconds\":9}\n\
    ! 1000 ";

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

/// The same answer as a stand-in for ElevenLabs gives it: the same
/// 1.2 s of a tone in three pieces, each with the characters spoken in
/// it, timed as the provider times them: on a clock that runs on
/// through the speech, and with the space between two pieces at the end
/// of the first.
fn spoken_by_characters() -> Vec<WsStep> {
    let tone: Vec<i16> = (0..Rate::Hz24000.samples_in_millis(400))
        .map(|n| if n % 48 < 24 { 3_000 } else { -3_000 })
        .collect();
    let mut data = String::new();
    write_base64(&tone, &mut data);
    let piece = |text: &str, from: u32| {
        let count = u32::try_from(text.chars().count()).unwrap();
        let each = 400 / count;
        let chars: Vec<String> = text.chars().map(|c| format!("\"{c}\"")).collect();
        let starts: Vec<String> = (0..count).map(|n| (from + n * each).to_string()).collect();
        let lengths = vec![each.to_string(); chars.len()];
        format!(
            "< {{\"audio\":\"{data}\",\"contextId\":\"{{{{context_id}}}}\",\"alignment\":\
             {{\"chars\":[{}],\"charStartTimesMs\":[{}],\"charDurationsMs\":[{}]}},\
             \"normalizedAlignment\":null}}",
            chars.join(","),
            starts.join(","),
            lengths.join(",")
        )
    };
    let script = [
        // The first sentence.
        "> \"text\":\"It is".to_owned(),
        piece("It is ten ", 0),
        piece("past three. ", 400),
        // The second, which is given once the first has its audio.
        "> Goodbye!".to_owned(),
        piece("Goodbye! ", 800),
        // And once the adapter has said that no more text follows.
        "> \"close_context\":true".to_owned(),
        "< {\"isFinal\":true,\"contextId\":\"{{context_id}}\"}".to_owned(),
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

/// A folder for the command to be run in, which nothing else is run in:
/// a command that is told nothing else keeps its calls where it is run.
fn desk() -> PathBuf {
    static DESKS: AtomicU32 = AtomicU32::new(0);
    let number = DESKS.fetch_add(1, Ordering::Relaxed);
    folder(&format!("desks/{}-{number}", std::process::id()))
}

/// Starts the command in the folder `desk`. `halt` is where it is told
/// to stop, if anywhere.
fn started_at(desk: &Path, arguments: &[&str], halt: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_waveport"));
    command
        .args(arguments)
        .current_dir(desk)
        // No key of anyone's is needed for servers on this machine, and
        // none is to reach them.
        .env_remove("OPENAI_API_KEY")
        .env_remove("CARTESIA_API_KEY")
        .env_remove("WAVEPORT_TEST_HALT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(place) = halt {
        command.env("WAVEPORT_TEST_HALT", place);
    }
    command.spawn().expect("the command, started")
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
    waveport_at(&desk(), arguments).await
}

/// Runs the command to its end, in the folder `desk`.
async fn waveport_at(desk: &Path, arguments: &[&str]) -> Output {
    let mut child = started_at(desk, arguments, None);
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
        Self::start_at(&desk(), arguments, None).await
    }

    /// As [`start`](Self::start), in the folder `desk`. `halt` is where
    /// the command is told to stop, if anywhere.
    async fn start_at(desk: &Path, arguments: &[&str], halt: Option<&str>) -> (Self, String) {
        let mut child = started_at(desk, arguments, halt);
        let told = lines_of(child.stdout.take().unwrap());
        let errors = lines_of(child.stderr.take().unwrap());
        let mut serving = Self {
            child,
            told,
            errors,
        };
        let began = Instant::now();
        loop {
            // What it has to say of the calls it keeps comes first.
            let said = serving.told.lock().unwrap().clone();
            let address = said
                .iter()
                .find_map(|line| line.strip_prefix("taking calls at "));
            if let Some(address) = address {
                return (serving, address.to_owned());
            }
            let first = said.first().map_or("", String::as_str);
            assert!(
                first.is_empty() || first.starts_with("note: ") || said.len() < 8,
                "the command says other things than where it takes calls\n{}",
                serving.all()
            );
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

    /// Waits until the command has said a line that is `line`.
    async fn says(&self, line: &str) {
        let began = Instant::now();
        while !self.told.lock().unwrap().iter().any(|said| said == line) {
            assert!(
                began.elapsed() < Duration::from_secs(60),
                "the command did not say {line:?}\n{}",
                self.all()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Kills the command as `kill -9` does: it is given no chance to
    /// finish anything.
    fn killed(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
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

/// Who recognises speech in a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hears {
    OpenAi,
    Deepgram,
    AssemblyAi,
}

/// Who synthesises it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Speaks {
    Cartesia,
    ElevenLabs,
}

/// The three providers of a call in which the caller asks the time,
/// each a server on this machine. The recogniser replays what OpenAI
/// once said, or says what the documentation of another provider has it
/// say. The model and the synthesiser are made up.
struct Providers {
    hears: Hears,
    speaks: Speaks,
    stt: WsServer,
    llm: HttpServer,
    tts: WsServer,
}

/// How many messages of audio a connection was sent.
fn pieces_of_audio(seen: &WsSeen) -> usize {
    seen.messages
        .iter()
        .filter(|message| matches!(message, WsHeard::Bytes(_)))
        .count()
}

impl Providers {
    /// The providers of the first voice loop.
    async fn start() -> Self {
        Self::of(Hears::OpenAi, Speaks::Cartesia).await
    }

    async fn of(hears: Hears, speaks: Speaks) -> Self {
        let recognised = match hears {
            Hears::OpenAi => HEARD,
            Hears::Deepgram => HEARD_BY_DEEPGRAM,
            Hears::AssemblyAi => HEARD_BY_ASSEMBLYAI,
        };
        let spoken = match speaks {
            Speaks::Cartesia => spoken(),
            Speaks::ElevenLabs => spoken_by_characters(),
        };
        Self {
            hears,
            speaks,
            stt: WsServer::start(vec![WsStep::script(recognised)]).await,
            llm: HttpServer::start(vec![
                answer(&[], Some("get_time")),
                answer(&ANSWER, Some("end_call")),
                answer(&[], None),
            ])
            .await,
            tts: WsServer::start(vec![spoken]).await,
        }
    }

    /// What tells the command who its providers are, and where they and
    /// its models are.
    fn arguments(&self) -> Vec<String> {
        let (hears, heard_at) = match self.hears {
            Hears::OpenAi => ("openai", "/v1/realtime"),
            Hears::Deepgram => ("deepgram", "/v1/listen"),
            Hears::AssemblyAi => ("assemblyai", "/v3/ws"),
        };
        let (speaks, spoken_at) = match self.speaks {
            Speaks::Cartesia => ("cartesia", "/tts/websocket"),
            Speaks::ElevenLabs => ("elevenlabs", "/v1/text-to-speech"),
        };
        vec![
            "--assets".to_owned(),
            assets().to_str().unwrap().to_owned(),
            "--stt".to_owned(),
            hears.to_owned(),
            "--stt-url".to_owned(),
            self.stt.url(heard_at),
            "--llm-url".to_owned(),
            self.llm.url("/v1"),
            "--tts".to_owned(),
            speaks.to_owned(),
            "--tts-url".to_owned(),
            self.tts.url(spoken_at),
        ]
    }

    /// Checks what each provider was asked. `all` is what to show if it
    /// was not what a call with one question asks.
    fn were_asked_as_one_question_asks(&self, all: &str) {
        // The recogniser was set up for the caller's language, sent
        // audio all the while, asked once for what it had heard, and
        // given no key: there is none.
        let heard = self.stt.connections();
        assert_eq!(heard.len(), 1, "{all}");
        let heard = &heard[0];
        assert_eq!(heard.header("authorization"), None);
        let sent: Vec<&str> = heard.texts().collect();
        let count = |what: &str| sent.iter().filter(|text| text.contains(what)).count();
        match self.hears {
            Hears::OpenAi => {
                assert!(heard.path.starts_with("/v1/realtime?intent=transcription"));
                assert!(sent[0].contains("\"languages\":[\"en\"]"), "{}", sent[0]);
                assert!(count("input_audio_buffer.append") > 20);
                assert_eq!(count("input_audio_buffer.commit"), 1, "{all}");
            }
            Hears::Deepgram => {
                assert_eq!(
                    heard.path,
                    "/v1/listen?model=nova-3&language=en&encoding=linear16&sample_rate=16000\
                     &channels=1&interim_results=true&smart_format=true"
                );
                assert!(pieces_of_audio(heard) > 20);
                assert_eq!(count("Finalize"), 1, "{all}");
                assert_eq!(sent.last(), Some(&"{\"type\":\"CloseStream\"}"), "{all}");
            }
            Hears::AssemblyAi => {
                assert_eq!(
                    heard.path,
                    "/v3/ws?sample_rate=16000&encoding=pcm_s16le&speech_model=universal-3-6-pro\
                     &language_codes=%5B%22en%22%5D"
                );
                assert!(pieces_of_audio(heard) > 20);
                assert_eq!(count("ForceEndpoint"), 1, "{all}");
                assert_eq!(sent.last(), Some(&"{\"type\":\"Terminate\"}"), "{all}");
            }
        }

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
        let texts = spoke.iter().flat_map(WsSeen::texts);
        let given: String = match self.speaks {
            Speaks::Cartesia => texts
                .filter_map(|text| {
                    let (_, rest) = text.split_once("\"transcript\":\"")?;
                    Some(rest.split_once('"')?.0.to_owned())
                })
                .collect(),
            Speaks::ElevenLabs => {
                assert!(
                    spoke[0].path.starts_with(
                        "/v1/text-to-speech/Xb7hH8MSUJpSbSDYk0k2/multi-stream-input\
                         ?model_id=eleven_flash_v2_5&output_format=pcm_24000"
                    ),
                    "{}",
                    spoke[0].path
                );
                texts
                    // Not the space a response is begun with.
                    .filter(|text| text.contains("\"flush\":true"))
                    .filter_map(|text| {
                        let (_, rest) = text.split_once("\"text\":\"")?;
                        Some(rest.split_once('"')?.0.to_owned())
                    })
                    .collect()
            }
        };
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

/// Runs the call in which a caller asks the time, with these providers,
/// and checks all of it: what the command told, what each provider was
/// asked, and what the caller was played.
async fn a_caller_asks_the_time_of(hears: Hears, speaks: Speaks, name: &str) {
    let folder = folder(name);
    let script = one_question(&folder);
    let recording = folder.join("agent.wav");
    let providers = Providers::of(hears, speaks).await;

    let mut arguments = vec![
        "run".to_owned(),
        "--script".to_owned(),
        script.to_str().unwrap().to_owned(),
        "--record".to_owned(),
        recording.to_str().unwrap().to_owned(),
    ];
    arguments.extend(providers.arguments());
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = waveport_at(&folder, &arguments).await;
    let told = String::from_utf8_lossy(&output.stdout).into_owned();
    let all = everything(&output, &providers.stt, &providers.llm, &providers.tts);
    assert!(output.status.success(), "{all}");

    tells_the_call(&told, "", &all);
    providers.were_asked_as_one_question_asks(&all);
    holds_the_answer(&recording, &all);

    // The command was told nothing about where to keep its calls, and
    // kept them where it was run, in a folder that is its owner's alone.
    // The call is over, so there is none to go on with.
    let kept = folder.join(".waveport");
    assert!(kept.join("journal.redb").is_file(), "{all}");
    assert!(!kept.join("journal.last").exists(), "{all}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |file: &Path| std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&kept), 0o700, "{all}");
        assert_eq!(mode(&kept.join("journal.redb")), 0o600, "{all}");
    }
}

#[tokio::test]
async fn a_caller_asks_the_time_is_told_it_and_the_agent_hangs_up() {
    a_caller_asks_the_time_of(Hears::OpenAi, Speaks::Cartesia, "asks-the-time").await;
}

#[tokio::test]
async fn the_call_goes_the_same_with_deepgram_and_elevenlabs() {
    a_caller_asks_the_time_of(Hears::Deepgram, Speaks::ElevenLabs, "asks-the-others").await;
}

#[tokio::test]
async fn the_call_goes_the_same_with_assemblyai() {
    a_caller_asks_the_time_of(Hears::AssemblyAi, Speaks::Cartesia, "asks-the-third").await;
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

/// What a caller that is a program, and not the command, does on a
/// call: it types its question, and says of each run of audio that it
/// has played it once all of it has arrived. Returns the facts it was
/// told, with their numbers, and how the agent's side said goodbye.
async fn stays_on(caller: &mut Client) -> (Vec<(u64, Fact)>, ByeReason) {
    let mut facts = Vec::new();
    // How many samples of the run that is arriving have arrived.
    let mut held = 0;
    let staying = async {
        loop {
            match caller.next().await.unwrap().expect("a goodbye") {
                Heard::Begins { .. } => held = 0,
                Heard::Audio(samples) => held += u32::try_from(samples.len() / 2).unwrap(),
                Heard::Ends(run) => caller.played(run, held).await.unwrap(),
                Heard::Fact { seq, fact } => facts.push((seq, fact)),
                Heard::Bye(reason) => return reason,
                _ => {}
            }
        }
    };
    let reason = tokio::time::timeout(Duration::from_secs(60), staying)
        .await
        .unwrap_or_else(|_| panic!("the call did not end: so far {facts:?}"));
    (facts, reason)
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one call, told from the line that drops to the goodbye"
)]
async fn a_caller_whose_line_drops_is_given_it_back() {
    let providers = Providers::start().await;
    let mut arguments = ["serve", "--listen", "127.0.0.1:0", "--calls", "1"]
        .map(str::to_owned)
        .to_vec();
    arguments.extend(providers.arguments());
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let (mut serving, address) = Serving::start(&arguments).await;
    let soon = Duration::from_secs(10);

    // A caller that is told what the tools do, with audio as a browser
    // would send it.
    let hello = Hello {
        features: vec!["tools".to_owned()],
        ..Hello::new(Some(AudioFormat {
            codec: Codec::Pcm16,
            rate: 24_000,
        }))
    };
    let first = Client::connect(&address, &hello, soon).await.unwrap();
    let welcome = first.welcome().clone();
    assert!(!welcome.resumed, "{welcome:?}");
    assert_eq!((welcome.life, welcome.grace_ms), (1, Some(30_000)));
    assert_eq!(welcome.features, ["tools"]);
    let token = welcome
        .resume_token
        .clone()
        .expect("a token to come back with");
    assert!(token.starts_with("wpr1_"));
    // Its line drops before it has said anything.
    first.drop_line();

    // Nobody takes its place who does not hold what it was given: not
    // with a token that was made up, and not with one of another call.
    let with = |token: &str, life, seen| Hello {
        resume: Some(Resume {
            token: token.to_owned(),
            life,
            seen,
        }),
        ..hello.clone()
    };
    // One that is no token, and one that reads as the call's own and
    // is not: only whoever keeps the call can tell the second.
    let mut other = token.clone();
    let last = other.pop().unwrap();
    other.push(if last == '0' { '1' } else { '0' });
    assert!(other.parse::<waveport_assembly::ResumeToken>().is_ok());
    for wrong in ["wpr1_madeup", &other] {
        let refused = Client::connect(&address, &with(wrong, 1, 0), soon).await;
        let Err(Unconnected::Refused { code, .. }) = refused else {
            panic!("a stranger was taken");
        };
        assert_eq!(code, ErrorCode::Token);
    }

    // It comes back with it, and is given its line: the same call, in
    // the same life, and the same token to come back with again.
    let mut caller = Client::connect(&address, &with(&token, 1, 0), soon)
        .await
        .unwrap();
    let back = caller.welcome().clone();
    assert!(back.resumed, "{back:?}");
    assert_eq!(
        (back.session, back.life, back.resume_token),
        (welcome.session, 1, Some(token))
    );
    // And the call goes as any other.
    caller.text("What time is it right now?").await.unwrap();
    let (facts, reason) = stays_on(&mut caller).await;
    let code = serving.ended().await;
    let all = format!("{}--- the facts ---\n{facts:#?}\n", serving.all());
    assert_eq!(reason, ByeReason::Ended, "{all}");
    assert_eq!(code, Some(0), "{all}");

    // It was told the call in facts, each numbered after the one before.
    assert!(facts.windows(2).all(|two| two[0].0 < two[1].0), "{all}");
    let at = |is: &dyn Fn(&Fact) -> bool| {
        facts
            .iter()
            .position(|(_, fact)| is(fact))
            .unwrap_or_else(|| panic!("a fact is missing\n{all}"))
    };
    let asked = at(
        &|fact| matches!(fact, Fact::TurnCommitted { text, .. } if text == "What time is it right now?"),
    );
    // A caller of an agent that takes calls on this machine alone is
    // shown what the tools were given and what they answered.
    let looked_up = at(&|fact| {
        matches!(fact, Fact::ToolCalled { tool, args, .. }
            if tool == "get_time" && args.as_deref() == Some("{}"))
    });
    let answered = at(&|fact| {
        matches!(fact, Fact::UtteranceCommitted { text, truncated: false, .. }
            if text == "It is ten past three. Goodbye!")
    });
    let hung_up = at(&|fact| {
        matches!(fact, Fact::ToolFinished { result, .. }
            if result.as_deref() == Some("The call is ending."))
    });
    let ended = at(&|fact| {
        matches!(
            fact,
            Fact::LifecycleChanged {
                to: Lifecycle::Closed,
                ..
            }
        )
    });
    assert!(
        asked < looked_up && looked_up < answered && answered < hung_up && hung_up < ended,
        "{all}"
    );
    // How it ended is the last thing it was told before the goodbye.
    assert_eq!(ended, facts.len() - 1, "{all}");

    // The agent's side told the same call, and that its caller was away.
    let served = serving.told.lock().unwrap().join("\n");
    tells_the_call(&served, "[1] ", &all);
    assert!(
        served.contains("[1] the caller is back, from 127.0.0.1:"),
        "{all}"
    );
    assert!(!served.contains("the caller left"), "{all}");
}

#[tokio::test]
async fn a_caller_who_could_be_anybody_is_not_shown_what_the_tools_said() {
    let providers = Providers::start().await;
    // An agent that may be reached from other machines, though it is
    // not: nothing asks its callers who they are.
    let mut arguments = [
        "serve",
        "--listen",
        "127.0.0.1:0",
        "--exposed",
        "--calls",
        "1",
    ]
    .map(str::to_owned)
    .to_vec();
    arguments.extend(providers.arguments());
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let (mut serving, address) = Serving::start(&arguments).await;

    let hello = Hello {
        features: vec!["tools".to_owned()],
        ..Hello::new(Some(AudioFormat {
            codec: Codec::Pcm16,
            rate: 24_000,
        }))
    };
    let mut caller = Client::connect(&address, &hello, Duration::from_secs(10))
        .await
        .unwrap();
    caller.text("What time is it right now?").await.unwrap();
    let (facts, reason) = stays_on(&mut caller).await;
    let code = serving.ended().await;
    let all = format!("{}--- the facts ---\n{facts:#?}\n", serving.all());
    assert_eq!((reason, code), (ByeReason::Ended, Some(0)), "{all}");

    // It is told that the tools were called, and how that went, and
    // nothing of what they were given or what they answered.
    let mut tools = Vec::new();
    let mut finished = 0;
    for (_, fact) in &facts {
        match fact {
            Fact::ToolCalled { tool, args, .. } => {
                assert_eq!(*args, None, "{all}");
                tools.push(tool.as_str());
            }
            Fact::ToolFinished { result, .. } => {
                assert_eq!(*result, None, "{all}");
                finished += 1;
            }
            _ => {}
        }
    }
    assert_eq!(tools, ["get_time", "end_call"], "{all}");
    assert_eq!(finished, 2, "{all}");
    // What was said is no secret from whoever it was said to.
    assert!(
        facts.iter().any(|(_, fact)| matches!(fact,
            Fact::UtteranceCommitted { text, .. } if text == "It is ten past three. Goodbye!")),
        "{all}"
    );
    // The agent's own terminal has all of it.
    let served = serving.told.lock().unwrap().join("\n");
    tells_the_call(&served, "[1] ", &all);
}

/// How a stand-in for an agent's side ends a connection: with the facts
/// numbered up to this, if any, and then with a goodbye, or with none.
type LineEnd = (Option<u64>, Option<ByeReason>);

/// A stand-in for an agent's side, for a caller to be held to what the
/// protocol asks of it. It takes as many connections as `ends` has
/// entries, one after the other. Each is welcomed to a life of the call
/// that has the connection's number, with a token of its own, and ended
/// as its entry says. Returns the hello of each.
async fn stand_in_for_an_agent(
    listener: tokio::net::TcpListener,
    ends: Vec<LineEnd>,
) -> Vec<FromClient> {
    let soon = Duration::from_secs(10);
    let mut hellos = Vec::new();
    for (life, (facts, goodbye)) in (1_u64..).zip(ends) {
        let (tcp, _) = listener.accept().await.unwrap();
        let (mut conn, _) = WsConn::accept(tcp, PATH, Some(PROTOCOL), soon)
            .await
            .unwrap();
        let Some(WsMessage::Text(said)) = conn.next().await.unwrap() else {
            panic!("a connection that does not begin with a hello");
        };
        let hello: FromClient = serde_json::from_str(said.as_str()).unwrap();
        let FromClient::Hello { audio, .. } = &hello else {
            panic!("a connection that begins with {hello:?}");
        };
        let mut says = vec![FromServer::Welcome {
            v: protocol::VERSION,
            session: "a-call".to_owned(),
            life,
            resumed: life > 1,
            resume_token: Some(format!("wpr1_madeup{life}")),
            audio: *audio,
            features: Vec::new(),
            next_seq: 1,
            grace_ms: Some(30_000),
        }];
        says.extend((1..=facts.unwrap_or(0)).map(|seq| FromServer::Fact {
            seq,
            fact: Fact::CutOff {
                cause: Cut::BargeIn,
            },
        }));
        says.extend(goodbye.map(|reason| FromServer::Bye { reason }));
        for message in says {
            let written = serde_json::to_string(&message).unwrap();
            conn.send_text(written).await.unwrap();
        }
        hellos.push(hello);
        // Closed as a connection is that has nothing more to say.
        conn.close_and_wait(None, Duration::from_millis(500)).await;
    }
    hellos
}

#[tokio::test]
async fn a_caller_comes_back_to_a_line_that_closed_without_a_goodbye() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("ws://{}", listener.local_addr().unwrap());
    // The first line is closed with no goodbye, as something between
    // the caller and the agent closes one. The second is closed by an
    // agent's side that says it is going away, after three facts. The
    // third ends the call.
    let agent = tokio::spawn(stand_in_for_an_agent(
        listener,
        vec![
            (None, None),
            (Some(3), Some(ByeReason::GoingAway)),
            (None, Some(ByeReason::Ended)),
        ],
    ));

    let folder = folder("comes-back-unasked");
    let script = folder.join("waits.call");
    // A caller who waits for an agent that never speaks.
    std::fs::write(&script, "listen 500 20000\n").unwrap();
    let output = waveport(&[
        "client",
        "--url",
        &address,
        "--script",
        script.to_str().unwrap(),
    ])
    .await;
    let told = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    let all = format!("--- told ---\n{told}--- errors ---\n{errors}");

    // It came back each time, and said why it had to.
    for why in [
        "its connection closed with no goodbye",
        "the agent's side is going away",
    ] {
        let line =
            format!("waveport: the line to the agent dropped ({why}): coming back to the call");
        assert_eq!(errors.matches(&line).count(), 1, "{line}\n{all}");
    }
    assert_eq!(told.matches("the call goes on\n").count(), 2, "{all}");
    assert!(told.contains("the call ended: nobody said why"), "{all}");
    assert_eq!(output.status.code(), Some(0), "{all}");

    // Each time it said where it was: with the token and the life the
    // line before gave it, and the last fact it was sent.
    let hellos = tokio::time::timeout(Duration::from_secs(10), agent)
        .await
        .unwrap_or_else(|_| panic!("the caller did not come back\n{all}"))
        .unwrap();
    let came_with: Vec<Option<(String, u64, u64)>> = hellos
        .into_iter()
        .map(|hello| match hello {
            FromClient::Hello { resume, .. } => {
                resume.map(|Resume { token, life, seen }| (token, life, seen))
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        came_with,
        [
            None,
            Some(("wpr1_madeup1".to_owned(), 1, 0)),
            Some(("wpr1_madeup2".to_owned(), 2, 3)),
        ],
        "{all}"
    );
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
