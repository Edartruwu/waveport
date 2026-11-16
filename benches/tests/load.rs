//! How much of a thread a session takes.
//!
//! Fifty calls run at once on one shard of the real runtime, in real
//! time, on lines that carry Opus: a caller who asks every fourteen
//! seconds and an agent that answers for six and a half, so that the
//! agent speaks a little under half the time. The shard times every run
//! of every session, and what those runs add up to, against the time
//! that passed, is the share of its thread that the shard used.
//!
//! A share of a thread is a number about this machine. So the same run
//! also times a piece of reference work, encoding and decoding a second
//! of Opus, and the share is divided by that: the ratio moves little
//! from one machine to another, and it is what the nightly gate holds.
//! `cargo xtask perf` reads the lines this test prints, and records and
//! checks the numbers themselves on the machine it is run on.
//!
//! What is in the share: the audio path with its two codecs, the kernel,
//! the actor, and stand-ins for the providers and for the models that
//! listen. What is not: the models that listen themselves, which run on
//! threads of their own and were measured by the density spike
//! (`docs/decisions/0001-density-spike.md`), and everything a real
//! provider's adapter does with a real connection.
//!
//! A second measurement, `turns`, runs the same fifty calls with
//! providers that answer at once, and says how long the parts of a turn
//! take in real time by the marks the sessions make.
//!
//! Both are left out of an ordinary test run, and run one after the
//! other so that neither measures the other:
//!
//! ```text
//! cargo test --release -p waveport-benches --test load -- --ignored --nocapture --test-threads 1
//! ```

#![expect(
    clippy::unwrap_used,
    clippy::print_stdout,
    reason = "a measurement that cannot be set up fails, and one that can prints what it found"
)]

use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use waveport_adapter_kit::turns::{Spread, What};
use waveport_benches::calls::{Delays, STORY, Script, Shared, Watch};
use waveport_benches::targets::Targets;
use waveport_domain::ids::{GenId, TurnId};
use waveport_domain::policy::DefaultPolicies;
use waveport_media::codec::opus::OpusSettings;
use waveport_media::codec::{Decoder, Encoder};
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::journal::SessionId;
use waveport_runtime::session::Ended;
use waveport_runtime::shard::{Exit, Runtime, RuntimeConfig, ShardStats};
use waveport_testkit::fakes::tts::tone;

const ANSWERING: What = What::Answering(TurnId(0), GenId(0));
const PHRASING: What = What::Phrasing(GenId(0));
const SENDING: What = What::Sending(GenId(0));

/// A whole number from the environment, or `usual`.
fn from_env(name: &str, usual: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(usual)
}

/// The reference work: how long this machine takes to encode a second
/// of Opus and decode it again. The best of twenty tries, since anything
/// else that runs can only make a try slower.
fn reference() -> Duration {
    let rate = Rate::Hz48000;
    let mut encoder = Encoder::new(Codec::Opus, rate, OpusSettings::default()).unwrap();
    let mut decoder = Decoder::new(Codec::Opus, rate).unwrap();
    let mut frame = vec![0_i16; rate.frame()];
    let mut packet = vec![0_u8; Codec::Opus.max_packet()];
    let mut heard = vec![0_i16; rate.frame()];
    let mut best = Duration::MAX;
    for _ in 0..20 {
        let began = Instant::now();
        for n in 0..50 {
            for (offset, sample) in frame.iter_mut().enumerate() {
                *sample = tone(n * rate.frame() + offset);
            }
            let len = encoder.encode(&frame, &mut packet).unwrap();
            decoder
                .decode(packet.get(..len).unwrap(), &mut heard)
                .unwrap();
        }
        best = best.min(began.elapsed());
    }
    best
}

fn ms(length: Duration) -> f64 {
    length.as_secs_f64() * 1_000.0
}

/// What a run of calls on one shard of the runtime came to.
struct Run {
    /// The share of the shard's thread that one session took, from two
    /// seconds into the run to two seconds before its end.
    share: f64,
    /// The share of the runs of a session that went over the budget.
    over: f64,
    /// The longest run of a session.
    longest: Duration,
    /// How long each session took to start.
    starts: Vec<Duration>,
}

/// What the one shard of `runtime` has run so far.
fn the_shard(runtime: &Runtime) -> ShardStats {
    *runtime.stats().first().unwrap()
}

/// Runs `sessions` calls at once, each as `script` says, on one shard
/// of the runtime and in real time, to their ends.
fn run(script: Script, sessions: u32, watch: &Arc<Watch>) -> Run {
    let shared = Shared::new(script, watch);
    let runtime = Runtime::new(RuntimeConfig::default()).unwrap();
    let clock = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    clock.block_on(async {
        let mut started = Vec::new();
        let mut starts = Vec::new();
        for n in 0..sessions {
            // The calls take their turns 17 ms apart.
            let later = Duration::from_millis(17) * (n % 50);
            let (spec, parts, _) = shared.call_after(SessionId(u128::from(n) + 1), later);
            let began = Instant::now();
            let policies = DefaultPolicies::default();
            started.push(runtime.start(spec, parts, policies).await.unwrap());
            starts.push(began.elapsed());
        }
        // The first two seconds are left out: the sessions settle in.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let (from, at) = (the_shard(&runtime), Instant::now());
        tokio::time::sleep(script.length().saturating_sub(Duration::from_secs(4))).await;
        let (to, passed) = (the_shard(&runtime), at.elapsed());
        assert_eq!(to.sessions, usize::try_from(sessions).unwrap());
        for call in started {
            assert_eq!(call.exit().await, Exit::Ended(Ended::Closed));
        }
        let busy = to.busy.saturating_sub(from.busy);
        let runs = to.polls - from.polls;
        #[expect(clippy::cast_precision_loss, reason = "counts of runs that fit easily")]
        let over = (to.over_budget - from.over_budget) as f64 / runs.max(1) as f64;
        Run {
            share: busy.as_secs_f64() / passed.as_secs_f64() / f64::from(sessions),
            over,
            longest: to.longest,
            starts,
        }
    })
}

#[test]
#[ignore = "a measurement in real time, made every night and by `cargo xtask perf`"]
fn density() {
    let sessions = from_env("WAVEPORT_PERF_SESSIONS", 50);
    let questions = from_env("WAVEPORT_PERF_QUESTIONS", 2);
    let script = Script {
        answer: STORY,
        every: Duration::from_secs(14),
        ..Script::asking(questions, Codec::Opus, Delays::USUAL)
    };
    let before = reference();
    let watch = Watch::counting();
    let Run {
        share,
        over,
        longest,
        mut starts,
    } = run(script, sessions, &watch);
    let after = reference();

    // Every call ended as it should, and every question was answered.
    assert_eq!(watch.closed(), u64::from(sessions));
    let answers = u64::from(sessions) * u64::from(questions);
    assert_eq!(watch.counted(&ANSWERING), answers);

    // The reference work, as it was before and after: a machine that
    // changed its speed in between has measured nothing.
    let reference = before.min(after);
    let steady = before.max(after).as_secs_f64() / reference.as_secs_f64();
    let ratio = share / reference.as_secs_f64();
    let start = Spread::of(&mut starts).unwrap();
    println!("PERF density.sessions={sessions}");
    println!("PERF density.share_percent={:.4}", share * 100.0);
    println!("PERF density.sessions_per_thread={:.0}", 1.0 / share);
    println!("PERF density.reference_ms={:.3}", ms(reference));
    println!("PERF density.reference_drift={steady:.3}");
    println!("PERF density.ratio={ratio:.4}");
    println!("PERF poll.over_budget_share={over:.5}");
    println!("PERF poll.longest_ms={:.3}", ms(longest));
    println!("PERF start.p50_ms={:.3}", ms(start.p50));
    println!("PERF start.p99_ms={:.3}", ms(start.p99));

    // With this set, the ratio is held to what `perf/targets.toml`
    // says: for a machine nobody else is using, such as a nightly
    // runner. Without it the numbers are only printed.
    if std::env::var_os("WAVEPORT_PERF_GATE").is_some() {
        let targets = Targets::load();
        let held = targets.number("density.held_ratio");
        let tolerance = targets.number("density.tolerance");
        assert!(
            steady <= 1.0 + tolerance,
            "the reference work took {steady:.2} times as long at one end of the run as at the \
             other: the machine was not left alone, and nothing was measured"
        );
        assert!(
            ratio <= held * (1.0 + tolerance),
            "a session takes {ratio:.3} times the reference work, and perf/targets.toml holds \
             it to {held} with {tolerance} to spare"
        );
    }
}

/// How long the parts of a turn take in real time, by the marks the
/// sessions themselves make, while fifty of them share a shard. The
/// providers answer at once, so that what is left is this project's own
/// share: the time a session waits for its shard to run it, and for the
/// tick that paces what it sends.
#[test]
#[ignore = "a measurement in real time, made every night and by `cargo xtask perf`"]
fn turns() {
    let sessions = from_env("WAVEPORT_PERF_SESSIONS", 50);
    let questions = from_env("WAVEPORT_PERF_QUESTIONS", 2);
    let script = Script::asking(questions, Codec::Opus, Delays::NONE);
    let watch = Watch::keeping();
    let _ = run(script, sessions, &watch);

    assert_eq!(watch.closed(), u64::from(sessions));
    let answers = u64::from(sessions) * u64::from(questions);
    for (name, what) in [
        ("answering", ANSWERING),
        ("phrasing", PHRASING),
        ("sending", SENDING),
    ] {
        assert_eq!(watch.counted(&what), answers, "{name}");
        let spread = watch.spread(&what).unwrap();
        println!("PERF turn.{name}.p50_ms={:.3}", ms(spread.p50));
        println!("PERF turn.{name}.p99_ms={:.3}", ms(spread.p99));
        println!("PERF turn.{name}.max_ms={:.3}", ms(spread.max));
    }
}
