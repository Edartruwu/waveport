//! Workloads for the media path: one frame each, from a prepared state.
//!
//! Every 20 ms a session takes one packet in and, while its agent speaks,
//! hands one frame out. These are the two things to measure, for each way
//! audio is written on the wire. Setting the path up and warming it is
//! not part of the measurement.

use waveport_domain::ids::{Epoch, GenId, OpTag};
use waveport_domain::time::{MediaDur, MonoTime};
use waveport_media::codec::g711::Law;
use waveport_media::codec::opus::OpusSettings;
use waveport_media::codec::{Codec, Encoder};
use waveport_media::ingress::{Ingress, IngressConfig, Packet};
use waveport_media::ledger::{Mark, Spread};
use waveport_media::playout::{Playout, PlayoutConfig, Position};
use waveport_media::rate::Rate;

/// The rate speech synthesis gives.
const SPOKEN: Rate = Rate::Hz24000;

const RESPONSE: OpTag = OpTag {
    generation: GenId(1),
    epoch: Epoch(0),
};

/// A way audio is written on the wire, as a benchmark names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Wire {
    /// Opus, as a browser sends it.
    Opus,
    /// G.711, as a telephone carrier sends it. The rate is changed from
    /// and to 8 kHz.
    G711,
    /// Plain samples at 48 kHz, as a sound card gives them. The rate is
    /// changed from and to 48 kHz.
    Pcm48,
    /// Plain samples at the rate wanted: the path with no codec in it.
    Plain,
}

impl Wire {
    /// Every one.
    pub const ALL: [Self; 4] = [Self::Opus, Self::G711, Self::Pcm48, Self::Plain];

    /// The codec, for audio that is `plain` when nothing is to be done to
    /// it.
    const fn codec(self, plain: Rate) -> Codec {
        match self {
            Self::Opus => Codec::Opus,
            Self::G711 => Codec::G711(Law::Mu),
            Self::Pcm48 => Codec::Pcm(Rate::Hz48000),
            Self::Plain => Codec::Pcm(plain),
        }
    }
}

/// Something that sounds a little like a voice: a few tones that drift.
fn voice(rate: Rate, samples: usize) -> Vec<i16> {
    let mut phase: [u32; 3] = [0, 0, 0];
    (0..samples)
        .map(|n| {
            let mut sum = 0_i32;
            for (phase, hz) in phase.iter_mut().zip([140_u32, 690, 2_300]) {
                // A triangle wave, from whole numbers alone.
                let drift = u32::try_from(n / 400 % 20).unwrap_or(0);
                *phase = phase.wrapping_add((hz + drift) * (u32::MAX / rate.hz()));
                let ramp = i32::try_from(*phase >> 17).unwrap_or(0) - 16_384;
                sum += ramp.abs() - 8_192;
            }
            i16::try_from(sum / 3).unwrap_or(0)
        })
        .collect()
}

/// An inbound path in the middle of a stream.
#[derive(Debug)]
pub struct Inbound {
    ingress: Ingress,
    /// The packets of the stream, 20 ms each.
    packets: Vec<Vec<u8>>,
    /// Timestamp units in a packet.
    step: u32,
    /// The next packet to arrive.
    next: usize,
}

impl Inbound {
    /// A path that has taken the first ten packets of a stream of `wire`.
    ///
    /// # Panics
    ///
    /// The codec cannot be set up: there is nothing to measure then.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a benchmark that cannot be set up has nothing to measure"
    )]
    pub fn prepare(wire: Wire) -> Box<Self> {
        let config = IngressConfig::new(wire.codec(Rate::Hz16000));
        let rate = Rate::Hz48000;
        let mut encoder = Encoder::new(config.codec, rate, OpusSettings::default())
            .expect("the encoder for the stream");
        let step = encoder.timestamp_step(rate.frame());
        let packets = voice(rate, rate.frame() * 16)
            .chunks(rate.frame())
            .map(|frame| {
                let mut packet = vec![0; config.codec.max_packet()];
                let len = encoder.encode(frame, &mut packet).expect("a packet");
                packet.truncate(len);
                packet
            })
            .collect();
        let mut inbound = Box::new(Self {
            ingress: Ingress::new(config).expect("the inbound path"),
            packets,
            step,
            next: 0,
        });
        for _ in 0..10 {
            inbound.packet();
        }
        inbound
    }

    /// The next packet of the stream arrives and is handed on. Returns
    /// how many samples were handed on.
    pub fn packet(&mut self) -> usize {
        let n = self.next;
        self.next += 1;
        let Some(payload) = self.packets.get(n) else {
            return 0;
        };
        let number = u32::try_from(n).unwrap_or(0);
        let packet = Packet {
            sequence: u16::try_from(n).unwrap_or(0),
            timestamp: number.wrapping_mul(self.step),
            payload,
        };
        let now = MonoTime::from_millis(20 * u64::from(number));
        self.ingress.push(now, packet);
        let mut samples = 0;
        self.ingress.poll(now, |heard| samples += heard.pcm.len());
        samples
    }

    /// The next packet of the stream is lost, and the one after it
    /// arrives: the lost one is made up and both are handed on. Returns
    /// how many samples were handed on.
    pub fn packet_after_a_loss(&mut self) -> usize {
        self.next += 1;
        self.packet()
    }
}

/// An outbound path in the middle of a response.
#[derive(Debug)]
pub struct Outbound {
    playout: Playout,
    encoder: Encoder,
    frame: Vec<i16>,
    packet: Vec<u8>,
    /// The time of the next call, in milliseconds.
    now: u64,
}

impl Outbound {
    /// A path that holds three seconds of a response and has handed over
    /// its first half second, to be sent as `wire`.
    ///
    /// # Panics
    ///
    /// The codec cannot be set up: there is nothing to measure then.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a benchmark that cannot be set up has nothing to measure"
    )]
    pub fn prepare(wire: Wire) -> Box<Self> {
        let codec = wire.codec(SPOKEN);
        let mut playout = Playout::new(PlayoutConfig::new(SPOKEN));
        let start = MonoTime::from_millis(0);
        playout.start(RESPONSE, start).expect("a new response");
        let speech = voice(SPOKEN, SPOKEN.frame() * 150);
        playout
            .push(RESPONSE, start, &speech)
            .expect("room for the response");
        // A word every 300 ms.
        for word in 1..=10 {
            let mark = Mark {
                text: word * 6,
                audio: MediaDur::from_millis(word * 300),
                spread: Spread::AtOnce,
            };
            playout.mark(RESPONSE, mark).expect("room for a mark");
        }
        let mut outbound = Box::new(Self {
            playout,
            encoder: Encoder::new(codec, SPOKEN, OpusSettings::default())
                .expect("the encoder for the wire"),
            frame: vec![0; SPOKEN.frame()],
            packet: vec![0; codec.max_packet()],
            now: 0,
        });
        while outbound.now < 500 {
            outbound.frame();
        }
        outbound
    }

    /// The next frame is due: it is handed over and encoded. Returns the
    /// length of its packet.
    pub fn frame(&mut self) -> usize {
        let now = MonoTime::from_millis(self.now);
        self.now += 20;
        let mut bytes = 0;
        while self.playout.poll(now, &mut self.frame).is_some() {
            bytes = self
                .encoder
                .encode(&self.frame, &mut self.packet)
                .unwrap_or(0);
        }
        bytes
    }

    /// Someone starts to speak over the agent, and the next frame is
    /// due: it is handed over on its way down to the lower volume.
    pub fn frame_lowered(&mut self) -> usize {
        self.playout.duck(RESPONSE);
        self.frame()
    }

    /// Says how far playback has got.
    pub fn position(&mut self) -> Option<Position> {
        self.playout.position(MonoTime::from_millis(self.now))
    }
}

#[cfg(test)]
mod tests {
    use super::{Inbound, Outbound, Wire};

    #[test]
    fn a_packet_in_is_20_ms_handed_on() {
        for wire in Wire::ALL {
            let mut inbound = Inbound::prepare(wire);
            assert_eq!(inbound.packet(), 320, "{wire:?}");
            // The lost one and the one that arrived.
            assert_eq!(inbound.packet_after_a_loss(), 640, "{wire:?}");
        }
    }

    #[test]
    fn a_frame_out_is_one_packet() {
        let sizes = Wire::ALL.map(|wire| {
            let mut outbound = Outbound::prepare(wire);
            let first = outbound.frame();
            let lowered = outbound.frame_lowered();
            assert!(lowered > 0, "{wire:?}");
            first
        });
        assert!((20..=300).contains(&sizes[0]), "{sizes:?}");
        assert_eq!(sizes[1..], [160, 1_920, 960]);
    }

    #[test]
    fn playback_knows_where_it_is() {
        let mut outbound = Outbound::prepare(Wire::Plain);
        let position = outbound.position().unwrap();
        // Half a second in: one word has been heard in full.
        assert_eq!(position.played.as_millis(), 500);
        assert_eq!(position.heard_text, 6);
        assert!(!position.drained);
    }
}
