//! The daemon, from the command line to what a client is told.
//!
//! The daemon is run as anyone would run it, `waveportd --config`, and
//! is asked over HTTP as an application and a far end would ask it. Its
//! agents are the ones of a call: the same session runtime, the same
//! language model adapter, the same tools, the same journal. The
//! language model is a server on this machine whose answers are made
//! up, in the shape of the ones that were recorded for the adapter's own
//! tests.
//!
//! `lined` has the far end on a WebSocket, and `aloud` has it speak
//! there: that one needs the models that listen
//! (`cargo xtask fetch-assets`), and stand-ins for the providers that
//! recognise and synthesise speech.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

mod far_end;

mod aloud;
mod asked;
mod called;
mod dialled;
mod kept;
mod lined;
mod page;
mod placed;

use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use waveport_protocol::client::v1::{Applied, FromServer, View};
use waveport_testkit::servers::{HttpAnswer, HttpServer};
use waveport_testkit::stand_in::{answer, slow_answer, unfinished};

use crate::far_end::{Answer, Event, Events, ask};

/// What the language model writes when it is asked the time.
const ANSWER: [&str; 3] = ["It is", " ten past three.", " Anything else?"];

/// An answer that stops after its first words and is never finished.
fn stops_short() -> HttpAnswer {
    unfinished(&ANSWER[..1])
}

/// A folder for a daemon to be run in, which nothing else is run in.
fn desk() -> PathBuf {
    static DESKS: AtomicU32 = AtomicU32::new(0);
    let number = DESKS.fetch_add(1, Ordering::Relaxed);
    let desk = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("desks/{}-{number}", std::process::id()));
    let _ = std::fs::remove_dir_all(&desk);
    std::fs::create_dir_all(&desk).unwrap();
    desk
}

/// The configuration of a daemon with one agent, `desk`, whose language
/// model is `llm`. `more` is what it says besides, before its agents.
fn config(llm: &HttpServer, more: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n{more}\n\
         [agents.desk]\n\
         greeting = \"Good morning.\"\n\
         [agents.desk.llm]\n\
         url = \"{}\"\n\
         model = \"a-model\"\n",
        llm.url("/v1")
    )
}

/// Keeps the lines that come through `pipe`. Returns them, and the
/// thread that reads them, which ends when the pipe is closed.
fn lines_of(
    pipe: impl std::io::Read + Send + 'static,
) -> (Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let keeping = Arc::clone(&kept);
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            keeping.lock().unwrap().push(line);
        }
    });
    (kept, reader)
}

/// The daemon, started in a folder, with what it has to be told.
fn command(desk: &Path, config: &str, variables: &[(&str, &str)]) -> Command {
    std::fs::write(desk.join("waveportd.toml"), config).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_waveportd"));
    command
        .args(["--config", "waveportd.toml"])
        .current_dir(desk)
        // No key of anyone's is needed for a server on this machine,
        // and none is to reach it.
        .env_remove("OPENAI_API_KEY")
        .env_remove("CARTESIA_API_KEY")
        .env_remove("WAVEPORT_TEST_HALT")
        .envs(variables.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// A daemon that runs.
struct Served {
    child: Child,
    /// Where it listens.
    address: String,
    told: Arc<Mutex<Vec<String>>>,
    errors: Arc<Mutex<Vec<String>>>,
    /// The threads that read what it says.
    readers: Vec<JoinHandle<()>>,
}

impl Served {
    /// Starts a daemon in the folder `desk` and waits until it says
    /// where it listens.
    async fn start(desk: &Path, config: &str, variables: &[(&str, &str)]) -> Self {
        let mut child = command(desk, config, variables).spawn().unwrap();
        let (told, told_by) = lines_of(child.stdout.take().unwrap());
        let (errors, errors_by) = lines_of(child.stderr.take().unwrap());
        let began = Instant::now();
        let address = loop {
            let listening = told.lock().unwrap().iter().find_map(|line| {
                line.strip_prefix("waveportd: listening on http://")
                    .map(str::to_owned)
            });
            if let Some(address) = listening {
                break address;
            }
            let gone = child.try_wait().unwrap().is_some();
            assert!(
                !gone && began.elapsed() < Duration::from_secs(30),
                "the daemon did not start: {:#?}",
                errors.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        Self {
            child,
            address,
            told,
            errors,
            readers: vec![told_by, errors_by],
        }
    }

    /// Waits until the daemon says that it has stopped at `place`.
    async fn halted_at(&self, place: &str) {
        let halted = format!("halted: {place}");
        let began = Instant::now();
        while !self.told.lock().unwrap().contains(&halted) {
            assert!(
                began.elapsed() < Duration::from_secs(30),
                "the daemon did not stop at {place}: {}",
                self.errors()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// What the daemon said to whoever runs it, as far as it has been
    /// read. What a daemon says is read by a thread of its own, which
    /// may not have got to all of it: a test that looks for something
    /// the daemon said waits for it (`noted`), or for the daemon to be
    /// over.
    fn errors(&self) -> String {
        self.errors.lock().unwrap().join("\n")
    }

    /// What the daemon said to whoever runs it, once it has said `what`.
    async fn noted(&self, what: &str) -> String {
        let began = Instant::now();
        loop {
            let errors = self.errors();
            if errors.contains(what) {
                return errors;
            }
            assert!(
                began.elapsed() < Duration::from_secs(30),
                "the daemon did not say {what:?}: {errors}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Reads what a daemon that is over said, to its end: its pipes are
    /// closed, and the threads that read them end.
    fn read_to_the_end(&mut self) {
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }

    /// Asks the daemon as whoever is on its machine and says nothing of
    /// who they are.
    async fn ask(&self, method: &str, path: &str, body: Option<&str>) -> Answer {
        ask(&self.address, method, path, &[], body).await
    }

    /// Asks the daemon as whoever holds `token`.
    async fn ask_as(&self, token: &str, method: &str, path: &str, body: Option<&str>) -> Answer {
        let bearer = format!("Bearer {token}");
        let headers = [("Authorization", bearer.as_str())];
        ask(&self.address, method, path, &headers, body).await
    }

    /// Makes a session of the agent `desk`. Returns its name and the
    /// token its far end connects with.
    async fn session(&self) -> (String, String) {
        let made = self
            .ask("POST", "/v1/sessions", Some(r#"{"agent":"desk"}"#))
            .await;
        assert_eq!(made.status, 201, "{made:?}");
        let made = made.json();
        let name = made["session"].as_str().unwrap().to_owned();
        let token = made["client_token"].as_str().unwrap().to_owned();
        (name, token)
    }

    /// Follows the session `name` as whoever holds `token`, or as
    /// whoever says nothing of who they are. `more` is what the request
    /// says besides.
    async fn follow(
        &self,
        name: &str,
        query: &str,
        token: Option<&str>,
        more: &[(&str, &str)],
    ) -> Result<Events, Answer> {
        let bearer = token.map(|token| format!("Bearer {token}"));
        let mut headers = more.to_vec();
        if let Some(bearer) = &bearer {
            headers.push(("Authorization", bearer));
        }
        let path = format!("/v1/sessions/{name}/events{query}");
        Events::open(&self.address, &path, &headers).await
    }

    /// Types `text` into the session `name` as whoever holds `token`.
    async fn types(&self, name: &str, token: &str, text: &str) {
        let body = serde_json::json!({"type": "text", "text": text}).to_string();
        let path = format!("/v1/sessions/{name}/input");
        let answer = self.ask_as(token, "POST", &path, Some(&body)).await;
        assert_eq!(answer.status, 202, "{answer:?}");
    }

    /// Stops the daemon where it is, as a machine that loses power
    /// does.
    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.read_to_the_end();
    }

    /// Tells the daemon to stop, as whoever runs it does.
    #[cfg(unix)]
    fn tell_to_stop(&self) {
        let told = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(told.success());
    }

    /// Waits until the daemon has stopped, and says how.
    #[cfg(unix)]
    async fn ended(&mut self) -> ExitStatus {
        let began = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.read_to_the_end();
                return status;
            }
            assert!(
                began.elapsed() < Duration::from_secs(30),
                "the daemon did not stop: {}",
                self.errors()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs a daemon that is not to start, and returns how it ended and
/// what it said.
fn refused_to_start(config: &str, variables: &[(&str, &str)]) -> (ExitStatus, String) {
    let mut child = command(&desk(), config, variables).spawn().unwrap();
    // One that starts after all is not waited for: it says little, so
    // what it says can be left unread until it is over.
    let patience = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > patience {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("a daemon that was to refuse to start did not: {config}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let done = child.wait_with_output().unwrap();
    let said = String::from_utf8(done.stderr).unwrap();
    assert_eq!(String::from_utf8(done.stdout).unwrap(), "", "{said}");
    (done.status, said)
}

/// Whether the event is the fact of the kind `kind`.
fn is_fact(event: &Event, kind: &str) -> bool {
    event.kind == "fact" && event.json()["kind"] == kind
}

/// What a client holds of a session after `events`, from what it held
/// before them.
fn viewed(mut view: View, events: &[Event]) -> View {
    for event in events {
        let message = event.message();
        let applied = view.apply(&message);
        match message {
            // A stream sends no fact twice.
            FromServer::Fact { .. } => assert_eq!(applied, Applied::Fact, "{event:?}"),
            FromServer::State(_) => assert_eq!(applied, Applied::State),
            _ => {}
        }
    }
    view
}
