//! Sessions made up from seeds, run by the real session actor.
//!
//! The kernel is tried in `waveport-domain` against a world that is made
//! of events. Here the same is asked of the whole session: the kernel,
//! the audio path, the listening models' hand-off and the actor that
//! holds them together, with a fake in every port and the kernel's
//! monitor watching every step. A seed decides how late every part
//! answers, what the line does to the callers' packets, which parts fail
//! and when, and at which record of its journal the process dies.
//!
//! `stories` tells the six stories of the kernel's scenarios again, each
//! under many seeds. `sweeps` makes up sessions nobody wrote.
//!
//! A failure prints its seed and the last steps of the session.
//! `WAVEPORT_SIM_SEED=<seed>` runs that session alone and prints all of
//! its steps, and `WAVEPORT_SIM_SEEDS=<count>` runs more sessions than
//! the default. With `--nocapture` each test prints what its runs did,
//! added up.

#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

mod stories;
mod sweeps;
mod world;

use waveport_sim::{Seed, SeedPlan};

use self::world::{Broken, Outcome, Plan, Tally, run};

/// How many seeds each story and each sweep is run under in an ordinary
/// test run. Every night each is run under a thousand, and other ones
/// each night.
const SEEDS: u64 = 25;

/// Runs `story` under each of `seeds` seeds, or under those the
/// environment asks for, and adds up what the runs did. `check` looks at
/// each run for what the story is about.
///
/// A run that breaks something is run again with every step written
/// down, and fails the test with its seed and its last steps.
fn told(
    seeds: u64,
    story: impl Fn(u64) -> Plan,
    check: impl Fn(&Plan, &Outcome) -> Result<(), String>,
) -> Option<Tally> {
    let default = SeedPlan::Range {
        start: 0,
        count: seeds,
    };
    let plan = SeedPlan::from_env(default).unwrap();
    let mut tally = Tally::default();
    for Seed(seed) in plan.seeds() {
        let world = story(seed);
        let outcome = run(&world, false).and_then(|outcome| {
            check(&world, &outcome).map_err(Broken)?;
            Ok(outcome)
        });
        match outcome {
            Ok(outcome) => tally.add(&outcome.tally),
            Err(Broken(first)) => {
                // Again, with every step written down. The world is
                // made anew: what it rolled the first time is rolled
                // again.
                let world = story(seed);
                let explained = match run(&world, true) {
                    Ok(outcome) => {
                        // All of it for a seed that was asked for by
                        // itself, and otherwise the end.
                        let shown = if matches!(plan, SeedPlan::Single(_)) {
                            outcome.log.len()
                        } else {
                            160
                        };
                        let steps = outcome.log.len().saturating_sub(shown);
                        let again = check(&world, &outcome).err().unwrap_or_default();
                        format!("{again}\n{}", outcome.log[steps..].join("\n"))
                    }
                    Err(Broken(what)) => what,
                };
                panic!(
                    "seed {seed}: {first}\n{explained}\n\nreproduce with: \
                     WAVEPORT_SIM_SEED={seed} <the same test command>"
                );
            }
        }
    }
    // What the runs did, for whoever asks to see it.
    let thread = std::thread::current();
    println!("{}: {tally:?}", thread.name().unwrap_or("runs"));
    // A test asks of its sweep that it was not empty only when all of it
    // ran.
    (tally.runs >= seeds).then_some(tally)
}
