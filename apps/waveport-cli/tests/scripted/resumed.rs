//! A call whose process is killed, gone on with in another.
//!
//! The command is started with a caller who asks one question, and is
//! killed where each test says: in the middle of the model's answer, in
//! the middle of the agent's sentence, once a tool has acted, while the
//! record that allows the tool waits to be stored, and around the commit
//! that stores all the call has come to. It is killed as `kill -9` does
//! it, with no chance to finish anything. The command is then started
//! again with `--resume last` and a caller who asks once more.
//!
//! Where the process is when it dies has to be exact, and some of the
//! places last less than a millisecond. So the command is told where to
//! stop in `WAVEPORT_TEST_HALT`, says `halted` when it is there, and is
//! killed then. `src/halt.rs` says what the places are.
//!
//! What every test checks afterwards:
//!
//! - the second process goes on with the call and does not begin one,
//! - the model is shown the conversation as the journal had it,
//! - the tool that acts, which adds a line to a file, has acted at most
//!   once in both processes together,
//! - the caller's next question is answered aloud,
//! - and a call that is over is not there to go on with.

use core::fmt::Write as _;
use core::time::Duration;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Instant;

use waveport_adapter_kit::pcm::write_base64;
use waveport_adapter_kit::wav;
use waveport_ports::audio::Rate;
use waveport_testkit::servers::{End, HttpAnswer, HttpServer, WsServer, WsStep};

use super::{HEARD, assets, ended, folder, lines_of, one_question, piece, started_at};

/// What the caller asks, as the recogniser was recorded hearing it.
const QUESTION: &str = "What time is it right now?";

/// What the agent says, every time it says anything: the stand-in for
/// the synthesiser then needs to know one sentence only.
const SAID: [&str; 2] = ["I have", " made a note."];

/// The note the model asks to have written down.
const NOTE: &str = "The caller asked for the time.";

/// What the model is told about a session that was restarted.
const RESTORED: &str = "the session was interrupted by a failure and has been restored";

/// An answer of the model: the sentence, and then, if `note` says so, a
/// call of the tool that writes a note down.
fn says(note: bool) -> HttpAnswer {
    let mut stream = piece("{\"role\":\"assistant\",\"content\":\"\"}", "null");
    for words in SAID {
        stream.push_str(&piece(&format!("{{\"content\":\"{words}\"}}"), "null"));
    }
    if note {
        let arguments = serde_json::json!({ "text": NOTE }).to_string();
        let call = serde_json::json!({ "tool_calls": [{
            "index": 0,
            "id": "call_take_note",
            "type": "function",
            "function": { "name": "take_note", "arguments": arguments },
        }] });
        stream.push_str(&piece(&call.to_string(), "null"));
        stream.push_str(&piece("{}", "\"tool_calls\""));
    } else {
        stream.push_str(&piece("{}", "\"stop\""));
    }
    stream.push_str("data: [DONE]\n\n");
    HttpAnswer::recorded(&stream)
}

/// An answer of the model that stops in the middle of the sentence, and
/// is never finished.
fn stops_short() -> HttpAnswer {
    let begun = [
        piece("{\"role\":\"assistant\",\"content\":\"\"}", "null"),
        piece(&format!("{{\"content\":\"{}\"}}", SAID[0]), "null"),
    ];
    HttpAnswer::Stream {
        pieces: begun
            .iter()
            .map(|piece| piece.as_bytes().to_vec())
            .collect(),
        pause: Duration::ZERO,
        end: End::Hang,
    }
}

/// What the synthesiser says, for as many answers as `answers`: for
/// each, 2.4 s of a tone at 24 kHz in four pieces, with where the five
/// words of the sentence are. An answer takes long enough to be killed
/// in the middle of.
fn spoken_slowly(answers: usize) -> Vec<WsStep> {
    let tone: Vec<i16> = (0..Rate::Hz24000.samples_in_millis(600))
        .map(|n| if n % 48 < 24 { 3_000 } else { -3_000 })
        .collect();
    let mut data = String::new();
    write_base64(&tone, &mut data);
    let chunk = format!(
        "< {{\"type\":\"chunk\",\"data\":\"{data}\",\"done\":false,\"status_code\":206,\
         \"step_time\":12,\"context_id\":\"{{{{context_id}}}}\"}}"
    );
    let words = "< {\"type\":\"timestamps\",\"done\":false,\"status_code\":206,\
                 \"context_id\":\"{{context_id}}\",\"word_timestamps\":{\"words\":\
                 [\"I\",\"have\",\"made\",\"a\",\"note.\"],\
                 \"start\":[0.0,0.5,1.0,1.5,1.9],\"end\":[0.5,1.0,1.5,1.9,2.4]}}";
    let done = "< {\"type\":\"done\",\"done\":true,\"status_code\":206,\
                \"context_id\":\"{{context_id}}\"}";
    let one = [
        "> \"transcript\":\"I have",
        &chunk,
        words,
        &chunk,
        &chunk,
        &chunk,
        "> \"continue\":false",
        done,
    ]
    .join("\n");
    WsStep::script(&vec![one; answers].join("\n"))
}

/// Where a call is killed, and what it is given to get there.
struct Killed {
    /// A name for the folder of the test.
    name: &'static str,
    /// Where the command is told to stop.
    place: &'static str,
    /// What the model answers in the process that is killed, and then
    /// in the one that goes on. The last answer is given again for as
    /// long as the model is asked.
    answers: Vec<HttpAnswer>,
    /// After how many records the call stores all it has come to.
    state_every: u32,
}

/// One thing in the conversation, as the model is shown it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    role: String,
    /// What was said. Empty for an answer that only called a tool.
    content: String,
    /// The tools an answer called, by name.
    calls: Vec<String>,
}

/// What the model was shown in one request, without what the agent is
/// told before the call.
fn shown(body: &str) -> Vec<Shown> {
    let request: serde_json::Value = serde_json::from_str(body).unwrap();
    let name = |call: &serde_json::Value| call["function"]["name"].as_str().unwrap().to_owned();
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|message| Shown {
            role: message["role"].as_str().unwrap().to_owned(),
            content: message["content"].as_str().unwrap_or_default().to_owned(),
            calls: message["tool_calls"]
                .as_array()
                .map_or_else(Vec::new, |calls| calls.iter().map(name).collect()),
        })
        .collect()
}

/// How a call that was killed went on.
struct WentOn {
    /// What the process that was killed told.
    before: Vec<String>,
    /// What the process that went on told.
    after: Vec<String>,
    /// What the model was shown each time it was asked by the process
    /// that was killed.
    asked_before: Vec<Vec<Shown>>,
    /// The same of the process that went on.
    asked_after: Vec<Vec<Shown>>,
    /// The lines of the file of notes when the process was killed.
    notes_before: Vec<String>,
    /// The lines of the file of notes in the end.
    notes: Vec<String>,
    /// Everything there is to go by if something is not as it should be.
    all: String,
}

impl WentOn {
    /// The line the process that went on told that begins with `start`.
    fn told(&self, start: &str) -> usize {
        self.after
            .iter()
            .position(|line| line.starts_with(start))
            .unwrap_or_else(|| panic!("no line begins with {start:?}\n{}", self.all))
    }

    /// What the model was shown when it was asked last.
    fn last_shown(&self) -> &[Shown] {
        self.asked_after
            .last()
            .unwrap_or_else(|| panic!("the model was not asked again\n{}", self.all))
    }
}

fn lines(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file).map_or_else(
        |_| Vec::new(),
        |text| text.lines().map(str::to_owned).collect(),
    )
}

/// Writes the script of a caller who lets the agent finish what it has
/// to say, asks the question, and waits to be answered. Returns where
/// the script is.
fn asks_again(folder: &Path) -> PathBuf {
    let script = folder.join("again.call");
    std::fs::write(
        &script,
        "# A caller who was cut off, and asks again.\n\
         listen 800 12000\n\
         say question.wav\n\
         listen 1500 40000\n",
    )
    .unwrap();
    script
}

/// What both processes of a test are run with: the providers, which
/// outlast the first, and the files, which are all that the second has
/// of it.
struct Set {
    folder: PathBuf,
    notes: PathBuf,
    recording: PathBuf,
    llm: HttpServer,
    /// Not asked anything by the test, and there for the command.
    _stt: WsServer,
    _tts: WsServer,
    /// What tells the command where everything is.
    common: Vec<String>,
}

impl Set {
    async fn up(killed: Killed) -> Self {
        let folder = folder(killed.name);
        let notes = folder.join("notes.txt");
        let stt = WsServer::start(vec![WsStep::script(HEARD)]).await;
        let llm = HttpServer::start(killed.answers).await;
        let tts = WsServer::start(vec![spoken_slowly(6)]).await;
        let common = [
            "--journal",
            folder.join("calls.redb").to_str().unwrap(),
            "--notes",
            notes.to_str().unwrap(),
            "--state-every",
            &killed.state_every.to_string(),
            "--assets",
            assets().to_str().unwrap(),
            "--stt-url",
            &stt.url("/v1/realtime"),
            "--llm-url",
            &llm.url("/v1"),
            "--tts-url",
            &tts.url("/tts/websocket"),
        ]
        .map(str::to_owned)
        .to_vec();
        Self {
            recording: folder.join("agent.wav"),
            folder,
            notes,
            llm,
            _stt: stt,
            _tts: tts,
            common,
        }
    }

    /// Starts the command with a caller who follows `script`, and with
    /// what `more` says besides.
    fn run(&self, script: &Path, more: &[&str], halt: Option<&str>) -> Child {
        let mut arguments = vec!["run", "--script", script.to_str().unwrap()];
        arguments.extend(more);
        arguments.extend(self.common.iter().map(String::as_str));
        started_at(&self.folder, &arguments, halt)
    }

    /// What the model was shown, each time it was asked so far.
    fn asked(&self) -> Vec<Vec<Shown>> {
        self.llm
            .requests()
            .iter()
            .map(|request| shown(&request.body))
            .collect()
    }

    /// What the model was shown so far, once its server has recorded
    /// nothing more for a while.
    ///
    /// A process that is killed may have sent the model a request that
    /// its server has not got round to yet. Read too early, that
    /// request is missing from what the killed process asked, and is
    /// taken for the first thing the next process asked.
    async fn asked_once_quiet(&self) -> Vec<Vec<Shown>> {
        let mut asked = self.asked();
        let mut quiet_since = Instant::now();
        while quiet_since.elapsed() < Duration::from_millis(300) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let now = self.asked();
            if now.len() != asked.len() {
                asked = now;
                quiet_since = Instant::now();
            }
        }
        asked
    }

    /// Runs a call until its process is at `place`, and kills the
    /// process there. Returns what it told, and its errors.
    async fn killed_at(&self, place: &str) -> (Vec<String>, String) {
        let mut first = self.run(&one_question(&self.folder), &[], Some(place));
        let told = lines_of(first.stdout.take().unwrap());
        let errors = lines_of(first.stderr.take().unwrap());
        let halted = format!("halted: {place}");
        let began = Instant::now();
        loop {
            let so_far = told.lock().unwrap().clone();
            if so_far.contains(&halted) {
                break;
            }
            let gone = first.try_wait().unwrap();
            assert!(
                gone.is_none() && began.elapsed() < Duration::from_secs(60),
                "the command did not get to {place}: {gone:?}\n--- told ---\n{}\n\
                 --- errors ---\n{}",
                so_far.join("\n"),
                errors.lock().unwrap().join("\n")
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // As `kill -9` does it: the process is given no chance to
        // finish anything.
        first.kill().unwrap();
        first.wait().unwrap();
        let told = told.lock().unwrap().clone();
        let errors = errors.lock().unwrap().join("\n");
        (told, errors)
    }

    /// Checks that there is no call to go on with.
    async fn has_nothing_to_go_on_with(&self, all: &str) {
        let script = self.folder.join("again.call");
        let mut third = self.run(&script, &["--resume", "last"], None);
        ended(&mut third).await;
        let output = third.wait_with_output().unwrap();
        let errors = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{errors}\n{all}");
        assert!(
            errors.contains("there is no call to go on with"),
            "{errors}\n{all}"
        );
    }
}

/// Runs a call, kills its process where `killed` says, goes on with the
/// call in another process, and checks what holds wherever a call is
/// killed. Returns how it went, for what holds only there.
async fn goes_on(killed: Killed) -> WentOn {
    let place = killed.place;
    let set = Set::up(killed).await;

    // The process that is killed.
    let (before, errors_before) = set.killed_at(place).await;
    let notes_before = lines(&set.notes);
    let asked_before = set.asked_once_quiet().await;

    // The process that goes on.
    let record = [
        "--resume",
        "last",
        "--record",
        set.recording.to_str().unwrap(),
    ];
    let mut second = set.run(&asks_again(&set.folder), &record, None);
    ended(&mut second).await;
    let output = second.wait_with_output().unwrap();
    let after: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    let asked = set.asked();
    let notes = lines(&set.notes);

    let mut all = format!(
        "--- the process that was killed told ---\n{}\n--- and its errors ---\n{errors_before}\n\
         --- the process that went on told ---\n{}\n--- and its errors ---\n{}\n\
         --- the notes ---\n{}\n",
        before.join("\n"),
        after.join("\n"),
        String::from_utf8_lossy(&output.stderr),
        notes.join("\n"),
    );
    for (n, shown) in asked.iter().enumerate() {
        let by = if n < asked_before.len() {
            "the process that was killed"
        } else {
            "the process that went on"
        };
        let _ = writeln!(all, "--- the model, request {n}, by {by} ---");
        for said in shown {
            let _ = writeln!(all, "{said:?}");
        }
    }
    assert!(output.status.success(), "{all}");
    let went = WentOn {
        before,
        after,
        asked_after: asked[asked_before.len()..].to_vec(),
        asked_before,
        notes_before,
        notes,
        all,
    };
    went.holds_wherever_a_call_is_killed(&set.recording);
    set.has_nothing_to_go_on_with(&went.all).await;
    went
}

impl WentOn {
    /// Checks what holds of a call that went on, wherever its first
    /// process was killed. `recording` is what the caller was played by
    /// the second.
    fn holds_wherever_a_call_is_killed(&self, recording: &Path) {
        let all = &self.all;
        // It found the call that had broken off, went on with it, and
        // did not begin one.
        self.told("note: 1 call that broke off earlier is kept in ");
        let went_on = self.told("the call goes on, from ");
        let began = |line: &String| line.starts_with("the call has begun");
        assert!(!self.after.iter().any(began), "{all}");
        // The caller asked again, and was answered: with the whole
        // sentence, and nothing after it.
        let asked_again = self.told(&format!("caller: {QUESTION}"));
        let answer = format!("agent: {}", SAID.concat());
        let answered = self
            .after
            .iter()
            .rposition(|line| line.starts_with(&answer) && !line.contains("[cut off]"))
            .unwrap_or_else(|| panic!("the caller was not answered\n{all}"));
        let over = self.told("the call ended: the caller left");
        assert!(
            went_on < asked_again && asked_again < answered && answered < over,
            "{all}"
        );
        // When the model was asked last, it was shown the question of
        // the first process and the question of the second, and was told
        // between them that the session had been restarted.
        let last = self.last_shown();
        let questions: Vec<usize> = (0..last.len())
            .filter(|at| last[*at].role == "user" && last[*at].content == QUESTION)
            .collect();
        assert_eq!(questions.len(), 2, "{all}");
        let restored = last
            .iter()
            .position(|said| said.role == "system" && said.content.contains(RESTORED))
            .unwrap_or_else(|| panic!("the model was not told of the restart\n{all}"));
        assert!(questions[0] < restored && restored < questions[1], "{all}");
        // The tool that acts did not act twice.
        assert!(self.notes.len() <= 1, "{all}");
        assert!(self.notes.iter().all(|note| note == NOTE), "{all}");
        // What the caller was played by the second process has the
        // answer in it: 2.4 s of the tone, at least.
        let played = wav::read(&std::fs::read(recording).unwrap()).unwrap();
        let loud = played.samples.iter().filter(|s| s.abs() > 1_000).count();
        assert!(loud >= 110_000, "{loud} loud samples\n{all}");
    }
}

/// The first thing in a conversation: what the caller asked.
fn question() -> Shown {
    Shown {
        role: "user".to_owned(),
        content: QUESTION.to_owned(),
        calls: Vec::new(),
    }
}

/// Something the agent said.
fn agent_said(text: &str) -> Shown {
    Shown {
        role: "assistant".to_owned(),
        content: text.to_owned(),
        calls: Vec::new(),
    }
}

/// An answer of the model that called the tool that writes a note down.
fn asked_for_a_note() -> Shown {
    Shown {
        role: "assistant".to_owned(),
        content: String::new(),
        calls: vec!["take_note".to_owned()],
    }
}

/// What the tool that writes a note down answered, or what was said of
/// it in its place.
fn tool_said(text: &str) -> Shown {
    Shown {
        role: "tool".to_owned(),
        content: text.to_owned(),
        calls: Vec::new(),
    }
}

impl WentOn {
    /// Checks that the model was asked by the process that went on, the
    /// first time, with a conversation that begins as `begins` and ends
    /// with being told of the restart.
    fn first_shown_is(&self, begins: &[Shown]) {
        let all = &self.all;
        let first = self.asked_after.first().expect("it was asked again");
        let (told, before) = first.split_last().expect("it was shown something");
        assert_eq!(before, begins, "{all}");
        assert_eq!(told.role, "system", "{all}");
        assert!(told.content.contains(RESTORED), "{all}");
    }
}

#[tokio::test]
async fn a_call_killed_while_the_model_answers_goes_on() {
    // The model has begun its answer and never ends it. What the caller
    // asked is stored, and the journal stores nothing after it.
    let went = goes_on(Killed {
        name: "killed-while-the-model-answers",
        place: "after:turn_committed",
        answers: vec![stops_short(), says(false)],
        state_every: 256,
    })
    .await;
    let all = &went.all;
    // The model had been asked, and the agent had said nothing yet.
    assert_eq!(went.asked_before, [vec![question()]], "{all}");
    assert!(
        !went.before.iter().any(|line| line.starts_with("agent:")),
        "{all}"
    );

    // The call is the question and nothing else: of the half answer
    // there is no trace. The answer that was owed is asked for again at
    // once, before the caller says anything.
    went.told("the call goes on, from its 2 records");
    went.first_shown_is(&[question()]);
    let owed = went.told(&format!("agent: {}", SAID.concat()));
    assert!(owed < went.told("caller: "), "{all}");
    assert!(went.notes.is_empty(), "{all}");
}

#[tokio::test]
async fn a_call_killed_while_the_agent_speaks_goes_on() {
    // The agent is a second into a sentence of 2.4 s, and the journal
    // has how far the caller had heard it by then.
    let went = goes_on(Killed {
        name: "killed-while-the-agent-speaks",
        place: "after:utterance_progress",
        answers: vec![says(false)],
        state_every: 256,
    })
    .await;
    let all = &went.all;
    // The sentence had not been told as said: it was not over.
    assert!(
        !went.before.iter().any(|line| line.starts_with("agent:")),
        "{all}"
    );

    // The call has the sentence as far as it is known to have been
    // heard: its first words, and not all of it.
    went.told("the call goes on, from its 3 records");
    let cut = went.told("agent: ");
    let heard = went.after[cut]
        .strip_prefix("agent: ")
        .and_then(|line| line.strip_suffix(" [cut off]"))
        .unwrap_or_else(|| panic!("the sentence was not cut off\n{all}"));
    let sentence = SAID.concat();
    assert!(
        !heard.is_empty() && heard.len() < sentence.len() && sentence.starts_with(heard),
        "{heard:?}\n{all}"
    );
    assert!(sentence[heard.len()..].starts_with(' '), "{heard:?}\n{all}");
    // That is what the model is shown of it, and it is asked again for
    // the answer the caller did not get.
    went.first_shown_is(&[question(), agent_said(heard)]);
    assert!(went.notes.is_empty(), "{all}");
}

#[tokio::test]
async fn a_call_killed_after_a_tool_has_acted_goes_on() {
    // The tool has written its note and has not answered. By then the
    // record that allowed the call was stored: the tool is not called
    // before that. Whether the record that it was called is stored too
    // is not the same every time, and makes no difference.
    let went = goes_on(Killed {
        name: "killed-after-a-tool-has-acted",
        place: "tool",
        answers: vec![says(true), says(false)],
        state_every: 256,
    })
    .await;
    let all = &went.all;
    assert_eq!(went.notes_before, [NOTE], "{all}");

    // Whether the note was written is not known to the session that
    // goes on: only that the call was made. It says so, and does not
    // make the call again.
    let doubted = went.told("tool: a call from before the call broke off -> ");
    assert!(went.after[doubted].ends_with("[in doubt]"), "{all}");
    let first = &went.asked_after[0];
    assert_eq!(
        first[..3],
        [
            question(),
            asked_for_a_note(),
            tool_said("the call was made, and whether it took effect is not known"),
        ],
        "{all}"
    );
    // The sentence that came with the call was being said when the
    // process died. How much of it the journal had by then is not the
    // same every time: none of it, or its first words.
    let sentence = SAID.concat();
    match &first[3..] {
        [told] => assert!(told.content.contains(RESTORED), "{all}"),
        [said, told] => {
            assert_eq!(said.role, "assistant", "{all}");
            assert!(sentence.starts_with(&said.content), "{all}");
            assert!(told.content.contains(RESTORED), "{all}");
        }
        _ => panic!("the model was shown something else\n{all}"),
    }
    // One note, and no second one.
    assert_eq!(went.notes, [NOTE], "{all}");
    let called_again = went
        .after
        .iter()
        .any(|line| line.starts_with("tool: take_note"));
    assert!(!called_again, "{all}");
}

#[tokio::test]
async fn a_call_killed_before_a_tool_was_allowed_goes_on() {
    // The model has asked for the note. The record that allows the call
    // is with the journal and is not stored, and the tool waits for it.
    let went = goes_on(Killed {
        name: "killed-before-a-tool-was-allowed",
        place: "before:tool_authorized",
        answers: vec![says(true), says(true), says(false)],
        state_every: 256,
    })
    .await;
    let all = &went.all;
    // The tool had not acted when the process died.
    assert!(went.notes_before.is_empty(), "{all}");

    // The journal knows nothing of the call, so nothing is in doubt:
    // the model is asked again, asks for the note again, and this time
    // it is written. Once.
    went.told("the call goes on, from its 2 records");
    went.first_shown_is(&[question()]);
    let written = went.told("tool: take_note(");
    assert!(
        went.after[written].ends_with("-> It is written down."),
        "{all}"
    );
    assert_eq!(went.notes, [NOTE], "{all}");
    let shown = went.last_shown();
    assert_eq!(
        shown[2..4],
        [asked_for_a_note(), tool_said("It is written down.")],
        "{all}"
    );
}

#[tokio::test]
async fn a_call_killed_before_its_state_was_stored_goes_on() {
    // A call that stores all it has come to at every turn. The commit
    // that would store it the first time is not made.
    let went = goes_on(Killed {
        name: "killed-before-its-state-was-stored",
        place: "before:state",
        answers: vec![says(false)],
        state_every: 1,
    })
    .await;
    // The records are all there is, and all that is needed.
    went.told("the call goes on, from its 2 records");
    went.first_shown_is(&[question()]);
}

#[tokio::test]
async fn a_call_killed_after_its_state_was_stored_goes_on() {
    // The same call, killed once the commit is made: the state is in
    // the file, and the records it replaced are not.
    let went = goes_on(Killed {
        name: "killed-after-its-state-was-stored",
        place: "after:state",
        answers: vec![says(false)],
        state_every: 1,
    })
    .await;
    // It is the same call as from the records.
    went.told("the call goes on, from what it had come to by record 2 and 0 records after that");
    went.first_shown_is(&[question()]);
}
