//! Where a scenario stands: the three states a listing can give it.
//!
//! Each function explores a scenario and checks that it stands where the
//! listing says. The check works both ways, so that a scenario which
//! starts to pass, or stops, cannot go unnoticed.
//!
//! ```
//! use waveport_kernel_sim::check::{Report, passes};
//! use waveport_kernel_sim::prelude::*;
//!
//! fn nothing_happens(lab: &mut Lab) -> Verdict {
//!     let mut sim = lab.sim(Setup::new());
//!     sim.wait_ms(1_000);
//!     sim.finish()
//! }
//!
//! // In a test: `fn nothing_happens() -> Result<(), Report>`.
//! let checked: Result<(), Report> = passes(nothing_happens);
//! assert!(checked.is_ok());
//! ```

use core::fmt;

use crate::explore::{Lab, explore};
use crate::failure::Verdict;

/// Why a scenario does not stand where its listing says. A test returns it,
/// and the test harness prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct Report(String);

impl fmt::Debug for Report {
    /// The report as written, so that a failing test shows it as text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\n{}", self.0)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Report {}

/// The scenario holds on every path.
///
/// # Errors
///
/// It fails on one; the report shows where.
pub fn passes(scenario: impl Fn(&mut Lab) -> Verdict) -> Result<(), Report> {
    match explore(scenario) {
        Ok(_) => Ok(()),
        Err(failure) => Err(Report(failure.to_string())),
    }
}

/// The scenario gets past its first check on every path it is run along and
/// then fails: the kernel does part of what it needs.
///
/// # Errors
///
/// It passes, and must be listed as passing; or it fails at its first
/// check, and must be listed as unstarted.
pub fn pending(scenario: impl Fn(&mut Lab) -> Verdict) -> Result<(), Report> {
    match explore(scenario) {
        Ok(explored) => Err(Report(format!(
            "this scenario now passes on all {} paths: list it with `passes`",
            explored.paths
        ))),
        Err(failure) if failure.ordinal == 1 => Err(Report(format!(
            "this scenario is listed as pending and fails at its first check: \
             list it with `unstarted`, or find out what broke\n{failure}"
        ))),
        Err(_) => Ok(()),
    }
}

/// The scenario fails at its first check: the kernel does nothing of what
/// it needs yet.
///
/// # Errors
///
/// It passes, or gets further than its first check, and must be listed
/// accordingly.
pub fn unstarted(scenario: impl Fn(&mut Lab) -> Verdict) -> Result<(), Report> {
    match explore(scenario) {
        Ok(explored) => Err(Report(format!(
            "this scenario now passes on all {} paths: list it with `passes`",
            explored.paths
        ))),
        Err(failure) if failure.ordinal == 1 => Ok(()),
        Err(failure) => Err(Report(format!(
            "this scenario now gets past its first check: list it with `pending`\n{failure}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{passes, pending, unstarted};
    use crate::explore::Lab;
    use crate::failure::Verdict;
    use crate::setup::Setup;

    /// Holds: nothing is said and nothing is expected.
    fn quiet(lab: &mut Lab) -> Verdict {
        let mut sim = lab.sim(Setup::new());
        sim.wait_ms(100);
        sim.finish()
    }

    /// Fails at its first check: the kernel does not commit turns yet.
    fn first(lab: &mut Lab) -> Verdict {
        let mut sim = lab.sim(Setup::new());
        let caller = sim.caller();
        sim.says(caller, "hello");
        sim.expect_turn(caller, "hello")?;
        sim.finish()
    }

    /// Fails at its second check.
    fn second(lab: &mut Lab) -> Verdict {
        let mut sim = lab.sim(Setup::new());
        let caller = sim.caller();
        sim.expect_context(&[])?;
        sim.says(caller, "hello");
        sim.expect_turn(caller, "hello")?;
        sim.finish()
    }

    #[test]
    fn each_listing_accepts_only_the_state_it_names() {
        assert!(passes(quiet).is_ok());
        assert!(passes(first).is_err());
        assert!(passes(second).is_err());

        assert!(unstarted(first).is_ok());
        assert!(unstarted(quiet).is_err());
        assert!(unstarted(second).is_err());

        assert!(pending(second).is_ok());
        assert!(pending(quiet).is_err());
        assert!(pending(first).is_err());
    }

    #[test]
    fn a_report_says_what_to_do() {
        let promoted = unstarted(quiet).unwrap_err();
        assert!(promoted.to_string().contains("list it with `passes`"));
        let moved = unstarted(second).unwrap_err();
        assert!(moved.to_string().contains("list it with `pending`"));
        let failed = format!("{:?}", passes(first).unwrap_err());
        assert!(failed.starts_with("\ncheck 1 failed at"), "{failed}");
        assert!(failed.contains("a committed turn in which the caller says \"hello\""));
    }
}
