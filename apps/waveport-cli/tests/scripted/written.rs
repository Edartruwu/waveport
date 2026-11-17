//! A chat, from the command line to its last word.
//!
//! The command is run as anyone would run it, `waveport chat`, with
//! what is typed coming through a pipe. The agent is the one of a call:
//! the same session runtime, the same language model adapter, the same
//! two tools, the same journal. What a call has besides is not there at
//! all: no recogniser, no synthesiser and no model that listens is
//! started for these tests, and the command is told of none.
//!
//! The language model is a server on this machine whose answers are
//! made up, in the shape of the ones that were recorded for the
//! adapter's own tests.

use core::time::Duration;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use waveport_testkit::servers::HttpServer;

use super::{ANSWER, answer, desk, ended, lines_of, waveport_at};

/// The command in a chat, what is typed to it and what it has said.
struct Chatting {
    child: Child,
    typing: Option<ChildStdin>,
    told: Arc<Mutex<Vec<String>>>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Chatting {
    /// Starts a chat in the folder `desk`, with the model at `llm`.
    fn start(desk: &Path, llm: &HttpServer, arguments: &[&str]) -> Self {
        Self::start_to_halt(desk, llm, arguments, None)
    }

    /// The same, with the command told to stop at `halt`, if anywhere.
    fn start_to_halt(
        desk: &Path,
        llm: &HttpServer,
        arguments: &[&str],
        halt: Option<&str>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_waveport"));
        if let Some(place) = halt {
            command.env("WAVEPORT_TEST_HALT", place);
        } else {
            command.env_remove("WAVEPORT_TEST_HALT");
        }
        let mut child = command
            .arg("chat")
            .args(["--llm-url", &llm.url("/v1")])
            .args(arguments)
            .current_dir(desk)
            // No key of anyone's is needed for a server on this machine,
            // and none is to reach it.
            .env_remove("OPENAI_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the command, started");
        Self {
            typing: child.stdin.take(),
            told: lines_of(child.stdout.take().unwrap()),
            errors: lines_of(child.stderr.take().unwrap()),
            child,
        }
    }

    /// Types a line.
    fn types(&mut self, line: &str) {
        let typing = self.typing.as_mut().expect("someone still typing");
        writeln!(typing, "{line}").unwrap();
        typing.flush().unwrap();
    }

    /// Types nothing more: what is typed has come to its end.
    fn stops_typing(&mut self) {
        self.typing = None;
    }

    /// Everything the command has said.
    fn all(&self) -> String {
        format!(
            "--- told ---\n{}\n--- errors ---\n{}\n",
            self.told.lock().unwrap().join("\n"),
            self.errors.lock().unwrap().join("\n")
        )
    }

    /// The lines the command has said so far.
    fn lines(&self) -> Vec<String> {
        self.told.lock().unwrap().clone()
    }

    /// Waits until the command has said the line `line`.
    async fn tells(&mut self, line: &str) {
        let began = Instant::now();
        while !self.lines().iter().any(|told| told == line) {
            let gone = self.child.try_wait().unwrap().is_some();
            assert!(
                !gone && began.elapsed() < Duration::from_secs(60),
                "the chat never said {line:?}\n{}",
                self.all()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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

impl Drop for Chatting {
    fn drop(&mut self) {
        // A test that failed leaves nothing running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The lines of a chat that are the chat: what was typed, what the agent
/// wrote and did, and how it ended.
fn chat(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .map(String::as_str)
        .filter(|line| !line.starts_with("note: "))
        .collect()
}

#[tokio::test]
async fn what_is_typed_is_answered_in_writing() {
    // The model asks for the time, says it and ends the chat.
    let llm = HttpServer::start(vec![
        answer(&[], Some("get_time")),
        answer(&ANSWER, Some("end_call")),
        answer(&[], None),
    ])
    .await;
    let mut chatting = Chatting::start(&desk(), &llm, &[]);
    chatting.types("What time is it right now?");

    // The agent ended the chat, with whoever typed still there.
    assert_eq!(chatting.ended().await, Some(0), "{}", chatting.all());
    let all = chatting.all();
    let lines = chatting.lines();
    let chat = chat(&lines);
    assert_eq!(chat.len(), 6, "{all}");
    assert_eq!(chat[0], "the call has begun", "{all}");
    assert_eq!(chat[1], "you: What time is it right now?", "{all}");
    assert!(chat[2].starts_with("tool: get_time({}) -> "), "{all}");
    // The answer is one line, written as its pieces came.
    assert_eq!(chat[3], "agent: It is ten past three. Goodbye!", "{all}");
    assert!(chat[4].starts_with("tool: end_call({}) -> "), "{all}");
    assert_eq!(chat[5], "the call ended: it was asked to", "{all}");
    assert_eq!(chatting.errors.lock().unwrap().as_slice(), [""; 0], "{all}");

    // The model was asked twice: with what was typed and the two tools,
    // and then with the time as well. It was told that it writes, and
    // nothing of a voice.
    let asked = llm.requests();
    assert_eq!(asked.len(), 2, "{all}");
    assert_eq!(asked[0].line, "POST /v1/chat/completions HTTP/1.1");
    assert!(asked[0].body.contains("What time is it right now?"));
    assert!(asked[0].body.contains("\"get_time\"") && asked[0].body.contains("\"end_call\""));
    assert!(
        asked[0].body.contains("assistant in a chat"),
        "{}",
        asked[0].body
    );
    assert!(!asked[0].body.contains("spoken"), "{}", asked[0].body);
    assert!(!asked[0].body.contains("time_zone"));
    assert!(asked[1].body.contains("time_zone"), "{}", asked[1].body);
}

#[tokio::test]
async fn lines_that_come_at_once_wait_their_turn_and_the_chat_ends_with_them() {
    let llm = HttpServer::start(vec![
        answer(&["Hello", " there."], None),
        answer(&["I am", " well."], None),
        answer(&["Nobody", " asked."], None),
    ])
    .await;
    let greeting = ["--greeting", "Welcome."];
    let mut chatting = Chatting::start(&desk(), &llm, &greeting);
    // Three lines before anything is answered, one of them empty, and
    // then nothing more.
    chatting.types("hello");
    chatting.types("");
    chatting.types("how are you");
    chatting.stops_typing();

    assert_eq!(chatting.ended().await, Some(0), "{}", chatting.all());
    let all = chatting.all();
    let lines = chatting.lines();
    assert_eq!(
        chat(&lines),
        [
            "the call has begun",
            "agent: Welcome.",
            "you: hello",
            "agent: Hello there.",
            "you: how are you",
            "agent: I am well.",
            "the call ended: it was asked to",
        ],
        "{all}"
    );

    // Each line was answered by itself: the second was said when the
    // first had its answer, and the model saw that answer whole.
    let asked = llm.requests();
    assert_eq!(asked.len(), 2, "{all}");
    assert!(asked[0].body.contains("\"hello\""), "{}", asked[0].body);
    assert!(!asked[0].body.contains("how are you"), "{}", asked[0].body);
    assert!(asked[0].body.contains("Welcome."), "{}", asked[0].body);
    assert!(asked[1].body.contains("Hello there."), "{}", asked[1].body);
    assert!(asked[1].body.contains("how are you"), "{}", asked[1].body);
}

#[tokio::test]
async fn a_chat_that_broke_off_is_gone_on_with() {
    let llm = HttpServer::start(vec![
        answer(&["Noted,", " Ada."], None),
        answer(&["You are", " Ada."], None),
    ])
    .await;
    let desk = desk();
    // What is kept, as the command lists it.
    let kept = |desk: PathBuf| async move {
        let listed = waveport_at(&desk, &["calls"]).await;
        assert_eq!(listed.status.code(), Some(0), "{listed:?}");
        let listed = String::from_utf8(listed.stdout).unwrap();
        listed.lines().map(str::to_owned).collect::<Vec<_>>()
    };
    assert_eq!(
        kept(desk.clone()).await,
        ["no call is kept: there is no .waveport/journal.redb"]
    );
    assert!(!desk.join(".waveport").exists());
    // The command dies as `kill -9` kills it, once its journal has the
    // answer.
    let stored = "after:utterance_committed";
    let mut first = Chatting::start_to_halt(&desk, &llm, &[], Some(stored));
    first.types("my name is Ada");
    first.tells("agent: Noted, Ada.").await;
    first.tells(&format!("halted: {stored}")).await;
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    // The chat is there to go on with, by its name.
    let listed = kept(desk.clone()).await;
    assert_eq!(
        listed[0],
        "1 call that broke off is kept in .waveport/journal.redb"
    );
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(listed[1].len() == 32 && listed[1].bytes().all(|byte| byte.is_ascii_hexdigit()));

    let mut second = Chatting::start(&desk, &llm, &["--resume", "last"]);
    second.types("what is my name");
    second.tells("agent: You are Ada.").await;
    second.stops_typing();
    assert_eq!(second.ended().await, Some(0), "{}", second.all());
    let all = second.all();
    let lines = second.lines();
    assert_eq!(
        chat(&lines)[1..],
        [
            "you: what is my name",
            "agent: You are Ada.",
            "the call ended: it was asked to",
        ],
        "{all}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("the call goes on, from "))
    );

    // The model was shown the chat as the journal had it.
    let asked = llm.requests();
    assert_eq!(asked.len(), 2, "{all}");
    assert!(
        asked[1].body.contains("my name is Ada"),
        "{}",
        asked[1].body
    );
    assert!(asked[1].body.contains("Noted, Ada."), "{}", asked[1].body);
    assert!(
        asked[1].body.contains("what is my name"),
        "{}",
        asked[1].body
    );

    // A chat that is over is not there to go on with.
    assert_eq!(
        kept(desk.clone()).await,
        ["no call is kept in .waveport/journal.redb"]
    );
    let mut third = Chatting::start(&desk, &llm, &["--resume", "last"]);
    third.stops_typing();
    assert_eq!(third.ended().await, Some(1), "{}", third.all());
}

#[tokio::test]
async fn an_answer_the_journal_never_had_is_given_again() {
    let llm = HttpServer::start(vec![
        answer(&["Noted,", " Ada."], None),
        answer(&["As I said:", " noted."], None),
    ])
    .await;
    let desk = desk();
    // Whoever followed the chat read the answer. The command dies before
    // its journal has it.
    let unstored = "before:utterance_committed";
    let mut first = Chatting::start_to_halt(&desk, &llm, &[], Some(unstored));
    first.types("my name is Ada");
    first.tells("agent: Noted, Ada.").await;
    first.tells(&format!("halted: {unstored}")).await;
    first.child.kill().unwrap();
    first.child.wait().unwrap();

    // The chat that goes on is the one the journal has: one in which
    // what was typed has no answer yet. It is answered without anything
    // more being typed.
    let mut second = Chatting::start(&desk, &llm, &["--resume", "last"]);
    second.tells("agent: As I said: noted.").await;
    second.stops_typing();
    assert_eq!(second.ended().await, Some(0), "{}", second.all());
    let all = second.all();
    let asked = llm.requests();
    assert_eq!(asked.len(), 2, "{all}");
    assert!(
        asked[1].body.contains("my name is Ada"),
        "{}",
        asked[1].body
    );
    assert!(!asked[1].body.contains("Noted, Ada."), "{}", asked[1].body);
    // And the model was told that something was lost.
    let restored = "the session was interrupted by a failure and has been restored";
    assert!(asked[1].body.contains(restored), "{}", asked[1].body);
}
