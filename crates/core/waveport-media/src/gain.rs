//! Volume.

/// A volume, as a share of full volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Gain(u16);

impl Gain {
    /// No sound.
    pub const SILENT: Self = Self(0);

    /// The sound as it is.
    pub const FULL: Self = Self(1 << 15);

    /// `numerator` parts in `denominator` of full volume, and full volume
    /// at most.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "less than full volume, which fits"
    )]
    pub const fn ratio(numerator: u16, denominator: u16) -> Self {
        if denominator == 0 || numerator >= denominator {
            return Self::FULL;
        }
        Self((((numerator as u32) << 15) / denominator as u32) as u16)
    }

    /// The volume in parts of 32,768.
    #[must_use]
    pub const fn parts(self) -> u16 {
        self.0
    }

    /// `sample` at the volume that is `parts` in 32,768.
    fn applied(sample: i16, parts: i32) -> i16 {
        // Full volume is one, so the product is at most a sample.
        let scaled = (i32::from(sample) * parts + (1 << 14)) >> 15;
        i16::try_from(scaled).unwrap_or(sample)
    }
}

/// Plays `samples` at `gain`.
pub fn scale(samples: &mut [i16], gain: Gain) {
    if gain == Gain::FULL {
        return;
    }
    let parts = i32::from(gain.0);
    for sample in samples {
        *sample = Gain::applied(*sample, parts);
    }
}

/// Plays `samples` at a volume that moves evenly from `from`, which the
/// first sample has, to `to`, which the sample after the last would have.
pub fn ramp(samples: &mut [i16], from: Gain, to: Gain) {
    if from == to {
        scale(samples, from);
        return;
    }
    let (from, to) = (i64::from(from.0), i64::from(to.0));
    let length = i64::try_from(samples.len()).unwrap_or(i64::MAX);
    for (step, sample) in (0..length).zip(samples) {
        let parts = from + (to - from) * step / length;
        *sample = Gain::applied(*sample, i32::try_from(parts).unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use super::{Gain, ramp, scale};

    #[test]
    fn a_ratio_is_a_share_of_full_volume() {
        assert_eq!(Gain::ratio(1, 4).parts(), 8_192);
        assert_eq!(Gain::ratio(0, 4), Gain::SILENT);
        assert_eq!(Gain::ratio(4, 4), Gain::FULL);
        assert_eq!(Gain::ratio(9, 4), Gain::FULL, "no louder than it is");
        assert_eq!(Gain::ratio(1, 0), Gain::FULL);
        assert!(Gain::ratio(1, 4) < Gain::ratio(1, 2));
    }

    #[test]
    fn scaling_keeps_the_sign_and_the_ends_of_the_range() {
        let mut samples = [i16::MAX, i16::MIN, 1_000, -1_000, 1, -1, 0];
        scale(&mut samples, Gain::FULL);
        assert_eq!(samples, [i16::MAX, i16::MIN, 1_000, -1_000, 1, -1, 0]);
        scale(&mut samples, Gain::ratio(1, 4));
        assert_eq!(samples, [8_192, -8_192, 250, -250, 0, 0, 0]);
        scale(&mut samples, Gain::SILENT);
        assert_eq!(samples, [0; 7]);
    }

    #[test]
    fn a_ramp_moves_evenly_and_stops_short_of_its_end() {
        let mut samples = [10_000; 5];
        ramp(&mut samples, Gain::FULL, Gain::SILENT);
        assert_eq!(samples, [10_000, 8_000, 6_000, 4_000, 2_000]);

        let mut samples = [10_000; 4];
        ramp(&mut samples, Gain::ratio(1, 4), Gain::FULL);
        assert_eq!(samples, [2_500, 4_375, 6_250, 8_125]);

        // The same volume at both ends is no ramp.
        let mut samples = [10_000; 3];
        ramp(&mut samples, Gain::ratio(1, 2), Gain::ratio(1, 2));
        assert_eq!(samples, [5_000; 3]);
        ramp(&mut [], Gain::FULL, Gain::SILENT);
    }
}
