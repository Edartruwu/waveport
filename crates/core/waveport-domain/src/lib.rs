//! Waveport domain model and session kernel.
//!
//! Stability: experimental
//!
//! This crate holds the rules of a conversation: turns, interruptions, tool
//! calls and the context they produce. It is pure: it performs no I/O, reads
//! no clock and starts no thread. Time arrives as an argument and effects
//! leave as commands.
//!
//! The crate is `no_std` so that the compiler enforces that purity. It is
//! built for a target without `std` in CI; anything that needs the operating
//! system belongs in `waveport-runtime` or behind a port.
#![no_std]

extern crate alloc;
