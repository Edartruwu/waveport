//! Audio codecs: from the bytes a transport carries to samples and back.
//!
//! [`Decoder`] and [`Encoder`] put one face on the three ways audio is
//! written on the wire, and change the sample rate where the wire's is not
//! the one wanted. The codecs themselves are in [`opus`] and [`g711`].

pub mod g711;
pub mod opus;

use core::fmt;

use waveport_domain::audio::Law;
pub use waveport_domain::audio::{Codec, MAX_PLAIN_PACKET_MS};
use waveport_domain::time::MediaDur;

use self::opus::{OpusDecoder, OpusEncoder, OpusSettings};
use crate::rate::{FRAME_MS, Rate};
use crate::resample::Resampler;

/// How long audio that did not arrive is made up for, in milliseconds.
/// Past that there is nothing to go on, and silence is more honest.
pub const MAX_CONCEAL_MS: u32 = 60;

/// Why a codec refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecError {
    /// The codec has no such settings.
    Settings,
    /// The packet is not one the codec can read.
    Packet,
    /// The audio or the buffer given has a length the codec cannot work
    /// with.
    Length,
    /// The codec failed inside.
    Internal,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Settings => "the codec has no such settings",
            Self::Packet => "the packet is not one the codec can read",
            Self::Length => "the audio or the buffer has a length the codec cannot work with",
            Self::Internal => "the codec failed inside",
        })
    }
}

impl core::error::Error for CodecError {}

/// Makes up audio for packets that did not arrive, for the codecs that
/// cannot: the last frame heard, again and fading, silent after
/// [`MAX_CONCEAL_MS`].
#[derive(Clone, Debug)]
struct Repeat {
    /// The last frame decoded, or less if less was.
    last: Box<[i16]>,
    held: usize,
    /// Samples made up since the last real ones.
    made_up: usize,
}

impl Repeat {
    fn new(rate: Rate) -> Self {
        Self {
            last: vec![0; rate.frame()].into_boxed_slice(),
            held: 0,
            made_up: 0,
        }
    }

    /// Notes real audio.
    fn heard(&mut self, samples: &[i16]) {
        let kept = samples.len().min(self.last.len());
        let newest = samples.get(samples.len() - kept..).unwrap_or(&[]);
        // Keep the frame whole when less than one arrived.
        self.last.copy_within(kept.., 0);
        let from = self.last.len() - kept;
        if let Some(room) = self.last.get_mut(from..) {
            room.copy_from_slice(newest);
        }
        self.held = (self.held + kept).min(self.last.len());
        self.made_up = 0;
    }

    /// Fills `out` with made-up audio.
    fn fill(&mut self, out: &mut [i16]) {
        let fade = (self.last.len() * (MAX_CONCEAL_MS / FRAME_MS) as usize).max(1);
        let known = self.last.get(self.last.len() - self.held..).unwrap_or(&[]);
        for sample in out.iter_mut() {
            let left = fade.saturating_sub(self.made_up);
            let again = known
                .get(self.made_up % known.len().max(1))
                .copied()
                .unwrap_or(0);
            // One, as a gain, is 1 << 15.
            let gain = i32::try_from((left << 15) / fade).unwrap_or(0);
            *sample = i16::try_from((i32::from(again) * gain) >> 15).unwrap_or(0);
            self.made_up = self.made_up.saturating_add(1);
        }
    }
}

#[derive(Debug)]
enum Reads {
    Opus(OpusDecoder),
    G711(Law),
    Pcm,
}

/// Turns the packets of one stream into samples at one rate.
#[derive(Debug)]
pub struct Decoder {
    codec: Codec,
    reads: Reads,
    /// From the rate on the wire to the rate wanted. Opus needs none: it
    /// decodes to any rate.
    resampler: Resampler,
    /// One packet's samples at the rate on the wire.
    scratch: Box<[i16]>,
    repeat: Repeat,
}

impl Decoder {
    /// A decoder of `codec` that gives audio at `rate`.
    ///
    /// # Errors
    ///
    /// The codec could not be set up.
    pub fn new(codec: Codec, rate: Rate) -> Result<Self, CodecError> {
        let (reads, wire) = match codec {
            Codec::Opus => (Reads::Opus(OpusDecoder::new(rate)?), rate),
            Codec::G711(law) => (Reads::G711(law), Rate::Hz8000),
            Codec::Pcm(wire) => (Reads::Pcm, wire),
        };
        Ok(Self {
            codec,
            reads,
            resampler: Resampler::new(wire, rate),
            scratch: vec![0; wire.samples_in_millis(MAX_PLAIN_PACKET_MS)].into_boxed_slice(),
            repeat: Repeat::new(wire),
        })
    }

    /// What it decodes.
    #[must_use]
    pub const fn codec(&self) -> Codec {
        self.codec
    }

    /// The rate of the audio it gives.
    #[must_use]
    pub const fn rate(&self) -> Rate {
        self.resampler.to()
    }

    /// The most samples one packet can decode to.
    #[must_use]
    pub const fn max_samples(&self) -> usize {
        let millis = match self.codec {
            Codec::Opus => opus::MAX_PACKET_MS,
            Codec::G711(_) | Codec::Pcm(_) => MAX_PLAIN_PACKET_MS,
        };
        // One more for where a change of rate rounds up.
        self.rate().samples_in_millis(millis) + 1
    }

    /// How long the audio in `packet` is.
    ///
    /// # Errors
    ///
    /// The packet is not one the codec can read.
    pub fn length_of(&self, packet: &[u8]) -> Result<MediaDur, CodecError> {
        let wire = self.resampler.from();
        let samples = match &self.reads {
            Reads::Opus(decoder) => decoder.samples_in(packet)?,
            Reads::G711(_) => packet.len(),
            Reads::Pcm if packet.len().is_multiple_of(2) => packet.len() / 2,
            Reads::Pcm => return Err(CodecError::Packet),
        };
        if samples == 0 {
            return Err(CodecError::Packet);
        }
        Ok(wire.length_of(samples))
    }

    /// Decodes `packet` into the start of `out`, and returns how many
    /// samples it wrote. [`max_samples`](Self::max_samples) of room is
    /// always enough.
    ///
    /// # Errors
    ///
    /// The packet is not one the codec can read, is longer than a packet
    /// may be, or `out` is too short for it.
    pub fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize, CodecError> {
        let samples = match &mut self.reads {
            Reads::Opus(decoder) => return decoder.decode(packet, out),
            Reads::G711(law) => {
                let room = self
                    .scratch
                    .get_mut(..packet.len())
                    .ok_or(CodecError::Length)?;
                g711::decode(*law, packet, room)
            }
            Reads::Pcm => {
                let (pairs, odd) = packet.as_chunks::<2>();
                if !odd.is_empty() {
                    return Err(CodecError::Packet);
                }
                let room = self
                    .scratch
                    .get_mut(..pairs.len())
                    .ok_or(CodecError::Length)?;
                for (sample, pair) in room.iter_mut().zip(pairs) {
                    *sample = i16::from_le_bytes(*pair);
                }
                room.len()
            }
        };
        if samples == 0 {
            return Err(CodecError::Packet);
        }
        let decoded = self.scratch.get(..samples).unwrap_or(&[]);
        if self.resampler.yields(samples) > out.len() {
            return Err(CodecError::Length);
        }
        self.repeat.heard(decoded);
        self.resampler
            .process(decoded, out)
            .map_err(|_| CodecError::Length)
    }

    /// Makes up `length` of audio for a packet that did not arrive, into
    /// the start of `out`, and returns how many samples it wrote. Audio
    /// made up for longer than [`MAX_CONCEAL_MS`] in a row fades to
    /// silence.
    ///
    /// # Errors
    ///
    /// `length` is not one a packet can have, or `out` is too short.
    pub fn conceal(&mut self, length: MediaDur, out: &mut [i16]) -> Result<usize, CodecError> {
        self.fill_gap(None, length, out)
    }

    /// Rebuilds `length` of audio for a packet that did not arrive from
    /// the packet after it, `next`, into the start of `out`. Opus packets
    /// may carry a coarse copy of the one before; where there is none, and
    /// for the other codecs, the audio is made up as
    /// [`conceal`](Self::conceal) does.
    ///
    /// `next` itself still has to be decoded afterwards.
    ///
    /// # Errors
    ///
    /// `next` is not one the codec can read, `length` is not one a packet
    /// can have, or `out` is too short.
    pub fn recover(
        &mut self,
        next: &[u8],
        length: MediaDur,
        out: &mut [i16],
    ) -> Result<usize, CodecError> {
        self.fill_gap(Some(next), length, out)
    }

    fn fill_gap(
        &mut self,
        next: Option<&[u8]>,
        length: MediaDur,
        out: &mut [i16],
    ) -> Result<usize, CodecError> {
        let wire = self.resampler.from();
        let samples = wire.samples_in(length);
        if samples == 0 {
            return Err(CodecError::Length);
        }
        if let Reads::Opus(decoder) = &mut self.reads {
            let room = out.get_mut(..samples).ok_or(CodecError::Length)?;
            return match next {
                Some(next) => decoder.recover(next, room),
                None => decoder.conceal(room),
            };
        }
        let room = self.scratch.get_mut(..samples).ok_or(CodecError::Length)?;
        if self.resampler.yields(samples) > out.len() {
            return Err(CodecError::Length);
        }
        self.repeat.fill(room);
        self.resampler
            .process(room, out)
            .map_err(|_| CodecError::Length)
    }
}

#[derive(Debug)]
enum Writes {
    Opus(OpusEncoder),
    G711(Law),
    Pcm,
}

/// Turns frames of samples at one rate into the packets of one stream.
#[derive(Debug)]
pub struct Encoder {
    codec: Codec,
    writes: Writes,
    /// From the rate given to the rate on the wire. Opus needs none: it
    /// encodes from any rate.
    resampler: Resampler,
    /// One frame's samples at the rate on the wire.
    scratch: Box<[i16]>,
}

impl Encoder {
    /// An encoder to `codec` of audio at `rate`. `opus` is used when the
    /// codec is Opus.
    ///
    /// # Errors
    ///
    /// The settings are out of range, or the codec could not be set up.
    pub fn new(codec: Codec, rate: Rate, opus: OpusSettings) -> Result<Self, CodecError> {
        let (writes, wire) = match codec {
            Codec::Opus => (Writes::Opus(OpusEncoder::new(rate, opus)?), rate),
            Codec::G711(law) => (Writes::G711(law), Rate::Hz8000),
            Codec::Pcm(wire) => (Writes::Pcm, wire),
        };
        Ok(Self {
            codec,
            writes,
            resampler: Resampler::new(rate, wire),
            scratch: vec![0; wire.samples_in_millis(MAX_PLAIN_PACKET_MS) + 1].into_boxed_slice(),
        })
    }

    /// What it encodes to.
    #[must_use]
    pub const fn codec(&self) -> Codec {
        self.codec
    }

    /// The rate of the audio it takes.
    #[must_use]
    pub const fn rate(&self) -> Rate {
        self.resampler.from()
    }

    /// Expects this share of packets to be lost from here on, in
    /// percent ([`OpusEncoder::expect_loss`]). A codec that has nothing
    /// to do about loss takes it and does nothing.
    ///
    /// # Errors
    ///
    /// It is more than all of them.
    pub fn expect_loss(&mut self, percent: u8) -> Result<(), CodecError> {
        match &mut self.writes {
            Writes::Opus(encoder) => encoder.expect_loss(percent),
            Writes::G711(_) | Writes::Pcm if percent > 100 => Err(CodecError::Settings),
            Writes::G711(_) | Writes::Pcm => Ok(()),
        }
    }

    /// The length of what a packet's timestamp counts, for a frame of
    /// `samples` samples given to [`encode`](Self::encode): how far the
    /// timestamp of the next packet is from this one's, in units of the
    /// codec's [clock](Codec::clock).
    #[must_use]
    pub fn timestamp_step(&self, samples: usize) -> u32 {
        let length = self.rate().length_of(samples);
        u32::try_from(self.codec.clock().samples_in(length)).unwrap_or(u32::MAX)
    }

    /// Encodes `pcm`, one frame, as one packet at the start of `out`, and
    /// returns the packet's length. For Opus the frame must be 2.5, 5,
    /// 10, 20, 40 or 60 ms long. [`Codec::max_packet`] bytes of `out` are
    /// always enough.
    ///
    /// # Errors
    ///
    /// The frame has a length the codec cannot take, or `out` is too
    /// short for the packet.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError> {
        if let Writes::Opus(encoder) = &mut self.writes {
            return encoder.encode(pcm, out);
        }
        let samples = self.resampler.yields(pcm.len());
        let bytes = match self.writes {
            Writes::Pcm => samples * 2,
            Writes::Opus(_) | Writes::G711(_) => samples,
        };
        let room = self.scratch.get_mut(..samples).ok_or(CodecError::Length)?;
        let packet = out.get_mut(..bytes).ok_or(CodecError::Length)?;
        self.resampler
            .process(pcm, room)
            .map_err(|_| CodecError::Length)?;
        match self.writes {
            Writes::G711(law) => {
                g711::encode(law, room, packet);
            }
            Writes::Pcm => {
                for (pair, sample) in packet.as_chunks_mut::<2>().0.iter_mut().zip(room.iter()) {
                    *pair = sample.to_le_bytes();
                }
            }
            Writes::Opus(_) => {}
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use waveport_domain::time::MediaDur;

    use super::g711::Law;
    use super::opus::OpusSettings;
    use super::{Codec, CodecError, Decoder, Encoder};
    use crate::rate::Rate;
    use crate::testing::{energy, share_at, tone};

    const CODECS: [Codec; 5] = [
        Codec::Opus,
        Codec::G711(Law::Mu),
        Codec::G711(Law::A),
        Codec::Pcm(Rate::Hz16000),
        Codec::Pcm(Rate::Hz48000),
    ];

    const FRAME: MediaDur = MediaDur::from_millis(20);

    /// Encodes `frames` frames of a tone given at `rate`.
    fn packets(codec: Codec, rate: Rate, hz: f64, frames: usize) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new(codec, rate, OpusSettings::default()).unwrap();
        tone(rate, hz, rate.frame() * frames)
            .chunks(rate.frame())
            .map(|frame| {
                let mut packet = vec![0; codec.max_packet()];
                let len = encoder.encode(frame, &mut packet).unwrap();
                packet.truncate(len);
                packet
            })
            .collect()
    }

    #[test]
    fn speech_synthesis_to_the_wire_to_speech_recognition() {
        // Given at 24 kHz, wanted at 16 kHz, whatever is in between.
        for codec in CODECS {
            let sent = packets(codec, Rate::Hz24000, 440.0, 30);
            let mut decoder = Decoder::new(codec, Rate::Hz16000).unwrap();
            assert_eq!((decoder.codec(), decoder.rate()), (codec, Rate::Hz16000));
            let mut heard = Vec::new();
            for packet in &sent {
                assert_eq!(decoder.length_of(packet), Ok(FRAME), "{codec:?}");
                let mut frame = vec![0; decoder.max_samples()];
                let written = decoder.decode(packet, &mut frame).unwrap();
                assert_eq!(written, 320, "{codec:?}");
                heard.extend(&frame[..written]);
            }
            assert!(
                share_at(Rate::Hz16000, 440.0, &heard[3_200..]) > 0.95,
                "{codec:?}"
            );
        }
    }

    #[test]
    fn each_codec_has_its_packet_size_and_its_clock() {
        let sizes = CODECS.map(|codec| packets(codec, Rate::Hz24000, 440.0, 1)[0].len());
        assert!((20..=200).contains(&sizes[0]), "{sizes:?}");
        assert_eq!(sizes[1..], [160, 160, 640, 1_920]);

        let steps = CODECS.map(|codec| {
            let encoder = Encoder::new(codec, Rate::Hz24000, OpusSettings::default()).unwrap();
            assert_eq!((encoder.codec(), encoder.rate()), (codec, Rate::Hz24000));
            encoder.timestamp_step(480)
        });
        assert_eq!(steps, [960, 160, 160, 320, 960]);
        assert_eq!(
            CODECS.map(Codec::max_packet),
            [1_275, 480, 480, 1_920, 5_760]
        );
    }

    #[test]
    fn a_gap_is_filled_with_what_was_heard_and_fades_out() {
        for codec in CODECS {
            let sent = packets(codec, Rate::Hz16000, 440.0, 12);
            let mut decoder = Decoder::new(codec, Rate::Hz16000).unwrap();
            let mut frame = vec![0; decoder.max_samples()];
            for packet in &sent[..10] {
                decoder.decode(packet, &mut frame).unwrap();
            }
            let before = energy(&frame[..320]);

            let mut levels = Vec::new();
            for _ in 0..6 {
                assert_eq!(decoder.conceal(FRAME, &mut frame), Ok(320), "{codec:?}");
                levels.push(energy(&frame[..320]) / before);
            }
            assert!(
                levels[0] > 0.25,
                "{codec:?}: the first sounds on: {levels:?}"
            );
            assert!(
                levels[5] < 0.01,
                "{codec:?}: by 120 ms it is quiet: {levels:?}"
            );
            // The stream is picked up again.
            assert_eq!(decoder.decode(&sent[11], &mut frame), Ok(320), "{codec:?}");
        }
    }

    #[test]
    fn a_gap_is_filled_from_the_next_packet_where_that_carries_a_copy() {
        for codec in CODECS {
            let sent = packets(codec, Rate::Hz16000, 440.0, 12);
            let mut decoder = Decoder::new(codec, Rate::Hz16000).unwrap();
            let mut frame = vec![0; decoder.max_samples()];
            for packet in &sent[..10] {
                decoder.decode(packet, &mut frame).unwrap();
            }
            assert_eq!(
                decoder.recover(&sent[11], FRAME, &mut frame),
                Ok(320),
                "{codec:?}"
            );
            assert!(
                share_at(Rate::Hz16000, 440.0, &frame[..320]) > 0.5,
                "{codec:?}"
            );
            assert_eq!(decoder.decode(&sent[11], &mut frame), Ok(320), "{codec:?}");
        }
    }

    #[test]
    fn nothing_heard_yet_is_concealed_with_silence() {
        for codec in CODECS {
            let mut decoder = Decoder::new(codec, Rate::Hz16000).unwrap();
            let mut frame = vec![1; decoder.max_samples()];
            assert_eq!(decoder.conceal(FRAME, &mut frame), Ok(320), "{codec:?}");
            assert!(frame[..320].iter().all(|&sample| sample == 0), "{codec:?}");
        }
    }

    #[test]
    fn what_cannot_be_read_or_does_not_fit_is_refused() {
        for codec in CODECS {
            let mut decoder = Decoder::new(codec, Rate::Hz16000).unwrap();
            let mut frame = vec![0; decoder.max_samples()];
            assert_eq!(decoder.length_of(&[]), Err(CodecError::Packet), "{codec:?}");
            assert_eq!(
                decoder.decode(&[], &mut frame),
                Err(CodecError::Packet),
                "{codec:?}"
            );
            // A good packet, and no room for it.
            let sent = packets(codec, Rate::Hz16000, 440.0, 1);
            assert_eq!(
                decoder.decode(&sent[0], &mut frame[..100]),
                Err(CodecError::Length),
                "{codec:?}"
            );
            assert_eq!(
                decoder.conceal(FRAME, &mut frame[..100]),
                Err(CodecError::Length),
                "{codec:?}"
            );
            assert_eq!(
                decoder.conceal(MediaDur::ZERO, &mut frame),
                Err(CodecError::Length),
                "{codec:?}"
            );
            // The refusals took nothing: the packet still decodes whole.
            assert_eq!(decoder.decode(&sent[0], &mut frame), Ok(320), "{codec:?}");
        }

        // Half a sample.
        let mut decoder = Decoder::new(Codec::Pcm(Rate::Hz16000), Rate::Hz16000).unwrap();
        let mut frame = vec![0; decoder.max_samples()];
        assert_eq!(decoder.length_of(&[1, 2, 3]), Err(CodecError::Packet));
        assert_eq!(
            decoder.decode(&[1, 2, 3], &mut frame),
            Err(CodecError::Packet)
        );
        // More than a packet may hold.
        let long = vec![0; Codec::G711(Law::Mu).max_packet() + 1];
        let mut decoder = Decoder::new(Codec::G711(Law::Mu), Rate::Hz16000).unwrap();
        assert_eq!(decoder.decode(&long, &mut frame), Err(CodecError::Length));
    }

    #[test]
    fn an_encoder_refuses_what_does_not_fit() {
        for codec in CODECS {
            let mut encoder = Encoder::new(codec, Rate::Hz24000, OpusSettings::default()).unwrap();
            let frame = tone(Rate::Hz24000, 440.0, 480);
            assert_eq!(
                encoder.encode(&frame, &mut []),
                Err(CodecError::Length),
                "{codec:?}"
            );
            let mut packet = vec![0; codec.max_packet()];
            // Two seconds are no frame.
            let long = vec![0; 48_000];
            assert_eq!(
                encoder.encode(&long, &mut packet),
                Err(CodecError::Length),
                "{codec:?}"
            );
            assert!(encoder.encode(&frame, &mut packet).is_ok(), "{codec:?}");
        }
        let bad = OpusSettings {
            complexity: 99,
            ..OpusSettings::default()
        };
        assert_eq!(
            Encoder::new(Codec::Opus, Rate::Hz24000, bad).err(),
            Some(CodecError::Settings)
        );
    }

    #[test]
    fn plain_samples_cross_the_wire_unchanged() {
        let rate = Rate::Hz16000;
        let codec = Codec::Pcm(rate);
        let frame = tone(rate, 440.0, 320);
        let mut encoder = Encoder::new(codec, rate, OpusSettings::default()).unwrap();
        let mut packet = vec![0; codec.max_packet()];
        let len = encoder.encode(&frame, &mut packet).unwrap();
        assert_eq!(len, 640);
        assert_eq!(packet[..2], frame[0].to_le_bytes());

        let mut decoder = Decoder::new(codec, rate).unwrap();
        let mut back = vec![0; decoder.max_samples()];
        assert_eq!(decoder.decode(&packet[..len], &mut back), Ok(320));
        assert_eq!(back[..320], frame[..]);
    }
}
