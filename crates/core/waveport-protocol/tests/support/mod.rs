//! What the tests of the wire formats share: the fixtures.
//!
//! A fixture is the format as it is promised: the tests of the
//! JavaScript client read the same files. A line of a `.jsonl` file is
//! one JSON value.

use std::path::Path;

use serde_json::Value;

/// The text of the fixture `name`.
pub(crate) fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The lines of the fixture `name`.
pub(crate) fn lines(name: &str) -> Vec<String> {
    let lines: Vec<String> = fixture(name)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    // A test that reads nothing finds nothing.
    assert!(!lines.is_empty(), "{name} is empty");
    lines
}

/// Every message and every body the fixtures hold, each as JSON: what
/// the parsers are given to begin with when they are given anything at
/// all.
pub(crate) fn every_message() -> Vec<Value> {
    let mut all = Vec::new();
    for name in ["client-v1/from_client.jsonl", "client-v1/from_server.jsonl"] {
        all.extend(lines(name).iter().map(|line| value(line)));
    }
    for name in [
        "client-v1/tolerated.jsonl",
        "client-v1/refused.jsonl",
        "control-v1/bodies.jsonl",
        "control-v1/tolerated.jsonl",
        "control-v1/refused.jsonl",
    ] {
        for line in lines(name) {
            let case = value(&line);
            for part in ["given", "is", "body"] {
                all.extend(case.get(part).cloned());
            }
        }
    }
    // What a client of another protocol writes, which is a message or
    // text that is none.
    for line in lines("rtvi/from_client.jsonl") {
        all.extend(value(&line).get("given").cloned());
    }
    all.extend(lines("rtvi/to_client.jsonl").iter().map(|line| value(line)));
    let views = value(&fixture("client-v1/views.json"));
    for case in views.as_array().into_iter().flatten() {
        all.extend(case["messages"].as_array().into_iter().flatten().cloned());
        all.push(case["view"].clone());
    }
    all
}

fn value(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}
