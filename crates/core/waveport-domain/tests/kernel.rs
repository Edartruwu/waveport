//! The kernel, driven one event at a time.
//!
//! The scenarios in `scenarios/` tell whole stories. These tests look at
//! single rules, including the ones for input that no well-behaved adapter
//! sends.

use core::time::Duration;

use waveport_domain::command::{
    Activity, ClientEvent, Command, LlmCause, LlmCmd, PlayoutCmd, SenseCmd, SttCmd, TtsCmd,
};
use waveport_domain::config::KernelConfig;
use waveport_domain::event::{
    CloseReason, ControlInput, Event, Finish, OpEvent, OverlapVerdict, Part, RetryClass, TimerId,
    VadEdge,
};
use waveport_domain::fact::{Dismissal, EpochCause, Fact, Lifecycle};
use waveport_domain::ids::{
    AgentId, ContextVersion, Epoch, GenId, OpTag, ParticipantId, Prob, Seq, TurnId,
};
use waveport_domain::kernel::{ItemView, Kernel};
use waveport_domain::out::Out;
use waveport_domain::policy::{BoxedPolicies, OverlapDecision, SentenceChunker};
use waveport_domain::text::TextRef;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

const ANA: ParticipantId = ParticipantId(1);
const BEN: ParticipantId = ParticipantId(2);

/// What one step emitted, with its text resolved.
#[derive(Debug, Default)]
struct Emitted {
    cmds: Vec<(Command, String)>,
    facts: Vec<(Fact, String)>,
}

impl Emitted {
    fn has(&self, command: Command) -> bool {
        self.cmds.iter().any(|(emitted, _)| *emitted == command)
    }

    fn commands(&self) -> Vec<Command> {
        self.cmds.iter().map(|(command, _)| *command).collect()
    }

    fn facts(&self) -> Vec<Fact> {
        self.facts.iter().map(|(fact, _)| *fact).collect()
    }

    /// The text handed to synthesis in this step.
    fn spoken(&self) -> Vec<&str> {
        self.cmds
            .iter()
            .filter(|(command, _)| matches!(command, Command::Tts(_, TtsCmd::Speak { .. })))
            .map(|(_, text)| text.as_str())
            .collect()
    }

    /// The response started in this step.
    fn started(&self) -> Option<OpTag> {
        self.cmds.iter().find_map(|(command, _)| match command {
            Command::Llm(tag, LlmCmd::Start { .. }) => Some(*tag),
            _ => None,
        })
    }

    /// The text of the first fact that `matches`.
    fn fact_text(&self, matches: impl Fn(&Fact) -> bool) -> Option<&str> {
        self.facts
            .iter()
            .find(|(fact, _)| matches(fact))
            .map(|(_, text)| text.as_str())
    }

    fn journals_nothing(&self) -> bool {
        self.facts.is_empty()
    }

    fn speaks_nothing(&self) -> bool {
        self.spoken().is_empty()
    }

    fn is_silent(&self) -> bool {
        self.cmds.is_empty() && self.journals_nothing()
    }
}

/// A kernel and the clock it is stepped with.
struct Session {
    kernel: Kernel<BoxedPolicies>,
    out: Out,
    now_ms: u64,
}

#[expect(
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "helpers of tests: a failed unwrap fails the test, and each call builds its event in place"
)]
impl Session {
    /// A session with Ana in it.
    fn new() -> Self {
        Self::with(KernelConfig::default(), BoxedPolicies::default())
    }

    fn with(config: KernelConfig, policies: BoxedPolicies) -> Self {
        let mut session = Self {
            kernel: Kernel::new(config, policies).unwrap(),
            out: Out::new(),
            now_ms: 0,
        };
        session.step(Event::Control(ControlInput::Join { who: ANA }));
        session
    }

    fn step(&mut self, event: Event) -> Emitted {
        let now = MonoTime::from_millis(self.now_ms);
        self.kernel.step(now, &event, &mut self.out);
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
                .map(|stamped| (stamped.fact, text(stamped.fact.text())))
                .collect(),
        }
    }

    /// Steps at `millis` on the clock.
    fn at(&mut self, millis: u64, event: Event) -> Emitted {
        self.now_ms = millis;
        self.step(event)
    }

    fn control(&mut self, input: ControlInput) -> Emitted {
        self.step(Event::Control(input))
    }

    fn op(&mut self, tag: OpTag, op: OpEvent) -> Emitted {
        self.step(Event::Op(tag, op))
    }

    /// `who` says `text` from `from_ms` to `to_ms`, and every adapter
    /// reports it in the usual order. Returns what the last report, the
    /// transcript, led to.
    fn says(&mut self, who: ParticipantId, from_ms: u64, to_ms: u64, text: &str) -> Emitted {
        let started = self.at(from_ms, speech(who, VadEdge::Start, from_ms));
        let turn = started
            .cmds
            .iter()
            .find_map(|(command, _)| match command {
                Command::Emit(ClientEvent::UserSpeaking { turn, .. }) => Some(*turn),
                _ => None,
            })
            .unwrap();
        self.at(to_ms, speech(who, VadEdge::End, to_ms));
        self.at(
            to_ms + 60,
            Event::Eou {
                turn,
                p: Prob::from_f32(0.95),
            },
        );
        self.at(to_ms + 200, transcript(who, from_ms, to_ms, text))
    }

    /// Ana asks something and the agent starts to answer. Returns the tag
    /// of the response.
    fn asked(&mut self) -> OpTag {
        self.says(ANA, 0, 900, "where is my order")
            .started()
            .unwrap()
    }

    /// The language model writes `text` and finishes; synthesis delivers
    /// its audio, `audio_ms` long.
    fn answered(&mut self, tag: OpTag, text: &str, audio_ms: u32) {
        self.op(tag, delta(text));
        self.op(
            tag,
            OpEvent::LlmDone {
                finish: Finish::Stop,
            },
        );
        let all = u32::try_from(text.len()).unwrap();
        self.op(tag, segment(all, audio_ms, false));
        self.op(tag, segment(all, audio_ms, true));
    }

    fn items(&self) -> Vec<String> {
        self.kernel
            .view()
            .items()
            .map(|item| match item {
                ItemView::UserTurn {
                    who,
                    text,
                    addressed,
                    ..
                } => {
                    format!("{who}{} {text}", if addressed { ":" } else { " (aside):" })
                }
                ItemView::Utterance {
                    text, truncated, ..
                } => {
                    format!("agent: {text}{}", if truncated { " [cut]" } else { "" })
                }
                other => format!("{other:?}"),
            })
            .collect()
    }
}

fn speech(who: ParticipantId, edge: VadEdge, at_ms: u64) -> Event {
    Event::Speech {
        who,
        edge,
        at: MediaTime::from_millis(at_ms),
    }
}

fn span(from_ms: u64, to_ms: u64) -> MediaSpan {
    MediaSpan::new(
        MediaTime::from_millis(from_ms),
        MediaTime::from_millis(to_ms),
    )
}

fn transcript(who: ParticipantId, from_ms: u64, to_ms: u64, text: &str) -> Event {
    Event::Transcript {
        who,
        span: span(from_ms, to_ms),
        text: text.into(),
        is_final: true,
    }
}

fn delta(text: &str) -> OpEvent {
    OpEvent::LlmDelta { text: text.into() }
}

fn segment(text_end: u32, audio_ms: u32, last: bool) -> OpEvent {
    OpEvent::TtsSegment {
        text_end,
        audio_end: MediaDur::from_millis(audio_ms),
        last,
    }
}

fn played(millis: u32, heard_text: u32, drained: bool) -> OpEvent {
    let position = MediaDur::from_millis(millis);
    OpEvent::Played {
        handed: position,
        acked: position,
        heard_text,
        drained,
    }
}

fn listening(generation: u32) -> Command {
    Command::Emit(ClientEvent::Agent {
        agent: AgentId(0),
        generation: Some(GenId(generation)),
        activity: Activity::Listening,
    })
}

// ---- Turns -----------------------------------------------------------------

#[test]
fn a_turn_is_committed_when_its_speaker_is_done_and_its_words_are_in() {
    let mut session = Session::new();
    let started = session.at(1_000, speech(ANA, VadEdge::Start, 1_000));
    assert!(started.has(Command::Emit(ClientEvent::UserSpeaking {
        who: ANA,
        turn: TurnId(1),
        speaking: true,
    })));
    assert!(started.has(Command::SetTimer(
        TimerId::TurnMax(TurnId(1)),
        Some(MonoTime::from_millis(121_000))
    )));

    let stopped = session.at(1_900, speech(ANA, VadEdge::End, 1_900));
    let estimate = Command::Sense(SenseCmd::EstimateEou {
        turn: TurnId(1),
        who: ANA,
    });
    assert!(stopped.has(estimate));
    // Until an estimate arrives, plain silence would end the turn.
    assert!(stopped.has(Command::SetTimer(
        TimerId::Eou(TurnId(1)),
        Some(MonoTime::from_millis(2_700))
    )));

    let sure = session.at(
        1_960,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::from_f32(0.9),
        },
    );
    assert!(sure.has(Command::Stt(SttCmd::Finalize { who: ANA })));
    assert!(sure.journals_nothing(), "the words are not in yet");

    let heard = session.at(2_100, transcript(ANA, 1_000, 1_900, "where is my order"));
    assert_eq!(
        heard.facts(),
        [Fact::TurnCommitted {
            turn: TurnId(1),
            who: ANA,
            span: span(1_000, 1_900),
            text: heard.facts[0].0.text().unwrap(),
            addressed: true,
        }]
    );
    assert_eq!(heard.facts[0].1, "where is my order");
    let tag = OpTag {
        generation: GenId(1),
        epoch: Epoch(0),
    };
    assert!(heard.has(Command::Llm(
        tag,
        LlmCmd::Start {
            agent: AgentId(0),
            context: ContextVersion(1),
            cause: LlmCause::Reply { turn: TurnId(1) },
        }
    )));
    assert!(session.kernel.view().is_live(tag));
    assert_eq!(session.kernel.view().turns().count(), 0);
    assert_eq!(session.items(), ["1: where is my order"]);
}

#[test]
fn the_order_of_the_reports_does_not_matter() {
    // The transcript first, then the end of the speech, then the estimate.
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    assert!(
        session
            .at(10, transcript(ANA, 0, 900, "where is my order"))
            .journals_nothing()
    );
    session.at(20, speech(ANA, VadEdge::End, 900));
    let last = session.at(
        30,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::ONE,
        },
    );
    assert_eq!(last.fact_text(|_| true), Some("where is my order"));
    assert!(
        !last.has(Command::Stt(SttCmd::Finalize { who: ANA })),
        "nothing is missing"
    );
    assert!(last.started().is_some());
}

#[test]
fn speech_that_goes_on_continues_the_turn() {
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    session.at(600, speech(ANA, VadEdge::End, 600));
    session.at(
        660,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::ONE,
        },
    );
    // The speaker goes on before the first words have arrived.
    let resumed = session.at(700, speech(ANA, VadEdge::Start, 700));
    assert!(resumed.has(Command::SetTimer(TimerId::Eou(TurnId(1)), None)));
    assert!(
        session
            .at(800, transcript(ANA, 0, 600, "where is"))
            .journals_nothing()
    );
    session.at(1_300, speech(ANA, VadEdge::End, 1_300));
    // An estimate asked for before they went on would be stale; this one
    // is for the speech that just stopped.
    session.at(
        1_360,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::ONE,
        },
    );
    let done = session.at(1_500, transcript(ANA, 700, 1_300, "my order"));
    assert_eq!(done.fact_text(|_| true), Some("where is my order"));
    assert_eq!(session.kernel.view().context_version(), ContextVersion(1));
}

#[test]
fn an_unsure_estimate_waits_and_silence_ends_the_turn_anyway() {
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    session.at(900, speech(ANA, VadEdge::End, 900));
    session.at(950, transcript(ANA, 0, 900, "i would like to"));
    let unsure = session.at(
        960,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::from_f32(0.1),
        },
    );
    assert!(unsure.journals_nothing());
    assert!(unsure.has(Command::SetTimer(
        TimerId::Eou(TurnId(1)),
        Some(MonoTime::from_millis(3_900))
    )));
    let ended = session.at(3_900, Event::Timer(TimerId::Eou(TurnId(1))));
    assert_eq!(ended.fact_text(|_| true), Some("i would like to"));
}

#[test]
fn a_turn_without_words_is_dismissed_when_they_have_had_their_time() {
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    session.at(300, speech(ANA, VadEdge::End, 300));
    let ended = session.at(
        360,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::ONE,
        },
    );
    assert!(ended.has(Command::SetTimer(
        TimerId::Eou(TurnId(1)),
        Some(MonoTime::from_millis(2_360))
    )));
    let waited = session.at(2_360, Event::Timer(TimerId::Eou(TurnId(1))));
    assert_eq!(
        waited.facts(),
        [Fact::TurnDismissed {
            turn: TurnId(1),
            who: ANA,
            reason: Dismissal::Empty
        }]
    );
    assert!(waited.started().is_none());

    // The words arrive after all: they are a turn of their own.
    let late = session.at(2_500, transcript(ANA, 0, 300, "hello"));
    assert!(matches!(
        late.facts()[..],
        [Fact::TurnCommitted {
            turn: TurnId(2),
            ..
        }]
    ));
    assert!(late.started().is_some());
    assert_eq!(session.items(), ["1: hello"]);
}

#[test]
fn words_nobody_was_heard_saying_are_a_turn_of_their_own() {
    let mut session = Session::new();
    let orphan = session.at(500, transcript(ANA, 0, 400, "  hello there "));
    assert_eq!(orphan.fact_text(|_| true), Some("hello there"));
    let typed = session.at(
        600,
        Event::TextIn {
            who: ANA,
            text: "and this is typed".into(),
        },
    );
    // The response to the first has not been heard, so it is replaced.
    assert!(matches!(
        typed.facts()[..],
        [
            Fact::TurnCommitted {
                turn: TurnId(2),
                ..
            },
            Fact::EpochAdvanced {
                cause: EpochCause::Superseded,
                ..
            },
        ]
    ));
    assert!(
        session
            .at(
                700,
                Event::TextIn {
                    who: ANA,
                    text: "  ".into()
                }
            )
            .is_silent()
    );
    assert_eq!(session.items(), ["1: hello there", "1: and this is typed"]);

    // Words from before the open turn began belong to no open turn either.
    session.at(5_000, speech(ANA, VadEdge::Start, 5_000));
    let older = session.at(5_100, transcript(ANA, 4_000, 4_500, "one more thing"));
    assert_eq!(older.fact_text(|_| true), Some("one more thing"));
    assert_eq!(session.kernel.view().turns().count(), 1);
}

#[test]
fn interim_transcripts_and_strangers_change_nothing() {
    let mut session = Session::new();
    let before = session.kernel.fingerprint();
    let interim = Event::Transcript {
        who: ANA,
        span: span(0, 300),
        text: "whe".into(),
        is_final: false,
    };
    assert!(session.step(interim).is_silent());
    assert!(session.step(speech(BEN, VadEdge::Start, 0)).is_silent());
    assert!(session.step(transcript(BEN, 0, 300, "hello")).is_silent());
    assert!(
        session
            .step(Event::TextIn {
                who: BEN,
                text: "hello".into()
            })
            .is_silent()
    );
    assert!(
        session.step(speech(ANA, VadEdge::End, 0)).is_silent(),
        "she was not speaking"
    );
    assert!(
        session
            .step(Event::Eou {
                turn: TurnId(9),
                p: Prob::ONE
            })
            .is_silent()
    );
    let verdict = Event::Overlap {
        turn: TurnId(9),
        verdict: OverlapVerdict::BargeIn,
    };
    assert!(session.step(verdict).is_silent());
    assert_eq!(session.kernel.fingerprint(), before);
}

#[test]
fn a_turn_that_never_ends_is_ended() {
    // The end of the speech is never reported.
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    session.at(5_000, transcript(ANA, 0, 5_000, "and another thing"));
    let forced = session.at(120_000, Event::Timer(TimerId::TurnMax(TurnId(1))));
    // The speech is taken to have lasted until now, so words are missing.
    assert!(forced.has(Command::Emit(ClientEvent::UserSpeaking {
        who: ANA,
        turn: TurnId(1),
        speaking: false,
    })));
    assert!(forced.has(Command::Stt(SttCmd::Finalize { who: ANA })));
    assert!(forced.journals_nothing());
    assert_eq!(
        session.kernel.view().turns().next().unwrap().span,
        span(0, 120_000)
    );
    let waited = session.at(122_000, Event::Timer(TimerId::Eou(TurnId(1))));
    assert_eq!(waited.fact_text(|_| true), Some("and another thing"));

    // The speaker stops and starts for two minutes, and no pause is ever
    // taken for the end.
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    session.at(119_000, speech(ANA, VadEdge::End, 119_000));
    session.at(119_100, transcript(ANA, 0, 119_000, "and so on"));
    let forced = session.at(120_000, Event::Timer(TimerId::TurnMax(TurnId(1))));
    assert_eq!(
        forced.fact_text(|_| true),
        Some("and so on"),
        "nothing is missing"
    );
}

#[test]
fn a_turn_that_runs_out_over_the_agent_gives_the_floor_back() {
    struct Undecided;
    impl waveport_domain::policy::InterruptionPolicy for Undecided {
        fn decide(&self, _input: &waveport_domain::policy::OverlapInput) -> OverlapDecision {
            OverlapDecision::Wait(Duration::from_secs(1))
        }
    }
    let policies = BoxedPolicies {
        interruption: Box::new(Undecided),
        ..BoxedPolicies::default()
    };
    let mut session = Session::with(KernelConfig::default(), policies);
    let tag = session.asked();
    session.answered(tag, "It shipped on Monday. It arrives on Thursday.", 3_000);
    session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    assert!(session.kernel.view().generations().next().unwrap().lowered);

    let forced = session.at(122_000, Event::Timer(TimerId::TurnMax(TurnId(2))));
    assert!(forced.has(Command::Playout(tag, PlayoutCmd::Unduck)));
    assert!(!session.kernel.view().generations().next().unwrap().lowered);
    assert!(session.kernel.view().is_live(tag));
}

#[test]
fn speech_edges_are_ordered_by_the_audio_and_not_by_arrival() {
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    session.at(900, speech(ANA, VadEdge::End, 900));
    // The start arrives a second time, after the end.
    assert!(session.at(910, speech(ANA, VadEdge::Start, 0)).is_silent());
    assert!(!session.kernel.view().turns().next().unwrap().speaking);

    // The speaker goes on, and the end of the first stretch is repeated.
    session.at(1_500, speech(ANA, VadEdge::Start, 1_500));
    assert!(
        session
            .at(1_510, speech(ANA, VadEdge::End, 900))
            .is_silent()
    );
    assert!(session.kernel.view().turns().next().unwrap().speaking);
    session.at(2_000, speech(ANA, VadEdge::End, 2_000));
    assert_eq!(
        session.kernel.view().turns().next().unwrap().span,
        span(0, 2_000)
    );
}

#[test]
fn speech_after_a_turn_has_run_out_is_a_new_turn() {
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    session.at(5_000, transcript(ANA, 0, 5_000, "and another thing"));
    // Forced to end, the turn waits two seconds for its last words.
    session.at(120_000, Event::Timer(TimerId::TurnMax(TurnId(1))));
    let more = session.at(121_000, speech(ANA, VadEdge::Start, 121_000));
    assert_eq!(more.fact_text(|_| true), Some("and another thing"));
    assert!(more.has(Command::Emit(ClientEvent::UserSpeaking {
        who: ANA,
        turn: TurnId(2),
        speaking: true,
    })));
    assert!(more.has(Command::SetTimer(
        TimerId::TurnMax(TurnId(2)),
        Some(MonoTime::from_millis(241_000))
    )));
    // The words that were still on their way are not lost either.
    let late = session.at(121_500, transcript(ANA, 5_000, 120_000, "and so on"));
    assert_eq!(late.fact_text(|_| true), Some("and so on"));

    // The same when the timer fired while the turn was already ending.
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    session.at(119_000, speech(ANA, VadEdge::End, 119_000));
    session.at(
        119_500,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::ONE,
        },
    );
    assert!(
        session
            .at(120_000, Event::Timer(TimerId::TurnMax(TurnId(1))))
            .is_silent()
    );
    let more = session.at(120_500, speech(ANA, VadEdge::Start, 120_500));
    assert_eq!(
        more.facts(),
        [Fact::TurnDismissed {
            turn: TurnId(1),
            who: ANA,
            reason: Dismissal::Empty
        }]
    );
    assert_eq!(
        session.kernel.view().turns().next().unwrap().turn,
        TurnId(2)
    );
}

#[test]
fn only_so_many_people_can_be_in_the_middle_of_a_turn() {
    let mut config = KernelConfig::default();
    config.limits.open_turns = 1;
    let mut session = Session::with(config, BoxedPolicies::default());
    session.control(ControlInput::Join { who: BEN });
    session.at(0, speech(ANA, VadEdge::Start, 0));
    assert!(session.at(10, speech(BEN, VadEdge::Start, 10)).is_silent());
    assert_eq!(session.kernel.view().turns().count(), 1);
}

// ---- People ----------------------------------------------------------------

#[test]
fn people_join_once_and_leave_with_their_words() {
    let mut session = Session::new();
    assert_eq!(
        session.control(ControlInput::Join { who: BEN }).facts(),
        [Fact::ParticipantJoined { who: BEN }]
    );
    assert!(session.control(ControlInput::Join { who: BEN }).is_silent());
    assert_eq!(
        session.kernel.view().participants().collect::<Vec<_>>(),
        [ANA, BEN]
    );

    session.at(0, speech(BEN, VadEdge::Start, 0));
    session.at(400, transcript(BEN, 0, 400, "i have to go"));
    let left = session.at(500, Event::Control(ControlInput::Leave { who: BEN }));
    assert!(matches!(
        left.facts()[..],
        [
            Fact::TurnCommitted { who: BEN, .. },
            Fact::ParticipantLeft { who: BEN }
        ]
    ));
    assert!(left.has(Command::Stt(SttCmd::Close { who: BEN })));
    assert!(
        session
            .control(ControlInput::Leave { who: BEN })
            .is_silent()
    );
    assert!(session.step(speech(BEN, VadEdge::Start, 900)).is_silent());

    let mut config = KernelConfig::default();
    config.limits.participants = 1;
    let mut full = Session::with(config, BoxedPolicies::default());
    assert!(full.control(ControlInput::Join { who: BEN }).is_silent());
}

#[test]
fn someone_who_leaves_while_speaking_over_the_agent_gives_the_floor_back() {
    let (mut session, tag) = speaking();
    session.control(ControlInput::Join { who: BEN });
    let over = session.at(2_000, speech(BEN, VadEdge::Start, 2_000));
    assert!(over.has(Command::Playout(tag, PlayoutCmd::Duck)));
    let left = session.at(2_100, Event::Control(ControlInput::Leave { who: BEN }));
    assert!(left.has(Command::Playout(tag, PlayoutCmd::Unduck)));
    assert!(matches!(
        left.facts()[..],
        [
            Fact::TurnDismissed {
                who: BEN,
                reason: Dismissal::Empty,
                ..
            },
            Fact::ParticipantLeft { who: BEN },
        ]
    ));
    assert!(!session.kernel.view().generations().next().unwrap().lowered);
}

// ---- Responses -------------------------------------------------------------

#[test]
fn text_reaches_synthesis_a_sentence_at_a_time_and_only_as_it_is_heard() {
    let mut session = Session::new();
    let tag = session.asked();

    assert!(session.op(tag, delta("It shipped ")).speaks_nothing());
    let first = session.op(tag, delta("on Monday. It "));
    assert_eq!(first.spoken(), ["It shipped on Monday. "]);
    // The first words may be heard at once: nobody else is speaking.
    assert!(first.has(Command::Playout(tag, PlayoutCmd::Start)));
    assert!(first.has(Command::SetTimer(
        TimerId::Checkpoint,
        Some(MonoTime::from_millis(2_100))
    )));

    // The next sentence waits until the audio of the first has arrived.
    assert!(
        session
            .op(tag, delta("arrives on Thursday. "))
            .speaks_nothing()
    );
    let second = session.op(tag, segment(22, 1_500, false));
    assert_eq!(second.spoken(), ["It arrives on Thursday. "]);

    // Four seconds of audio are waiting to be played: the rest is held.
    assert!(session.op(tag, delta("Anything else?")).speaks_nothing());
    assert!(session.op(tag, segment(46, 4_000, false)).speaks_nothing());
    let done = session.op(
        tag,
        OpEvent::LlmDone {
            finish: Finish::Stop,
        },
    );
    assert!(done.speaks_nothing());
    assert!(!done.has(Command::Tts(tag, TtsCmd::Finish)));

    // The listener catches up, and the rest follows with the end marker.
    let caught_up = session.op(tag, played(1_200, 22, false));
    assert_eq!(caught_up.spoken(), ["Anything else?"]);
    assert!(caught_up.has(Command::Tts(tag, TtsCmd::Finish)));
}

#[test]
fn a_response_that_plays_to_its_end_enters_the_conversation_whole() {
    let mut session = Session::new();
    let tag = session.asked();
    session.answered(tag, "It shipped on Monday. ", 1_500);
    assert!(session.op(tag, played(900, 10, false)).journals_nothing());

    let over = session.op(tag, played(1_500, 22, true));
    assert_eq!(
        over.facts(),
        [Fact::UtteranceCommitted {
            generation: GenId(1),
            agent: AgentId(0),
            text: over.facts[0].0.text().unwrap(),
            truncated: false,
        }]
    );
    assert_eq!(over.facts[0].1, "It shipped on Monday.");
    assert!(over.has(listening(1)));
    assert!(over.has(Command::SetTimer(TimerId::Gen(GenId(1)), None)));
    assert!(!session.kernel.view().is_live(tag));
    assert_eq!(
        session.kernel.view().epoch(),
        Epoch(0),
        "nothing was interrupted"
    );
    assert_eq!(
        session.items(),
        ["1: where is my order", "agent: It shipped on Monday."]
    );

    // Whatever arrives for it afterwards is dropped.
    assert!(session.op(tag, played(1_500, 22, true)).is_silent());
    assert!(session.op(tag, delta("And another thing.")).is_silent());
}

#[test]
fn the_end_of_playback_counts_only_once_synthesis_is_done() {
    let mut session = Session::new();
    let tag = session.asked();
    session.op(tag, delta("It shipped on Monday. It arrives "));
    session.op(tag, segment(22, 1_500, false));
    // Playback has run dry while the model is still writing.
    assert!(session.op(tag, played(1_500, 22, true)).journals_nothing());
    assert!(session.kernel.view().is_live(tag));
}

#[test]
fn what_playback_reports_is_held_to_what_was_released() {
    let mut session = Session::new();
    let tag = session.asked();
    session.op(tag, delta("It shipped on Monday. It arrives on Thursday."));
    // Only the first sentence is with synthesis. A report of more than
    // that was heard is cut down to it.
    session.op(tag, played(600, 4_000, false));
    let cut = session.control(ControlInput::Interrupt);
    assert_eq!(cut.fact_text(|_| true), Some("It shipped on Monday."));
}

#[test]
fn a_model_that_writes_too_far_ahead_is_told_to_wait() {
    let mut config = KernelConfig::default();
    config.limits.pending_text = 40;
    let mut session = Session::with(config, BoxedPolicies::default());
    let tag = session.asked();
    session.op(tag, delta("One. "));
    // Synthesis has the first sentence; everything after it piles up.
    let piled = session.op(
        tag,
        delta("Two two two two two two two two two two two two. "),
    );
    assert!(piled.has(Command::Llm(tag, LlmCmd::Pause)));
    let more = session.op(tag, delta("Three. "));
    assert!(
        !more.has(Command::Llm(tag, LlmCmd::Pause)),
        "it was told once"
    );

    let released = session.op(tag, segment(5, 300, false));
    assert_eq!(released.spoken().len(), 1);
    assert!(released.has(Command::Llm(tag, LlmCmd::Resume)));
}

#[test]
fn a_model_that_will_not_wait_is_stopped() {
    let mut config = KernelConfig::default();
    config.limits.pending_text = 40;
    let mut session = Session::with(config, BoxedPolicies::default());
    let tag = session.asked();
    session.op(tag, delta("One. "));
    assert!(
        session
            .op(tag, delta(&"two ".repeat(11)))
            .has(Command::Llm(tag, LlmCmd::Pause))
    );
    // Told to wait, it writes on, past twice the limit.
    let more = session.op(tag, delta(&"three ".repeat(7)));
    assert!(more.has(Command::Llm(tag, LlmCmd::Cancel)));
    assert!(
        session.op(tag, delta("four ")).is_silent(),
        "what it writes now is dropped"
    );

    // What it wrote before is said, and that is the end of the response.
    let released = session.op(tag, segment(5, 300, false));
    assert_eq!(
        released.spoken(),
        [format!("{}{}", "two ".repeat(11), "three ".repeat(7))]
    );
    assert!(released.has(Command::Tts(tag, TtsCmd::Finish)));
}

#[test]
fn a_model_with_nothing_to_say_ends_quietly() {
    let mut session = Session::new();
    let tag = session.asked();
    session.op(tag, delta("  "));
    let done = session.op(
        tag,
        OpEvent::LlmDone {
            finish: Finish::Stop,
        },
    );
    assert!(done.journals_nothing());
    assert!(done.has(listening(1)));
    assert!(!session.kernel.view().is_live(tag));
    assert_eq!(session.kernel.view().epoch(), Epoch(0));
    assert!(
        !session.kernel.view().reply_owed(),
        "the turn was answered, with nothing"
    );
}

#[test]
fn a_policy_that_answers_nonsense_releases_nothing() {
    struct Greedy;
    impl waveport_domain::policy::TextChunker for Greedy {
        fn ready(&self, pending: &str, _done: bool) -> usize {
            pending.len() + 7
        }
    }
    struct MidCharacter;
    impl waveport_domain::policy::TextChunker for MidCharacter {
        fn ready(&self, _pending: &str, _done: bool) -> usize {
            1
        }
    }
    for chunker in [
        Box::new(Greedy) as Box<dyn waveport_domain::policy::TextChunker + Send>,
        Box::new(MidCharacter),
    ] {
        let policies = BoxedPolicies {
            chunker,
            ..BoxedPolicies::default()
        };
        let mut session = Session::with(KernelConfig::default(), policies);
        let tag = session.asked();
        assert!(session.op(tag, delta("Übermorgen. ")).speaks_nothing());
    }
    // The shipped chunker, for comparison.
    let policies = BoxedPolicies {
        chunker: Box::new(SentenceChunker::default()),
        ..BoxedPolicies::default()
    };
    let mut session = Session::with(KernelConfig::default(), policies);
    let tag = session.asked();
    assert_eq!(
        session.op(tag, delta("Übermorgen. ")).spoken(),
        ["Übermorgen. "]
    );
}

#[test]
fn the_agent_waits_for_a_gap_before_it_speaks() {
    let mut session = Session::new();
    session.control(ControlInput::Join { who: BEN });
    let tag = session.asked();
    // Ben starts to talk while the model is thinking.
    session.at(1_200, speech(BEN, VadEdge::Start, 1_200));
    let ready = session.at(1_300, Event::Op(tag, delta("It shipped on Monday. ")));
    assert_eq!(ready.spoken(), ["It shipped on Monday. "]);
    assert!(!ready.has(Command::Playout(tag, PlayoutCmd::Start)));
    assert!(
        !ready.has(Command::Sense(SenseCmd::ClassifyOverlap {
            turn: TurnId(2),
            who: BEN
        })),
        "nothing is being spoken over"
    );

    // Ben's remark is not for the agent... (the default policy says every
    // turn is, so here it is dismissed for having no words instead.)
    session.at(1_500, speech(BEN, VadEdge::End, 1_500));
    session.at(
        1_560,
        Event::Eou {
            turn: TurnId(2),
            p: Prob::ONE,
        },
    );
    let gap = session.at(3_560, Event::Timer(TimerId::Eou(TurnId(2))));
    assert!(gap.has(Command::Playout(tag, PlayoutCmd::Start)));
}

#[test]
fn a_response_nobody_has_heard_is_replaced_when_the_user_says_more() {
    let mut session = Session::new();
    let first = session.asked();
    // The user goes on while the model is still thinking.
    session.at(1_200, speech(ANA, VadEdge::Start, 1_200));
    session.at(1_300, Event::Op(first, delta("It shipped on Monday. ")));
    let more = {
        session.at(1_800, speech(ANA, VadEdge::End, 1_800));
        session.at(
            1_860,
            Event::Eou {
                turn: TurnId(2),
                p: Prob::ONE,
            },
        );
        session.at(2_000, transcript(ANA, 1_200, 1_800, "the second one"))
    };
    assert!(more.has(Command::Llm(first, LlmCmd::Cancel)));
    assert!(more.has(Command::Tts(first, TtsCmd::Cancel)));
    assert!(
        !more.has(Command::Playout(first, PlayoutCmd::Stop { fade_ms: 8 })),
        "it never played"
    );
    assert!(more.facts().contains(&Fact::EpochAdvanced {
        to: Epoch(1),
        cause: EpochCause::Superseded,
    }));
    let second = more.started().unwrap();
    assert_eq!(
        second,
        OpTag {
            generation: GenId(2),
            epoch: Epoch(1)
        }
    );
    assert!(!session.kernel.view().is_live(first));
    assert_eq!(
        session.items(),
        ["1: where is my order", "1: the second one"]
    );
}

#[test]
fn a_failed_model_still_says_what_it_wrote() {
    let failure = |part| OpEvent::Failed {
        part,
        class: RetryClass::Transient,
    };

    // Nothing written: the response ends, and nothing is said.
    let mut session = Session::new();
    let tag = session.asked();
    let failed = session.op(tag, failure(Part::Think));
    assert!(failed.journals_nothing());
    assert!(!session.kernel.view().is_live(tag));

    // Something written: it is spoken, as if the model had finished.
    let mut session = Session::new();
    let tag = session.asked();
    session.op(tag, delta("It shipped on"));
    let failed = session.op(tag, failure(Part::Think));
    assert_eq!(failed.spoken(), ["It shipped on"]);
    assert!(failed.has(Command::Tts(tag, TtsCmd::Finish)));
    session.op(tag, segment(13, 900, true));
    let over = session.op(tag, played(900, 13, true));
    assert_eq!(over.fact_text(|_| true), Some("It shipped on"));
}

#[test]
fn failed_speech_ends_the_response_with_what_was_heard() {
    let mut session = Session::new();
    let tag = session.asked();
    session.op(tag, delta("It shipped on Monday. It arrives on Thursday."));
    session.op(tag, segment(22, 1_500, false));
    session.op(tag, played(900, 10, false));
    let failed = session.op(
        tag,
        OpEvent::Failed {
            part: Part::Speak,
            class: RetryClass::Permanent,
        },
    );
    assert_eq!(
        failed.fact_text(|fact| matches!(fact, Fact::UtteranceCommitted { .. })),
        Some("It shipped")
    );
    assert!(failed.facts().contains(&Fact::EpochAdvanced {
        to: Epoch(1),
        cause: EpochCause::Failure
    }));
    assert!(failed.has(Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 })));
    assert!(failed.has(Command::Llm(tag, LlmCmd::Cancel)));
}

#[test]
fn a_response_that_stalls_is_ended() {
    let mut session = Session::new();
    let tag = session.asked();
    // Armed when the response starts, and again at its first words.
    let first = session.at(2_000, Event::Op(tag, delta("It ")));
    assert!(first.has(Command::SetTimer(
        TimerId::Gen(GenId(1)),
        Some(MonoTime::from_millis(62_000))
    )));
    let again = session.at(2_100, Event::Op(tag, delta("shipped ")));
    assert!(
        !again
            .cmds
            .iter()
            .any(|(command, _)| matches!(command, Command::SetTimer(TimerId::Gen(_), _)))
    );

    let stalled = session.at(62_000, Event::Timer(TimerId::Gen(GenId(1))));
    assert!(stalled.has(Command::Llm(tag, LlmCmd::Cancel)));
    assert!(stalled.facts().contains(&Fact::EpochAdvanced {
        to: Epoch(1),
        cause: EpochCause::Failure
    }));
    assert!(
        session
            .at(63_000, Event::Timer(TimerId::Gen(GenId(1))))
            .is_silent()
    );
}

#[test]
fn progress_is_recorded_once_a_second_while_the_agent_speaks() {
    let mut session = Session::new();
    let tag = session.asked();
    session.answered(tag, "It shipped on Monday. ", 1_500);
    assert!(
        session
            .at(2_100, Event::Timer(TimerId::Checkpoint))
            .journals_nothing(),
        "nothing heard yet"
    );

    session.op(tag, played(600, 10, false));
    let first = session.at(3_100, Event::Timer(TimerId::Checkpoint));
    assert_eq!(first.fact_text(|_| true), Some("It shipped"));
    assert!(first.has(Command::SetTimer(
        TimerId::Checkpoint,
        Some(MonoTime::from_millis(4_100))
    )));
    assert!(first.has(Command::SetTimer(
        TimerId::Gen(GenId(1)),
        Some(MonoTime::from_millis(63_100))
    )));

    session.op(tag, played(1_200, 21, false));
    let second = session.at(4_100, Event::Timer(TimerId::Checkpoint));
    assert_eq!(
        second.fact_text(|_| true),
        Some(" on Monday."),
        "only what is new"
    );
    assert!(
        session
            .at(5_100, Event::Timer(TimerId::Checkpoint))
            .journals_nothing()
    );

    session.op(tag, played(1_500, 22, true));
    assert!(
        session
            .at(6_100, Event::Timer(TimerId::Checkpoint))
            .is_silent(),
        "nobody is speaking"
    );
}

// ---- Speech over the agent's speech ----------------------------------------

/// A session in which the agent is speaking and has been heard to say
/// "It shipped". Returns the tag of the response.
fn speaking() -> (Session, OpTag) {
    let mut session = Session::new();
    let tag = session.asked();
    session.answered(tag, "It shipped on Monday. It arrives on Thursday.", 3_000);
    session.op(tag, played(600, 10, false));
    (session, tag)
}

#[test]
fn speech_over_the_agent_lowers_it_and_a_barge_in_stops_it() {
    let (mut session, tag) = speaking();
    let over = session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    assert!(over.has(Command::Playout(tag, PlayoutCmd::Duck)));
    assert!(over.has(Command::Sense(SenseCmd::ClassifyOverlap {
        turn: TurnId(2),
        who: ANA
    })));
    assert!(over.has(Command::SetTimer(
        TimerId::Overlap(TurnId(2)),
        Some(MonoTime::from_millis(2_500))
    )));
    assert_eq!(
        session.kernel.view().turns().next().unwrap().ducking,
        Some(GenId(1))
    );

    let verdict = Event::Overlap {
        turn: TurnId(2),
        verdict: OverlapVerdict::BargeIn,
    };
    let cut = session.at(2_150, verdict);
    assert_eq!(
        cut.commands()[..3],
        [
            Command::SetTimer(TimerId::Overlap(TurnId(2)), None),
            Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 }),
            Command::SetTimer(TimerId::Gen(GenId(1)), None),
        ]
    );
    assert!(
        !cut.has(Command::Tts(tag, TtsCmd::Cancel)),
        "synthesis had finished"
    );
    assert!(
        !cut.has(Command::Llm(tag, LlmCmd::Cancel)),
        "so had the model"
    );
    assert_eq!(
        cut.fact_text(|fact| matches!(
            fact,
            Fact::UtteranceCommitted {
                truncated: true,
                ..
            }
        )),
        Some("It shipped")
    );
    assert!(cut.facts().contains(&Fact::EpochAdvanced {
        to: Epoch(1),
        cause: EpochCause::BargeIn
    }));
    assert!(!session.kernel.view().is_live(tag));

    // What playback reports afterwards carries a dead tag.
    assert!(session.op(tag, played(700, 10, false)).is_silent());
}

#[test]
fn a_backchannel_restores_the_volume_and_is_dismissed_with_its_words() {
    let (mut session, tag) = speaking();
    session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    let verdict = Event::Overlap {
        turn: TurnId(2),
        verdict: OverlapVerdict::Backchannel,
    };
    let resumed = session.at(2_150, verdict);
    assert!(resumed.has(Command::Playout(tag, PlayoutCmd::Unduck)));
    assert!(resumed.journals_nothing());
    assert!(session.kernel.view().is_live(tag));

    session.at(2_300, speech(ANA, VadEdge::End, 2_300));
    session.at(
        2_360,
        Event::Eou {
            turn: TurnId(2),
            p: Prob::ONE,
        },
    );
    let heard = session.at(2_500, transcript(ANA, 2_000, 2_300, "mm-hm"));
    assert_eq!(
        heard.facts(),
        [Fact::TurnDismissed {
            turn: TurnId(2),
            who: ANA,
            reason: Dismissal::Backchannel
        }]
    );
    assert!(heard.started().is_none());
    assert_eq!(session.items(), ["1: where is my order"]);
}

#[test]
fn noise_is_dismissed_as_noise() {
    let (mut session, _) = speaking();
    session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    session.at(
        2_150,
        Event::Overlap {
            turn: TurnId(2),
            verdict: OverlapVerdict::Noise,
        },
    );
    session.at(2_300, speech(ANA, VadEdge::End, 2_300));
    session.at(
        2_360,
        Event::Eou {
            turn: TurnId(2),
            p: Prob::ONE,
        },
    );
    let waited = session.at(4_360, Event::Timer(TimerId::Eou(TurnId(2))));
    assert_eq!(
        waited.facts(),
        [Fact::TurnDismissed {
            turn: TurnId(2),
            who: ANA,
            reason: Dismissal::Noise
        }]
    );
}

#[test]
fn without_a_verdict_voice_activity_decides() {
    // Still speaking after half a second: a barge-in.
    let (mut session, tag) = speaking();
    session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    let lasted = session.at(2_500, Event::Timer(TimerId::Overlap(TurnId(2))));
    assert!(lasted.has(Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 })));

    // Stopped before that: a backchannel, but not before the verdict has
    // had its half second.
    let (mut session, tag) = speaking();
    session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    let stopped = session.at(2_300, speech(ANA, VadEdge::End, 2_300));
    assert!(!stopped.has(Command::Playout(tag, PlayoutCmd::Unduck)));
    assert!(stopped.has(Command::SetTimer(
        TimerId::Overlap(TurnId(2)),
        Some(MonoTime::from_millis(2_500))
    )));
    let waited = session.at(2_500, Event::Timer(TimerId::Overlap(TurnId(2))));
    assert!(waited.has(Command::Playout(tag, PlayoutCmd::Unduck)));
}

#[test]
fn a_late_barge_in_verdict_still_counts_while_the_turn_is_open() {
    let (mut session, tag) = speaking();
    session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    session.at(2_300, speech(ANA, VadEdge::End, 2_300));
    session.at(2_500, Event::Timer(TimerId::Overlap(TurnId(2))));
    // Taken for a backchannel. Then the classifier answers.
    let verdict = Event::Overlap {
        turn: TurnId(2),
        verdict: OverlapVerdict::BargeIn,
    };
    let late = session.at(2_600, verdict.clone());
    assert!(late.has(Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 })));
    session.at(
        2_700,
        Event::Eou {
            turn: TurnId(2),
            p: Prob::ONE,
        },
    );
    let heard = session.at(2_800, transcript(ANA, 2_000, 2_300, "stop"));
    assert_eq!(heard.fact_text(|_| true), Some("stop"));
    assert!(heard.started().is_some());

    // The same verdict a second time does nothing.
    assert!(session.at(2_900, verdict).is_silent());
}

#[test]
fn a_turn_does_not_end_while_its_overlap_is_undecided() {
    let (mut session, tag) = speaking();
    session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    session.at(2_010, transcript(ANA, 2_000, 2_600, "cancel it"));
    session.at(2_020, speech(ANA, VadEdge::End, 2_600));
    let estimate = session.at(
        2_030,
        Event::Eou {
            turn: TurnId(2),
            p: Prob::ONE,
        },
    );
    assert!(
        estimate.journals_nothing(),
        "nobody knows yet what this speech is"
    );

    let verdict = Event::Overlap {
        turn: TurnId(2),
        verdict: OverlapVerdict::BargeIn,
    };
    let decided = session.at(2_040, verdict);
    // The agent's turn first, then the user's, then the new response.
    assert!(matches!(
        decided.facts()[..],
        [
            Fact::UtteranceCommitted {
                truncated: true,
                ..
            },
            Fact::EpochAdvanced { .. },
            Fact::TurnCommitted {
                turn: TurnId(2),
                ..
            },
        ]
    ));
    assert_eq!(
        decided.started(),
        Some(OpTag {
            generation: GenId(2),
            epoch: Epoch(1)
        })
    );
    assert!(!session.kernel.view().is_live(tag));
    assert_eq!(
        session.items(),
        [
            "1: where is my order",
            "agent: It shipped [cut]",
            "1: cancel it"
        ]
    );
}

#[test]
fn two_people_over_the_agent_keep_it_lowered_until_both_are_judged() {
    let (mut session, tag) = speaking();
    session.control(ControlInput::Join { who: BEN });
    let first = session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    assert!(first.has(Command::Playout(tag, PlayoutCmd::Duck)));
    let second = session.at(2_050, speech(BEN, VadEdge::Start, 2_050));
    assert!(
        !second.has(Command::Playout(tag, PlayoutCmd::Duck)),
        "it is lowered already"
    );

    let backchannel = |turn| Event::Overlap {
        turn: TurnId(turn),
        verdict: OverlapVerdict::Backchannel,
    };
    assert!(
        !session
            .at(2_150, backchannel(2))
            .has(Command::Playout(tag, PlayoutCmd::Unduck))
    );
    assert!(
        session
            .at(2_200, backchannel(3))
            .has(Command::Playout(tag, PlayoutCmd::Unduck))
    );
}

#[test]
fn an_overlap_with_a_response_that_has_ended_interrupts_nothing() {
    let (mut session, tag) = speaking();
    session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
    // The agent finishes by itself while the verdict is on its way.
    session.at(2_100, Event::Op(tag, played(3_000, 45, true)));
    assert_eq!(session.kernel.view().epoch(), Epoch(0));
    let verdict = Event::Overlap {
        turn: TurnId(2),
        verdict: OverlapVerdict::BargeIn,
    };
    let decided = session.at(2_150, verdict);
    assert!(decided.journals_nothing());
    assert_eq!(session.kernel.view().epoch(), Epoch(0));
    assert_eq!(session.kernel.view().turns().next().unwrap().ducking, None);
}

// ---- Control ---------------------------------------------------------------

#[test]
fn the_application_can_interrupt_and_can_speak() {
    let (mut session, tag) = speaking();
    let cut = session.control(ControlInput::Interrupt);
    assert!(cut.facts().contains(&Fact::EpochAdvanced {
        to: Epoch(1),
        cause: EpochCause::Interrupted
    }));
    assert!(!session.kernel.view().is_live(tag));
    assert!(
        session.control(ControlInput::Interrupt).is_silent(),
        "nothing holds the floor"
    );

    let said = session.control(ControlInput::Say {
        text: "One moment, please.".into(),
    });
    let greeting = OpTag {
        generation: GenId(2),
        epoch: Epoch(1),
    };
    assert_eq!(said.spoken(), ["One moment, please."]);
    assert!(said.has(Command::Tts(greeting, TtsCmd::Finish)));
    assert!(said.has(Command::Playout(greeting, PlayoutCmd::Start)));
    assert!(said.started().is_none(), "no language model is involved");

    session.op(greeting, segment(19, 1_200, true));
    let over = session.op(greeting, played(1_200, 19, true));
    assert_eq!(over.fact_text(|_| true), Some("One moment, please."));
    assert!(
        session
            .control(ControlInput::Say { text: " ".into() })
            .is_silent()
    );
}

#[test]
fn what_the_application_says_is_no_answer_and_no_answer_replaces_it() {
    let (mut session, _) = speaking();
    // The user types while the agent speaks: an answer is owed, and waits.
    session.at(
        2_000,
        Event::TextIn {
            who: ANA,
            text: "and the invoice".into(),
        },
    );
    assert!(session.kernel.view().reply_owed());

    let said = session.at(
        2_100,
        Event::Control(ControlInput::Say {
            text: "One moment.".into(),
        }),
    );
    let notice = OpTag {
        generation: GenId(2),
        epoch: Epoch(1),
    };
    assert!(said.has(Command::Playout(notice, PlayoutCmd::Start)));
    assert!(said.started().is_none());
    assert!(
        session.kernel.view().reply_owed(),
        "the user is still owed an answer"
    );

    // Once it has been said, the answer follows.
    session.op(notice, segment(11, 700, true));
    let over = session.at(3_000, Event::Op(notice, played(700, 11, true)));
    assert_eq!(
        over.started(),
        Some(OpTag {
            generation: GenId(3),
            epoch: Epoch(1)
        })
    );
    assert!(!session.kernel.view().reply_owed());

    // Asked for in the middle of a user's turn, it waits for the turn to
    // end and is said before the answer to it.
    let mut session = Session::new();
    session.at(0, speech(ANA, VadEdge::Start, 0));
    let said = session.at(
        100,
        Event::Control(ControlInput::Say {
            text: "Are you there?".into(),
        }),
    );
    let question = OpTag {
        generation: GenId(1),
        epoch: Epoch(0),
    };
    assert_eq!(said.spoken(), ["Are you there?"]);
    assert!(!said.has(Command::Playout(question, PlayoutCmd::Start)));
    session.at(900, speech(ANA, VadEdge::End, 900));
    session.at(
        960,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::ONE,
        },
    );
    let committed = session.at(1_100, transcript(ANA, 0, 900, "yes i am here"));
    assert!(committed.has(Command::Playout(question, PlayoutCmd::Start)));
    assert!(committed.started().is_none());
    assert!(session.kernel.view().is_live(question));
}

#[test]
fn a_timer_only_wakes_the_session() {
    let mut session = Session::new();
    let tag = session.asked();
    // The response makes progress half a minute in, which gives it another
    // minute. The timer set for the first minute fires all the same.
    session.at(30_000, Event::Op(tag, delta("It ")));
    assert!(
        session
            .at(61_100, Event::Timer(TimerId::Gen(GenId(1))))
            .is_silent()
    );
    assert!(session.kernel.view().is_live(tag));

    // A timer for the longest turn, a second into the turn.
    session.at(62_000, speech(ANA, VadEdge::Start, 62_000));
    assert!(
        session
            .at(63_000, Event::Timer(TimerId::TurnMax(TurnId(2))))
            .is_silent()
    );
    assert_eq!(session.kernel.view().turns().count(), 1);

    // Timers for a shutdown nobody asked for.
    assert!(session.at(64_000, Event::Timer(TimerId::Drain)).is_silent());
    assert!(session.at(64_000, Event::Timer(TimerId::Grace)).is_silent());
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Running);
}

#[test]
fn a_session_with_nothing_under_way_closes_at_once() {
    let mut session = Session::new();
    let closed = session.at(
        1_000,
        Event::Control(ControlInput::Close {
            reason: CloseReason::Requested,
        }),
    );
    let stages = closed
        .facts()
        .into_iter()
        .map(|fact| match fact {
            Fact::LifecycleChanged { to, reason } => (to, reason),
            other => panic!("{other:?}"),
        })
        .collect::<Vec<_>>();
    let why = Some(CloseReason::Requested);
    assert_eq!(
        stages,
        [
            (Lifecycle::Draining, why),
            (Lifecycle::Closing, why),
            (Lifecycle::Closed, why)
        ]
    );
    assert!(closed.has(Command::Stt(SttCmd::Close { who: ANA })));
    assert!(closed.has(Command::Emit(ClientEvent::Lifecycle(Lifecycle::Closed))));
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Closed);

    // A closed session says nothing more, whatever it is told.
    assert!(session.step(speech(ANA, VadEdge::Start, 2_000)).is_silent());
    assert!(session.control(ControlInput::Join { who: BEN }).is_silent());
    assert!(session.step(Event::Timer(TimerId::Grace)).is_silent());
}

#[test]
fn a_closing_session_lets_the_agent_finish_and_starts_nothing_new() {
    let (mut session, tag) = speaking();
    let asked = session.at(
        2_000,
        Event::Control(ControlInput::Close {
            reason: CloseReason::Abandoned,
        }),
    );
    assert_eq!(asked.facts().len(), 1);
    assert!(asked.has(Command::SetTimer(
        TimerId::Drain,
        Some(MonoTime::from_millis(7_000))
    )));
    assert!(asked.has(Command::SetTimer(
        TimerId::Grace,
        Some(MonoTime::from_millis(9_000))
    )));
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Draining);
    assert!(
        session
            .control(ControlInput::Close {
                reason: CloseReason::Failure
            })
            .is_silent()
    );

    // The caller is still heard, and still gets no reply.
    let more = session.at(
        2_100,
        Event::TextIn {
            who: ANA,
            text: "one more thing".into(),
        },
    );
    assert_eq!(more.facts().len(), 1);
    assert!(session.kernel.view().reply_owed());

    // The agent finishes its sentence, and the session closes.
    let over = session.at(4_000, Event::Op(tag, played(3_000, 45, true)));
    assert!(matches!(
        over.facts()[..],
        [
            Fact::UtteranceCommitted {
                truncated: false,
                ..
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Closing,
                ..
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Closed,
                ..
            },
        ]
    ));
    assert!(over.started().is_none());
    assert!(over.has(Command::SetTimer(TimerId::Drain, None)));
}

#[test]
fn a_closing_session_stops_what_does_not_finish_in_time() {
    let (mut session, tag) = speaking();
    session.control(ControlInput::Join { who: BEN });
    session.at(
        2_000,
        Event::Control(ControlInput::Close {
            reason: CloseReason::Requested,
        }),
    );
    session.at(2_500, speech(BEN, VadEdge::Start, 2_500));
    session.at(
        2_600,
        Event::Overlap {
            turn: TurnId(2),
            verdict: OverlapVerdict::Backchannel,
        },
    );

    // Any event after the deadline will do: here it is not even the timer.
    let late = session.at(7_000, Event::Timer(TimerId::Checkpoint));
    assert!(late.has(Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 })));
    assert!(matches!(
        late.facts()[..],
        [
            Fact::UtteranceProgress { .. },
            Fact::LifecycleChanged {
                to: Lifecycle::Closing,
                ..
            },
            Fact::UtteranceCommitted {
                truncated: true,
                ..
            },
            Fact::EpochAdvanced {
                cause: EpochCause::Closing,
                ..
            },
            Fact::TurnDismissed {
                who: BEN,
                reason: Dismissal::Closed,
                ..
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Closed,
                ..
            },
        ]
    ));
    assert_eq!(session.kernel.view().turns().count(), 0);
    assert_eq!(session.kernel.view().generations().count(), 0);
}

#[test]
fn a_kernel_that_lost_its_lease_goes_silent() {
    let (mut session, tag) = speaking();
    let before = session.kernel.view().next_seq();
    assert!(session.control(ControlInput::LeaseLost).is_silent());
    assert!(session.op(tag, played(3_000, 45, true)).is_silent());
    assert!(
        session
            .control(ControlInput::Close {
                reason: CloseReason::Requested
            })
            .is_silent()
    );
    assert!(session.step(speech(ANA, VadEdge::Start, 9_000)).is_silent());
    assert_eq!(session.kernel.view().next_seq(), before);
}

#[test]
fn the_connection_state_is_remembered() {
    let mut session = Session::new();
    assert!(session.kernel.view().is_attached());
    assert!(session.control(ControlInput::Detached).is_silent());
    assert!(!session.kernel.view().is_attached());
    session.control(ControlInput::Attached);
    assert!(session.kernel.view().is_attached());
}

// ---- The journal -----------------------------------------------------------

#[test]
fn facts_are_numbered_without_gaps_and_rebuild_the_conversation() {
    let mut session = Session::new();
    let mut journal = Vec::new();
    let mut arena = waveport_domain::text::TextArena::default();
    let snapshot = session.kernel.snapshot();
    assert_eq!(snapshot.next_seq(), Seq(2), "the caller has joined");

    // Keeps what a step journals, with its text moved to one arena.
    let mut keep = |session: &Session| {
        for stamped in &session.out.facts {
            let mut fact = stamped.fact;
            let moved = arena.push(fact.text().map_or("", |text| session.out.text.get(text)));
            match &mut fact {
                Fact::TurnCommitted { text, .. } | Fact::UtteranceCommitted { text, .. } => {
                    *text = moved;
                }
                Fact::UtteranceProgress { heard, .. } => *heard = moved,
                _ => {}
            }
            journal.push(waveport_domain::fact::Stamped {
                seq: stamped.seq,
                fact,
            });
        }
    };
    let tag = session.asked();
    keep(&session);
    session.answered(tag, "It shipped on Monday. ", 1_500);
    session.op(tag, played(1_500, 22, true));
    keep(&session);
    session.at(
        5_000,
        Event::TextIn {
            who: ANA,
            text: "thanks".into(),
        },
    );
    keep(&session);

    let seqs = journal
        .iter()
        .map(|stamped| stamped.seq.get())
        .collect::<Vec<_>>();
    assert_eq!(seqs, [2, 3, 4]);

    let rebuilt = Kernel::restore(
        KernelConfig::default(),
        BoxedPolicies::default(),
        &snapshot,
        &journal,
        &arena,
    )
    .unwrap();
    let conversation = |kernel: &Kernel<BoxedPolicies>| {
        kernel
            .view()
            .items()
            .map(|item| format!("{item:?}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(conversation(&rebuilt), conversation(&session.kernel));
    assert_eq!(rebuilt.view().next_seq(), session.kernel.view().next_seq());
    assert!(rebuilt.view().reply_owed());
}

#[test]
fn the_same_events_give_the_same_session() {
    let run = || {
        let (mut session, tag) = speaking();
        session.at(2_000, speech(ANA, VadEdge::Start, 2_000));
        session.at(
            2_150,
            Event::Overlap {
                turn: TurnId(2),
                verdict: OverlapVerdict::BargeIn,
            },
        );
        session.op(tag, delta("stale"));
        session.kernel.fingerprint()
    };
    assert_eq!(run(), run());
    let (other, _) = speaking();
    assert_ne!(run(), other.kernel.fingerprint());
}

#[test]
fn a_parked_session_carries_on() {
    let (mut session, tag) = speaking();
    let before = session.kernel.fingerprint();
    let held = session.kernel.heap_bytes();
    session.kernel.park();
    assert!(session.kernel.heap_bytes() <= held);
    assert_eq!(
        session.kernel.fingerprint(),
        before,
        "parking changes no state"
    );
    let over = session.op(tag, played(3_000, 45, true));
    assert_eq!(
        over.fact_text(|_| true),
        Some("It shipped on Monday. It arrives on Thursday.")
    );
    let _ = Duration::ZERO;
}
