//! Behaviour when the counting allocator is the global allocator.

use std::hint::black_box;

use waveport_alloc_counter::{
    AllocStats, CountingAllocator, assert_no_alloc, is_installed, measure,
};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::system();

#[test]
fn reports_that_it_is_installed() {
    assert!(is_installed());
}

#[test]
fn counts_an_allocation_and_its_size() {
    let (buffer, stats) = measure(|| black_box(Vec::<u8>::with_capacity(64)));
    assert_eq!(stats.allocations, 1);
    assert_eq!(stats.bytes_allocated, 64);
    assert_eq!(stats.deallocations, 0);
    drop(buffer);
}

#[test]
fn counts_zeroed_allocations() {
    let (buffer, stats) = measure(|| black_box(vec![0_u8; 128]));
    assert_eq!(stats.allocations, 1);
    assert_eq!(stats.bytes_allocated, 128);
    drop(buffer);
}

#[test]
fn counts_a_deallocation() {
    let buffer = black_box(Vec::<u8>::with_capacity(64));
    let ((), stats) = measure(|| drop(buffer));
    assert_eq!(
        stats,
        AllocStats {
            deallocations: 1,
            bytes_freed: 64,
            ..AllocStats::ZERO
        }
    );
}

#[test]
fn counts_a_reallocation_and_its_growth() {
    let mut buffer = black_box(Vec::<u8>::with_capacity(8));
    let ((), stats) = measure(|| buffer.reserve_exact(64));
    assert_eq!(stats.reallocations, 1);
    assert_eq!(stats.allocations, 0);
    assert_eq!(stats.bytes_allocated, 56);
    assert_eq!(stats.bytes_freed, 0);
}

#[test]
fn counts_the_shrinkage_of_a_reallocation() {
    let mut buffer = black_box(Vec::<u8>::with_capacity(64));
    buffer.push(1);
    let ((), stats) = measure(|| buffer.shrink_to(8));
    assert_eq!(stats.reallocations, 1);
    assert_eq!(stats.bytes_allocated, 0);
    assert_eq!(stats.bytes_freed, 56);
}

#[test]
fn live_bytes_is_what_the_measured_code_still_holds() {
    let (kept, stats) = measure(|| {
        let scratch = black_box(vec![0_u8; 1024]);
        drop(scratch);
        black_box(vec![0_u8; 256])
    });
    assert_eq!(stats.bytes_allocated, 1280);
    assert_eq!(stats.bytes_freed, 1024);
    assert_eq!(stats.live_bytes(), 256);
    drop(kept);
}

#[test]
fn code_that_stays_off_the_heap_passes() {
    let mut buffer = Vec::with_capacity(4);
    let sum = assert_no_alloc(|| {
        buffer.extend_from_slice(&[1_u32, 2, 3, 4]);
        buffer.iter().sum::<u32>()
    });
    assert_eq!(sum, 10);
}

#[test]
#[should_panic(expected = "expected no calls to the allocator")]
fn an_allocation_fails_the_assertion() {
    assert_no_alloc(|| black_box(Box::new(1_u8)));
}

#[test]
#[should_panic(expected = "expected no calls to the allocator")]
fn a_deallocation_fails_the_assertion() {
    let boxed = black_box(Box::new(1_u8));
    assert_no_alloc(|| drop(boxed));
}

#[test]
fn counts_are_kept_per_thread() {
    let ((), stats) = measure(|| {
        // The spawned thread allocates; this one only waits for it.
        std::thread::scope(|scope| {
            scope.spawn(|| drop(black_box(vec![0_u8; 4096])));
        });
    });
    // Starting a thread allocates on the spawning side, so the count is not
    // zero, but the 4096 bytes belong to the other thread.
    assert!(stats.bytes_allocated < 4096, "{stats:?}");
}
