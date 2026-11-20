//! RTVI against its fixtures: what a client wrote is read as the
//! fixtures say, and what a client is told is written as they say.
//!
//! The fixtures are RTVI as the stock Pipecat client and Pipecat's
//! source have it. A change to one says that they changed, or that
//! what is promised of the gateway did.

#![expect(
    clippy::panic,
    reason = "a test that cannot read what it is given fails"
)]

#[allow(
    dead_code,
    reason = "shared with the other test files, each of which uses part of it"
)]
mod support;

use std::collections::BTreeSet;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use waveport_protocol::rtvi::{
    AboutBot, Asks, BotOutput, BotReady, Capabilities, FromClient, InputType, Json, LABEL, Says,
    SpokenProgress, SpokenStatus, ToClient, UserInput, UserTranscription, VERSION, Written, read,
};

/// JSON as `serde_json` reads it.
struct Serde;

impl Json for Serde {
    fn read<T: DeserializeOwned>(text: &str) -> Option<T> {
        serde_json::from_str(text).ok()
    }
}

/// What was read, as the fixtures say it.
fn shown(written: &Written) -> Value {
    let FromClient { id, asks } = match written {
        Written::Foreign => return json!("foreign"),
        Written::Unreadable => return json!("unreadable"),
        Written::Said(said) => said,
    };
    let mut shown = serde_json::Map::new();
    let mut put = |name: &str, value: Value| drop(shown.insert(name.to_owned(), value));
    put("id", json!(id));
    match asks {
        Asks::ClientReady { version, about } => {
            put("asks", json!("client-ready"));
            let about = about.as_ref();
            for (name, value) in [
                ("version", version.as_ref()),
                ("library", about.map(|about| &about.library)),
                (
                    "library_version",
                    about.and_then(|about| about.library_version.as_ref()),
                ),
                ("platform", about.and_then(|about| about.platform.as_ref())),
            ] {
                if let Some(value) = value {
                    put(name, json!(value));
                }
            }
        }
        Asks::DisconnectBot => put("asks", json!("disconnect-bot")),
        Asks::SendText(text) => {
            put("asks", json!("send-text"));
            put("content", json!(text.content));
            put("run_immediately", json!(text.options.run_immediately));
            put("audio_response", json!(text.options.audio_response));
        }
        Asks::Dtmf(buttons) => {
            put("asks", json!("dtmf"));
            put("buttons", json!(buttons));
        }
        Asks::Invalid(kind) => {
            put("asks", json!("invalid"));
            put("kind", json!(kind));
        }
        Asks::Other(kind) => {
            put("asks", json!("other"));
            put("kind", json!(kind));
        }
    }
    Value::Object(shown)
}

/// The text a client wrote, of what a fixture gives: the message, or
/// the text itself where it is no message.
fn text_of(given: &Value) -> String {
    match given {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[test]
fn what_a_client_wrote_is_read_as_the_fixtures_say() {
    let mut kinds = BTreeSet::new();
    for line in support::lines("rtvi/from_client.jsonl") {
        let case: Value = serde_json::from_str(&line).unwrap();
        let read = shown(&read::<Serde>(&text_of(&case["given"])));
        assert_eq!(read, case["read"], "{line}");
        // What it is read as: what it asks, or that it is no message.
        let kind = read.get("asks").unwrap_or(&read).as_str().unwrap();
        kinds.insert(kind.to_owned());
    }
    // Every way a text can be read is in the fixtures.
    let all = [
        "client-ready",
        "disconnect-bot",
        "dtmf",
        "foreign",
        "invalid",
        "other",
        "send-text",
        "unreadable",
    ];
    assert_eq!(kinds.into_iter().collect::<Vec<_>>(), all);
}

#[test]
fn a_message_is_read_the_same_wherever_its_kind_stands_in_it() {
    // The kind of a message decides how its data is read, and nothing
    // says that it is written first: every order of the four fields is
    // read as the first is.
    let fields = [
        r#""label":"rtvi-ai""#,
        r#""type":"send-text""#,
        r#""id":"c1""#,
        r#""data":{"content":"what time is it","options":{"audio_response":false}}"#,
    ];
    let mut orders = Vec::new();
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                for d in 0..4 {
                    if BTreeSet::from([a, b, c, d]).len() == 4 {
                        orders.push([a, b, c, d]);
                    }
                }
            }
        }
    }
    assert_eq!(orders.len(), 24);
    let mut read_as = BTreeSet::new();
    for order in orders {
        let text = format!("{{{}}}", order.map(|n| fields[n]).join(","));
        let read = shown(&read::<Serde>(&text));
        assert_eq!(read["asks"], "send-text", "{text}");
        assert_eq!(read["audio_response"], false, "{text}");
        read_as.insert(read.to_string());
    }
    assert_eq!(read_as.len(), 1, "{read_as:?}");
}

/// What a message to a client is called on the wire. Every kind is
/// named here: one that is added does not compile until it is, and
/// belongs in [`TO_CLIENT`] then, which the fixtures have to cover.
const fn says_is(says: &Says<Value>) -> &'static str {
    match says {
        Says::BotReady(_) => "bot-ready",
        Says::Error { .. } => "error",
        Says::ErrorResponse { .. } => "error-response",
        Says::UserStartedSpeaking => "user-started-speaking",
        Says::UserStoppedSpeaking => "user-stopped-speaking",
        Says::BotStartedSpeaking => "bot-started-speaking",
        Says::BotStoppedSpeaking => "bot-stopped-speaking",
        Says::BotInterrupted => "bot-interrupted",
        Says::UserInput(_) => "user-input",
        Says::UserTranscription(_) => "user-transcription",
        Says::BotOutput(_) => "bot-output",
        Says::BotLlmStarted => "bot-llm-started",
        Says::BotLlmStopped => "bot-llm-stopped",
        Says::BotLlmText { .. } => "bot-llm-text",
        Says::BotTtsStarted => "bot-tts-started",
        Says::BotTtsStopped => "bot-tts-stopped",
        Says::BotTtsText { .. } => "bot-tts-text",
        Says::LlmFunctionCallStarted { .. } => "llm-function-call-started",
        Says::LlmFunctionCallInProgress { .. } => "llm-function-call-in-progress",
        Says::LlmFunctionCallStopped { .. } => "llm-function-call-stopped",
    }
}

/// Every kind of message a client is told.
const TO_CLIENT: [&str; 20] = [
    "bot-ready",
    "error",
    "error-response",
    "user-started-speaking",
    "user-stopped-speaking",
    "bot-started-speaking",
    "bot-stopped-speaking",
    "bot-interrupted",
    "user-input",
    "user-transcription",
    "bot-output",
    "bot-llm-started",
    "bot-llm-stopped",
    "bot-llm-text",
    "bot-tts-started",
    "bot-tts-stopped",
    "bot-tts-text",
    "llm-function-call-started",
    "llm-function-call-in-progress",
    "llm-function-call-stopped",
];

fn input(text: &str, input_type: InputType, second: u8, is_final: bool) -> UserInput {
    UserInput {
        text: text.to_owned(),
        input_type,
        timestamp: format!("2026-11-20T10:00:0{second}.000Z"),
        is_final,
        user_id: (input_type != InputType::Backchannel).then(|| "1".to_owned()),
        msg_id: (input_type == InputType::Chat).then(|| "c1".to_owned()),
    }
}

fn output(
    segment: u64,
    text: &str,
    status: SpokenStatus,
    heard: Option<(&str, &str)>,
) -> Says<Value> {
    let progress = heard.map(|(accumulated, remaining)| SpokenProgress {
        accumulated_text: accumulated.to_owned(),
        remaining_text: remaining.to_owned(),
    });
    let spoken = progress.is_some();
    Says::BotOutput(BotOutput::of(
        segment,
        text.to_owned(),
        spoken,
        status,
        progress,
    ))
}

/// The messages of `to_client.jsonl`, in its order.
fn told() -> Vec<ToClient<Value>> {
    let whole = "It is ten o'clock.";
    let clock = || Some("clock".to_owned());
    let call = |id: &str| id.to_owned();
    let said = vec![
        Says::Error {
            error: "the session has failed".to_owned(),
            fatal: true,
        },
        Says::UserStartedSpeaking,
        Says::UserStoppedSpeaking,
        Says::BotStartedSpeaking,
        Says::BotStoppedSpeaking,
        Says::BotInterrupted,
        Says::UserInput(input("what time", InputType::Transcription, 0, false)),
        Says::UserInput(input("what time is it", InputType::Chat, 1, true)),
        Says::UserInput(input("mhm", InputType::Backchannel, 2, true)),
        Says::UserTranscription(UserTranscription {
            text: "what time is it".to_owned(),
            user_id: "1".to_owned(),
            timestamp: "2026-11-20T10:00:01.000Z".to_owned(),
            is_final: true,
        }),
        output(3, whole, SpokenStatus::New, Some(("", whole))),
        output(
            3,
            "It is ",
            SpokenStatus::InProgress,
            Some(("It is ", "ten o'clock.")),
        ),
        output(
            3,
            "ten o'clock.",
            SpokenStatus::Completed,
            Some((whole, "")),
        ),
        output(4, whole, SpokenStatus::New, None),
        Says::BotLlmStarted,
        Says::BotLlmStopped,
        Says::BotLlmText {
            text: whole.to_owned(),
        },
        Says::BotTtsStarted,
        Says::BotTtsStopped,
        Says::BotTtsText {
            text: "It is ".to_owned(),
        },
        Says::LlmFunctionCallStarted {
            function_name: clock(),
        },
        Says::LlmFunctionCallStarted {
            function_name: None,
        },
        Says::LlmFunctionCallInProgress {
            tool_call_id: call("7"),
            function_name: clock(),
            arguments: Some(json!({"zone": "UTC"})),
        },
        Says::LlmFunctionCallInProgress {
            tool_call_id: call("7"),
            function_name: None,
            arguments: None,
        },
        Says::LlmFunctionCallStopped {
            tool_call_id: call("7"),
            cancelled: false,
            function_name: clock(),
            result: Some(json!("10:00")),
        },
        Says::LlmFunctionCallStopped {
            tool_call_id: call("8"),
            cancelled: true,
            function_name: None,
            result: None,
        },
    ];
    let ready = Says::BotReady(BotReady {
        version: VERSION.to_owned(),
        about: AboutBot {
            library: "waveport".to_owned(),
            library_version: "0.1.0".to_owned(),
        },
        capabilities: Capabilities {
            audio_in: true,
            audio_out: true,
            ..Capabilities::default()
        },
    });
    let mut all = vec![ToClient::answer("0dca44e2".to_owned(), ready)];
    all.extend(said.into_iter().map(ToClient::told));
    // The one that answers a message says which.
    let refused = Says::ErrorResponse {
        error: "text that is not answered at once is not taken".to_owned(),
    };
    all.insert(2, ToClient::answer("c4".to_owned(), refused));
    all
}

#[test]
fn what_a_client_is_told_is_written_as_the_fixtures_say() {
    let lines = support::lines("rtvi/to_client.jsonl");
    let told = told();
    assert_eq!(told.len(), lines.len());
    let mut kinds = BTreeSet::new();
    for (message, line) in told.iter().zip(&lines) {
        let expected: Value = serde_json::from_str(line).unwrap();
        let written = serde_json::to_value(message).unwrap();
        assert_eq!(written, expected, "{line}");
        // What it is called is what its kind says, and it is labelled.
        assert_eq!(written["type"], says_is(&message.says), "{line}");
        assert_eq!(written["label"], LABEL);
        kinds.insert(says_is(&message.says));
        // Nothing that has no value is written as one: a client takes a
        // field that is there for what it says.
        let text = serde_json::to_string(message).unwrap();
        assert!(!text.contains("null"), "{text}");
    }
    assert_eq!(kinds, BTreeSet::from(TO_CLIENT));
}

#[test]
fn a_message_that_holds_no_json_of_elsewhere_needs_no_type_for_it() {
    // Most of what a client is told holds none, and is written without
    // anybody saying what such JSON would be held in.
    let message: ToClient = ToClient::told(Says::BotInterrupted);
    let text = serde_json::to_string(&message).unwrap();
    assert_eq!(text, r#"{"label":"rtvi-ai","type":"bot-interrupted"}"#);
}
