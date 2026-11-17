//! The tools an agent has.
//!
//! One only looks: it tells the time. Another acts, and once: it ends
//! the call. Between them they are the two kinds of tool a session has to
//! get right, and something to ask an agent for that it cannot make up.
//!
//! An agent that is given a file for notes has a third, which acts where
//! it can be seen afterwards: it adds a line to the file. What a call
//! did to the world, and how often, can be counted there.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Weak;

use waveport_domain::config::{Effect, Gate, ToolSpec};
use waveport_domain::event::RetryClass;
use waveport_ports::control::{SessionControl, SessionInput};
use waveport_ports::error::BoxFuture;
use waveport_ports::tool::ToolReply;
use waveport_runtime::context::ToolDef;
use waveport_runtime::tools::{Call, Registry, Tool};

/// A tool that takes no arguments, as the model is told.
const NOTHING: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;

/// A tool that takes a text, as the model is told.
const A_TEXT: &str = r#"{"type":"object","properties":{"text":{"type":"string","description":"What to write down, in one line."}},"required":["text"],"additionalProperties":false}"#;

/// The tool that takes notes: where it writes them, and whether it is to
/// stop once it has written, for the test that kills the program there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notes {
    /// The file a note is a line of.
    pub file: PathBuf,
    /// Whether the tool stops once it has written, and never answers.
    pub halt: bool,
}

/// Adds the tools to `registry`. `sessions` is where the session that
/// asks for its call to be ended is found, and `notes` is the tool that
/// takes notes, if the agent has one. The order is the order the tools
/// have in a call's journal: a new tool goes last.
pub fn add(registry: &mut Registry, sessions: Weak<dyn SessionControl>, notes: Option<Notes>) {
    registry.add(
        ToolDef {
            name: "get_time".to_owned(),
            description: "Tells the time and the date now, where the agent is.".to_owned(),
            parameters: NOTHING.to_owned(),
        },
        ToolSpec::new(Effect::Pure),
        GetTime,
    );
    registry.add(
        ToolDef {
            name: "end_call".to_owned(),
            description: "Ends the call. Say goodbye in the same answer: the call ends once \
                          you have finished speaking."
                .to_owned(),
            parameters: NOTHING.to_owned(),
        },
        // Not before the goodbye has been heard, and not twice.
        ToolSpec {
            gate: Gate::AfterOwnUtterance,
            ..ToolSpec::new(Effect::SideEffecting)
        },
        EndCall { sessions },
    );
    if let Some(notes) = notes {
        registry.add(
            ToolDef {
                name: "take_note".to_owned(),
                description: "Writes a note down where the caller finds it after the call."
                    .to_owned(),
                parameters: A_TEXT.to_owned(),
            },
            // It is not taken back, and a second one is a second note.
            ToolSpec::new(Effect::SideEffecting),
            TakeNote(notes),
        );
    }
}

/// Tells the time.
struct GetTime;

impl Tool for GetTime {
    fn call<'a>(&'a self, _call: Call<'a>) -> BoxFuture<'a, ToolReply> {
        Box::pin(async { ToolReply::Completed(now(&jiff::Zoned::now())) })
    }
}

/// What the model is told the time is.
fn now(now: &jiff::Zoned) -> String {
    let zone = now.time_zone().iana_name().unwrap_or("local time");
    serde_json::json!({
        "time": now.strftime("%H:%M").to_string(),
        "date": now.strftime("%A, %-d %B %Y").to_string(),
        "time_zone": zone,
    })
    .to_string()
}

/// Ends the call it is called in.
struct EndCall {
    sessions: Weak<dyn SessionControl>,
}

impl Tool for EndCall {
    fn call<'a>(&'a self, call: Call<'a>) -> BoxFuture<'a, ToolReply> {
        let asked = self
            .sessions
            .upgrade()
            .map(|sessions| sessions.send(call.session, SessionInput::Close));
        Box::pin(async move {
            // The session takes what it is asked before it takes what its
            // tools answer. By not answering in the same breath, the tool
            // lets the session learn that it is closing first: it then
            // does not ask the model what to say to "the call is ending".
            tokio::task::yield_now().await;
            match asked {
                Some(Ok(())) => ToolReply::Completed("The call is ending.".to_owned()),
                // The session has too much waiting for it: the same call
                // may be tried again, and ends the call once.
                Some(Err(error)) => ToolReply::Failed {
                    class: RetryClass::Transient,
                    message: format!("The call could not be ended: {error}."),
                },
                None => ToolReply::Failed {
                    class: RetryClass::Permanent,
                    message: "The call could not be ended: nothing runs it any more.".to_owned(),
                },
            }
        })
    }
}

/// Writes a note down.
struct TakeNote(Notes);

/// The text of the note that the arguments of a call ask for, in one
/// line.
fn note(args: &str) -> Result<String, String> {
    let asked: serde_json::Value = serde_json::from_str(args)
        .map_err(|error| format!("The arguments are not what the tool takes: {error}."))?;
    let text = asked
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map_or_else(String::new, |text| {
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        });
    if text.is_empty() {
        return Err("There is nothing to write down: the tool takes a text.".to_owned());
    }
    Ok(text)
}

/// Adds `line` to the end of the file at `file`.
fn add_line(file: &Path, line: &str) -> std::io::Result<()> {
    let mut notes = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(file)?;
    // In one write, so that two notes at once are two lines.
    notes.write_all(format!("{line}\n").as_bytes())?;
    notes.sync_all()
}

impl Tool for TakeNote {
    fn call<'a>(&'a self, call: Call<'a>) -> BoxFuture<'a, ToolReply> {
        Box::pin(async move {
            let text = match note(call.args) {
                Ok(text) => text,
                Err(message) => {
                    return ToolReply::Failed {
                        class: RetryClass::Permanent,
                        message,
                    };
                }
            };
            if let Err(error) = add_line(&self.0.file, &text) {
                return ToolReply::Failed {
                    class: RetryClass::Permanent,
                    message: format!("The note could not be written down: {error}."),
                };
            }
            if self.0.halt {
                crate::halt::say("tool");
                core::future::pending::<()>().await;
            }
            ToolReply::Completed("It is written down.".to_owned())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, Weak};

    use waveport_domain::config::{Effect, Gate};
    use waveport_domain::ids::{Seq, ToolCallId, ToolId};
    use waveport_ports::control::{ControlError, EventRx, SessionControl, SessionInput};
    use waveport_ports::journal::SessionId;
    use waveport_ports::tool::{ToolExecutor, ToolReply, ToolRequest};
    use waveport_runtime::tools::Registry;

    use super::{Notes, add, note, now};

    /// Sessions that note what is asked of them.
    #[derive(Default)]
    struct Sessions {
        asked: Mutex<Vec<(SessionId, SessionInput)>>,
    }

    impl SessionControl for Sessions {
        fn send(&self, session: SessionId, input: SessionInput) -> Result<(), ControlError> {
            if session == SessionId(404) {
                return Err(ControlError::NoSuchSession);
            }
            self.asked.lock().unwrap().push((session, input));
            Ok(())
        }

        fn subscribe(
            &self,
            _session: SessionId,
            _from: Seq,
        ) -> Result<core::pin::Pin<Box<dyn EventRx>>, ControlError> {
            Err(ControlError::NoSuchSession)
        }
    }

    fn request(session: u128, tool: u16) -> ToolRequest<'static> {
        ToolRequest {
            session: SessionId(session),
            call: ToolCallId(1),
            tool: ToolId(tool),
            args: "{}",
            attempt: 0,
        }
    }

    fn tools(sessions: &Arc<Sessions>) -> Registry {
        let mut registry = Registry::new();
        let sessions: Arc<dyn SessionControl> = Arc::clone(sessions) as _;
        add(&mut registry, Arc::downgrade(&sessions), None);
        registry
    }

    #[test]
    fn the_tools_are_what_the_kernel_is_told_they_are() {
        let registry = tools(&Arc::default());
        let (defs, specs) = (registry.defs(), registry.specs());
        assert_eq!(defs[0].name, "get_time");
        assert_eq!((specs[0].effect, specs[0].gate), (Effect::Pure, Gate::None));
        assert_eq!(defs[1].name, "end_call");
        assert_eq!(
            (specs[1].effect, specs[1].gate),
            (Effect::SideEffecting, Gate::AfterOwnUtterance)
        );
        for def in &defs {
            let schema: serde_json::Value = serde_json::from_str(&def.parameters).unwrap();
            assert_eq!(schema["type"], "object");
        }
    }

    #[tokio::test]
    async fn the_time_is_told_as_a_person_would_say_it() {
        let registry = tools(&Arc::default());
        let ToolReply::Completed(told) = registry.call(request(1, 0)).await else {
            panic!("the time can always be told");
        };
        let told: serde_json::Value = serde_json::from_str(&told).unwrap();
        assert_eq!(told["time"].as_str().unwrap().len(), 5);

        let then: jiff::Zoned = "2026-11-05T09:07:30-05:00[America/Lima]".parse().unwrap();
        let told: serde_json::Value = serde_json::from_str(&now(&then)).unwrap();
        assert_eq!(told["time"], "09:07");
        assert_eq!(told["date"], "Thursday, 5 November 2026");
        assert_eq!(told["time_zone"], "America/Lima");
    }

    #[tokio::test]
    async fn the_call_that_asks_is_the_call_that_ends() {
        let sessions = Arc::new(Sessions::default());
        let registry = tools(&sessions);
        assert_eq!(
            registry.call(request(7, 1)).await,
            ToolReply::Completed("The call is ending.".to_owned())
        );
        assert_eq!(
            *sessions.asked.lock().unwrap(),
            [(SessionId(7), SessionInput::Close)]
        );
        // A session that is not there, and sessions that are not there.
        let reply = registry.call(request(404, 1)).await;
        assert!(matches!(reply, ToolReply::Failed { .. }), "{reply:?}");
        let mut alone = Registry::new();
        add(&mut alone, Weak::<Sessions>::new(), None);
        let reply = alone.call(request(7, 1)).await;
        assert!(matches!(reply, ToolReply::Failed { .. }), "{reply:?}");
    }

    #[tokio::test]
    async fn a_note_is_a_line_in_the_file_and_two_are_two() {
        let folder = std::env::temp_dir().join(format!("waveport-cli-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("notes.txt");
        let _ = std::fs::remove_file(&file);
        let mut registry = Registry::new();
        let notes = Notes {
            file: file.clone(),
            halt: false,
        };
        add(&mut registry, Weak::<Sessions>::new(), Some(notes));
        // It is the last of the tools, and one that acts.
        let (defs, specs) = (registry.defs(), registry.specs());
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[2].name, "take_note");
        assert_eq!(
            (specs[2].effect, specs[2].gate),
            (Effect::SideEffecting, Gate::None)
        );
        let schema: serde_json::Value = serde_json::from_str(&defs[2].parameters).unwrap();
        assert_eq!(schema["required"][0], "text");

        let asks = |args: &'static str| ToolRequest {
            args,
            ..request(1, 2)
        };
        let reply = registry
            .call(asks(r#"{"text":"The door\n is open."}"#))
            .await;
        assert_eq!(
            reply,
            ToolReply::Completed("It is written down.".to_owned())
        );
        let reply = registry.call(asks(r#"{"text":"So is the window."}"#)).await;
        assert_eq!(
            reply,
            ToolReply::Completed("It is written down.".to_owned())
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "The door is open.\nSo is the window.\n"
        );

        // What is not a note is not written.
        for args in ["{}", r#"{"text":"  "}"#, r#"{"text":7}"#, "not json"] {
            let reply = registry.call(asks(args)).await;
            assert!(
                matches!(reply, ToolReply::Failed { .. }),
                "{args}: {reply:?}"
            );
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 2);
        assert_eq!(note(r#"{"text":" a  b "}"#), Ok("a b".to_owned()));
    }
}
