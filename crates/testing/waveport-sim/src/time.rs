//! Simulated time.

use core::fmt;
use core::time::Duration;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// A point in simulated time: nanoseconds since the run started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimTime(u64);

impl SimTime {
    /// The start of a run.
    pub const ZERO: Self = Self(0);

    /// The point `nanos` nanoseconds after the start.
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// The point `millis` milliseconds after the start.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis.saturating_mul(1_000_000))
    }

    /// Nanoseconds since the start.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// The point `delay` later. Stops at the largest representable time,
    /// about 584 years in.
    #[must_use]
    pub fn after(self, delay: Duration) -> Self {
        let nanos = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(nanos))
    }

    /// How long after `earlier` this point is; zero if it is not after it.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

impl fmt::Display for SimTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{:09}s",
            self.0 / NANOS_PER_SECOND,
            self.0 % NANOS_PER_SECOND
        )
    }
}

/// A clock that moves only when told to, and never backwards.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VirtualClock {
    now: SimTime,
}

impl VirtualClock {
    /// A clock at [`SimTime::ZERO`].
    #[must_use]
    pub const fn new() -> Self {
        Self { now: SimTime::ZERO }
    }

    /// The current time.
    #[must_use]
    pub const fn now(&self) -> SimTime {
        self.now
    }

    /// Moves the clock forward by `delay` and returns the new time.
    pub fn advance(&mut self, delay: Duration) -> SimTime {
        self.now = self.now.after(delay);
        self.now
    }

    /// Moves the clock forward to `target` and returns the new time.
    ///
    /// A target in the past leaves the clock where it is.
    pub fn advance_to(&mut self, target: SimTime) -> SimTime {
        self.now = self.now.max(target);
        self.now
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{SimTime, VirtualClock};

    #[test]
    fn time_arithmetic() {
        let start = SimTime::from_millis(1_500);
        assert_eq!(start.as_nanos(), 1_500_000_000);
        assert_eq!(
            start.after(Duration::from_millis(250)),
            SimTime::from_millis(1_750)
        );
        assert_eq!(
            SimTime::from_millis(1_750).since(start),
            Duration::from_millis(250)
        );
        assert_eq!(start.since(SimTime::from_millis(2_000)), Duration::ZERO);
        assert_eq!(start.to_string(), "1.500000000s");
        assert_eq!(SimTime::from_nanos(7).to_string(), "0.000000007s");
    }

    #[test]
    fn time_saturates_instead_of_wrapping() {
        let end = SimTime::from_nanos(u64::MAX);
        assert_eq!(end.after(Duration::from_secs(1)), end);
        assert_eq!(SimTime::ZERO.after(Duration::MAX), end);
        assert_eq!(SimTime::from_millis(u64::MAX), end);
    }

    #[test]
    fn clock_never_goes_back() {
        let mut clock = VirtualClock::new();
        assert_eq!(clock.now(), SimTime::ZERO);
        assert_eq!(
            clock.advance(Duration::from_millis(20)),
            SimTime::from_millis(20)
        );
        assert_eq!(
            clock.advance_to(SimTime::from_millis(50)),
            SimTime::from_millis(50)
        );
        assert_eq!(
            clock.advance_to(SimTime::from_millis(10)),
            SimTime::from_millis(50)
        );
        assert_eq!(clock.now(), SimTime::from_millis(50));
    }
}
