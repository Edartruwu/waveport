//! What is in the packets of an audio stream, and where each belongs.
//!
//! A browser's audio stream is not audio alone. Keys travel on it as
//! events of their own (RFC 4733), and both browsers send packets that
//! hold nothing but padding. All of them take sequence numbers of the
//! stream, so a listener that goes by those counts every one of them as
//! audio that was lost. With redundant audio a frame may also arrive
//! twice, inside a later packet and by itself.
//!
//! So audio is numbered here by its place in time: the frame that
//! begins where the last one ended is the next, whatever else was sent
//! in between, and a frame that was given before is not given again.
//! The session's reorder buffer is told those numbers and never the
//! stream's own.

use core::time::Duration;
use std::time::Instant;

use waveport_domain::event::Key;

use crate::media::CLOCK;

/// A key as a packet of the stream says it (RFC 4733): which key, and
/// whether this packet says that it was let go of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyEvent {
    pub(crate) key: Key,
    pub(crate) ends: bool,
}

/// Reads a packet of a key. `None` for an event that is no key: the
/// tones of a modem, a flash of the hook.
pub(crate) fn read_key(payload: &[u8]) -> Option<KeyEvent> {
    let &[event, flags, _, _] = payload.first_chunk::<4>()?;
    let key = *Key::ALL.get(usize::from(event))?;
    let ends = flags & 0x80 != 0;
    Some(KeyEvent { key, ends })
}

/// How loud a key is said to be: 10 dB below the loudest a line
/// carries, as a browser says it.
const VOLUME: u8 = 0x0a;

/// The number RFC 4733 gives a key.
fn code(key: Key) -> u8 {
    let place = Key::ALL.iter().position(|known| *known == key);
    u8::try_from(place.unwrap_or(0)).unwrap_or(0)
}

/// Writes a packet of a key that has been held for `held` units of the
/// clock its payload type has, at a level of -10 dBm0.
pub(crate) fn write_key(key: Key, ends: bool, held: u16) -> [u8; 4] {
    let [high, low] = held.to_be_bytes();
    [code(key), u8::from(ends) << 7 | VOLUME, high, low]
}

/// How often a key that is held is said again.
pub(crate) const KEY_EVERY: Duration = Duration::from_millis(20);

/// How many times a key is said while it is held: 160 ms of it.
pub(crate) const KEY_SAID: u32 = 8;

/// How many times it is said that a key was let go of, which is the one
/// packet of a key that must not be lost (RFC 4733, section 2.5.1.4).
const KEY_ENDS: u32 = 3;

/// How long nothing is pressed after a key, so that two of the same are
/// two.
pub(crate) const KEY_REST: Duration = Duration::from_millis(80);

/// How many units of a clock that counts `clock` in a second go by in
/// `time`.
pub(crate) fn samples_in(time: Duration, clock: u32) -> u32 {
    let samples = time.as_micros() * u128::from(clock) / 1_000_000;
    u32::try_from(samples).unwrap_or(u32::MAX)
}

/// One of the times a key is said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Saying {
    /// The packet.
    pub(crate) payload: [u8; 4],
    /// How many times it is sent.
    pub(crate) times: u32,
    /// Whether the key begins with it.
    pub(crate) begins: bool,
    /// Whether the key is let go of with it.
    pub(crate) ends: bool,
}

/// The `said`th time a key is said, from the first to the
/// [`KEY_SAID`]th, on a payload type whose clock counts `clock` in a
/// second. Each says how long the key has been held by then, and the
/// last that it was let go of, three times over.
pub(crate) fn saying(key: Key, said: u32, clock: u32) -> Saying {
    let ends = said >= KEY_SAID;
    let held = samples_in(KEY_EVERY * said, clock);
    Saying {
        payload: write_key(key, ends, u16::try_from(held).unwrap_or(u16::MAX)),
        times: if ends { KEY_ENDS } else { 1 },
        begins: said == 1,
        ends,
    }
}

/// Where `now` is on the clock of a stream whose audio so far ends at
/// the time `time`, which is played to at `ends`: as much before it as
/// that audio was sent ahead, and as much after as it is over.
pub(crate) fn now_in_stream(time: u32, ends: Option<Instant>, now: Instant) -> u32 {
    match ends {
        Some(ends) if now >= ends => {
            let since = now.saturating_duration_since(ends);
            time.wrapping_add(samples_in(since, CLOCK))
        }
        Some(ends) => {
            let ahead = ends.saturating_duration_since(now);
            time.wrapping_sub(samples_in(ahead, CLOCK))
        }
        None => time,
    }
}

/// The time of a key that is pressed at `at`, when the key before it
/// was at `last`: `at`, and no sooner than a key and its rest after the
/// one before. A key is told from the one before by its time and by
/// nothing else, so no two have one.
pub(crate) fn key_after(at: u32, last: Option<u32>) -> u32 {
    let Some(last) = last else {
        return at;
    };
    let earliest = last.wrapping_add(samples_in(KEY_EVERY * KEY_SAID + KEY_REST, CLOCK));
    // Whichever is later, on a clock that wraps round.
    if at.wrapping_sub(earliest) < 1 << 31 {
        at
    } else {
        earliest
    }
}

/// The keys a stream has said. A key is the time its packets share: it
/// is said many times over, and each of them says the same.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KeysHeard {
    /// The time of the newest.
    newest: Option<u32>,
}

impl KeysHeard {
    /// Whether a packet of a key at `time` is of one that was not heard
    /// yet: it is not of the newest, and not of one before it, whose
    /// packets may come late.
    pub(crate) fn is_new(&mut self, time: u32) -> bool {
        let new = self
            .newest
            .is_none_or(|newest| time != newest && time.wrapping_sub(newest) < 1 << 31);
        if new {
            self.newest = Some(time);
        }
        new
    }
}

/// How many frames back a frame is still known to have been given.
const REMEMBERED: u64 = 64;

/// Where audio stands in a stream: the number of each frame by the time
/// it begins at, and which were given.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Numbering {
    /// The newest frame that was given: when it begins, how long it is,
    /// and its number.
    newest: Option<(u32, u32, u64)>,
    /// Which of the [`REMEMBERED`] frames up to the newest were given:
    /// the lowest bit is the newest.
    given: u64,
}

impl Numbering {
    /// The number of the frame that begins at `time` and is `length`
    /// long, both in units of the RTP clock. `None` for a frame that
    /// was given before, or is too far back to be told from one that
    /// was.
    pub(crate) fn place(&mut self, time: u32, length: u32) -> Option<u16> {
        let length = length.max(1);
        let Some((newest, lasts, number)) = self.newest else {
            self.newest = Some((time, length, 0));
            self.given = 1;
            return Some(0);
        };
        // Time wraps round: more than half the range ahead is behind.
        let ahead = time.wrapping_sub(newest);
        let number = if ahead < 1 << 31 {
            // As many frames on as fit between the two, at the length
            // of the newest: a browser that skips frames leaves a hole
            // in time, and a hole is that many frames.
            let frames = u64::from((ahead + lasts / 2) / lasts);
            if frames == 0 {
                return None;
            }
            self.given = self
                .given
                .checked_shl(u32::try_from(frames).ok()?)
                .unwrap_or(0)
                | 1;
            let number = number.wrapping_add(frames);
            self.newest = Some((time, length, number));
            number
        } else {
            // As many back as fit, at its own length.
            let behind = newest.wrapping_sub(time);
            let frames = u64::from((behind + length / 2) / length);
            if frames == 0 || frames >= REMEMBERED {
                return None;
            }
            let bit = 1_u64 << frames;
            if self.given & bit != 0 {
                return None;
            }
            self.given |= bit;
            number.wrapping_sub(frames)
        };
        // A reorder buffer counts in sixteen bits, which wrap round.
        Some(u16::try_from(number & 0xffff).unwrap_or(0))
    }
}

/// What a datagram on the port of a call is, by its first byte (RFC
/// 7983).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Datagram {
    /// A check of ICE.
    Stun,
    /// A message of the handshake, or of what it seals.
    Dtls,
    /// Audio, or a report about it.
    Media,
    /// Nothing a call sends.
    Other,
}

impl Datagram {
    pub(crate) fn of(bytes: &[u8]) -> Self {
        match bytes.first() {
            Some(0..=3) => Self::Stun,
            Some(20..=63) => Self::Dtls,
            Some(128..=191) => Self::Media,
            _ => Self::Other,
        }
    }
}

/// Whose a check of ICE is: the name the answer gave this end of the
/// call, which a request names before a colon and the name of its
/// sender (RFC 8445, section 7.2.2). `None` for what is no request with
/// a name.
///
/// Nothing is proved by it: a request is checked by the call it is
/// handed to, with the password of that call. This only finds the call.
pub(crate) fn asked_of(datagram: &[u8]) -> Option<&str> {
    let (head, mut rest) = datagram.split_first_chunk::<20>()?;
    let [kind_high, kind_low, _, _, cookie @ ..] = head;
    // A binding request, with the cookie that says it is STUN.
    if [*kind_high, *kind_low] != [0x00, 0x01] || !cookie.starts_with(&[0x21, 0x12, 0xa4, 0x42]) {
        return None;
    }
    while let Some((&[kind_high, kind_low, length_high, length_low], after)) =
        rest.split_first_chunk::<4>()
    {
        let length = usize::from(u16::from_be_bytes([length_high, length_low]));
        let value = after.get(..length)?;
        if [kind_high, kind_low] == [0x00, 0x06] {
            let name = core::str::from_utf8(value).ok()?;
            let (ours, _theirs) = name.split_once(':')?;
            return Some(ours);
        }
        // Each attribute ends on a multiple of four.
        rest = after.get(length.div_ceil(4) * 4..)?;
    }
    None
}

/// A check that is addressed to the call `name` by somebody who knows
/// the name and not the password: nothing in it shows that they do.
#[cfg(test)]
pub(crate) fn forged_check(name: &str) -> Vec<u8> {
    let user = format!("{name}:theirs");
    let mut check = vec![0, 1, 0, 0, 0x21, 0x12, 0xA4, 0x42];
    check.extend_from_slice(&[7; 12]);
    check.extend_from_slice(&[0, 6, 0, user.len().to_le_bytes()[0]]);
    check.extend_from_slice(user.as_bytes());
    check.resize(check.len().next_multiple_of(4), 0);
    let length = (check.len() - 20).to_le_bytes()[0];
    check.splice(2..4, [0, length]);
    check
}

#[cfg(test)]
mod tests {
    use waveport_domain::event::Key;

    use core::time::Duration;
    use std::time::Instant;

    use super::{
        Datagram, KeyEvent, KeysHeard, Numbering, asked_of, key_after, now_in_stream, read_key,
        saying, write_key,
    };

    #[test]
    fn a_key_is_read_as_it_is_written() {
        for (number, key) in Key::ALL.into_iter().enumerate() {
            let written = write_key(key, false, 160);
            assert_eq!(written, [u8::try_from(number).unwrap(), 10, 0, 160]);
            assert_eq!(read_key(&written), Some(KeyEvent { key, ends: false }));
            let ended = write_key(key, true, 1_280);
            assert_eq!(ended[1..], [0x80 | 0x0a, 5, 0]);
            assert_eq!(read_key(&ended), Some(KeyEvent { key, ends: true }));
        }
        // An event that is no key, and a packet that is cut short.
        assert_eq!(read_key(&[16, 10, 0, 160]), None);
        assert_eq!(read_key(&[1, 10, 0]), None);
        // What follows the four bytes is another event of the same
        // packet, and is not read.
        let two = [1, 10, 0, 160, 2, 10, 0, 160];
        assert_eq!(read_key(&two).unwrap().key, Key::One);
    }

    #[test]
    fn audio_is_numbered_by_where_it_is_in_time() {
        let mut stream = Numbering::default();
        let start = 4_000_000_000_u32;
        // One frame after another, across the place where time wraps.
        for frame in 0..400_000_u32 {
            let time = start.wrapping_add(frame * 960);
            let number = stream.place(time, 960).unwrap();
            assert_eq!(u32::from(number), frame & 0xffff, "{frame}");
        }
        // Whatever came between two frames, the one that begins where
        // the last ended is the next: nothing here was told of the
        // sequence numbers the stream gave them.
        let mut stream = Numbering::default();
        assert_eq!(stream.place(0, 960), Some(0));
        assert_eq!(stream.place(960, 960), Some(1));
        // A hole in time is as many frames as fit in it.
        assert_eq!(stream.place(3 * 960, 960), Some(3));
        assert_eq!(stream.place(10 * 960, 960), Some(10));
        // The frame of the hole arrives after all, once.
        assert_eq!(stream.place(2 * 960, 960), Some(2));
        assert_eq!(stream.place(2 * 960, 960), None);
        // What was given is not given again, inside another packet or
        // by itself.
        for given in [0, 1, 3, 10] {
            assert_eq!(stream.place(given * 960, 960), None, "{given}");
        }
        assert_eq!(stream.place(9 * 960, 960), Some(9));
        // A frame that begins a little off its place is the frame of
        // that place.
        assert_eq!(stream.place(11 * 960 + 3, 960), Some(11));
        assert_eq!(stream.place(11 * 960 - 2, 960), None);
    }

    #[test]
    fn frames_of_another_length_are_numbered_one_by_one() {
        let mut stream = Numbering::default();
        assert_eq!(stream.place(0, 960), Some(0));
        // The sender goes over to 10 ms, and back.
        assert_eq!(stream.place(960, 480), Some(1));
        assert_eq!(stream.place(1_440, 480), Some(2));
        assert_eq!(stream.place(1_920, 960), Some(3));
        assert_eq!(stream.place(2_880, 960), Some(4));
        // And to 60 ms, of which one is lost.
        assert_eq!(stream.place(3_840, 2_880), Some(5));
        assert_eq!(stream.place(3_840 + 2 * 2_880, 2_880), Some(7));
        assert_eq!(stream.place(3_840 + 2_880, 2_880), Some(6));
        // A packet that says no length is taken as one that is as short
        // as can be, and is not divided by.
        let mut odd = Numbering::default();
        assert_eq!(odd.place(0, 0), Some(0));
        assert_eq!(odd.place(5, 0), Some(5));
    }

    #[test]
    fn a_frame_from_far_back_is_not_told_from_one_that_was_given() {
        let mut stream = Numbering::default();
        for frame in 0..200_u32 {
            stream.place(frame * 960, 960).unwrap();
        }
        // The last 63 before the newest are known, and none was missed.
        for back in 1..64 {
            assert_eq!(stream.place((199 - back) * 960, 960), None, "{back}");
        }
        // One that was missed that far back is still placed.
        assert_eq!(stream.place(263 * 960, 960), Some(263));
        assert_eq!(stream.place(201 * 960, 960), Some(201));
        assert_eq!(stream.place(200 * 960, 960), Some(200));
        // Further back, nothing is known, so nothing is given.
        assert_eq!(stream.place(199 * 960, 960), None);
        // A leap further on than is remembered forgets all of it.
        assert_eq!(stream.place(1_000 * 960, 960), Some(1_000));
        assert_eq!(stream.place(999 * 960, 960), Some(999));
        assert_eq!(stream.place(937 * 960, 960), Some(937));
        assert_eq!(stream.place(936 * 960, 960), None);
    }

    #[test]
    fn a_key_is_said_eight_times_and_let_go_of_three_times_over() {
        // On the clock of the audio, and on a telephone's.
        for (clock, every) in [(48_000, 960_u16), (8_000, 160)] {
            for said in 1..=8_u32 {
                let saying = saying(Key::Five, said, clock);
                let held = every * u16::try_from(said).unwrap();
                let ends = said == 8;
                assert_eq!(saying.payload, write_key(Key::Five, ends, held), "{said}");
                assert_eq!(saying.times, if ends { 3 } else { 1 }, "{said}");
                assert_eq!((saying.begins, saying.ends), (said == 1, ends), "{said}");
            }
        }
        // A key that is held for longer than its packet can say is said
        // to be held for as long as that.
        assert_eq!(saying(Key::Five, 80, 48_000).payload[2..], [0xff, 0xff]);
    }

    #[test]
    fn a_key_is_where_its_moment_is_on_the_clock_of_the_audio() {
        // A moment with a second before it and one after it.
        let now = Instant::now() + Duration::from_secs(1);
        let ms = Duration::from_millis;
        let ago = |time: u64| now.checked_sub(ms(time)).unwrap();
        // Nothing was sent: the stream is where it begins.
        assert_eq!(now_in_stream(5_000, None, now), 5_000);
        // The audio that was sent ended just now.
        assert_eq!(now_in_stream(5_000, Some(now), now), 5_000);
        // 200 ms of audio were sent ahead of their time: the key is at
        // their beginning, and not after them.
        assert_eq!(now_in_stream(15_000, Some(now + ms(200)), now), 5_400);
        // The audio ended a second ago: the key is a second later.
        assert_eq!(now_in_stream(5_000, Some(ago(1_000)), now), 53_000);
        // The clock wraps round, either way.
        assert_eq!(
            now_in_stream(100, Some(now + ms(10)), now),
            100_u32.wrapping_sub(480)
        );
        assert_eq!(now_in_stream(u32::MAX, Some(ago(10)), now), 479);
    }

    #[test]
    fn no_key_is_at_the_time_of_the_one_before_it_or_too_near_it() {
        // A key and its rest are 240 ms: 11,520 on the clock.
        assert_eq!(key_after(7_000, None), 7_000);
        assert_eq!(key_after(7_000, Some(7_000)), 18_520);
        assert_eq!(key_after(18_519, Some(7_000)), 18_520);
        assert_eq!(key_after(18_520, Some(7_000)), 18_520);
        assert_eq!(key_after(90_000, Some(7_000)), 90_000);
        // A moment that is before the key before it, because audio was
        // sent ahead: the key is after that one all the same.
        assert_eq!(key_after(1_000, Some(7_000)), 18_520);
        // Round the end of the clock.
        assert_eq!(key_after(5, Some(u32::MAX - 20)), 11_499);
        assert_eq!(key_after(20_000, Some(u32::MAX - 20)), 20_000);
    }

    #[test]
    fn a_key_is_heard_once_however_often_and_however_late_it_is_said() {
        let mut heard = KeysHeard::default();
        assert!(heard.is_new(40_000));
        // It is said again and again.
        assert!(!heard.is_new(40_000));
        assert!(!heard.is_new(40_000));
        // The next key.
        assert!(heard.is_new(51_520));
        // A packet of the one before, which comes late: it is no key
        // again, and the newest stays the newest.
        assert!(!heard.is_new(40_000));
        assert!(!heard.is_new(51_520));
        // Round the end of the clock.
        let mut heard = KeysHeard::default();
        assert!(heard.is_new(u32::MAX - 100));
        assert!(heard.is_new(11_000));
        assert!(!heard.is_new(u32::MAX - 100));
    }

    #[test]
    fn a_datagram_is_told_by_its_first_byte() {
        assert_eq!(Datagram::of(&[0, 1]), Datagram::Stun);
        assert_eq!(Datagram::of(&[3]), Datagram::Stun);
        assert_eq!(Datagram::of(&[20]), Datagram::Dtls);
        assert_eq!(Datagram::of(&[63]), Datagram::Dtls);
        assert_eq!(Datagram::of(&[128]), Datagram::Media);
        assert_eq!(Datagram::of(&[191]), Datagram::Media);
        for other in [&[][..], &[4], &[19], &[64], &[127], &[192], &[255]] {
            assert_eq!(Datagram::of(other), Datagram::Other, "{other:?}");
        }
    }

    /// A binding request with the attributes `attributes`, each a kind
    /// and a value.
    fn request(kind: [u8; 2], attributes: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in attributes {
            body.extend_from_slice(&name.to_be_bytes());
            body.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
            body.extend_from_slice(value);
            body.resize(body.len().div_ceil(4) * 4, 0);
        }
        let mut message = kind.to_vec();
        message.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
        message.extend_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        message.extend_from_slice(&[7; 12]);
        message.extend_from_slice(&body);
        message
    }

    #[test]
    fn a_check_names_the_call_it_is_for() {
        let named = request([0, 1], &[(0x0024, &[0, 0, 0, 1]), (0x0006, b"ours:theirs")]);
        assert_eq!(asked_of(&named), Some("ours"));
        // What comes before the name and has a length that is no
        // multiple of four is filled up, and the name is after that.
        let after = request([0, 1], &[(0x8022, b"agent"), (0x0006, b"ours:theirs")]);
        assert_eq!(asked_of(&after), Some("ours"));
        // A name whose length is no multiple of four is followed by
        // what fills it up, and by the next attribute.
        let odd = request([0, 1], &[(0x0006, b"abc:de"), (0x0008, &[1; 20])]);
        assert_eq!(asked_of(&odd), Some("abc"));
        // What is no request, names nobody, or is cut short.
        let answer = request([1, 1], &[(0x0006, b"ours:theirs")]);
        let nameless = request([0, 1], &[(0x0024, &[0, 0, 0, 1])]);
        let unsplit = request([0, 1], &[(0x0006, b"ours")]);
        let mut no_cookie = named.clone();
        no_cookie[4] = 0;
        for none in [&answer, &nameless, &unsplit, &no_cookie] {
            assert_eq!(asked_of(none), None);
        }
        // Cut anywhere before the end of the name, it names nobody.
        // What fills the name up to a multiple of four is not needed.
        assert_eq!(named.len(), 44);
        for cut in 0..43 {
            assert_eq!(asked_of(&named[..cut]), None, "{cut}");
        }
        assert_eq!(asked_of(&named[..43]), Some("ours"));
        assert_eq!(asked_of(&[0x80; 40]), None);
    }
}
