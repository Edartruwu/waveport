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
//!
//! Start at [`kernel::Kernel`]. It takes an [`event::Event`] and fills an
//! [`out::Out`] with [`command::Command`]s to carry out and [`fact::Fact`]s
//! to journal. The decisions an application may replace are in [`policy`].
#![no_std]

extern crate alloc;

pub mod bounded;
pub mod command;
pub mod config;
pub mod event;
pub mod fact;
pub mod ids;
pub mod kernel;
pub mod out;
pub mod policy;
pub mod text;
pub mod time;
