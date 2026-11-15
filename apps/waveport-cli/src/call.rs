//! The parts an agent is made of, put together, and a call run on them.

use core::time::Duration;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use waveport_adapter_kit::secret::Secret;
use waveport_domain::policy::DefaultPolicies;
use waveport_llm_openai_compat::{Config as LlmConfig, Dialect, OpenAiCompat};
use waveport_models_onnx::{Runtime as Models, SileroVad, SmartTurn, TURN_MODEL, VAD_MODEL};
use waveport_ports::control::{SessionControl, SessionInput};
use waveport_ports::journal::SessionId;
use waveport_ports::stt::Stt;
use waveport_ports::transport::Transport;
use waveport_ports::tts::Tts;
use waveport_runtime::compute::Compute;
use waveport_runtime::session::{Ended, Parts, Rebuilt, SessionSpec, StateRule};
use waveport_runtime::shard::{Exit, Runtime, RuntimeConfig, Started};
use waveport_runtime::tools::Registry;
use waveport_stt_assemblyai::AssemblyAiStt;
use waveport_stt_deepgram::DeepgramStt;
use waveport_stt_openai::OpenAiStt;
use waveport_transport_local_audio::{Health, LocalAudio};
use waveport_tts_cartesia::CartesiaTts;
use waveport_tts_elevenlabs::ElevenLabsTts;

use crate::args::{Agent, Chosen, Hears, INSTRUCTIONS, LLM_URL, Resume, Speaks};
use crate::halt::Halt;
use crate::kept::Kept;
use crate::report::{Reporter, Tell};
use crate::tools::Notes;
use crate::{Context, Failure, tools};

/// How long a call that was asked to end has to do so before the command
/// stops waiting for it.
const LAST_WORDS: Duration = Duration::from_secs(15);

/// How a call went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It ended, and everything it decided was recorded.
    Ended,
    /// It broke off: its journal failed, or it panicked.
    Broken,
}

/// The key in the environment variable `variable`, for the provider at
/// `url`. A provider's own address needs one; a server somewhere else
/// may not.
fn key(variable: &str, url: &str, provider: &str) -> Result<Option<Secret>, Failure> {
    match std::env::var(variable) {
        Ok(key) if !key.trim().is_empty() => Ok(Some(Secret::new(key.trim()))),
        _ if url == provider => Err(Failure(format!(
            "the environment variable {variable} holds no key, and {provider} needs one"
        ))),
        _ => Ok(None),
    }
}

/// The key of a provider that was chosen on the command line.
fn key_of(chosen: &Chosen) -> Result<Option<Secret>, Failure> {
    let own = if chosen.own { chosen.url.as_str() } else { "" };
    key(&chosen.key_env, &chosen.url, own)
}

/// The part that recognises speech.
fn recogniser(agent: &Agent) -> Result<Arc<dyn Stt>, Failure> {
    let chosen = agent.hears();
    let key = key_of(&chosen)?;
    let Chosen { url, model, .. } = chosen;
    Ok(match agent.stt {
        Hears::OpenAi => Arc::new(OpenAiStt::new(waveport_stt_openai::Config {
            url,
            ..waveport_stt_openai::Config::new(&model, key)
        })),
        Hears::Deepgram => Arc::new(DeepgramStt::new(waveport_stt_deepgram::Config {
            url,
            ..waveport_stt_deepgram::Config::new(&model, key)
        })),
        Hears::AssemblyAi => Arc::new(AssemblyAiStt::new(waveport_stt_assemblyai::Config {
            url,
            ..waveport_stt_assemblyai::Config::new(&model, key)
        })),
    })
}

/// The part that synthesises speech, with a connection made ahead: the
/// first answer does not then wait for one to be made. If none can be
/// made the call says so itself, at its first answer.
async fn synthesiser(agent: &Agent) -> Result<Arc<dyn Tts>, Failure> {
    let chosen = agent.speaks();
    let key = key_of(&chosen)?;
    let Chosen {
        url, model, voice, ..
    } = chosen;
    let (tts, warmed): (Arc<dyn Tts>, _) = match agent.tts {
        Speaks::Cartesia => {
            let tts = Arc::new(CartesiaTts::new(waveport_tts_cartesia::Config {
                url,
                language: agent.language.clone(),
                ..waveport_tts_cartesia::Config::new(&model, &voice, key)
            }));
            let warmed = tts.warm().await;
            (tts, warmed)
        }
        Speaks::ElevenLabs => {
            let config = waveport_tts_elevenlabs::Config {
                url,
                ..waveport_tts_elevenlabs::Config::new(&model, &voice, key)
            };
            let tts = ElevenLabsTts::new(config)
                .with(|| "speech cannot be synthesised as the command line says".to_owned())?;
            let tts = Arc::new(tts);
            let warmed = tts.warm().await;
            (tts, warmed)
        }
    };
    if let Err(error) = warmed {
        eprintln!("waveport: speech synthesis could not be reached yet: {error}");
    }
    Ok(tts)
}

/// What the agent is told before the call.
fn instructions(agent: &Agent) -> Result<String, Failure> {
    if let Some(file) = &agent.instructions_file {
        return std::fs::read_to_string(file)
            .with(|| format!("the instructions in {} cannot be read", file.display()));
    }
    Ok(agent
        .instructions
        .clone()
        .unwrap_or_else(|| INSTRUCTIONS.to_owned()))
}

/// The models that listen, from the files in `assets`.
fn models(assets: &Path) -> Result<Compute, Failure> {
    let hint = || {
        format!(
            "the models that listen are not in {}: fetch them with `cargo xtask fetch-assets`, \
             or say where they are with --assets",
            assets.display()
        )
    };
    let models = Models::load_from(assets).with(hint)?;
    let speech = SileroVad::load(models, &assets.join(VAD_MODEL)).with(hint)?;
    let turns = SmartTurn::load(models, &assets.join(TURN_MODEL)).with(hint)?;
    Compute::spawn(speech, Box::new(turns), None).with(|| "the models cannot be run".to_owned())
}

/// A name for a call that no other call has had.
pub(crate) fn name() -> SessionId {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    SessionId(now ^ (u128::from(std::process::id()) << 96))
}

/// What every call of this command is made of: the runtime, the models
/// that listen, the providers, the tools and the journal.
pub(crate) struct Stack {
    runtime: Arc<Runtime>,
    stt: Arc<dyn Stt>,
    llm: Arc<OpenAiCompat>,
    tts: Arc<dyn Tts>,
    tools: Arc<Registry>,
    /// Where calls are kept.
    pub kept: Kept,
    /// What whoever ran the command should know before any call.
    pub notes: Vec<String>,
    compute: Arc<Compute>,
    instructions: String,
    language: String,
    greeting: Option<String>,
    states: StateRule,
}

/// A call that has begun.
pub(crate) struct Call {
    /// Its name.
    pub id: SessionId,
    /// What the call will do less well than it could with better parts.
    pub notes: Vec<String>,
    /// What the call was rebuilt from, if it goes on after it broke off.
    pub rebuilt: Option<Rebuilt>,
    /// Whether it was begun as one to go on with.
    resumed: bool,
    started: Started,
}

impl Call {
    /// What is said of the call when it begins.
    pub(crate) fn begins(&self) -> String {
        let records = |count: u64| if count == 1 { "record" } else { "records" };
        match self.rebuilt {
            Some(Rebuilt {
                state: Some(upto),
                records: after,
            }) => format!(
                "the call goes on, from what it had come to by record {} and {after} {} after \
                 that",
                upto.get(),
                records(after)
            ),
            Some(Rebuilt {
                state: None,
                records: all,
            }) => format!("the call goes on, from its {all} {}", records(all)),
            None if self.resumed => {
                "the call has begun: nothing of it was stored before it broke off".to_owned()
            }
            None => "the call has begun".to_owned(),
        }
    }

    /// How the call ends.
    pub(crate) async fn exit(self) -> Exit {
        self.started.exit().await
    }
}

impl Stack {
    /// Puts the parts together as `agent` says.
    pub(crate) async fn build(agent: &Agent) -> Result<Self, Failure> {
        let halt = Halt::from_env().map_err(Failure)?;
        // First, so that a file of calls that cannot be used is said
        // before anything is waited for.
        let (kept, mut notes) = Kept::open(agent, &halt)?;
        notes.extend(kept.earlier().await);
        let instructions = instructions(agent)?;
        let llm_key = key(&agent.llm_key_env, &agent.llm_url, LLM_URL)?;
        let stt = recogniser(agent)?;

        let compute = models(&agent.assets)?;
        // What only OpenAI itself understands is asked of nobody else.
        let llm = if agent.llm_url == LLM_URL {
            LlmConfig::openai(&agent.llm_model, llm_key)
        } else {
            LlmConfig {
                key: llm_key,
                dialect: Dialect::Generic,
                ..LlmConfig::generic(&agent.llm_url, &agent.llm_model)
            }
        };
        let llm =
            OpenAiCompat::new(llm).with(|| "the language model cannot be reached".to_owned())?;
        let tts = synthesiser(agent).await?;

        let runtime = Arc::new(
            Runtime::new(RuntimeConfig::default()).with(|| "no session can run".to_owned())?,
        );
        let mut registry = Registry::new();
        let sessions: Weak<Runtime> = Arc::downgrade(&runtime);
        let takes_notes = agent.notes.clone().map(|file| Notes {
            file,
            halt: halt.in_tool(),
        });
        tools::add(&mut registry, sessions, takes_notes);
        Ok(Self {
            runtime,
            stt,
            llm: Arc::new(llm),
            tts,
            tools: Arc::new(registry),
            kept,
            notes,
            compute: Arc::new(compute),
            instructions,
            language: agent.language.clone(),
            greeting: agent.greeting.clone(),
            states: StateRule {
                records: agent.state_every.max(1),
                ..StateRule::default()
            },
        })
    }

    /// Begins a call called `id` over `transport`, or goes on with it if
    /// `resumed` says that it is one that broke off. Every line about it
    /// is given to `tell`, on the call's own thread.
    pub(crate) async fn start(
        &self,
        id: SessionId,
        transport: Box<dyn Transport>,
        tell: Tell,
        resumed: bool,
    ) -> Result<Call, Failure> {
        let mut spec = SessionSpec::new(id, &self.instructions);
        spec.language = Some(self.language.clone());
        spec.states = self.states;
        self.tools.offer(&mut spec);
        let names = spec.tools.iter().map(|tool| tool.name.clone()).collect();
        let parts = Parts {
            transport,
            stt: self.stt.clone(),
            llm: self.llm.clone(),
            tts: self.tts.clone(),
            tools: self.tools.clone(),
            journal: self.kept.journal(),
            compute: Arc::clone(&self.compute),
            sink: Some(Arc::new(Reporter::new(tell, names))),
        };
        let started = self
            .runtime
            .start(spec, parts, DefaultPolicies::default())
            .await
            .with(|| "the call could not be started".to_owned())?;
        let notes = started
            .plan
            .shortfalls
            .iter()
            .map(ToString::to_string)
            .collect();
        // A call that goes on has been greeted.
        let rebuilt = started.rebuilt;
        if let Some(greeting) = self.greeting.as_ref().filter(|_| rebuilt.is_none()) {
            let text = greeting.clone();
            self.runtime
                .send(id, SessionInput::Say { text })
                .with(|| "the greeting could not be said".to_owned())?;
        }
        Ok(Call {
            id,
            notes,
            rebuilt,
            resumed,
            started,
        })
    }

    /// The name of the call to run: the one `resume` says to go on with,
    /// or a new one, which is then the one that `--resume last` finds.
    pub(crate) async fn call_to_run(&self, resume: Option<&Resume>) -> Result<SessionId, Failure> {
        if let Some(from) = resume {
            return self.kept.resume(from).await;
        }
        let id = name();
        if let Some(token) = self.kept.issue(id).await? {
            self.kept.remember(&token)?;
        }
        Ok(id)
    }

    /// Deals with what is kept of the call `id` once it has left as
    /// `exit` says: a call that is over is let go of, and one that broke
    /// off is there to go on with. `how` is how whoever ran the command
    /// goes on with it, if they can.
    pub(crate) async fn settle(&self, id: SessionId, exit: &Exit, how: Option<&str>) {
        if matches!(exit, Exit::Ended(Ended::Closed)) {
            self.kept.forget(id).await;
        } else if let Some(how) = how.filter(|_| self.kept.outlasts_the_command()) {
            eprintln!("waveport: the call is kept: {how} goes on with it");
        }
    }

    /// Asks the call `id` to end: the agent finishes what it is saying.
    pub(crate) fn ask_to_end(&self, id: SessionId) {
        let _ = self.runtime.send(id, SessionInput::Close);
    }
}

/// How a call went, from how its session left.
pub(crate) fn outcome(exit: &Exit) -> Outcome {
    match exit {
        Exit::Ended(Ended::Closed) => Outcome::Ended,
        Exit::Ended(Ended::Lost(broken)) => {
            eprintln!("waveport: the call broke off: {broken:?}");
            Outcome::Broken
        }
        Exit::Panicked => {
            eprintln!("waveport: the call broke off: its session panicked");
            Outcome::Broken
        }
        Exit::Stopped => {
            eprintln!("waveport: the call was stopped where it was");
            Outcome::Broken
        }
    }
}

/// Runs one call over `transport`, and tells it on the terminal as it
/// goes. The call is a new one, or the one `resume` says to go on with.
pub(crate) async fn run(
    agent: &Agent,
    transport: LocalAudio,
    resume: Option<&Resume>,
) -> Result<Outcome, Failure> {
    let health = transport.health();
    let stack = Stack::build(agent).await?;
    for note in &stack.notes {
        println!("note: {note}");
    }
    let id = stack.call_to_run(resume).await?;
    let (lines, mut printed) = mpsc::unbounded_channel();
    let tell = Box::new(move |line| {
        // Nobody reads any more when the command is on its way out.
        let _ = lines.send(line);
    });
    let call = stack
        .start(id, Box::new(transport), tell, resume.is_some())
        .await?;
    for note in &call.notes {
        println!("note: {note}");
    }
    println!("{}", call.begins());

    let id = call.id;
    let exit = call.exit();
    tokio::pin!(exit);
    let mut asked_to_end = false;
    let exit = loop {
        tokio::select! {
            exit = &mut exit => break exit,
            Some(line) = printed.recv() => println!("{line}"),
            _ = tokio::signal::ctrl_c(), if !asked_to_end => {
                asked_to_end = true;
                eprintln!("waveport: ending the call. Ctrl-C again stops it where it is.");
                stack.ask_to_end(id);
            }
            () = last_words(asked_to_end) => break Exit::Stopped,
        }
    };
    while let Ok(line) = printed.try_recv() {
        println!("{line}");
    }
    tell_health(&health);
    stack.settle(id, &exit, Some("--resume last")).await;
    Ok(outcome(&exit))
}

/// Waits until a call that was asked to end has had its time, or until
/// it is told again to stop. Never returns for a call nobody has asked to
/// end.
pub(crate) async fn last_words(asked_to_end: bool) {
    if !asked_to_end {
        return core::future::pending().await;
    }
    tokio::select! {
        () = tokio::time::sleep(LAST_WORDS) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Says what went wrong with the sound devices, if anything did.
pub(crate) fn tell_health(health: &Health) {
    if let Some(failure) = health.failure() {
        eprintln!("waveport: {failure}");
    }
    let (stumbles, lost) = (health.stumbles(), health.lost());
    if stumbles > 0 || lost > 0 {
        eprintln!(
            "waveport: the sound devices fell behind {stumbles} times, and {} ms of the \
             microphone were lost",
            lost / 48
        );
    }
}
