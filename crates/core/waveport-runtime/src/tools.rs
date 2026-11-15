//! The tools of an application, kept in one place.
//!
//! A session is told three things about a tool by three different parts:
//! the kernel what running it does to the world, the language model what
//! it is called and what it takes, and the tool port how to run it. A
//! [`Registry`] holds all three for each tool, in one order, so that they
//! cannot come apart: the number the kernel gives a tool is its place
//! here.
//!
//! A tool is given its arguments as the model wrote them. [`arguments`]
//! reads them into a type, and what it says when they do not fit is said
//! to the model, which then asks again or says that it cannot.

use core::fmt;
use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use serde::Deserialize;
use waveport_domain::config::ToolSpec;
use waveport_domain::event::RetryClass;
use waveport_domain::ids::{ToolCallId, ToolId};
use waveport_ports::error::BoxFuture;
use waveport_ports::journal::SessionId;
use waveport_ports::tool::{ToolExecutor, ToolReply, ToolRequest};

use crate::context::ToolDef;
use crate::session::SessionSpec;

/// How many calls a registry remembers the tool of, for when one of them
/// is to be undone.
const REMEMBERED: usize = 1_024;

/// A call, as a tool is given it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call<'a> {
    /// The session that asks. With `call` it names the call for good: a
    /// tool that is given the same pair again is being asked to do the
    /// same thing, not another.
    pub session: SessionId,
    /// The call.
    pub call: ToolCallId,
    /// The arguments, as the model wrote them.
    pub args: &'a str,
    /// 0 for the first time the call is sent, higher when it is sent
    /// again after the session was restarted.
    pub attempt: u8,
}

/// Something an agent can do besides talk.
pub trait Tool: Send + Sync + 'static {
    /// Runs a call. Dropping the future stops waiting for it, which does
    /// not undo anything the tool has done.
    fn call<'a>(&'a self, call: Call<'a>) -> BoxFuture<'a, ToolReply>;

    /// Undoes a call that was started and may have taken effect. The
    /// answer is whether nothing of it is left: `true` only when the tool
    /// is sure. A tool that does not say otherwise cannot undo anything.
    fn undo(&self, _session: SessionId, _call: ToolCallId) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
}

/// Reads the arguments of a call into what a tool takes.
///
/// A model that calls a tool that takes nothing may write no arguments at
/// all, which is read as an empty object.
///
/// # Errors
///
/// The arguments are not what the tool takes. The error is the answer to
/// the call: it says so to the model, with what was wrong.
pub fn arguments<'a, A: Deserialize<'a>>(args: &'a str) -> Result<A, ToolReply> {
    let args = if args.trim().is_empty() { "{}" } else { args };
    serde_json::from_str(args).map_err(|error| ToolReply::Failed {
        class: RetryClass::Permanent,
        message: format!("The arguments are not what this tool takes: {error}."),
    })
}

struct Entry {
    def: ToolDef,
    spec: ToolSpec,
    tool: Box<dyn Tool>,
}

/// The tools of an application.
#[derive(Default)]
pub struct Registry {
    entries: Vec<Entry>,
    /// The calls that were sent, and the tool of each, the oldest first.
    sent: Mutex<VecDeque<(SessionId, ToolCallId, ToolId)>>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self.entries.iter().map(|entry| entry.def.name.as_str());
        f.debug_list().entries(names).finish()
    }
}

impl Registry {
    /// A registry with no tools.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool, and returns the number the kernel knows it by.
    ///
    /// # Panics
    ///
    /// There are 65,536 tools already.
    pub fn add(&mut self, def: ToolDef, spec: ToolSpec, tool: impl Tool) -> ToolId {
        #[expect(
            clippy::expect_used,
            reason = "an application does not have tens of thousands of tools, and one that \
                      does is told so when it starts"
        )]
        let id = u16::try_from(self.entries.len()).expect("fewer than 65,536 tools");
        self.entries.push(Entry {
            def,
            spec,
            tool: Box::new(tool),
        });
        ToolId(id)
    }

    /// The numbers of the tools, in order.
    #[must_use]
    pub fn ids(&self) -> Vec<ToolId> {
        (0..self.entries.len())
            .filter_map(|n| u16::try_from(n).ok())
            .map(ToolId)
            .collect()
    }

    /// What the tools are called and take, for the language model.
    #[must_use]
    pub fn defs(&self) -> Vec<ToolDef> {
        self.entries.iter().map(|entry| entry.def.clone()).collect()
    }

    /// What running each tool does to the world, for the kernel.
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.entries.iter().map(|entry| entry.spec).collect()
    }

    /// Writes these tools into the specification of a session, and lets
    /// every agent of it call every one of them. Tools the specification
    /// had are replaced.
    pub fn offer(&self, spec: &mut SessionSpec) {
        spec.kernel.tools = self.specs();
        spec.tools = self.defs();
        for agent in &mut spec.kernel.agents {
            agent.tools = self.ids();
        }
    }

    fn sent(&self) -> std::sync::MutexGuard<'_, VecDeque<(SessionId, ToolCallId, ToolId)>> {
        self.sent.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ToolExecutor for Registry {
    fn call<'a>(&'a self, request: ToolRequest<'a>) -> BoxFuture<'a, ToolReply> {
        let Some(entry) = self.entries.get(usize::from(request.tool.0)) else {
            let message = format!("There is no tool with the number {}.", request.tool.0);
            return Box::pin(async move {
                ToolReply::Failed {
                    class: RetryClass::Permanent,
                    message,
                }
            });
        };
        {
            let mut sent = self.sent();
            if sent.len() >= REMEMBERED {
                sent.pop_front();
            }
            sent.push_back((request.session, request.call, request.tool));
        }
        entry.tool.call(Call {
            session: request.session,
            call: request.call,
            args: request.args,
            attempt: request.attempt,
        })
    }

    fn undo(&self, session: SessionId, call: ToolCallId) -> BoxFuture<'_, bool> {
        let tool = self
            .sent()
            .iter()
            .rev()
            .find(|(of, sent, _)| *of == session && *sent == call)
            .map(|(_, _, tool)| *tool);
        let entry = tool.and_then(|tool| self.entries.get(usize::from(tool.0)));
        match entry {
            Some(entry) => entry.tool.undo(session, call),
            // A call that was never sent left nothing behind. One that is
            // not remembered any more may have.
            None => Box::pin(async { false }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde::Deserialize;
    use waveport_domain::config::{Effect, Gate, ToolSpec};
    use waveport_domain::event::RetryClass;
    use waveport_domain::ids::{ToolCallId, ToolId};
    use waveport_ports::error::BoxFuture;
    use waveport_ports::journal::SessionId;
    use waveport_ports::tool::{ToolExecutor, ToolReply, ToolRequest};

    use super::{Call, Registry, Tool, arguments};
    use crate::context::ToolDef;
    use crate::session::SessionSpec;

    #[derive(Deserialize)]
    struct Sum {
        a: i64,
        b: i64,
    }

    struct Adds;

    impl Tool for Adds {
        fn call<'a>(&'a self, call: Call<'a>) -> BoxFuture<'a, ToolReply> {
            Box::pin(async move {
                match arguments::<Sum>(call.args) {
                    Ok(sum) => ToolReply::Completed((sum.a + sum.b).to_string()),
                    Err(reply) => reply,
                }
            })
        }
    }

    /// A tool that books something, and can take a booking back.
    struct Books(Arc<AtomicU32>);

    impl Tool for Books {
        fn call<'a>(&'a self, _call: Call<'a>) -> BoxFuture<'a, ToolReply> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { ToolReply::Completed("booked".to_owned()) })
        }

        fn undo(&self, _session: SessionId, _call: ToolCallId) -> BoxFuture<'_, bool> {
            self.0.fetch_sub(1, Ordering::Relaxed);
            Box::pin(async { true })
        }
    }

    fn def(name: &str) -> ToolDef {
        ToolDef {
            name: name.to_owned(),
            description: format!("What {name} does."),
            parameters: r#"{"type":"object","properties":{}}"#.to_owned(),
        }
    }

    fn registry() -> (Registry, Arc<AtomicU32>) {
        let booked = Arc::new(AtomicU32::new(0));
        let mut registry = Registry::new();
        let adds = registry.add(def("add"), ToolSpec::new(Effect::Pure), Adds);
        let books = ToolSpec {
            gate: Gate::AfterOwnUtterance,
            ..ToolSpec::new(Effect::SideEffecting)
        };
        let books = registry.add(def("book"), books, Books(Arc::clone(&booked)));
        assert_eq!((adds, books), (ToolId(0), ToolId(1)));
        (registry, booked)
    }

    fn request(tool: u16, call: u64, args: &str) -> ToolRequest<'_> {
        ToolRequest {
            session: SessionId(1),
            call: ToolCallId(call),
            tool: ToolId(tool),
            args,
            attempt: 0,
        }
    }

    #[tokio::test]
    async fn a_call_goes_to_the_tool_of_its_number() {
        let (registry, booked) = registry();
        assert_eq!(
            registry.call(request(0, 1, r#"{"a":2,"b":40}"#)).await,
            ToolReply::Completed("42".to_owned())
        );
        assert_eq!(
            registry.call(request(1, 2, "")).await,
            ToolReply::Completed("booked".to_owned())
        );
        assert_eq!(booked.load(Ordering::Relaxed), 1);

        let ToolReply::Failed { class, message } = registry.call(request(9, 3, "{}")).await else {
            panic!("there is no tool 9");
        };
        assert_eq!(class, RetryClass::Permanent);
        assert!(message.contains('9'), "{message}");
    }

    #[tokio::test]
    async fn arguments_that_do_not_fit_are_said_to_the_model() {
        let (registry, _) = registry();
        let ToolReply::Failed { class, message } =
            registry.call(request(0, 1, r#"{"a":"two"}"#)).await
        else {
            panic!("a word is not a number");
        };
        assert_eq!(class, RetryClass::Permanent);
        assert!(message.starts_with("The arguments are not what this tool takes"));
        // No arguments at all are an empty object, for a tool that takes
        // none, and still not enough for one that takes two.
        assert_eq!(
            arguments::<serde_json::Value>("  ").unwrap().to_string(),
            "{}"
        );
        assert!(arguments::<Sum>("").is_err());
    }

    #[tokio::test]
    async fn a_call_is_undone_by_the_tool_it_went_to() {
        let (registry, booked) = registry();
        registry.call(request(1, 5, "{}")).await;
        registry.call(request(0, 6, r#"{"a":1,"b":1}"#)).await;
        assert_eq!(booked.load(Ordering::Relaxed), 1);
        // The tool that books takes it back. The one that adds has
        // nothing to say, and neither has anyone about a call that was
        // never sent, or one of another session.
        assert!(registry.undo(SessionId(1), ToolCallId(5)).await);
        assert_eq!(booked.load(Ordering::Relaxed), 0);
        assert!(!registry.undo(SessionId(1), ToolCallId(6)).await);
        assert!(!registry.undo(SessionId(1), ToolCallId(7)).await);
        assert!(!registry.undo(SessionId(2), ToolCallId(5)).await);
        assert_eq!(booked.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_session_is_told_of_the_tools_in_their_order() {
        let (registry, _) = registry();
        let mut spec = SessionSpec::new(SessionId(1), "Be brief.");
        registry.offer(&mut spec);
        assert_eq!(spec.kernel.validate(), Ok(()));
        assert_eq!(spec.tools.len(), spec.kernel.tools.len());
        assert_eq!(spec.tools[1].name, "book");
        assert_eq!(spec.kernel.tools[1].gate, Gate::AfterOwnUtterance);
        assert_eq!(spec.kernel.agents[0].tools, [ToolId(0), ToolId(1)]);
        assert_eq!(format!("{registry:?}"), r#"["add", "book"]"#);
    }
}
