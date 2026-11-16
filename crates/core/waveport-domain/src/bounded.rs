//! A vector that never grows past the limit it was created with.
//!
//! Every table in the kernel has a limit that comes from the session's
//! configuration. Reserving the whole table once means a step never has to
//! ask the allocator for more, and a full table is an explicit outcome the
//! caller must handle instead of unbounded growth.

use alloc::vec::Vec;
use core::ops::Deref;
use core::slice;

/// A vector with a fixed limit, reserved in full when it is created.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Bounded<T> {
    items: Vec<T>,
    limit: usize,
}

impl<T> Bounded<T> {
    /// An empty vector that holds at most `limit` items.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            items: Vec::with_capacity(limit),
            limit,
        }
    }

    /// Appends `item`.
    ///
    /// # Errors
    ///
    /// Hands `item` back if the vector is full.
    pub fn push(&mut self, item: T) -> Result<(), T> {
        if self.items.len() >= self.limit {
            return Err(item);
        }
        self.items.push(item);
        Ok(())
    }

    /// The most items the vector holds.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Whether another item would be refused.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.items.len() >= self.limit
    }

    /// Removes and returns the item at `index`, keeping the rest in order.
    /// `None` if there is no such item.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        (index < self.items.len()).then(|| self.items.remove(index))
    }

    /// Keeps only the items `keep` accepts, in order.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.items.retain(keep);
    }

    /// Removes every item and keeps the reserved space.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// The items, for changing in place.
    pub fn iter_mut(&mut self) -> slice::IterMut<'_, T> {
        self.items.iter_mut()
    }

    /// The item at `index`, for changing in place.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.items.get_mut(index)
    }

    /// Gives back reserved space that is not in use. The next push reserves
    /// again, so this is for a session that is going idle.
    pub fn park(&mut self) {
        self.items.shrink_to_fit();
    }

    /// Bytes of heap this vector holds, in use or not.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.items.capacity().saturating_mul(size_of::<T>())
    }
}

impl<T> Deref for Bounded<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl<'a, T> IntoIterator for &'a Bounded<T> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut Bounded<T> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::Bounded;

    #[test]
    fn a_full_vector_hands_the_item_back() {
        let mut table = Bounded::new(2);
        assert_eq!(table.push('a'), Ok(()));
        assert_eq!(table.push('b'), Ok(()));
        assert!(table.is_full());
        assert_eq!(table.push('c'), Err('c'));
        assert_eq!(&*table, &['a', 'b']);
    }

    #[test]
    fn removal_keeps_order_and_frees_a_slot() {
        let mut table = Bounded::new(3);
        for item in [1, 2, 3] {
            table.push(item).unwrap();
        }
        assert_eq!(table.remove(0), Some(1));
        assert_eq!(table.remove(7), None);
        assert_eq!(&*table, &[2, 3]);
        table.retain(|item| *item != 2);
        assert_eq!(table.push(4), Ok(()));
        assert_eq!(&*table, &[3, 4]);
        for item in &mut table {
            *item += 1;
        }
        assert_eq!(table.iter().copied().sum::<i32>(), 9);
    }

    #[test]
    fn pushing_does_not_allocate_after_creation() {
        let mut table = Bounded::new(16);
        let reserved = table.heap_bytes();
        for item in 0..16_u64 {
            table.push(item).unwrap();
        }
        assert_eq!(table.heap_bytes(), reserved);
        table.clear();
        assert_eq!(table.heap_bytes(), reserved);
        assert_eq!(table.limit(), 16);
    }
}
