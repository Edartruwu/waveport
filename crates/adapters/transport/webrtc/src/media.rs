//! The audio line of a call, as either end has it: what was agreed for
//! it, what arrives on it, and what is sent on it.

use core::time::Duration;
use std::collections::VecDeque;
use std::time::Instant;

use bytes::Bytes;
use str0m::Rtc;
use str0m::format::Codec;
use str0m::media::{Mid, Pt};
use str0m::rtp::{RtpPacket, RtpWrite};
use waveport_adapter_kit::line::opus_samples;
use waveport_domain::event::Key;

use crate::red;
use crate::rtp::{
    KEY_EVERY, KEY_REST, KEY_SAID, KeysHeard, Numbering, key_after, now_in_stream, read_key,
    samples_in, saying,
};

/// The clock Opus is carried with, whatever rate it was recorded at.
pub(crate) const CLOCK: u32 = 48_000;

/// How long after the audio that was sent last has ended the next is
/// still taken to follow on. Later than that, there was a break: the
/// time of the stream moves on by as much, as the clock of a
/// microphone would have. It is what a session lets its own pace slip
/// by before it begins anew.
const FOLLOWS_WITHIN: Duration = Duration::from_millis(100);

/// How many keys may wait to be pressed.
const KEYS: usize = 32;

/// How long `samples` of audio last.
fn lasting(samples: u32) -> Duration {
    Duration::from_micros(u64::from(samples) * 1_000_000 / u64::from(CLOCK))
}

/// The payload types that were agreed for the audio line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Kinds {
    pub(crate) opus: Pt,
    /// Redundant audio, where both ends have it.
    pub(crate) red: Option<Pt>,
    /// The keys of a telephone, and the clock they are counted on.
    pub(crate) keys: Option<(Pt, u32)>,
    /// Whether audio that was lost is asked for again.
    pub(crate) nack: bool,
}

impl Kinds {
    /// What was agreed for the line `mid`. `None` for a line without
    /// Opus, which is no audio line of ours.
    fn of(rtc: &Rtc, mid: Mid) -> Option<Self> {
        let media = rtc.media(mid)?;
        let (mut opus, mut red, mut keys, mut nack) = (None, None, None, false);
        for kind in media.remote_pts() {
            let known = rtc.codec_config().params().iter();
            let Some(params) = known.into_iter().find(|params| params.pt() == *kind) else {
                continue;
            };
            let spec = params.spec();
            match spec.codec {
                Codec::Opus if opus.is_none() => {
                    opus = Some(params.pt());
                    red = params.red();
                    nack = params.fb_nack();
                }
                Codec::Tele => {
                    // The audio's clock where there is a choice.
                    let clock = spec.clock_rate.get();
                    if keys.is_none() || clock == CLOCK {
                        keys = Some((params.pt(), clock));
                    }
                }
                _ => {}
            }
        }
        Some(Self {
            opus: opus?,
            red,
            keys,
            nack,
        })
    }
}

/// Something that arrived on the audio line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum In {
    /// A frame of audio, numbered by where it is in time.
    Audio {
        sequence: u16,
        timestamp: u32,
        payload: Bytes,
    },
    /// A key was pressed.
    Key(Key),
}

/// Where a frame that was sent is in the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placed {
    /// The RTP time it begins at.
    pub(crate) time: u32,
    /// Whether there was a break before it.
    pub(crate) after_a_break: bool,
}

/// A key that is being pressed.
#[derive(Clone, Copy, Debug)]
struct Pressing {
    key: Key,
    /// The RTP time every packet of it has.
    time: u32,
    began: Instant,
    /// How many times it was said.
    said: u32,
}

/// The audio line of a call.
#[derive(Debug)]
pub(crate) struct Media {
    mid: Mid,
    kinds: Kinds,
    /// What arrives, by where it is in time.
    numbering: Numbering,
    /// The keys that were heard.
    keys_heard: KeysHeard,
    /// The RTP time of the key that was pressed last.
    key_pressed: Option<u32>,
    /// The number of the first packet.
    first: u64,
    /// The number of the next packet.
    sequence: u64,
    /// The RTP time at which the audio that was sent last ends.
    time: u32,
    /// When the audio that was sent last has been played, if it is
    /// played as it comes.
    ends: Option<Instant>,
    /// The frame that was sent last, and its RTP time: it goes with the
    /// next as its redundancy.
    before: Option<u32>,
    last: Vec<u8>,
    packed: Vec<u8>,
    keys: VecDeque<Key>,
    pressing: Option<Pressing>,
    rests_until: Option<Instant>,
}

impl Media {
    /// The audio line `mid` of `rtc`, as it was agreed. `None` for a
    /// line without Opus.
    pub(crate) fn of(rtc: &mut Rtc, mid: Mid) -> Option<Self> {
        let kinds = Kinds::of(rtc, mid)?;
        if kinds.nack {
            // `str0m` asks only where there is a stream of its own for
            // what is sent again, and browsers offer none for audio.
            if let Some(stream) = rtc.direct_api().stream_rx_by_mid(mid, None) {
                stream.suppress_nack(false);
            }
        }
        // Where a stream begins is nobody's business (RFC 3550, section
        // 5.1): chance, and nought where the system has none to give.
        let mut chance = [0_u8; 6];
        let _ = getrandom::fill(&mut chance);
        let [first @ .., _, _, _, _] = chance;
        let [_, _, time @ ..] = chance;
        // In the lower half of what sixteen bits count, so that it is
        // long before the count wraps round.
        let sequence = u64::from(u16::from_be_bytes(first) >> 1);
        Some(Self {
            mid,
            kinds,
            numbering: Numbering::default(),
            keys_heard: KeysHeard::default(),
            key_pressed: None,
            first: sequence,
            sequence,
            time: u32::from_be_bytes(time),
            ends: None,
            before: None,
            last: Vec::new(),
            packed: Vec::new(),
            keys: VecDeque::new(),
            pressing: None,
            rests_until: None,
        })
    }

    pub(crate) const fn kinds(&self) -> Kinds {
        self.kinds
    }

    /// The number of the first packet that was sent, or will be.
    pub(crate) const fn first(&self) -> u64 {
        self.first
    }

    /// Takes a packet of the line, and gives `heard` what it holds: a
    /// frame of audio for each one in it that was not heard before, or
    /// a key.
    pub(crate) fn heard(&mut self, packet: &RtpPacket, mut heard: impl FnMut(In)) {
        let kind = packet.header.payload_type;
        // The low 32 bits are the time of the packet.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "RTP time is the low 32 bits of what str0m counts on with"
        )]
        let time = packet.time.numer() as u32;
        let payload: &[u8] = &packet.payload;
        if self.kinds.keys.is_some_and(|(keys, _)| keys == kind) {
            // A key is the time its packets share: it is said many
            // times over, and each of them says the same.
            let Some(event) = read_key(payload) else {
                return;
            };
            if self.keys_heard.is_new(time) {
                heard(In::Key(event.key));
            }
            return;
        }
        if self.kinds.red == Some(kind) {
            let Some(frames) = red::frames(payload, *self.kinds.opus) else {
                return;
            };
            for block in frames {
                self.frame(time.wrapping_sub(block.back), block.audio, &mut heard);
            }
        } else if self.kinds.opus == kind {
            self.frame(time, payload, &mut heard);
        }
    }

    /// One frame of audio that arrived, by itself or beside a later
    /// one.
    fn frame(&mut self, time: u32, audio: &[u8], heard: &mut impl FnMut(In)) {
        // A packet of nothing but padding holds no audio, and neither
        // does the place of a frame the sender had nothing for.
        let Some(samples) = opus_samples(audio) else {
            return;
        };
        if let Some(sequence) = self.numbering.place(time, samples) {
            heard(In::Audio {
                sequence,
                timestamp: time,
                payload: Bytes::copy_from_slice(audio),
            });
        }
    }

    /// The RTP time of `now`: where audio that was sent now would
    /// begin.
    fn time_at(&self, now: Instant) -> u32 {
        let silent = self
            .ends
            .map_or(Duration::ZERO, |ends| now.saturating_duration_since(ends));
        if silent > FOLLOWS_WITHIN {
            self.time.wrapping_add(samples_in(silent, CLOCK))
        } else {
            self.time
        }
    }

    /// The RTP time of a key that is pressed at `now`: where `now` is
    /// on the clock of the audio, and after the key before it. A key is
    /// told from the one before by its time and by nothing else, so no
    /// two have one, whatever was sent of audio in between and however
    /// far ahead of its time that was sent.
    fn key_time(&self, now: Instant) -> u32 {
        key_after(now_in_stream(self.time, self.ends, now), self.key_pressed)
    }

    /// Sends a frame of `samples` of audio. `None` when the call has no
    /// line to send it on any more.
    pub(crate) fn send(
        &mut self,
        rtc: &mut Rtc,
        now: Instant,
        audio: &[u8],
        samples: u32,
    ) -> Option<Placed> {
        let time = self.time_at(now);
        // The first frame begins a stretch of speech, as one after a
        // break does (RFC 3551, section 4.1).
        let after_a_break = self.ends.is_none() || time != self.time;
        let (kind, payload): (Pt, &[u8]) = match self.kinds.red {
            Some(red) => {
                let back = self.before.filter(|_| !after_a_break);
                let back = back.map(|before| (time.wrapping_sub(before), self.last.as_slice()));
                red::pack(*self.kinds.opus, back.as_slice(), audio, &mut self.packed);
                (red, &self.packed)
            }
            None => (self.kinds.opus, audio),
        };
        let mut api = rtc.direct_api();
        let stream = api.stream_tx_by_mid(self.mid, None)?;
        let write = RtpWrite::new(kind, self.sequence.into(), time, now, payload);
        stream.write_rtp(write.marker(after_a_break));
        self.sequence += 1;
        self.last.clear();
        self.last.extend_from_slice(audio);
        self.before = Some(time);
        self.time = time.wrapping_add(samples);
        let from = match self.ends {
            Some(ends) if !after_a_break => ends,
            _ => now,
        };
        self.ends = Some(from + lasting(samples));
        Some(Placed {
            time,
            after_a_break,
        })
    }

    /// Whether keys can be pressed on this line.
    pub(crate) const fn presses(&self) -> bool {
        self.kinds.keys.is_some()
    }

    /// Presses `key` after the keys that wait, from `now` on at the
    /// earliest. Returns whether there was room for it.
    pub(crate) fn press(&mut self, now: Instant, key: Key) -> bool {
        if !self.presses() || self.keys.len() >= KEYS {
            return false;
        }
        self.keys.push_back(key);
        // Nothing is being pressed, and nothing was lately: it is due.
        self.rests_until.get_or_insert(now);
        true
    }

    /// When the keys want to be looked at again.
    pub(crate) fn keys_due(&self) -> Option<Instant> {
        match self.pressing {
            Some(pressing) => Some(pressing.began + KEY_EVERY * pressing.said),
            None if self.keys.is_empty() => None,
            None => self.rests_until,
        }
    }

    /// Says of the keys what is due by `now`. Returns whether anything
    /// was sent.
    ///
    /// A key is an event beside the audio (RFC 4733): its packets take
    /// the numbers of the stream and the time at which it began.
    pub(crate) fn press_on(&mut self, rtc: &mut Rtc, now: Instant) -> bool {
        let Some((kind, clock)) = self.kinds.keys else {
            return false;
        };
        let mut sent = false;
        loop {
            let pressing = if let Some(pressing) = self.pressing {
                pressing
            } else {
                if self.rests_until.is_some_and(|until| now < until) {
                    break;
                }
                let Some(key) = self.keys.pop_front() else {
                    break;
                };
                let time = self.key_time(now);
                self.key_pressed = Some(time);
                Pressing {
                    key,
                    time,
                    began: now,
                    said: 0,
                }
            };
            let held = now.saturating_duration_since(pressing.began);
            let due = u32::try_from(held.as_millis() / KEY_EVERY.as_millis()).unwrap_or(u32::MAX);
            let due = due.saturating_add(1).min(KEY_SAID);
            let mut api = rtc.direct_api();
            let Some(stream) = api.stream_tx_by_mid(self.mid, None) else {
                self.keys.clear();
                self.pressing = None;
                break;
            };
            let mut said = pressing.said;
            while said < due {
                said += 1;
                let saying = saying(pressing.key, said, clock);
                for _ in 0..saying.times {
                    let number = self.sequence.into();
                    let payload = saying.payload.as_slice();
                    let write = RtpWrite::new(kind, number, pressing.time, now, payload);
                    stream.write_rtp(write.marker(saying.begins));
                    self.sequence += 1;
                }
                sent = true;
            }
            if said == KEY_SAID {
                self.pressing = None;
                self.rests_until = Some(pressing.began + KEY_EVERY * KEY_SAID + KEY_REST);
            } else {
                self.pressing = Some(Pressing { said, ..pressing });
                break;
            }
        }
        sent
    }
}
