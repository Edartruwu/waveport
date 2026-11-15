//! Waveport test kit.
//!
//! Stability: experimental
//!
//! Shared helpers for testing Waveport crates. Today it re-exports the
//! allocation counter; fakes for the ports and other fixtures are added with
//! the code they test.

pub use waveport_alloc_counter::{
    AllocStats, CountingAllocator, assert_no_alloc, is_installed, measure, snapshot,
};
