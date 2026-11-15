//! Behaviour when the counting allocator is not the global allocator.
//!
//! Without it nothing is counted, so an assertion that passed silently would
//! prove nothing. It must fail instead.

use std::hint::black_box;

use waveport_alloc_counter::{AllocStats, assert_no_alloc, is_installed, measure};

#[test]
fn reports_that_it_is_not_installed() {
    assert!(!is_installed());
}

#[test]
fn measures_nothing() {
    let (buffer, stats) = measure(|| black_box(vec![0_u8; 64]));
    assert_eq!(stats, AllocStats::ZERO);
    drop(buffer);
}

#[test]
#[should_panic(expected = "CountingAllocator is not installed")]
fn the_assertion_refuses_to_pass() {
    assert_no_alloc(|| ());
}
