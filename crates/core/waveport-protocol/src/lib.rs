//! Waveport wire formats.
//!
//! Stability: experimental
//!
//! This crate defines what crosses a process boundary: the client protocol,
//! the control API, the out-of-process adapter protocol and the journal
//! record format. It is a leaf: it depends on no other workspace crate, so
//! client SDKs can use it without linking the session kernel.
//!
//! Of these, there are the client protocol in [`client`], the bodies of
//! the control API in [`control`], and the journal record format in
//! [`journal`].
//!
//! A reader of any of them skips what it does not know, so each can grow
//! without a new version, and each says the version it is in.

pub mod client;
pub mod control;
pub mod journal;
