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
pub const MAX_PACKET: usize = waveport_domain::audio::MAX_OPUS_PACKET;

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

/// What an encoder is to expect of a line, from what the far end says
/// it lost.
///
/// A far end says every second or so how much of what it was sent
/// lately did not arrive. An encoder that is told to expect loss spends
/// part of every packet on a copy of the one before, so it is told only
/// when it is worth it, and not at every change:
///
/// - It expects the worst of the last four things it was told, so it is
///   ready at the first sign of loss and stays ready until a line has
///   been well for a while.
/// - Less than two in a hundred is expected as none: a packet now and
///   then is made up for well enough without a copy.
/// - What it expects is rounded up to five in a hundred, and is no more
///   than thirty, past which an encoder does nothing more.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LossWatch {
    /// What the far end said last, the newest last.
    said: [u8; 4],
    expects: u8,
}

impl LossWatch {
    /// Below this, in percent, no loss is expected.
    const FROM: u8 = 2;
    /// What is expected is a multiple of this.
    const STEP: u8 = 5;
    /// No more than this is expected.
    const MOST: u8 = 30;

    /// What an encoder is to expect now, in percent.
    #[must_use]
    pub const fn expects(&self) -> u8 {
        self.expects
    }

    /// The far end says that `percent` of what it was sent lately did
    /// not arrive. Returns what an encoder is to expect from here on,
    /// when that is not what it expected before.
    pub fn told(&mut self, percent: u8) -> Option<u8> {
        self.said.rotate_left(1);
        if let Some(newest) = self.said.last_mut() {
            *newest = percent.min(100);
        }
        let worst = self.said.iter().copied().max().unwrap_or(0);
        let expects = if worst < Self::FROM {
            0
        } else {
            worst
                .div_ceil(Self::STEP)
                .saturating_mul(Self::STEP)
                .min(Self::MOST)
        };
        if expects == self.expects {
            return None;
        }
        self.expects = expects;
        Some(expects)
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

    /// Expects this share of packets to be lost from here on, in
    /// percent, as [`OpusSettings::expected_loss`] says for an encoder
    /// that is new: above 0 each packet carries a coarse copy of the one
    /// before it, at the cost of some of its own quality.
    ///
    /// # Errors
    ///
    /// It is more than all of them.
    pub fn expect_loss(&mut self, percent: u8) -> Result<(), CodecError> {
        if percent > 100 {
            return Err(CodecError::Settings);
        }
        let set = |done: ::opus::Result<()>| done.map_err(|_| CodecError::Settings);
        set(self.inner.set_inband_fec(percent > 0))?;
        set(self.inner.set_packet_loss_perc(i32::from(percent)))
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
    use super::{LossWatch, MAX_PACKET, OpusDecoder, OpusEncoder, OpusSettings};
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
    fn an_encoder_that_is_told_of_loss_begins_to_send_copies_and_stops_when_told() {
        let rate = Rate::Hz16000;
        let mut encoder = OpusEncoder::new(rate, OpusSettings::default()).unwrap();
        // A low tone and a high one, turn and turn about: what a frame
        // holds cannot be guessed from the one before it.
        let pitch = |frame: usize| {
            if frame.is_multiple_of(2) {
                440.0
            } else {
                1_320.0
            }
        };
        let mut sent = Vec::new();
        for at in 0..60 {
            // A line that is well, one that loses a fifth, and one that
            // is well again.
            match at {
                20 => encoder.expect_loss(20).unwrap(),
                40 => encoder.expect_loss(0).unwrap(),
                _ => {}
            }
            let mut packet = [0; MAX_PACKET];
            let pcm = tone(rate, pitch(at), rate.frame());
            let len = encoder.encode(&pcm, &mut packet).unwrap();
            sent.push(packet[..len].to_vec());
        }
        assert_eq!(encoder.expect_loss(101), Err(CodecError::Settings));

        // How much of what is rebuilt of a lost packet, from the one
        // after it, is the tone the lost one held.
        let rebuilt = |lost: usize| {
            let mut decoder = OpusDecoder::new(rate).unwrap();
            let mut frame = [0; 320];
            for packet in &sent[..lost] {
                decoder.decode(packet, &mut frame).unwrap();
            }
            let mut rebuilt = [0; 320];
            assert_eq!(decoder.recover(&sent[lost + 1], &mut rebuilt), Ok(320));
            share_at(rate, pitch(lost), &rebuilt)
        };
        // With a copy in the next packet much of it is that tone, for a
        // copy is coarse. Without one a decoder makes something up from
        // what it heard before, which was the other tone.
        let (well, lossy, well_again) = (rebuilt(11), rebuilt(31), rebuilt(51));
        assert!(lossy > 0.3, "{well} {lossy} {well_again}");
        assert!(
            well < 0.05 && well_again < 0.05,
            "{well} {lossy} {well_again}"
        );
    }

    #[test]
    fn what_an_encoder_expects_follows_what_the_far_end_says_it_lost() {
        let mut watch = LossWatch::default();
        assert_eq!(watch.expects(), 0);
        // A line that is well, and one that loses a packet now and then.
        assert_eq!([0, 0, 1, 0, 1].map(|said| watch.told(said)), [None; 5]);
        // The first sign of loss is acted on, rounded up.
        assert_eq!(watch.told(2), Some(5));
        assert_eq!(watch.told(7), Some(10));
        assert_eq!(watch.told(10), None);
        assert_eq!(watch.told(11), Some(15));
        // It is expected for as long as the worst of the last four says.
        assert_eq!([0, 0, 0].map(|said| watch.told(said)), [None; 3]);
        assert_eq!(watch.expects(), 15);
        assert_eq!(watch.told(0), Some(0));
        assert_eq!(watch.told(0), None);
        // No more is expected than an encoder does anything about.
        assert_eq!(watch.told(64), Some(30));
        assert_eq!(watch.told(255), None);
        assert_eq!(watch.expects(), 30);
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
