//! How long the parts of a turn take in the session's own time.
//!
//! The real session actor runs whole calls with a stand-in in every
//! port, on a clock the test controls. With providers that answer at
//! once, whatever a caller still waits is this project's own share, and
//! it is held to the numbers in `perf/targets.toml`.
//!
//! The clock counts waiting and not work. A part of the session that
//! waits for a timer, or for its next turn to run, shows here as
//! milliseconds; one that computes for longer does not, and is caught by
//! the instruction counts instead.

#![expect(
    clippy::unwrap_used,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;
use std::sync::{Arc, Mutex};

use waveport_adapter_kit::turns::{Spread, What};
use waveport_benches::calls::{Delays, Script, Sent, Shared, Watch};
use waveport_benches::targets::Targets;
use waveport_domain::ids::{GenId, TurnId};
use waveport_domain::policy::DefaultPolicies;
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::journal::SessionId;
use waveport_runtime::session::{Ended, Session};

const ANSWERING: What = What::Answering(TurnId(0), GenId(0));
const THINKING: What = What::Thinking(GenId(0));
const PHRASING: What = What::Phrasing(GenId(0));
const VOICING: What = What::Voicing(GenId(0));
const SENDING: What = What::Sending(GenId(0));
const YIELDING: What = What::Yielding(GenId(0));

/// Runs `sessions` calls at once, each as `script` says, to their ends.
/// Returns what was seen of them, and what each far end was sent.
async fn calls(script: Script, sessions: u128) -> (Arc<Watch>, Vec<Arc<Mutex<Sent>>>) {
    let watch = Watch::keeping();
    let shared = Shared::new(script, &watch);
    let local = tokio::task::LocalSet::new();
    let mut lines = Vec::new();
    for n in 0..sessions {
        let (spec, parts, sent) = shared.call(SessionId(n + 1));
        lines.push(sent);
        local.spawn_local(async move {
            let mut session = Session::open(spec, parts, DefaultPolicies::default())
                .await
                .unwrap();
            assert_eq!(session.run().await, Ended::Closed);
        });
    }
    local.await;
    assert_eq!(u128::from(watch.closed()), sessions);
    (watch, lines)
}

fn ms(length: Duration) -> f64 {
    length.as_secs_f64() * 1_000.0
}

/// Checks that lengths are spread no wider than `perf/targets.toml`
/// holds the part called `name` to.
fn held(targets: &Targets, name: &str, spread: Spread) {
    for (of, measured) in [("p50", spread.p50), ("p99", spread.p99)] {
        let held = targets.number(&format!("latency.{name}.held_{of}_ms"));
        assert!(
            ms(measured) <= held,
            "{name}: {of} is {} ms, and perf/targets.toml holds it to {held} ms ({spread:?})",
            ms(measured)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_session_adds_no_wait_of_its_own_to_an_answer() {
    let targets = Targets::load();
    for codec in [
        Codec::Pcm(Rate::Hz48000),
        Codec::Opus,
        Codec::Pcm(Rate::Hz8000),
    ] {
        let script = Script::asking(10, codec, Delays::NONE);
        let (watch, lines) = calls(script, 3).await;
        // Every question was answered aloud.
        assert_eq!(watch.counted(&ANSWERING), 30, "{codec:?}");
        for sent in &lines {
            assert!(sent.lock().unwrap().frames > 500, "{codec:?}");
        }
        held(&targets, "answering", watch.spread(&ANSWERING).unwrap());
        held(&targets, "phrasing", watch.spread(&PHRASING).unwrap());
        held(&targets, "sending", watch.spread(&SENDING).unwrap());
    }
}

#[tokio::test(start_paused = true)]
async fn what_a_caller_waits_is_what_the_providers_take() {
    // Providers that take their time: the model 200 ms to its first
    // word and 20 ms to each of the four after it, the synthesiser
    // 100 ms to its first audio.
    let script = Script::asking(10, Codec::Opus, Delays::USUAL);
    let (watch, _) = calls(script, 3).await;
    let every = |what: &What| {
        let spread = watch.spread(what).unwrap();
        assert_eq!(spread.min, spread.max, "{what:?}: {spread:?}");
        assert_eq!(spread.count, 30, "{what:?}");
        spread.max
    };
    assert_eq!(every(&THINKING), Duration::from_millis(200));
    // The sentence is given to the synthesiser when the model has
    // written all of it and said that it is done: five words.
    assert_eq!(every(&PHRASING), Duration::from_millis(100));
    assert_eq!(every(&VOICING), Duration::from_millis(100));
    assert_eq!(every(&SENDING), Duration::ZERO);
    // And nothing besides: the wait is the sum of its parts.
    assert_eq!(every(&ANSWERING), Duration::from_millis(400));
}

#[tokio::test(start_paused = true)]
async fn a_caller_who_speaks_over_the_agent_is_yielded_to_in_time() {
    let targets = Targets::load();
    let over = Duration::from_millis(2_300);
    let script = Script::asking(8, Codec::Opus, Delays::USUAL).interrupting(over);
    let (watch, lines) = calls(script, 3).await;

    // Every answer was lowered and then stopped, once.
    let (lowered, stopped) = (watch.lowered(), watch.stopped());
    assert_eq!((lowered.len(), stopped.len()), (24, 24));
    assert_eq!(watch.counted(&YIELDING), 24);

    // From the moment the caller began to speak to the agent being
    // lowered.
    let mut ducking = Vec::new();
    for (n, (_, _, at)) in lowered.iter().enumerate() {
        // The sessions take their turns together, so the marks come in
        // rounds of three.
        let began = script.speaks_over(u32::try_from(n / 3).unwrap()).unwrap();
        let at = Duration::from_nanos(at.as_nanos());
        assert!(at >= began, "lowered at {at:?}, spoken over from {began:?}");
        ducking.push(at.checked_sub(began).unwrap());
    }
    held(&targets, "ducking", Spread::of(&mut ducking).unwrap());

    // From the agent being stopped to the last frame of it handed to
    // the far end.
    let mut stopping = Vec::new();
    for (session, generation, at) in stopped {
        let line = usize::try_from(session.0 - 1).unwrap();
        let last = lines[line].lock().unwrap().last_of(generation).unwrap();
        let at = Duration::from_nanos(at.as_nanos());
        stopping.push(last.saturating_sub(at));
    }
    held(&targets, "stopping", Spread::of(&mut stopping).unwrap());
}
