//! Tool calls, one rule at a time: when a call is sent, how it ends, what
//! an interruption does to it, and what its result starts.

#[allow(
    dead_code,
    reason = "shared with the other test files, each of which uses part of it"
)]
mod support;

use waveport_domain::command::{Command, LlmCause, LlmCmd, ToolCmd};
use waveport_domain::config::KernelConfig;
use waveport_domain::event::{
    CloseReason, ControlInput, Event, Finish, OpEvent, Part, RetryClass, TimerId, ToolEvent,
};
use waveport_domain::fact::{EpochCause, Fact, Lifecycle, ToolOutcome};
use waveport_domain::ids::{BatchId, Epoch, OpTag, Seq, ToolCallId, ToolId};
use waveport_domain::kernel::{BatchView, CallState, CallView, GenerationRole};
use waveport_domain::time::MonoTime;

use self::support::{
    ANA, Emitted, FRONT, HANG_UP, LOOKUP, REFUND, RESEARCH, RESEARCHER, SYNC, Session, desk,
};

const FIRST: ToolCallId = ToolCallId(1);
const SECOND: ToolCallId = ToolCallId(2);
const BATCH: BatchId = BatchId(1);

fn timeout(call: ToolCallId) -> Event {
    Event::Timer(TimerId::ToolTimeout(call))
}

fn resolved(emitted: &Emitted) -> bool {
    emitted
        .facts()
        .contains(&Fact::BatchResolved { batch: BATCH })
}

fn state_of(session: &Session, call: ToolCallId) -> Option<CallState> {
    session
        .kernel
        .view()
        .calls()
        .find(|open| open.call == call)
        .map(|open| open.state)
}

/// What starts the continuation after the calls of the first batch.
fn continuation(emitted: &Emitted) -> Option<OpTag> {
    emitted.started().and_then(|(tag, start)| match start {
        LlmCmd::Start {
            agent,
            cause: LlmCause::Continuation { batch },
            ..
        } if agent == FRONT && batch == BATCH => Some(tag),
        _ => None,
    })
}

/// A session in which the agent has asked for a refund, the record of
/// that is stored, and the call has been sent.
#[expect(clippy::unwrap_used, reason = "a failed unwrap fails the test")]
fn refunding() -> (Session, OpTag) {
    let mut session = Session::new();
    let tag = session.asked();
    let proposed = session.proposes(tag, REFUND, "{}");
    let cleared = proposed
        .seq_of(|fact| matches!(fact, Fact::ToolAuthorized { .. }))
        .unwrap();
    assert_eq!(session.stored(cleared).sent().len(), 1);
    (session, tag)
}

// ---- Sending ---------------------------------------------------------------

#[test]
fn a_call_that_changes_nothing_is_sent_at_once() {
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, "Let me look. ");
    let args = r#"{"order":"last"}"#;
    let proposed = session.proposes(tag, LOOKUP, args);
    assert_eq!(
        proposed.facts(),
        [
            Fact::ToolProposed {
                call: FIRST,
                batch: BATCH,
                generation: tag.generation,
                tool: LOOKUP,
                args: proposed.facts[0].0.fact.text().unwrap(),
            },
            Fact::ToolAuthorized { call: FIRST },
            Fact::ToolDispatched {
                call: FIRST,
                attempt: 0,
            },
        ]
    );
    assert_eq!(proposed.sent(), [(FIRST, LOOKUP, args, 0)]);
    let due = MonoTime::from_millis(session.now_ms() + 10_000);
    assert!(proposed.has(Command::SetTimer(TimerId::ToolTimeout(FIRST), Some(due))));

    let view = session.kernel.view();
    assert_eq!(
        view.calls().collect::<Vec<_>>(),
        [CallView {
            call: FIRST,
            tool: LOOKUP,
            batch: BATCH,
            by: tag.generation,
            state: CallState::Dispatched,
            attempt: 0,
        }]
    );
    assert_eq!(
        view.batches().collect::<Vec<_>>(),
        [BatchView {
            batch: BATCH,
            by: tag.generation,
            pending: 1,
            ran: true,
        }]
    );
    assert_eq!(
        session.items().last().unwrap(),
        r#"call 1: tool 0 {"order":"last"}"#
    );
}

#[test]
fn a_call_that_changes_something_waits_until_its_record_is_stored() {
    for tool in [REFUND, SYNC] {
        let mut session = Session::new();
        let tag = session.asked();
        // A report that more is stored than was emitted counts for what
        // was emitted.
        session.stored(Seq(99));

        let proposed = session.proposes(tag, tool, "{}");
        assert_eq!(proposed.facts().len(), 2);
        assert_eq!(proposed.facts()[1], Fact::ToolAuthorized { call: FIRST });
        let cleared = proposed.facts[1].0.seq;
        assert!(proposed.has(Command::AwaitDurable(cleared)));
        assert!(proposed.sends_nothing());
        assert_eq!(state_of(&session, FIRST), Some(CallState::Authorized));

        // The record that the call was asked for is not the one that
        // counts.
        assert!(session.stored(Seq(cleared.get() - 1)).is_silent());

        let sent = session.stored(cleared);
        assert_eq!(
            sent.facts(),
            [Fact::ToolDispatched {
                call: FIRST,
                attempt: 0,
            }]
        );
        assert_eq!(sent.sent(), [(FIRST, tool, "{}", 0)]);
        assert!(
            session.stored(cleared).is_silent(),
            "it is sent once, however often the record is reported"
        );
    }
}

#[test]
fn a_record_that_is_never_stored_fails_the_call() {
    let mut session = Session::new();
    let tag = session.asked();
    let proposed = session.proposes(tag, REFUND, "{}");
    let cleared = proposed.facts[1].0.seq;

    assert!(session.after(9_999, timeout(FIRST)).is_silent());
    let late = session.after(1, timeout(FIRST));
    assert_eq!(
        late.ended(),
        [(
            FIRST,
            ToolOutcome::Failed,
            "the call could not be recorded, and was not made"
        )]
    );
    assert!(late.sends_nothing());
    assert!(session.stored(cleared).is_silent(), "and it stays unsent");
}

#[test]
fn requests_nobody_may_make_are_ignored() {
    let mut session = Session::new();
    let tag = session.asked();
    assert!(
        session.proposes(tag, ToolId(40), "{}").is_silent(),
        "no such tool"
    );
    let stale = OpTag {
        epoch: Epoch(7),
        ..tag
    };
    assert!(
        session.proposes(stale, LOOKUP, "{}").is_silent(),
        "no such response"
    );

    session.writes(tag, "Sure. ");
    session.done(tag, Finish::Stop);
    assert!(
        session.proposes(tag, LOOKUP, "{}").is_silent(),
        "the model had finished"
    );

    // Text the application gave is not a language model's.
    let said = session.control(ControlInput::Say {
        text: "Please hold.".into(),
    });
    let given = OpTag {
        generation: tag.generation.next(),
        epoch: Epoch(1),
    };
    assert!(said.has(Command::Llm(tag, LlmCmd::Cancel)) || !session.kernel.view().is_live(tag));
    assert!(session.kernel.view().is_live(given));
    assert!(session.proposes(given, LOOKUP, "{}").is_silent());

    // An agent has the tools it was given and no others.
    let mut config = desk();
    config.agents[0].tools = vec![LOOKUP];
    let mut session = Session::with(config);
    let tag = session.asked();
    assert!(session.proposes(tag, REFUND, "{}").is_silent());
    assert_eq!(session.proposes(tag, LOOKUP, "{}").sent().len(), 1);
}

#[test]
fn a_full_table_takes_no_more_calls() {
    let mut config = desk();
    config.limits.open_calls = 1;
    let mut session = Session::with(config);
    let tag = session.asked();
    assert_eq!(session.proposes(tag, LOOKUP, "1").sent().len(), 1);
    assert!(session.proposes(tag, LOOKUP, "2").is_silent());
    assert_eq!(session.kernel.view().calls().count(), 1);

    // Once the first has ended there is room again.
    session.returns(FIRST, "a");
    assert_eq!(
        session.proposes(tag, LOOKUP, "3").sent(),
        [(SECOND, LOOKUP, "3", 0)]
    );
}

// ---- Results ---------------------------------------------------------------

#[test]
fn a_result_enters_the_conversation_once() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");

    let first = session.returns(FIRST, "A-1041");
    assert_eq!(first.ended(), [(FIRST, ToolOutcome::Completed, "A-1041")]);
    assert!(first.has(Command::SetTimer(TimerId::ToolTimeout(FIRST), None)));
    assert_eq!(state_of(&session, FIRST), None);

    assert!(session.returns(FIRST, "A-1041").is_silent());
    let failed = ToolEvent::Failed {
        class: RetryClass::Permanent,
        message: "too late".into(),
    };
    assert!(session.step(Event::Tool(FIRST, failed)).is_silent());
    assert!(session.after(10_000, timeout(FIRST)).is_silent());
    assert_eq!(
        session.items()[1..],
        ["call 1: tool 0 {}", "result 1: Completed A-1041"]
    );
}

#[test]
fn a_result_for_a_call_that_was_not_sent_is_ignored() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, REFUND, "{}");
    assert!(session.returns(FIRST, "done").is_silent());
    assert!(session.returns(ToolCallId(9), "done").is_silent());
    assert_eq!(state_of(&session, FIRST), Some(CallState::Authorized));
}

#[test]
fn a_failure_is_what_the_language_model_is_told() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    session.done(tag, Finish::ToolCalls);
    let failed = ToolEvent::Failed {
        class: RetryClass::Permanent,
        message: "no such order".into(),
    };
    let told = session.step(Event::Tool(FIRST, failed));
    assert_eq!(
        told.ended(),
        [(FIRST, ToolOutcome::Failed, "no such order")]
    );
    assert!(
        continuation(&told).is_some(),
        "the model gets to say that it failed"
    );
}

#[test]
fn results_start_one_continuation_once_the_response_has_been_said() {
    let mut session = Session::new();
    let tag = session.asked();
    let text = "One moment. ";
    session.writes(tag, text);
    session.proposes(tag, LOOKUP, "1");
    session.proposes(tag, LOOKUP, "2");
    session.done(tag, Finish::ToolCalls);
    session.voiced(tag, text.len());

    let one = session.returns(FIRST, "a");
    assert!(!resolved(&one), "the other call is still running");
    let both = session.returns(SECOND, "b");
    assert!(resolved(&both));
    assert!(
        both.started().is_none(),
        "the response is still being heard"
    );
    assert_eq!(session.kernel.view().batches().count(), 0);
    assert!(session.kernel.view().reply_owed());

    let over = session.played_out(tag, text.len());
    let next = continuation(&over).unwrap();
    let (_, start) = over.started().unwrap();
    assert_eq!(
        start,
        LlmCmd::Start {
            agent: FRONT,
            context: session.kernel.view().context_version(),
            cause: LlmCause::Continuation { batch: BATCH },
        }
    );
    assert_eq!(
        session.items()[1..],
        [
            "call 1: tool 0 1",
            "call 2: tool 0 2",
            "result 1: Completed a",
            "result 2: Completed b",
            "agent 0: One moment.",
        ]
    );

    // Nothing starts a second one.
    assert!(session.returns(SECOND, "b").is_silent());
    assert!(!session.kernel.view().reply_owed());
    session.writes(next, "Here it is. ");
    session.done(next, Finish::Stop);
    session.voiced(next, 12);
    let said = session.played_out(next, 12);
    assert!(said.started().is_none());
}

#[test]
fn a_batch_stays_open_while_its_response_may_ask_for_more() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "1");
    let early = session.returns(FIRST, "a");
    assert!(!resolved(&early), "the model may ask for another call");
    assert_eq!(
        session.kernel.view().batches().collect::<Vec<_>>(),
        [BatchView {
            batch: BATCH,
            by: tag.generation,
            pending: 0,
            ran: true,
        }]
    );

    let second = session.proposes(tag, LOOKUP, "2");
    assert!(matches!(
        second.facts()[0],
        Fact::ToolProposed {
            call: SECOND,
            batch: BATCH,
            ..
        }
    ));
    assert!(!resolved(&session.returns(SECOND, "b")));

    // The model is done, and said nothing: the continuation follows at
    // once.
    let done = session.done(tag, Finish::ToolCalls);
    assert_eq!(done.facts(), [Fact::BatchResolved { batch: BATCH }]);
    assert!(continuation(&done).is_some());
}

#[test]
fn the_news_of_a_tool_does_not_take_the_floor() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    session.control(ControlInput::Interrupt);

    // Ana goes on, and the agent starts to answer her.
    let second = session.says(ANA, "and one more thing").started().unwrap().0;
    let back = session.returns(FIRST, "A-1041");
    assert!(resolved(&back));
    assert!(back.started().is_none());
    assert!(!back.has(Command::Llm(second, LlmCmd::Cancel)));
    assert!(session.kernel.view().is_live(second));

    // What it has to say about the result comes after.
    session.writes(second, "Sure. ");
    session.done(second, Finish::Stop);
    session.voiced(second, 6);
    let over = session.played_out(second, 6);
    assert!(continuation(&over).is_some());
}

#[test]
fn a_new_turn_takes_the_floor_from_the_news_of_a_tool() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    session.done(tag, Finish::ToolCalls);
    let next = continuation(&session.returns(FIRST, "A-1041")).unwrap();

    // Nothing of the continuation has been heard when Ana speaks again.
    let reply = session.says(ANA, "never mind, what time is it");
    assert!(reply.has(Command::Llm(next, LlmCmd::Cancel)));
    let (_, start) = reply.started().unwrap();
    assert!(matches!(
        start,
        LlmCmd::Start {
            cause: LlmCause::Reply { .. },
            ..
        }
    ));
    assert!(!session.kernel.view().reply_owed());
}

// ---- Interruptions ---------------------------------------------------------

#[test]
fn an_interruption_withdraws_what_was_not_sent_and_leaves_the_rest() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    session.proposes(tag, REFUND, "{}");

    let cut = session.control(ControlInput::Interrupt);
    assert_eq!(
        cut.ended(),
        [(
            SECOND,
            ToolOutcome::Cancelled,
            "the call was withdrawn before it was made"
        )]
    );
    assert!(!cut.has(Command::Tool(FIRST, ToolCmd::Cancel)));
    assert!(cut.has(Command::Llm(tag, LlmCmd::Cancel)));
    assert!(cut.facts().contains(&Fact::EpochAdvanced {
        to: Epoch(1),
        cause: EpochCause::Interrupted,
    }));
    assert!(session.stored(Seq(99)).sends_nothing());
    assert_eq!(state_of(&session, FIRST), Some(CallState::Dispatched));

    // The call that was running still counts, and is answered for.
    let back = session.returns(FIRST, "A-1041");
    assert_eq!(back.ended(), [(FIRST, ToolOutcome::Completed, "A-1041")]);
    assert!(resolved(&back));
    assert!(continuation(&back).is_some());
}

#[test]
fn calls_that_were_all_withdrawn_are_owed_nothing() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, REFUND, "{}");
    let cut = session.control(ControlInput::Interrupt);
    assert!(resolved(&cut));
    assert!(cut.started().is_none());
    assert!(!session.kernel.view().reply_owed());
    assert_eq!(
        session.items()[1..],
        [
            "call 1: tool 1 {}",
            "result 1: Cancelled the call was withdrawn before it was made",
        ]
    );
}

#[test]
fn a_gated_call_waits_for_its_response_to_be_said() {
    let mut session = Session::new();
    let tag = session.asked();
    let text = "Goodbye. ";
    session.writes(tag, text);
    let proposed = session.proposes(tag, HANG_UP, "{}");
    assert_eq!(proposed.facts().len(), 1, "asked for, and no more");
    session.done(tag, Finish::ToolCalls);
    session.voiced(tag, text.len());
    assert_eq!(state_of(&session, FIRST), Some(CallState::Proposed));

    let over = session.played_out(tag, text.len());
    assert!(matches!(over.facts()[0], Fact::UtteranceCommitted { .. }));
    assert_eq!(over.facts()[1], Fact::ToolAuthorized { call: FIRST });
    let cleared = over.facts[1].0.seq;
    assert!(over.has(Command::AwaitDurable(cleared)));
    assert_eq!(session.stored(cleared).sent(), [(FIRST, HANG_UP, "{}", 0)]);
}

#[test]
fn a_gated_call_is_withdrawn_when_its_response_is_cut_off() {
    let mut session = Session::new();
    let tag = session.asked();
    let text = "Goodbye. ";
    session.writes(tag, text);
    session.proposes(tag, HANG_UP, "{}");
    session.done(tag, Finish::ToolCalls);
    session.voiced(tag, text.len());

    let cut = session.control(ControlInput::Interrupt);
    assert_eq!(cut.ended().len(), 1);
    assert_eq!(cut.ended()[0].1, ToolOutcome::Cancelled);
    assert!(session.stored(Seq(99)).is_silent());
}

// ---- Calls that take too long ----------------------------------------------

#[test]
fn a_call_that_changes_nothing_is_stopped_when_it_takes_too_long() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    session.done(tag, Finish::ToolCalls);

    assert!(session.after(9_999, timeout(FIRST)).is_silent());
    let late = session.after(1, timeout(FIRST));
    assert!(late.has(Command::Tool(FIRST, ToolCmd::Cancel)));
    assert_eq!(
        late.ended(),
        [(
            FIRST,
            ToolOutcome::Failed,
            "the tool did not answer in time"
        )]
    );
    assert!(continuation(&late).is_some());
    assert!(session.returns(FIRST, "A-1041").is_silent());
}

#[test]
fn a_tool_may_undo_a_call_that_took_too_long() {
    let (mut session, _) = refunding();
    let undone = Event::Tool(FIRST, ToolEvent::RolledBack);
    assert!(
        session.step(undone.clone()).is_silent(),
        "nobody asked it to"
    );

    let late = session.after(10_000, timeout(FIRST));
    assert!(late.has(Command::Tool(FIRST, ToolCmd::Cancel)));
    assert!(late.ends_nothing(), "it may have taken effect");
    let wait = MonoTime::from_millis(session.now_ms() + 2_000);
    assert!(late.has(Command::SetTimer(TimerId::Rollback(FIRST), Some(wait))));
    assert_eq!(state_of(&session, FIRST), Some(CallState::Cancelling));

    let ended = session.after(500, undone);
    assert_eq!(
        ended.ended(),
        [(
            FIRST,
            ToolOutcome::Cancelled,
            "the call was stopped and left no effect"
        )]
    );
    assert!(ended.has(Command::SetTimer(TimerId::Rollback(FIRST), None)));
}

#[test]
fn a_call_nobody_can_vouch_for_is_in_doubt() {
    let (mut session, _) = refunding();
    session.after(10_000, timeout(FIRST));
    let rollback = Event::Timer(TimerId::Rollback(FIRST));
    assert!(session.after(1_999, rollback.clone()).is_silent());
    let ended = session.after(1, rollback);
    assert_eq!(
        ended.ended(),
        [(
            FIRST,
            ToolOutcome::InDoubt,
            "the call was made, and whether it took effect is not known"
        )]
    );
}

#[test]
fn an_answer_that_comes_late_still_counts() {
    let (mut session, _) = refunding();
    session.after(10_000, timeout(FIRST));
    let ended = session.after(
        300,
        Event::Tool(
            FIRST,
            ToolEvent::Completed {
                result: "refunded".into(),
            },
        ),
    );
    assert_eq!(ended.ended(), [(FIRST, ToolOutcome::Completed, "refunded")]);
}

// ---- Calls answered by another agent ---------------------------------------

/// A session in which the agent has asked the researcher something.
/// Returns the tags of the response that asked and of the researcher's
/// work.
#[expect(clippy::unwrap_used, reason = "a failed unwrap fails the test")]
fn researching() -> (Session, OpTag, OpTag) {
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, "Let me check. ");
    let proposed = session.proposes(tag, RESEARCH, r#"{"topic":"warranty"}"#);
    let (work, start) = proposed.started().unwrap();
    assert_eq!(
        start,
        LlmCmd::Start {
            agent: RESEARCHER,
            context: session.kernel.view().context_version(),
            cause: LlmCause::Delegated { call: FIRST },
        }
    );
    assert!(proposed.sends_nothing(), "no tool outside is asked");
    assert!(proposed.facts().contains(&Fact::ToolDispatched {
        call: FIRST,
        attempt: 0,
    }));
    (session, tag, work)
}

#[test]
fn another_agent_answers_a_delegated_call() {
    let (mut session, tag, work) = researching();
    let role = |session: &Session, tag: OpTag| {
        session
            .kernel
            .view()
            .generations()
            .find(|running| running.tag == tag)
            .map(|running| (running.agent, running.role, running.speaking))
    };
    assert_eq!(
        role(&session, work),
        Some((RESEARCHER, GenerationRole::Delegated(FIRST), false))
    );
    assert_eq!(
        role(&session, tag),
        Some((FRONT, GenerationRole::Floor, true))
    );

    assert!(session.writes(work, " Two years, parts ").is_silent());
    assert!(session.writes(work, "and labour. ").is_silent());
    let done = session.done(work, Finish::Stop);
    assert!(!done.speaks(), "what it writes is not said");
    assert_eq!(
        done.ended(),
        [(
            FIRST,
            ToolOutcome::Completed,
            "Two years, parts and labour."
        )]
    );
    assert!(!session.kernel.view().is_live(work));
    assert!(session.kernel.view().is_live(tag));
    assert!(session.writes(work, "more").is_silent());
}

#[test]
fn delegated_work_goes_on_when_the_floor_is_interrupted() {
    let (mut session, tag, work) = researching();
    let cut = session.control(ControlInput::Interrupt);
    assert!(cut.ends_nothing());
    assert!(!cut.has(Command::Llm(work, LlmCmd::Cancel)));
    assert_eq!(session.kernel.view().epoch(), Epoch(1));
    assert!(!session.kernel.view().is_live(tag));
    assert!(
        session.kernel.view().is_live(work),
        "it began in the epoch that has passed, and still counts"
    );

    session.writes(work, "Two years.");
    let done = session.done(work, Finish::Stop);
    assert_eq!(
        done.ended(),
        [(FIRST, ToolOutcome::Completed, "Two years.")]
    );
    let next = continuation(&done).unwrap();
    assert_eq!(next.epoch, Epoch(1));
}

#[test]
fn delegated_work_that_gives_no_answer_fails_the_call() {
    let no_answer = "the agent working on the call gave no answer";

    let (mut session, _, work) = researching();
    session.writes(work, "  ");
    let done = session.done(work, Finish::Stop);
    assert_eq!(done.ended(), [(FIRST, ToolOutcome::Failed, no_answer)]);

    let (mut session, _, work) = researching();
    session.writes(work, "Two ");
    let failed = OpEvent::Failed {
        part: Part::Think,
        class: RetryClass::Transient,
    };
    let done = session.op(work, failed);
    assert_eq!(done.ended(), [(FIRST, ToolOutcome::Failed, no_answer)]);
    assert!(!done.has(Command::Llm(work, LlmCmd::Cancel)));
    assert!(!session.kernel.view().is_live(work));
}

#[test]
fn delegated_work_that_takes_too_long_is_stopped() {
    let (mut session, _, work) = researching();
    assert!(session.after(29_999, timeout(FIRST)).is_silent());
    let late = session.after(1, timeout(FIRST));
    assert!(late.has(Command::Llm(work, LlmCmd::Cancel)));
    assert!(!late.has(Command::Tool(FIRST, ToolCmd::Cancel)));
    assert_eq!(
        late.ended(),
        [(
            FIRST,
            ToolOutcome::Failed,
            "the tool did not answer in time"
        )]
    );
    assert!(!session.kernel.view().is_live(work));
}

#[test]
fn delegated_work_asks_for_no_tools_and_has_a_limit() {
    let (mut session, _, work) = researching();
    assert!(session.proposes(work, LOOKUP, "{}").is_silent());
    assert_eq!(session.kernel.view().calls().count(), 1);

    let mut config = desk();
    config.limits.pending_text = 16;
    let mut session = Session::with(config);
    let tag = session.asked();
    let (work, _) = session.proposes(tag, RESEARCH, "{}").started().unwrap();
    assert!(session.writes(work, "Twenty bytes of it. ").is_silent());
    let over = session.writes(work, "And twenty more.    ");
    assert!(over.has(Command::Llm(work, LlmCmd::Cancel)));
    assert_eq!(
        over.ended(),
        [(
            FIRST,
            ToolOutcome::Completed,
            "Twenty bytes of it. And twenty more."
        )]
    );
}

#[test]
fn a_call_no_agent_is_free_for_fails() {
    let mut config = desk();
    config.limits.generations = 1;
    let mut session = Session::with(config);
    let tag = session.asked();
    let proposed = session.proposes(tag, RESEARCH, "{}");
    assert_eq!(
        proposed.ended(),
        [(
            FIRST,
            ToolOutcome::Failed,
            "no agent was free to work on the call"
        )]
    );
    assert!(proposed.started().is_none());
}

// ---- Closing ---------------------------------------------------------------

fn close() -> ControlInput {
    ControlInput::Close {
        reason: CloseReason::Requested,
    }
}

#[test]
fn a_closing_session_makes_no_calls() {
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, "Goodbye. ");
    session.proposes(tag, HANG_UP, "{}");
    session.proposes(tag, REFUND, "{}");

    let asked = session.control(close());
    let withdrawn = "the call was withdrawn before it was made";
    assert_eq!(
        asked.ended(),
        [
            (FIRST, ToolOutcome::Cancelled, withdrawn),
            (SECOND, ToolOutcome::Cancelled, withdrawn),
        ]
    );
    // A call asked for while the session closes goes the same way.
    let more = session.proposes(tag, LOOKUP, "{}");
    assert_eq!(more.ended().len(), 1);
    assert!(more.sends_nothing());
}

/// A session that is closing with two calls running: a look-up, and a
/// refund.
fn closing_with_calls() -> Session {
    let (mut session, tag) = refunding();
    session.proposes(tag, LOOKUP, "{}");
    session.done(tag, Finish::ToolCalls);
    let asked = session.control(close());
    assert!(asked.ends_nothing(), "what is running may finish");
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Draining);

    let drained = session.after(5_000, Event::Timer(TimerId::Drain));
    assert!(drained.has(Command::Tool(SECOND, ToolCmd::Cancel)));
    assert!(!drained.has(Command::Tool(FIRST, ToolCmd::Cancel)));
    assert_eq!(
        drained.ended(),
        [(
            SECOND,
            ToolOutcome::Cancelled,
            "the call was stopped and left no effect"
        )]
    );
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Closing);
    session
}

#[test]
fn a_closing_session_waits_for_a_call_that_may_have_taken_effect() {
    let mut session = closing_with_calls();
    let answered = session.after(
        500,
        Event::Tool(
            FIRST,
            ToolEvent::Completed {
                result: "refunded".into(),
            },
        ),
    );
    assert_eq!(
        answered.ended(),
        [(FIRST, ToolOutcome::Completed, "refunded")]
    );
    assert!(resolved(&answered));
    assert!(answered.started().is_none(), "nothing starts any more");
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Closed);
}

#[test]
fn a_closing_session_does_not_wait_for_ever() {
    let mut session = closing_with_calls();
    assert!(
        session
            .after(1_999, Event::Timer(TimerId::Grace))
            .is_silent()
    );
    let closed = session.after(1, Event::Timer(TimerId::Grace));
    assert_eq!(closed.ended().len(), 1);
    assert_eq!(closed.ended()[0].1, ToolOutcome::InDoubt);
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Closed);
    assert_eq!(session.kernel.view().calls().count(), 0);
}

#[test]
fn a_session_with_calls_running_closes_as_soon_as_they_end() {
    let (mut session, tag) = refunding();
    session.done(tag, Finish::ToolCalls);
    session.control(close());
    let answered = session.after(
        700,
        Event::Tool(
            FIRST,
            ToolEvent::Completed {
                result: "refunded".into(),
            },
        ),
    );
    assert_eq!(answered.ended().len(), 1);
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Closed);
    let _ = KernelConfig::default();
}
