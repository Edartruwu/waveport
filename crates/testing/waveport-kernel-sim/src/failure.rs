//! How a scenario reports that something did not hold.

use core::fmt;

/// What a scenario returns: nothing, or the first thing that did not hold.
/// The failure is boxed because it is large and almost never there.
pub type Verdict = Result<(), Box<Failure>>;

/// An expectation that did not hold, with what is needed to see why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// Which of the scenario's checks failed, counting from 1 in the order
    /// they ran. 0 for a failure of the exploration itself.
    pub ordinal: u32,
    /// What the scenario expected.
    pub expected: String,
    /// What happened instead.
    pub found: String,
    /// The source file of the check.
    pub file: &'static str,
    /// Its line.
    pub line: u32,
    /// Virtual time when the check gave up, in milliseconds.
    pub at_ms: u64,
    /// The choices that led here, one per [`Lab::either`](crate::Lab::either)
    /// and per step of a [`Sim::race`](crate::Sim::race).
    pub path: Vec<usize>,
    /// The last steps of the session.
    pub log: Vec<String>,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ordinal == 0 {
            writeln!(f, "the exploration failed")?;
        } else {
            writeln!(
                f,
                "check {} failed at {}:{}, {} ms into the session",
                self.ordinal, self.file, self.line, self.at_ms
            )?;
        }
        writeln!(f, "  expected: {}", self.expected)?;
        writeln!(f, "  but:      {}", self.found)?;
        writeln!(f, "  path:     {:?}", self.path)?;
        if !self.log.is_empty() {
            writeln!(f, "  last steps:")?;
            for line in &self.log {
                writeln!(f, "    {line}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}
