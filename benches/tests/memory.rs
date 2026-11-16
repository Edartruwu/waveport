//! What sessions hold on the heap, and that nothing of them is left.
//!
//! The real session actor runs whole calls with a stand-in in every
//! port, on a clock the test controls, under an allocator that counts.
//! What a session holds when it listens and when it speaks is held to
//! the numbers in `perf/targets.toml`. What is counted is the Rust heap
//! of the thread the sessions run on: what libopus keeps for itself is
//! not in it.
//!
//! `soak_a_thousand_sessions` is the long run of many calls. It is left
//! out of an ordinary test run and made every night:
//!
//! ```text
//! cargo test --release -p waveport-benches --test memory -- --ignored
//! ```

#![expect(
    clippy::unwrap_used,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;
use std::sync::Arc;

use tokio::task::LocalSet;
use waveport_adapter_kit::turns::What;
use waveport_alloc_counter::{AllocStats, CountingAllocator, snapshot};
use waveport_benches::calls::{Delays, Script, Shared, Watch};
use waveport_benches::targets::Targets;
use waveport_domain::ids::{GenId, TurnId};
use waveport_domain::policy::DefaultPolicies;
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::journal::SessionId;
use waveport_runtime::session::{Ended, Session};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::system();

const ANSWERING: What = What::Answering(TurnId(0), GenId(0));

/// Begins `sessions` calls on `local`, numbered from `first`. Call
/// number `n` does everything `n` times `apart` later than the script
/// says, up to fifty times.
fn begin(local: &LocalSet, shared: &Shared, first: u128, sessions: u32, apart: Duration) {
    for n in 0..sessions {
        let later = apart * (n % 50);
        let (spec, parts, _) = shared.call_after(SessionId(first + u128::from(n)), later);
        local.spawn_local(async move {
            let mut session = Session::open(spec, parts, DefaultPolicies::default())
                .await
                .unwrap();
            assert_eq!(session.run().await, Ended::Closed);
        });
    }
}

/// Runs a few calls to their ends and lets go of everything they were
/// made of.
///
/// What is counted afterwards is then the sessions' and their parts'.
/// The runtime the test itself runs on makes some things the first time
/// they are used, and keeps them for as long as it lives: on macOS a
/// lock is 64 bytes on the heap from its first use. Without this they
/// would be made in the middle of the count and read as bytes that were
/// never given back.
async fn warm_up() {
    let script = Script::asking(1, Codec::Opus, Delays::USUAL);
    let watch = Watch::counting();
    let shared = Shared::new(script, &watch);
    let local = LocalSet::new();
    begin(&local, &shared, 1, 20, Duration::from_millis(17));
    local.await;
    assert_eq!(watch.closed(), 20);
}

/// What has been allocated since `before` and not freed, for each of
/// `sessions`, in kilobytes.
fn each_holds(before: AllocStats, sessions: u32) -> f64 {
    let bytes = snapshot().since(before).live_bytes();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a count of bytes that fits easily"
    )]
    let each = bytes as f64 / f64::from(sessions) / 1_024.0;
    each
}

#[tokio::test(start_paused = true)]
async fn a_session_holds_no_more_than_it_is_held_to() {
    let targets = Targets::load();
    for (codec, line) in [(Codec::Pcm(Rate::Hz48000), "pcm"), (Codec::Opus, "opus")] {
        // One question, ten seconds into the call, answered at length.
        let script = Script {
            first: Duration::from_secs(10),
            over: None,
            ..Script::asking(1, codec, Delays::USUAL).interrupting(Duration::ZERO)
        };
        let watch = Watch::counting();
        let shared = Shared::new(script, &watch);
        let local = LocalSet::new();
        let sessions = 20;
        let before = snapshot();
        begin(&local, &shared, 1, sessions, Duration::ZERO);

        // Five seconds in, nobody has spoken: the sessions listen.
        local
            .run_until(tokio::time::sleep(Duration::from_secs(5)))
            .await;
        let idle = each_holds(before, sessions);
        // Fourteen seconds in, the question is asked and the agent is in
        // the middle of its answer, with the rest of it made and
        // waiting to be sent.
        local
            .run_until(tokio::time::sleep(Duration::from_secs(9)))
            .await;
        let active = each_holds(before, sessions);
        assert_eq!(watch.counted(&ANSWERING), u64::from(sessions));
        assert_eq!(watch.closed(), 0);

        for (kind, measured) in [("idle", idle), ("active", active)] {
            let held = targets.number(&format!("memory.{kind}.held_{line}_kb"));
            assert!(
                measured <= held,
                "{kind} on {line}: a session holds {measured:.0} KB, and perf/targets.toml \
                 holds it to {held} KB"
            );
            // A number far under what is held means that the measure is
            // broken, or that the file should be told the good news.
            assert!(
                measured >= held * 0.9,
                "{kind} on {line}: a session holds {measured:.0} KB, far under the {held} KB \
                 that perf/targets.toml holds it to: say so there"
            );
        }
        assert!(
            active >= idle,
            "{line}: {active} KB when it speaks, {idle} KB when it listens"
        );
        local.await;
        assert_eq!(watch.closed(), u64::from(sessions));
    }
}

#[tokio::test(start_paused = true)]
async fn nothing_is_left_of_sessions_that_are_over() {
    let script = Script::asking(2, Codec::Opus, Delays::USUAL);
    // Made before the count begins, so that it is not in it.
    let mut after_wave = Vec::with_capacity(6);
    warm_up().await;
    let before = snapshot();
    {
        let watch = Watch::counting();
        let shared = Shared::new(script, &watch);
        // Wave after wave of calls on the same parts, as a process that
        // runs for a long time has them.
        for wave in 0..6_u32 {
            let local = LocalSet::new();
            begin(
                &local,
                &shared,
                u128::from(wave) * 1_000,
                20,
                Duration::from_millis(17),
            );
            local.await;
            assert_eq!(watch.closed(), u64::from(wave + 1) * 20);
            assert_eq!(watch.followed(), 0);
            after_wave.push(snapshot().since(before).live_bytes());
        }
    }
    // What the shared parts hold between waves is room for the calls
    // that are under way at once. It is there after the first wave, and
    // it is the same after every wave that follows: not a byte more.
    let first = *after_wave.first().unwrap();
    assert!(
        after_wave.iter().all(|held| *held == first),
        "{after_wave:?}"
    );
    // And when the parts are gone too, every byte is back.
    assert_eq!(snapshot().since(before).live_bytes(), 0);
}

/// Runs `sessions` calls of `questions` questions each, which begin
/// together and take their turns 17 ms apart, and checks all of them.
async fn soak(sessions: u32, questions: u32) {
    let targets = Targets::load();
    let script = Script::asking(questions, Codec::Opus, Delays::USUAL);
    warm_up().await;
    let before = snapshot();
    {
        let watch = Watch::keeping();
        let shared = Shared::new(script, &watch);
        let local = LocalSet::new();
        begin(&local, &shared, 1, sessions, Duration::from_millis(17));

        // Half way through, every session is in the middle of its call.
        local
            .run_until(tokio::time::sleep(script.length() / 2))
            .await;
        assert_eq!(watch.closed(), 0);
        let held = targets.number("memory.active.held_opus_kb");
        let each = each_holds(before, sessions);
        // The lengths the watch keeps are in the count too: a few
        // kilobytes for each session.
        assert!(
            each <= held + 8.0,
            "a session holds {each:.0} KB, held to {held} KB"
        );

        local.await;
        // Every call ended as it should, and every question was
        // answered.
        assert_eq!(watch.closed(), u64::from(sessions));
        assert_eq!(watch.followed(), 0);
        let answers = u64::from(sessions) * u64::from(questions);
        assert_eq!(watch.counted(&ANSWERING), answers);
        // On the session's own clock an answer takes what the providers
        // take, 400 ms, however many sessions there are: none waited
        // for another.
        let waited = watch.spread(&ANSWERING).unwrap();
        let expected = Duration::from_millis(400);
        assert_eq!((waited.min, waited.max), (expected, expected), "{waited:?}");
    }
    assert_eq!(snapshot().since(before).live_bytes(), 0);
}

#[tokio::test(start_paused = true)]
async fn soak_in_brief() {
    soak(60, 3).await;
}

#[tokio::test(start_paused = true)]
#[ignore = "the long run: a thousand calls of a minute each, made every night"]
async fn soak_a_thousand_sessions() {
    soak(1_000, 7).await;
}

/// Keeps the name of the allocator's type in use where the allocator is
/// not asked anything: the static above is all that installs it.
#[test]
fn the_allocator_that_counts_is_installed() {
    assert!(waveport_alloc_counter::is_installed());
    let _: Arc<()> = Arc::new(());
}
