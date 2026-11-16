//! Timing every run of a session.
//!
//! The sessions of a shard share one thread, so a session that runs for
//! long holds the others up: their audio goes out late. Each time a
//! session runs it is timed, and a run that took longer than the budget is
//! counted. The counts say whether something that does not belong on a
//! shard has got onto one: a tool that computes, an adapter that blocks.
//!
//! The time is the machine's, not the session's: a session on a clock
//! that a test controls is timed all the same.

use core::pin::Pin;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use core::task::{Context, Poll};
use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

/// What has run on a shard, counted. A shard writes it and anyone reads
/// it, so the numbers of one reading need not belong to the same moment.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// The sessions on the shard now.
    pub sessions: AtomicUsize,
    /// How many times a session ran.
    polls: AtomicU64,
    /// How many of those took longer than the budget.
    over: AtomicU64,
    /// The longest of them, in nanoseconds.
    longest: AtomicU64,
    /// How long the sessions ran in all, in nanoseconds.
    busy: AtomicU64,
}

impl Counters {
    fn ran(&self, took: Duration, budget: Duration) {
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        self.polls.fetch_add(1, Ordering::Relaxed);
        if took > budget {
            self.over.fetch_add(1, Ordering::Relaxed);
        }
        self.longest.fetch_max(nanos, Ordering::Relaxed);
        self.busy.fetch_add(nanos, Ordering::Relaxed);
    }

    /// The counts as they stand.
    pub(crate) fn read(&self) -> ShardStats {
        ShardStats {
            sessions: self.sessions.load(Ordering::Relaxed),
            polls: self.polls.load(Ordering::Relaxed),
            over_budget: self.over.load(Ordering::Relaxed),
            longest: Duration::from_nanos(self.longest.load(Ordering::Relaxed)),
            busy: Duration::from_nanos(self.busy.load(Ordering::Relaxed)),
        }
    }
}

/// What has run on one shard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShardStats {
    /// The sessions on it now.
    pub sessions: usize,
    /// How many times a session of it ran.
    pub polls: u64,
    /// How many of those runs took longer than the budget, and held the
    /// shard's other sessions up.
    pub over_budget: u64,
    /// The longest run.
    pub longest: Duration,
    /// How long its sessions ran in all. Against the time that has
    /// passed, it says how much of its thread the shard uses.
    pub busy: Duration,
}

/// A future whose every poll is timed.
pub(crate) struct Watched<F> {
    inner: Pin<Box<F>>,
    counters: Arc<Counters>,
    budget: Duration,
}

impl<F> Watched<F> {
    pub(crate) fn new(inner: F, counters: Arc<Counters>, budget: Duration) -> Self {
        Self {
            inner: Box::pin(inner),
            counters,
            budget,
        }
    }
}

impl<F: Future> Future for Watched<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let began = Instant::now();
        let polled = self.inner.as_mut().poll(cx);
        self.counters.ran(began.elapsed(), self.budget);
        polled
    }
}

#[cfg(test)]
mod tests {
    use core::future::poll_fn;
    use core::task::{Context, Poll, Waker};
    use core::time::Duration;
    use std::sync::Arc;

    use super::{Counters, Watched};

    #[test]
    fn a_run_that_takes_too_long_is_counted() {
        let counters = Arc::new(Counters::default());
        let mut runs = 0;
        let work = poll_fn(move |_| {
            runs += 1;
            match runs {
                // The first run is quick, the second is not.
                1 => Poll::Pending,
                2 => {
                    std::thread::sleep(Duration::from_millis(20));
                    Poll::Pending
                }
                _ => Poll::Ready(runs),
            }
        });
        let budget = Duration::from_millis(10);
        let mut watched = Box::pin(Watched::new(work, Arc::clone(&counters), budget));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(watched.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(counters.read().over_budget, 0);
        assert_eq!(watched.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(watched.as_mut().poll(&mut cx), Poll::Ready(3));

        let stats = counters.read();
        assert_eq!(stats.polls, 3);
        assert_eq!(stats.over_budget, 1);
        assert!(stats.longest >= Duration::from_millis(20), "{stats:?}");
        // The three runs together took at least as long as the longest,
        // and it is their sum that is kept, not the last.
        assert!(stats.busy >= stats.longest, "{stats:?}");
    }
}
