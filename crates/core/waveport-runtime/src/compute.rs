//! The compute plane: the models that listen, shared by every session.
//!
//! A model that says whether someone is speaking is asked 31 times a second
//! for every stream of audio. One instance serves all streams, and what it
//! is asked is batched: every round takes one frame from each stream that
//! has one ready, as many as the model takes at once, and judges them in
//! one call. A round starts as soon as there is a frame, so batches are
//! small when there is little to do and grow as there is more.
//!
//! The models that judge a whole turn (has the speaker finished, do they
//! want the floor) are slower and are asked far less often. They have a
//! thread of their own, so that a turn being judged does not hold up the
//! frames of everyone else.
//!
//! A session's side of this is a [`Listener`], one for each stream. It cuts
//! the audio it is given into the frames the model wants, hands them over
//! through a ring that neither side ever waits on, and is told the answers
//! through another. When the ring is full the frame is dropped and counted:
//! audio is never queued behind a model that is slow.
//!
//! A compute plane runs on threads of its own ([`Compute::spawn`]) or, for
//! tests that have to be the same on every run, inside the calls that give
//! it work ([`Compute::inline`]). Both run the same rounds.

use core::fmt;
use core::task::{Context, Poll};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use futures_util::task::AtomicWaker;
use rtrb::{Consumer, Producer, RingBuffer};
use waveport_domain::event::OverlapVerdict;
use waveport_domain::ids::{Prob, TurnId};
use waveport_domain::time::{MediaDur, MediaTime};
use waveport_ports::audio::Rate;
use waveport_ports::model::{InterruptionClassifier, TurnModel, VadModel};

/// How many frames of a stream wait for the model at most. With frames of
/// 32 ms this is a little over 100 ms of audio.
const FRAMES_HELD: usize = 4;

/// How many answers wait for a session at most.
const ANSWERS_HELD: usize = 64;

/// How long a thread with nothing to do sleeps before it looks again. A
/// frame that is handed over wakes it at once; this is for the wake-up
/// that is never needed.
const IDLE: Duration = Duration::from_millis(5);

/// What a model said about a stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sensed {
    /// How likely it is that a frame holds speech.
    Speech {
        /// Where the frame begins on the media clock.
        at: MediaTime,
        /// How long the frame is.
        length: MediaDur,
        /// The likelihood, from 0 to 1.
        p: f32,
    },
    /// How likely it is that the speaker of a turn has finished.
    Eou {
        /// The turn.
        turn: TurnId,
        /// The likelihood.
        p: Prob,
    },
    /// What speech over the agent's own was.
    Overlap {
        /// The turn the speech belongs to.
        turn: TurnId,
        /// What it was.
        verdict: OverlapVerdict,
    },
}

/// Counts of what a compute plane has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComputeStats {
    /// Rounds of the model that says whether someone is speaking.
    pub rounds: u64,
    /// Frames it judged.
    pub frames: u64,
    /// Frames dropped because the model had not caught up.
    pub dropped: u64,
    /// Turns judged.
    pub turns: u64,
    /// Calls to a model that failed.
    pub failures: u64,
}

/// What the models want to be given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wants {
    /// The rate of the audio the models take.
    pub rate: Rate,
    /// The samples in one frame of the model that says whether someone is
    /// speaking.
    pub frame: usize,
    /// The most audio, in samples, that the model that judges the end of
    /// a turn looks at.
    pub turn_window: usize,
    /// The same for the model that judges speech over the agent's, if
    /// there is one.
    pub overlap_window: Option<usize>,
}

/// The models could not be put together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputeError {
    /// The models do not take audio at one rate.
    Rates,
    /// A model takes no frames, or frames of no length.
    Empty,
    /// A thread could not be started.
    Thread,
}

impl fmt::Display for ComputeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Rates => "the listening models take audio at different rates",
            Self::Empty => "a listening model takes no audio",
            Self::Thread => "a thread for the listening models could not be started",
        })
    }
}

impl core::error::Error for ComputeError {}

/// A frame handed over: where it begins. Its samples are in the ring
/// beside it.
#[derive(Clone, Copy, Debug)]
struct Head {
    at: MediaTime,
}

/// What a listener and the plane both hold of a stream.
#[derive(Debug, Default)]
struct Link {
    /// Wakes the session when there is an answer.
    waker: AtomicWaker,
    /// Answers about whole turns. They come from another thread than the
    /// answers about frames, and there are few of them.
    late: Mutex<Vec<Sensed>>,
}

/// The plane's end of a stream.
struct Stream {
    heads: Consumer<Head>,
    samples: Consumer<i16>,
    answers: Producer<Sensed>,
    link: Arc<Link>,
}

/// A turn to be judged.
struct Job {
    link: Arc<Link>,
    turn: TurnId,
    what: Asked,
    audio: Vec<i16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    Eou,
    Overlap,
}

/// The rounds of the model that says whether someone is speaking, with
/// the model's own type hidden.
trait FrameLane: Send {
    fn admit(&mut self, stream: Stream);
    /// Judges one frame of every stream that has one, as many as the
    /// model takes. Returns how many frames were judged.
    fn round(&mut self, counters: &Counters) -> usize;
}

struct Frames<V: VadModel> {
    model: V,
    frame: usize,
    length: MediaDur,
    streams: Vec<(Stream, V::State)>,
    /// Where the next round starts looking, so that no stream is always
    /// last.
    next: usize,
    /// The frames of a round, one after the other.
    audio: Vec<i16>,
    states: Vec<V::State>,
    /// Which stream each frame of the round is of, and where it begins.
    picked: Vec<(usize, MediaTime)>,
    out: Vec<f32>,
}

impl<V: VadModel> Frames<V> {
    fn new(model: V) -> Self {
        let frame = model.frame();
        let batch = model.max_batch();
        Self {
            length: model.rate().length_of(frame),
            model,
            frame,
            streams: Vec::new(),
            next: 0,
            audio: vec![0; frame * batch],
            states: Vec::with_capacity(batch),
            picked: Vec::with_capacity(batch),
            out: vec![0.0; batch],
        }
    }
}

impl<V: VadModel> FrameLane for Frames<V> {
    fn admit(&mut self, stream: Stream) {
        self.streams.push((stream, V::State::default()));
    }

    fn round(&mut self, counters: &Counters) -> usize {
        // A stream whose listener is gone is forgotten.
        self.streams
            .retain(|(stream, _)| !stream.heads.is_abandoned());
        let count = self.streams.len();
        let batch = self.model.max_batch();
        self.picked.clear();
        self.states.clear();
        for step in 0..count {
            if self.picked.len() == batch {
                break;
            }
            let index = (self.next + step) % count;
            let Some((stream, state)) = self.streams.get_mut(index) else {
                continue;
            };
            let Ok(head) = stream.heads.pop() else {
                continue;
            };
            let from = self.picked.len() * self.frame;
            let Some(room) = self.audio.get_mut(from..from + self.frame) else {
                continue;
            };
            // A head is written after its samples, so they are there.
            if stream.samples.pop_entire_slice(room).is_err() {
                continue;
            }
            self.picked.push((index, head.at));
            self.states.push(core::mem::take(state));
        }
        if count > 0 {
            self.next = (self.next + 1) % count;
        }
        let judged = self.picked.len();
        if judged == 0 {
            return 0;
        }

        let frames: Vec<&[i16]> = self.audio.chunks_exact(self.frame).take(judged).collect();
        let out = self.out.get_mut(..judged).unwrap_or_default();
        let answered = self.model.infer(&frames, &mut self.states, out).is_ok();
        counters.rounds.fetch_add(1, Ordering::Relaxed);
        if answered {
            counters.frames.fetch_add(judged as u64, Ordering::Relaxed);
        } else {
            counters.failures.fetch_add(1, Ordering::Relaxed);
        }
        // The states go back whether or not there is an answer: a model
        // that failed left them as they were.
        let states = self.states.drain(..);
        for (((index, at), state), p) in self.picked.iter().zip(states).zip(out.iter()) {
            let Some((stream, kept)) = self.streams.get_mut(*index) else {
                continue;
            };
            *kept = state;
            if answered {
                let answer = Sensed::Speech {
                    at: *at,
                    length: self.length,
                    p: *p,
                };
                // A session that does not take its answers loses them.
                let _ = stream.answers.push(answer);
                stream.link.waker.wake();
            }
        }
        judged
    }
}

/// The models that judge a whole turn.
struct Turns {
    turn: Box<dyn TurnModel>,
    overlap: Option<Box<dyn InterruptionClassifier>>,
}

impl Turns {
    /// Judges the jobs, which are emptied.
    fn judge(&mut self, jobs: &mut Vec<Job>, counters: &Counters) {
        let mut probabilities = vec![0.0_f32; jobs.len()];
        let mut verdicts = vec![OverlapVerdict::Noise; jobs.len()];
        for what in [Asked::Eou, Asked::Overlap] {
            let asked: Vec<&Job> = jobs.iter().filter(|job| job.what == what).collect();
            let most = match what {
                Asked::Eou => self.turn.max_batch(),
                // A classifier is asked about one speaker at a time.
                Asked::Overlap => 1,
            }
            .max(1);
            for group in asked.chunks(most) {
                let audio: Vec<&[i16]> = group.iter().map(|job| job.audio.as_slice()).collect();
                let n = group.len();
                let answers: Vec<Sensed> = match what {
                    Asked::Eou => {
                        let out = probabilities.get_mut(..n).unwrap_or_default();
                        if self.turn.infer(&audio, out).is_err() {
                            counters.failures.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let turns = group.iter().zip(out.iter());
                        turns
                            .map(|(job, p)| Sensed::Eou {
                                turn: job.turn,
                                p: Prob::from_f32(*p),
                            })
                            .collect()
                    }
                    Asked::Overlap => {
                        let Some(classifier) = &mut self.overlap else {
                            // Nobody to ask. The session decides without
                            // an answer when it has waited long enough.
                            continue;
                        };
                        let out = verdicts.get_mut(..n).unwrap_or_default();
                        if classifier.infer(&audio, out).is_err() {
                            counters.failures.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let turns = group.iter().zip(out.iter());
                        turns
                            .map(|(job, verdict)| Sensed::Overlap {
                                turn: job.turn,
                                verdict: *verdict,
                            })
                            .collect()
                    }
                };
                counters.turns.fetch_add(n as u64, Ordering::Relaxed);
                for (job, answer) in group.iter().zip(answers) {
                    lock(&job.link.late).push(answer);
                    job.link.waker.wake();
                }
            }
        }
        jobs.clear();
    }
}

#[derive(Debug, Default)]
struct Counters {
    rounds: AtomicU64,
    frames: AtomicU64,
    dropped: AtomicU64,
    turns: AtomicU64,
    failures: AtomicU64,
}

/// Where the rounds are run.
enum Mode {
    /// Inside the calls that give the plane work.
    Inline(Mutex<(Box<dyn FrameLane>, Turns)>),
    /// On threads of the plane's own.
    Threads {
        /// The thread of the frames, to be woken when there is one.
        frames: Thread,
        /// Streams that have opened and that the thread has not taken.
        opened: Mutex<Vec<Stream>>,
        jobs: mpsc::Sender<Job>,
    },
}

struct Shared {
    wants: Wants,
    mode: Mode,
    counters: Counters,
    stop: AtomicBool,
}

/// The compute plane. Dropping it stops its threads.
pub struct Compute {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl fmt::Debug for Compute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compute")
            .field("wants", &self.shared.wants)
            .field("threads", &self.threads.len())
            .finish_non_exhaustive()
    }
}

impl Compute {
    /// A plane that runs its models inside the calls that give it work:
    /// an answer is there when [`Listener::push`] or a request returns.
    /// For tests, where every run has to be the same.
    ///
    /// # Errors
    ///
    /// The models cannot work together.
    pub fn inline<V: VadModel>(
        vad: V,
        turn: Box<dyn TurnModel>,
        overlap: Option<Box<dyn InterruptionClassifier>>,
    ) -> Result<Self, ComputeError> {
        let wants = wants(&vad, turn.as_ref(), overlap.as_deref())?;
        let lanes: (Box<dyn FrameLane>, Turns) =
            (Box::new(Frames::new(vad)), Turns { turn, overlap });
        Ok(Self {
            shared: Arc::new(Shared {
                wants,
                mode: Mode::Inline(Mutex::new(lanes)),
                counters: Counters::default(),
                stop: AtomicBool::new(false),
            }),
            threads: Vec::new(),
        })
    }

    /// A plane with two threads of its own: one for the frames of every
    /// stream, one for the turns.
    ///
    /// # Errors
    ///
    /// The models cannot work together, or a thread could not be started.
    pub fn spawn<V: VadModel>(
        vad: V,
        turn: Box<dyn TurnModel>,
        overlap: Option<Box<dyn InterruptionClassifier>>,
    ) -> Result<Self, ComputeError> {
        let wants = wants(&vad, turn.as_ref(), overlap.as_deref())?;
        let (jobs, asked) = mpsc::channel::<Job>();
        // The thread of the frames needs the shared state, which holds
        // the handle that wakes that thread. So the thread waits to be
        // given the state.
        let (give, given) = mpsc::sync_channel::<Arc<Shared>>(1);
        let mut lane = Frames::new(vad);
        let frames = thread::Builder::new()
            .name("waveport-frames".into())
            .spawn(move || {
                if let Ok(shared) = given.recv() {
                    frames_thread(&mut lane, &shared);
                }
            })
            .map_err(|_| ComputeError::Thread)?;
        let shared = Arc::new(Shared {
            wants,
            mode: Mode::Threads {
                frames: frames.thread().clone(),
                opened: Mutex::new(Vec::new()),
                jobs,
            },
            counters: Counters::default(),
            stop: AtomicBool::new(false),
        });
        let mut plane = Self {
            shared: Arc::clone(&shared),
            threads: vec![frames],
        };
        // From here on dropping `plane` stops what was started.
        give.send(Arc::clone(&shared))
            .map_err(|_| ComputeError::Thread)?;
        let mut turns = Turns { turn, overlap };
        let turns = thread::Builder::new()
            .name("waveport-turns".into())
            .spawn(move || turns_thread(&mut turns, &asked, &shared))
            .map_err(|_| ComputeError::Thread)?;
        plane.threads.push(turns);
        Ok(plane)
    }

    /// What the models want to be given.
    #[must_use]
    pub fn wants(&self) -> Wants {
        self.shared.wants
    }

    /// A listener for one stream of audio.
    #[must_use]
    pub fn listen(&self) -> Listener {
        let wants = self.shared.wants;
        let (heads, heads_out) = RingBuffer::new(FRAMES_HELD);
        let (samples, samples_out) = RingBuffer::new(FRAMES_HELD * wants.frame);
        let (answers_in, answers) = RingBuffer::new(ANSWERS_HELD);
        let link = Arc::new(Link::default());
        let stream = Stream {
            heads: heads_out,
            samples: samples_out,
            answers: answers_in,
            link: Arc::clone(&link),
        };
        match &self.shared.mode {
            Mode::Inline(lanes) => lock(lanes).0.admit(stream),
            Mode::Threads { frames, opened, .. } => {
                lock(opened).push(stream);
                frames.unpark();
            }
        }
        Listener {
            shared: Arc::clone(&self.shared),
            heads,
            samples,
            answers,
            link,
            partial: Vec::with_capacity(wants.frame),
            partial_at: MediaTime::from_ticks(0),
        }
    }

    /// What the plane has done so far.
    #[must_use]
    pub fn stats(&self) -> ComputeStats {
        let counters = &self.shared.counters;
        ComputeStats {
            rounds: counters.rounds.load(Ordering::Relaxed),
            frames: counters.frames.load(Ordering::Relaxed),
            dropped: counters.dropped.load(Ordering::Relaxed),
            turns: counters.turns.load(Ordering::Relaxed),
            failures: counters.failures.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Compute {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Mode::Threads { frames, .. } = &self.shared.mode {
            frames.unpark();
        }
        for thread in self.threads.drain(..) {
            // A thread that panicked has stopped, which is all that is
            // wanted of it here.
            let _ = thread.join();
        }
    }
}

fn wants(
    vad: &impl VadModel,
    turn: &dyn TurnModel,
    overlap: Option<&dyn InterruptionClassifier>,
) -> Result<Wants, ComputeError> {
    let rate = vad.rate();
    if turn.rate() != rate || overlap.is_some_and(|overlap| overlap.rate() != rate) {
        return Err(ComputeError::Rates);
    }
    if vad.frame() == 0 || vad.max_batch() == 0 || turn.window() == 0 {
        return Err(ComputeError::Empty);
    }
    Ok(Wants {
        rate,
        frame: vad.frame(),
        turn_window: turn.window(),
        overlap_window: overlap.map(InterruptionClassifier::window),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn frames_thread(lane: &mut dyn FrameLane, shared: &Shared) {
    let Mode::Threads { opened, .. } = &shared.mode else {
        return;
    };
    while !shared.stop.load(Ordering::Acquire) {
        for stream in lock(opened).drain(..) {
            lane.admit(stream);
        }
        if lane.round(&shared.counters) == 0 {
            // Whoever hands over a frame wakes this thread, and a wake-up
            // that comes before the thread sleeps is not lost: the sleep
            // then ends at once.
            thread::park_timeout(IDLE);
        }
    }
}

fn turns_thread(turns: &mut Turns, asked: &mpsc::Receiver<Job>, shared: &Shared) {
    let mut jobs = Vec::new();
    while !shared.stop.load(Ordering::Acquire) {
        match asked.recv_timeout(IDLE) {
            Ok(job) => jobs.push(job),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Whatever else has been asked meanwhile is judged with it.
        jobs.extend(asked.try_iter());
        turns.judge(&mut jobs, &shared.counters);
    }
}

/// A session's end of one stream of audio: gives the models the audio and
/// is told what they make of it.
pub struct Listener {
    shared: Arc<Shared>,
    heads: Producer<Head>,
    samples: Producer<i16>,
    answers: Consumer<Sensed>,
    link: Arc<Link>,
    /// The beginning of a frame that is not whole yet.
    partial: Vec<i16>,
    /// Where `partial` begins on the media clock.
    partial_at: MediaTime,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("partial", &self.partial.len())
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Takes the next audio of the stream, which begins at `at` on the
    /// media clock, at the rate the models [want](Compute::wants). When
    /// `begins` is set the audio does not follow what was given before,
    /// and the beginning of a frame left over from before is dropped.
    ///
    /// Never waits. A frame the model has no room for is dropped and
    /// counted.
    pub fn push(&mut self, at: MediaTime, pcm: &[i16], begins: bool) {
        let wants = self.shared.wants;
        if begins {
            self.partial.clear();
        }
        let mut at = at;
        let mut rest = pcm;
        let mut handed = false;
        while !rest.is_empty() {
            if self.partial.is_empty() {
                self.partial_at = at;
            }
            let (now, later) = rest.split_at(rest.len().min(wants.frame - self.partial.len()));
            self.partial.extend_from_slice(now);
            at = at.after(wants.rate.length_of(now.len()));
            rest = later;
            if self.partial.len() < wants.frame {
                break;
            }
            // The samples go first: a head says that they are there.
            let head = Head {
                at: self.partial_at,
            };
            if self.heads.is_full() || self.samples.push_entire_slice(&self.partial).is_err() {
                self.shared.counters.dropped.fetch_add(1, Ordering::Relaxed);
            } else if self.heads.push(head).is_ok() {
                handed = true;
            }
            self.partial.clear();
        }
        if handed {
            match &self.shared.mode {
                Mode::Inline(lanes) => {
                    let mut lanes = lock(lanes);
                    while lanes.0.round(&self.shared.counters) > 0 {}
                }
                Mode::Threads { frames, .. } => frames.unpark(),
            }
        }
    }

    /// Asks how likely it is that the speaker of `turn` has finished.
    /// `audio` is the end of the turn so far: the newest audio of the
    /// stream, of which the model takes as much as it looks at. The answer
    /// comes through [`poll`](Self::poll).
    pub fn estimate_eou(&mut self, turn: TurnId, audio: Vec<i16>) {
        self.ask(Asked::Eou, turn, audio);
    }

    /// Asks what the speech of `turn`, which is spoken over the agent's
    /// own, is. With no model for that, there is no answer.
    pub fn classify_overlap(&mut self, turn: TurnId, audio: Vec<i16>) {
        if self.shared.wants.overlap_window.is_some() {
            self.ask(Asked::Overlap, turn, audio);
        }
    }

    fn ask(&mut self, what: Asked, turn: TurnId, audio: Vec<i16>) {
        let job = Job {
            link: Arc::clone(&self.link),
            turn,
            what,
            audio,
        };
        match &self.shared.mode {
            Mode::Inline(lanes) => {
                let mut jobs = vec![job];
                lock(lanes).1.judge(&mut jobs, &self.shared.counters);
            }
            Mode::Threads { jobs, .. } => {
                // A plane that has stopped answers nothing.
                let _ = jobs.send(job);
            }
        }
    }

    /// The next answer of the models about this stream. The waker of `cx`
    /// is woken when there is one.
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Sensed> {
        if let Some(answer) = self.take() {
            return Poll::Ready(answer);
        }
        self.link.waker.register(cx.waker());
        // An answer may have come while the waker was put in place.
        self.take().map_or(Poll::Pending, Poll::Ready)
    }

    fn take(&mut self) -> Option<Sensed> {
        if let Ok(answer) = self.answers.pop() {
            return Some(answer);
        }
        let mut late = lock(&self.link.late);
        if late.is_empty() {
            None
        } else {
            Some(late.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use core::future::poll_fn;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use waveport_domain::event::OverlapVerdict;
    use waveport_domain::ids::{Prob, TurnId};
    use waveport_domain::time::{MediaDur, MediaTime};
    use waveport_ports::audio::Rate;
    use waveport_ports::error::PortError;
    use waveport_ports::model::{InterruptionClassifier, TurnModel, VadModel};

    use super::{Compute, ComputeError, Listener, Sensed};

    const FRAME: usize = 512;

    /// What the test models were asked, for the test to look at.
    #[derive(Debug, Default)]
    struct Seen {
        /// The size of each batch of frames.
        batches: Mutex<Vec<usize>>,
        /// Makes the model fail.
        broken: AtomicBool,
        /// Holds the model up for as long as it is set.
        stuck: AtomicBool,
        /// How many times the model was asked, counted as it is entered.
        entered: AtomicUsize,
        turns: AtomicUsize,
    }

    /// Says that a frame holds speech when it is loud, and counts the
    /// frames of each stream.
    struct Loudness {
        seen: Arc<Seen>,
        batch: usize,
    }

    impl VadModel for Loudness {
        type State = u32;

        fn rate(&self) -> Rate {
            Rate::Hz16000
        }

        fn frame(&self) -> usize {
            FRAME
        }

        fn max_batch(&self) -> usize {
            self.batch
        }

        fn infer(
            &mut self,
            frames: &[&[i16]],
            states: &mut [u32],
            out: &mut [f32],
        ) -> Result<(), PortError> {
            self.seen.entered.fetch_add(1, Ordering::AcqRel);
            while self.seen.stuck.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            if self.seen.broken.load(Ordering::Acquire) {
                return Err(PortError::transient("the model fell over"));
            }
            self.seen.batches.lock().unwrap().push(frames.len());
            for ((frame, state), p) in frames.iter().zip(states).zip(out) {
                *state += 1;
                let loud = frame.iter().any(|sample| sample.unsigned_abs() > 1_000);
                // The whole part is the number of the frame in its
                // stream, so that a test can see the state at work.
                #[expect(clippy::cast_precision_loss, reason = "small numbers")]
                let count = *state as f32;
                *p = if loud { 0.9 } else { 0.1 } + count;
            }
            Ok(())
        }
    }

    /// Says a turn is over when its audio ends in silence.
    struct Pause {
        seen: Arc<Seen>,
    }

    impl TurnModel for Pause {
        fn rate(&self) -> Rate {
            Rate::Hz16000
        }

        fn window(&self) -> usize {
            16_000
        }

        fn max_batch(&self) -> usize {
            4
        }

        fn infer(&mut self, turns: &[&[i16]], out: &mut [f32]) -> Result<(), PortError> {
            self.seen.turns.fetch_add(turns.len(), Ordering::AcqRel);
            for (turn, p) in turns.iter().zip(out) {
                *p = if turn.last().is_some_and(|last| *last == 0) {
                    0.95
                } else {
                    0.1
                };
            }
            Ok(())
        }
    }

    /// Takes long speech for a wish to speak.
    struct Length;

    impl InterruptionClassifier for Length {
        fn rate(&self) -> Rate {
            Rate::Hz16000
        }

        fn window(&self) -> usize {
            32_000
        }

        fn infer(
            &mut self,
            speech: &[&[i16]],
            out: &mut [OverlapVerdict],
        ) -> Result<(), PortError> {
            for (speech, verdict) in speech.iter().zip(out) {
                *verdict = if speech.len() > 8_000 {
                    OverlapVerdict::BargeIn
                } else {
                    OverlapVerdict::Backchannel
                };
            }
            Ok(())
        }
    }

    fn models(batch: usize) -> (Loudness, Box<dyn TurnModel>, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let vad = Loudness {
            seen: Arc::clone(&seen),
            batch,
        };
        let turn = Box::new(Pause {
            seen: Arc::clone(&seen),
        });
        (vad, turn, seen)
    }

    fn inline(batch: usize) -> (Compute, Arc<Seen>) {
        let (vad, turn, seen) = models(batch);
        (Compute::inline(vad, turn, None).unwrap(), seen)
    }

    /// Waits until the model of the frames has been asked `times` times.
    fn entered(seen: &Seen, times: usize) {
        for _ in 0..10_000 {
            if seen.entered.load(Ordering::Acquire) >= times {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the model was not asked {times} times");
    }

    /// The answers that are there now.
    fn answers(listener: &mut Listener) -> Vec<Sensed> {
        let mut all = Vec::new();
        while let Some(answer) = listener.take() {
            all.push(answer);
        }
        all
    }

    fn speech(n: u32, p: f32) -> Sensed {
        Sensed::Speech {
            at: MediaTime::from_millis(u64::from(n) * 32),
            length: MediaDur::from_millis(32),
            p,
        }
    }

    #[test]
    fn audio_is_cut_into_the_frames_the_model_wants() {
        let (plane, seen) = inline(8);
        let mut listener = plane.listen();
        // 20 ms at a time, as a transport brings it: 320 samples.
        let loud = [3_000_i16; 320];
        let quiet = [0_i16; 320];
        let mut at = MediaTime::from_ticks(0);
        for n in 0..8 {
            let pcm = if n < 4 { &loud } else { &quiet };
            listener.push(at, pcm, n == 0);
            at = at.after(MediaDur::from_millis(20));
        }
        // 2,560 samples are five frames of 512. The third is loud to its
        // middle.
        assert_eq!(
            answers(&mut listener),
            [
                speech(0, 1.9),
                speech(1, 2.9),
                speech(2, 3.9),
                speech(3, 4.1),
                speech(4, 5.1)
            ]
        );
        assert_eq!(*seen.batches.lock().unwrap(), [1, 1, 1, 1, 1]);
        let stats = plane.stats();
        assert_eq!((stats.rounds, stats.frames, stats.dropped), (5, 5, 0));
    }

    #[test]
    fn a_stream_that_begins_again_drops_the_frame_it_had_begun() {
        let (plane, _) = inline(8);
        let mut listener = plane.listen();
        listener.push(MediaTime::from_millis(0), &[3_000; 500], true);
        // The stream broke off, and begins again five seconds later.
        listener.push(MediaTime::from_millis(5_000), &[0; 512], true);
        assert_eq!(
            answers(&mut listener),
            [Sensed::Speech {
                at: MediaTime::from_millis(5_000),
                length: MediaDur::from_millis(32),
                p: 1.1
            }]
        );
    }

    #[test]
    fn frames_of_several_streams_are_judged_together() {
        let (vad, turn, seen) = models(3);
        let plane = Compute::spawn(vad, turn, None).unwrap();
        let mut listeners: Vec<Listener> = (0..5).map(|_| plane.listen()).collect();
        // The model is held up while every stream hands over a frame, so
        // that all five wait at once.
        seen.stuck.store(true, Ordering::Release);
        let mut warm = plane.listen();
        warm.push(MediaTime::from_ticks(0), &[0; FRAME], true);
        entered(&seen, 1);
        for (n, listener) in listeners.iter_mut().enumerate() {
            let level = if n % 2 == 0 { 3_000 } else { 0 };
            listener.push(MediaTime::from_ticks(0), &[level; FRAME], true);
        }
        seen.stuck.store(false, Ordering::Release);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            for (n, listener) in listeners.iter_mut().enumerate() {
                let answer =
                    tokio::time::timeout(Duration::from_secs(10), poll_fn(|cx| listener.poll(cx)))
                        .await
                        .unwrap();
                let p = if n % 2 == 0 { 1.9 } else { 1.1 };
                assert_eq!(answer, speech(0, p), "stream {n}");
            }
        });
        // Five frames waited: a batch of three, the most the model takes,
        // and a batch of two.
        let batches = seen.batches.lock().unwrap().clone();
        assert_eq!(batches, [1, 3, 2]);
    }

    #[test]
    fn a_model_that_is_behind_loses_frames_and_not_its_place() {
        let (vad, turn, seen) = models(8);
        let plane = Compute::spawn(vad, turn, None).unwrap();
        let mut listener = plane.listen();
        seen.stuck.store(true, Ordering::Release);
        // One frame is taken by the model, which is then held up; four
        // more fill the ring; the three after them are dropped.
        listener.push(MediaTime::from_ticks(0), &[0; FRAME], true);
        entered(&seen, 1);
        for n in 1..8_u64 {
            listener.push(MediaTime::from_millis(n * 32), &[0; FRAME], false);
        }
        assert_eq!(plane.stats().dropped, 3);
        seen.stuck.store(false, Ordering::Release);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let heard: Vec<Sensed> = runtime.block_on(async {
            let mut heard = Vec::new();
            for _ in 0..5 {
                let next = poll_fn(|cx| listener.poll(cx));
                heard.push(
                    tokio::time::timeout(Duration::from_secs(10), next)
                        .await
                        .unwrap(),
                );
            }
            heard
        });
        // The frames that got through are the first five, and the model
        // counted them as five: it kept its state across the gap.
        let expected: Vec<Sensed> = (0..5_u32)
            .map(|n| {
                #[expect(clippy::cast_precision_loss, reason = "small numbers")]
                let count = (n + 1) as f32;
                speech(n, 0.1 + count)
            })
            .collect();
        assert_eq!(heard, expected);
    }

    #[test]
    fn a_model_that_fails_answers_nothing_and_goes_on() {
        let (plane, seen) = inline(8);
        let mut listener = plane.listen();
        seen.broken.store(true, Ordering::Release);
        listener.push(MediaTime::from_millis(0), &[0; FRAME], true);
        assert_eq!(answers(&mut listener), []);
        assert_eq!(plane.stats().failures, 1);

        seen.broken.store(false, Ordering::Release);
        listener.push(MediaTime::from_millis(32), &[0; FRAME], false);
        // The frame that failed was not counted by the model.
        assert_eq!(answers(&mut listener), [speech(1, 1.1)]);
    }

    #[test]
    fn a_turn_is_judged_on_request() {
        let (vad, turn, seen) = models(8);
        let plane = Compute::inline(vad, turn, Some(Box::new(Length))).unwrap();
        let mut listener = plane.listen();
        listener.estimate_eou(TurnId(3), vec![3_000; 4_000]);
        listener.estimate_eou(TurnId(4), vec![0; 4_000]);
        listener.classify_overlap(TurnId(5), vec![3_000; 9_000]);
        listener.classify_overlap(TurnId(6), vec![3_000; 2_000]);
        assert_eq!(
            answers(&mut listener),
            [
                Sensed::Eou {
                    turn: TurnId(3),
                    p: Prob::from_basis_points(1_000)
                },
                Sensed::Eou {
                    turn: TurnId(4),
                    p: Prob::from_basis_points(9_500)
                },
                Sensed::Overlap {
                    turn: TurnId(5),
                    verdict: OverlapVerdict::BargeIn
                },
                Sensed::Overlap {
                    turn: TurnId(6),
                    verdict: OverlapVerdict::Backchannel
                },
            ]
        );
        assert_eq!(seen.turns.load(Ordering::Acquire), 2);
        assert_eq!(plane.stats().turns, 4);
    }

    #[test]
    fn without_a_model_for_it_speech_over_the_agent_gets_no_answer() {
        let (plane, _) = inline(8);
        assert_eq!(plane.wants().overlap_window, None);
        let mut listener = plane.listen();
        listener.classify_overlap(TurnId(5), vec![3_000; 9_000]);
        assert_eq!(answers(&mut listener), []);
    }

    #[test]
    fn turns_are_judged_on_a_thread_of_their_own() {
        let (vad, turn, seen) = models(8);
        let plane = Compute::spawn(vad, turn, None).unwrap();
        let mut listener = plane.listen();
        // The model of the frames is held up, and a turn is still judged.
        seen.stuck.store(true, Ordering::Release);
        listener.push(MediaTime::from_ticks(0), &[0; FRAME], true);
        entered(&seen, 1);
        listener.estimate_eou(TurnId(1), vec![0; 4_000]);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let answer = runtime.block_on(async {
            let next = poll_fn(|cx| listener.poll(cx));
            tokio::time::timeout(Duration::from_secs(10), next)
                .await
                .unwrap()
        });
        assert_eq!(
            answer,
            Sensed::Eou {
                turn: TurnId(1),
                p: Prob::from_basis_points(9_500)
            }
        );
        seen.stuck.store(false, Ordering::Release);
    }

    #[test]
    fn a_plane_that_is_dropped_stops_its_threads() {
        let (vad, turn, seen) = models(8);
        let plane = Compute::spawn(vad, turn, None).unwrap();
        let mut listener = plane.listen();
        listener.push(MediaTime::from_ticks(0), &[0; FRAME], true);
        drop(plane);
        // The listener outlives the plane, and nothing it does waits.
        listener.push(MediaTime::from_millis(32), &[0; FRAME], false);
        listener.estimate_eou(TurnId(1), vec![0; 100]);
        assert!(seen.turns.load(Ordering::Acquire) <= 1);
    }

    #[test]
    fn models_that_cannot_work_together_are_refused() {
        struct Narrow;
        impl TurnModel for Narrow {
            fn rate(&self) -> Rate {
                Rate::Hz8000
            }
            fn window(&self) -> usize {
                8_000
            }
            fn max_batch(&self) -> usize {
                1
            }
            fn infer(&mut self, _: &[&[i16]], _: &mut [f32]) -> Result<(), PortError> {
                Ok(())
            }
        }
        let (vad, _, _) = models(8);
        let refused = Compute::inline(vad, Box::new(Narrow), None).unwrap_err();
        assert_eq!(refused, ComputeError::Rates);
        let (vad, turn, _) = models(0);
        let refused = Compute::inline(vad, turn, None).unwrap_err();
        assert_eq!(refused, ComputeError::Empty);
    }
}
