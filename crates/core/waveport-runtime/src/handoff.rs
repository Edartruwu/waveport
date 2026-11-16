//! What a session and the compute plane hand each other.
//!
//! A session runs on its shard and the listening models run on threads of
//! their own. Everything the two sides share is here: two rings for the
//! frames of a stream, a ring and a short list for the answers, the waker
//! of the session, and the bell of the thread that judges frames. Neither
//! side ever waits for the other.
//!
//! Three things have to hold however the threads take turns:
//!
//! - a frame that was handed over arrives whole, once and in its place,
//!   and a frame there was no room for leaves nothing behind;
//! - the thread that judges frames does not sleep while a frame waits for
//!   it;
//! - a session that was told to wait for an answer is woken when there is
//!   one.
//!
//! The tests at the end of this file try every way the threads of a small
//! case can take turns, with [loom](https://docs.rs/loom). They are built
//! with `--cfg waveport_loom`, and `cargo xtask ci` runs them as the gate
//! `loom`. In that build the lock, the waker and the sleep are loom's, and
//! the rings are stand-ins of which every operation is one step: the real
//! rings are `rtrb`'s, and what is tried here is how they are used, not
//! how they are made.

use core::task::{Context, Poll, Waker};
use core::time::Duration;

use waveport_domain::time::MediaTime;

use self::ring::{Consumer, Producer, RingBuffer};
use self::sync::{Arc, AtomicWaker, Mutex, MutexGuard, PoisonError, Thread};

/// What is the machine's in an ordinary build and loom's in a build that
/// tries every schedule.
mod sync {
    pub(super) use std::sync::PoisonError;
    #[cfg(not(waveport_loom))]
    pub(super) use std::sync::{Arc, Mutex, MutexGuard};
    #[cfg(not(waveport_loom))]
    pub(super) use std::thread::Thread;

    #[cfg(not(waveport_loom))]
    pub(super) use futures_util::task::AtomicWaker;
    #[cfg(waveport_loom)]
    pub(super) use loom::future::AtomicWaker;
    #[cfg(waveport_loom)]
    pub(super) use loom::sync::{Arc, Mutex, MutexGuard};
    #[cfg(waveport_loom)]
    pub(super) use loom::thread::Thread;
}

#[cfg(not(waveport_loom))]
mod ring {
    pub(super) use rtrb::{Consumer, Producer, RingBuffer};
}

/// Rings for the build that tries every schedule: what a ring with one
/// thread at each end promises, with every operation as one step.
#[cfg(waveport_loom)]
mod ring {
    use std::collections::VecDeque;
    use std::sync::PoisonError;

    use loom::sync::atomic::{AtomicBool, Ordering};
    use loom::sync::{Arc, Mutex};

    struct Shared<T> {
        room: usize,
        held: Mutex<VecDeque<T>>,
        abandoned: AtomicBool,
    }

    /// There was no room, or nothing was there.
    #[derive(Debug)]
    pub(super) struct Refused;

    pub(super) struct Producer<T>(Arc<Shared<T>>);
    pub(super) struct Consumer<T>(Arc<Shared<T>>);
    pub(super) struct RingBuffer;

    impl RingBuffer {
        pub(super) fn new<T>(room: usize) -> (Producer<T>, Consumer<T>) {
            let shared = Arc::new(Shared {
                room,
                held: Mutex::new(VecDeque::new()),
                abandoned: AtomicBool::new(false),
            });
            (Producer(Arc::clone(&shared)), Consumer(shared))
        }
    }

    impl<T> Producer<T> {
        pub(super) fn is_full(&self) -> bool {
            let held = self.0.held.lock().unwrap_or_else(PoisonError::into_inner);
            held.len() >= self.0.room
        }

        pub(super) fn push(&mut self, value: T) -> Result<(), Refused> {
            let mut held = self.0.held.lock().unwrap_or_else(PoisonError::into_inner);
            if held.len() >= self.0.room {
                return Err(Refused);
            }
            held.push_back(value);
            Ok(())
        }
    }

    impl<T: Copy> Producer<T> {
        pub(super) fn push_entire_slice(&mut self, values: &[T]) -> Result<(), Refused> {
            let mut held = self.0.held.lock().unwrap_or_else(PoisonError::into_inner);
            if held.len() + values.len() > self.0.room {
                return Err(Refused);
            }
            held.extend(values.iter().copied());
            Ok(())
        }
    }

    impl<T> Drop for Producer<T> {
        fn drop(&mut self) {
            self.0.abandoned.store(true, Ordering::Release);
        }
    }

    impl<T> Consumer<T> {
        pub(super) fn pop(&mut self) -> Result<T, Refused> {
            let mut held = self.0.held.lock().unwrap_or_else(PoisonError::into_inner);
            held.pop_front().ok_or(Refused)
        }

        pub(super) fn is_abandoned(&self) -> bool {
            self.0.abandoned.load(Ordering::Acquire)
        }
    }

    impl<T: Copy> Consumer<T> {
        pub(super) fn pop_entire_slice(&mut self, room: &mut [T]) -> Result<(), Refused> {
            let mut held = self.0.held.lock().unwrap_or_else(PoisonError::into_inner);
            let wanted = room.len();
            if held.len() < wanted {
                return Err(Refused);
            }
            for (slot, value) in room.iter_mut().zip(held.drain(..wanted)) {
                *slot = value;
            }
            Ok(())
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The session's end of the frames of a stream.
pub(crate) struct FrameTx {
    heads: Producer<MediaTime>,
    samples: Producer<i16>,
}

/// The plane's end of the frames of a stream.
pub(crate) struct FrameRx {
    heads: Consumer<MediaTime>,
    samples: Consumer<i16>,
}

/// The two ends of the frames of a stream: room for `held` frames of
/// `frame` samples each. Where a frame begins is in one ring and its
/// samples are in another beside it.
pub(crate) fn frames(held: usize, frame: usize) -> (FrameTx, FrameRx) {
    let (heads, heads_out) = RingBuffer::new(held);
    let (samples, samples_out) = RingBuffer::new(held * frame);
    let tx = FrameTx { heads, samples };
    let rx = FrameRx {
        heads: heads_out,
        samples: samples_out,
    };
    (tx, rx)
}

impl FrameTx {
    /// Hands a frame over: where it begins, and its samples. Returns
    /// whether there was room for it. A frame for which there was none
    /// leaves nothing behind.
    pub(crate) fn push(&mut self, at: MediaTime, samples: &[i16]) -> bool {
        // The samples go first: a head says that they are there. And they
        // go together or not at all, so that the other end, which may
        // have taken a head and not yet its samples, never finds part of
        // a frame.
        if self.heads.is_full() || self.samples.push_entire_slice(samples).is_err() {
            return false;
        }
        self.heads.push(at).is_ok()
    }
}

impl FrameRx {
    /// Takes the oldest frame into `room`, which is a frame long, and
    /// returns where it begins.
    pub(crate) fn pop(&mut self, room: &mut [i16]) -> Option<MediaTime> {
        let at = self.heads.pop().ok()?;
        // A head is written after its samples, so they are there.
        self.samples.pop_entire_slice(room).ok()?;
        Some(at)
    }

    /// Whether the session's end is gone.
    pub(crate) fn is_abandoned(&self) -> bool {
        self.heads.is_abandoned()
    }
}

/// What both ends hold of the answers of a stream.
struct Link<A> {
    /// Wakes the session when there is an answer.
    waker: AtomicWaker,
    /// Answers about whole turns. They come from another thread than the
    /// answers about frames, and there are few of them.
    late: Mutex<Vec<A>>,
}

/// The plane's end of the answers about frames.
pub(crate) struct AnswerTx<A> {
    answers: Producer<A>,
    link: Arc<Link<A>>,
}

/// Where a thread other than the one that judges frames puts its answers
/// for a session.
pub(crate) struct Late<A> {
    link: Arc<Link<A>>,
}

/// The session's end of the answers.
pub(crate) struct AnswerRx<A> {
    answers: Consumer<A>,
    link: Arc<Link<A>>,
}

/// The two ends of the answers of a stream, with room for `held` answers
/// about frames.
pub(crate) fn answers<A>(held: usize) -> (AnswerTx<A>, AnswerRx<A>) {
    let (answers_in, answers) = RingBuffer::new(held);
    let link = Arc::new(Link {
        waker: AtomicWaker::new(),
        late: Mutex::new(Vec::new()),
    });
    let tx = AnswerTx {
        answers: answers_in,
        link: Arc::clone(&link),
    };
    (tx, AnswerRx { answers, link })
}

impl<A> AnswerTx<A> {
    /// Gives the session an answer and wakes it. A session that does not
    /// take its answers loses them.
    pub(crate) fn give(&mut self, answer: A) {
        let _ = self.answers.push(answer);
        self.link.waker.wake();
    }
}

impl<A> Late<A> {
    /// Gives the session an answer and wakes it.
    pub(crate) fn give(&self, answer: A) {
        lock(&self.link.late).push(answer);
        self.link.waker.wake();
    }
}

impl<A> AnswerRx<A> {
    /// Where another thread puts its answers for this session.
    pub(crate) fn late(&self) -> Late<A> {
        Late {
            link: Arc::clone(&self.link),
        }
    }

    /// The next answer. The waker of `cx` is woken when there is one.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<A> {
        if let Some(answer) = self.take() {
            return Poll::Ready(answer);
        }
        register(&self.link.waker, cx.waker());
        // An answer may have come while the waker was put in place, and
        // whoever gave it may have woken the one that was there before.
        self.take().map_or(Poll::Pending, Poll::Ready)
    }

    fn take(&mut self) -> Option<A> {
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

#[cfg(not(waveport_loom))]
fn register(waker: &AtomicWaker, with: &Waker) {
    waker.register(with);
}

#[cfg(waveport_loom)]
fn register(waker: &AtomicWaker, with: &Waker) {
    waker.register_by_ref(with);
}

/// What wakes the thread that judges frames.
pub(crate) struct Bell(Thread);

impl Bell {
    /// The bell of `thread`.
    pub(crate) const fn of(thread: Thread) -> Self {
        Self(thread)
    }

    /// Wakes the thread, now or when it next goes to sleep. It is rung
    /// after what the thread is to find has been put where it looks.
    pub(crate) fn ring(&self) {
        self.0.unpark();
    }
}

/// Sleeps until the bell of this thread is rung, and for no longer than
/// `longest`. A ring that came since the thread last slept ends the sleep
/// at once, so a thread that looks for work, finds none and sleeps does
/// not miss work that came in between.
///
/// In the build that tries every schedule there is no clock, and the
/// sleep ends with a ring or not at all: a ring that could be missed shows
/// there as a thread that never wakes.
#[cfg_attr(waveport_loom, expect(unused_variables, reason = "there is no clock"))]
pub(crate) fn doze(longest: Duration) {
    #[cfg(not(waveport_loom))]
    std::thread::park_timeout(longest);
    #[cfg(waveport_loom)]
    loom::thread::park();
}

#[cfg(all(test, waveport_loom))]
mod schedules {
    use core::future::poll_fn;
    use core::time::Duration;

    use loom::future::block_on;
    use loom::thread;
    use waveport_domain::time::MediaTime;

    use super::{Bell, answers, doze, frames};

    const IDLE: Duration = Duration::from_millis(5);

    /// A frame of two samples that says which frame it is.
    fn frame(n: i16) -> (MediaTime, [i16; 2]) {
        (MediaTime::from_ticks(u64::from(n.unsigned_abs())), [n, -n])
    }

    #[test]
    fn frames_arrive_whole_and_in_order_and_the_thread_is_never_left_asleep() {
        loom::model(|| {
            let (mut tx, mut rx) = frames(2, 2);
            // The thread that judges frames: it takes what there is, and
            // sleeps when there is nothing.
            let judge = thread::spawn(move || {
                let mut got = Vec::new();
                let mut room = [0_i16; 2];
                while got.len() < 2 {
                    match rx.pop(&mut room) {
                        Some(at) => got.push((at, room)),
                        None => doze(IDLE),
                    }
                }
                got
            });
            let bell = Bell::of(judge.thread().clone());
            for n in 1..=2 {
                let (at, samples) = frame(n);
                assert!(tx.push(at, &samples), "there is room for two frames");
                bell.ring();
            }
            let got = judge.join().unwrap();
            assert_eq!(got, [frame(1), frame(2)]);
        });
    }

    #[test]
    fn a_frame_that_finds_no_room_is_dropped_whole() {
        loom::model(|| {
            // Room for one frame, and three are handed over as fast as
            // they can be.
            let (mut tx, mut rx) = frames(1, 2);
            let judge = thread::spawn(move || {
                let mut got = Vec::new();
                let mut room = [0_i16; 2];
                loop {
                    if let Some(at) = rx.pop(&mut room) {
                        got.push((at, room));
                    } else if rx.is_abandoned() {
                        // Nothing more is coming. What came before the
                        // session left is still taken.
                        if let Some(at) = rx.pop(&mut room) {
                            got.push((at, room));
                        }
                        break;
                    } else {
                        doze(IDLE);
                    }
                }
                got
            });
            let bell = Bell::of(judge.thread().clone());
            let mut taken = Vec::new();
            for n in 1..=3 {
                let (at, samples) = frame(n);
                if tx.push(at, &samples) {
                    taken.push(frame(n));
                    bell.ring();
                }
            }
            drop(tx);
            bell.ring();
            // What was taken arrived, as it was and in order, and what
            // was refused left nothing in the rings.
            let got = judge.join().unwrap();
            assert_eq!(got, taken);
            assert!(!got.is_empty());
        });
    }

    #[test]
    fn a_session_that_waits_is_woken_by_an_answer_about_a_frame() {
        loom::model(|| {
            let (mut tx, mut rx) = answers::<u32>(4);
            let frames = thread::spawn(move || {
                tx.give(1);
                tx.give(2);
            });
            // The session: it is woken, or it waits for ever.
            let got = [
                block_on(poll_fn(|cx| rx.poll(cx))),
                block_on(poll_fn(|cx| rx.poll(cx))),
            ];
            assert_eq!(got, [1, 2]);
            frames.join().unwrap();
        });
    }

    #[test]
    fn a_session_that_waits_is_woken_by_an_answer_about_a_turn() {
        loom::model(|| {
            let (_tx, mut rx) = answers::<u32>(4);
            let late = rx.late();
            let turns = thread::spawn(move || {
                late.give(1);
                late.give(2);
            });
            let got = [
                block_on(poll_fn(|cx| rx.poll(cx))),
                block_on(poll_fn(|cx| rx.poll(cx))),
            ];
            assert_eq!(got, [1, 2]);
            turns.join().unwrap();
        });
    }

    #[test]
    fn answers_from_two_threads_at_once_both_arrive() {
        // Three threads have more ways to take turns than can all be
        // tried in the time a gate has: here a thread is taken off its
        // work at most three times in a schedule.
        let mut schedules = loom::model::Builder::new();
        schedules.preemption_bound = Some(3);
        schedules.check(|| {
            let (mut tx, mut rx) = answers::<u32>(4);
            let late = rx.late();
            let frames = thread::spawn(move || tx.give(1));
            let turns = thread::spawn(move || late.give(2));
            let mut got = [
                block_on(poll_fn(|cx| rx.poll(cx))),
                block_on(poll_fn(|cx| rx.poll(cx))),
            ];
            got.sort_unstable();
            assert_eq!(got, [1, 2]);
            frames.join().unwrap();
            turns.join().unwrap();
        });
    }
}
