//! A call that is watched: traced, counted and summed up.
//!
//! The command is run with `--otlp`, at a server on this machine that
//! stands in for a collector, and with `--report`. What the adapter
//! sends for each thing a session does is checked in the adapter's own
//! tests, which read the requests as the protocol says. Here it is the
//! command that is checked: that a real call reaches the collector, that
//! the report is written, and that neither holds a word of the call.

use waveport_testkit::servers::{HttpAnswer, HttpServer};

use super::{ANSWER, Providers, everything, folder, one_question, waveport_at};

/// What the caller asks, as the recogniser was recorded hearing it.
const QUESTION: &str = "What time is it right now?";

/// Checks that a report says what the call came to: one turn, answered
/// after the model was asked twice and two tools were called.
fn sums_the_call_up(written: &str) {
    let summed: serde_json::Value = serde_json::from_str(written).unwrap();
    assert_eq!(summed["v"], 1, "{written}");
    assert_eq!(summed["ended"], "closed", "{written}");
    assert_eq!(summed["why"], "requested", "{written}");
    assert_eq!(summed["rebuilt"], serde_json::Value::Null, "{written}");
    assert_eq!(summed["counts"]["turns"], 1, "{written}");
    assert_eq!(summed["counts"]["utterances"], 1, "{written}");
    assert_eq!(summed["counts"]["cut_off"], 0, "{written}");
    assert_eq!(summed["counts"]["tool_calls"], 2, "{written}");
    assert_eq!(summed["counts"]["tool_calls_not_completed"], 0, "{written}");
    let turn = &summed["turns"][0];
    let tools: Vec<&str> = turn["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["tool"].as_str())
        .collect();
    assert_eq!(tools, ["get_time", "end_call"], "{written}");
    assert_eq!(turn["thinking_ms"].as_array().map(Vec::len), Some(2));
    let waited = turn["waited_ms"].as_f64().unwrap();
    assert!(waited > 0.0 && waited < 20_000.0, "{written}");
    assert_eq!(summed["waited_ms"]["count"], 1, "{written}");
    assert_eq!(summed["waited_ms"]["max"], turn["waited_ms"], "{written}");
    for part in ["deciding", "thinking", "voicing", "sending", "calling"] {
        assert!(
            summed["parts_ms"][part]["count"].as_u64() >= Some(1),
            "{part}: {written}"
        );
    }
}

#[tokio::test]
async fn a_call_is_traced_counted_and_summed_up() {
    let folder = folder("watched");
    let script = one_question(&folder);
    let report = folder.join("report.json");
    let providers = Providers::start().await;
    let taken = HttpAnswer::Status {
        status: 200,
        headers: Vec::new(),
        body: String::new(),
    };
    let collector = HttpServer::start(vec![taken]).await;
    let collector_at = collector.url("");

    let mut arguments = vec![
        "run".to_owned(),
        "--script".to_owned(),
        script.to_str().unwrap().to_owned(),
        "--report".to_owned(),
        report.to_str().unwrap().to_owned(),
        "--otlp".to_owned(),
        collector_at.clone(),
    ];
    arguments.extend(providers.arguments());
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = waveport_at(&folder, &arguments).await;
    let told = String::from_utf8_lossy(&output.stdout).into_owned();
    let errors = String::from_utf8_lossy(&output.stderr).into_owned();
    let all = everything(&output, &providers.stt, &providers.llm, &providers.tts);
    assert!(output.status.success(), "{all}");
    let going = format!("note: traces and metrics of the calls go to {collector_at}");
    assert!(told.lines().any(|line| line == going), "{all}");
    assert!(told.contains("the report of the call is in "), "{all}");
    assert!(!errors.contains("did not reach"), "{all}");

    // The collector was sent the traces and the metrics, by the time the
    // command was over.
    let sent = collector.requests();
    let to = |path: &str| -> String {
        let line = format!("POST {path} HTTP/1.1");
        let there = sent.iter().filter(|request| request.line == line);
        there.map(|request| request.body.as_str()).collect()
    };
    for request in &sent {
        let kind = request.header("content-type");
        assert_eq!(kind, Some("application/x-protobuf"), "{}", request.line);
    }
    let traces = to("/v1/traces");
    for name in [
        "turn",
        "response",
        "execute_tool get_time",
        "execute_tool end_call",
        "gen_ai.conversation.id",
        "waveport.turn.time_to_first_frame",
    ] {
        assert!(traces.contains(name), "no {name} among the traces\n{all}");
    }
    let metrics = to("/v1/metrics");
    for name in [
        "waveport.turn.time_to_first_frame",
        "waveport.llm.time_to_first_token",
        "waveport.tts.time_to_first_audio",
        "waveport.tool.call.duration",
        "waveport.turns",
        "waveport.sessions.closed",
    ] {
        assert!(metrics.contains(name), "no {name} among the metrics\n{all}");
    }

    let written = std::fs::read_to_string(&report).unwrap();
    sums_the_call_up(&written);

    // No word of the call left the machine or is in the report.
    let said = ANSWER.concat();
    for words in [QUESTION, said.as_str(), "ten past three"] {
        for (holds, what) in [
            (&traces, "traces"),
            (&metrics, "metrics"),
            (&written, "report"),
        ] {
            assert!(!holds.contains(words), "{words:?} is in the {what}");
        }
    }
}
