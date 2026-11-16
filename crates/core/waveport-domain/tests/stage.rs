//! The floor changing hands from one agent to another.

#[allow(
    dead_code,
    reason = "shared with the other test files, each of which uses part of it"
)]
mod support;

use waveport_domain::command::{Activity, ClientEvent, Command, LlmCause, LlmCmd, PlayoutCmd};
use waveport_domain::event::{CloseReason, ControlInput, Event, Finish, HandoffContext, TimerId};
use waveport_domain::fact::{EpochCause, Fact, NoticeKind};
use waveport_domain::ids::{AgentId, Epoch};
use waveport_domain::time::MonoTime;

use self::support::{ANA, BILLING, FRONT, LOOKUP, RESEARCHER, Session, played};

fn handoff(context: HandoffContext) -> ControlInput {
    ControlInput::Handoff {
        to: BILLING,
        context,
    }
}

#[test]
fn the_floor_goes_to_another_agent_once_its_parts_are_ready() {
    let mut session = Session::new();
    session.exchange("Let me put you through.");

    let asked = session.control(handoff(HandoffContext::Full));
    assert_eq!(asked.facts(), [Fact::HandoffRequested { to: BILLING }]);
    let until = MonoTime::from_millis(session.now_ms() + 5_000);
    assert!(asked.has(Command::SetTimer(TimerId::StageAck, Some(until))));
    let view = session.kernel.view();
    assert_eq!((view.front(), view.handoff()), (FRONT, Some(BILLING)));

    // What Ana says meanwhile is kept, and waits for an agent.
    let waiting = session.says(ANA, "hello?");
    assert!(waiting.started().is_none());
    assert!(session.kernel.view().reply_owed());

    let ready = session.control(ControlInput::StageReady);
    assert_eq!(ready.facts(), [Fact::AgentActivated { agent: BILLING }]);
    assert!(ready.has(Command::SetTimer(TimerId::StageAck, None)));
    assert!(ready.has(Command::Emit(ClientEvent::Agent {
        agent: BILLING,
        generation: None,
        activity: Activity::Listening,
    })));
    let (_, start) = ready.started().unwrap();
    assert!(matches!(
        start,
        LlmCmd::Start {
            agent: BILLING,
            cause: LlmCause::Reply { .. },
            ..
        }
    ));
    let view = session.kernel.view();
    assert_eq!((view.front(), view.handoff()), (BILLING, None));
    assert!(session.control(ControlInput::StageReady).is_silent());
}

#[test]
fn a_handoff_stops_the_agent_that_is_speaking() {
    let mut session = Session::new();
    let tag = session.asked();
    let text = "It shipped on Monday. It arrives on Thursday.";
    session.writes(tag, text);
    session.done(tag, Finish::Stop);
    session.voiced(tag, text.len());
    session.op(tag, played(900, 10, false));

    let asked = session.control(handoff(HandoffContext::Full));
    assert!(asked.has(Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 })));
    assert!(asked.facts().contains(&Fact::EpochAdvanced {
        to: Epoch(1),
        cause: EpochCause::Handoff,
    }));
    assert_eq!(session.items()[1], "agent 0: It shipped [cut]");
}

#[test]
fn the_agent_that_takes_over_is_told_what_it_is_given() {
    let note = |context| {
        let mut session = Session::new();
        session.exchange("One moment.");
        let asked = session.control(handoff(context));
        asked.told().iter().find_map(|(fact, text)| match fact {
            Fact::NoticeAppended {
                kind: NoticeKind::Handoff,
                ..
            } => Some((*text).to_owned()),
            _ => None,
        })
    };
    assert_eq!(note(HandoffContext::Full), None);
    assert_eq!(
        note(HandoffContext::Summary("Ana wants a refund.".into())),
        Some("Ana wants a refund.".to_owned())
    );
    assert_eq!(note(HandoffContext::None), Some(String::new()));
}

#[test]
fn an_agent_that_never_becomes_ready_does_not_get_the_floor() {
    let mut session = Session::new();
    session.control(handoff(HandoffContext::Full));
    session.says(ANA, "hello?");
    let ack = Event::Timer(TimerId::StageAck);
    // The clock moved while Ana spoke; the wait began at the handoff.
    let early = session.at(4_999, ack.clone());
    assert!(early.is_silent());

    let late = session.at(5_000, ack);
    assert_eq!(late.facts(), [Fact::AgentActivated { agent: FRONT }]);
    let (_, start) = late.started().unwrap();
    assert!(matches!(start, LlmCmd::Start { agent: FRONT, .. }));
    assert_eq!(session.kernel.view().handoff(), None);
    assert!(session.control(ControlInput::StageReady).is_silent());
}

#[test]
fn a_handoff_nobody_can_take_is_ignored() {
    let mut session = Session::new();
    let to = |to| ControlInput::Handoff {
        to,
        context: HandoffContext::Full,
    };
    assert!(session.control(to(AgentId(9))).is_silent(), "no such agent");
    assert!(
        session.control(to(RESEARCHER)).is_silent(),
        "it works on calls and never speaks"
    );
    assert!(session.control(to(FRONT)).is_silent(), "it has the floor");
    assert!(session.control(ControlInput::StageReady).is_silent());

    assert!(!session.control(to(BILLING)).is_silent());
    assert!(
        session.control(to(AgentId(2))).is_silent(),
        "one is under way"
    );

    let mut session = Session::new();
    session.control(ControlInput::Close {
        reason: CloseReason::Requested,
    });
    assert!(session.control(to(BILLING)).is_silent(), "it is over");
}

#[test]
fn nothing_is_said_while_the_floor_changes_hands() {
    let mut session = Session::new();
    session.control(handoff(HandoffContext::Full));
    let said = session.control(ControlInput::Say {
        text: "Please hold.".into(),
    });
    assert!(said.is_silent());
    session.control(ControlInput::StageReady);
    let said = session.control(ControlInput::Say {
        text: "Billing here.".into(),
    });
    assert!(said.speaks());
    let agent = session.kernel.view().generations().next().unwrap().agent;
    assert_eq!(agent, BILLING);
}

#[test]
fn calls_outlive_a_handoff_and_the_new_agent_answers_for_them() {
    let mut session = Session::new();
    let tag = session.asked();
    session.proposes(tag, LOOKUP, "{}");
    session.done(tag, Finish::ToolCalls);

    session.control(handoff(HandoffContext::Full));
    let back = session.returns(waveport_domain::ids::ToolCallId(1), "A-1041");
    assert!(back.started().is_none(), "nobody has the floor yet");
    let ready = session.control(ControlInput::StageReady);
    let (_, start) = ready.started().unwrap();
    assert!(matches!(
        start,
        LlmCmd::Start {
            agent: BILLING,
            cause: LlmCause::Continuation { .. },
            ..
        }
    ));
}
