//! Opus, through libopus.
//!
//! One channel only: a voice session has no use for stereo. A decoder and
//! an encoder each work at one of the media path's sample rates, whatever
//! rate the other end of the stream uses, so audio is decoded straight to
//! the rate speech recognition takes and encoded straight from the rate
//! speech synthesis gives.

use core::fmt;

use ::opus::{Application, Bitrate, Channels, ErrorCode};

use super::CodecError;
use crate::rate::Rate;

/// The most bytes one encoded frame can take.
pub const MAX_PACKET: usize = 1_275;

/// The longest audio one packet can hold, in milliseconds.
pub const MAX_PACKET_MS: u32 = 120;

fn refused(error: &::opus::Error) -> CodecError {
    match error.code() {
        ErrorCode::BadArg | ErrorCode::BufferTooSmall => CodecError::Length,
        ErrorCode::InvalidPacket => CodecError::Packet,
        ErrorCode::Unimplemented => CodecError::Settings,
        ErrorCode::InternalError
        | ErrorCode::InvalidState
        | ErrorCode::AllocFail
        | ErrorCode::Unknown => CodecError::Internal,
    }
}

/// Decodes one Opus stream.
pub struct OpusDecoder {
    inner: ::opus::Decoder,
    rate: Rate,
}

impl OpusDecoder {
    /// A decoder that gives audio at `rate`.
    ///
    /// # Errors
    ///
    /// libopus could not set one up.
    pub fn new(rate: Rate) -> Result<Self, CodecError> {
        let inner = ::opus::Decoder::new(rate.hz(), Channels::Mono).map_err(|e| refused(&e))?;
        Ok(Self { inner, rate })
    }

    /// The rate of the audio it gives.
    #[must_use]
    pub const fn rate(&self) -> Rate {
        self.rate
    }

    /// How many samples `packet` decodes to.
    ///
    /// # Errors
    ///
    /// The packet is not valid Opus.
    pub fn samples_in(&self, packet: &[u8]) -> Result<usize, CodecError> {
        if packet.is_empty() {
            return Err(CodecError::Packet);
        }
        self.inner.get_nb_samples(packet).map_err(|e| refused(&e))
    }

    /// Decodes `packet` into the start of `out`, and returns how many
    /// samples it wrote.
    ///
    /// # Errors
    ///
    /// The packet is not valid Opus, or `out` is too short for it.
    pub fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize, CodecError> {
        // To libopus an empty packet means a lost one.
        if packet.is_empty() {
            return Err(CodecError::Packet);
        }
        self.inner
            .decode(packet, out, false)
            .map_err(|e| refused(&e))
    }

    /// Makes up audio for a packet that did not arrive, from what was
    /// decoded before it. All of `out` is filled, and its length must be
    /// that of the lost audio: a multiple of 2.5 ms, 120 ms at most.
    ///
    /// # Errors
    ///
    /// `out` has a length no packet has.
    pub fn conceal(&mut self, out: &mut [i16]) -> Result<usize, CodecError> {
        self.inner.decode(&[], out, false).map_err(|e| refused(&e))
    }

    /// Rebuilds the audio of a lost packet from the copy of it that the
    /// packet after it, `next`, carries. All of `out` is filled, and its
    /// length must be that of the lost audio. Where `next` carries no
    /// copy the audio is made up, as [`conceal`](Self::conceal) does.
    ///
    /// `next` itself still has to be decoded afterwards.
    ///
    /// # Errors
    ///
    /// `next` is not valid Opus, or `out` has a length no packet has.
    pub fn recover(&mut self, next: &[u8], out: &mut [i16]) -> Result<usize, CodecError> {
        if next.is_empty() {
            return Err(CodecError::Packet);
        }
        self.inner.decode(next, out, true).map_err(|e| refused(&e))
    }
}

impl fmt::Debug for OpusDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpusDecoder")
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

/// What an Opus encoder is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpusSettings {
    /// Bits per second.
    pub bitrate: u32,
    /// How hard the encoder works, from 0 to 10. Lower is cheaper and
    /// sounds a little worse.
    pub complexity: u8,
    /// The share of packets expected to be lost, in percent. Above 0 each
    /// packet also carries a coarse copy of the one before it.
    pub expected_loss: u8,
    /// Whether silence is sent as packets of one or two bytes, which a
    /// transport may leave out.
    pub dtx: bool,
}

impl Default for OpusSettings {
    fn default() -> Self {
        Self {
            bitrate: 32_000,
            complexity: 10,
            expected_loss: 0,
            dtx: false,
        }
    }
}

/// Encodes one Opus stream of speech.
pub struct OpusEncoder {
    inner: ::opus::Encoder,
    rate: Rate,
}

impl OpusEncoder {
    /// An encoder that takes audio at `rate`.
    ///
    /// # Errors
    ///
    /// The settings are out of range, or libopus could not set an encoder
    /// up.
    pub fn new(rate: Rate, settings: OpusSettings) -> Result<Self, CodecError> {
        let bitrate = i32::try_from(settings.bitrate).map_err(|_| CodecError::Settings)?;
        if settings.complexity > 10 || settings.expected_loss > 100 {
            return Err(CodecError::Settings);
        }
        let mut inner = ::opus::Encoder::new(rate.hz(), Channels::Mono, Application::Voip)
            .map_err(|e| refused(&e))?;
        let set = |done: ::opus::Result<()>| done.map_err(|_| CodecError::Settings);
        set(inner.set_bitrate(Bitrate::Bits(bitrate)))?;
        set(inner.set_complexity(i32::from(settings.complexity)))?;
        set(inner.set_inband_fec(settings.expected_loss > 0))?;
        set(inner.set_packet_loss_perc(i32::from(settings.expected_loss)))?;
        set(inner.set_dtx(settings.dtx))?;
        Ok(Self { inner, rate })
    }

    /// The rate of the audio it takes.
    #[must_use]
    pub const fn rate(&self) -> Rate {
        self.rate
    }

    /// Encodes `pcm` as one packet at the start of `out`, and returns the
    /// packet's length. `pcm` must be 2.5, 5, 10, 20, 40 or 60 ms long.
    ///
    /// The packet is made to fit `out`: with less room than the bitrate
    /// asks for, the encoder lowers the quality instead of failing. With
    /// [`MAX_PACKET`] bytes it never has to.
    ///
    /// # Errors
    ///
    /// `pcm` has a length no packet has, or `out` is empty.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError> {
        self.inner.encode(pcm, out).map_err(|e| refused(&e))
    }
}

impl fmt::Debug for OpusEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpusEncoder")
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_PACKET, OpusDecoder, OpusEncoder, OpusSettings};
    use crate::codec::CodecError;
    use crate::rate::Rate;
    use crate::testing::{share_at, tone};

    /// Encodes `frames` frames of a tone and returns the packets.
    fn packets(rate: Rate, settings: OpusSettings, hz: f64, frames: usize) -> Vec<Vec<u8>> {
        let mut encoder = OpusEncoder::new(rate, settings).unwrap();
        let pcm = tone(rate, hz, rate.frame() * frames);
        pcm.chunks(rate.frame())
            .map(|frame| {
                let mut packet = [0; MAX_PACKET];
                let len = encoder.encode(frame, &mut packet).unwrap();
                packet[..len].to_vec()
            })
            .collect()
    }

    #[test]
    fn a_tone_survives_encoding_and_decoding() {
        for rate in Rate::ALL {
            let sent = packets(rate, OpusSettings::default(), 440.0, 25);
            let mut decoder = OpusDecoder::new(rate).unwrap();
            let mut heard = Vec::new();
            for packet in &sent {
                assert_eq!(decoder.samples_in(packet), Ok(rate.frame()));
                let mut frame = vec![0; rate.frame()];
                assert_eq!(decoder.decode(packet, &mut frame), Ok(rate.frame()));
                heard.extend(frame);
            }
            // The codec needs a few frames to settle.
            let settled = &heard[rate.frame() * 5..];
            assert!(share_at(rate, 440.0, settled) > 0.9, "{rate:?}");
        }
    }

    #[test]
    fn audio_is_decoded_at_the_rate_asked_for_whatever_it_was_sent_at() {
        let sent = packets(Rate::Hz48000, OpusSettings::default(), 300.0, 20);
        let mut decoder = OpusDecoder::new(Rate::Hz16000).unwrap();
        let mut heard = Vec::new();
        for packet in &sent {
            let mut frame = [0; 320];
            assert_eq!(decoder.decode(packet, &mut frame), Ok(320));
            heard.extend(frame);
        }
        assert!(share_at(Rate::Hz16000, 300.0, &heard[1_600..]) > 0.9);
    }

    #[test]
    fn a_lost_packet_is_made_up_from_what_came_before() {
        let rate = Rate::Hz16000;
        let sent = packets(rate, OpusSettings::default(), 440.0, 12);
        let mut decoder = OpusDecoder::new(rate).unwrap();
        let mut frame = [0; 320];
        for packet in &sent[..10] {
            decoder.decode(packet, &mut frame).unwrap();
        }
        let mut made_up = [0; 320];
        assert_eq!(decoder.conceal(&mut made_up), Ok(320));
        assert!(
            share_at(rate, 440.0, &made_up) > 0.5,
            "it carries the tone on"
        );
        // The stream is picked up again after the gap.
        assert_eq!(decoder.decode(&sent[11], &mut frame), Ok(320));

        // No packet is 7 ms long.
        assert_eq!(decoder.conceal(&mut [0; 112]), Err(CodecError::Length));
    }

    #[test]
    fn a_lost_packet_is_rebuilt_from_the_copy_in_the_next() {
        let rate = Rate::Hz16000;
        let settings = OpusSettings {
            expected_loss: 30,
            ..OpusSettings::default()
        };
        let sent = packets(rate, settings, 440.0, 12);
        let mut decoder = OpusDecoder::new(rate).unwrap();
        let mut frame = [0; 320];
        for packet in &sent[..10] {
            decoder.decode(packet, &mut frame).unwrap();
        }
        // Packet 10 is lost; packet 11 carries a copy of it.
        let mut rebuilt = [0; 320];
        assert_eq!(decoder.recover(&sent[11], &mut rebuilt), Ok(320));
        assert!(share_at(rate, 440.0, &rebuilt) > 0.5);
        assert_eq!(decoder.decode(&sent[11], &mut frame), Ok(320));
    }

    #[test]
    fn what_is_not_opus_is_refused() {
        let mut decoder = OpusDecoder::new(Rate::Hz16000).unwrap();
        let mut frame = [0; 320];
        assert_eq!(decoder.decode(&[], &mut frame), Err(CodecError::Packet));
        assert_eq!(decoder.samples_in(&[]), Err(CodecError::Packet));
        assert_eq!(
            decoder.recover(&[], &mut frame),
            Err(CodecError::Packet),
            "an empty packet carries no copy of anything"
        );
        // A packet that says it holds frames and gives none.
        assert_eq!(
            decoder.decode(&[0x03, 0x00], &mut frame),
            Err(CodecError::Packet)
        );

        // A whole frame does not fit into half the room.
        let sent = packets(Rate::Hz16000, OpusSettings::default(), 440.0, 1);
        assert_eq!(
            decoder.decode(&sent[0], &mut frame[..160]),
            Err(CodecError::Length)
        );
    }

    #[test]
    fn an_encoder_takes_whole_frames_and_settings_in_range() {
        let rate = Rate::Hz24000;
        let mut encoder = OpusEncoder::new(rate, OpusSettings::default()).unwrap();
        let mut packet = [0; MAX_PACKET];
        let pcm = tone(rate, 440.0, rate.frame());
        let len = encoder.encode(&pcm, &mut packet).unwrap();
        // 32 kbit/s is 80 bytes in 20 ms; the encoder may vary around it.
        assert!((20..=200).contains(&len), "{len}");
        assert_eq!(
            encoder.encode(&pcm[..100], &mut packet),
            Err(CodecError::Length)
        );
        assert_eq!(encoder.encode(&pcm, &mut []), Err(CodecError::Length));
        // With little room the packet is made to fit.
        let cramped = encoder.encode(&pcm, &mut packet[..30]).unwrap();
        assert!(cramped <= 30, "{cramped}");

        for settings in [
            OpusSettings {
                complexity: 11,
                ..OpusSettings::default()
            },
            OpusSettings {
                expected_loss: 101,
                ..OpusSettings::default()
            },
            OpusSettings {
                bitrate: u32::MAX,
                ..OpusSettings::default()
            },
        ] {
            assert_eq!(
                OpusEncoder::new(rate, settings).err(),
                Some(CodecError::Settings),
                "{settings:?}"
            );
        }
    }

    #[test]
    fn silence_takes_almost_nothing_when_it_may_be_left_out() {
        let rate = Rate::Hz24000;
        let settings = OpusSettings {
            dtx: true,
            ..OpusSettings::default()
        };
        let mut encoder = OpusEncoder::new(rate, settings).unwrap();
        let silence = vec![0; rate.frame()];
        let mut packet = [0; MAX_PACKET];
        let mut last = usize::MAX;
        for _ in 0..50 {
            last = encoder.encode(&silence, &mut packet).unwrap();
        }
        assert!(last <= 2, "{last}");
    }
}
