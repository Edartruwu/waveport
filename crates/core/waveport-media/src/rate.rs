//! Sample rates.

use waveport_domain::time::{MEDIA_TICKS_PER_SECOND, MediaDur};

/// The length of one frame of audio in the media path, in milliseconds.
pub const FRAME_MS: u32 = 20;

/// A sample rate the media path works at.
///
/// Each divides the 48 kHz media clock, so a number of samples at any of
/// them is a whole number of ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rate {
    /// 8 kHz: telephony.
    Hz8000,
    /// 16 kHz: what speech recognition and the voice models take.
    Hz16000,
    /// 24 kHz: what speech synthesis gives.
    Hz24000,
    /// 48 kHz: the media clock, and what sound cards and browsers use.
    Hz48000,
}

impl Rate {
    /// Every rate, from the lowest.
    pub const ALL: [Self; 4] = [Self::Hz8000, Self::Hz16000, Self::Hz24000, Self::Hz48000];

    /// Samples per second.
    #[must_use]
    pub const fn hz(self) -> u32 {
        match self {
            Self::Hz8000 => 8_000,
            Self::Hz16000 => 16_000,
            Self::Hz24000 => 24_000,
            Self::Hz48000 => 48_000,
        }
    }

    /// Ticks of the media clock in one sample.
    #[must_use]
    pub const fn ticks_per_sample(self) -> u32 {
        MEDIA_TICKS_PER_SECOND / self.hz()
    }

    /// Samples in `millis` milliseconds.
    #[must_use]
    pub const fn samples_in_millis(self, millis: u32) -> usize {
        (self.hz() / 1_000) as usize * millis as usize
    }

    /// Samples in one frame.
    #[must_use]
    pub const fn frame(self) -> usize {
        self.samples_in_millis(FRAME_MS)
    }

    /// How long `samples` samples last. A length the media clock cannot
    /// hold, about a day, is given as the longest it can.
    #[must_use]
    pub fn length_of(self, samples: usize) -> MediaDur {
        let ticks = u32::try_from(samples)
            .unwrap_or(u32::MAX)
            .saturating_mul(self.ticks_per_sample());
        MediaDur::from_ticks(ticks)
    }

    /// Whole samples in `length`.
    #[must_use]
    pub const fn samples_in(self, length: MediaDur) -> usize {
        (length.ticks() / self.ticks_per_sample()) as usize
    }
}

#[cfg(test)]
mod tests {
    use waveport_domain::time::MediaDur;

    use super::Rate;

    #[test]
    fn every_rate_divides_the_media_clock() {
        for rate in Rate::ALL {
            assert_eq!(rate.ticks_per_sample() * rate.hz(), 48_000, "{rate:?}");
        }
    }

    #[test]
    fn a_frame_is_20_ms() {
        let frames = Rate::ALL.map(Rate::frame);
        assert_eq!(frames, [160, 320, 480, 960]);
        for rate in Rate::ALL {
            assert_eq!(rate.length_of(rate.frame()), MediaDur::from_millis(20));
            assert_eq!(rate.samples_in(MediaDur::from_millis(20)), rate.frame());
        }
    }

    #[test]
    fn lengths_round_down_to_whole_samples_and_do_not_overflow() {
        // Five ticks are less than one sample at 8 kHz.
        assert_eq!(Rate::Hz8000.samples_in(MediaDur::from_ticks(5)), 0);
        assert_eq!(Rate::Hz8000.samples_in(MediaDur::from_ticks(13)), 2);
        assert_eq!(
            Rate::Hz8000.length_of(usize::MAX),
            MediaDur::from_ticks(u32::MAX)
        );
    }
}
