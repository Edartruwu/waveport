//! What the agents of one process share, an agent made on it, and a
//! session begun.

use core::pin::Pin;
use core::time::Duration;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use waveport_domain::ids::Seq;
use waveport_domain::policy::DefaultPolicies;
use waveport_llm_openai_compat::OpenAiCompat;
use waveport_ports::control::{ControlError, EventRx, SessionControl, SessionInput};
use waveport_ports::journal::SessionId;
use waveport_ports::plan::SessionPlan;
use waveport_ports::stt::Stt;
use waveport_ports::telemetry::EventSink;
use waveport_ports::transport::Transport;
use waveport_ports::tts::Tts;
use waveport_runtime::compute::Compute;
use waveport_runtime::session::{Brain, Ended, Parts, Rebuilt, SessionSpec, StateRule};
use waveport_runtime::shard::{Exit, Runtime, RuntimeConfig, Started, Status};
use waveport_runtime::tools::Registry;
use waveport_telemetry_otel::{Config as OtelConfig, Otel};

use crate::halt::Halt;
use crate::kept::{Kept, Resume};
use crate::spec::{AgentSpec, INSTRUCTIONS, Named, StackSpec, WRITTEN};
use crate::tools::Notes;
use crate::{Context, Failure, providers, tools};

/// What a session tells whoever follows it.
pub type Told = Pin<Box<dyn EventRx>>;

/// A name for a session that no other session has had.
#[must_use]
pub fn name() -> SessionId {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    SessionId(now ^ (u128::from(std::process::id()) << 96))
}

/// What an agent hears and speaks with, and the language it does so in.
struct Speech {
    stt: Arc<dyn Stt>,
    tts: Arc<dyn Tts>,
    compute: Arc<Compute>,
    language: String,
}

/// An agent, made from its definition: its providers are there to be
/// asked. One serves every session of the agent.
pub struct Agent {
    llm: Arc<OpenAiCompat>,
    /// What it hears and speaks with. An agent without only reads and
    /// writes.
    speech: Option<Speech>,
    /// What it is told, if its definition says. Otherwise it is told
    /// what suits how its session is run.
    instructions: Option<String>,
    greeting: Option<String>,
    /// What whoever runs the program should know about the agent before
    /// any session of it: a provider that could not be reached yet.
    pub notes: Vec<String>,
}

impl Agent {
    /// Whether the agent hears and speaks where a line carries audio.
    #[must_use]
    pub const fn speaks(&self) -> bool {
        self.speech.is_some()
    }
}

/// What the agents of one process share: the runtime their sessions run
/// on, the models that listen, the tools, the journal, and what sends
/// traces and metrics.
pub struct Stack {
    runtime: Arc<Runtime>,
    tools: Arc<Registry>,
    /// What the tools are called, in the kernel's order.
    names: Vec<String>,
    /// Where sessions are kept.
    pub kept: Kept,
    /// What whoever runs the program should know before any session.
    pub notes: Vec<String>,
    states: StateRule,
    /// What sends traces and metrics to a collector, and where the
    /// collector is.
    otel: Option<(Arc<Otel>, String)>,
    assets: PathBuf,
    named: Named,
    /// The models that listen, once an agent that hears has needed them.
    compute: OnceLock<Arc<Compute>>,
}

/// A session that has begun.
pub struct Call {
    /// Its name.
    pub id: SessionId,
    /// What the session will do less well than it could with better
    /// parts.
    pub notes: Vec<String>,
    /// What the session was rebuilt from, if it goes on after it broke
    /// off.
    pub rebuilt: Option<Rebuilt>,
    /// Whether the agent was given something to say as it began.
    pub greeted: bool,
    /// Whether it was begun as one to go on with.
    pub resumed: bool,
    started: Started,
}

impl Call {
    /// How the session is run with the parts it has: hearing and
    /// speaking, or reading and writing.
    #[must_use]
    pub const fn plan(&self) -> &SessionPlan {
        &self.started.plan
    }

    /// Which of its lives the session is in. The number goes up each
    /// time a session is gone on with after it broke off, and whoever
    /// was told of it in an earlier life was told of what this one may
    /// never have done.
    #[must_use]
    pub const fn life(&self) -> u64 {
        self.started.lease.0
    }

    /// How the session ends.
    pub async fn exit(self) -> Exit {
        self.started.exit().await
    }
}

/// What became of what was kept of a session that has left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settled {
    /// It was over, and was let go of. If it could not be taken out of
    /// where it was kept, this says why.
    Forgotten(Option<String>),
    /// It broke off and is kept: it can be gone on with.
    Kept,
    /// It broke off and was kept in memory only: it is gone when the
    /// process is.
    Gone,
}

impl Stack {
    /// Puts together what the agents of this process share, as `spec`
    /// says. `resuming` is whether a session is to be gone on with,
    /// which the sessions that were kept are needed for.
    ///
    /// # Errors
    ///
    /// Something cannot be had: the file sessions are kept in, a thread
    /// to run them on, or the collector's address.
    pub async fn build(spec: &StackSpec, resuming: bool) -> Result<Self, Failure> {
        let halt = Halt::from_env().map_err(Failure)?;
        // First, so that a file of sessions that cannot be used is said
        // before anything is waited for.
        let (kept, mut notes) = Kept::open(&spec.keeps, &spec.named, &halt, resuming)?;
        notes.extend(kept.earlier().await);
        let config = RuntimeConfig {
            shards: spec.shards,
            ..RuntimeConfig::default()
        };
        let runtime = Arc::new(Runtime::new(config).with(|| "no session can run".to_owned())?);
        let mut registry = Registry::new();
        let sessions: Weak<Runtime> = Arc::downgrade(&runtime);
        let takes_notes = spec.notes.clone().map(|file| Notes {
            file,
            halt: halt.in_tool(),
        });
        tools::add(&mut registry, sessions, takes_notes);
        let names: Vec<String> = registry.defs().into_iter().map(|def| def.name).collect();
        let otel = match &spec.otlp {
            Some(url) => {
                let config = OtelConfig {
                    tools: names.clone(),
                    ..OtelConfig::new(url)
                };
                let otel = Otel::new(config).with(|| {
                    format!(
                        "traces and metrics cannot be sent as {} says",
                        spec.named.otlp
                    )
                })?;
                notes.push(format!("traces and metrics of the calls go to {url}"));
                Some((Arc::new(otel), url.clone()))
            }
            None => None,
        };
        Ok(Self {
            runtime,
            tools: Arc::new(registry),
            names,
            kept,
            notes,
            states: StateRule {
                records: spec.state_every.max(1),
                ..StateRule::default()
            },
            otel,
            assets: spec.assets.clone(),
            named: spec.named,
            compute: OnceLock::new(),
        })
    }

    /// The models that listen, loaded when the first agent that hears
    /// needs them and shared from then on.
    fn compute(&self) -> Result<Arc<Compute>, Failure> {
        if let Some(compute) = self.compute.get() {
            return Ok(Arc::clone(compute));
        }
        let compute = Arc::new(providers::models(&self.assets, &self.named)?);
        Ok(Arc::clone(self.compute.get_or_init(|| compute)))
    }

    /// Makes the agent `spec` defines: reads what it is told, finds the
    /// keys of its providers, and has a connection made ahead to the one
    /// that speaks.
    ///
    /// # Errors
    ///
    /// The file of instructions cannot be read, a provider's own address
    /// has no key, the models that listen are not where they should be,
    /// or a provider cannot be asked as the definition says.
    pub async fn agent(&self, spec: &AgentSpec) -> Result<Agent, Failure> {
        let instructions = match &spec.instructions_file {
            Some(file) => Some(
                std::fs::read_to_string(file)
                    .with(|| format!("the instructions in {} cannot be read", file.display()))?,
            ),
            None => spec.instructions.clone(),
        };
        let llm_key = providers::llm_key(&spec.llm)?;
        let hears = match &spec.speech {
            Some(speech) => Some((providers::recogniser(&speech.stt)?, self.compute()?)),
            None => None,
        };
        let llm = providers::llm(&spec.llm, llm_key)?;
        let mut notes = Vec::new();
        let speech = match (&spec.speech, hears) {
            (Some(speech), Some((stt, compute))) => {
                let (tts, unreached) = providers::synthesiser(speech, &self.named).await?;
                notes.extend(unreached);
                Some(Speech {
                    stt,
                    tts,
                    compute,
                    language: speech.language.clone(),
                })
            }
            _ => None,
        };
        Ok(Agent {
            llm: Arc::new(llm),
            speech,
            instructions,
            greeting: spec.greeting.clone(),
            notes,
        })
    }

    /// What the tools are called, in the kernel's order.
    #[must_use]
    pub fn tools(&self) -> Vec<String> {
        self.names.clone()
    }

    /// What is told of every session to the collector, if there is one.
    #[must_use]
    pub fn collector(&self) -> Option<Arc<dyn EventSink>> {
        self.otel
            .as_ref()
            .map(|(otel, _)| Arc::clone(otel) as Arc<dyn EventSink>)
    }

    /// Sends what is still to be sent of the traces and the metrics. For
    /// the end of the program. Returns what whoever runs it should know,
    /// if any of them did not reach the collector.
    pub async fn finish(&self) -> Option<String> {
        let (otel, url) = self.otel.as_ref()?;
        let stopping = Arc::clone(otel);
        // It waits for the collector, which a thread that runs sessions
        // does not do.
        let sent = tokio::task::spawn_blocking(move || stopping.shutdown()).await;
        let Ok(sent) = sent else {
            return Some(format!(
                "the traces and the metrics could not be sent to {url}"
            ));
        };
        sent.lost_any().then(|| {
            let why = sent.error.as_deref().unwrap_or("nobody said why");
            format!(
                "{} spans and the metrics, {} times, did not reach {url}: {why}",
                sent.spans_lost, sent.metrics_lost
            )
        })
    }

    /// Begins a session of `agent` called `id`, or goes on with it if it
    /// is one that broke off.
    ///
    /// How the session is run is picked by its line. Over a transport
    /// that carries audio, an agent that hears and speaks does. Over one
    /// that carries none, or over no transport at all, the agent reads
    /// and writes.
    ///
    /// `sinks` are told what the session does, besides the collector.
    /// With `followed`, what the session tells is returned from its
    /// beginning here on: that is how what an agent writes is read.
    ///
    /// # Errors
    ///
    /// The session cannot be started with the parts it has, or cannot be
    /// followed, or its greeting cannot be said.
    pub async fn start(
        &self,
        agent: &Agent,
        id: SessionId,
        transport: Option<Box<dyn Transport>>,
        mut sinks: Vec<Arc<dyn EventSink>>,
        resumed: bool,
        followed: bool,
    ) -> Result<(Call, Option<Told>), Failure> {
        sinks.extend(self.collector());
        let aloud = transport
            .as_ref()
            .is_some_and(|line| line.caps().audio.is_some());
        let speech = agent.speech.as_ref().filter(|_| aloud);
        let usual = if speech.is_some() {
            INSTRUCTIONS
        } else {
            WRITTEN
        };
        let instructions = agent.instructions.as_deref().unwrap_or(usual);
        let mut spec = SessionSpec::new(id, instructions);
        spec.language = speech.map(|speech| speech.language.clone());
        spec.states = self.states;
        self.tools.offer(&mut spec);
        let brain = match speech {
            Some(speech) => Brain::Cascaded {
                stt: speech.stt.clone(),
                llm: agent.llm.clone(),
                tts: speech.tts.clone(),
                compute: Arc::clone(&speech.compute),
            },
            None => Brain::Text {
                llm: agent.llm.clone(),
            },
        };
        let parts = Parts {
            brain,
            transport,
            tools: self.tools.clone(),
            journal: self.kept.journal(),
            sink: (!sinks.is_empty()).then(|| Arc::new(sinks) as Arc<dyn EventSink>),
            taps: Vec::new(),
        };
        let started = self
            .runtime
            .start(spec, parts, DefaultPolicies::default())
            .await
            .with(|| "the call could not be started".to_owned())?;
        let notes = started
            .plan
            .shortfalls()
            .iter()
            .map(ToString::to_string)
            .collect();
        let rebuilt = started.rebuilt;
        // Whoever follows is there before anything is said: from the
        // first fact of a new session, and from the first that a session
        // which goes on did not already hold.
        let told = if followed {
            let held = rebuilt.map_or(0, |from| {
                from.state.map_or(0, Seq::get).saturating_add(from.records)
            });
            let from = Seq(held.saturating_add(1));
            Some(
                self.runtime
                    .subscribe(id, from)
                    .with(|| "the call cannot be followed".to_owned())?,
            )
        } else {
            None
        };
        // A session that goes on has been greeted.
        let greeting = agent.greeting.as_ref().filter(|_| rebuilt.is_none());
        if let Some(greeting) = greeting {
            let text = greeting.clone();
            self.runtime
                .send(id, SessionInput::Say { text })
                .with(|| "the greeting could not be said".to_owned())?;
        }
        let call = Call {
            id,
            notes,
            rebuilt,
            greeted: greeting.is_some(),
            resumed,
            started,
        };
        Ok((call, told))
    }

    /// Follows the session `id` from the fact numbered `from`.
    #[must_use]
    pub fn follow(&self, id: SessionId, from: Seq) -> Option<Told> {
        self.runtime.subscribe(id, from).ok()
    }

    /// Follows the session `id` from what it is now: its state first,
    /// and then what happens from there. For whoever cannot go on from
    /// what they hold of it.
    #[must_use]
    pub fn follow_from_state(&self, id: SessionId) -> Option<Told> {
        self.runtime.subscribe_from_state(id).ok()
    }

    /// Asks `input` of the session `id`. Returns whether it was taken.
    #[must_use]
    pub fn ask(&self, id: SessionId, input: SessionInput) -> bool {
        self.runtime.send(id, input).is_ok()
    }

    /// Asks `input` of the session `id`.
    ///
    /// # Errors
    ///
    /// The session does not run here, or has too much waiting for it.
    pub fn send(&self, id: SessionId, input: SessionInput) -> Result<(), ControlError> {
        self.runtime.send(id, input)
    }

    /// Where the session `id` stood when it last ran, if it runs here.
    #[must_use]
    pub fn status(&self, id: SessionId) -> Option<Status> {
        self.runtime.status(id)
    }

    /// Whether every session that runs here has nothing under way.
    #[must_use]
    pub fn at_rest(&self) -> bool {
        self.runtime.at_rest()
    }

    /// Starts no session from now on, and waits until the ones that run
    /// have ended, for as long as `patience`. Returns how many still
    /// run: they stop where they are when the stack is let go of, and
    /// can be gone on with if they are kept where that can be done.
    pub async fn drain(&self, patience: Duration) -> usize {
        self.runtime.drain(patience).await
    }

    /// The name of the session to run: the one `resume` says to go on
    /// with, or a new one, which is then the one that [`Resume::Last`]
    /// finds.
    ///
    /// # Errors
    ///
    /// There is no such session to go on with, or where sessions are
    /// kept cannot be written to.
    pub async fn call_to_run(&self, resume: Option<&Resume>) -> Result<SessionId, Failure> {
        if let Some(from) = resume {
            return self.kept.resume(from).await;
        }
        let id = name();
        if let Some(token) = self.kept.issue(id).await? {
            self.kept.remember(&token)?;
        }
        Ok(id)
    }

    /// Deals with what is kept of the session `id` once it has left as
    /// `exit` says: a session that is over is let go of, and one that
    /// broke off is there to go on with, if sessions are kept where that
    /// can be done.
    pub async fn settle(&self, id: SessionId, exit: &Exit) -> Settled {
        if matches!(exit, Exit::Ended(Ended::Closed)) {
            Settled::Forgotten(self.kept.forget(id).await.err().map(|failure| failure.0))
        } else if self.kept.outlasts_the_program() {
            Settled::Kept
        } else {
            Settled::Gone
        }
    }
}
