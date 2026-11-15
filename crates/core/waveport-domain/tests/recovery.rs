//! A session rebuilt from its journal: what the journal knows, and what
//! the first step does about everything it does not.

#[allow(
    dead_code,
    reason = "shared with the other test files, each of which uses part of it"
)]
mod support;

use waveport_domain::command::{Command, LlmCause, LlmCmd};
use waveport_domain::event::{
    CloseReason, ControlInput, Event, Finish, HandoffContext, TimerId, VadEdge,
};
use waveport_domain::fact::{EpochCause, Fact, Lifecycle, NoticeKind, ToolOutcome};
use waveport_domain::ids::{Epoch, GenId, Seq, ToolCallId, TurnId};
use waveport_domain::kernel::{Kernel, RestoreError};
use waveport_domain::policy::BoxedPolicies;
use waveport_domain::time::MediaTime;

use self::support::{
    ANA, BILLING, FRONT, HANG_UP, LOOKUP, REFUND, RESEARCH, SYNC, Session, desk, played,
};

const NOTE: &str = "the session was interrupted by a failure and has been restored; what was under way then was lost";

/// Whether what the journal adds up to is what the session holds, from
/// the session's beginning and from every snapshot in `earlier`.
fn agrees(session: &Session, earlier: &[waveport_domain::kernel::Snapshot]) -> bool {
    let live = session.kernel.snapshot();
    session.rebuilt(&session.origin) == live
        && earlier.iter().all(|from| session.rebuilt(from) == live)
}

// ---- What the journal knows ------------------------------------------------

#[test]
fn a_snapshot_and_the_facts_since_add_up_to_the_session() {
    let mut session = Session::new();
    let mut snapshots = Vec::new();
    let mut check = |session: &Session| {
        assert!(agrees(session, &snapshots));
        snapshots.push(session.kernel.snapshot());
    };
    check(&session);

    // An exchange, with speech recorded on the way.
    let tag = session.asked();
    check(&session);
    let text = "It shipped on Monday. It arrives on Thursday.";
    session.writes(tag, text);
    session.done(tag, Finish::Stop);
    session.voiced(tag, text.len());
    session.op(tag, played(900, 10, false));
    let progress = session.after(1_000, Event::Timer(TimerId::Checkpoint));
    assert!(matches!(
        progress.facts()[..],
        [Fact::UtteranceProgress { .. }]
    ));
    check(&session);
    session.played_out(tag, text.len());
    check(&session);

    // Tool calls in every state, with a turn open and a response under
    // way, none of which the journal knows.
    let tag = session.asked();
    session.writes(tag, "One moment. ");
    session.proposes(tag, HANG_UP, "{}");
    session.proposes(tag, LOOKUP, r#"{"order":"last"}"#);
    check(&session);
    let synced = session.proposes(tag, SYNC, "{}");
    session.stored(synced.facts[1].0.seq);
    session.proposes(tag, REFUND, "{}");
    session.proposes(tag, RESEARCH, "{}");
    check(&session);
    let edge = Event::Speech {
        who: ANA,
        edge: VadEdge::Start,
        at: MediaTime::from_millis(60_000),
    };
    session.after(50, edge);
    check(&session);
    session.returns(ToolCallId(2), "A-1041");
    session.after(10_000, Event::Timer(TimerId::ToolTimeout(ToolCallId(3))));
    check(&session);

    // A handoff, and the end of the session.
    session.control(ControlInput::Handoff {
        to: BILLING,
        context: HandoffContext::Summary("Ana wants a refund.".into()),
    });
    check(&session);
    session.control(ControlInput::StageReady);
    check(&session);
    session.control(ControlInput::Close {
        reason: CloseReason::Requested,
    });
    check(&session);
    session.after(10_000, Event::Timer(TimerId::Grace));
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Closed);
    check(&session);
    assert!(snapshots.len() > 10);
}

#[test]
fn what_never_reached_the_disk_never_happened() {
    let mut session = Session::new();
    session.exchange("It shipped on Monday.");
    let stored = session.last_seq();
    session.asked();
    assert_eq!(session.items().len(), 3);

    let mut rebuilt = session.crash(&session.origin, stored);
    assert_eq!(
        rebuilt.items(),
        ["1: can you help me", "agent 0: It shipped on Monday."]
    );
    assert_eq!(rebuilt.kernel.view().next_seq(), stored.next());
    assert!(rebuilt.wake().started().is_none());
}

#[test]
fn a_journal_with_a_gap_or_a_snapshot_that_does_not_fit_is_refused() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "1");
    session.proposes(tag, LOOKUP, "2");
    let restore = |config, tail| {
        Kernel::restore(
            config,
            BoxedPolicies::default(),
            &session.origin,
            tail,
            &session.journal.text,
        )
        .map(|kernel| kernel.snapshot())
    };

    assert_eq!(
        restore(desk(), &session.journal.facts),
        Ok(session.kernel.snapshot())
    );
    assert_eq!(
        restore(desk(), session.journal.since(Seq(2))),
        Err(RestoreError::Gap {
            expected: Seq(1),
            found: Seq(2),
        })
    );

    // The snapshot of a session with two calls, and room for one.
    let mut small = desk();
    small.limits.open_calls = 1;
    let full = session.kernel.snapshot();
    let refused = Kernel::restore(
        small,
        BoxedPolicies::default(),
        &full,
        &[],
        &session.journal.text,
    );
    let error = refused.map(|_| ()).unwrap_err();
    assert_eq!(error, RestoreError::TooSmall("open_calls"));
    assert_eq!(
        error.to_string(),
        "the limit `open_calls` is too small for the snapshot"
    );
}

// ---- The first step --------------------------------------------------------

#[test]
fn a_session_that_was_idle_is_rebuilt_without_a_word() {
    let mut session = Session::new();
    session.exchange("It shipped on Monday.");
    let mut rebuilt = session.restart();
    assert_eq!(rebuilt.kernel.snapshot(), session.kernel.snapshot());

    let first = rebuilt.wake();
    assert_eq!(
        first.facts(),
        [Fact::EpochAdvanced {
            to: Epoch(1),
            cause: EpochCause::Recovery,
        }],
        "whatever arrives for its earlier life is stale"
    );
    assert!(first.asks_nothing());
    assert_eq!(
        rebuilt.items().len(),
        2,
        "nothing was lost, so nothing is noted"
    );
    assert!(rebuilt.wake().is_silent(), "it is done once");
}

#[test]
fn a_response_that_was_lost_is_given_again() {
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, "It shipped ");

    let mut rebuilt = session.restart();
    assert!(rebuilt.kernel.view().reply_owed());
    let first = rebuilt.wake();
    assert!(matches!(
        first.told()[..],
        [
            (
                Fact::EpochAdvanced {
                    cause: EpochCause::Recovery,
                    ..
                },
                _
            ),
            (
                Fact::NoticeAppended {
                    kind: NoticeKind::Recovered,
                    ..
                },
                NOTE
            ),
        ]
    ));
    let (again, start) = first.started().unwrap();
    assert_eq!(again.epoch, Epoch(1));
    assert_eq!(
        start,
        LlmCmd::Start {
            agent: FRONT,
            context: rebuilt.kernel.view().context_version(),
            cause: LlmCause::Reply { turn: TurnId(1) },
        }
    );
    // The response that was lost is dead, and the new one is not it.
    assert!(again.generation > tag.generation);
    assert!(rebuilt.writes(tag, "on Monday.").is_silent());
}

#[test]
fn speech_that_was_being_heard_is_recorded_as_cut_off_where_it_was_last_known_to_be() {
    let mut session = Session::new();
    let tag = session.asked();
    let text = "It shipped on Monday. It arrives on Thursday.";
    session.writes(tag, text);
    session.done(tag, Finish::Stop);
    session.voiced(tag, text.len());
    session.op(tag, played(900, 10, false));
    session.after(1_000, Event::Timer(TimerId::Checkpoint));
    // More is heard, and the process dies before that is recorded.
    session.op(tag, played(1_500, 21, false));

    let mut rebuilt = session.restart();
    let first = rebuilt.wake();
    assert!(matches!(
        first.told()[..],
        [
            (Fact::EpochAdvanced { .. }, _),
            (
                Fact::UtteranceCommitted {
                    generation: GenId(1),
                    agent: FRONT,
                    answers: None,
                    truncated: true,
                    ..
                },
                "It shipped"
            ),
            (Fact::NoticeAppended { .. }, NOTE),
        ]
    ));
    assert_eq!(
        rebuilt.items(),
        [
            "1: can you help me".to_owned(),
            "agent 0: It shipped [cut]".to_owned(),
            format!("note Recovered: {NOTE}"),
        ]
    );
    assert!(first.started().is_some(), "and the agent takes it up again");
    assert_eq!(rebuilt.kernel.snapshot(), rebuilt.rebuilt(&rebuilt.origin));
}

#[test]
fn an_answer_that_was_heard_to_the_end_is_not_given_again() {
    let mut session = Session::new();
    let tag = session.asked();
    let text = "It shipped on Monday.";
    session.writes(tag, text);
    session.done(tag, Finish::Stop);
    session.voiced(tag, text.len());
    session.op(tag, played(900, 10, false));
    session.after(1_000, Event::Timer(TimerId::Checkpoint));
    session.played_out(tag, text.len());

    let mut rebuilt = session.restart();
    assert!(!rebuilt.kernel.view().reply_owed());
    let first = rebuilt.wake();
    assert_eq!(first.facts().len(), 1);
    assert!(first.started().is_none());
}

#[test]
fn calls_are_dealt_with_by_what_may_have_happened_to_them() {
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, "One moment. ");
    session.proposes(tag, HANG_UP, "{}");
    session.proposes(tag, LOOKUP, "{}");
    let synced = session.proposes(tag, SYNC, "{}");
    session.stored(synced.facts[1].0.seq);
    session.proposes(tag, REFUND, "{}");
    let [asked, looked_up, synced, refund] = [1, 2, 3, 4].map(ToolCallId);

    let mut rebuilt = session.restart();
    let first = rebuilt.wake();
    assert_eq!(
        first.ended(),
        [
            // Never cleared, so never sent.
            (
                asked,
                ToolOutcome::Failed,
                "the session was restarted before the call was made"
            ),
            // Cleared, and it must happen at most once: it may have been
            // sent just before the crash.
            (
                refund,
                ToolOutcome::InDoubt,
                "the call was made, and whether it took effect is not known"
            ),
        ]
    );
    // What does no harm twice is sent again, as a further attempt.
    assert_eq!(
        first.sent(),
        [(looked_up, LOOKUP, "{}", 1), (synced, SYNC, "{}", 1)]
    );
    assert!(first.has(Command::SetTimer(
        TimerId::ToolTimeout(looked_up),
        Some(waveport_domain::time::MonoTime::from_millis(
            rebuilt.now_ms() + 10_000
        ))
    )));
    assert_eq!(rebuilt.kernel.view().calls().count(), 2);

    // Their results end the batch, and it is answered for.
    rebuilt.returns(looked_up, "A-1041");
    let last = rebuilt.returns(synced, "ok");
    assert!(
        last.facts()
            .iter()
            .any(|fact| matches!(fact, Fact::BatchResolved { .. }))
    );
    assert!(rebuilt.kernel.view().reply_owed() || last.started().is_some());
}

#[test]
fn a_call_is_not_sent_again_for_ever() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    let call = ToolCallId(1);
    for attempt in 1..=3 {
        session = session.restart();
        assert_eq!(session.wake().sent(), [(call, LOOKUP, "{}", attempt)]);
    }
    session = session.restart();
    let last = session.wake();
    assert!(last.sends_nothing());
    assert_eq!(
        last.ended(),
        [(
            call,
            ToolOutcome::Failed,
            "the answer to the call was lost when the session was restarted"
        )]
    );
}

#[test]
fn a_session_that_is_told_to_close_as_it_wakes_sends_nothing_again() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    let synced = session.proposes(tag, SYNC, "{}");
    session.stored(synced.facts[1].0.seq);
    let [looked_up, synced] = [1, 2].map(ToolCallId);

    let mut rebuilt = session.restart();
    let first = rebuilt.control(ControlInput::Close {
        reason: CloseReason::Requested,
    });
    assert!(first.sends_nothing());
    // Both may have been sent before the crash, so neither was withdrawn.
    assert_eq!(
        first.ended(),
        [
            (
                looked_up,
                ToolOutcome::Failed,
                "the answer to the call was lost when the session was restarted"
            ),
            (
                synced,
                ToolOutcome::InDoubt,
                "the call was made, and whether it took effect is not known"
            ),
        ]
    );
}

#[test]
fn work_another_agent_was_doing_is_started_again() {
    let mut session = Session::new();
    let tag = session.asked();
    let (work, _) = session.proposes(tag, RESEARCH, "{}").started().unwrap();

    let mut rebuilt = session.restart();
    let first = rebuilt.wake();
    assert!(first.facts().contains(&Fact::ToolDispatched {
        call: ToolCallId(1),
        attempt: 1,
    }));
    let again = first
        .cmds
        .iter()
        .find_map(|(command, _)| match command {
            Command::Llm(
                tag,
                LlmCmd::Start {
                    cause: LlmCause::Delegated { .. },
                    ..
                },
            ) => Some(*tag),
            _ => None,
        })
        .unwrap();
    assert_ne!(again, work);
    assert!(rebuilt.writes(work, "stale").is_silent());
    rebuilt.writes(again, "Two years.");
    let done = rebuilt.done(again, Finish::Stop);
    assert_eq!(
        done.ended(),
        [(ToolCallId(1), ToolOutcome::Completed, "Two years.")]
    );
}

#[test]
fn a_session_that_was_closing_is_closed() {
    let (mut session, tag) = {
        let mut session = Session::new();
        let tag = session.asked();
        (session, tag)
    };
    let refund = session.proposes(tag, REFUND, "{}");
    session.stored(refund.facts[1].0.seq);
    session.proposes(tag, LOOKUP, "{}");
    session.control(ControlInput::Close {
        reason: CloseReason::Requested,
    });
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Draining);

    let mut rebuilt = session.restart();
    let first = rebuilt.wake();
    assert_eq!(
        first
            .ended()
            .iter()
            .map(|(call, outcome, _)| (call.get(), *outcome))
            .collect::<Vec<_>>(),
        [(1, ToolOutcome::InDoubt), (2, ToolOutcome::Failed)],
        "nothing is sent again by a session that is closing"
    );
    assert!(first.sends_nothing());
    assert!(first.started().is_none());
    assert!(first.facts().contains(&Fact::LifecycleChanged {
        to: Lifecycle::Closed,
        reason: Some(CloseReason::Requested),
    }));
    assert_eq!(rebuilt.kernel.view().lifecycle(), Lifecycle::Closed);
    assert!(rebuilt.wake().is_silent());
}

#[test]
fn a_handoff_that_was_under_way_is_completed() {
    let mut session = Session::new();
    session.control(ControlInput::Handoff {
        to: BILLING,
        context: HandoffContext::Full,
    });
    let mut rebuilt = session.restart();
    assert_eq!(rebuilt.kernel.view().handoff(), Some(BILLING));
    let first = rebuilt.wake();
    assert!(
        first
            .facts()
            .contains(&Fact::AgentActivated { agent: BILLING })
    );
    assert_eq!(rebuilt.kernel.view().front(), BILLING);
    assert_eq!(rebuilt.kernel.view().handoff(), None);
}

#[test]
fn new_turns_and_responses_get_numbers_that_were_never_used() {
    let mut session = Session::new();
    let tag = session.asked();
    let edge = |at| Event::Speech {
        who: ANA,
        edge: VadEdge::Start,
        at: MediaTime::from_millis(at),
    };
    // A turn is open and a response is under way, and no fact names
    // either.
    let open = session.after(5_000, edge(9_000));
    assert!(open.has(Command::Emit(
        waveport_domain::command::ClientEvent::UserSpeaking {
            who: ANA,
            turn: TurnId(2),
            speaking: true,
        }
    )));
    assert_eq!(tag.generation, GenId(1));

    let mut rebuilt = session.restart();
    let first = rebuilt.wake();
    let (again, _) = first.started().unwrap();
    assert!(again.generation > GenId(1), "{again:?}");
    let spoke = rebuilt.after(1_000, edge(20_000));
    let turn = spoke.cmds.iter().find_map(|(command, _)| match command {
        Command::Emit(waveport_domain::command::ClientEvent::UserSpeaking { turn, .. }) => {
            Some(*turn)
        }
        _ => None,
    });
    assert!(turn.unwrap() > TurnId(2), "{turn:?}");
}

#[test]
fn a_kernel_that_lost_its_lease_does_not_recover() {
    let mut session = Session::new();
    session.asked();
    let mut rebuilt = session.restart();
    assert!(rebuilt.control(ControlInput::LeaseLost).is_silent());
    assert!(rebuilt.wake().is_silent());
}
