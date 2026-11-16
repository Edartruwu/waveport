//! Waveport session runtime.
//!
//! Stability: experimental
//!
//! This crate runs sessions: it owns the executor shards, the per-session
//! actor that drives the kernel, the drivers that translate kernel commands
//! into port calls, and the compute plane that batches local models across
//! sessions. It depends on the domain and the ports, never on an adapter.

// A build with `--cfg waveport_loom` is of `handoff` alone: it tries every
// way the threads of the compute plane can take turns, and the rest of the
// crate has no part in that.
#[cfg(not(waveport_loom))]
pub mod codec;
#[cfg(not(waveport_loom))]
pub mod compute;
#[cfg(not(waveport_loom))]
pub mod context;
#[cfg(not(waveport_loom))]
pub mod edges;
mod handoff;
#[cfg(not(waveport_loom))]
pub mod session;
#[cfg(not(waveport_loom))]
pub mod shard;
#[cfg(not(waveport_loom))]
pub mod tools;
#[cfg(not(waveport_loom))]
mod watch;
