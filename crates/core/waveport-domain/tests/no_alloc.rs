//! Steps that must leave the heap alone.
//!
//! Most of what a session does is small and frequent: a position report
//! from playback ten times a second, a voice starting or stopping, a piece
//! of model text. Once a session is warm none of these may call the
//! allocator. What may is rare and ends something: a turn or a response
//! entering the conversation, and a response beginning.

use waveport_alloc_counter::{CountingAllocator, is_installed, measure};
use waveport_domain::event::{
    ControlInput, Event, Finish, OpEvent, OverlapVerdict, TimerId, ToolEvent, VadEdge,
};
use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId, Prob, Seq, ToolCallId, TurnId};
use waveport_domain::kernel::Kernel;
use waveport_domain::out::Out;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::system();

const ANA: ParticipantId = ParticipantId(1);

struct Session {
    kernel: Kernel,
    out: Out,
}

impl Session {
    /// Takes a step that may use the heap.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "each call builds its event in place"
    )]
    fn step(&mut self, millis: u64, event: Event) {
        self.kernel
            .step(MonoTime::from_millis(millis), &event, &mut self.out);
    }

    /// Takes a step that may not. The event is built by the caller, so
    /// what it owns is not counted.
    #[track_caller]
    fn quiet_step(&mut self, what: &str, millis: u64, event: &Event) {
        let now = MonoTime::from_millis(millis);
        let ((), stats) = measure(|| self.kernel.step(now, event, &mut self.out));
        assert_eq!(stats.calls(), 0, "{what} called the allocator: {stats:?}");
    }
}

fn speech(edge: VadEdge, at_ms: u64) -> Event {
    Event::Speech {
        who: ANA,
        edge,
        at: MediaTime::from_millis(at_ms),
    }
}

fn transcript(from_ms: u64, to_ms: u64, text: &str, is_final: bool) -> Event {
    Event::Transcript {
        who: ANA,
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

fn segment(tag: OpTag, text_end: u32, audio_ms: u32, last: bool) -> Event {
    let segment = OpEvent::TtsSegment {
        text_end,
        audio_end: MediaDur::from_millis(audio_ms),
        last,
    };
    Event::Op(tag, segment)
}

fn played(tag: OpTag, millis: u32, heard_text: u32) -> Event {
    let position = MediaDur::from_millis(millis);
    let played = OpEvent::Played {
        handed: position,
        acked: position,
        heard_text,
        drained: false,
    };
    Event::Op(tag, played)
}

/// A session in which one exchange has happened, ended by a barge-in, so
/// that every buffer has been used once.
fn warm() -> Session {
    let mut session = Session {
        kernel: Kernel::default(),
        out: Out::new(),
    };
    let first = OpTag {
        generation: GenId(1),
        epoch: Epoch(0),
    };
    session.step(0, Event::Control(ControlInput::Join { who: ANA }));
    session.step(0, speech(VadEdge::Start, 0));
    session.step(900, speech(VadEdge::End, 900));
    session.step(
        960,
        Event::Eou {
            turn: TurnId(1),
            p: Prob::ONE,
        },
    );
    session.step(1_100, transcript(0, 900, "where is my order", true));
    session.step(
        1_300,
        delta(first, "It shipped on Monday. It arrives on Thursday. "),
    );
    session.step(1_400, segment(first, 22, 1_500, false));
    session.step(1_500, played(first, 600, 10));
    session.step(2_300, Event::Timer(TimerId::Checkpoint));
    session.step(2_400, speech(VadEdge::Start, 2_400));
    let barge_in = Event::Overlap {
        turn: TurnId(2),
        verdict: OverlapVerdict::BargeIn,
    };
    session.step(2_550, barge_in);
    session.step(3_000, speech(VadEdge::End, 3_000));
    session.step(
        3_060,
        Event::Eou {
            turn: TurnId(2),
            p: Prob::ONE,
        },
    );
    // This commits the second turn and begins the second response.
    session.step(3_200, transcript(2_400, 3_000, "no the other one", true));
    assert!(session.kernel.view().is_live(OpTag {
        generation: GenId(2),
        epoch: Epoch(1)
    }));
    session
}

#[test]
fn the_frequent_steps_do_not_touch_the_heap() {
    assert!(is_installed(), "nothing would be counted");
    let mut session = warm();
    let old = OpTag {
        generation: GenId(1),
        epoch: Epoch(0),
    };
    let tag = OpTag {
        generation: GenId(2),
        epoch: Epoch(1),
    };

    // Every event is built first: an event owns its text.
    let held = delta(tag, "The other ");
    let released = delta(tag, "one shipped on Tuesday. It arrives ");
    let audio = segment(tag, 33, 2_000, false);
    let early = played(tag, 300, 5);
    let later = played(tag, 900, 14);
    let checkpoint = Event::Timer(TimerId::Checkpoint);
    let over = speech(VadEdge::Start, 5_000);
    let interim = transcript(5_000, 5_200, "mm", false);
    let backchannel = Event::Overlap {
        turn: TurnId(3),
        verdict: OverlapVerdict::Backchannel,
    };
    let stopped = speech(VadEdge::End, 5_300);
    let unsure = Event::Eou {
        turn: TurnId(3),
        p: Prob::from_basis_points(1_000),
    };
    let more = delta(tag, "on Friday. ");
    let done = Event::Op(
        tag,
        OpEvent::LlmDone {
            finish: Finish::Stop,
        },
    );
    let stale = [
        delta(old, "stale"),
        segment(old, 45, 3_000, true),
        played(old, 3_000, 45),
        Event::Timer(TimerId::Gen(GenId(1))),
        Event::Timer(TimerId::Eou(TurnId(1))),
        Event::Timer(TimerId::Overlap(TurnId(2))),
        Event::Timer(TimerId::TurnMax(TurnId(2))),
        Event::Timer(TimerId::Overlap(TurnId(3))),
        Event::Eou {
            turn: TurnId(1),
            p: Prob::ONE,
        },
        Event::Overlap {
            turn: TurnId(2),
            verdict: OverlapVerdict::BargeIn,
        },
        Event::Durable(Seq(3)),
        Event::Tool(
            ToolCallId(1),
            ToolEvent::Completed {
                result: "{}".into(),
            },
        ),
        Event::Control(ControlInput::Detached),
        Event::Control(ControlInput::Attached),
        speech(VadEdge::Start, 100),
        speech(VadEdge::End, 100),
    ];

    session.quiet_step("model text that is held back", 3_400, &held);
    session.quiet_step("model text that goes to synthesis", 3_450, &released);
    session.quiet_step("audio from synthesis", 3_600, &audio);
    session.quiet_step("a position report", 3_900, &early);
    session.quiet_step("a second position report", 4_500, &later);
    session.quiet_step("a checkpoint with progress", 4_600, &checkpoint);
    assert_eq!(
        session.out.facts.len(),
        1,
        "the checkpoint recorded progress"
    );
    session.quiet_step("speech over the agent", 5_000, &over);
    session.quiet_step("an interim transcript", 5_100, &interim);
    session.quiet_step("a backchannel verdict", 5_150, &backchannel);
    session.quiet_step("the end of speech", 5_300, &stopped);
    session.quiet_step("an unsure end-of-turn estimate", 5_360, &unsure);
    session.quiet_step("model text while someone speaks", 5_400, &more);
    session.quiet_step("the end of the model's text", 5_450, &done);
    for event in &stale {
        session.quiet_step(&format!("{event:?}"), 5_500, event);
    }
    // The session is where the steps above should have left it.
    let view = session.kernel.view();
    assert!(view.is_live(tag));
    assert_eq!(view.turns().count(), 1);
}

#[test]
fn a_parked_session_is_small() {
    let mut session = warm();
    session.kernel.park();
    let held = session.kernel.heap_bytes();
    assert!(held <= 32 * 1024, "a parked session holds {held} bytes");
}
