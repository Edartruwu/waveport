//! Waveport wire formats.
//!
//! Stability: experimental
//!
//! This crate defines what crosses a process boundary: the client protocol,
//! the control API, the out-of-process adapter protocol and the journal
//! record format. It is a leaf: it depends on no other workspace crate, so
//! client SDKs can use it without linking the session kernel.
//!
//! Of these, phase 1 has the journal record format, in [`journal`].

pub mod journal;
