//! The bodies of control API v1 against their fixtures.

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

use std::collections::BTreeSet;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use waveport_protocol::client::v1::{ErrorCode, Refusal};
use waveport_protocol::control::v1::{
    Context, CreateSession, Input, Problem, SessionCreated, SessionInfo, Sessions,
};

/// `body` read as a `T` and written again.
fn through<T: DeserializeOwned + Serialize>(body: &Value) -> String {
    let read: T = serde_json::from_value(body.clone()).unwrap();
    serde_json::to_string(&read).unwrap()
}

/// `body` read as the body the fixture says it is, and written again.
fn read_as(what: &str, body: &Value) -> String {
    match what {
        "create_session" => through::<CreateSession>(body),
        "session_created" => through::<SessionCreated>(body),
        "session_info" => through::<SessionInfo>(body),
        "sessions" => through::<Sessions>(body),
        "input" => through::<Input>(body),
        "problem" => through::<Problem>(body),
        other => panic!("there is no body called {other}"),
    }
}

/// What an input is called on the wire. Every kind is named here: one
/// that is added does not compile until it is, and belongs in
/// [`INPUTS`] then, which the fixtures have to cover.
const fn input_is(input: &Input) -> &'static str {
    match input {
        Input::Text { .. } => "text",
        Input::Say { .. } => "say",
        Input::Press { .. } => "press",
        Input::Interrupt => "interrupt",
        Input::Handoff { .. } => "handoff",
        Input::Close => "close",
        Input::Unknown => "unknown",
    }
}

/// Every kind of input.
const INPUTS: [&str; 6] = ["text", "say", "press", "interrupt", "handoff", "close"];

#[test]
fn every_body_is_written_as_the_fixtures_have_it() {
    let mut bodies = BTreeSet::new();
    let mut inputs = BTreeSet::new();
    for line in support::lines("control-v1/bodies.jsonl") {
        let case: Value = serde_json::from_str(&line).unwrap();
        let what = case["a"].as_str().unwrap();
        // The line holds the body as it is written, byte for byte.
        let written = read_as(what, &case["body"]);
        assert!(line.ends_with(&format!("\"body\":{written}}}")), "{line}");
        if what == "input" {
            let input: Input = serde_json::from_value(case["body"].clone()).unwrap();
            assert_eq!(input.check(), Ok(()), "{line}");
            assert_eq!(case["body"]["type"], input_is(&input));
            inputs.insert(input_is(&input));
        }
        bodies.insert(what.to_owned());
    }
    assert_eq!(bodies.len(), 6, "{bodies:?}");
    assert_eq!(inputs, INPUTS.into_iter().collect());
}

#[test]
fn what_a_reader_does_not_know_it_skips_or_calls_unknown() {
    for line in support::lines("control-v1/tolerated.jsonl") {
        let case: Value = serde_json::from_str(&line).unwrap();
        let read = read_as(case["a"].as_str().unwrap(), &case["given"]);
        let read: Value = serde_json::from_str(&read).unwrap();
        assert_eq!(read, case["is"], "{line}");
    }
}

#[test]
fn what_cannot_be_asked_is_refused() {
    for line in support::lines("control-v1/refused.jsonl") {
        let case: Value = serde_json::from_str(&line).unwrap();
        let input: Input = serde_json::from_value(case["given"].clone()).unwrap();
        let refusal = input.check().expect_err(&line);
        let why = match refusal {
            Refusal::Text => "text",
            Refusal::Keys => "keys",
            Refusal::Unknown => "unknown",
            Refusal::Version | Refusal::Rate | Refusal::Token => "nothing an input is refused for",
        };
        assert_eq!(case["why"], why, "{line}");
        assert_eq!(
            case["code"],
            serde_json::to_value(refusal.code()).unwrap(),
            "{line}"
        );
    }
    // A summary is checked, and what needs none is not asked for one.
    let handoff = Input::Handoff {
        to: 1,
        context: Context::None,
    };
    assert_eq!(handoff.check(), Ok(()));
}

#[test]
fn a_problem_has_the_status_its_code_stands_for() {
    // Every code, so that one that is added is given a status.
    let codes = [
        (ErrorCode::Version, 400),
        (ErrorCode::Token, 401),
        (ErrorCode::Forbidden, 403),
        (ErrorCode::Malformed, 400),
        (ErrorCode::UnknownMessage, 400),
        (ErrorCode::OutOfPlace, 409),
        (ErrorCode::Unsupported, 422),
        (ErrorCode::Busy, 503),
        (ErrorCode::NoSession, 404),
        (ErrorCode::NoAgent, 404),
        (ErrorCode::Internal, 500),
        (ErrorCode::Unknown, 500),
    ];
    for (code, status) in codes {
        let problem = Problem::of(code, "why");
        assert_eq!(problem.status, status, "{code:?}");
        assert_ne!(problem.title, "");
        assert_eq!(problem.detail.as_deref(), Some("why"));
    }
    let bare = Problem::of(ErrorCode::NoAgent, "");
    assert_eq!(
        serde_json::to_string(&bare).unwrap(),
        r#"{"status":404,"title":"Not Found","code":"no_agent"}"#
    );
}
