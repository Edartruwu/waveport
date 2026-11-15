//! Waveport simulation harness.
//!
//! Stability: experimental
//!
//! Runs a scenario many times, each time with a different seed, and makes
//! every run repeatable: the seed decides all randomness, and time is a
//! number the scenario advances itself. A failure names the seed that
//! reproduces it.
//!
//! ```
//! use core::time::Duration;
//! use waveport_sim::{Schedule, SeedPlan, run};
//!
//! let plan = SeedPlan::Range { start: 0, count: 100 };
//! let summary = run(plan, |sim| {
//!     let mut timers = Schedule::new();
//!     for id in 0..8_u32 {
//!         let delay = Duration::from_millis(sim.rng.range(1..50));
//!         timers.push(sim.clock.now().after(delay), id);
//!     }
//!     let mut last = sim.clock.now();
//!     while let Some((at, _id)) = timers.pop() {
//!         sim.tick();
//!         sim.clock.advance_to(at);
//!         if sim.clock.now() < last {
//!             return Err("time went backwards");
//!         }
//!         last = sim.clock.now();
//!     }
//!     Ok(())
//! });
//! assert_eq!(summary.map(|summary| summary.seeds), Ok(100));
//! ```
//!
//! The harness has no dependencies. In particular the random number
//! generator is written here, so a seed from an old bug report produces the
//! same run for as long as this crate exists.

mod rng;
mod run;
mod schedule;
mod time;

pub use rng::{Seed, SimRng};
pub use run::{
    RunSummary, SEED_VAR, SEEDS_VAR, SeedPlan, SeedPlanError, SimContext, SimFailure, run,
};
pub use schedule::Schedule;
pub use time::{SimTime, VirtualClock};
