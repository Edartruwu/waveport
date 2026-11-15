//! A person who follows a script, in place of a sound card.
//!
//! The person has a microphone and a loudspeaker like anyone at a sound
//! card. What they say is written down beforehand, and what they are
//! played is kept. Nothing runs on a thread of its own: the person is
//! moved on by the clock whenever the session looks at its transport, so
//! a scripted session is the same every time it is run.

use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::time::{Instant, Sleep};
use waveport_adapter_kit::wav;
use waveport_media::rate::Rate;
use waveport_media::resample::Resampler;

use crate::line::{DeviceEnd, HZ};

/// How many samples the person's devices move on by at a time: 10 ms, as
/// a sound card does.
const TICK: usize = (HZ / 100) as usize;

/// How long a tick lasts.
const TICK_TIME: Duration = Duration::from_millis(10);

/// One thing a scripted person does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Says nothing for this long.
    Silence(Duration),
    /// Says this: samples at 48 kHz.
    Speech(Vec<i16>),
    /// Says nothing until the agent has spoken and has then been silent
    /// for `quiet`, or until `at_most` has passed, whichever is first.
    Listen {
        /// How long the agent has to have been silent.
        quiet: Duration,
        /// The longest the person listens.
        at_most: Duration,
    },
}

impl Step {
    /// What a WAV file holds, said.
    ///
    /// # Errors
    ///
    /// The file's audio is at a rate other than 8, 16, 24 or 48 kHz.
    pub fn speech(file: &wav::Wav) -> Result<Self, UnknownRate> {
        let from = Rate::from_hz(file.hz).ok_or(UnknownRate(file.hz))?;
        let mut resampler = Resampler::new(from, Rate::Hz48000);
        // The filter gives each sample only once it has seen what follows
        // it: silence is put after the speech, so that its end comes out.
        let tail = from.samples_in(resampler.delay()) + 1;
        let mut input = file.samples.clone();
        input.resize(input.len() + tail, 0);
        let mut out = vec![0; resampler.yields(input.len())];
        let written = resampler.process(&input, &mut out).unwrap_or(0);
        out.truncate(written);
        Ok(Self::Speech(out))
    }
}

/// A WAV file holds audio at a rate the project does not work at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownRate(pub u32);

impl core::fmt::Display for UnknownRate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "audio at {} Hz: only 8,000, 16,000, 24,000 and 48,000 Hz are read",
            self.0
        )
    }
}

impl core::error::Error for UnknownRate {}

/// What a scripted person's loudspeaker played, from the moment the
/// session first looked at its transport: the agent's side of the call,
/// with the silences in it.
#[derive(Clone, Debug, Default)]
pub struct Recording {
    samples: Arc<Mutex<Vec<i16>>>,
}

impl Recording {
    /// The samples so far, at 48 kHz.
    #[must_use]
    pub fn samples(&self) -> Vec<i16> {
        self.lock().clone()
    }

    /// The recording so far, as a WAV file.
    #[must_use]
    pub fn wav(&self) -> Vec<u8> {
        wav::write(HZ, &self.lock())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<i16>> {
        self.samples.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The clock a person is moved on by. It starts when the session first
/// looks at the transport, on the session's own thread: a timer belongs
/// to the executor it was made on.
#[derive(Debug)]
struct Clock {
    start: Instant,
    timer: Pin<Box<Sleep>>,
}

/// A scripted person and their devices.
#[derive(Debug)]
pub(crate) struct Person {
    device: DeviceEnd,
    steps: VecDeque<Step>,
    /// How many samples of the step in hand are done.
    done: usize,
    /// Whether the agent has spoken since the person began to listen, and
    /// for how many samples it has been silent since.
    spoke: bool,
    quiet: usize,
    clock: Option<Clock>,
    /// How many ticks have been done.
    ticks: u64,
    recording: Recording,
    /// Whether the script has run out: the person has hung up.
    gone: bool,
}

/// How many samples last as long as `time`.
fn samples_in(time: Duration) -> usize {
    let samples = time.as_micros() * u128::from(HZ) / 1_000_000;
    usize::try_from(samples).unwrap_or(usize::MAX)
}

impl Person {
    pub(crate) fn new(device: DeviceEnd, steps: Vec<Step>) -> (Self, Recording) {
        let recording = Recording::default();
        let person = Self {
            device,
            steps: steps.into(),
            done: 0,
            spoke: false,
            quiet: 0,
            clock: None,
            ticks: 0,
            recording: recording.clone(),
            gone: false,
        };
        (person, recording)
    }

    /// Whether the person has hung up.
    pub(crate) const fn gone(&self) -> bool {
        self.gone
    }

    /// Moves the person on to now, and leaves `cx` to be woken when there
    /// is more for them to do. Returns whether anything was done.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> bool {
        let now = Instant::now();
        let clock = self.clock.get_or_insert_with(|| Clock {
            start: now,
            timer: Box::pin(tokio::time::sleep_until(now)),
        });
        let start = clock.start;
        let due = now.saturating_duration_since(start).as_micros() / TICK_TIME.as_micros();
        let due = u64::try_from(due).unwrap_or(u64::MAX);
        let mut moved = false;
        while self.ticks < due && !self.gone {
            self.tick();
            moved = true;
        }
        if self.gone {
            return moved;
        }
        // Until the next tick is due. If that is now, the caller is told
        // that something was done, and looks again.
        let next = u32::try_from(self.ticks + 1).unwrap_or(u32::MAX);
        if let Some(clock) = &mut self.clock {
            clock.timer.as_mut().reset(start + TICK_TIME * next);
            if clock.timer.as_mut().poll(cx) == Poll::Ready(()) {
                return true;
            }
        }
        moved
    }

    /// Ten milliseconds of the person's life: what the loudspeaker plays
    /// is kept, and what the person says goes to the microphone.
    fn tick(&mut self) {
        self.ticks += 1;
        let mut played = [0_i16; TICK];
        let audio = self.device.speaker.fill(&mut played);
        self.recording.lock().extend_from_slice(&played);

        let mut said = [0_i16; TICK];
        let mut filled = 0;
        while filled < TICK {
            let room = said.get_mut(filled..).unwrap_or_default();
            let Some(step) = self.steps.front() else {
                self.gone = true;
                return;
            };
            let over = match step {
                Step::Silence(time) => {
                    let all = samples_in(*time);
                    let take = room.len().min(all.saturating_sub(self.done));
                    self.done += take;
                    filled += take;
                    self.done >= all
                }
                Step::Speech(speech) => {
                    let rest = speech.get(self.done..).unwrap_or_default();
                    let take = room.len().min(rest.len());
                    for (place, sample) in room.iter_mut().zip(rest) {
                        *place = *sample;
                    }
                    self.done += take;
                    filled += take;
                    self.done >= speech.len()
                }
                Step::Listen { quiet, at_most } => {
                    if audio > 0 {
                        self.spoke = true;
                        self.quiet = 0;
                    } else if self.spoke {
                        self.quiet += TICK;
                    }
                    self.done += TICK;
                    filled = TICK;
                    (self.spoke && self.quiet >= samples_in(*quiet))
                        || self.done >= samples_in(*at_most)
                }
            };
            if over {
                self.steps.pop_front();
                self.done = 0;
                self.spoke = false;
                self.quiet = 0;
            }
        }
        self.device.mic.push(said.iter().copied());
    }
}
