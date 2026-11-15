//! The adapter against a server on this machine that answers as the
//! provider was recorded to, and, when asked, against the provider.
//!
//! `fixtures/` holds answers of the provider as they came over the wire
//! on 15 November 2026: a response of text, a response that asks for two
//! tool calls, and the refusal of a request the provider would not take.
//! The checks of `waveport_testkit::conformance` are the same ones that
//! the fakes pass.
//!
//! The tests named `live_*` talk to the provider. They are ignored
//! unless asked for, and need `OPENAI_API_KEY`:
//!
//! ```sh
//! cargo test -p waveport-llm-openai-compat -- --ignored live_
//! ```

#![expect(
    clippy::unwrap_used,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;

use waveport_adapter_kit::http::HttpSettings;
use waveport_adapter_kit::secret::Secret;
use waveport_domain::event::{Finish, RetryClass};
use waveport_llm_openai_compat::{Config, OpenAiCompat};
use waveport_ports::llm::{Llm, LlmEvent, LlmRequest, Message, ToolDecl, Usage};
use waveport_testkit::conformance::{llm_abandoned, llm_response};
use waveport_testkit::drive::next_llm;
use waveport_testkit::servers::{End, HttpAnswer, HttpServer};

const TEXT: &str = include_str!("fixtures/text.sse");
const TOOL_CALLS: &str = include_str!("fixtures/tool_calls.sse");
const REFUSED: &str = include_str!("fixtures/refused.json");

const QUESTION: [Message<'static>; 1] = [Message::User {
    speaker: None,
    text: "What time is it in Lima and in Madrid?",
}];

const TOOLS: [ToolDecl<'static>; 1] = [ToolDecl {
    name: "get_time",
    description: "The time of day in a city.",
    parameters: "{\"type\":\"object\",\"properties\":{\"city\":{\"type\":\"string\"}},\
                 \"required\":[\"city\"],\"additionalProperties\":false}",
}];

fn request(tools: &'static [ToolDecl<'static>]) -> LlmRequest<'static> {
    LlmRequest {
        instructions: "You are a helpful agent on a call. Use the tools you have.",
        messages: &QUESTION,
        tools,
        max_output_tokens: Some(128),
    }
}

/// An adapter that talks to `server`, and gives up on silence quickly.
fn adapter(server: &HttpServer) -> OpenAiCompat {
    let config = Config {
        base_url: server.url("/v1"),
        key: Some(Secret::new("sk-test-not-a-key")),
        http: HttpSettings {
            read_timeout: Duration::from_millis(300),
            direct: true,
            ..HttpSettings::default()
        },
        ..Config::openai("gpt-6-luna", None)
    };
    OpenAiCompat::new(config).unwrap()
}

#[tokio::test]
async fn a_recorded_response_of_text_is_read_as_the_port_says() {
    let server = HttpServer::start(vec![HttpAnswer::recorded(TEXT)]).await;
    let seen = llm_response(&adapter(&server), request(&[])).await.unwrap();
    assert_eq!(seen.text, "The capital of France is Paris.");
    assert_eq!(seen.calls, []);
    assert_eq!(seen.finish, Finish::Stop);
    let usage = Usage {
        input: 32,
        cached: 0,
        output: 10,
    };
    assert_eq!(seen.usage, Some(usage));

    // What was sent: one request, to the right place, with the key.
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-test-not-a-key")
    );
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(body["model"], "gpt-6-luna");
    assert_eq!(body["stream"], true);
    assert_eq!(body["reasoning_effort"], "none");
    assert_eq!(body["max_completion_tokens"], 128);
    assert_eq!(body["messages"][1]["content"], QUESTION_TEXT);
}

const QUESTION_TEXT: &str = "What time is it in Lima and in Madrid?";

#[tokio::test]
async fn a_recorded_response_with_two_tool_calls_is_read_as_the_port_says() {
    let server = HttpServer::start(vec![HttpAnswer::recorded(TOOL_CALLS)]).await;
    let seen = llm_response(&adapter(&server), request(&TOOLS))
        .await
        .unwrap();
    assert_eq!(seen.text, "");
    assert_eq!(
        seen.calls,
        [
            ("get_time".to_owned(), "{\"city\": \"Lima\"}".to_owned()),
            ("get_time".to_owned(), "{\"city\": \"Madrid\"}".to_owned()),
        ]
    );
    assert_eq!(seen.finish, Finish::ToolCalls);
    assert_eq!(seen.usage.map(|usage| usage.output), Some(48));
    let body: serde_json::Value = serde_json::from_str(&server.requests()[0].body).unwrap();
    assert_eq!(body["tools"][0]["function"]["name"], "get_time");
}

#[tokio::test]
async fn an_answer_cut_anywhere_reads_the_same() {
    // Bytes arrive as the network cuts them: in the middle of a line, of
    // a word, of an escape.
    for size in [1, 7, 64, 1_000] {
        let server = HttpServer::start(vec![
            HttpAnswer::in_pieces_of(TEXT, size),
            HttpAnswer::in_pieces_of(TOOL_CALLS, size),
        ])
        .await;
        let adapter = adapter(&server);
        let text = llm_response(&adapter, request(&[])).await.unwrap();
        assert_eq!(text.text, "The capital of France is Paris.", "{size}");
        let calls = llm_response(&adapter, request(&TOOLS)).await.unwrap();
        assert_eq!(calls.calls.len(), 2, "{size}");
    }
}

#[tokio::test]
async fn a_response_that_is_abandoned_leaves_the_adapter_as_good_as_new() {
    let slowly = HttpAnswer::Stream {
        pieces: TEXT
            .split_inclusive('\n')
            .map(|line| line.as_bytes().to_vec())
            .collect(),
        pause: Duration::from_millis(5),
        end: End::Complete,
    };
    let server = HttpServer::start(vec![slowly, HttpAnswer::recorded(TEXT)]).await;
    let seen = llm_abandoned(&adapter(&server), request(&[]))
        .await
        .unwrap();
    assert_eq!(seen.text, "The capital of France is Paris.");
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn the_first_tool_call_is_given_before_the_model_has_finished() {
    // The answer stops arriving in the middle of the second call.
    let cut = TOOL_CALLS.find("Madri").unwrap();
    let begun = HttpAnswer::Stream {
        pieces: vec![TOOL_CALLS.as_bytes()[..cut].to_vec()],
        pause: Duration::ZERO,
        end: End::Hang,
    };
    let server = HttpServer::start(vec![begun]).await;
    let adapter = adapter(&server);
    let mut rx = adapter.start(request(&TOOLS)).await.unwrap();
    let first = next_llm(&mut rx).await.unwrap().unwrap();
    assert_eq!(
        first,
        LlmEvent::ToolCall {
            tool: "get_time".to_owned(),
            args: "{\"city\": \"Lima\"}".to_owned()
        }
    );
    // And then the server is silent for longer than it may be.
    let error = next_llm(&mut rx).await.unwrap().unwrap_err();
    assert_eq!(error.class, RetryClass::Timeout, "{error}");
    assert!(next_llm(&mut rx).await.is_none());
}

#[tokio::test]
async fn an_answer_that_breaks_off_is_a_failure_that_may_not_happen_again() {
    let cut = TEXT.find(" France").unwrap();
    let broken = HttpAnswer::Stream {
        pieces: vec![TEXT.as_bytes()[..cut].to_vec()],
        pause: Duration::ZERO,
        end: End::Cut,
    };
    let server = HttpServer::start(vec![broken]).await;
    let adapter = adapter(&server);
    let mut rx = adapter.start(request(&[])).await.unwrap();
    let mut text = String::new();
    let error = loop {
        match next_llm(&mut rx).await.unwrap() {
            Ok(LlmEvent::Delta { text: piece }) => text.push_str(&piece),
            Ok(other) => panic!("{other:?}"),
            Err(error) => break error,
        }
    };
    // What arrived whole was handed on first.
    assert_eq!(text, "The capital of");
    assert_eq!(error.class, RetryClass::Transient, "{error}");
    assert!(next_llm(&mut rx).await.is_none());
}

#[tokio::test]
async fn a_stream_that_ends_without_a_reason_is_a_failure() {
    // Everything but the chunk that says why the model stopped, and a
    // server that then says it is done.
    let without: String = TEXT
        .split_inclusive("\n\n")
        .filter(|event| !event.contains("\"finish_reason\":\"stop\""))
        .collect();
    assert_ne!(without, TEXT);
    let server = HttpServer::start(vec![HttpAnswer::recorded(&without)]).await;
    let adapter = adapter(&server);
    let mut rx = adapter.start(request(&[])).await.unwrap();
    let mut last = None;
    while let Some(event) = next_llm(&mut rx).await {
        last = Some(event);
    }
    let error = last.unwrap().unwrap_err();
    assert_eq!(error.class, RetryClass::Transient);
    assert!(error.message.contains("why it stopped"), "{error}");
}

#[tokio::test]
async fn a_refusal_says_how_it_can_be_helped_and_keeps_the_key_to_itself() {
    let cases = [
        (400, REFUSED, RetryClass::Permanent),
        (
            401,
            "{\"error\":{\"message\":\"Incorrect API key provided: sk-test-not-a-key\"}}",
            RetryClass::Auth,
        ),
        (
            429,
            "{\"error\":{\"message\":\"Rate limit reached\"}}",
            RetryClass::RateLimited,
        ),
        (
            500,
            "{\"error\":{\"message\":\"The server had an error\"}}",
            RetryClass::Transient,
        ),
    ];
    for (status, body, class) in cases {
        let server = HttpServer::start(vec![HttpAnswer::status(status, body)]).await;
        let adapter = adapter(&server);
        let Err(error) = adapter.start(request(&TOOLS)).await else {
            panic!("{status} was taken for a response");
        };
        assert_eq!(error.class, class, "{status}: {error}");
        assert!(!error.message.contains("sk-test-not-a-key"), "{error}");
    }
    // The provider's own words are in the message, for whoever reads the
    // log.
    let server = HttpServer::start(vec![HttpAnswer::status(400, REFUSED)]).await;
    let Err(error) = adapter(&server).start(request(&TOOLS)).await else {
        panic!("a refusal was taken for a response");
    };
    assert!(error.message.contains("reasoning_effort"), "{error}");
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_failure_that_may_not_happen_again() {
    let server = HttpServer::start(vec![]).await;
    let adapter = adapter(&server);
    drop(server);
    // The port is closed now, or will be in a moment.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let Err(error) = adapter.start(request(&[])).await else {
        panic!("nothing answered, and a response began");
    };
    assert!(
        matches!(error.class, RetryClass::Transient | RetryClass::Timeout),
        "{error}"
    );
}

/// The fixtures are what the provider sent, and must hold nothing that
/// opens a door.
#[test]
fn no_fixture_holds_a_key() {
    for fixture in [TEXT, TOOL_CALLS, REFUSED] {
        for word in fixture.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
            assert!(!word.starts_with("sk-"), "{word}");
        }
        assert!(!fixture.to_ascii_lowercase().contains("bearer"));
    }
}

fn live() -> OpenAiCompat {
    let key = Secret::from_env("OPENAI_API_KEY").unwrap();
    OpenAiCompat::new(Config::openai("gpt-6-luna", Some(key))).unwrap()
}

#[tokio::test]
#[ignore = "talks to the provider; needs OPENAI_API_KEY"]
async fn live_a_response_of_text() {
    let seen = llm_response(&live(), request(&[])).await.unwrap();
    assert_ne!(seen.text, "");
    assert_eq!(seen.finish, Finish::Stop);
    assert!(seen.usage.is_some_and(|usage| usage.output > 0));
}

#[tokio::test]
#[ignore = "talks to the provider; needs OPENAI_API_KEY"]
async fn live_a_response_with_tool_calls() {
    let seen = llm_response(&live(), request(&TOOLS)).await.unwrap();
    assert!(!seen.calls.is_empty(), "{seen:?}");
    assert_eq!(seen.finish, Finish::ToolCalls);
    for (tool, args) in &seen.calls {
        assert_eq!(tool, "get_time");
        let args: serde_json::Value = serde_json::from_str(args).unwrap();
        assert!(args["city"].is_string(), "{args}");
    }
}

#[tokio::test]
#[ignore = "talks to the provider; needs OPENAI_API_KEY"]
async fn live_a_response_that_is_abandoned() {
    let seen = llm_abandoned(&live(), request(&[])).await.unwrap();
    assert_ne!(seen.text, "");
}
