//! Audio as it crosses a port.
//!
//! Audio is one channel of 16-bit samples at one of the four
//! [rates](Rate). Going out to an adapter it is borrowed, as an
//! [`AudioRef`]. Coming in from one it is owned, as [`PcmBytes`]: the bytes
//! as they came off the wire, so that an adapter that was sent plain
//! samples hands them on without copying them.

use core::fmt;

use bytes::Bytes;
pub use waveport_domain::audio::{Codec, Law, Rate};
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};

/// A piece of a participant's audio, lent to an adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioRef<'a> {
    /// Where its first sample is on the media clock.
    pub at: MediaTime,
    /// Its rate.
    pub rate: Rate,
    /// The samples.
    pub samples: &'a [i16],
}

impl AudioRef<'_> {
    /// How long it lasts.
    #[must_use]
    pub fn length(&self) -> MediaDur {
        self.rate.length_of(self.samples.len())
    }

    /// The stretch of the media clock it covers.
    #[must_use]
    pub fn span(&self) -> MediaSpan {
        MediaSpan::new(self.at, self.at.after(self.length()))
    }
}

/// The bytes given do not make whole samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct OddLength;

impl fmt::Display for OddLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an odd number of bytes does not make whole 16-bit samples")
    }
}

impl core::error::Error for OddLength {}

/// Audio an adapter hands over: 16-bit samples, low byte first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PcmBytes {
    bytes: Bytes,
}

impl PcmBytes {
    /// Audio from its bytes.
    ///
    /// # Errors
    ///
    /// The number of bytes is odd. A provider may cut its stream in the
    /// middle of a sample; the adapter has to keep the odd byte for the
    /// next piece, because dropping it would turn everything after it into
    /// noise.
    pub fn new(bytes: Bytes) -> Result<Self, OddLength> {
        if bytes.len().is_multiple_of(2) {
            Ok(Self { bytes })
        } else {
            Err(OddLength)
        }
    }

    /// Audio from samples, which are copied.
    #[must_use]
    pub fn from_samples(samples: &[i16]) -> Self {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        Self {
            bytes: Bytes::from(bytes),
        }
    }

    /// How many samples it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len() / 2
    }

    /// Whether it holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The samples, in order.
    pub fn samples(&self) -> impl ExactSizeIterator<Item = i16> + '_ {
        let (pairs, _) = self.bytes.as_chunks::<2>();
        pairs.iter().map(|pair| i16::from_le_bytes(*pair))
    }

    /// Copies samples into `out`, leaving out the first `skip`, and
    /// returns how many were copied: as many as there are, or as `out` has
    /// room for.
    pub fn copy_to(&self, skip: usize, out: &mut [i16]) -> usize {
        let mut copied = 0;
        for (sample, slot) in self.samples().skip(skip).zip(out.iter_mut()) {
            *slot = sample;
            copied += 1;
        }
        copied
    }
}

/// A set of rates: the ones an adapter can take, or can give.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RateSet(u8);

impl RateSet {
    /// No rate.
    pub const NONE: Self = Self(0);
    /// All four.
    pub const ALL: Self = Self::of(&Rate::ALL);

    const fn bit(rate: Rate) -> u8 {
        match rate {
            Rate::Hz8000 => 1,
            Rate::Hz16000 => 2,
            Rate::Hz24000 => 4,
            Rate::Hz48000 => 8,
        }
    }

    /// The set of these rates.
    #[must_use]
    pub const fn of(rates: &[Rate]) -> Self {
        let mut bits = 0;
        let mut rest = rates;
        while let [rate, tail @ ..] = rest {
            bits |= Self::bit(*rate);
            rest = tail;
        }
        Self(bits)
    }

    /// Whether `rate` is in the set.
    #[must_use]
    pub const fn contains(self, rate: Rate) -> bool {
        self.0 & Self::bit(rate) != 0
    }

    /// Whether no rate is in the set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The rates in the set, from the lowest.
    pub fn iter(self) -> impl Iterator<Item = Rate> {
        Rate::ALL
            .into_iter()
            .filter(move |rate| self.contains(*rate))
    }

    /// The rate in the set that is nearest to `wanted`: `wanted` itself
    /// when it is there, otherwise the lowest above it, so that nothing is
    /// lost, and otherwise the highest below it.
    #[must_use]
    pub fn nearest(self, wanted: Rate) -> Option<Rate> {
        self.iter()
            .find(|rate| *rate >= wanted)
            .or_else(|| self.iter().last())
    }
}

impl fmt::Debug for RateSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use waveport_domain::time::{MediaDur, MediaTime};

    use super::{AudioRef, OddLength, PcmBytes, Rate, RateSet};

    #[test]
    fn lent_audio_knows_the_time_it_covers() {
        let samples = [0_i16; 320];
        let audio = AudioRef {
            at: MediaTime::from_millis(100),
            rate: Rate::Hz16000,
            samples: &samples,
        };
        assert_eq!(audio.length(), MediaDur::from_millis(20));
        assert_eq!(audio.span().end, MediaTime::from_millis(120));
    }

    #[test]
    fn bytes_are_samples_low_byte_first() {
        let pcm = PcmBytes::new(Bytes::from_static(&[0x01, 0x00, 0xFF, 0xFF, 0x00, 0x80])).unwrap();
        assert_eq!(pcm.len(), 3);
        assert_eq!(pcm.samples().collect::<Vec<_>>(), [1, -1, i16::MIN]);
        assert_eq!(PcmBytes::from_samples(&[1, -1, i16::MIN]), pcm);
        assert!(PcmBytes::default().is_empty());
    }

    #[test]
    fn half_a_sample_is_refused() {
        assert_eq!(
            PcmBytes::new(Bytes::from_static(&[1, 2, 3])),
            Err(OddLength)
        );
    }

    #[test]
    fn samples_are_copied_out_as_far_as_there_is_room() {
        let pcm = PcmBytes::from_samples(&[10, 20, 30, 40, 50]);
        let mut out = [0; 3];
        assert_eq!(pcm.copy_to(0, &mut out), 3);
        assert_eq!(out, [10, 20, 30]);
        assert_eq!(pcm.copy_to(3, &mut out), 2);
        assert_eq!(out, [40, 50, 30]);
        assert_eq!(pcm.copy_to(9, &mut out), 0);
    }

    #[test]
    fn a_set_of_rates_gives_the_nearest_one() {
        let set = RateSet::of(&[Rate::Hz16000, Rate::Hz24000]);
        assert!(set.contains(Rate::Hz16000) && !set.contains(Rate::Hz8000));
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            [Rate::Hz16000, Rate::Hz24000]
        );
        assert_eq!(set.nearest(Rate::Hz16000), Some(Rate::Hz16000));
        // Nothing is lost by going up.
        assert_eq!(set.nearest(Rate::Hz8000), Some(Rate::Hz16000));
        // With nothing above, the highest below.
        assert_eq!(set.nearest(Rate::Hz48000), Some(Rate::Hz24000));
        assert_eq!(RateSet::NONE.nearest(Rate::Hz16000), None);
        assert!(RateSet::NONE.is_empty());
        assert_eq!(RateSet::ALL.iter().count(), 4);
        assert_eq!(format!("{set:?}"), "{Hz16000, Hz24000}");
    }
}
