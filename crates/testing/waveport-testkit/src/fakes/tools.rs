//! Tools that follow a script.

use core::fmt;
use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use waveport_domain::event::RetryClass;
use waveport_domain::ids::{ToolCallId, ToolId};
use waveport_ports::error::BoxFuture;
use waveport_ports::journal::SessionId;
use waveport_ports::tool::{ToolExecutor, ToolReply, ToolRequest};

/// A call a scripted executor was given, kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Called {
    /// The session that asked.
    pub session: SessionId,
    /// The call.
    pub call: ToolCallId,
    /// The tool.
    pub tool: ToolId,
    /// The arguments.
    pub args: String,
    /// Which time the call was sent.
    pub attempt: u8,
}

type Run = dyn Fn(&Called) -> ToolReply + Send + Sync;

/// Tools that follow a script.
#[derive(Clone)]
pub struct ScriptedTools {
    /// How long a call takes.
    pub latency: Duration,
    /// Whether a call that is undone leaves nothing behind.
    pub undoes: bool,
    run: Arc<Run>,
    called: Arc<Mutex<Vec<Called>>>,
    undone: Arc<Mutex<Vec<ToolCallId>>>,
}

impl fmt::Debug for ScriptedTools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptedTools")
            .field("latency", &self.latency)
            .finish_non_exhaustive()
    }
}

impl ScriptedTools {
    /// Tools that answer each call with what `run` makes of it.
    #[must_use]
    pub fn with(run: impl Fn(&Called) -> ToolReply + Send + Sync + 'static) -> Self {
        Self {
            latency: Duration::from_millis(150),
            undoes: false,
            run: Arc::new(run),
            called: Arc::default(),
            undone: Arc::default(),
        }
    }

    /// Tools that fail every call: for a session that should make none.
    #[must_use]
    pub fn none() -> Self {
        Self::with(|called| ToolReply::Failed {
            class: RetryClass::Permanent,
            message: format!("there is no tool {}", called.tool.0),
        })
    }

    /// The calls it was given so far.
    #[must_use]
    pub fn called(&self) -> Vec<Called> {
        self.called
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The calls it was asked to undo so far.
    #[must_use]
    pub fn undone(&self) -> Vec<ToolCallId> {
        self.undone
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ToolExecutor for ScriptedTools {
    fn call<'a>(&'a self, request: ToolRequest<'a>) -> BoxFuture<'a, ToolReply> {
        let called = Called {
            session: request.session,
            call: request.call,
            tool: request.tool,
            args: request.args.to_owned(),
            attempt: request.attempt,
        };
        self.called
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(called.clone());
        Box::pin(async move {
            tokio::time::sleep(self.latency).await;
            (self.run)(&called)
        })
    }

    fn undo(&self, _session: SessionId, call: ToolCallId) -> BoxFuture<'_, bool> {
        self.undone
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
        Box::pin(async move {
            tokio::time::sleep(self.latency).await;
            self.undoes
        })
    }
}
