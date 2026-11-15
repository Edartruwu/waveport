//! Waveport session runtime.
//!
//! Stability: experimental
//!
//! This crate runs sessions: it owns the executor shards, the per-session
//! actor that drives the kernel, the drivers that translate kernel commands
//! into port calls, and the compute plane that batches local models across
//! sessions. It depends on the domain and the ports, never on an adapter.

pub mod codec;
pub mod compute;
pub mod context;
pub mod edges;
pub mod session;
