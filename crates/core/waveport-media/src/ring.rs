//! A fixed amount of audio, oldest first.

/// A buffer that holds up to a fixed number of samples in the order they
/// were written.
///
/// It serves two uses. As a window on a stream it is written with
/// [`push`](Self::push), which makes room by dropping the oldest samples,
/// and read with [`latest`](Self::latest). As a bounded queue it is written
/// with [`push_some`](Self::push_some), which takes only what fits, and
/// read with [`pop`](Self::pop).
///
/// A window can be a queue as well, for a reader that keeps count of how
/// far behind the end it is and reads on with [`behind`](Self::behind):
/// the audio is then held once, and not once more for the reader.
///
/// Its memory is reserved once, when it is made.
#[derive(Clone, Debug)]
pub struct Ring {
    samples: Box<[i16]>,
    /// Where the oldest sample is.
    start: usize,
    len: usize,
    /// Samples ever written.
    written: u64,
}

impl Ring {
    /// A ring that holds `capacity` samples.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: vec![0; capacity].into_boxed_slice(),
            start: 0,
            len: 0,
            written: 0,
        }
    }

    /// How many samples it can hold.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.samples.len()
    }

    /// How many samples it holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many more samples fit without dropping any.
    #[must_use]
    pub const fn room(&self) -> usize {
        self.capacity() - self.len
    }

    /// How many samples were ever written to it: the place in the stream
    /// of the next sample to come.
    #[must_use]
    pub const fn written(&self) -> u64 {
        self.written
    }

    /// Forgets what it holds.
    pub const fn clear(&mut self) {
        self.start = 0;
        self.len = 0;
    }

    /// Where the sample `offset` places after the oldest is.
    const fn index(&self, offset: usize) -> usize {
        let capacity = self.capacity();
        if capacity == 0 {
            return 0;
        }
        // Both are below the capacity, so the sum does not overflow for
        // any buffer that fits in memory.
        (self.start + offset) % capacity
    }

    /// Writes `samples`, dropping the oldest samples held when there is no
    /// room. Returns how many were dropped, counting those of `samples`
    /// that did not fit themselves.
    pub fn push(&mut self, samples: &[i16]) -> usize {
        let capacity = self.capacity();
        self.written = self.written.saturating_add(samples.len() as u64);
        if samples.len() >= capacity {
            // Only the end of it fits.
            let dropped = self.len + (samples.len() - capacity);
            let kept = samples.get(samples.len() - capacity..).unwrap_or(&[]);
            self.samples.copy_from_slice(kept);
            self.start = 0;
            self.len = capacity;
            return dropped;
        }
        let dropped = samples.len().saturating_sub(self.room());
        self.start = self.index(dropped);
        self.len -= dropped;
        self.write(samples);
        dropped
    }

    /// Writes as many of `samples` as there is room for, from the first,
    /// and returns how many that was.
    pub fn push_some(&mut self, samples: &[i16]) -> usize {
        let taken = samples.len().min(self.room());
        self.written = self.written.saturating_add(taken as u64);
        self.write(samples.get(..taken).unwrap_or(&[]));
        taken
    }

    /// Writes `samples` after what is held. There is room for them.
    fn write(&mut self, samples: &[i16]) {
        let at = self.index(self.len);
        let (first, second) = samples.split_at(samples.len().min(self.capacity() - at));
        if let Some(room) = self.samples.get_mut(at..at + first.len()) {
            room.copy_from_slice(first);
        }
        if let Some(room) = self.samples.get_mut(..second.len()) {
            room.copy_from_slice(second);
        }
        self.len += samples.len();
    }

    /// What is held, oldest first, in two parts.
    #[must_use]
    pub fn as_slices(&self) -> (&[i16], &[i16]) {
        let first = self.len.min(self.capacity() - self.start);
        (
            self.samples
                .get(self.start..self.start + first)
                .unwrap_or(&[]),
            self.samples.get(..self.len - first).unwrap_or(&[]),
        )
    }

    /// Copies the `out.len()` oldest samples into `out` without taking
    /// them out, or as many as are held. Returns how many.
    pub fn peek(&self, out: &mut [i16]) -> usize {
        let (first, second) = self.as_slices();
        let from_first = first.len().min(out.len());
        let from_second = second.len().min(out.len() - from_first);
        if let (Some(room), Some(held)) = (out.get_mut(..from_first), first.get(..from_first)) {
            room.copy_from_slice(held);
        }
        if let (Some(room), Some(held)) = (
            out.get_mut(from_first..from_first + from_second),
            second.get(..from_second),
        ) {
            room.copy_from_slice(held);
        }
        from_first + from_second
    }

    /// Takes the `out.len()` oldest samples out into `out`, or as many as
    /// are held. Returns how many.
    pub fn pop(&mut self, out: &mut [i16]) -> usize {
        let taken = self.peek(out);
        self.skip(taken);
        taken
    }

    /// Drops the `count` oldest samples, or all of them. Returns how many
    /// were dropped.
    pub const fn skip(&mut self, count: usize) -> usize {
        let dropped = if count < self.len { count } else { self.len };
        self.start = self.index(dropped);
        self.len -= dropped;
        dropped
    }

    /// Copies the newest samples into the start of `out`, oldest first:
    /// `out.len()` of them, or as many as are held. Returns how many.
    pub fn latest(&self, out: &mut [i16]) -> usize {
        self.behind(out.len(), out)
    }

    /// Copies into the start of `out` the samples that begin `behind`
    /// places before the end of what is held, oldest first: `out.len()` of
    /// them, or as many as there are from there on. Returns how many. A
    /// place before the oldest sample counts as the oldest.
    ///
    /// A reader that is `behind` samples behind the stream reads on with
    /// this, and is then behind by what it was given less.
    pub fn behind(&self, behind: usize, out: &mut [i16]) -> usize {
        let behind = behind.min(self.len);
        let count = behind.min(out.len());
        let (first, second) = self.as_slices();
        // What is skipped comes off the older part first.
        let skip = self.len - behind;
        let first = first.get(skip.min(first.len())..).unwrap_or(&[]);
        let second = second
            .get(skip.saturating_sub(self.len - second.len())..)
            .unwrap_or(&[]);
        let from_first = first.len().min(count);
        let from_second = count - from_first;
        if let (Some(room), Some(held)) = (out.get_mut(..from_first), first.get(..from_first)) {
            room.copy_from_slice(held);
        }
        if let (Some(room), Some(held)) =
            (out.get_mut(from_first..count), second.get(..from_second))
        {
            room.copy_from_slice(held);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::Ring;

    fn held(ring: &Ring) -> Vec<i16> {
        let (first, second) = ring.as_slices();
        [first, second].concat()
    }

    #[test]
    fn a_window_keeps_the_newest_samples() {
        let mut ring = Ring::new(5);
        assert_eq!(ring.push(&[1, 2, 3]), 0);
        assert_eq!(held(&ring), [1, 2, 3]);
        assert_eq!(ring.push(&[4, 5, 6, 7]), 2, "1 and 2 make room");
        assert_eq!(held(&ring), [3, 4, 5, 6, 7]);
        assert_eq!((ring.len(), ring.room(), ring.written()), (5, 0, 7));

        let mut newest = [0; 3];
        assert_eq!(ring.latest(&mut newest), 3);
        assert_eq!(newest, [5, 6, 7]);
        let mut all = [0; 8];
        assert_eq!(ring.latest(&mut all), 5);
        assert_eq!(all, [3, 4, 5, 6, 7, 0, 0, 0]);
    }

    #[test]
    fn more_than_it_holds_leaves_the_end() {
        let mut ring = Ring::new(4);
        ring.push(&[1, 2]);
        assert_eq!(ring.push(&[3, 4, 5, 6, 7, 8, 9]), 5);
        assert_eq!(held(&ring), [6, 7, 8, 9]);
        assert_eq!(ring.written(), 9);
    }

    #[test]
    fn a_queue_takes_only_what_fits_and_gives_it_back_in_order() {
        let mut ring = Ring::new(4);
        assert_eq!(ring.push_some(&[1, 2, 3]), 3);
        assert_eq!(ring.push_some(&[4, 5, 6]), 1, "one place was left");
        assert_eq!(ring.written(), 4);

        let mut out = [0; 3];
        assert_eq!(ring.peek(&mut out), 3);
        assert_eq!(out, [1, 2, 3]);
        assert_eq!(ring.len(), 4, "looking takes nothing out");
        assert_eq!(ring.pop(&mut out), 3);
        assert_eq!(out, [1, 2, 3]);

        // The next write wraps round the end.
        assert_eq!(ring.push_some(&[5, 6, 7]), 3);
        assert_eq!(held(&ring), [4, 5, 6, 7]);
        assert_eq!(ring.skip(1), 1);
        let mut out = [0; 8];
        assert_eq!(ring.pop(&mut out), 3);
        assert_eq!(out[..3], [5, 6, 7]);
        assert!(ring.is_empty());
        assert_eq!(ring.skip(4), 0);
    }

    #[test]
    fn every_way_of_reading_agrees_wherever_the_start_is() {
        // Walk the start round the buffer and compare with a plain list.
        let mut ring = Ring::new(7);
        let mut model: Vec<i16> = Vec::new();
        let mut next = 0;
        for round in 0..40 {
            let count = round % 5 + 1;
            let fresh: Vec<i16> = (next..next + count).collect();
            next += count;
            let dropped = ring.push(&fresh);
            model.extend(&fresh);
            let over = model.len().saturating_sub(7);
            assert_eq!(dropped, over);
            model.drain(..over);
            assert_eq!(held(&ring), model);

            for wanted in 0..9 {
                let mut out = vec![0; wanted];
                let got = ring.latest(&mut out);
                assert_eq!(got, wanted.min(model.len()));
                assert_eq!(out[..got], model[model.len() - got..]);
                let got = ring.peek(&mut out);
                assert_eq!(out[..got], model[..got]);
                for behind in 0..9 {
                    let from = model.len().saturating_sub(behind);
                    let got = ring.behind(behind, &mut out);
                    assert_eq!(got, wanted.min(model.len() - from));
                    assert_eq!(out[..got], model[from..from + got]);
                }
            }

            let taken = usize::try_from(round % 3).unwrap();
            let mut out = vec![0; taken];
            let got = ring.pop(&mut out);
            assert_eq!(out[..got], model[..got]);
            model.drain(..got);
        }
    }

    #[test]
    fn a_reader_behind_a_window_reads_on_from_where_it_is() {
        let mut ring = Ring::new(6);
        ring.push(&[1, 2, 3, 4]);
        // Three behind: it is given two of them, and is then one behind.
        let mut out = [0; 2];
        assert_eq!(ring.behind(3, &mut out), 2);
        assert_eq!(out, [2, 3]);
        assert_eq!(ring.behind(1, &mut out), 1);
        assert_eq!(out[..1], [4]);
        assert_eq!(ring.behind(0, &mut out), 0);

        // The window moves on under it, round the end of the buffer.
        ring.push(&[5, 6, 7, 8]);
        assert_eq!(held(&ring), [3, 4, 5, 6, 7, 8]);
        let mut out = [0; 8];
        assert_eq!(ring.behind(4, &mut out), 4);
        assert_eq!(out[..4], [5, 6, 7, 8]);
        // Nothing is further behind than the oldest sample.
        assert_eq!(ring.behind(9, &mut out), 6);
        assert_eq!(out[..6], [3, 4, 5, 6, 7, 8]);
        assert_eq!(ring.len(), 6, "reading takes nothing out");
    }

    #[test]
    fn a_ring_of_nothing_holds_nothing() {
        let mut ring = Ring::new(0);
        assert_eq!(ring.push(&[1, 2]), 2);
        assert_eq!(ring.push_some(&[1, 2]), 0);
        assert_eq!(ring.pop(&mut [0; 2]), 0);
        assert_eq!(ring.latest(&mut [0; 2]), 0);
        assert_eq!(ring.behind(2, &mut [0; 2]), 0);
        assert!(ring.is_empty());
        ring.clear();
    }
}
