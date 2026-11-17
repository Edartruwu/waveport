//! What the transports share that carry audio over a connection of
//! messages: a WebSocket to a browser, or a carrier's media stream.
//!
//! The far end of such a connection is sent the audio of a response in
//! pieces, and says in its own words how much of it has been played.
//! [`Runs`] keeps the responses it was sent under the numbers it knows
//! them by.
//!
//! What the far end sends is audio that is cut wherever its messages
//! end. [`Inbound`] cuts it into the packets a session takes. A far end
//! that sends Opus sends a packet a message, and [`OpusInbound`] numbers
//! and places those.

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};
use tokio::time::Instant;
use waveport_domain::ids::OpTag;
use waveport_ports::audio::Rate;

/// How many responses are remembered, to know which one a far end says
/// it has played.
pub const MOST_RUNS: usize = 64;

/// A response on its way to the far end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// What the session knows it by.
    pub tag: OpTag,
    /// The number the far end knows it by.
    pub number: u32,
    /// How many of its samples were sent.
    pub sent: u32,
}

/// The responses the far end was sent, the newest last.
#[derive(Debug, Default)]
pub struct Runs {
    known: VecDeque<Run>,
    /// The number of the next response.
    next: u32,
    /// The number of the response whose audio the far end was last told
    /// follows.
    open: Option<u32>,
}

impl Runs {
    /// The response `tag`, which is a new one if it is not known. The
    /// oldest is forgotten when there are [`MOST_RUNS`].
    pub fn of(&mut self, tag: OpTag) -> &mut Run {
        let found = self.known.iter().position(|run| run.tag == tag);
        let at = found.unwrap_or_else(|| {
            if self.known.len() >= MOST_RUNS {
                self.known.pop_front();
            }
            self.known.push_back(Run {
                tag,
                number: self.next,
                sent: 0,
            });
            self.next = self.next.wrapping_add(1);
            self.known.len() - 1
        });
        // The place is one that was just found or made.
        #[expect(clippy::indexing_slicing, reason = "the index is of this queue")]
        &mut self.known[at]
    }

    /// The response the far end knows as `number`, if it is remembered.
    #[must_use]
    pub fn numbered(&self, number: u32) -> Option<Run> {
        self.known.iter().copied().find(|run| run.number == number)
    }

    /// The response `tag`, if anything of it was sent and it is
    /// remembered.
    #[must_use]
    pub fn tagged(&self, tag: OpTag) -> Option<Run> {
        self.known.iter().copied().find(|run| run.tag == tag)
    }

    /// Whether the far end has to be told that the audio that follows is
    /// of the response `number`: it was last told of another, or of
    /// none. From here on it has been told.
    pub fn begins(&mut self, number: u32) -> bool {
        if self.open == Some(number) {
            return false;
        }
        self.open = Some(number);
        true
    }

    /// The audio of the response `number` follows no longer: its last
    /// piece was sent, or it was discarded.
    pub fn ends(&mut self, number: u32) {
        if self.open == Some(number) {
            self.open = None;
        }
    }

    /// The far end is one that holds nothing of what was sent, as one
    /// that came back on another connection: the audio that follows has
    /// to say whose it is, whatever was said last.
    pub const fn begin_anew(&mut self) {
        self.open = None;
    }

    /// How many responses are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.known.len()
    }

    /// Whether no response is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }
}

/// A packet of a far end's audio, as a session takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Its number, which wraps round.
    pub sequence: u16,
    /// The place of its first sample in the stream, which wraps round.
    pub timestamp: u32,
    /// Its audio, 20 ms of it.
    pub payload: Bytes,
}

/// A far end's audio, cut into the packets a session takes: 20 ms each,
/// numbered, and each with the place of its first sample in the stream.
#[derive(Debug)]
pub struct Inbound {
    rate: Rate,
    /// How many bytes a sample is.
    width: usize,
    /// What has arrived and does not fill a packet yet.
    held: BytesMut,
    sequence: u16,
    /// How many samples came before the next packet.
    position: u64,
    /// When the first audio arrived.
    began: Option<Instant>,
}

impl Inbound {
    /// For samples as they are: 16 bits each, at `rate`.
    #[must_use]
    pub fn pcm(rate: Rate) -> Self {
        Self::of(rate, 2)
    }

    /// For G.711, which is a byte a sample at 8 kHz, in either law.
    #[must_use]
    pub fn g711() -> Self {
        Self::of(Rate::Hz8000, 1)
    }

    fn of(rate: Rate, width: usize) -> Self {
        Self {
            rate,
            width,
            held: BytesMut::new(),
            sequence: 0,
            position: 0,
            began: None,
        }
    }

    /// How many bytes a packet holds.
    const fn packet(&self) -> usize {
        self.rate.frame() * self.width
    }

    /// Takes what a message holds, which arrived at `now`.
    ///
    /// A far end that stopped sending for more than a second is heard
    /// from when it goes on, and not from where the count of its samples
    /// would have it.
    ///
    /// # Errors
    ///
    /// The message is not whole samples, or holds more than a second.
    pub fn take(&mut self, audio: &[u8], now: Instant) -> Result<(), &'static str> {
        let a_second = u64::from(self.rate.hz());
        if !audio.len().is_multiple_of(self.width) {
            return Err("a message of audio has to hold whole samples");
        }
        let arriving = (audio.len() / self.width) as u64;
        if arriving > a_second {
            return Err("a message of audio may hold a second at most");
        }
        let began = *self.began.get_or_insert(now);
        let since = now.saturating_duration_since(began);
        let due =
            u64::try_from(since.as_micros() * u128::from(a_second) / 1_000_000).unwrap_or(u64::MAX);
        let have = self.position + (self.held.len() / self.width) as u64 + arriving;
        if due > have + a_second {
            // The far end stopped sending for a while. What it sends now
            // was heard now, and not when the count of samples says.
            self.held.clear();
            self.position = due.saturating_sub(arriving);
        }
        self.held.extend_from_slice(audio);
        Ok(())
    }

    /// The next packet that is full.
    pub fn next_packet(&mut self) -> Option<Packet> {
        let packet = self.packet();
        if self.held.len() < packet {
            return None;
        }
        let payload = self.held.split_to(packet).freeze();
        let sequence = self.sequence;
        // Both wrap round, as the port has them.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the place in a stream wraps round"
        )]
        let timestamp = self.position as u32;
        self.sequence = self.sequence.wrapping_add(1);
        self.position += (packet / self.width) as u64;
        Some(Packet {
            sequence,
            timestamp,
            payload,
        })
    }
}

/// The rate the place of an Opus packet is counted at, whatever rate
/// its audio was made from.
const OPUS_CLOCK: u64 = 48_000;

/// The most samples one Opus packet may hold: 120 ms.
const OPUS_AT_MOST: u32 = 5_760;

/// How many samples an Opus packet holds, at 48 kHz, as its first bytes
/// say (RFC 6716, section 3.1). `None` for what is no packet.
#[must_use]
pub fn opus_samples(packet: &[u8]) -> Option<u32> {
    let (&toc, rest) = packet.split_first()?;
    let config = toc >> 3;
    // The length of a frame, by how the packet was made: 10 to 60 ms of
    // SILK, 10 or 20 ms of both, 2.5 to 20 ms of CELT.
    let frame = match config {
        0..=11 => match config % 4 {
            0 => 480,
            1 => 960,
            2 => 1_920,
            _ => 2_880,
        },
        12..=15 => {
            if config % 2 == 0 {
                480
            } else {
                960
            }
        }
        _ => match config % 4 {
            0 => 120,
            1 => 240,
            2 => 480,
            _ => 960,
        },
    };
    let frames = match toc & 0b11 {
        0 => 1,
        1 | 2 => 2,
        // As many as the next byte says.
        _ => u32::from(rest.first()? & 0b11_1111),
    };
    let samples = frame * frames;
    (frames > 0 && samples <= OPUS_AT_MOST).then_some(samples)
}

/// A far end's Opus, a packet a message: each is numbered, and placed
/// where its first sample is in the stream.
#[derive(Debug, Default)]
pub struct OpusInbound {
    sequence: u16,
    /// How many samples came before the next packet, at 48 kHz.
    position: u64,
    /// When the first packet arrived.
    began: Option<Instant>,
}

impl OpusInbound {
    /// Takes a packet, which arrived at `now`.
    ///
    /// A far end that stopped sending for more than a second is heard
    /// from when it goes on, as with [`Inbound`].
    ///
    /// # Errors
    ///
    /// The message is no Opus packet.
    pub fn take(&mut self, packet: Bytes, now: Instant) -> Result<Packet, &'static str> {
        let Some(samples) = opus_samples(&packet) else {
            return Err("a message of audio has to hold one Opus packet");
        };
        let samples = u64::from(samples);
        let began = *self.began.get_or_insert(now);
        let since = now.saturating_duration_since(began);
        let due = u64::try_from(since.as_micros() * u128::from(OPUS_CLOCK) / 1_000_000)
            .unwrap_or(u64::MAX);
        if due > self.position + samples + OPUS_CLOCK {
            self.position = due.saturating_sub(samples);
        }
        let sequence = self.sequence;
        // Both wrap round, as the port has them.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the place in a stream wraps round"
        )]
        let timestamp = self.position as u32;
        self.sequence = self.sequence.wrapping_add(1);
        self.position += samples;
        Ok(Packet {
            sequence,
            timestamp,
            payload: packet,
        })
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use tokio::time::Instant;
    use waveport_domain::ids::{Epoch, GenId, OpTag};
    use waveport_ports::audio::Rate;

    use bytes::Bytes;

    use super::{Inbound, MOST_RUNS, OpusInbound, Packet, Runs, opus_samples};

    fn tag(generation: u32) -> OpTag {
        OpTag {
            generation: GenId(generation),
            epoch: Epoch(1),
        }
    }

    /// The number, the place and the length of a packet.
    fn placed(packet: &Packet) -> (u16, u32, usize) {
        (packet.sequence, packet.timestamp, packet.payload.len())
    }

    #[test]
    fn audio_is_cut_into_packets_whatever_the_pieces_it_arrives_in() {
        let mut inbound = Inbound::pcm(Rate::Hz16000);
        let now = Instant::now();
        // 20 ms at 16 kHz is 320 samples, 640 bytes.
        let piece: Vec<u8> = (0..500_u16).flat_map(u16::to_le_bytes).collect();
        inbound.take(&piece, now).unwrap();
        let first = inbound.next_packet().unwrap();
        assert_eq!(placed(&first), (0, 0, 640));
        assert_eq!(first.payload[..4], [0, 0, 1, 0]);
        assert!(inbound.next_packet().is_none());
        inbound.take(&piece, now).unwrap();
        let second = inbound.next_packet().unwrap();
        assert_eq!(placed(&second), (1, 320, 640));
        // It goes on where the first packet ended: sample 320 of the
        // first piece.
        assert_eq!(second.payload[..2], 320_u16.to_le_bytes());
        assert_eq!(placed(&inbound.next_packet().unwrap()), (2, 640, 640));
        assert!(inbound.next_packet().is_none());
    }

    #[test]
    fn a_byte_is_a_sample_of_g711() {
        let mut inbound = Inbound::g711();
        let now = Instant::now();
        // 20 ms at 8 kHz is 160 samples, and as many bytes. A carrier
        // sends them in pieces of its own length.
        let piece: Vec<u8> = (0..=255).collect();
        inbound.take(&piece[..100], now).unwrap();
        assert!(inbound.next_packet().is_none());
        // An odd number of bytes is whole samples here.
        inbound.take(&piece[100..221], now).unwrap();
        let first = inbound.next_packet().unwrap();
        assert_eq!(placed(&first), (0, 0, 160));
        assert_eq!(first.payload[..], piece[..160]);
        assert!(inbound.next_packet().is_none());
        inbound.take(&piece[..99], now).unwrap();
        let second = inbound.next_packet().unwrap();
        // The place counts samples, and so do the limits.
        assert_eq!(placed(&second), (1, 160, 160));
        assert_eq!(second.payload[..61], piece[160..221]);
        assert!(inbound.take(&[0; 8_001], now).is_err());
        inbound.take(&[0; 8_000], now).unwrap();
    }

    #[test]
    fn audio_that_is_not_whole_samples_or_is_too_long_is_refused() {
        let mut inbound = Inbound::pcm(Rate::Hz8000);
        let now = Instant::now();
        assert!(inbound.take(&[0; 3], now).is_err());
        assert!(inbound.take(&[0; 16_002], now).is_err());
        // A second is taken.
        inbound.take(&[0; 16_000], now).unwrap();
        // Nothing is nothing.
        inbound.take(&[], now).unwrap();
    }

    #[test]
    fn a_far_end_that_paused_is_heard_when_it_speaks_again() {
        let mut inbound = Inbound::pcm(Rate::Hz8000);
        let start = Instant::now();
        // 20 ms at 8 kHz is 160 samples, 320 bytes. Half a second
        // arrives, and a part of a packet with it.
        inbound.take(&[0; 8_100], start).unwrap();
        let mut last = None;
        while let Some(packet) = inbound.next_packet() {
            last = Some(packet);
        }
        assert_eq!(placed(&last.unwrap()), (24, 24 * 160, 320));

        // Nothing for five seconds, and then a packet: it is placed at
        // now, five seconds on, and the part that was held is let go.
        let later = start + Duration::from_secs(5);
        inbound.take(&[0; 320], later).unwrap();
        assert_eq!(
            placed(&inbound.next_packet().unwrap()),
            (25, 5 * 8_000 - 160, 320)
        );

        // A far end that is a little late is not taken to have paused.
        let mut inbound = Inbound::pcm(Rate::Hz8000);
        inbound.take(&[0; 320], start).unwrap();
        inbound
            .take(&[0; 320], start + Duration::from_millis(900))
            .unwrap();
        assert_eq!(inbound.next_packet().unwrap().timestamp, 0);
        assert_eq!(inbound.next_packet().unwrap().timestamp, 160);

        // The same in the other encoding, where a byte is a sample.
        let mut inbound = Inbound::g711();
        inbound.take(&[0; 160], start).unwrap();
        assert_eq!(placed(&inbound.next_packet().unwrap()), (0, 0, 160));
        inbound.take(&[0; 160], later).unwrap();
        assert_eq!(
            placed(&inbound.next_packet().unwrap()),
            (1, 5 * 8_000 - 160, 160)
        );
    }

    #[test]
    fn an_opus_packet_says_how_long_it_is() {
        // How a packet was made is in the top five bits of its first
        // byte, and how many frames it holds in the bottom two.
        for (packet, samples) in [
            // One frame: 10 and 60 ms of SILK, 20 ms of both, 2.5 and
            // 20 ms of CELT.
            (&[0x00][..], Some(480)),
            (&[0x18], Some(2_880)),
            (&[0x78], Some(960)),
            (&[0x80], Some(120)),
            (&[0xF8, 1, 2, 3], Some(960)),
            // Two frames, of one length or of two.
            (&[0xF9], Some(1_920)),
            (&[0xFA], Some(1_920)),
            // As many as the second byte says, whose top bits say other
            // things.
            (&[0xFB, 0x03], Some(2_880)),
            (&[0xFB, 0xC6], Some(5_760)),
            // More than a packet may hold, none at all, and a count that
            // is not there.
            (&[0xFB, 0x07], None),
            (&[0x1B, 0x03], None),
            (&[0xFB, 0x00], None),
            (&[0xFB], None),
            (&[], None),
        ] {
            assert_eq!(opus_samples(packet), samples, "{packet:02X?}");
        }
    }

    #[test]
    fn opus_packets_are_numbered_and_placed_by_what_they_hold() {
        let mut inbound = OpusInbound::default();
        let start = Instant::now();
        let twenty = Bytes::from_static(&[0xF8, 9, 9]);
        let ten = Bytes::from_static(&[0xF0]);
        let first = inbound.take(twenty.clone(), start).unwrap();
        assert_eq!(placed(&first), (0, 0, 3));
        assert_eq!(first.payload, twenty);
        // The next is placed after what the one before it held.
        assert_eq!(
            placed(&inbound.take(ten.clone(), start).unwrap()),
            (1, 960, 1)
        );
        assert_eq!(
            placed(&inbound.take(twenty.clone(), start).unwrap()),
            (2, 1_440, 3)
        );
        // What is no packet is refused, and counts for nothing.
        assert!(inbound.take(Bytes::new(), start).is_err());
        assert!(inbound.take(Bytes::from_static(&[0xFB]), start).is_err());
        // A far end that is a little late is not taken to have paused.
        let late = start + Duration::from_millis(900);
        assert_eq!(
            placed(&inbound.take(twenty.clone(), late).unwrap()),
            (3, 2_400, 3)
        );
        // One that paused is heard when it goes on: five seconds in.
        let later = start + Duration::from_secs(5);
        assert_eq!(
            placed(&inbound.take(twenty, later).unwrap()),
            (4, 5 * 48_000 - 960, 3)
        );
        assert_eq!(
            placed(&inbound.take(ten, later).unwrap()),
            (5, 5 * 48_000, 1)
        );
    }

    #[test]
    fn a_far_end_that_came_back_is_told_again_whose_audio_follows() {
        let mut runs = Runs::default();
        let number = runs.of(tag(1)).number;
        assert!(runs.begins(number));
        assert!(!runs.begins(number));
        runs.begin_anew();
        // It is the same response under the same number, and is said to
        // begin once more.
        assert_eq!(runs.of(tag(1)).number, number);
        assert!(runs.begins(number));
        assert!(!runs.begins(number));
    }

    #[test]
    fn responses_are_numbered_as_they_come_and_the_oldest_are_forgotten() {
        let mut runs = Runs::default();
        assert!(runs.is_empty());
        assert_eq!(runs.of(tag(7)).number, 0);
        runs.of(tag(7)).sent = 960;
        assert_eq!(runs.of(tag(9)).number, 1);
        // A response that is known keeps its number, and what was sent.
        assert_eq!(runs.of(tag(7)).number, 0);
        assert_eq!(runs.numbered(0).unwrap().sent, 960);
        assert_eq!(runs.tagged(tag(9)).unwrap().number, 1);
        assert!(runs.numbered(2).is_none());
        assert!(runs.tagged(tag(8)).is_none());
        assert_eq!(runs.len(), 2);

        for (_, generation) in (0..MOST_RUNS).zip(100_u32..) {
            let _ = runs.of(tag(generation));
        }
        assert!(runs.tagged(tag(7)).is_none());
        assert!(runs.numbered(0).is_none());
        assert_eq!(runs.len(), MOST_RUNS);
    }

    #[test]
    fn a_far_end_is_told_once_whose_audio_follows() {
        let mut runs = Runs::default();
        // The first piece of a response says whose it is, and the ones
        // after it do not.
        assert!(runs.begins(0));
        assert!(!runs.begins(0));
        // Another response in between, and the first has to be said
        // again.
        assert!(runs.begins(1));
        assert!(runs.begins(0));
        // One that ended is said again if more of it comes.
        runs.ends(0);
        assert!(runs.begins(0));
        // The end of a response that is not the one that follows changes
        // nothing.
        runs.ends(1);
        assert!(!runs.begins(0));
    }
}
