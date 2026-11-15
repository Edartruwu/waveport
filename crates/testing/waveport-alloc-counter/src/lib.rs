//! Waveport allocation counter.
//!
//! Stability: experimental
//!
//! A global allocator that counts the calls each thread makes to it, so a
//! test can prove that a code path does not touch the heap.
//!
//! Install it once in the test binary, then wrap the code under test:
//!
//! ```
//! use waveport_alloc_counter::{CountingAllocator, assert_no_alloc, measure};
//!
//! #[global_allocator]
//! static ALLOCATOR: CountingAllocator = CountingAllocator::system();
//!
//! let mut buffer = Vec::with_capacity(16);
//! assert_no_alloc(|| buffer.push(1_u8));
//!
//! let (_, stats) = measure(|| vec![0_u8; 32]);
//! assert_eq!(stats.allocations, 1);
//! ```
//!
//! Counts are kept per thread, so tests running in parallel do not disturb
//! each other. Work the measured code hands to another thread is not counted.
//!
//! This is the only Waveport crate that contains `unsafe` code: implementing
//! [`GlobalAlloc`] requires it. Every call is forwarded unchanged to the
//! wrapped allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;

thread_local! {
    // Constant initialisation and no destructor: reading this from inside the
    // allocator neither allocates nor registers anything to run at thread exit.
    static STATS: Cell<AllocStats> = const { Cell::new(AllocStats::ZERO) };
}

/// Calls made to the allocator by one thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocStats {
    /// Calls that requested new memory.
    pub allocations: u64,
    /// Calls that resized existing memory.
    pub reallocations: u64,
    /// Calls that returned memory.
    pub deallocations: u64,
    /// Bytes requested by allocations, plus the growth of reallocations.
    pub bytes_allocated: u64,
    /// Bytes returned by deallocations, plus the shrinkage of reallocations.
    pub bytes_freed: u64,
}

impl AllocStats {
    /// No calls at all.
    pub const ZERO: Self = Self {
        allocations: 0,
        reallocations: 0,
        deallocations: 0,
        bytes_allocated: 0,
        bytes_freed: 0,
    };

    /// Total number of calls to the allocator.
    #[must_use]
    pub const fn calls(self) -> u64 {
        self.allocations
            .saturating_add(self.reallocations)
            .saturating_add(self.deallocations)
    }

    /// What happened after `earlier` was taken.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            allocations: self.allocations.saturating_sub(earlier.allocations),
            reallocations: self.reallocations.saturating_sub(earlier.reallocations),
            deallocations: self.deallocations.saturating_sub(earlier.deallocations),
            bytes_allocated: self.bytes_allocated.saturating_sub(earlier.bytes_allocated),
            bytes_freed: self.bytes_freed.saturating_sub(earlier.bytes_freed),
        }
    }

    /// Bytes allocated and not freed again, when the two are compared over
    /// the same span: what the measured code still holds on the heap.
    ///
    /// Zero when more was freed than allocated, which happens when the span
    /// frees memory that was allocated before it started.
    #[must_use]
    pub const fn live_bytes(self) -> u64 {
        self.bytes_allocated.saturating_sub(self.bytes_freed)
    }
}

/// A global allocator that counts calls and forwards them to `A`.
#[derive(Debug, Default)]
pub struct CountingAllocator<A = System> {
    inner: A,
}

impl CountingAllocator<System> {
    /// Counts calls to the system allocator.
    #[must_use]
    pub const fn system() -> Self {
        Self::new(System)
    }
}

impl<A> CountingAllocator<A> {
    /// Counts calls to `inner`.
    #[must_use]
    pub const fn new(inner: A) -> Self {
        Self { inner }
    }
}

fn record(change: impl FnOnce(&mut AllocStats)) {
    // While a thread shuts down its storage may already be gone. Calls made
    // that late are not counted; nothing can be measuring them any more.
    let _ = STATS.try_with(|cell| {
        let mut stats = cell.get();
        change(&mut stats);
        cell.set(stats);
    });
}

fn bytes(size: usize) -> u64 {
    u64::try_from(size).unwrap_or(u64::MAX)
}

#[expect(
    unsafe_code,
    reason = "a global allocator can only be written with `unsafe`"
)]
// SAFETY: every method forwards its arguments unchanged to `inner`, which
// upholds the `GlobalAlloc` contract. The bookkeeping around each call only
// updates a thread-local counter: it does not allocate and cannot unwind.
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(|stats| {
            stats.allocations = stats.allocations.saturating_add(1);
            stats.bytes_allocated = stats.bytes_allocated.saturating_add(bytes(layout.size()));
        });
        // SAFETY: the caller guarantees what `GlobalAlloc::alloc` requires of
        // `layout`, and it is passed on unchanged.
        unsafe { self.inner.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(|stats| {
            stats.allocations = stats.allocations.saturating_add(1);
            stats.bytes_allocated = stats.bytes_allocated.saturating_add(bytes(layout.size()));
        });
        // SAFETY: the caller guarantees what `GlobalAlloc::alloc_zeroed`
        // requires of `layout`, and it is passed on unchanged.
        unsafe { self.inner.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(|stats| {
            stats.deallocations = stats.deallocations.saturating_add(1);
            stats.bytes_freed = stats.bytes_freed.saturating_add(bytes(layout.size()));
        });
        // SAFETY: the caller guarantees that `ptr` came from this allocator
        // with `layout`. Every allocation here was made by `inner`, so the
        // same holds for `inner`.
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(|stats| {
            stats.reallocations = stats.reallocations.saturating_add(1);
            let growth = bytes(new_size.saturating_sub(layout.size()));
            let shrinkage = bytes(layout.size().saturating_sub(new_size));
            stats.bytes_allocated = stats.bytes_allocated.saturating_add(growth);
            stats.bytes_freed = stats.bytes_freed.saturating_add(shrinkage);
        });
        // SAFETY: the caller guarantees that `ptr` came from this allocator
        // with `layout` and that `new_size` is valid for it. Every allocation
        // here was made by `inner`, so the same holds for `inner`.
        unsafe { self.inner.realloc(ptr, layout, new_size) }
    }
}

/// The calls the current thread has made to the allocator so far.
///
/// Always [`AllocStats::ZERO`] when no [`CountingAllocator`] is installed.
#[must_use]
pub fn snapshot() -> AllocStats {
    STATS.try_with(Cell::get).unwrap_or(AllocStats::ZERO)
}

/// Whether a [`CountingAllocator`] is the global allocator of this program.
#[must_use]
pub fn is_installed() -> bool {
    let before = snapshot();
    // `black_box` keeps the compiler from removing the allocation.
    drop(black_box(Box::new(0_u8)));
    snapshot() != before
}

/// Runs `f` and reports the calls it made to the allocator on this thread.
///
/// The counts are all zero when no [`CountingAllocator`] is installed; use
/// [`assert_no_alloc`] or [`is_installed`] when that must not go unnoticed.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
    let before = snapshot();
    let result = f();
    (result, snapshot().since(before))
}

/// Runs `f` and fails unless it left the heap alone.
///
/// Allocating, resizing and freeing all count: each is a call into the
/// allocator.
///
/// # Panics
///
/// Panics when `f` calls the allocator on this thread, and when no
/// [`CountingAllocator`] is installed, since nothing would be counted.
#[track_caller]
pub fn assert_no_alloc<R>(f: impl FnOnce() -> R) -> R {
    assert!(
        is_installed(),
        "CountingAllocator is not installed: add `#[global_allocator] static ALLOCATOR: \
         CountingAllocator = CountingAllocator::system();` to this test binary"
    );
    let (result, stats) = measure(f);
    assert!(
        stats == AllocStats::ZERO,
        "expected no calls to the allocator, found {stats:?}"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::AllocStats;

    #[test]
    fn since_subtracts_field_by_field() {
        let earlier = AllocStats {
            allocations: 1,
            reallocations: 2,
            deallocations: 3,
            bytes_allocated: 40,
            bytes_freed: 10,
        };
        let later = AllocStats {
            allocations: 4,
            reallocations: 2,
            deallocations: 5,
            bytes_allocated: 100,
            bytes_freed: 30,
        };
        let delta = later.since(earlier);
        assert_eq!(
            delta,
            AllocStats {
                allocations: 3,
                reallocations: 0,
                deallocations: 2,
                bytes_allocated: 60,
                bytes_freed: 20,
            }
        );
        assert_eq!(delta.live_bytes(), 40);
        assert_eq!(delta.calls(), 5);
        assert_eq!(AllocStats::default(), AllocStats::ZERO);
    }
}
