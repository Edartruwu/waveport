//! Client protocol v1 against its fixtures, and what a client holds
//! against what it was sent.
//!
//! The fixtures are the protocol as it is promised to clients that are
//! not written yet, and to the ones in other languages. A change to one
//! is a change to that promise.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test that cannot read what it is given fails"
)]

#[allow(
    dead_code,
    reason = "shared with the other test files, each of which uses part of it"
)]
mod support;

use std::borrow::Cow;
use std::collections::BTreeSet;

use serde_json::Value;
use waveport_protocol::client::v1::{
    Applied, ErrorCode, Fact, FromClient, FromServer, Item, Lifecycle, News, Refusal, Show, Speech,
    State, ToolOutcome, View,
};
use waveport_protocol::journal;
use waveport_sim::{SeedPlan, SimContext, run};

/// What a client's message is called on the wire. Every kind is named
/// here: one that is added does not compile until it is, and belongs in
/// [`FROM_CLIENT`] then, which the fixtures have to cover.
const fn client_says(message: &FromClient) -> &'static str {
    match message {
        FromClient::Hello { .. } => "hello",
        FromClient::Played { .. } => "played",
        FromClient::PlayedRtp { .. } => "played_rtp",
        FromClient::Text { .. } => "text",
        FromClient::Dtmf { .. } => "dtmf",
        FromClient::Interrupt => "interrupt",
        FromClient::Bye => "bye",
        FromClient::Unknown => "unknown",
    }
}

/// Every kind of message a client says.
const FROM_CLIENT: [&str; 7] = [
    "hello",
    "played",
    "played_rtp",
    "text",
    "dtmf",
    "interrupt",
    "bye",
];

/// What a server's message is called on the wire, and the kind of what
/// it holds where it holds a fact, a piece of news or items.
fn server_says(message: &FromServer, seen: &mut BTreeSet<String>) -> &'static str {
    match message {
        FromServer::Welcome { .. } => "welcome",
        FromServer::Mark { .. } => "mark",
        FromServer::Clear { .. } => "clear",
        FromServer::Fact { fact, .. } => {
            seen.insert(format!("fact/{}", fact_is(fact)));
            "fact"
        }
        FromServer::Told(news) => {
            seen.insert(format!("told/{}", news_is(news)));
            "told"
        }
        FromServer::Gap { .. } => "gap",
        FromServer::State(state) => {
            for item in &state.items {
                seen.insert(format!("item/{}", item_is(item)));
            }
            "state"
        }
        FromServer::Error { .. } => "error",
        FromServer::Bye { .. } => "bye",
        FromServer::Unknown => "unknown",
    }
}

const fn fact_is(fact: &Fact) -> &'static str {
    match fact {
        Fact::ParticipantJoined { .. } => "participant_joined",
        Fact::ParticipantLeft { .. } => "participant_left",
        Fact::LifecycleChanged { .. } => "lifecycle_changed",
        Fact::CutOff { .. } => "cut_off",
        Fact::TurnCommitted { .. } => "turn_committed",
        Fact::KeysCommitted { .. } => "keys_committed",
        Fact::TurnDismissed { .. } => "turn_dismissed",
        Fact::UtteranceProgress { .. } => "utterance_progress",
        Fact::UtteranceCommitted { .. } => "utterance_committed",
        Fact::ToolCalled { .. } => "tool_called",
        Fact::ToolSent { .. } => "tool_sent",
        Fact::ToolFinished { .. } => "tool_finished",
        Fact::AgentActivated { .. } => "agent_activated",
        Fact::Unknown => "unknown",
    }
}

const fn news_is(news: &News) -> &'static str {
    match news {
        News::UserSpeaking { .. } => "user_speaking",
        News::Agent { .. } => "agent",
        News::Said { .. } => "said",
        News::Hearing { .. } => "hearing",
        News::Unknown => "unknown",
    }
}

const fn item_is(item: &Item) -> &'static str {
    match item {
        Item::Turn { .. } => "turn",
        Item::Keys { .. } => "keys",
        Item::Utterance { .. } => "utterance",
        Item::ToolCall { .. } => "tool_call",
        Item::ToolResult { .. } => "tool_result",
        Item::Unknown => "unknown",
    }
}

/// Every kind of message a server says, and every kind of fact, of news
/// and of item.
const FROM_SERVER: [&str; 31] = [
    "welcome",
    "mark",
    "clear",
    "fact",
    "told",
    "gap",
    "state",
    "error",
    "bye",
    "fact/participant_joined",
    "fact/participant_left",
    "fact/lifecycle_changed",
    "fact/cut_off",
    "fact/turn_committed",
    "fact/keys_committed",
    "fact/turn_dismissed",
    "fact/utterance_progress",
    "fact/utterance_committed",
    "fact/tool_called",
    "fact/tool_sent",
    "fact/tool_finished",
    "fact/agent_activated",
    "told/user_speaking",
    "told/agent",
    "told/said",
    "told/hearing",
    "item/turn",
    "item/keys",
    "item/utterance",
    "item/tool_call",
    "item/tool_result",
];

fn all_of(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn every_message_is_written_as_the_fixtures_have_it() {
    let mut seen = BTreeSet::new();
    for line in support::lines("client-v1/from_client.jsonl") {
        let message: FromClient = serde_json::from_str(&line).unwrap();
        assert_eq!(serde_json::to_string(&message).unwrap(), line);
        assert_eq!(message.check(), Ok(()), "{line}");
        let name = client_says(&message);
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], name, "{line}");
        seen.insert(name.to_owned());
    }
    assert_eq!(seen, all_of(&FROM_CLIENT));

    let mut seen = BTreeSet::new();
    for line in support::lines("client-v1/from_server.jsonl") {
        let message: FromServer = serde_json::from_str(&line).unwrap();
        assert_eq!(serde_json::to_string(&message).unwrap(), line);
        let name = server_says(&message, &mut seen);
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], name, "{line}");
        seen.insert(name.to_owned());
    }
    assert_eq!(seen, all_of(&FROM_SERVER));
}

#[test]
fn what_a_reader_does_not_know_it_skips_or_calls_unknown() {
    let mut unknown = 0;
    for line in support::lines("client-v1/tolerated.jsonl") {
        let case: Value = serde_json::from_str(&line).unwrap();
        let read = match case["from"].as_str() {
            Some("client") => {
                let message: FromClient = serde_json::from_value(case["given"].clone()).unwrap();
                serde_json::to_value(&message).unwrap()
            }
            Some("server") => {
                let message: FromServer = serde_json::from_value(case["given"].clone()).unwrap();
                // What is not known is no reason to stop: a view takes
                // it and goes on.
                let _ = View::new().apply(&message);
                serde_json::to_value(&message).unwrap()
            }
            from => panic!("{from:?} says nothing: {line}"),
        };
        assert_eq!(read, case["is"], "{line}");
        unknown += usize::from(line.contains("\"unknown\""));
    }
    assert!(unknown >= 10, "only {unknown} cases of something unknown");
}

const fn refusal_is(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Version => "version",
        Refusal::Rate => "rate",
        Refusal::Token => "token",
        Refusal::Text => "text",
        Refusal::Keys => "keys",
        Refusal::Unknown => "unknown",
    }
}

#[test]
fn a_message_that_says_what_it_cannot_is_refused() {
    let mut seen = BTreeSet::new();
    for line in support::lines("client-v1/refused.jsonl") {
        let case: Value = serde_json::from_str(&line).unwrap();
        let message: FromClient = serde_json::from_value(case["given"].clone()).unwrap();
        let refusal = message.check().expect_err(&line);
        assert_eq!(case["why"], refusal_is(refusal), "{line}");
        assert_eq!(
            case["code"],
            serde_json::to_value(refusal.code()).unwrap(),
            "{line}"
        );
        assert_ne!(refusal.to_string(), "");
        seen.insert(refusal_is(refusal));
    }
    assert_eq!(seen.len(), 6, "{seen:?}");

    // The most that is taken is taken.
    let most = FromClient::Text {
        text: "a".repeat(16 * 1024),
    };
    assert_eq!(most.check(), Ok(()));
    let more = FromClient::Text {
        text: "a".repeat(16 * 1024 + 1),
    };
    assert_eq!(more.check(), Err(Refusal::Text));
    let most = FromClient::Dtmf {
        keys: "7".repeat(32),
    };
    assert_eq!(most.check(), Ok(()));
    assert_eq!(Refusal::Unknown.code(), ErrorCode::UnknownMessage);
}

#[test]
fn a_client_holds_what_the_fixtures_say_it_holds() {
    let cases: Value = serde_json::from_str(&support::fixture("client-v1/views.json")).unwrap();
    let cases = cases.as_array().unwrap();
    let mut held = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let mut view = View::new();
        for message in case["messages"].as_array().unwrap() {
            let message: FromServer = serde_json::from_value(message.clone()).unwrap();
            let _ = view.apply(&message);
        }
        let expected: State = serde_json::from_value(case["view"].clone()).unwrap();
        assert_eq!(view.state(), &expected, "{name}");
        assert_eq!(view.seen(), expected.upto, "{name}");
        assert_eq!(Value::Bool(view.is_behind()), case["behind"], "{name}");
        held.push(view);
    }
    assert!(held.len() >= 7);
    // The first two are one call, for a client that was there all along
    // and for one that was away.
    assert_eq!(held[0], held[1]);
}

/// The names of a session's tools.
fn tools() -> Vec<String> {
    vec!["get_time".to_owned(), "end_call".to_owned()]
}

/// What no client is to be shown unless the server says so.
const PRIVATE: &str = "4111-1111";

#[test]
fn what_a_client_is_shown_of_a_journal_is_decided_fact_by_fact() {
    let all = Show {
        tools: true,
        tool_text: true,
    };
    let named = Show {
        tools: true,
        tool_text: false,
    };
    let none = Show::default();
    let shown = |fact: &journal::Fact<'_>, show| Fact::shown(fact, &tools(), show);

    // What was said is shown to everyone, without what only the server
    // needs of it.
    let turn = journal::Fact::TurnCommitted {
        turn: 2,
        who: 1,
        span: [48_000, 105_600],
        text: Cow::Borrowed("what time is it"),
        addressed: true,
    };
    assert_eq!(
        shown(&turn, none),
        Some(Fact::TurnCommitted {
            turn: 2,
            who: 1,
            text: "what time is it".to_owned(),
            addressed: true,
        })
    );
    let cut = journal::Fact::EpochAdvanced {
        to: 7,
        cause: journal::EpochCause::BargeIn,
    };
    let told = serde_json::to_string(&shown(&cut, none).unwrap()).unwrap();
    assert_eq!(told, r#"{"kind":"cut_off","cause":"barge_in"}"#);

    // A tool call is shown to a client that asked, by the tool's name,
    // and what it was given and what it answered only where that may
    // leave the server.
    let called = journal::Fact::ToolProposed {
        call: 5,
        batch: 2,
        generation: 3,
        tool: 1,
        args: Cow::Borrowed(PRIVATE),
    };
    let finished = journal::Fact::ToolFinished {
        call: 5,
        outcome: journal::ToolOutcome::Completed,
        result: Cow::Borrowed(PRIVATE),
    };
    let sent = journal::Fact::ToolDispatched {
        call: 5,
        attempt: 0,
    };
    for fact in [&called, &finished, &sent] {
        assert_eq!(shown(fact, none), None);
        // Text that may leave the server is no reason to show a client
        // what it did not ask for.
        let unasked = Show {
            tools: false,
            tool_text: true,
        };
        assert_eq!(shown(fact, unasked), None);
        let named = serde_json::to_string(&shown(fact, named).unwrap()).unwrap();
        assert!(!named.contains(PRIVATE), "{named}");
    }
    assert_eq!(
        shown(&called, named),
        Some(Fact::ToolCalled {
            call: 5,
            tool: "end_call".to_owned(),
            generation: 3,
            args: None,
        })
    );
    assert_eq!(
        shown(&finished, all),
        Some(Fact::ToolFinished {
            call: 5,
            outcome: ToolOutcome::Completed,
            result: Some(PRIVATE.to_owned()),
        })
    );
    // A tool the names do not reach is called by its number.
    let nameless = journal::Fact::ToolProposed {
        call: 6,
        batch: 2,
        generation: 3,
        tool: 9,
        args: Cow::Borrowed("{}"),
    };
    assert!(matches!(
        shown(&nameless, all),
        Some(Fact::ToolCalled { tool, .. }) if tool == "#9"
    ));

    // What is the server's own is shown to nobody.
    for fact in [
        journal::Fact::NoticeAppended {
            about: journal::NoticeKind::Recovered,
            text: Cow::Borrowed(PRIVATE),
        },
        journal::Fact::ToolAuthorized { call: 5 },
        journal::Fact::BatchResolved { batch: 2 },
        journal::Fact::HandoffRequested { to: 1 },
    ] {
        assert_eq!(shown(&fact, all), None, "{fact:?}");
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "a state with one item of every kind, and what each client is shown of it"
)]
fn a_stored_state_is_shown_as_the_facts_before_it_were() {
    let stored = journal::State {
        v: 2,
        upto: 40,
        heard_until: 960_000,
        epoch: 6,
        lifecycle: journal::Lifecycle::Draining,
        closed_for: Some(journal::CloseReason::Requested),
        front: 1,
        handoff: Some(2),
        turn_floor: 4,
        gen_floor: 5,
        next_call: 2,
        next_batch: 2,
        participants: vec![1, 3],
        calls: Vec::new(),
        batches: Vec::new(),
        items: vec![
            journal::Item::UserTurn {
                turn: 1,
                who: 1,
                text: Cow::Borrowed("book a table"),
                addressed: true,
            },
            journal::Item::Notice {
                about: journal::NoticeKind::Handoff,
                text: Cow::Borrowed(PRIVATE),
            },
            journal::Item::Keys {
                turn: 2,
                who: 3,
                keys: Cow::Borrowed("2#"),
            },
            journal::Item::ToolCall {
                call: 1,
                tool: 0,
                generation: 2,
                args: Cow::Borrowed(PRIVATE),
            },
            journal::Item::ToolResult {
                call: 1,
                outcome: journal::ToolOutcome::Failed,
                text: Cow::Borrowed(PRIVATE),
            },
            journal::Item::Utterance {
                generation: 3,
                agent: 1,
                text: Cow::Borrowed("For how many?"),
                truncated: false,
            },
        ],
        owed: None,
        answered: 6,
        speech: Some(journal::Speech {
            generation: 4,
            agent: 1,
            heard: Cow::Borrowed("And at "),
        }),
    };
    let turn = Item::Turn {
        turn: 1,
        who: 1,
        text: "book a table".to_owned(),
        addressed: true,
    };
    let keys = Item::Keys {
        turn: 2,
        who: 3,
        keys: "2#".to_owned(),
    };
    let utterance = Item::Utterance {
        generation: 3,
        agent: 1,
        text: "For how many?".to_owned(),
        truncated: false,
    };
    let plain = State::shown(&stored, &tools(), Show::default());
    assert_eq!(
        plain,
        State {
            upto: 40,
            lifecycle: Lifecycle::Draining,
            closed_for: Some(journal::CloseReason::Requested.into()),
            agent: 1,
            participants: vec![1, 3],
            items: vec![turn.clone(), keys.clone(), utterance.clone()],
            speech: Some(Speech {
                generation: 4,
                agent: 1,
                heard: "And at ".to_owned(),
            }),
        }
    );
    assert!(!serde_json::to_string(&plain).unwrap().contains(PRIVATE));

    let named = Show {
        tools: true,
        tool_text: false,
    };
    let with_tools = State::shown(&stored, &tools(), named);
    assert_eq!(
        with_tools.items,
        [
            turn,
            keys,
            Item::ToolCall {
                call: 1,
                tool: "get_time".to_owned(),
                generation: 2,
                args: None,
            },
            Item::ToolResult {
                call: 1,
                outcome: ToolOutcome::Failed,
                result: None,
            },
            utterance,
        ]
    );
    assert!(
        !serde_json::to_string(&with_tools)
            .unwrap()
            .contains(PRIVATE)
    );
}

/// A fact a journal could hold, about a few people, responses and calls,
/// so that the facts of a stream are about one another.
#[expect(clippy::too_many_lines, reason = "one arm for every kind of fact")]
fn any_fact(context: &mut SimContext) -> journal::Fact<'static> {
    let rng = &mut context.rng;
    let small = |rng: &mut waveport_sim::SimRng| u32::try_from(rng.range(1..5)).unwrap();
    let who = u16::try_from(rng.range(1..4)).unwrap();
    let agent = u8::try_from(rng.range(0..3)).unwrap();
    let words = *rng
        .choose(&[
            "Yes. ",
            "We close at six ",
            "on Sundays.",
            PRIVATE,
            "¿Cómo? 🙂 ",
        ])
        .unwrap();
    match rng.range(0..18) {
        0 => journal::Fact::ParticipantJoined { who },
        1 => journal::Fact::ParticipantLeft { who },
        2 => journal::Fact::LifecycleChanged {
            to: *rng
                .choose(&[
                    journal::Lifecycle::Running,
                    journal::Lifecycle::Draining,
                    journal::Lifecycle::Closing,
                    journal::Lifecycle::Closed,
                ])
                .unwrap(),
            reason: *rng
                .choose(&[
                    None,
                    Some(journal::CloseReason::Requested),
                    Some(journal::CloseReason::Abandoned),
                    Some(journal::CloseReason::Failure),
                ])
                .unwrap(),
        },
        3 => journal::Fact::EpochAdvanced {
            to: small(rng),
            cause: *rng
                .choose(&[
                    journal::EpochCause::BargeIn,
                    journal::EpochCause::Interrupted,
                    journal::EpochCause::Handoff,
                    journal::EpochCause::Superseded,
                    journal::EpochCause::Failure,
                    journal::EpochCause::Recovery,
                    journal::EpochCause::Closing,
                ])
                .unwrap(),
        },
        4 | 5 => journal::Fact::TurnCommitted {
            turn: small(rng),
            who,
            span: [0, 48_000],
            text: Cow::Borrowed(words),
            addressed: rng.chance(3, 4),
        },
        6 => journal::Fact::KeysCommitted {
            turn: small(rng),
            who,
            keys: Cow::Borrowed("4071#"),
        },
        7 => journal::Fact::TurnDismissed {
            turn: small(rng),
            who,
            reason: *rng
                .choose(&[
                    journal::Dismissal::Backchannel,
                    journal::Dismissal::Noise,
                    journal::Dismissal::Empty,
                    journal::Dismissal::Closed,
                ])
                .unwrap(),
        },
        8 => journal::Fact::NoticeAppended {
            about: journal::NoticeKind::Recovered,
            text: Cow::Borrowed(PRIVATE),
        },
        9 | 10 => journal::Fact::UtteranceProgress {
            generation: small(rng),
            agent,
            heard: Cow::Borrowed(words),
        },
        11 => journal::Fact::UtteranceCommitted {
            generation: small(rng),
            agent,
            answers: None,
            text: Cow::Borrowed(words),
            truncated: rng.chance(1, 3),
        },
        12 => journal::Fact::ToolProposed {
            call: rng.range(1..5),
            batch: small(rng),
            generation: small(rng),
            tool: u16::try_from(rng.range(0..3)).unwrap(),
            args: Cow::Borrowed(PRIVATE),
        },
        13 => journal::Fact::ToolAuthorized {
            call: rng.range(1..5),
        },
        14 => journal::Fact::ToolDispatched {
            call: rng.range(1..5),
            attempt: 0,
        },
        15 => journal::Fact::ToolFinished {
            call: rng.range(1..5),
            outcome: *rng
                .choose(&[
                    journal::ToolOutcome::Completed,
                    journal::ToolOutcome::Failed,
                    journal::ToolOutcome::Cancelled,
                    journal::ToolOutcome::InDoubt,
                ])
                .unwrap(),
            result: Cow::Borrowed(PRIVATE),
        },
        16 => journal::Fact::BatchResolved { batch: small(rng) },
        _ => {
            if rng.chance(1, 2) {
                journal::Fact::HandoffRequested { to: agent }
            } else {
                journal::Fact::AgentActivated { agent }
            }
        }
    }
}

/// The facts of a journal as a client is sent them: each with the
/// number it has there.
fn sent(facts: &[journal::Fact<'_>], show: Show) -> Vec<FromServer> {
    let names = tools();
    facts
        .iter()
        .zip(1_u64..)
        .filter_map(|(fact, seq)| {
            Fact::shown(fact, &names, show).map(|fact| FromServer::Fact { seq, fact })
        })
        .collect()
}

const fn seq_of(message: &FromServer) -> u64 {
    match message {
        FromServer::Fact { seq, .. } => *seq,
        _ => 0,
    }
}

/// What a client holds that was sent all of `stream` up to the fact
/// numbered `upto`.
fn held(stream: &[FromServer], upto: u64) -> View {
    let mut view = View::new();
    for message in stream.iter().filter(|message| seq_of(message) <= upto) {
        assert_eq!(view.apply(message), Applied::Fact);
    }
    view
}

/// One client that is away and comes back, twice: to a session that
/// went on without it, and to one that was rebuilt after its process
/// died and never did the last things the client was told.
fn comes_back(context: &mut SimContext) -> Result<(), String> {
    let show = Show {
        tools: context.rng.chance(2, 3),
        tool_text: context.rng.chance(1, 2),
    };
    let count = context.rng.range(1..60);
    let facts: Vec<_> = (0..count).map(|_| any_fact(context)).collect();
    let stream = sent(&facts, show);
    let whole = held(&stream, count);

    // Every message is read back as it was written, and holds nothing
    // it was not to show.
    for message in &stream {
        let written = serde_json::to_string(message).map_err(|error| error.to_string())?;
        let read: FromServer = serde_json::from_str(&written).map_err(|error| error.to_string())?;
        if &read != message {
            return Err(format!("{written} is read back as {read:?}"));
        }
        if !show.tool_text && written.contains("tool") && written.contains(PRIVATE) {
            return Err(format!("{written} shows what a tool was given or answered"));
        }
        if !show.tools && written.contains("tool") {
            return Err(format!(
                "{written} shows a tool call to a client that asked for none"
            ));
        }
    }

    // The client saw the facts up to `seen`. The server holds them no
    // longer, and sends the state as of `upto` and what follows.
    let upto = context.rng.range(0..count + 1);
    let seen = context.rng.range(0..upto + 1);
    let mut state = held(&stream, upto).state().clone();
    state.upto = upto;
    let mut client = held(&stream, seen);
    let gap = FromServer::Gap { resumes: upto + 1 };
    if client.apply(&gap) != Applied::Gap || !client.is_behind() {
        return Err("a gap leaves the client as it was".to_owned());
    }
    if client.apply(&FromServer::State(state.clone())) != Applied::State || client.is_behind() {
        return Err("a state leaves the client behind".to_owned());
    }
    for message in &stream {
        let applied = client.apply(message);
        let expected = if seq_of(message) > upto {
            Applied::Fact
        } else {
            // Sent again, as a server may: nothing changes.
            Applied::Seen
        };
        if applied != expected {
            return Err(format!("{message:?} was taken as {applied:?}"));
        }
    }
    let mut expected = whole.state().clone();
    expected.upto = expected.upto.max(upto);
    if client.state() != &expected {
        return Err(format!(
            "seen {seen}, state as of {upto}: the client holds\n{:?}\nand all the facts add up \
             to\n{expected:?}",
            client.state()
        ));
    }

    // The process dies. The journal has the facts up to `stored`, and
    // the session that is rebuilt from it does other things from there.
    // The client was told more than was stored.
    let stored = context.rng.range(0..count + 1);
    let told = context.rng.range(stored..count + 1);
    let more = context.rng.range(0..20);
    let mut rebuilt: Vec<_> = facts
        .iter()
        .take(usize::try_from(stored).map_err(|error| error.to_string())?)
        .cloned()
        .collect();
    rebuilt.extend((0..more).map(|_| any_fact(context)));
    let again = sent(&rebuilt, show);
    let mut state = held(&again, stored).state().clone();
    state.upto = stored;
    let mut client = held(&stream, told);
    let _ = client.apply(&FromServer::Gap {
        resumes: stored + 1,
    });
    let _ = client.apply(&FromServer::State(state));
    for message in again.iter().filter(|message| seq_of(message) > stored) {
        if client.apply(message) != Applied::Fact {
            return Err(format!(
                "told up to {told}, stored up to {stored}: {message:?} of the rebuilt session \
                 was not taken"
            ));
        }
    }
    let mut expected = held(&again, stored + more).state().clone();
    expected.upto = expected.upto.max(stored);
    if client.state() != &expected {
        return Err(format!(
            "told up to {told}, stored up to {stored}: the client holds\n{:?}\nand the rebuilt \
             session is\n{expected:?}",
            client.state()
        ));
    }
    Ok(())
}

#[test]
fn a_state_and_the_facts_after_it_are_what_all_the_facts_add_up_to() {
    let default = SeedPlan::Range {
        start: 0,
        count: 2_000,
    };
    let plan = SeedPlan::from_env(default).unwrap();
    let summary = run(plan, comes_back).unwrap();
    assert!(summary.seeds > 0);
}
