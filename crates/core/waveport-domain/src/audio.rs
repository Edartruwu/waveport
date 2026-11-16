//! What audio is called: its rates and the ways it is written on the wire.
//!
//! These are names and numbers only. The work on audio is done in
//! `waveport-media`; ports and adapters need the names without the work.

use crate::time::{MEDIA_TICKS_PER_SECOND, MediaDur};

/// The length of one frame of audio in the media path, in milliseconds.
pub const FRAME_MS: u32 = 20;

/// The most bytes one Opus packet can take.
pub const MAX_OPUS_PACKET: usize = 1_275;

/// The longest audio one packet may hold when it is not Opus, in
/// milliseconds.
pub const MAX_PLAIN_PACKET_MS: u32 = 60;

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

    /// The rate with this many samples per second, if it is one of the
    /// four.
    #[must_use]
    pub const fn from_hz(hz: u32) -> Option<Self> {
        match hz {
            8_000 => Some(Self::Hz8000),
            16_000 => Some(Self::Hz16000),
            24_000 => Some(Self::Hz24000),
            48_000 => Some(Self::Hz48000),
            _ => None,
        }
    }

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

/// Which of the two encodings of G.711.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Law {
    /// µ-law (PCMU): North America and Japan.
    Mu,
    /// A-law (PCMA): everywhere else.
    A,
}

/// A way audio is written on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    /// Opus.
    Opus,
    /// G.711, which is at 8 kHz.
    G711(Law),
    /// Samples as they are: 16 bits each, low byte first, at this rate.
    Pcm(Rate),
}

impl Codec {
    /// The rate the timestamps of its packets count at. For Opus that is
    /// 48 kHz whatever rate the audio was encoded from.
    #[must_use]
    pub const fn clock(self) -> Rate {
        match self {
            Self::Opus => Rate::Hz48000,
            Self::G711(_) => Rate::Hz8000,
            Self::Pcm(rate) => rate,
        }
    }

    /// The most bytes one packet of it is taken to have.
    #[must_use]
    pub const fn max_packet(self) -> usize {
        match self {
            Self::Opus => MAX_OPUS_PACKET,
            Self::G711(_) => Rate::Hz8000.samples_in_millis(MAX_PLAIN_PACKET_MS),
            Self::Pcm(rate) => rate.samples_in_millis(MAX_PLAIN_PACKET_MS) * 2,
        }
    }

    /// The highest rate at which audio put through it still sounds any
    /// different: there is nothing to gain from handing it more.
    #[must_use]
    pub const fn widest(self) -> Rate {
        match self {
            Self::Opus => Rate::Hz48000,
            Self::G711(_) => Rate::Hz8000,
            Self::Pcm(rate) => rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Codec, Law, Rate};
    use crate::time::MediaDur;

    #[test]
    fn every_rate_divides_the_media_clock() {
        for rate in Rate::ALL {
            assert_eq!(rate.ticks_per_sample() * rate.hz(), 48_000, "{rate:?}");
            assert_eq!(Rate::from_hz(rate.hz()), Some(rate));
        }
        assert_eq!(Rate::from_hz(44_100), None);
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

    #[test]
    fn each_codec_has_its_clock_and_its_packet_size() {
        let codecs = [
            Codec::Opus,
            Codec::G711(Law::Mu),
            Codec::G711(Law::A),
            Codec::Pcm(Rate::Hz16000),
        ];
        assert_eq!(
            codecs.map(Codec::clock),
            [Rate::Hz48000, Rate::Hz8000, Rate::Hz8000, Rate::Hz16000]
        );
        // 60 ms of G.711 is 480 bytes; of 16 kHz samples, 1,920.
        assert_eq!(codecs.map(Codec::max_packet), [1_275, 480, 480, 1_920]);
        assert_eq!(Codec::G711(Law::A).widest(), Rate::Hz8000);
    }
}
