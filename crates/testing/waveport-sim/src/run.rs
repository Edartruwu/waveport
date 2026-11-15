//! Running a scenario over a set of seeds.

use core::fmt;
use std::env::VarError;
use std::io::Write as _;

use crate::rng::{Seed, SimRng};
use crate::time::{SimTime, VirtualClock};

/// Environment variable that runs a single seed, to reproduce a failure.
pub const SEED_VAR: &str = "WAVEPORT_SIM_SEED";

/// Environment variable that sets how many seeds run: a count such as
/// `10000`, or a range such as `500..600`.
pub const SEEDS_VAR: &str = "WAVEPORT_SIM_SEEDS";

/// Which seeds to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedPlan {
    /// One seed.
    Single(Seed),
    /// `count` consecutive seeds beginning at `start`.
    Range {
        /// The first seed.
        start: u64,
        /// How many seeds follow, the first included.
        count: u64,
    },
}

impl SeedPlan {
    /// The plan the environment asks for, or `default` when it asks for none.
    ///
    /// [`SEED_VAR`] selects a single seed and wins over [`SEEDS_VAR`], which
    /// changes how many seeds run. A test therefore runs its default in
    /// ordinary use, a larger sweep on demand, and one seed to reproduce a
    /// failure, without being edited.
    ///
    /// # Errors
    ///
    /// Fails when a variable is set to something that is not a seed, a count
    /// or a range.
    pub fn from_env(default: Self) -> Result<Self, SeedPlanError> {
        let read = |variable: &'static str| match std::env::var(variable) {
            Ok(value) => Ok(Some(value)),
            Err(VarError::NotPresent) => Ok(None),
            Err(VarError::NotUnicode(raw)) => Err(SeedPlanError {
                variable,
                value: raw.to_string_lossy().into_owned(),
            }),
        };
        let seed = read(SEED_VAR)?;
        let seeds = read(SEEDS_VAR)?;
        Self::from_vars(seed.as_deref(), seeds.as_deref(), default)
    }

    /// [`from_env`](Self::from_env) with the values of the two variables
    /// passed in.
    ///
    /// # Errors
    ///
    /// Fails when a value is not a seed, a count or a range.
    pub fn from_vars(
        seed: Option<&str>,
        seeds: Option<&str>,
        default: Self,
    ) -> Result<Self, SeedPlanError> {
        let invalid = |variable: &'static str, value: &str| SeedPlanError {
            variable,
            value: value.to_owned(),
        };
        if let Some(text) = seed {
            return text
                .parse()
                .map(Self::Single)
                .map_err(|_| invalid(SEED_VAR, text));
        }
        let Some(text) = seeds else {
            return Ok(default);
        };
        let number = |part: &str| {
            part.parse::<Seed>()
                .map(|seed| seed.0)
                .map_err(|_| invalid(SEEDS_VAR, text))
        };
        if let Some((start, end)) = text.split_once("..") {
            let (start, end) = (number(start)?, number(end)?);
            let count = end
                .checked_sub(start)
                .ok_or_else(|| invalid(SEEDS_VAR, text))?;
            Ok(Self::Range { start, count })
        } else {
            let start = default.seeds().next().map_or(0, |seed| seed.0);
            Ok(Self::Range {
                start,
                count: number(text)?,
            })
        }
    }

    /// The seeds of the plan, in the order they run.
    pub fn seeds(self) -> impl Iterator<Item = Seed> {
        let (start, count) = match self {
            Self::Single(seed) => (seed.0, 1),
            Self::Range { start, count } => (start, count),
        };
        (0..count).map(move |offset| Seed(start.wrapping_add(offset)))
    }
}

/// A seed variable holds something that is not a seed, a count or a range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedPlanError {
    /// The variable at fault.
    pub variable: &'static str,
    /// What it was set to.
    pub value: String,
}

impl fmt::Display for SeedPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {} `{}`: expected a number such as 42 or 0x2a",
            self.variable, self.value
        )?;
        if self.variable == SEEDS_VAR {
            write!(f, ", or a range such as 500..600")?;
        }
        Ok(())
    }
}

impl std::error::Error for SeedPlanError {}

/// What one run of a scenario works with.
#[derive(Debug)]
pub struct SimContext {
    /// The run's source of randomness.
    pub rng: SimRng,
    /// The run's clock. It starts at zero and moves when the scenario moves it.
    pub clock: VirtualClock,
    seed: Seed,
    step: u64,
}

impl SimContext {
    /// The context of the run that belongs to `seed`.
    #[must_use]
    pub fn new(seed: Seed) -> Self {
        Self {
            rng: SimRng::new(seed),
            clock: VirtualClock::new(),
            seed,
            step: 0,
        }
    }

    /// The seed of this run.
    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.seed
    }

    /// Counts one step of the scenario and returns its number, starting at 1.
    ///
    /// A failure reports the step it happened in, which tells how far into
    /// the run to look.
    pub fn tick(&mut self) -> u64 {
        self.step = self.step.saturating_add(1);
        self.step
    }

    /// Number of steps counted so far.
    #[must_use]
    pub const fn step(&self) -> u64 {
        self.step
    }
}

/// A run that failed, with what is needed to repeat it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimFailure<E> {
    /// The seed that reproduces the failure.
    pub seed: Seed,
    /// The step the scenario was in.
    pub step: u64,
    /// The simulated time.
    pub at: SimTime,
    /// What the scenario reported.
    pub error: E,
}

impl<E: fmt::Display> fmt::Display for SimFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "simulation failed with seed {} at step {} (t={}): {}\n  \
             reproduce with: {SEED_VAR}={} <the same test command>",
            self.seed, self.step, self.at, self.error, self.seed
        )
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for SimFailure<E> {}

/// What a completed plan covered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Seeds that ran.
    pub seeds: u64,
    /// Steps counted across all of them.
    pub steps: u64,
}

/// Names the seed on standard error when a scenario panics instead of
/// returning an error, so the failure can still be reproduced.
struct PanicReport(Seed);

impl Drop for PanicReport {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Writing must not panic here: a second panic would abort.
            let _ = writeln!(
                std::io::stderr(),
                "simulation panicked with seed {}\n  \
                 reproduce with: {SEED_VAR}={} <the same test command>",
                self.0,
                self.0
            );
        }
    }
}

/// Runs `scenario` once for every seed of `plan`, stopping at the first failure.
///
/// The scenario gets a fresh [`SimContext`] each time. To stay repeatable it
/// must take all randomness from the context's generator and all time from
/// its clock.
///
/// # Errors
///
/// Returns the first failure, which names the seed that reproduces it. The
/// same line is written to standard error, where a failing test shows it.
pub fn run<E: fmt::Display>(
    plan: SeedPlan,
    mut scenario: impl FnMut(&mut SimContext) -> Result<(), E>,
) -> Result<RunSummary, SimFailure<E>> {
    let mut summary = RunSummary::default();
    for seed in plan.seeds() {
        let mut context = SimContext::new(seed);
        let report = PanicReport(seed);
        let outcome = scenario(&mut context);
        drop(report);
        if let Err(error) = outcome {
            let failure = SimFailure {
                seed,
                step: context.step,
                at: context.clock.now(),
                error,
            };
            let _ = writeln!(std::io::stderr(), "{failure}");
            return Err(failure);
        }
        summary.seeds = summary.seeds.saturating_add(1);
        summary.steps = summary.steps.saturating_add(context.step);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::{SEED_VAR, SEEDS_VAR, SeedPlan, SeedPlanError};
    use crate::rng::Seed;

    const DEFAULT: SeedPlan = SeedPlan::Range {
        start: 100,
        count: 8,
    };

    fn seeds(plan: SeedPlan) -> Vec<u64> {
        plan.seeds().map(|seed| seed.0).collect()
    }

    #[test]
    fn plans_list_their_seeds() {
        assert_eq!(seeds(SeedPlan::Single(Seed(7))), [7]);
        assert_eq!(seeds(SeedPlan::Range { start: 3, count: 3 }), [3, 4, 5]);
        assert_eq!(seeds(SeedPlan::Range { start: 3, count: 0 }), [0_u64; 0]);
        let wrapping = SeedPlan::Range {
            start: u64::MAX,
            count: 2,
        };
        assert_eq!(seeds(wrapping), [u64::MAX, 0]);
    }

    #[test]
    fn environment_selects_the_plan() {
        let plan = |seed, seeds| SeedPlan::from_vars(seed, seeds, DEFAULT);
        assert_eq!(plan(None, None), Ok(DEFAULT));
        assert_eq!(plan(Some("0x2a"), None), Ok(SeedPlan::Single(Seed(42))));
        // A single seed wins: it is how a failure is reproduced.
        assert_eq!(
            plan(Some("42"), Some("1000")),
            Ok(SeedPlan::Single(Seed(42)))
        );
        // A count keeps the default's first seed.
        assert_eq!(
            plan(None, Some("1000")),
            Ok(SeedPlan::Range {
                start: 100,
                count: 1000
            })
        );
        assert_eq!(
            plan(None, Some("500..600")),
            Ok(SeedPlan::Range {
                start: 500,
                count: 100
            })
        );
    }

    #[test]
    fn invalid_values_are_reported_with_their_variable() {
        let plan = |seed, seeds| SeedPlan::from_vars(seed, seeds, DEFAULT);
        let error = |variable, value: &str| {
            Err(SeedPlanError {
                variable,
                value: value.to_owned(),
            })
        };
        assert_eq!(plan(Some("abc"), None), error(SEED_VAR, "abc"));
        assert_eq!(plan(None, Some("many")), error(SEEDS_VAR, "many"));
        assert_eq!(plan(None, Some("9..3")), error(SEEDS_VAR, "9..3"));
        assert_eq!(plan(None, Some("1..x")), error(SEEDS_VAR, "1..x"));
        let message = SeedPlanError {
            variable: SEEDS_VAR,
            value: "many".to_owned(),
        }
        .to_string();
        assert_eq!(
            message,
            "invalid WAVEPORT_SIM_SEEDS `many`: expected a number such as 42 or 0x2a, \
             or a range such as 500..600"
        );
    }
}
