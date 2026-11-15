//! A queue of things due at simulated times.

use core::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::time::SimTime;

/// Items due at given times, handed back earliest first.
///
/// Items due at the same time come back in the order they were added, so the
/// order never depends on memory layout or hashing.
#[derive(Debug)]
pub struct Schedule<T> {
    heap: BinaryHeap<Entry<T>>,
    next_sequence: u64,
}

#[derive(Debug)]
struct Entry<T> {
    at: SimTime,
    sequence: u64,
    item: T,
}

impl<T> Entry<T> {
    fn key(&self) -> (SimTime, u64) {
        (self.at, self.sequence)
    }
}

impl<T> PartialEq for Entry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl<T> Eq for Entry<T> {}

impl<T> PartialOrd for Entry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Entry<T> {
    /// Reversed, so the heap's largest entry is the earliest one.
    fn cmp(&self, other: &Self) -> Ordering {
        other.key().cmp(&self.key())
    }
}

impl<T> Default for Schedule<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Schedule<T> {
    /// An empty schedule.
    #[must_use]
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            next_sequence: 0,
        }
    }

    /// Adds `item`, due at `at`.
    pub fn push(&mut self, at: SimTime, item: T) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.heap.push(Entry { at, sequence, item });
    }

    /// Removes and returns the earliest item with the time it is due.
    pub fn pop(&mut self) -> Option<(SimTime, T)> {
        self.heap.pop().map(|entry| (entry.at, entry.item))
    }

    /// Like [`pop`](Self::pop), but only if the earliest item is due by `now`.
    pub fn pop_due(&mut self, now: SimTime) -> Option<(SimTime, T)> {
        if self.next_at()? <= now {
            self.pop()
        } else {
            None
        }
    }

    /// When the earliest item is due.
    #[must_use]
    pub fn next_at(&self) -> Option<SimTime> {
        self.heap.peek().map(|entry| entry.at)
    }

    /// Number of items waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Schedule;
    use crate::time::SimTime;

    fn at(millis: u64) -> SimTime {
        SimTime::from_millis(millis)
    }

    #[test]
    fn earliest_first_then_insertion_order() {
        let mut schedule = Schedule::new();
        schedule.push(at(30), "c");
        schedule.push(at(10), "a1");
        schedule.push(at(20), "b");
        schedule.push(at(10), "a2");
        schedule.push(at(10), "a3");
        assert_eq!(schedule.len(), 5);
        assert_eq!(schedule.next_at(), Some(at(10)));
        let order: Vec<_> = core::iter::from_fn(|| schedule.pop()).collect();
        assert_eq!(
            order,
            [
                (at(10), "a1"),
                (at(10), "a2"),
                (at(10), "a3"),
                (at(20), "b"),
                (at(30), "c"),
            ]
        );
        assert!(schedule.is_empty());
        assert_eq!(schedule.next_at(), None);
    }

    #[test]
    fn pop_due_waits_for_the_clock() {
        let mut schedule = Schedule::default();
        schedule.push(at(10), 1);
        schedule.push(at(20), 2);
        assert_eq!(schedule.pop_due(at(9)), None);
        assert_eq!(schedule.pop_due(at(10)), Some((at(10), 1)));
        assert_eq!(schedule.pop_due(at(10)), None);
        assert_eq!(schedule.pop_due(at(99)), Some((at(20), 2)));
        assert_eq!(schedule.pop_due(at(99)), None);
    }
}
