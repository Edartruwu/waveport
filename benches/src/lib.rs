//! Waveport benchmarks.
//!
//! Stability: internal
//!
//! Two kinds of benchmark live in this package:
//!
//! - `wall/`: wall-clock time, measured with criterion. Good for seeing how
//!   fast something is; too noisy on shared machines to gate on.
//! - `ir/`: instructions executed, counted under Valgrind. Repeatable to
//!   within a fraction of a percent, so CI fails a change that makes a
//!   benchmark more than 2% more expensive. Linux only.
//!
//! This library holds the workloads, so that both kinds measure the same code.

pub mod journal;
pub mod kernel;
pub mod media;

use waveport_sim::{Seed, SimRng};

/// The smoke workload: draws 1024 numbers from the simulation's generator
/// and folds them together.
///
/// It exists to prove that both harnesses work, whatever the product code
/// does.
#[must_use]
pub fn smoke(seed: u64) -> u64 {
    let mut rng = SimRng::new(Seed(seed));
    (0..1024).fold(0, |sum: u64, _| sum.wrapping_add(rng.next_u64()))
}

#[cfg(test)]
mod tests {
    use super::smoke;

    #[test]
    fn smoke_is_deterministic() {
        assert_eq!(smoke(42), smoke(42));
        assert_ne!(smoke(42), smoke(43));
    }
}
