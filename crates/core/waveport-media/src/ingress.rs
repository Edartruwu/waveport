//! The inbound path: from the packets of a transport to audio in order.
//!
//! Packets can arrive late, twice, out of order or not at all. [`Ingress`]
//! holds them until each can be handed on in its place, decodes them, and
//! fills what is missing, so that what comes out is one unbroken stream of
//! samples at one rate with a place on the media clock.
//!
//! What it hands on goes three ways: to the caller, frame by frame; into a
//! window of the most recent audio, for the models that want to look back;
//! and to speech recognition, which takes it in its own time. What waits
//! for speech recognition is the end of the window, and is not held a
//! second time: the path keeps count of how much of the end is not taken
//! yet.
//!
//! # Waiting, and not waiting
//!
//! The listeners here are machines, so audio is handed on as soon as it
//! can be: nothing is held back to smooth the pace. A packet is waited for
//! only when a later one has overtaken it, and then for no longer than
//! packets have lately needed. The wait starts at nothing, grows by one
//! frame each time a packet arrives after it was given up, to
//! [`max_wait`](IngressConfig::max_wait) at most, and shrinks again when
//! none has for a while.
//!
//! # Gaps
//!
//! A packet that is given up is made up from the audio before it, for
//! [`MAX_CONCEAL_MS`] in a row at most. A longer gap, and the gaps a
//! sender leaves when it stops sending in silence, are filled with silence,
//! so that the stream keeps its length. A gap longer than
//! [`max_fill`](IngressConfig::max_fill) is not filled: the stream is
//! broken off there and begins again, at the time the next packet
//! arrives.

use core::time::Duration;

use waveport_domain::time::{MediaDur, MediaTime, MonoTime};

use crate::codec::{Codec, CodecError, Decoder, MAX_CONCEAL_MS};
use crate::rate::{FRAME_MS, Rate};
use crate::ring::Ring;

/// How much the wait for a missing packet changes at a time.
const WAIT_STEP: Duration = Duration::from_millis(FRAME_MS as u64);

/// How long no packet may arrive too late before the wait is shortened.
const CALM: Duration = Duration::from_secs(10);

/// How many packets in a row may arrive far behind the stream before they
/// are taken for a new stream.
const RESTART_AFTER: u32 = 8;

/// How many packets behind the stream is far: more than any packet is
/// delayed, and less than a sender that begins anew is likely to land.
const FAR_BEHIND: u16 = 100;

/// How an inbound path is set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngressConfig {
    /// How the audio is written on the wire.
    pub codec: Codec,
    /// The rate of the audio handed on.
    pub rate: Rate,
    /// The reading of the clock at which the session's media clock is
    /// zero.
    pub origin: MonoTime,
    /// The longest a missing packet is waited for.
    pub max_wait: Duration,
    /// How many packets are held at most while one before them is
    /// missing.
    pub held: usize,
    /// The longest gap that is filled with silence.
    pub max_fill: MediaDur,
    /// How much of the most recent audio is kept to look back on.
    pub window: MediaDur,
    /// How much audio waits for speech recognition at most. It waits in
    /// the window, so this is no more than the window holds.
    pub queue: MediaDur,
}

impl IngressConfig {
    /// The settings of the design for `codec`: audio at 16 kHz, a wait of
    /// 60 ms at most, 200 ms of packets held, gaps filled up to a second,
    /// a window of 8 s and a queue of 2 s.
    #[must_use]
    pub const fn new(codec: Codec) -> Self {
        Self {
            codec,
            rate: Rate::Hz16000,
            origin: MonoTime::ZERO,
            max_wait: Duration::from_millis(60),
            held: 10,
            max_fill: MediaDur::from_millis(1_000),
            window: MediaDur::from_millis(8_000),
            queue: MediaDur::from_millis(2_000),
        }
    }
}

/// One packet of a stream, as a transport delivers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    /// Its number in the stream. It goes up by one from each packet to the
    /// next, and starts again from zero after 65,535.
    pub sequence: u16,
    /// Where its audio begins, in units of the codec's
    /// [clock](Codec::clock).
    pub timestamp: u32,
    /// The audio, encoded.
    pub payload: &'a [u8],
}

/// What became of a packet given to [`Ingress::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// It is held, to be handed on in its place.
    Held,
    /// Its place in the stream was already passed.
    Late,
    /// It is already held.
    Twice,
    /// It is longer than a packet of its codec may be, or empty.
    Refused,
    /// There is no room: too many packets are held.
    NoRoom,
}

/// What a piece of audio handed on is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Decoded from a packet.
    Packet,
    /// Made up for a packet that did not arrive.
    MadeUp,
    /// Silence where nothing was sent or too much was lost.
    Silence,
}

/// A piece of the inbound stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heard<'a> {
    /// Where it begins on the session's media clock.
    pub at: MediaTime,
    /// Its samples.
    pub pcm: &'a [i16],
    /// What it is.
    pub source: Source,
    /// Whether the stream begins here: this is the first audio, or the
    /// audio before it ends earlier than this begins.
    pub begins: bool,
}

/// Counts of what an inbound path has seen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngressStats {
    /// Packets held to be handed on.
    pub packets: u64,
    /// Packets that arrived after their place was passed.
    pub late: u64,
    /// Packets that arrived twice.
    pub twice: u64,
    /// Packets refused for their length or for want of room.
    pub refused: u64,
    /// Packets that could not be decoded.
    pub undecodable: u64,
    /// Packets whose audio was made up.
    pub made_up: u64,
    /// Packets given up with nothing made up for them.
    pub skipped: u64,
    /// Times the stream began again after a gap too long to fill.
    pub breaks: u64,
    /// Samples the queue for speech recognition lost, because nothing
    /// took them out in time or because the stream broke off.
    pub queue_dropped: u64,
}

#[derive(Clone, Debug)]
struct Slot {
    used: bool,
    /// The packet's number, counted on past 65,535.
    sequence: u64,
    timestamp: u32,
    arrived: MonoTime,
    len: usize,
    payload: Box<[u8]>,
}

/// The inbound path of one stream of audio.
#[derive(Debug)]
pub struct Ingress {
    config: IngressConfig,
    decoder: Decoder,
    slots: Box<[Slot]>,
    /// The number of the next packet to hand on, once one has arrived.
    expected: Option<u64>,
    /// The timestamp the next packet carries if nothing is missing.
    expected_timestamp: u32,
    /// Whether the next audio handed on begins the stream anew.
    begins: bool,
    /// Whether any audio was handed on.
    started: bool,
    /// How long the last packet was.
    packet: MediaDur,
    /// Audio made up since the last packet that was decoded.
    made_up: MediaDur,
    /// Where the audio handed on so far ends.
    end: MediaTime,
    wait: Duration,
    /// When a packet last arrived too late, or the wait last changed.
    calm_since: MonoTime,
    /// Packets in a row that arrived far behind the stream.
    behind: u32,
    /// One packet's samples.
    pcm: Box<[i16]>,
    window: Ring,
    /// How many samples can wait for speech recognition.
    queue: usize,
    /// How many do: the end of the window that it has not taken yet.
    unread: usize,
    stats: IngressStats,
}

impl Ingress {
    /// An inbound path.
    ///
    /// # Errors
    ///
    /// The codec could not be set up.
    pub fn new(config: IngressConfig) -> Result<Self, CodecError> {
        let decoder = Decoder::new(config.codec, config.rate)?;
        let slot = Slot {
            used: false,
            sequence: 0,
            timestamp: 0,
            arrived: MonoTime::ZERO,
            len: 0,
            payload: vec![0; config.codec.max_packet()].into_boxed_slice(),
        };
        let window = Ring::new(config.rate.samples_in(config.window));
        let queue = config.rate.samples_in(config.queue).min(window.capacity());
        Ok(Self {
            slots: vec![slot; config.held.max(1)].into_boxed_slice(),
            expected: None,
            expected_timestamp: 0,
            begins: true,
            started: false,
            packet: MediaDur::from_millis(FRAME_MS),
            made_up: MediaDur::ZERO,
            end: MediaTime::ZERO,
            wait: Duration::ZERO,
            calm_since: config.origin,
            behind: 0,
            pcm: vec![0; decoder.max_samples()].into_boxed_slice(),
            window,
            queue,
            unread: 0,
            stats: IngressStats::default(),
            decoder,
            config,
        })
    }

    /// How it was set up.
    #[must_use]
    pub const fn config(&self) -> &IngressConfig {
        &self.config
    }

    /// What it has seen.
    #[must_use]
    pub const fn stats(&self) -> IngressStats {
        self.stats
    }

    /// How long a missing packet is waited for at present.
    #[must_use]
    pub const fn wait(&self) -> Duration {
        self.wait
    }

    /// Where the audio handed on so far ends on the media clock.
    #[must_use]
    pub const fn end(&self) -> MediaTime {
        self.end
    }

    /// The most recent audio handed on. Its newest sample is the one
    /// before [`end`](Self::end), and it has no gaps: it is emptied when
    /// the stream breaks off.
    #[must_use]
    pub const fn window(&self) -> &Ring {
        &self.window
    }

    /// How much audio waits for speech recognition.
    #[must_use]
    pub const fn queued(&self) -> usize {
        self.unread
    }

    /// Takes audio that waits for speech recognition into the start of
    /// `out`, oldest first, and returns where it begins on the media clock
    /// and how many samples there are. What waits has no gaps.
    pub fn take_queued(&mut self, out: &mut [i16]) -> (MediaTime, usize) {
        let behind = self.config.rate.length_of(self.unread);
        let at = MediaTime::from_ticks(self.end.ticks().saturating_sub(u64::from(behind.ticks())));
        // No more is counted as waiting than the window holds, so what is
        // taken is never more than what waits.
        let taken = self.window.behind(self.unread, out);
        self.unread -= taken;
        (at, taken)
    }

    /// The place on the media clock of a reading of the clock.
    fn media_time(&self, now: MonoTime) -> MediaTime {
        let nanos = now.since(self.config.origin).as_nanos();
        // 48,000 ticks in 1,000,000,000 ns.
        MediaTime::from_ticks(u64::try_from(nanos * 3 / 62_500).unwrap_or(u64::MAX))
    }

    /// The codec's timestamp units in `length`.
    fn units(&self, length: MediaDur) -> u32 {
        length.ticks() / self.config.codec.clock().ticks_per_sample()
    }

    /// Takes a packet that arrived at `now`.
    pub fn push(&mut self, now: MonoTime, packet: Packet<'_>) -> Taken {
        self.calm(now);
        if packet.payload.is_empty() || packet.payload.len() > self.config.codec.max_packet() {
            self.stats.refused += 1;
            return Taken::Refused;
        }
        let sequence = match self.expected {
            None => u64::from(packet.sequence),
            Some(expected) => {
                // The nearest number to the one expected that ends in
                // these sixteen bits.
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the low sixteen bits are what is wanted"
                )]
                let ahead = packet.sequence.wrapping_sub(expected as u16);
                if ahead >= 0x8000 {
                    return self.behind(now, packet, ahead.wrapping_neg());
                }
                expected.saturating_add(u64::from(ahead))
            }
        };
        self.behind = 0;
        if self
            .slots
            .iter()
            .any(|slot| slot.used && slot.sequence == sequence)
        {
            self.stats.twice += 1;
            return Taken::Twice;
        }
        if self
            .slots
            .iter()
            .any(|slot| slot.used && slot.sequence > sequence)
        {
            // It was overtaken and still came in time: the wait is in use.
            self.calm_since = now;
        }
        let Some(slot) = self.slots.iter_mut().find(|slot| !slot.used) else {
            self.stats.refused += 1;
            return Taken::NoRoom;
        };
        let Some(room) = slot.payload.get_mut(..packet.payload.len()) else {
            self.stats.refused += 1;
            return Taken::Refused;
        };
        room.copy_from_slice(packet.payload);
        slot.used = true;
        slot.sequence = sequence;
        slot.timestamp = packet.timestamp;
        slot.arrived = now;
        slot.len = packet.payload.len();
        if self.expected.is_none() {
            self.expected = Some(sequence);
        }
        self.stats.packets += 1;
        Taken::Held
    }

    /// A packet arrived `back` places behind the stream.
    fn behind(&mut self, now: MonoTime, packet: Packet<'_>, back: u16) -> Taken {
        if back <= FAR_BEHIND {
            // Its place was passed a moment ago: packets need longer than
            // they were given.
            self.behind = 0;
            self.stats.late += 1;
            self.wait = (self.wait + WAIT_STEP).min(self.config.max_wait);
            self.calm_since = now;
            return Taken::Late;
        }
        self.behind += 1;
        if self.behind < RESTART_AFTER {
            self.stats.late += 1;
            return Taken::Late;
        }
        // Packets keep arriving from somewhere else in the numbering: the
        // sender began anew.
        for slot in &mut self.slots {
            slot.used = false;
        }
        self.expected = None;
        self.behind = 0;
        self.begins = true;
        self.push(now, packet)
    }

    /// Shortens the wait when no packet has arrived too late for a while.
    fn calm(&mut self, now: MonoTime) {
        if self.wait > Duration::ZERO && now.since(self.calm_since) >= CALM {
            self.wait = self.wait.saturating_sub(WAIT_STEP);
            self.calm_since = now;
        }
    }

    /// Hands on, through `sink`, all the audio that can be handed on at
    /// `now`. Returns when to call again if no packet arrives before
    /// then: the time at which a missing packet will be given up.
    pub fn poll(&mut self, now: MonoTime, mut sink: impl FnMut(Heard<'_>)) -> Option<MonoTime> {
        self.calm(now);
        loop {
            let expected = self.expected?;
            if let Some(index) = self
                .slots
                .iter()
                .position(|slot| slot.used && slot.sequence == expected)
            {
                self.release(index, &mut sink);
                continue;
            }
            // The packet is missing. Whether to go on without it depends
            // on what is held from after it.
            let (mut count, mut noticed, mut nearest) = (0, None, None);
            for slot in self.slots.iter().filter(|slot| slot.used) {
                count += 1;
                noticed =
                    Some(noticed.map_or(slot.arrived, |first: MonoTime| first.min(slot.arrived)));
                nearest = Some(nearest.map_or(slot.sequence, |near: u64| near.min(slot.sequence)));
            }
            let (Some(noticed), Some(nearest)) = (noticed, nearest) else {
                // Nothing has overtaken it: it is simply not here yet.
                return None;
            };
            let until = noticed.after(self.wait);
            if now < until && count < self.slots.len() {
                return Some(until);
            }
            self.give_up(expected, nearest, &mut sink);
        }
    }

    /// Hands on the packet in the slot at `index`, which is the one
    /// expected.
    fn release(&mut self, index: usize, sink: &mut impl FnMut(Heard<'_>)) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        slot.used = false;
        let (timestamp, arrived, sequence) = (slot.timestamp, slot.arrived, slot.sequence);
        self.expected = Some(sequence.saturating_add(1));
        let payload = slot.payload.get(..slot.len).unwrap_or(&[]);
        let Ok(length) = self.decoder.length_of(payload) else {
            self.stats.undecodable += 1;
            self.lose(None, sink);
            return;
        };

        // Where the packet's audio begins, against where the audio so far
        // ends.
        #[expect(
            clippy::cast_possible_wrap,
            reason = "timestamps wrap round: more than half the range ahead is behind"
        )]
        let ahead = timestamp.wrapping_sub(self.expected_timestamp) as i32;
        let gap = u32::try_from(ahead).unwrap_or(0);
        let gap =
            MediaDur::from_ticks(gap.saturating_mul(self.config.codec.clock().ticks_per_sample()));
        if self.begins || gap > self.config.max_fill {
            self.begin(arrived);
        } else if gap > MediaDur::ZERO {
            self.fill(gap, sink);
        }
        self.expected_timestamp = timestamp.wrapping_add(self.units(length));

        let payload = self
            .slots
            .get(index)
            .and_then(|slot| slot.payload.get(..slot.len))
            .unwrap_or(&[]);
        let Ok(samples) = self.decoder.decode(payload, &mut self.pcm) else {
            self.stats.undecodable += 1;
            // The place its audio would have taken is kept.
            self.expected_timestamp = timestamp;
            self.lose(None, sink);
            return;
        };
        self.packet = length;
        self.made_up = MediaDur::ZERO;
        self.hand_on(samples, Source::Packet, sink);
    }

    /// Goes on without the packet `expected`. `nearest` is the first
    /// packet held from after it.
    fn give_up(&mut self, expected: u64, nearest: u64, sink: &mut impl FnMut(Heard<'_>)) {
        if self.made_up >= MediaDur::from_millis(MAX_CONCEAL_MS) {
            // Nothing more can be made up. What is missing shows as a gap
            // when the next packet is handed on.
            self.stats.skipped += nearest.saturating_sub(expected);
            self.expected = Some(nearest);
            return;
        }
        self.expected = Some(expected.saturating_add(1));
        // The packet after a lost one may carry a copy of it.
        let next = (nearest == expected.saturating_add(1))
            .then(|| {
                self.slots
                    .iter()
                    .position(|slot| slot.used && slot.sequence == nearest)
            })
            .flatten();
        self.lose(next, sink);
    }

    /// Makes up the audio of one lost packet, from the packet after it in
    /// the slot `next` where that is at hand.
    fn lose(&mut self, next: Option<usize>, sink: &mut impl FnMut(Heard<'_>)) {
        if self.begins || self.made_up >= MediaDur::from_millis(MAX_CONCEAL_MS) {
            self.stats.skipped += 1;
            return;
        }
        let length = self.packet;
        let copy = next
            .and_then(|index| self.slots.get(index))
            .and_then(|slot| slot.payload.get(..slot.len));
        let made = match copy {
            Some(copy) => self.decoder.recover(copy, length, &mut self.pcm),
            None => self.decoder.conceal(length, &mut self.pcm),
        };
        let Ok(samples) = made else {
            self.stats.skipped += 1;
            return;
        };
        self.stats.made_up += 1;
        self.made_up = self.made_up.saturating_add(length);
        self.expected_timestamp = self.expected_timestamp.wrapping_add(self.units(length));
        self.hand_on(samples, Source::MadeUp, sink);
    }

    /// Begins the stream anew with a packet that arrived at `arrived`.
    fn begin(&mut self, arrived: MonoTime) {
        self.stats.breaks += u64::from(self.started);
        self.begins = true;
        self.made_up = MediaDur::ZERO;
        // What was heard before does not join on to what follows.
        self.window.clear();
        self.stats.queue_dropped += self.unread as u64;
        self.unread = 0;
        self.end = self.end.max(self.media_time(arrived));
    }

    /// Hands on `gap` of silence.
    fn fill(&mut self, gap: MediaDur, sink: &mut impl FnMut(Heard<'_>)) {
        let mut left = self.config.rate.samples_in(gap);
        let frame = self.config.rate.frame().min(self.pcm.len());
        if let Some(silence) = self.pcm.get_mut(..frame) {
            silence.fill(0);
        }
        while left > 0 {
            let samples = left.min(frame);
            self.hand_on(samples, Source::Silence, sink);
            left -= samples;
        }
    }

    /// Hands on the first `samples` samples of the scratch buffer.
    fn hand_on(&mut self, samples: usize, source: Source, sink: &mut impl FnMut(Heard<'_>)) {
        let Some(pcm) = self.pcm.get(..samples) else {
            return;
        };
        self.window.push(pcm);
        // Speech recognition is that much further behind the end of the
        // window. What is over the most that may wait for it is lost to
        // it, oldest first.
        let waiting = self.unread + samples;
        self.unread = waiting.min(self.queue);
        self.stats.queue_dropped += (waiting - self.unread) as u64;
        sink(Heard {
            at: self.end,
            pcm,
            source,
            begins: self.begins,
        });
        self.begins = false;
        self.started = true;
        self.end = self.end.after(self.config.rate.length_of(samples));
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use waveport_domain::time::{MediaDur, MediaTime, MonoTime};

    use super::{Heard, Ingress, IngressConfig, IngressStats, Packet, Source, Taken};
    use crate::codec::g711::Law;
    use crate::codec::opus::OpusSettings;
    use crate::codec::{Codec, Encoder};
    use crate::rate::Rate;
    use crate::testing::{share_at, tone};

    /// What a path handed on, written down.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Piece {
        at_ms: u64,
        samples: usize,
        source: Source,
        begins: bool,
    }

    fn piece(at_ms: u64, source: Source) -> Piece {
        Piece {
            at_ms,
            samples: 320,
            source,
            begins: false,
        }
    }

    /// A stream of packets and a path that takes them.
    struct Line {
        ingress: Ingress,
        /// The first 170 packets of the stream, 20 ms each.
        packets: Vec<Vec<u8>>,
        step: u32,
        pieces: Vec<Piece>,
        pcm: Vec<i16>,
    }

    impl Line {
        fn new(codec: Codec) -> Self {
            Self::with(IngressConfig::new(codec))
        }

        fn with(config: IngressConfig) -> Self {
            // Speech synthesis is not involved here: a tone, encoded from
            // 48 kHz.
            let rate = Rate::Hz48000;
            let mut encoder = Encoder::new(config.codec, rate, OpusSettings::default()).unwrap();
            let step = encoder.timestamp_step(rate.frame());
            let packets = tone(rate, 440.0, rate.frame() * 170)
                .chunks(rate.frame())
                .map(|frame| {
                    let mut packet = vec![0; config.codec.max_packet()];
                    let len = encoder.encode(frame, &mut packet).unwrap();
                    packet.truncate(len);
                    packet
                })
                .collect();
            Self {
                ingress: Ingress::new(config).unwrap(),
                packets,
                step,
                pieces: Vec::new(),
                pcm: Vec::new(),
            }
        }

        /// Packet `n` of the stream arrives at `ms`. Its number and
        /// timestamp start well away from zero and wrap round soon.
        fn arrive(&mut self, ms: u64, n: usize) -> Taken {
            let n32 = u32::try_from(n).unwrap();
            let packet = Packet {
                sequence: 65_500_u16.wrapping_add(u16::try_from(n).unwrap()),
                timestamp: 0xFFFF_F000_u32.wrapping_add(n32 * self.step),
                payload: &self.packets[n],
            };
            self.ingress.push(MonoTime::from_millis(ms), packet)
        }

        fn poll(&mut self, ms: u64) -> Option<u64> {
            let (pieces, pcm) = (&mut self.pieces, &mut self.pcm);
            let next = self
                .ingress
                .poll(MonoTime::from_millis(ms), |heard: Heard<'_>| {
                    pieces.push(Piece {
                        at_ms: heard.at.ticks() / 48,
                        samples: heard.pcm.len(),
                        source: heard.source,
                        begins: heard.begins,
                    });
                    pcm.extend(heard.pcm);
                });
            next.map(|at| at.as_nanos() / 1_000_000)
        }

        /// Packet `n` arrives on time: 20 ms after the one before it,
        /// with the first at 100 ms. Then the path is polled.
        fn on_time(&mut self, n: usize) -> Taken {
            let ms = 100 + 20 * u64::try_from(n).unwrap();
            let taken = self.arrive(ms, n);
            self.poll(ms);
            taken
        }

        /// Packet `n + 1` overtakes packet `n`, which arrives 5 ms after
        /// it was given up. The packets come a second apart here, so that
        /// there is time for that.
        fn overtaken(&mut self, n: usize) -> Taken {
            let at = 1_000 * (u64::try_from(n).unwrap() + 1);
            self.arrive(at, n + 1);
            let until = self.poll(at).unwrap_or(at);
            self.poll(until);
            let taken = self.arrive(until + 5, n);
            self.poll(until + 5);
            taken
        }

        fn stats(&self) -> IngressStats {
            self.ingress.stats()
        }
    }

    #[test]
    fn packets_in_order_are_handed_on_at_once() {
        for codec in [Codec::Opus, Codec::G711(Law::A), Codec::Pcm(Rate::Hz48000)] {
            let mut line = Line::new(codec);
            assert_eq!(line.poll(50), None, "nothing has arrived");
            for n in 0..50 {
                assert_eq!(line.on_time(n), Taken::Held);
                assert_eq!(line.pieces.len(), n + 1, "{codec:?}");
            }
            // The stream begins where the clock was when it arrived, and
            // each piece follows the one before it.
            let first = Piece {
                begins: true,
                ..piece(100, Source::Packet)
            };
            assert_eq!(line.pieces[0], first, "{codec:?}");
            for (n, heard) in line.pieces.iter().enumerate().skip(1) {
                let at_ms = 100 + 20 * u64::try_from(n).unwrap();
                assert_eq!(*heard, piece(at_ms, Source::Packet), "{codec:?}");
            }
            assert_eq!(line.ingress.end(), MediaTime::from_millis(1_100));
            assert!(share_at(Rate::Hz16000, 440.0, &line.pcm[3_200..]) > 0.95);
            assert_eq!(line.stats().packets, 50);
            assert_eq!(line.ingress.wait(), Duration::ZERO);
        }
    }

    #[test]
    fn a_lost_packet_is_made_up_as_soon_as_the_next_arrives() {
        let mut line = Line::new(Codec::Opus);
        for n in 0..10 {
            line.on_time(n);
        }
        // Packet 10 never comes.
        line.on_time(11);
        line.on_time(12);
        let sources: Vec<Source> = line.pieces[9..].iter().map(|piece| piece.source).collect();
        assert_eq!(
            sources,
            [
                Source::Packet,
                Source::MadeUp,
                Source::Packet,
                Source::Packet
            ]
        );
        // The stream keeps its length.
        assert_eq!(line.pieces[10], piece(300, Source::MadeUp));
        assert_eq!(line.pieces[11], piece(320, Source::Packet));
        assert_eq!((line.stats().made_up, line.stats().skipped), (1, 0));
        assert!(share_at(Rate::Hz16000, 440.0, &line.pcm[3_200..]) > 0.9);
    }

    #[test]
    fn a_packet_that_was_overtaken_is_dropped_the_first_time_and_waited_for_after() {
        let mut line = Line::new(Codec::Opus);
        for n in 0..5 {
            line.on_time(n);
        }
        // Packet 6 overtakes packet 5. Nothing is waited for yet, so 5 is
        // made up, and is too late when it comes.
        line.on_time(6);
        assert_eq!(line.arrive(225, 5), Taken::Late);
        assert_eq!(line.ingress.wait(), Duration::from_millis(20));
        assert_eq!(line.stats().late, 1);

        // The next time, the path waits.
        line.on_time(7);
        assert_eq!(line.arrive(280, 9), Taken::Held);
        assert_eq!(line.poll(280), Some(300), "packet 8 has until 300 ms");
        assert_eq!(line.pieces.len(), 8);
        assert_eq!(line.arrive(290, 8), Taken::Held);
        assert_eq!(line.poll(290), None);
        let last: Vec<Piece> = line.pieces[8..].to_vec();
        assert_eq!(
            last,
            [piece(260, Source::Packet), piece(280, Source::Packet)]
        );
        assert_eq!(line.stats().made_up, 1, "only the first time");
    }

    #[test]
    fn a_packet_that_does_not_come_in_time_is_given_up() {
        let mut line = Line::new(Codec::Opus);
        for n in 0..5 {
            line.on_time(n);
        }
        // Make the path wait: 20 ms after one late packet.
        line.on_time(6);
        line.arrive(225, 5);
        line.on_time(7);

        assert_eq!(line.arrive(280, 9), Taken::Held);
        assert_eq!(line.poll(280), Some(300));
        assert_eq!(line.poll(299), Some(300));
        assert_eq!(line.pieces.len(), 8);
        assert_eq!(line.poll(300), None);
        let last: Vec<Source> = line.pieces[8..].iter().map(|piece| piece.source).collect();
        assert_eq!(last, [Source::MadeUp, Source::Packet]);
    }

    #[test]
    fn the_wait_grows_to_its_limit_and_shrinks_when_packets_keep_time() {
        let mut line = Line::new(Codec::Opus);
        line.arrive(0, 0);
        line.poll(0);
        // Five packets are overtaken, each a little later than was waited.
        let mut waits = Vec::new();
        for pair in 0..5 {
            assert_eq!(line.overtaken(1 + 2 * pair), Taken::Late);
            waits.push(line.ingress.wait().as_millis());
        }
        assert_eq!(waits, [20, 40, 60, 60, 60], "60 ms is the limit");
        assert_eq!(line.stats().late, 5);

        // One that comes within the wait is taken.
        assert_eq!(line.arrive(12_000, 12), Taken::Held);
        assert_eq!(line.poll(12_000), Some(12_060));
        assert_eq!(line.arrive(12_030, 11), Taken::Held);
        assert_eq!(line.poll(12_030), None);
        assert_eq!(line.stats().late, 5);

        // The wait was last of use at 12.03 s. It shrinks ten seconds
        // after that, and every ten seconds from then on.
        line.poll(22_029);
        assert_eq!(line.ingress.wait(), Duration::from_millis(60));
        line.poll(22_030);
        assert_eq!(line.ingress.wait(), Duration::from_millis(40));
        line.poll(32_030);
        line.poll(42_030);
        assert_eq!(line.ingress.wait(), Duration::ZERO);
        line.poll(52_030);
        assert_eq!(line.ingress.wait(), Duration::ZERO);
    }

    #[test]
    fn a_long_loss_is_made_up_for_60_ms_and_silent_after() {
        let mut line = Line::new(Codec::G711(Law::Mu));
        for n in 0..10 {
            line.on_time(n);
        }
        // Packets 10 to 19 are lost: 200 ms.
        line.on_time(20);
        let after: Vec<Piece> = line.pieces[10..].to_vec();
        let mut expected = vec![
            piece(300, Source::MadeUp),
            piece(320, Source::MadeUp),
            piece(340, Source::MadeUp),
        ];
        expected.extend((0..7).map(|n| piece(360 + 20 * n, Source::Silence)));
        expected.push(piece(500, Source::Packet));
        assert_eq!(after, expected);
        assert_eq!((line.stats().made_up, line.stats().skipped), (3, 7));
        assert_eq!(line.stats().breaks, 0);
        assert_eq!(line.ingress.end(), MediaTime::from_millis(520));
    }

    #[test]
    fn a_sender_that_is_silent_for_a_while_leaves_silence() {
        let mut line = Line::new(Codec::Opus);
        for n in 0..10 {
            line.on_time(n);
        }
        // Nothing is lost: the sender sends nothing for 400 ms and then
        // carries on with the next number.
        let quiet = 20 * line.step;
        let packet = Packet {
            sequence: 65_500_u16.wrapping_add(10),
            timestamp: 0xFFFF_F000_u32.wrapping_add(10 * line.step + quiet),
            payload: &line.packets[10],
        };
        assert_eq!(
            line.ingress.push(MonoTime::from_millis(700), packet),
            Taken::Held
        );
        line.poll(700);
        let after: Vec<Piece> = line.pieces[10..].to_vec();
        assert_eq!(after.len(), 21);
        assert!(
            after[..20]
                .iter()
                .all(|piece| piece.source == Source::Silence)
        );
        assert_eq!(after[20], piece(700, Source::Packet));
        assert_eq!(line.stats().made_up, 0);
    }

    #[test]
    fn a_gap_too_long_to_fill_breaks_the_stream_off() {
        let mut line = Line::new(Codec::Opus);
        for n in 0..10 {
            line.on_time(n);
        }
        let mut queued = [0; 3_200];
        assert_eq!(line.ingress.take_queued(&mut queued[..1_600]).1, 1_600);

        // Three seconds are lost.
        assert_eq!(line.arrive(3_300, 160), Taken::Held);
        assert_eq!(line.poll(3_300), None);
        let after: Vec<Piece> = line.pieces[10..].to_vec();
        let begun = Piece {
            begins: true,
            ..piece(3_300, Source::Packet)
        };
        // The start of the gap is made up, as for any loss.
        assert_eq!(
            after,
            [
                piece(300, Source::MadeUp),
                piece(320, Source::MadeUp),
                piece(340, Source::MadeUp),
                begun
            ]
        );
        assert_eq!(line.stats().breaks, 1);
        assert_eq!((line.stats().made_up, line.stats().skipped), (3, 147));
        // What was heard before the break is not joined to what follows.
        assert_eq!(line.ingress.window().len(), 320);
        assert_eq!(line.stats().queue_dropped, 1_600 + 3 * 320);
        assert_eq!(
            line.ingress.take_queued(&mut queued),
            (MediaTime::from_millis(3_300), 320)
        );
    }

    #[test]
    fn numbers_and_timestamps_wrap_round_unnoticed() {
        let mut line = Line::new(Codec::Opus);
        // Numbers pass 65,535 at packet 36, timestamps pass their end at
        // packet 5.
        for n in 0..80 {
            assert_eq!(line.on_time(n), Taken::Held);
        }
        assert_eq!(line.pieces.len(), 80);
        assert!(
            line.pieces
                .iter()
                .all(|piece| piece.source == Source::Packet)
        );
        assert_eq!(line.ingress.end(), MediaTime::from_millis(1_700));
    }

    #[test]
    fn a_packet_that_comes_twice_counts_once() {
        let mut line = Line::new(Codec::Opus);
        for n in 0..5 {
            line.on_time(n);
        }
        // Once while it is held, and once after it was handed on.
        assert_eq!(line.arrive(210, 6), Taken::Held);
        assert_eq!(line.arrive(211, 6), Taken::Twice);
        assert_eq!(line.arrive(212, 3), Taken::Late);
        assert_eq!(line.stats().twice, 1);
        assert_eq!(line.stats().late, 1);
    }

    #[test]
    fn a_sender_that_begins_anew_is_followed() {
        let mut line = Line::new(Codec::Opus);
        for n in 100..110 {
            line.on_time(n);
        }
        // The numbers start again far behind.
        let mut taken = Vec::new();
        for n in 0..12 {
            taken.push(line.arrive(2_400 + 20 * n, usize::try_from(n).unwrap()));
            line.poll(2_400 + 20 * n);
        }
        assert_eq!(taken[..7], [Taken::Late; 7]);
        assert_eq!(taken[7..], [Taken::Held; 5]);
        let begun = Piece {
            begins: true,
            ..piece(2_540, Source::Packet)
        };
        assert_eq!(line.pieces[10], begun);
        assert_eq!(line.pieces.len(), 15);
        assert_eq!(line.stats().breaks, 1);
    }

    #[test]
    fn a_stream_that_ran_ahead_of_the_clock_does_not_go_back_when_it_begins_anew() {
        let mut line = Line::new(Codec::Opus);
        // A sender whose clock is fast: 20 ms of audio every 10 ms.
        for n in 110..160 {
            let ms = 100 + 10 * (u64::try_from(n).unwrap() - 110);
            line.arrive(ms, n);
            line.poll(ms);
        }
        assert_eq!(line.ingress.end(), MediaTime::from_millis(1_100));
        // It begins anew at 700 ms, where the audio so far ends at 1.1 s.
        for n in 0..9 {
            let ms = 700 + 10 * u64::try_from(n).unwrap();
            line.arrive(ms, n);
            line.poll(ms);
        }
        let begun = Piece {
            begins: true,
            ..piece(1_100, Source::Packet)
        };
        assert_eq!(line.pieces[50..], [begun, piece(1_120, Source::Packet)]);
    }

    #[test]
    fn no_more_is_held_than_there_is_room_for() {
        let mut line = Line::with(IngressConfig {
            held: 4,
            ..IngressConfig::new(Codec::Opus)
        });
        line.arrive(0, 0);
        line.poll(0);
        // Make the path wait 60 ms.
        for pair in 0..3 {
            assert_eq!(line.overtaken(1 + 2 * pair), Taken::Late);
        }
        assert_eq!(line.ingress.wait(), Duration::from_millis(60));
        let handed = line.pieces.len();

        // Packet 7 is missing and four more arrive within a millisecond.
        for n in 8..12 {
            assert_eq!(line.arrive(8_000, n), Taken::Held);
        }
        assert_eq!(line.arrive(8_000, 12), Taken::NoRoom);
        assert_eq!(line.stats().refused, 1);
        // With every place taken there is no point in waiting.
        assert_eq!(line.poll(8_000), None);
        assert_eq!(line.pieces.len(), handed + 5);
        assert_eq!(line.pieces[handed].source, Source::MadeUp);
    }

    #[test]
    fn what_is_not_a_packet_is_refused() {
        let mut line = Line::new(Codec::G711(Law::Mu));
        let now = MonoTime::from_millis(100);
        let empty = Packet {
            sequence: 1,
            timestamp: 0,
            payload: &[],
        };
        assert_eq!(line.ingress.push(now, empty), Taken::Refused);
        let long = vec![0; 481];
        let long = Packet {
            sequence: 1,
            timestamp: 0,
            payload: &long,
        };
        assert_eq!(line.ingress.push(now, long), Taken::Refused);
        assert_eq!(line.stats().refused, 2);
        assert_eq!(line.poll(100), None);
    }

    #[test]
    fn a_packet_that_cannot_be_decoded_is_treated_as_lost() {
        let mut line = Line::new(Codec::Opus);
        for n in 0..5 {
            line.on_time(n);
        }
        // A packet that says it holds frames and gives none.
        let bad = Packet {
            sequence: 65_505,
            timestamp: 0xFFFF_F000_u32.wrapping_add(5 * line.step),
            payload: &[0x03, 0x00],
        };
        assert_eq!(
            line.ingress.push(MonoTime::from_millis(200), bad),
            Taken::Held
        );
        line.poll(200);
        line.on_time(6);
        let last: Vec<Piece> = line.pieces[5..].to_vec();
        assert_eq!(
            last,
            [piece(200, Source::MadeUp), piece(220, Source::Packet)]
        );
        assert_eq!(line.stats().undecodable, 1);
    }

    #[test]
    fn packets_that_cannot_be_decoded_are_made_up_no_longer_than_lost_ones() {
        let mut line = Line::new(Codec::Opus);
        for n in 0..5 {
            line.on_time(n);
        }
        // Five in a row that say they hold frames and give none.
        for n in 5..10_u32 {
            let ms = 100 + 20 * u64::from(n);
            let bad = Packet {
                sequence: 65_500 + u16::try_from(n).unwrap(),
                timestamp: 0xFFFF_F000_u32.wrapping_add(n * line.step),
                payload: &[0x03, 0x00],
            };
            assert_eq!(
                line.ingress.push(MonoTime::from_millis(ms), bad),
                Taken::Held
            );
            line.poll(ms);
        }
        // 60 ms are made up, and then nothing is handed on.
        let made_up: Vec<Piece> = line.pieces[5..].to_vec();
        assert_eq!(
            made_up,
            [
                piece(200, Source::MadeUp),
                piece(220, Source::MadeUp),
                piece(240, Source::MadeUp),
            ]
        );
        // The next packet that can be decoded keeps its place: the rest of
        // the gap is silence.
        line.on_time(10);
        let after: Vec<Piece> = line.pieces[8..].to_vec();
        assert_eq!(
            after,
            [
                piece(260, Source::Silence),
                piece(280, Source::Silence),
                piece(300, Source::Packet),
            ]
        );
        let stats = line.stats();
        assert_eq!((stats.undecodable, stats.made_up, stats.skipped), (5, 3, 2));
    }

    #[test]
    fn the_window_and_the_queue_hold_what_was_handed_on() {
        let config = IngressConfig {
            window: MediaDur::from_millis(100),
            queue: MediaDur::from_millis(60),
            ..IngressConfig::new(Codec::Pcm(Rate::Hz16000))
        };
        let mut line = Line::with(config);
        for n in 0..10 {
            line.on_time(n);
        }
        // The last 100 ms.
        let mut window = vec![0; 1_600];
        assert_eq!(line.ingress.window().latest(&mut window), 1_600);
        assert_eq!(window[..], line.pcm[1_600..]);

        // The queue kept its newest 60 ms and lost the rest.
        assert_eq!(line.ingress.queued(), 960);
        assert_eq!(line.stats().queue_dropped, 3_200 - 960);
        let mut queued = vec![0; 2_000];
        let (at, samples) = line.ingress.take_queued(&mut queued[..320]);
        assert_eq!((at, samples), (MediaTime::from_millis(240), 320));
        assert_eq!(queued[..320], line.pcm[2_240..2_560]);
        let (at, samples) = line.ingress.take_queued(&mut queued);
        assert_eq!((at, samples), (MediaTime::from_millis(260), 640));
        assert_eq!(line.ingress.take_queued(&mut queued).1, 0);
    }

    #[test]
    fn no_more_waits_for_speech_recognition_than_the_window_holds() {
        let config = IngressConfig {
            window: MediaDur::from_millis(60),
            queue: MediaDur::from_millis(100),
            ..IngressConfig::new(Codec::Pcm(Rate::Hz16000))
        };
        let mut line = Line::with(config);
        for n in 0..10 {
            line.on_time(n);
        }
        // What waits is the end of the window, and the window is 60 ms.
        assert_eq!(line.ingress.queued(), 960);
        assert_eq!(line.stats().queue_dropped, 3_200 - 960);
        let mut queued = vec![0; 2_000];
        let (at, samples) = line.ingress.take_queued(&mut queued);
        assert_eq!((at, samples), (MediaTime::from_millis(240), 960));
        assert_eq!(queued[..960], line.pcm[2_240..]);
        assert_eq!(line.ingress.queued(), 0);
    }

    #[test]
    fn a_stream_starts_on_the_clock_of_its_session() {
        let config = IngressConfig {
            origin: MonoTime::from_millis(5_000),
            ..IngressConfig::new(Codec::Opus)
        };
        let mut line = Line::with(config);
        line.arrive(5_250, 0);
        line.poll(5_250);
        assert_eq!(line.pieces[0].at_ms, 250);
        assert!(line.pieces[0].begins);
        assert_eq!(line.ingress.config().origin, MonoTime::from_millis(5_000));
    }
}
