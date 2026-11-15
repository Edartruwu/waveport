//! Workloads for the session kernel: one step each, from a prepared state.
//!
//! A session spends its life taking small steps, and the cost that matters
//! is that of the ones it takes most often. Each [`Case`] prepares a
//! kernel in the middle of a conversation and holds the one event to
//! measure; preparing is not part of the measurement.
//!
//! One workload is not a step: [`Journaled`] rebuilds a session from its
//! journal, which is what a process does for every session it takes over.

use waveport_domain::command::{Command, LlmCmd};
use waveport_domain::config::{AgentSpec, Effect, KernelConfig, ToolSpec};
use waveport_domain::event::{
    ControlInput, Event, Finish, OpEvent, OverlapVerdict, TimerId, ToolEvent, VadEdge,
};
use waveport_domain::fact::Stamped;
use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId, Prob, ToolCallId, ToolId, TurnId};
use waveport_domain::kernel::{Kernel, Snapshot};
use waveport_domain::out::Out;
use waveport_domain::policy::DefaultPolicies;
use waveport_domain::text::TextArena;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

const CALLER: ParticipantId = ParticipantId(1);

/// The response to the caller's first turn.
const REPLY: OpTag = OpTag {
    generation: GenId(1),
    epoch: Epoch(0),
};

const ANSWER: &str = "It shipped on Monday. It arrives on Thursday. ";

/// A step worth measuring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Case {
    /// Playback reports its position in the middle of a response. A
    /// session takes this step ten times a second while the agent speaks.
    PlayoutProgress,
    /// Someone starts to speak while nobody else does.
    SpeechEdge,
    /// Someone starts to speak over the agent: its volume is lowered and a
    /// verdict is asked for.
    Duck,
    /// An interim transcript, which changes nothing.
    InterimTranscript,
    /// A result for a response that has ended.
    StaleResult,
    /// Model text that does not complete a sentence, and is held.
    DeltaHeld,
    /// Model text that completes a sentence, which goes to synthesis.
    DeltaReleased,
    /// The verdict that interrupts the agent: the response ends, and what
    /// was heard of it enters the conversation.
    BargeIn,
    /// The transcript that completes a turn: the turn enters the
    /// conversation and a response begins.
    TurnCommit,
    /// A timer for something that has ended.
    StaleTimer,
    /// The last result of two tool calls made together: the batch is
    /// resolved and the response that tells of the results begins.
    ToolBatchResolve,
}

impl Case {
    /// Every case.
    pub const ALL: [Self; 11] = [
        Self::PlayoutProgress,
        Self::SpeechEdge,
        Self::Duck,
        Self::InterimTranscript,
        Self::StaleResult,
        Self::DeltaHeld,
        Self::DeltaReleased,
        Self::BargeIn,
        Self::TurnCommit,
        Self::StaleTimer,
        Self::ToolBatchResolve,
    ];

    /// A kernel in the state the case starts from, with the event to
    /// measure. It is boxed so that a benchmark hands a pointer around
    /// and not the kernel: moving a kernel is not part of a step, and
    /// would be counted as one.
    #[must_use]
    pub fn prepare(self) -> Box<Prepared> {
        let mut session = Session::new();
        let (at, event) = match self {
            Self::ToolBatchResolve => {
                session = Session::with(desk());
                session.asks();
                for tool in [ToolId(0), ToolId(1)] {
                    session.step(1_300, proposes(REPLY, tool));
                }
                let finish = Finish::ToolCalls;
                session.step(1_350, Event::Op(REPLY, OpEvent::LlmDone { finish }));
                session.step(1_500, returns(ToolCallId(1)));
                (1_520, returns(ToolCallId(2)))
            }
            Self::TurnCommit => {
                session.asks_without_words();
                (1_100, words(0, 900, "where is my order", true))
            }
            Self::DeltaHeld => {
                session.asks();
                (1_300, delta(REPLY, "It shipped on "))
            }
            Self::DeltaReleased => {
                session.asks();
                session.step(1_300, delta(REPLY, "It shipped on "));
                (1_320, delta(REPLY, "Monday. It arrives "))
            }
            Self::PlayoutProgress => {
                session.is_answered();
                (1_600, played(700, 12))
            }
            Self::Duck => {
                session.is_answered();
                (2_000, speech(VadEdge::Start, 2_000))
            }
            Self::BargeIn => {
                session.is_answered();
                session.step(2_000, speech(VadEdge::Start, 2_000));
                let verdict = OverlapVerdict::BargeIn;
                (
                    2_150,
                    Event::Overlap {
                        turn: TurnId(2),
                        verdict,
                    },
                )
            }
            Self::InterimTranscript => {
                session.is_answered();
                session.step(2_000, speech(VadEdge::Start, 2_000));
                (2_100, words(2_000, 2_100, "mm", false))
            }
            Self::SpeechEdge => {
                session.is_over();
                (4_000, speech(VadEdge::Start, 4_000))
            }
            Self::StaleResult => {
                session.is_over();
                (4_000, played(3_000, 45))
            }
            Self::StaleTimer => {
                session.is_over();
                (4_000, Event::Timer(TimerId::Gen(GenId(1))))
            }
        };
        Box::new(Prepared {
            kernel: session.kernel,
            out: session.out,
            now: MonoTime::from_millis(at),
            event,
        })
    }
}

/// A kernel and the event to give it.
pub struct Prepared {
    kernel: Kernel,
    out: Out,
    now: MonoTime,
    event: Event,
}

impl Prepared {
    /// Takes the step. Returns how many commands and facts it emitted.
    pub fn step(&mut self) -> (usize, usize) {
        self.kernel.step(self.now, &self.event, &mut self.out);
        (self.out.cmds.len(), self.out.facts.len())
    }
}

struct Session {
    kernel: Kernel,
    out: Out,
}

impl Session {
    fn new() -> Self {
        Self::with(KernelConfig::default())
    }

    fn with(config: KernelConfig) -> Self {
        // The configurations of this file are valid; a default kernel
        // stands in if one ever is not, and the test below notices.
        let kernel = Kernel::new(config, DefaultPolicies::default()).unwrap_or_default();
        let mut session = Self {
            kernel,
            out: Out::new(),
        };
        session.step(0, Event::Control(ControlInput::Join { who: CALLER }));
        session
    }

    /// The response the last step started.
    fn started(&self) -> Option<OpTag> {
        self.out.cmds.iter().find_map(|command| match command {
            Command::Llm(tag, LlmCmd::Start { .. }) => Some(*tag),
            _ => None,
        })
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "each call builds its event in place"
    )]
    fn step(&mut self, millis: u64, event: Event) {
        self.kernel
            .step(MonoTime::from_millis(millis), &event, &mut self.out);
    }

    /// The caller has asked something, and its words have not arrived.
    fn asks_without_words(&mut self) {
        self.step(0, speech(VadEdge::Start, 0));
        self.step(900, speech(VadEdge::End, 900));
        let p = Prob::ONE;
        self.step(960, Event::Eou { turn: TurnId(1), p });
    }

    /// The caller has asked something, and a response has begun.
    fn asks(&mut self) {
        self.asks_without_words();
        self.step(1_100, words(0, 900, "where is my order", true));
    }

    /// The agent is in the middle of its answer.
    fn is_answered(&mut self) {
        self.asks();
        self.step(1_300, delta(REPLY, ANSWER));
        let finish = Finish::Stop;
        self.step(1_350, Event::Op(REPLY, OpEvent::LlmDone { finish }));
        self.step(1_400, segment(22, 1_500, false));
        self.step(1_450, segment(45, 3_000, true));
        self.step(1_500, played(600, 10));
    }

    /// The agent has said its answer to the end.
    fn is_over(&mut self) {
        self.is_answered();
        let position = MediaDur::from_millis(3_000);
        let played = OpEvent::Played {
            handed: position,
            acked: position,
            heard_text: 45,
            drained: true,
        };
        self.step(3_900, Event::Op(REPLY, played));
    }
}

/// A session whose agent has two tools that change nothing.
fn desk() -> KernelConfig {
    let tools = vec![ToolSpec::new(Effect::Pure), ToolSpec::new(Effect::Pure)];
    let agents = vec![AgentSpec {
        tools: vec![ToolId(0), ToolId(1)],
    }];
    KernelConfig {
        tools,
        agents,
        ..KernelConfig::default()
    }
}

/// A session's journal, and what to rebuild the session from it.
pub struct Journaled {
    config: KernelConfig,
    origin: Snapshot,
    facts: Vec<Stamped>,
    text: TextArena,
}

impl Journaled {
    /// The journal of a conversation of `facts` facts: the caller asks,
    /// the agent calls a tool and tells the result, again and again.
    #[must_use]
    pub fn prepare(facts: usize) -> Self {
        let mut session = Session::with(desk());
        let mut journaled = Self {
            config: desk(),
            origin: Kernel::new(desk(), DefaultPolicies::default())
                .unwrap_or_default()
                .snapshot(),
            facts: Vec::new(),
            text: TextArena::default(),
        };
        // The caller joined before the journal was looked at.
        journaled.keep(&session.out);
        let mut now = 0;
        let mut call = 0;
        while journaled.facts.len() < facts {
            now += 10_000;
            call += 1;
            let asked = Event::TextIn {
                who: CALLER,
                text: "where is my order".into(),
            };
            journaled.step(&mut session, now, asked);
            let Some(reply) = session.started() else {
                break;
            };
            journaled.step(&mut session, now + 100, proposes(reply, ToolId(0)));
            let finish = Finish::ToolCalls;
            let done = Event::Op(reply, OpEvent::LlmDone { finish });
            journaled.step(&mut session, now + 200, done);
            journaled.step(&mut session, now + 300, returns(ToolCallId(call)));
            let Some(reply) = session.started() else {
                break;
            };
            journaled.step(&mut session, now + 400, delta(reply, ANSWER));
            let finish = Finish::Stop;
            let done = Event::Op(reply, OpEvent::LlmDone { finish });
            journaled.step(&mut session, now + 500, done);
            for last in [false, true] {
                let segment = OpEvent::TtsSegment {
                    text_end: 45,
                    audio_end: MediaDur::from_millis(3_000),
                    last,
                };
                journaled.step(&mut session, now + 600, Event::Op(reply, segment));
            }
            let position = MediaDur::from_millis(3_000);
            let played = OpEvent::Played {
                handed: position,
                acked: position,
                heard_text: 45,
                drained: true,
            };
            journaled.step(&mut session, now + 4_000, Event::Op(reply, played));
        }
        journaled.facts.truncate(facts);
        journaled
    }

    fn step(&mut self, session: &mut Session, millis: u64, event: Event) {
        session.step(millis, event);
        self.keep(&session.out);
    }

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

    /// How many facts the journal holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.facts.len()
    }

    /// Whether the journal holds no fact.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    /// Rebuilds the session. `None` if the journal does not add up, which
    /// it does.
    #[must_use]
    pub fn restore(&self) -> Option<Kernel> {
        Kernel::restore(
            self.config.clone(),
            DefaultPolicies::default(),
            &self.origin,
            &self.facts,
            &self.text,
        )
        .ok()
    }
}

fn proposes(tag: OpTag, tool: ToolId) -> Event {
    let args = "{\"order\":\"A-1041\"}".into();
    Event::Op(tag, OpEvent::ToolProposed { tool, args })
}

fn returns(call: ToolCallId) -> Event {
    let result = "{\"status\":\"shipped\"}".into();
    Event::Tool(call, ToolEvent::Completed { result })
}

fn speech(edge: VadEdge, at_ms: u64) -> Event {
    Event::Speech {
        who: CALLER,
        edge,
        at: MediaTime::from_millis(at_ms),
    }
}

fn words(from_ms: u64, to_ms: u64, text: &str, is_final: bool) -> Event {
    Event::Transcript {
        who: CALLER,
        span: MediaSpan::new(
            MediaTime::from_millis(from_ms),
            MediaTime::from_millis(to_ms),
        ),
        text: text.into(),
        is_final,
    }
}

fn delta(tag: OpTag, text: &str) -> Event {
    Event::Op(tag, OpEvent::LlmDelta { text: text.into() })
}

fn segment(text_end: u32, audio_ms: u32, last: bool) -> Event {
    let segment = OpEvent::TtsSegment {
        text_end,
        audio_end: MediaDur::from_millis(audio_ms),
        last,
    };
    Event::Op(REPLY, segment)
}

fn played(millis: u32, heard_text: u32) -> Event {
    let position = MediaDur::from_millis(millis);
    let played = OpEvent::Played {
        handed: position,
        acked: position,
        heard_text,
        drained: false,
    };
    Event::Op(REPLY, played)
}

#[cfg(test)]
mod tests {
    use super::{Case, Journaled};

    #[test]
    fn every_case_takes_the_step_it_names() {
        // Commands and facts each step emits. A case that measures the
        // wrong step shows here.
        let emitted = Case::ALL.map(|case| (case, case.prepare().step()));
        assert_eq!(
            emitted,
            [
                (Case::PlayoutProgress, (0, 0)),
                (Case::SpeechEdge, (2, 0)),
                (Case::Duck, (5, 0)),
                (Case::InterimTranscript, (0, 0)),
                (Case::StaleResult, (0, 0)),
                (Case::DeltaHeld, (1, 0)),
                (Case::DeltaReleased, (4, 0)),
                (Case::BargeIn, (5, 2)),
                (Case::TurnCommit, (6, 1)),
                (Case::StaleTimer, (0, 0)),
                (Case::ToolBatchResolve, (5, 2)),
            ]
        );
    }

    #[test]
    fn a_journal_of_200_facts_rebuilds_its_session() {
        let journaled = Journaled::prepare(200);
        assert_eq!(journaled.len(), 200);
        let kernel = journaled.restore().unwrap();
        let view = kernel.view();
        assert_eq!(view.next_seq().get(), 201);
        // Twenty-eight questions, calls, results and answers, and the
        // beginning of one more.
        assert!(view.items().count() >= 110, "{}", view.items().count());
    }
}
