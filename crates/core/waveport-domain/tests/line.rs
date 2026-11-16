//! A line that drops, one rule at a time: what is cut off, what waits,
//! what is owed when the line is back, and when the wait is over.
//!
//! The stories are `line_drops` and `line_stays_gone` in `scenarios/`.

#[allow(
    dead_code,
    reason = "shared with the other test files, each of which uses part of it"
)]
mod support;

use core::time::Duration;

use waveport_domain::command::{ClientEvent, Command, LlmCause, LlmCmd, PlayoutCmd, SttCmd};
use waveport_domain::config::{KernelConfig, Timing};
use waveport_domain::event::{CloseReason, ControlInput, Event, Finish, TimerId, VadEdge};
use waveport_domain::fact::{EpochCause, Fact, Lifecycle};
use waveport_domain::ids::{BatchId, Epoch, GenId, OpTag, ToolCallId, TurnId};
use waveport_domain::time::{MediaSpan, MediaTime, MonoTime};

use self::support::{ANA, Emitted, HANG_UP, LOOKUP, Session, desk, played};

/// What the agent says, and how much of it is its first sentence.
const ANSWER: &str = "It shipped on Monday. It arrives on Thursday.";
const FIRST_SENTENCE: u32 = 21;
/// How long a session waits for its line unless it is told otherwise.
const WAIT_MS: u64 = 30_000;

fn dropped() -> Event {
    Event::Control(ControlInput::Detached)
}

fn back() -> Event {
    Event::Control(ControlInput::Attached)
}

fn wait_over() -> Event {
    Event::Timer(TimerId::Detach)
}

/// What a response started in this step was started for.
fn started_for(emitted: &Emitted) -> Option<LlmCause> {
    emitted.started().and_then(|(_, start)| match start {
        LlmCmd::Start { cause, .. } => Some(cause),
        _ => None,
    })
}

fn waits_until(emitted: &Emitted, millis: u64) -> bool {
    emitted.has(Command::SetTimer(
        TimerId::Detach,
        Some(MonoTime::from_millis(millis)),
    ))
}

/// A session in which the agent is in the middle of its answer: all of
/// it is written, and its first sentence was heard.
fn speaking() -> (Session, OpTag) {
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, ANSWER);
    session.done(tag, Finish::Stop);
    session.voiced(tag, ANSWER.len());
    session.op(tag, played(900, FIRST_SENTENCE, false));
    (session, tag)
}

// ---- What a drop cuts off ---------------------------------------------------

#[test]
fn a_line_that_drops_cuts_the_agent_off_where_it_was_heard() {
    let (mut session, tag) = speaking();
    let now = session.now_ms();
    let cut = session.step(dropped());

    assert!(cut.has(Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 })));
    assert!(matches!(
        cut.told()[..],
        [
            (
                Fact::UtteranceCommitted {
                    truncated: true,
                    answers: None,
                    ..
                },
                "It shipped on Monday."
            ),
            (
                Fact::EpochAdvanced {
                    to: Epoch(1),
                    cause: EpochCause::Interrupted
                },
                _
            ),
        ]
    ));
    assert!(waits_until(&cut, now + WAIT_MS));
    let view = session.kernel.view();
    assert!(!view.is_attached());
    assert_eq!(
        view.gone_until(),
        Some(MonoTime::from_millis(now + WAIT_MS))
    );
    assert_eq!(view.lifecycle(), Lifecycle::Running);
    assert_eq!(
        session.items(),
        ["1: can you help me", "agent 0: It shipped on Monday. [cut]"]
    );

    // Told of the same drop again, the session goes on waiting from the
    // first time.
    assert!(session.after(1_000, dropped()).is_silent());
    assert_eq!(
        session.kernel.view().gone_until(),
        Some(MonoTime::from_millis(now + WAIT_MS))
    );
}

#[test]
fn a_reply_that_was_being_written_is_stopped_and_leaves_nothing() {
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, "It shipped ");
    let cut = session.step(dropped());

    assert!(cut.has(Command::Llm(tag, LlmCmd::Cancel)));
    assert_eq!(
        cut.facts(),
        [Fact::EpochAdvanced {
            to: Epoch(1),
            cause: EpochCause::Interrupted
        }]
    );
    assert_eq!(session.items(), ["1: can you help me"]);
}

#[test]
fn speech_that_the_drop_cut_off_is_a_turn_with_the_words_that_come() {
    let mut session = Session::new();
    session.exchange("Hello.");
    let from = session.now_ms() + 100;
    let began = Event::Speech {
        who: ANA,
        edge: VadEdge::Start,
        at: MediaTime::from_millis(from),
    };
    session.at(from, began);

    // Nobody is going to say where the speech ends.
    let cut = session.at(from + 700, dropped());
    assert!(cut.has(Command::Emit(ClientEvent::UserSpeaking {
        who: ANA,
        turn: TurnId(2),
        speaking: false,
    })));
    assert!(cut.has(Command::Stt(SttCmd::Finalize { who: ANA })));
    assert_eq!(session.kernel.view().turns().count(), 1);

    // What was heard of it comes, and is the turn. Its reply waits.
    let span = MediaSpan::new(
        MediaTime::from_millis(from),
        MediaTime::from_millis(from + 700),
    );
    let words = session.after(
        300,
        Event::Transcript {
            who: ANA,
            span,
            text: "where is my".into(),
            is_final: true,
        },
    );
    assert!(matches!(
        words.told()[..],
        [(
            Fact::TurnCommitted {
                turn: TurnId(2),
                ..
            },
            "where is my"
        )]
    ));
    assert!(words.started().is_none());

    let returned = session.after(2_000, back());
    assert_eq!(
        started_for(&returned),
        Some(LlmCause::Reply { turn: TurnId(2) })
    );
}

// ---- While the line is gone -------------------------------------------------

#[test]
fn nothing_is_said_while_the_line_is_gone() {
    let mut session = Session::new();
    session.exchange("It shipped on Monday.");
    let now = session.now_ms();
    // Nothing was being said, and nothing is owed: the session only
    // begins to wait.
    let gone = session.step(dropped());
    assert_eq!(gone.facts(), Vec::new());
    assert!(waits_until(&gone, now + WAIT_MS));
    assert_eq!(gone.cmds.len(), 1);

    // What the application has the agent say would be heard by nobody.
    let say = ControlInput::Say {
        text: "Are you still there?".into(),
    };
    assert!(session.control(say).is_silent());

    // Something typed is taken, and its reply waits for the line.
    let typed = session.after(
        500,
        Event::TextIn {
            who: ANA,
            text: "and my refund".into(),
        },
    );
    assert!(matches!(
        typed.facts()[..],
        [Fact::TurnCommitted {
            turn: TurnId(2),
            ..
        }]
    ));
    assert!(typed.asks_nothing());
    assert!(session.kernel.view().reply_owed());
    assert!(
        session
            .after(500, Event::Timer(TimerId::Checkpoint))
            .is_silent(),
        "whatever else happens"
    );

    let returned = session.after(1_000, back());
    assert_eq!(
        started_for(&returned),
        Some(LlmCause::Reply { turn: TurnId(2) })
    );
    assert!(returned.has(Command::SetTimer(TimerId::Detach, None)));
    assert!(session.kernel.view().is_attached());
}

#[test]
fn tool_calls_go_on_while_the_line_is_gone_and_their_results_wait() {
    let mut session = Session::new();
    let tag = session.asked();
    assert_eq!(session.proposes(tag, LOOKUP, "{}").sent().len(), 1);

    // The response that asked is cut off. The call it sent goes on.
    let cut = session.step(dropped());
    assert!(cut.has(Command::Llm(tag, LlmCmd::Cancel)));
    assert!(cut.sends_nothing() && cut.ends_nothing());
    assert_eq!(session.kernel.view().calls().count(), 1);

    // A line that is back before the result starts nothing: a reply begun
    // now would ask for the same call again.
    let early = session.after(500, back());
    assert!(early.started().is_none());

    session.after(100, dropped());
    let result = session.after(400, Event::Timer(TimerId::Checkpoint));
    assert!(result.is_silent());
    let result = session.returns(ToolCallId(1), r#"{"shipped":"Monday"}"#);
    assert!(
        result
            .facts()
            .contains(&Fact::BatchResolved { batch: BatchId(1) })
    );
    assert!(result.started().is_none(), "the result waits for the line");

    let returned = session.after(500, back());
    assert_eq!(
        started_for(&returned),
        Some(LlmCause::Continuation { batch: BatchId(1) })
    );
}

#[test]
fn a_call_that_was_not_sent_is_withdrawn_and_the_reply_is_owed_again() {
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, "Goodbye then. ");
    // It is sent only when the agent has said what it was saying.
    assert!(session.proposes(tag, HANG_UP, "{}").sends_nothing());

    let cut = session.step(dropped());
    assert_eq!(cut.ended().len(), 1, "{cut:?}");
    assert!(
        cut.facts()
            .contains(&Fact::BatchResolved { batch: BatchId(1) })
    );
    // Nothing of the batch ran, so nothing is owed for it. The turn is
    // owed its reply.
    let returned = session.after(500, back());
    assert_eq!(
        started_for(&returned),
        Some(LlmCause::Reply { turn: TurnId(1) })
    );
}

// ---- When the line is back --------------------------------------------------

#[test]
fn the_reply_that_was_cut_off_is_given_again_when_the_line_is_back() {
    let (mut session, _) = speaking();
    session.step(dropped());
    let returned = session.after(2_000, back());

    assert!(returned.has(Command::SetTimer(TimerId::Detach, None)));
    let (tag, start) = returned.started().unwrap();
    assert_eq!(
        tag,
        OpTag {
            generation: GenId(2),
            epoch: Epoch(1)
        }
    );
    // The model is shown the turn and what was heard of the first reply.
    let LlmCmd::Start { cause, context, .. } = start else {
        panic!("{start:?}");
    };
    assert_eq!(cause, LlmCause::Reply { turn: TurnId(1) });
    assert_eq!(context, session.kernel.view().context_version());
    assert_eq!(session.items().len(), 2);

    // That the line is there is no news the second time.
    assert!(session.step(back()).is_silent());
    // And the timer of a wait that is over ends nothing.
    assert!(session.after(WAIT_MS, wait_over()).is_silent());
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Running);
}

#[test]
fn the_news_of_a_tool_that_was_cut_off_is_given_again() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    session.done(tag, Finish::ToolCalls);
    let result = session.returns(ToolCallId(1), r#"{"shipped":"Monday"}"#);
    let batch = BatchId(1);
    assert_eq!(started_for(&result), Some(LlmCause::Continuation { batch }));
    let (telling, _) = result.started().unwrap();
    session.writes(telling, "It shipped ");

    let cut = session.step(dropped());
    assert!(cut.has(Command::Llm(telling, LlmCmd::Cancel)));
    let returned = session.after(1_000, back());
    assert_eq!(
        started_for(&returned),
        Some(LlmCause::Continuation { batch })
    );
}

#[test]
fn what_the_application_had_the_agent_say_is_not_said_again() {
    let mut session = Session::new();
    session.exchange("It shipped on Monday.");
    let said = session.control(ControlInput::Say {
        text: "Is there anything else?".into(),
    });
    assert!(said.speaks());
    let cut = session.step(dropped());
    assert!(matches!(
        cut.facts()[..],
        [Fact::EpochAdvanced {
            cause: EpochCause::Interrupted,
            ..
        }]
    ));
    assert!(session.after(1_000, back()).started().is_none());
}

#[test]
fn a_reply_the_caller_stopped_is_not_given_again_by_a_line_that_drops() {
    let (mut session, _) = speaking();
    // The caller, or the application for them, stops the agent. Nothing
    // is owed after that until someone says something.
    session.control(ControlInput::Interrupt);
    assert!(!session.kernel.view().reply_owed());
    session.after(500, dropped());
    let returned = session.after(500, back());
    assert!(returned.started().is_none());
    assert!(!session.kernel.view().reply_owed());
}

// ---- When the wait is over --------------------------------------------------

#[test]
fn a_line_that_stays_gone_closes_the_session_when_the_wait_is_over() {
    let (mut session, _) = speaking();
    session.at(10_000, dropped());
    // The timer only wakes the session. One that fires early ends
    // nothing.
    assert!(session.at(10_000 + WAIT_MS - 1, wait_over()).is_silent());

    let over = session.at(10_000 + WAIT_MS, wait_over());
    let abandoned = Some(CloseReason::Abandoned);
    assert_eq!(
        over.facts(),
        [
            Fact::LifecycleChanged {
                to: Lifecycle::Draining,
                reason: abandoned
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Closing,
                reason: abandoned
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Closed,
                reason: abandoned
            },
        ]
    );
    assert!(over.has(Command::SetTimer(TimerId::Detach, None)));
    assert!(over.has(Command::Emit(ClientEvent::Lifecycle(Lifecycle::Closed))));
    assert_eq!(
        session.items(),
        ["1: can you help me", "agent 0: It shipped on Monday. [cut]"]
    );
    // A line that comes back to a session that is closed finds nobody.
    assert!(session.after(100, back()).is_silent());
}

#[test]
fn a_wait_begins_anew_each_time_the_line_drops() {
    let (mut session, _) = speaking();
    session.at(10_000, dropped());
    session.at(12_000, back());
    // The timer of the first wait was cancelled. If it fires all the
    // same, the line is there.
    assert!(session.at(10_000 + WAIT_MS, wait_over()).is_silent());

    let again = session.at(50_000, dropped());
    assert!(waits_until(&again, 50_000 + WAIT_MS));
    // The timer of the first wait is not the end of the second.
    let early = session.at(50_000 + WAIT_MS - 1, wait_over());
    assert_eq!(early.facts(), Vec::new());
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Running);
}

#[test]
fn told_again_that_its_line_is_gone_a_session_that_has_waited_closes() {
    let (mut session, _) = speaking();
    session.at(10_000, dropped());
    assert!(session.at(10_000 + WAIT_MS - 1, dropped()).is_silent());
    // The timer is late, and the news of the line comes first.
    let over = session.at(10_000 + WAIT_MS, dropped());
    assert!(over.facts().contains(&Fact::LifecycleChanged {
        to: Lifecycle::Closed,
        reason: Some(CloseReason::Abandoned)
    }));
}

#[test]
fn a_session_that_waits_no_time_closes_when_its_line_drops() {
    let timing = Timing {
        detach_wait: Duration::ZERO,
        ..Timing::default()
    };
    let mut session = Session::with(KernelConfig { timing, ..desk() });
    let tag = session.asked();
    session.writes(tag, ANSWER);
    session.done(tag, Finish::Stop);
    session.voiced(tag, ANSWER.len());
    session.op(tag, played(900, FIRST_SENTENCE, false));

    let gone = session.step(dropped());
    assert!(matches!(
        gone.facts()[..],
        [
            Fact::UtteranceCommitted {
                truncated: true,
                ..
            },
            Fact::EpochAdvanced { .. },
            Fact::LifecycleChanged {
                to: Lifecycle::Draining,
                reason: Some(CloseReason::Abandoned)
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
    let set = gone
        .cmds
        .iter()
        .any(|(command, _)| matches!(command, Command::SetTimer(TimerId::Detach, Some(_))));
    assert!(!set, "there is nothing to wait for");
}

#[test]
fn a_session_asked_to_close_while_its_line_is_gone_closes_as_it_was_asked() {
    let (mut session, _) = speaking();
    session.at(10_000, dropped());
    let asked = session.at(
        11_000,
        Event::Control(ControlInput::Close {
            reason: CloseReason::Requested,
        }),
    );
    let requested = Some(CloseReason::Requested);
    assert!(matches!(
        asked.facts()[..],
        [
            Fact::LifecycleChanged {
                to: Lifecycle::Draining,
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
    assert!(
        asked.facts().iter().all(
            |fact| matches!(fact, Fact::LifecycleChanged { reason, .. } if *reason == requested)
        )
    );
    // The wait is over with the session.
    assert!(asked.has(Command::SetTimer(TimerId::Detach, None)));
}

#[test]
fn a_closing_session_whose_line_drops_stops_talking_and_waits_for_nothing() {
    let (mut session, tag) = speaking();
    session.at(
        10_000,
        Event::Control(ControlInput::Close {
            reason: CloseReason::Requested,
        }),
    );
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Draining);

    // It was finishing what it was saying. Nobody hears the rest.
    let gone = session.at(10_500, dropped());
    assert!(gone.has(Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 })));
    let set = gone
        .cmds
        .iter()
        .any(|(command, _)| matches!(command, Command::SetTimer(TimerId::Detach, Some(_))));
    assert!(!set, "it is closing already");
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Closed);
    assert!(gone.facts().contains(&Fact::LifecycleChanged {
        to: Lifecycle::Closed,
        reason: Some(CloseReason::Requested)
    }));
}
