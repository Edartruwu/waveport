//! Every parser of the wire formats against text made up from a seed.
//!
//! What arrives from a client is whatever somebody chose to send. Each
//! run takes a message of the fixtures and, as its seed says, bends it
//! (it is still JSON, and says something else), damages it (it may be
//! JSON no longer), or both. Every parser is given the result: none may
//! panic, and what one of them does take has to be something that can
//! be written, read back as it was, checked and applied. A failure
//! names its seed;
//! `WAVEPORT_SIM_SEED=<seed>` runs that text alone, and
//! `WAVEPORT_SIM_SEEDS=<count>` runs more of them.

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

use core::fmt::Debug;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use waveport_protocol::client::v1::{FromClient, FromServer, State, View};
use waveport_protocol::control::v1::{
    CreateSession, Input, Problem, SessionCreated, SessionInfo, Sessions,
};
use waveport_protocol::journal;
use waveport_sim::{SeedPlan, SimContext, SimRng, run};

/// What damaged text is patched with: what means something to JSON, and
/// what means something to nothing.
const PIECES: [&[u8]; 24] = [
    b"{",
    b"}",
    b"[",
    b"]",
    b"\"",
    b":",
    b",",
    b"\\",
    b"\\u0000",
    b"\\ud800",
    b"null",
    b"true",
    b"-",
    b"1e999",
    b"18446744073709551616",
    b"-1",
    b"0.5",
    b"\"type\":",
    b"\"kind\":\"state\"",
    b"\"type\":\"fact\",\"seq\":",
    b"\xff",
    b"\xf0\x9f",
    b"\0",
    b" \n\t",
];

fn at(rng: &mut SimRng, len: usize) -> usize {
    usize::try_from(rng.range(0..len as u64 + 1)).unwrap()
}

/// Where every value in `value` is, as a JSON pointer.
fn places(value: &Value, here: &str, out: &mut Vec<String>) {
    out.push(here.to_owned());
    match value {
        Value::Object(fields) => {
            for (name, inner) in fields {
                places(inner, &format!("{here}/{name}"), out);
            }
        }
        Value::Array(items) => {
            for (index, inner) in items.iter().enumerate() {
                places(inner, &format!("{here}/{index}"), out);
            }
        }
        _ => {}
    }
}

/// Puts something else where one value of `message` is, takes a field
/// away, or adds one. What comes of it is JSON that says something its
/// writer did not mean.
fn bend(rng: &mut SimRng, message: &mut Value, all: &[Value]) {
    let mut found = Vec::new();
    places(message, "", &mut found);
    let place = rng.choose(&found).unwrap().clone();
    // Something from another message: a value of the right kind in the
    // wrong place.
    let other = rng.choose(all).unwrap();
    let mut theirs = Vec::new();
    places(other, "", &mut theirs);
    let borrowed = other
        .pointer(rng.choose(&theirs).unwrap())
        .cloned()
        .unwrap_or_default();
    let instead = match rng.range(0..14) {
        0 => Value::Null,
        1 => json!(true),
        2 => json!(0),
        3 => json!(u64::MAX),
        4 => json!(-1),
        5 => json!(1.5),
        6 => json!(""),
        7 => json!("unknown"),
        8 => json!("x".repeat(at(rng, 40_000))),
        9 => json!([]),
        10 => json!({}),
        _ => borrowed.clone(),
    };
    match rng.range(0..4) {
        // A field goes missing.
        0 => {
            if let Some((parent, name)) = place.rsplit_once('/')
                && let Some(Value::Object(fields)) = message.pointer_mut(parent)
            {
                fields.remove(name);
            }
        }
        // A field nobody knows appears, or one that is known appears
        // where it does not belong.
        1 => {
            if let Some(Value::Object(fields)) = message.pointer_mut(&place) {
                let name = *rng
                    .choose(&["type", "kind", "seq", "upto", "text", "items", "zz"])
                    .unwrap();
                fields.insert(name.to_owned(), borrowed);
            }
        }
        _ => {
            if let Some(found) = message.pointer_mut(&place) {
                *found = instead;
            }
        }
    }
}

/// A message of the fixtures after `rng` has been at it.
fn damaged(rng: &mut SimRng, all: &[Value]) -> Vec<u8> {
    let mut message = rng.choose(all).unwrap().clone();
    let bent = rng.chance(2, 3);
    if bent {
        for _ in 0..rng.range(1..4) {
            bend(rng, &mut message, all);
        }
    }
    let mut text = serde_json::to_vec(&message).unwrap();
    if bent && rng.chance(2, 3) {
        return text;
    }
    for _ in 0..rng.range(1..4) {
        let from = at(rng, text.len());
        let to = (from + at(rng, 12)).min(text.len());
        match rng.range(0..8) {
            // Something is cut out.
            0 => drop(text.drain(from..to)),
            // Something is put in.
            1 | 2 => {
                let piece = rng.choose(&PIECES).unwrap();
                text.splice(from..from, piece.iter().copied());
            }
            // Something is put in place of something else.
            3 => {
                let piece = rng.choose(&PIECES).unwrap();
                text.splice(from..to, piece.iter().copied());
            }
            // The text ends early.
            4 => text.truncate(from),
            // A byte is another byte.
            5 => {
                if let Some(byte) = text.get_mut(from) {
                    *byte = u8::try_from(rng.range(0..256)).unwrap();
                }
            }
            // A part of another message is put in.
            6 => {
                let other = serde_json::to_vec(rng.choose(all).unwrap()).unwrap();
                let begin = at(rng, other.len());
                let end = (begin + at(rng, 40)).min(other.len());
                let part = other.get(begin..end).unwrap_or_default();
                text.splice(from..from, part.iter().copied());
            }
            // It is nested far deeper than anything is.
            _ => {
                let depth = at(rng, 300);
                text.splice(from..from, core::iter::repeat_n(b'[', depth));
            }
        }
    }
    text
}

/// Gives `text` to the parser of `T`. What is taken is written and read
/// back, and has to be what it was.
fn read<T>(text: &[u8]) -> Result<Option<T>, String>
where
    T: DeserializeOwned + Serialize + PartialEq + Debug,
{
    let Ok(taken) = serde_json::from_slice::<T>(text) else {
        return Ok(None);
    };
    let written = serde_json::to_string(&taken).map_err(|error| format!("{taken:?}: {error}"))?;
    let again: T = serde_json::from_str(&written).map_err(|error| format!("{written}: {error}"))?;
    if again != taken {
        return Err(format!("{written} is read back as {again:?}"));
    }
    Ok(Some(taken))
}

/// How many of the texts of a run some parser took.
#[derive(Default)]
struct Taken {
    texts: u64,
    by_some: u64,
}

fn one(context: &mut SimContext, all: &[Value], taken: &mut Taken) -> Result<(), String> {
    let text = damaged(&mut context.rng, all);
    let mut by_some = false;

    if let Some(message) = read::<FromClient>(&text)? {
        let _ = message.check();
        by_some = true;
    }
    if let Some(message) = read::<FromServer>(&text)? {
        // A view takes whatever a server says, in any order.
        let mut view = View::new();
        let _ = view.apply(&message);
        let _ = view.apply(&message);
        let _ = (view.seen(), view.is_behind());
        by_some = true;
    }
    if let Some(state) = read::<State>(&text)? {
        let _ = View::from_state(state);
        by_some = true;
    }
    if let Some(input) = read::<Input>(&text)? {
        let _ = input.check();
        by_some = true;
    }
    by_some |= read::<CreateSession>(&text)?.is_some();
    by_some |= read::<SessionCreated>(&text)?.is_some();
    by_some |= read::<SessionInfo>(&text)?.is_some();
    by_some |= read::<Sessions>(&text)?.is_some();
    by_some |= read::<Problem>(&text)?.is_some();

    // The journal's records are read by the same server, from a file
    // somebody else may have had their hands on.
    if let Ok(record) = serde_json::from_slice::<journal::Record<'_>>(&text) {
        let _ = record.fact.since();
        by_some = true;
    }
    if let Ok(state) = serde_json::from_slice::<journal::State<'_>>(&text) {
        let _ = state.since();
        by_some = true;
    }

    taken.texts += 1;
    taken.by_some += u64::from(by_some);
    Ok(())
}

#[test]
fn no_text_makes_a_parser_panic() {
    let all = support::every_message();
    assert!(
        all.len() >= 150,
        "only {} messages to begin with",
        all.len()
    );
    let default = SeedPlan::Range {
        start: 0,
        count: 20_000,
    };
    let plan = SeedPlan::from_env(default).unwrap();
    let mut taken = Taken::default();
    let summary = run(plan, |context| one(context, &all, &mut taken)).unwrap();
    assert!(summary.seeds > 0);
    if plan != default {
        return;
    }
    // The damage is of both kinds: text that nothing takes, and text
    // that is still taken and holds something else than it did.
    let refused = taken.texts - taken.by_some;
    assert!(taken.by_some * 4 >= taken.texts, "{} taken", taken.by_some);
    assert!(refused * 4 >= taken.texts, "{refused} refused");
}
