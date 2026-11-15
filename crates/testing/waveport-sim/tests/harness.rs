//! The harness used the way a simulation test uses it.

use core::time::Duration;

use waveport_sim::{RunSummary, Schedule, Seed, SeedPlan, SimContext, SimTime, run};

/// A small simulation: timers that re-arm themselves at random intervals and
/// are sometimes dropped. Returns everything that happened, in order.
fn timers(sim: &mut SimContext) -> Vec<(SimTime, u32)> {
    let mut faults = sim.rng.fork("faults");
    let mut schedule = Schedule::new();
    for id in 0..4 {
        let delay = Duration::from_micros(sim.rng.range(1..5_000));
        schedule.push(sim.clock.now().after(delay), id);
    }
    let mut trace = Vec::new();
    while let Some((at, id)) = schedule.pop() {
        if sim.tick() > 200 {
            break;
        }
        sim.clock.advance_to(at);
        trace.push((sim.clock.now(), id));
        if !faults.chance(1, 10) {
            let delay = Duration::from_micros(sim.rng.range(1..5_000));
            schedule.push(sim.clock.now().after(delay), id);
        }
    }
    trace
}

fn trace_of(seed: u64) -> Vec<(SimTime, u32)> {
    let mut trace = Vec::new();
    let summary = run(SeedPlan::Single(Seed(seed)), |sim| {
        trace = timers(sim);
        Ok::<(), String>(())
    });
    assert_eq!(summary.map(|summary| summary.seeds), Ok(1));
    trace
}

#[test]
fn the_same_seed_gives_the_same_run() {
    let first = trace_of(42);
    assert!(first.len() > 10, "the scenario ended too early to compare");
    assert_eq!(first, trace_of(42));
}

#[test]
fn different_seeds_give_different_runs() {
    assert_ne!(trace_of(42), trace_of(43));
}

#[test]
fn time_never_goes_backwards_on_any_seed() {
    let plan = SeedPlan::Range {
        start: 0,
        count: 200,
    };
    let summary = run(plan, |sim| {
        let trace = timers(sim);
        match trace.windows(2).find(|pair| pair[0].0 > pair[1].0) {
            Some(pair) => Err(format!("{} came before {}", pair[0].0, pair[1].0)),
            None => Ok(()),
        }
    });
    let summary = summary.unwrap();
    assert_eq!(summary.seeds, 200);
    assert!(summary.steps > 200);
}

#[test]
fn a_failure_names_the_seed_step_and_time() {
    let plan = SeedPlan::Range {
        start: 10,
        count: 10,
    };
    let mut ran = Vec::new();
    let failure = run(plan, |sim| {
        ran.push(sim.seed().0);
        sim.tick();
        sim.tick();
        sim.clock.advance(Duration::from_millis(5));
        if sim.seed() == Seed(13) {
            Err("lost a frame")
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    // The run stops at the first failing seed.
    assert_eq!(ran, [10, 11, 12, 13]);
    assert_eq!(failure.seed, Seed(13));
    assert_eq!(failure.step, 2);
    assert_eq!(failure.at, SimTime::from_millis(5));
    assert_eq!(failure.error, "lost a frame");
    assert_eq!(
        failure.to_string(),
        "simulation failed with seed 0x000000000000000d at step 2 (t=0.005000000s): lost a frame\n  \
         reproduce with: WAVEPORT_SIM_SEED=0x000000000000000d <the same test command>"
    );
}

#[test]
fn the_reported_seed_reproduces_the_failure() {
    // A failure that depends on the random stream, found by a sweep.
    let scenario = |sim: &mut SimContext| {
        if sim.rng.range(0..50) == 0 {
            Err("rare")
        } else {
            Ok(())
        }
    };
    let plan = SeedPlan::Range {
        start: 0,
        count: 10_000,
    };
    let found = run(plan, scenario).unwrap_err();
    // Running only that seed, as the reproduction line says, fails the same way.
    let again = run(SeedPlan::Single(found.seed), scenario).unwrap_err();
    assert_eq!(again, found);
}

#[test]
fn an_empty_plan_runs_nothing() {
    let plan = SeedPlan::Range { start: 0, count: 0 };
    let summary = run(plan, |_| Err("never called"));
    assert_eq!(summary, Ok(RunSummary::default()));
}
