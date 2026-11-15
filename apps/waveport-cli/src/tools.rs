//! The two tools an agent of this command has.
//!
//! One only looks: it tells the time. The other acts, and once: it ends
//! the call. Between them they are the two kinds of tool a session has to
//! get right, and something to ask an agent for that it cannot make up.

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

/// Adds the two tools to `registry`. `sessions` is where the session that
/// asks for its call to be ended is found.
pub(crate) fn add(registry: &mut Registry, sessions: Weak<dyn SessionControl>) {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, Weak};

    use waveport_domain::config::{Effect, Gate};
    use waveport_domain::ids::{Seq, ToolCallId, ToolId};
    use waveport_ports::control::{ControlError, EventRx, SessionControl, SessionInput};
    use waveport_ports::journal::SessionId;
    use waveport_ports::tool::{ToolExecutor, ToolReply, ToolRequest};
    use waveport_runtime::tools::Registry;

    use super::{add, now};

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
        add(&mut registry, Arc::downgrade(&sessions));
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
        add(&mut alone, Weak::<Sessions>::new());
        let reply = alone.call(request(7, 1)).await;
        assert!(matches!(reply, ToolReply::Failed { .. }), "{reply:?}");
    }
}
