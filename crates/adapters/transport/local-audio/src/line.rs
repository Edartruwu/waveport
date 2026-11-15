//! The line between a session and a sound device.
//!
//! A sound device asks for audio and hands audio over on a thread of its
//! own, at moments it chooses, and must never be kept waiting. So nothing
//! is locked between the two sides: samples cross in two rings, one each
//! way, and the little the sides have to tell each other besides is held
//! in single numbers that are read and written whole.
//!
//! Audio on its way to the loudspeaker is in *runs*: the frames of one
//! response, one after the other. The transport numbers the runs from 1,
//! and every sample carries the number of its run, so that the device's
//! side can say how much of which run it has played without knowing what
//! a response is.
//!
//! The device's side of the line is the same whatever the device: a sound
//! card calls it from its own thread, and a scripted device calls it from
//! the session's, by the clock.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::task::Waker;
use std::sync::{Arc, Mutex, PoisonError};

use rtrb::{Consumer, Producer, RingBuffer};

/// The rate of the audio on the line, both ways.
pub(crate) const HZ: u32 = 48_000;

/// How many samples either ring holds: a second of audio.
const ROOM: usize = HZ as usize;

/// How much of a run has to be waiting, beyond what the device asks for
/// at once, before the device begins to play it: a frame, because that is
/// how audio arrives, and a little for a frame that arrives late.
const CUSHION: usize = (HZ as usize / 1_000) * 25;

/// How many samples a run that is cut off takes to fall silent: 5 ms.
const FADE: usize = 240;

/// [`FADE`], for arithmetic on samples.
const FADE_STEPS: i32 = 240;

/// What the count of a discarded run holds when the device had gone on to a later run
/// before it was told to discard this one.
const PASSED: u32 = u32::MAX;

/// A sample on its way to the loudspeaker.
#[derive(Clone, Copy, Debug)]
struct Slot {
    run: u32,
    sample: i16,
}

/// A run, and a number of its samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Progress {
    /// The run. 0 is no run.
    pub run: u32,
    /// How many of its samples.
    pub played: u32,
}

impl Progress {
    const fn pack(self) -> u64 {
        ((self.run as u64) << 32) | self.played as u64
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "the two halves of the number are taken apart"
    )]
    const fn unpack(packed: u64) -> Self {
        Self {
            run: (packed >> 32) as u32,
            played: packed as u32,
        }
    }
}

/// How far a run had been played when it was discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cut {
    /// This many of its samples had been played.
    At(u32),
    /// The device had gone on to a later run: as much of this one was
    /// played as was ever going to be.
    Passed,
}

/// What the two sides of a line tell each other besides samples.
#[derive(Debug, Default)]
struct Shared {
    /// The run the loudspeaker is playing, and how much of it has been
    /// played.
    playing: AtomicU64,
    /// The newest run to be discarded, with every run before it.
    discard: AtomicU32,
    /// The newest run that was discarded, and how much of it had been
    /// played by then.
    cut: AtomicU64,
    /// The newest run whose last sample is on the line.
    ended: AtomicU32,
    /// How many samples of the microphone found no room on the line.
    lost: AtomicU64,
    /// How many times the device said that it had fallen behind.
    stumbles: AtomicU64,
    /// Whether the device has failed.
    failed: AtomicBool,
    /// Why, in the device's words.
    failure: Mutex<Option<String>>,
    /// Whoever waits to be told that something has changed.
    waker: Mutex<Option<Waker>>,
}

impl Shared {
    /// Tells whoever waits that something has changed. This is called on
    /// the device's thread, which does not wait for the lock: if the
    /// session is leaving its waker just now, it looks at the line again
    /// once it has, and finds what this would have told it.
    fn wake(&self) {
        if let Ok(mut waker) = self.waker.try_lock()
            && let Some(waker) = waker.take()
        {
            waker.wake();
        }
    }
}

/// A look at how a line's device is doing, for whoever started it.
#[derive(Clone, Debug)]
pub struct Health {
    shared: Arc<Shared>,
}

impl Health {
    /// Why the device failed, if it has. A device that has failed says
    /// nothing more and plays nothing more, and its transport is closed.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        if !self.shared.failed.load(Ordering::Acquire) {
            return None;
        }
        let failure = self
            .shared
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Some(failure.clone().unwrap_or_default())
    }

    /// How many times the device said that it had fallen behind: audio
    /// was asked for and not there in time, or heard and not taken in
    /// time. Each is a click, or a moment lost.
    #[must_use]
    pub fn stumbles(&self) -> u64 {
        self.shared.stumbles.load(Ordering::Relaxed)
    }

    /// How many samples of the microphone were lost because the session
    /// did not take what was heard for a second or more.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.shared.lost.load(Ordering::Relaxed)
    }
}

/// The device's side of the way to the loudspeaker.
#[derive(Debug)]
pub(crate) struct Speaker {
    slots: Consumer<Slot>,
    shared: Arc<Shared>,
    /// The run being played, and how much of it has been.
    at: Progress,
    /// The newest run that was discarded here.
    discarded: u32,
    /// Whether nothing is being played. What comes next is then not
    /// begun until enough of it is waiting.
    idle: bool,
}

impl Speaker {
    /// Fills `out` with what is to be played next, and with silence where
    /// there is nothing. Returns how many of the samples are audio of a
    /// response.
    pub(crate) fn fill(&mut self, out: &mut [i16]) -> usize {
        let mut places = out.iter_mut();
        let mut written = self.discard(&mut places);

        if self.idle
            && let Ok(first) = self.slots.peek()
        {
            let whole = self.shared.ended.load(Ordering::Acquire) >= first.run;
            let enough = (places.len() + CUSHION).min(ROOM / 2);
            if whole || self.slots.slots() >= enough {
                self.idle = false;
            }
        }

        if !self.idle {
            for place in places.by_ref() {
                let Ok(slot) = self.slots.pop() else {
                    // Nothing more is waiting: the run is over, or the
                    // rest of it is late.
                    self.idle = true;
                    *place = 0;
                    break;
                };
                if slot.run != self.at.run {
                    self.at = Progress {
                        run: slot.run,
                        played: 0,
                    };
                }
                self.at.played = self.at.played.saturating_add(1);
                *place = slot.sample;
                written += 1;
            }
        }
        for place in places {
            *place = 0;
        }

        self.shared.playing.store(self.at.pack(), Ordering::Release);
        self.shared.wake();
        written
    }

    /// Lets go of what it was told to discard, and says how far the run
    /// had been played. Returns how many samples it wrote to `places` on
    /// the way.
    ///
    /// What is being played does not stop dead, which is heard as a
    /// click: a few milliseconds more of it are played, each sample
    /// quieter than the one before.
    fn discard(&mut self, places: &mut core::slice::IterMut<'_, i16>) -> usize {
        let discard = self.shared.discard.load(Ordering::Acquire);
        if discard == self.discarded {
            return 0;
        }
        self.discarded = discard;
        if self.at.run > discard {
            // It was played, as far as it was going to be, before anyone
            // said to discard it. Nothing of it is here any more.
            let cut = Progress {
                run: discard,
                played: PASSED,
            };
            self.shared.cut.store(cut.pack(), Ordering::Release);
            return 0;
        }

        let mut faded = 0;
        if !self.idle {
            for place in places.by_ref().take(FADE) {
                *place = 0;
                let Ok(slot) = self.slots.peek() else {
                    break;
                };
                if slot.run != self.at.run {
                    break;
                }
                let left = i32::try_from(FADE - faded).unwrap_or(0);
                let quieter = i32::from(slot.sample) * left / FADE_STEPS;
                *place = i16::try_from(quieter).unwrap_or(0);
                let _ = self.slots.pop();
                self.at.played = self.at.played.saturating_add(1);
                faded += 1;
            }
        }
        while self.slots.peek().is_ok_and(|slot| slot.run <= discard) {
            let _ = self.slots.pop();
        }
        self.idle = true;
        let cut = Progress {
            run: discard,
            played: if self.at.run == discard {
                self.at.played
            } else {
                0
            },
        };
        self.shared.cut.store(cut.pack(), Ordering::Release);
        faded
    }
}

/// The device's side of the way from the microphone.
#[derive(Debug)]
pub(crate) struct Mic {
    samples: Producer<i16>,
    shared: Arc<Shared>,
}

impl Mic {
    /// Takes what the microphone heard. What there is no room for is
    /// lost, all of it, and counted.
    pub(crate) fn push(&mut self, heard: impl ExactSizeIterator<Item = i16>) {
        let length = heard.len();
        if length == 0 {
            return;
        }
        match self.samples.write_chunk_uninit(length) {
            Ok(chunk) => {
                chunk.fill_from_iter(heard);
            }
            Err(_) => {
                self.shared.lost.fetch_add(length as u64, Ordering::Relaxed);
            }
        }
        self.shared.wake();
    }
}

/// What a device does when it cannot go on.
#[derive(Clone, Debug)]
pub(crate) struct Alarm {
    shared: Arc<Shared>,
}

impl Alarm {
    /// The device has failed, and says why.
    pub(crate) fn fail(&self, why: String) {
        {
            let mut failure = self
                .shared
                .failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            failure.get_or_insert(why);
        }
        self.shared.failed.store(true, Ordering::Release);
        // Not the device's audio thread, and the session has to hear of
        // this: the lock is waited for.
        let waker = self
            .shared
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// The device fell behind, and goes on.
    pub(crate) fn stumbled(&self) {
        self.shared.stumbles.fetch_add(1, Ordering::Relaxed);
    }
}

/// The session's side of a line.
#[derive(Debug)]
pub(crate) struct Line {
    to_speaker: Producer<Slot>,
    from_mic: Consumer<i16>,
    shared: Arc<Shared>,
    /// How many of the lost samples have been accounted for.
    lost_seen: u64,
}

/// The device's side of a line.
#[derive(Debug)]
pub(crate) struct DeviceEnd {
    pub mic: Mic,
    pub speaker: Speaker,
    pub alarm: Alarm,
}

/// A line, as its two sides.
pub(crate) fn line() -> (Line, DeviceEnd) {
    let shared = Arc::new(Shared::default());
    let (to_speaker, slots) = RingBuffer::new(ROOM);
    let (samples, from_mic) = RingBuffer::new(ROOM);
    let line = Line {
        to_speaker,
        from_mic,
        shared: Arc::clone(&shared),
        lost_seen: 0,
    };
    let device = DeviceEnd {
        mic: Mic {
            samples,
            shared: Arc::clone(&shared),
        },
        speaker: Speaker {
            slots,
            shared: Arc::clone(&shared),
            at: Progress::default(),
            discarded: 0,
            idle: true,
        },
        alarm: Alarm { shared },
    };
    (line, device)
}

impl Line {
    /// A look at how the device is doing.
    pub(crate) fn health(&self) -> Health {
        Health {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Whether the device has failed.
    pub(crate) fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    /// Whether there is room for `samples` more on the way to the
    /// loudspeaker.
    pub(crate) fn has_room(&self, samples: usize) -> bool {
        self.to_speaker.slots() >= samples
    }

    /// Puts audio of `run` on the way to the loudspeaker. Returns whether
    /// there was room: if not, none of it was taken.
    pub(crate) fn play(&mut self, run: u32, audio: impl ExactSizeIterator<Item = i16>) -> bool {
        let Ok(chunk) = self.to_speaker.write_chunk_uninit(audio.len()) else {
            return false;
        };
        chunk.fill_from_iter(audio.map(|sample| Slot { run, sample }));
        true
    }

    /// The last sample of `run` is on the line.
    pub(crate) fn end(&self, run: u32) {
        self.shared.ended.fetch_max(run, Ordering::AcqRel);
    }

    /// What is on the line of `run` and of the runs before it is not to
    /// be played.
    pub(crate) fn discard(&self, run: u32) {
        self.shared.discard.fetch_max(run, Ordering::AcqRel);
    }

    /// The run the loudspeaker is playing, and how much of it has been
    /// played.
    pub(crate) fn playing(&self) -> Progress {
        Progress::unpack(self.shared.playing.load(Ordering::Acquire))
    }

    /// How far `run` had been played when it was discarded, once the
    /// device has discarded it.
    pub(crate) fn cut(&self, run: u32) -> Option<Cut> {
        let cut = Progress::unpack(self.shared.cut.load(Ordering::Acquire));
        match cut.run.cmp(&run) {
            core::cmp::Ordering::Less => None,
            core::cmp::Ordering::Equal if cut.played != PASSED => Some(Cut::At(cut.played)),
            // A later run was discarded too, and this one with it, before
            // anyone asked how far it had got.
            core::cmp::Ordering::Equal | core::cmp::Ordering::Greater => Some(Cut::Passed),
        }
    }

    /// How many samples of the microphone are waiting.
    #[cfg(test)]
    pub(crate) fn heard(&self) -> usize {
        self.from_mic.slots()
    }

    /// Takes what the microphone heard, as much as fills `out`. Returns
    /// whether there was that much: if not, nothing was taken.
    pub(crate) fn hear(&mut self, out: &mut [i16]) -> bool {
        let Ok(chunk) = self.from_mic.read_chunk(out.len()) else {
            return false;
        };
        let (first, second) = chunk.as_slices();
        let (head, tail) = out.split_at_mut(first.len());
        head.copy_from_slice(first);
        tail.copy_from_slice(second);
        chunk.commit_all();
        true
    }

    /// How many samples of the microphone were lost since this was last
    /// asked.
    pub(crate) fn lost(&mut self) -> u64 {
        let lost = self.shared.lost.load(Ordering::Relaxed);
        let new = lost.saturating_sub(self.lost_seen);
        self.lost_seen = lost;
        new
    }

    /// Leaves `waker` to be woken when the device has played or heard
    /// something, or has failed. Whoever leaves one looks at the line
    /// again afterwards: the device may have had something to say while
    /// the waker was being left.
    pub(crate) fn wake_me(&self, waker: &Waker) {
        let mut slot = self
            .shared
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &mut *slot {
            Some(old) if old.will_wake(waker) => {}
            slot => *slot = Some(waker.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CUSHION, Cut, DeviceEnd, Line, PASSED, Progress, line};

    fn tone(length: usize) -> impl ExactSizeIterator<Item = i16> {
        (0..length).map(|n| i16::try_from(n % 1_000).unwrap() + 1)
    }

    /// A line whose device asks for 480 samples at a time.
    fn lines() -> (Line, DeviceEnd, [i16; 480]) {
        let (line, device) = line();
        (line, device, [0; 480])
    }

    #[test]
    fn a_number_and_its_halves() {
        let progress = Progress {
            run: 7,
            played: 123_456,
        };
        assert_eq!(Progress::unpack(progress.pack()), progress);
        let most = Progress {
            run: u32::MAX,
            played: PASSED,
        };
        assert_eq!(Progress::unpack(most.pack()), most);
    }

    #[test]
    fn a_run_is_not_begun_until_enough_of_it_waits() {
        let (mut line, mut device, mut out) = lines();
        assert!(line.play(1, tone(960)));
        // 20 ms wait, and the device asks for 10 ms: a frame that came
        // late would leave it with nothing.
        assert_eq!(device.speaker.fill(&mut out), 0);
        assert_eq!(out, [0; 480]);
        assert_eq!(line.playing(), Progress::default());

        assert!(line.play(1, tone(960)));
        const { assert!(1_920 >= 480 + CUSHION) };
        assert_eq!(device.speaker.fill(&mut out), 480);
        assert_eq!(out[..3], [1, 2, 3]);
        assert_eq!(
            line.playing(),
            Progress {
                run: 1,
                played: 480
            }
        );
    }

    #[test]
    fn a_run_that_is_all_there_is_played_at_once_and_to_its_end() {
        let (mut line, mut device, mut out) = lines();
        assert!(line.play(1, tone(700)));
        line.end(1);
        assert_eq!(device.speaker.fill(&mut out), 480);
        // The rest, and then silence.
        assert_eq!(device.speaker.fill(&mut out), 220);
        assert_eq!(out[219], 700);
        assert_eq!(out[220..], [0; 260]);
        assert_eq!(
            line.playing(),
            Progress {
                run: 1,
                played: 700
            }
        );
        assert_eq!(device.speaker.fill(&mut out), 0);
    }

    #[test]
    fn what_is_discarded_is_not_played_and_the_cut_is_known() {
        let (mut line, mut device, mut out) = lines();
        assert!(line.play(1, tone(4_800)));
        assert_eq!(device.speaker.fill(&mut out), 480);
        assert_eq!(line.cut(1), None);

        line.discard(1);
        // Asked for, and not yet done.
        assert_eq!(line.cut(1), None);
        // It falls silent over 5 ms, and the rest is let go of.
        assert_eq!(device.speaker.fill(&mut out), 240);
        assert_eq!(out[0], 481);
        assert!(out[239] != 0 && out[239].abs() < 8);
        assert_eq!(out[240..], [0; 240]);
        assert_eq!(line.cut(1), Some(Cut::At(720)));
        assert!(line.has_room(48_000));

        // The next run plays as if the first had never been.
        assert!(line.play(2, tone(300)));
        line.end(2);
        assert_eq!(device.speaker.fill(&mut out), 300);
        assert_eq!(
            line.playing(),
            Progress {
                run: 2,
                played: 300
            }
        );
        // It was not discarded, and nobody is told that it was.
        assert_eq!(line.cut(2), None);
    }

    #[test]
    fn a_run_discarded_before_it_was_begun_was_not_heard_at_all() {
        let (mut line, mut device, mut out) = lines();
        assert!(line.play(1, tone(960)));
        line.discard(1);
        assert_eq!(device.speaker.fill(&mut out), 0);
        assert_eq!(line.cut(1), Some(Cut::At(0)));
    }

    #[test]
    fn a_run_the_device_has_left_behind_is_said_to_be() {
        let (mut line, mut device, mut out) = lines();
        assert!(line.play(1, tone(200)));
        line.end(1);
        assert!(line.play(2, tone(2_000)));
        assert_eq!(device.speaker.fill(&mut out), 480);
        assert_eq!(line.playing().run, 2);
        // The first is discarded only now. The second plays on.
        line.discard(1);
        assert_eq!(device.speaker.fill(&mut out), 480);
        assert_eq!(line.cut(1), Some(Cut::Passed));
        assert_eq!(line.playing().played, 760);
        line.discard(2);
        assert_eq!(device.speaker.fill(&mut out), 240);
        assert_eq!(line.cut(2), Some(Cut::At(1_000)));
        // Whoever asks about the first after that is told the same.
        assert_eq!(line.cut(1), Some(Cut::Passed));
    }

    #[test]
    fn what_the_microphone_hears_arrives_whole_or_is_counted() {
        let (mut line, mut device, _) = lines();
        device.mic.push(tone(500));
        let mut frame = [0; 960];
        assert!(!line.hear(&mut frame));
        assert_eq!(line.heard(), 500);
        device.mic.push(tone(500));
        assert!(line.hear(&mut frame));
        assert_eq!(frame[..2], [1, 2]);
        assert_eq!(frame[500..502], [1, 2]);
        assert_eq!(line.heard(), 40);

        // A second and more that nobody takes: what does not fit is lost.
        device.mic.push(tone(47_000));
        device.mic.push(tone(2_000));
        assert_eq!(line.lost(), 2_000);
        assert_eq!(line.lost(), 0);
        assert_eq!(line.health().lost(), 2_000);
    }

    #[test]
    fn a_device_that_fails_says_why_once() {
        let (line, device, _) = lines();
        let health = line.health();
        assert_eq!(health.failure(), None);
        assert!(!line.failed());
        device.alarm.stumbled();
        device.alarm.stumbled();
        assert_eq!(health.stumbles(), 2);
        device.alarm.fail("unplugged".to_owned());
        device.alarm.fail("and again".to_owned());
        assert!(line.failed());
        assert_eq!(health.failure().as_deref(), Some("unplugged"));
    }

    #[test]
    fn the_two_sides_agree_across_threads() {
        // A device on a thread of its own, asking for odd amounts, while
        // runs are sent and every other one is discarded part way.
        let (mut line, mut device) = line();
        let stop = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
        let stopped = std::sync::Arc::clone(&stop);
        let device_thread = std::thread::spawn(move || {
            let mut out = [0_i16; 331];
            let mut audio = 0_u64;
            while !stopped.load(core::sync::atomic::Ordering::Acquire) {
                audio += device.speaker.fill(&mut out) as u64;
                std::thread::yield_now();
            }
            audio
        });

        let mut heard = 0_u64;
        for run in 1..=200_u32 {
            let length = 500 + (run as usize * 37) % 900;
            while !line.play(run, tone(length)) {
                std::thread::yield_now();
            }
            if run % 2 == 0 {
                line.discard(run);
                let cut = loop {
                    if let Some(cut) = line.cut(run) {
                        break cut;
                    }
                    std::thread::yield_now();
                };
                match cut {
                    Cut::At(played) => {
                        assert!(played as usize <= length);
                        heard += u64::from(played);
                    }
                    Cut::Passed => panic!("nothing was sent after run {run}"),
                }
            } else {
                line.end(run);
                // Played to its end, and said to be.
                let all = Progress {
                    run,
                    played: u32::try_from(length).unwrap(),
                };
                while line.playing() != all {
                    std::thread::yield_now();
                }
                heard += length as u64;
            }
        }
        stop.store(true, core::sync::atomic::Ordering::Release);
        // What the device played is what it said it played.
        assert_eq!(device_thread.join().unwrap(), heard);
    }
}
