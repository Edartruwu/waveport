//! Buffers that are used again.
//!
//! A frame of audio that leaves the media path, for a model or for speech
//! recognition, needs memory that outlives the call it was made in. A
//! [`Pool`] hands such memory out and takes it back, so that in the steady
//! state no frame costs a call to the allocator.
//!
//! A pool belongs to one thread. It is sized for the most frames that are
//! out at once; when more are asked for they are made, and counted, so
//! that a pool that is too small shows up as a number and not as a pause.

/// A buffer of samples from a [`Pool`]: a fixed amount of room, and how
/// much of it is filled.
#[derive(Debug)]
pub struct Buffer {
    samples: Box<[i16]>,
    len: usize,
}

impl Buffer {
    fn new(size: usize) -> Self {
        Self {
            samples: vec![0; size].into_boxed_slice(),
            len: 0,
        }
    }

    /// How many samples it has room for.
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

    /// The samples it holds.
    #[must_use]
    pub fn as_slice(&self) -> &[i16] {
        self.samples.get(..self.len).unwrap_or(&[])
    }

    /// All of its room, to write into. [`set_len`](Self::set_len) then
    /// says how much was written.
    #[must_use]
    pub fn room(&mut self) -> &mut [i16] {
        &mut self.samples
    }

    /// Says that it holds its first `len` samples: all of its room at
    /// most.
    pub const fn set_len(&mut self, len: usize) {
        self.len = if len < self.samples.len() {
            len
        } else {
            self.samples.len()
        };
    }

    /// Makes it hold a copy of `samples`, as far as there is room, and
    /// returns how many samples that is.
    pub fn fill(&mut self, samples: &[i16]) -> usize {
        let len = samples.len().min(self.samples.len());
        if let (Some(room), Some(samples)) = (self.samples.get_mut(..len), samples.get(..len)) {
            room.copy_from_slice(samples);
        }
        self.len = len;
        len
    }
}

/// Buffers of one size, kept to be used again.
#[derive(Debug)]
pub struct Pool {
    ready: Vec<Buffer>,
    size: usize,
    kept: usize,
    made: u64,
}

impl Pool {
    /// A pool of buffers of `size` samples each, with `kept` of them made
    /// now. It never keeps more than that.
    #[must_use]
    pub fn new(size: usize, kept: usize) -> Self {
        let mut ready = Vec::with_capacity(kept);
        ready.resize_with(kept, || Buffer::new(size));
        Self {
            ready,
            size,
            kept,
            made: 0,
        }
    }

    /// How many samples each buffer has room for.
    #[must_use]
    pub const fn size(&self) -> usize {
        self.size
    }

    /// How many buffers are ready to be taken.
    #[must_use]
    pub const fn ready(&self) -> usize {
        self.ready.len()
    }

    /// How many buffers had to be made because none was ready. A number
    /// that keeps growing says that the pool is too small for its use.
    #[must_use]
    pub const fn made(&self) -> u64 {
        self.made
    }

    /// An empty buffer: one that was given back, or a new one when none
    /// is ready.
    #[must_use]
    pub fn take(&mut self) -> Buffer {
        if let Some(mut buffer) = self.ready.pop() {
            buffer.len = 0;
            return buffer;
        }
        self.made = self.made.saturating_add(1);
        Buffer::new(self.size)
    }

    /// Takes a buffer back. One that is not of this pool's size, or that
    /// there is no room to keep, is let go.
    pub fn give(&mut self, buffer: Buffer) {
        if buffer.capacity() == self.size && self.ready.len() < self.kept {
            self.ready.push(buffer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Pool;

    #[test]
    fn a_buffer_holds_what_it_is_filled_with_up_to_its_room() {
        let mut pool = Pool::new(4, 1);
        let mut buffer = pool.take();
        assert_eq!((buffer.capacity(), buffer.len()), (4, 0));
        assert!(buffer.is_empty());
        assert_eq!(buffer.fill(&[1, 2, 3]), 3);
        assert_eq!(buffer.as_slice(), [1, 2, 3]);
        assert_eq!(buffer.fill(&[5, 6, 7, 8, 9]), 4, "the fifth has no room");
        assert_eq!(buffer.as_slice(), [5, 6, 7, 8]);

        // Written in place.
        buffer.room()[..2].copy_from_slice(&[40, 41]);
        buffer.set_len(2);
        assert_eq!(buffer.as_slice(), [40, 41]);
        buffer.set_len(99);
        assert_eq!(buffer.len(), 4);
    }

    #[test]
    fn buffers_given_back_are_handed_out_again_empty() {
        let mut pool = Pool::new(8, 2);
        assert_eq!((pool.size(), pool.ready(), pool.made()), (8, 2, 0));
        let mut first = pool.take();
        let second = pool.take();
        first.fill(&[1, 2, 3]);
        assert_eq!(pool.ready(), 0);
        pool.give(first);
        pool.give(second);
        assert_eq!(pool.ready(), 2);
        let again = pool.take();
        assert!(again.is_empty(), "what it held is forgotten");
        assert_eq!(pool.made(), 0);
    }

    #[test]
    fn more_than_the_pool_keeps_are_made_and_counted() {
        let mut pool = Pool::new(8, 1);
        let first = pool.take();
        let second = pool.take();
        let third = pool.take();
        assert_eq!(pool.made(), 2);
        // Only one is kept when they come back.
        pool.give(first);
        pool.give(second);
        pool.give(third);
        assert_eq!(pool.ready(), 1);
        // A buffer of another size is not kept.
        let mut small = Pool::new(4, 1);
        let other = small.take();
        let _ = pool.take();
        pool.give(other);
        assert_eq!(pool.ready(), 0);

        // A pool that keeps nothing still works.
        let mut none = Pool::new(8, 0);
        let buffer = none.take();
        none.give(buffer);
        assert_eq!((none.ready(), none.made()), (0, 1));
    }
}
