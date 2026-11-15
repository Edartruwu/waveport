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
//!
//! Audio in this crate is one channel of 16-bit samples at one of four
//! [rates](rate::Rate). Time is always an argument.

pub mod codec;
pub mod gain;
pub mod ingress;
pub mod ledger;
pub mod playout;
pub mod rate;
pub mod resample;
pub mod ring;

#[cfg(test)]
mod testing;
