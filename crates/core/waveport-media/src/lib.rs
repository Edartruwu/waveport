//! Waveport media path.
//!
//! Stability: experimental
//!
//! This crate holds the per-frame audio path: reordering, decoding,
//! resampling, pacing and the playout ledger. Like `waveport-domain` it
//! performs no I/O, reads no clock and starts no thread.
//!
//! Unlike the domain it links `std`, because it wraps DSP libraries that need
//! it. The purity rule is therefore enforced by the Clippy bans in this
//! crate's `clippy.toml` instead of by the compiler.
