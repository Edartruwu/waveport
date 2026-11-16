//! Time, as the kernel sees it.
//!
//! The kernel reads no clock. Monotonic time arrives as an argument of
//! `step`, and positions in audio arrive inside events.

use core::time::Duration;

/// A reading of a monotonic clock, in nanoseconds from an origin the runtime
/// chooses. Only differences mean anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonoTime(u64);

impl MonoTime {
    /// The clock's origin.
    pub const ZERO: Self = Self(0);

    /// A reading `nanos` nanoseconds after the origin.
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// A reading `millis` milliseconds after the origin.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis.saturating_mul(1_000_000))
    }

    /// Nanoseconds since the origin.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// This reading moved later by `delay`. It stops at the largest value
    /// instead of wrapping.
    #[must_use]
    pub fn after(self, delay: Duration) -> Self {
        let nanos = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(nanos))
    }

    /// How long after `earlier` this reading is; zero if it is not after.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

/// Ticks per second of [`MediaTime`] and [`MediaDur`].
pub const MEDIA_TICKS_PER_SECOND: u32 = 48_000;

/// A position on an audio track, in ticks of 1/48,000 s. Every sample rate
/// the media path uses divides this evenly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaTime(u64);

impl MediaTime {
    /// The start of a track.
    pub const ZERO: Self = Self(0);

    /// The position `ticks` ticks into a track.
    #[must_use]
    pub const fn from_ticks(ticks: u64) -> Self {
        Self(ticks)
    }

    /// The position `millis` milliseconds into a track.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis.saturating_mul(MEDIA_TICKS_PER_SECOND as u64 / 1_000))
    }

    /// Ticks from the start of the track.
    #[must_use]
    pub const fn ticks(self) -> u64 {
        self.0
    }

    /// This position moved later by `length`.
    #[must_use]
    pub const fn after(self, length: MediaDur) -> Self {
        Self(self.0.saturating_add(length.0 as u64))
    }

    /// How far after `earlier` this position is; zero if it is not after.
    /// Lengths beyond what [`MediaDur`] holds, about a day, are clamped.
    #[must_use]
    pub const fn since(self, earlier: Self) -> MediaDur {
        let ticks = self.0.saturating_sub(earlier.0);
        if ticks > u32::MAX as u64 {
            MediaDur(u32::MAX)
        } else {
            #[expect(clippy::cast_possible_truncation, reason = "checked just above")]
            MediaDur(ticks as u32)
        }
    }
}

/// A length of audio in ticks of 1/48,000 s. It holds a little under a day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaDur(u32);

impl MediaDur {
    /// No audio.
    pub const ZERO: Self = Self(0);

    /// A length of `ticks` ticks.
    #[must_use]
    pub const fn from_ticks(ticks: u32) -> Self {
        Self(ticks)
    }

    /// A length of `millis` milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u32) -> Self {
        Self(millis.saturating_mul(MEDIA_TICKS_PER_SECOND / 1_000))
    }

    /// The length in ticks.
    #[must_use]
    pub const fn ticks(self) -> u32 {
        self.0
    }

    /// The length in whole milliseconds.
    #[must_use]
    pub const fn as_millis(self) -> u32 {
        self.0 / (MEDIA_TICKS_PER_SECOND / 1_000)
    }

    /// The sum of two lengths, clamped at the largest.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// This length less `other`; zero if `other` is longer.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

/// A stretch of a track: `start` up to, but not including, `end`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaSpan {
    /// Where the stretch begins.
    pub start: MediaTime,
    /// Where it ends. Not before `start`.
    pub end: MediaTime,
}

impl MediaSpan {
    /// The stretch from `start` to `end`. If `end` is earlier, the stretch is
    /// empty and sits at `start`.
    #[must_use]
    pub const fn new(start: MediaTime, end: MediaTime) -> Self {
        if end.0 < start.0 {
            Self { start, end: start }
        } else {
            Self { start, end }
        }
    }

    /// How long the stretch is.
    #[must_use]
    pub const fn len(self) -> MediaDur {
        self.end.since(self.start)
    }

    /// Whether the stretch holds no audio.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.end.0 <= self.start.0
    }

    /// Whether the two stretches share any audio.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.start.0 < other.end.0 && other.start.0 < self.end.0
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{MediaDur, MediaSpan, MediaTime, MonoTime};

    #[test]
    fn monotonic_time_saturates() {
        let start = MonoTime::from_millis(5);
        assert_eq!(
            start.after(Duration::from_millis(3)),
            MonoTime::from_millis(8)
        );
        assert_eq!(
            MonoTime::from_millis(8).since(start),
            Duration::from_millis(3)
        );
        assert_eq!(start.since(MonoTime::from_millis(8)), Duration::ZERO);
        assert_eq!(start.after(Duration::MAX).as_nanos(), u64::MAX);
    }

    #[test]
    fn media_time_counts_ticks_of_a_48_khz_clock() {
        assert_eq!(MediaTime::from_millis(20).ticks(), 960);
        assert_eq!(MediaDur::from_millis(20).ticks(), 960);
        assert_eq!(MediaDur::from_ticks(960).as_millis(), 20);
        let later = MediaTime::from_millis(20).after(MediaDur::from_millis(10));
        assert_eq!(later, MediaTime::from_millis(30));
        assert_eq!(
            later.since(MediaTime::from_millis(20)),
            MediaDur::from_millis(10)
        );
        assert_eq!(MediaTime::ZERO.since(later), MediaDur::ZERO);
    }

    #[test]
    fn spans_overlap_only_when_they_share_audio() {
        let span = |a, b| MediaSpan::new(MediaTime::from_millis(a), MediaTime::from_millis(b));
        assert!(span(0, 100).overlaps(span(50, 150)));
        assert!(!span(0, 100).overlaps(span(100, 200)));
        assert!(span(10, 5).is_empty());
        assert_eq!(span(10, 30).len(), MediaDur::from_millis(20));
    }
}
