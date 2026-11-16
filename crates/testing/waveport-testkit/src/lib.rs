//! Waveport test kit.
//!
//! Stability: experimental
//!
//! Shared helpers for testing Waveport crates:
//!
//! - [`fakes`]: a part for every port that follows a script, with no
//!   provider and no network behind it.
//! - [`conformance`]: the rules every adapter of a port has to keep,
//!   written once as checks that run against any of them: the fakes
//!   here, an adapter talking to a recorded provider, or one talking to
//!   the real thing.
//! - [`drive`]: taking the next event of a port, as a future.
//! - The allocation counter, re-exported.

pub mod conformance;
pub mod drive;
pub mod fakes;

pub use waveport_alloc_counter::{
    AllocStats, CountingAllocator, assert_no_alloc, is_installed, measure, snapshot,
};
