//! What the tests of tools and of recovery share: a kernel with a clock, a
//! desk of tools, and a journal that keeps what the kernel emits.

use core::time::Duration;

use waveport_domain::command::{ClientEvent, Command, LlmCmd, ToolCmd, TtsCmd};
use waveport_domain::config::{AgentSpec, Effect, Executor, Gate, KernelConfig, ToolSpec};
use waveport_domain::event::{ControlInput, Event, Finish, OpEvent, ToolEvent, VadEdge};
use waveport_domain::fact::{Fact, Stamped, ToolOutcome};
use waveport_domain::ids::{AgentId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId};
use waveport_domain::kernel::{ItemView, Kernel, Snapshot};
use waveport_domain::out::Out;
use waveport_domain::policy::BoxedPolicies;
use waveport_domain::text::{TextArena, TextRef};
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

pub(crate) const ANA: ParticipantId = ParticipantId(1);

/// Changes nothing: can be cancelled and repeated.
pub(crate) const LOOKUP: ToolId = ToolId(0);
/// Must happen at most once.
pub(crate) const REFUND: ToolId = ToolId(1);
/// Must happen at most once, and only after the agent has said what it
/// was saying.
pub(crate) const HANG_UP: ToolId = ToolId(2);
/// Changes something, and repeating it changes nothing more.
pub(crate) const SYNC: ToolId = ToolId(3);
/// Answered by the agent [`RESEARCHER`].
pub(crate) const RESEARCH: ToolId = ToolId(4);

pub(crate) const FRONT: AgentId = AgentId(0);
/// Works on calls to [`RESEARCH`] and never speaks.
pub(crate) const RESEARCHER: AgentId = AgentId(1);
/// Can be given the floor.
pub(crate) const BILLING: AgentId = AgentId(2);

/// A session with the five tools and three agents above.
pub(crate) fn desk() -> KernelConfig {
    let tools = vec![
        ToolSpec::new(Effect::Pure),
        ToolSpec::new(Effect::SideEffecting),
        ToolSpec {
            gate: Gate::AfterOwnUtterance,
            ..ToolSpec::new(Effect::SideEffecting)
        },
        ToolSpec::new(Effect::Idempotent),
        ToolSpec {
            executor: Executor::Agent(RESEARCHER),
            timeout: Duration::from_secs(30),
            ..ToolSpec::new(Effect::Pure)
        },
    ];
    let all = vec![LOOKUP, REFUND, HANG_UP, SYNC, RESEARCH];
    KernelConfig {
        tools,
        agents: vec![
            AgentSpec { tools: all.clone() },
            AgentSpec::default(),
            AgentSpec { tools: all },
        ],
        ..KernelConfig::default()
    }
}

/// What one step emitted, with its text resolved.
#[derive(Debug, Default)]
pub(crate) struct Emitted {
    pub(crate) cmds: Vec<(Command, String)>,
    pub(crate) facts: Vec<(Stamped, String)>,
}

impl Emitted {
    pub(crate) fn has(&self, command: Command) -> bool {
        self.cmds.iter().any(|(emitted, _)| *emitted == command)
    }

    pub(crate) fn facts(&self) -> Vec<Fact> {
        self.facts.iter().map(|(stamped, _)| stamped.fact).collect()
    }

    /// The facts, each with the text it carries.
    pub(crate) fn told(&self) -> Vec<(Fact, &str)> {
        self.facts
            .iter()
            .map(|(stamped, text)| (stamped.fact, text.as_str()))
            .collect()
    }

    /// The place in the journal of the first fact that `matches`.
    pub(crate) fn seq_of(&self, matches: impl Fn(&Fact) -> bool) -> Option<Seq> {
        self.facts
            .iter()
            .find(|(stamped, _)| matches(&stamped.fact))
            .map(|(stamped, _)| stamped.seq)
    }

    /// The response started in this step, and the command that started
    /// it.
    pub(crate) fn started(&self) -> Option<(OpTag, LlmCmd)> {
        self.cmds.iter().find_map(|(command, _)| match command {
            Command::Llm(tag, start @ LlmCmd::Start { .. }) => Some((*tag, *start)),
            _ => None,
        })
    }

    /// The calls sent to their tools in this step: which call, which
    /// tool, with what arguments, and which attempt it is.
    pub(crate) fn sent(&self) -> Vec<(ToolCallId, ToolId, &str, u8)> {
        self.cmds
            .iter()
            .filter_map(|(command, text)| match *command {
                Command::Tool(call, ToolCmd::Dispatch { tool, attempt, .. }) => {
                    Some((call, tool, text.as_str(), attempt))
                }
                _ => None,
            })
            .collect()
    }

    /// How the calls that ended in this step ended, and what the language
    /// model is told.
    pub(crate) fn ended(&self) -> Vec<(ToolCallId, ToolOutcome, &str)> {
        self.facts
            .iter()
            .filter_map(|(stamped, text)| match stamped.fact {
                Fact::ToolFinished { call, outcome, .. } => Some((call, outcome, text.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Whether anything was handed to speech synthesis.
    pub(crate) fn speaks(&self) -> bool {
        self.cmds
            .iter()
            .any(|(command, _)| matches!(command, Command::Tts(_, TtsCmd::Speak { .. })))
    }

    /// Whether no call was sent to its tool.
    pub(crate) fn sends_nothing(&self) -> bool {
        self.sent().is_empty()
    }

    /// Whether no call ended.
    pub(crate) fn ends_nothing(&self) -> bool {
        self.ended().is_empty()
    }

    /// Whether nothing was asked of the runtime.
    pub(crate) fn asks_nothing(&self) -> bool {
        self.cmds.is_empty()
    }

    pub(crate) fn is_silent(&self) -> bool {
        self.asks_nothing() && self.facts.is_empty()
    }
}

/// Everything a kernel has journaled, with the text kept.
#[derive(Debug, Default)]
pub(crate) struct Journal {
    pub(crate) facts: Vec<Stamped>,
    pub(crate) text: TextArena,
}

impl Journal {
    fn keep(&mut self, out: &Out) {
        for stamped in &out.facts {
            let fact = match stamped.fact.text() {
                Some(at) => stamped.fact.with_text(self.text.push(out.text.get(at))),
                None => stamped.fact,
            };
            self.facts.push(Stamped {
                seq: stamped.seq,
                fact,
            });
        }
    }

    /// The facts from `from` on.
    pub(crate) fn since(&self, from: Seq) -> &[Stamped] {
        let start = self
            .facts
            .iter()
            .position(|stamped| stamped.seq >= from)
            .unwrap_or(self.facts.len());
        self.facts.get(start..).unwrap_or(&[])
    }
}

/// A kernel, the clock it is stepped with, and its journal.
pub(crate) struct Session {
    pub(crate) kernel: Kernel<BoxedPolicies>,
    pub(crate) journal: Journal,
    /// What the session was before its first fact.
    pub(crate) origin: Snapshot,
    config: KernelConfig,
    out: Out,
    now_ms: u64,
}

#[expect(
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "helpers of tests: a failed unwrap fails the test, and each call builds its event in place"
)]
impl Session {
    /// A session at the desk, with Ana in it.
    pub(crate) fn new() -> Self {
        Self::with(desk())
    }

    pub(crate) fn with(config: KernelConfig) -> Self {
        let kernel = Kernel::new(config.clone(), BoxedPolicies::default()).unwrap();
        let mut session = Self {
            origin: kernel.snapshot(),
            kernel,
            journal: Journal::default(),
            config,
            out: Out::new(),
            now_ms: 0,
        };
        session.step(Event::Control(ControlInput::Join { who: ANA }));
        session
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub(crate) fn step(&mut self, event: Event) -> Emitted {
        let now = MonoTime::from_millis(self.now_ms);
        self.kernel.step(now, &event, &mut self.out);
        self.journal.keep(&self.out);
        let text = |at: Option<TextRef>| at.map_or("", |at| self.out.text.get(at)).to_owned();
        Emitted {
            cmds: self
                .out
                .cmds
                .iter()
                .map(|command| (*command, text(command.text())))
                .collect(),
            facts: self
                .out
                .facts
                .iter()
                .map(|stamped| (*stamped, text(stamped.fact.text())))
                .collect(),
        }
    }

    /// Steps at `millis` on the clock.
    pub(crate) fn at(&mut self, millis: u64, event: Event) -> Emitted {
        self.now_ms = millis;
        self.step(event)
    }

    /// Steps `millis` later than the last step.
    pub(crate) fn after(&mut self, millis: u64, event: Event) -> Emitted {
        self.at(self.now_ms + millis, event)
    }

    pub(crate) fn control(&mut self, input: ControlInput) -> Emitted {
        self.step(Event::Control(input))
    }

    pub(crate) fn op(&mut self, tag: OpTag, op: OpEvent) -> Emitted {
        self.step(Event::Op(tag, op))
    }

    /// `who` says `text` in one second from now on, and every adapter
    /// reports it in the usual order. Returns what the last report, the
    /// transcript, led to.
    pub(crate) fn says(&mut self, who: ParticipantId, text: &str) -> Emitted {
        let from = self.now_ms + 100;
        let to = from + 1_000;
        let edge = |edge, at| Event::Speech {
            who,
            edge,
            at: MediaTime::from_millis(at),
        };
        let started = self.at(from, edge(VadEdge::Start, from));
        let turn = started
            .cmds
            .iter()
            .find_map(|(command, _)| match command {
                Command::Emit(ClientEvent::UserSpeaking { turn, .. }) => Some(*turn),
                _ => None,
            })
            .unwrap();
        self.at(to, edge(VadEdge::End, to));
        self.at(to + 60, Event::Eou { turn, p: Prob::ONE });
        let span = MediaSpan::new(MediaTime::from_millis(from), MediaTime::from_millis(to));
        self.at(
            to + 200,
            Event::Transcript {
                who,
                span,
                text: text.into(),
                is_final: true,
            },
        )
    }

    /// Ana asks something and the agent starts to answer. Returns the tag
    /// of the response.
    pub(crate) fn asked(&mut self) -> OpTag {
        self.says(ANA, "can you help me").started().unwrap().0
    }

    /// The language model serving `tag` writes `text`.
    pub(crate) fn writes(&mut self, tag: OpTag, text: &str) -> Emitted {
        self.op(tag, OpEvent::LlmDelta { text: text.into() })
    }

    /// The language model serving `tag` asks for a call to `tool`.
    pub(crate) fn proposes(&mut self, tag: OpTag, tool: ToolId, args: &str) -> Emitted {
        let args = args.into();
        self.op(tag, OpEvent::ToolProposed { tool, args })
    }

    /// The language model serving `tag` finishes.
    pub(crate) fn done(&mut self, tag: OpTag, finish: Finish) -> Emitted {
        self.op(tag, OpEvent::LlmDone { finish })
    }

    /// Speech synthesis delivers all the audio of `tag`, whose text is
    /// `bytes` long, and the listener starts to hear it.
    pub(crate) fn voiced(&mut self, tag: OpTag, bytes: usize) {
        let text_end = u32::try_from(bytes).unwrap();
        for last in [false, true] {
            let segment = OpEvent::TtsSegment {
                text_end,
                audio_end: MediaDur::from_millis(2_000),
                last,
            };
            self.op(tag, segment);
        }
        self.op(tag, played(200, 0, false));
    }

    /// The response `tag`, whose text is `bytes` long, plays to its end.
    pub(crate) fn played_out(&mut self, tag: OpTag, bytes: usize) -> Emitted {
        let heard = u32::try_from(bytes).unwrap();
        self.op(tag, played(2_000, heard, true))
    }

    /// The agent says `text` in answer to Ana, to the end, having asked
    /// for nothing.
    pub(crate) fn exchange(&mut self, text: &str) {
        let tag = self.asked();
        self.writes(tag, text);
        self.done(tag, Finish::Stop);
        self.voiced(tag, text.len());
        self.played_out(tag, text.len());
    }

    /// The tool running `call` returns `result`.
    pub(crate) fn returns(&mut self, call: ToolCallId, result: &str) -> Emitted {
        let result = result.into();
        self.step(Event::Tool(call, ToolEvent::Completed { result }))
    }

    /// Every fact up to `seq` is stored.
    pub(crate) fn stored(&mut self, seq: Seq) -> Emitted {
        self.step(Event::Durable(seq))
    }

    /// The conversation, one line per item.
    pub(crate) fn items(&self) -> Vec<String> {
        lines(&self.kernel)
    }

    /// The process dies. A session is rebuilt from `snapshot` and the
    /// facts journaled after it, up to and including `stored`; later ones
    /// never reached the disk. The clock goes on.
    pub(crate) fn crash(&self, snapshot: &Snapshot, stored: Seq) -> Self {
        let mut journal = Journal::default();
        let mut out = Out::new();
        for stamped in &self.journal.facts {
            if stamped.seq > stored {
                break;
            }
            out.clear();
            let fact = match stamped.fact.text() {
                Some(at) => stamped
                    .fact
                    .with_text(out.text.push(self.journal.text.get(at))),
                None => stamped.fact,
            };
            out.facts.push(Stamped {
                seq: stamped.seq,
                fact,
            });
            journal.keep(&out);
        }
        let kernel = Kernel::restore(
            self.config.clone(),
            BoxedPolicies::default(),
            snapshot,
            journal.since(snapshot.next_seq()),
            &journal.text,
        )
        .unwrap();
        Self {
            kernel,
            journal,
            origin: self.origin.clone(),
            config: self.config.clone(),
            out: Out::new(),
            now_ms: self.now_ms,
        }
    }

    /// What `from`, a snapshot of this session, and the facts journaled
    /// after it add up to.
    pub(crate) fn rebuilt(&self, from: &Snapshot) -> Snapshot {
        Kernel::restore(
            self.config.clone(),
            BoxedPolicies::default(),
            from,
            self.journal.since(from.next_seq()),
            &self.journal.text,
        )
        .unwrap()
        .snapshot()
    }

    /// The place in the journal of the last fact so far.
    pub(crate) fn last_seq(&self) -> Seq {
        Seq(self.kernel.view().next_seq().get() - 1)
    }

    /// The process dies with everything journaled so far stored, and the
    /// session is rebuilt from its beginning.
    pub(crate) fn restart(&self) -> Self {
        self.crash(&self.origin, self.last_seq())
    }

    /// The first step of a session that was rebuilt.
    pub(crate) fn wake(&mut self) -> Emitted {
        self.control(ControlInput::Attached)
    }
}

/// A conversation, one line per item.
pub(crate) fn lines(kernel: &Kernel<BoxedPolicies>) -> Vec<String> {
    kernel
        .view()
        .items()
        .map(|item| match item {
            ItemView::UserTurn { who, text, .. } => format!("{who}: {text}"),
            ItemView::Utterance {
                agent,
                text,
                truncated,
                ..
            } => {
                let cut = if truncated { " [cut]" } else { "" };
                format!("agent {agent}: {text}{cut}")
            }
            ItemView::ToolCall {
                call, tool, args, ..
            } => format!("call {call}: tool {tool} {args}"),
            ItemView::ToolResult {
                call,
                outcome,
                text,
            } => format!("result {call}: {outcome:?} {text}"),
            ItemView::Notice { kind, text } => format!("note {kind:?}: {text}"),
        })
        .collect()
}

pub(crate) fn played(millis: u32, heard_text: u32, drained: bool) -> OpEvent {
    let position = MediaDur::from_millis(millis);
    OpEvent::Played {
        handed: position,
        acked: position,
        heard_text,
        drained,
    }
}
