//! Waveport adapter kit.
//!
//! Stability: experimental
//!
//! What provider adapters share, since one adapter may not depend on
//! another: connection plumbing, and the small parsers and types every
//! streaming client needs.
//!
//! - [`stream`] builds the two halves of a stream port from one ordinary
//!   loop.
//! - [`ws`] and [`http`] are the two kinds of connection providers offer,
//!   set up one way: the same TLS ([`tls`]), the same time limits, and
//!   failures already sorted into the classes the ports use
//!   ([`retry`]).
//! - [`pool`] keeps a WebSocket connection whose work is done for the
//!   work that comes next.
//! - [`sse`] reads the event streams language models answer with.
//! - [`timeline`] turns a speech recogniser's times into the session's.
//! - [`pcm`] puts samples together from audio that arrives cut anywhere,
//!   and reads and writes audio that travels as text.
//! - [`wav`] reads and writes the files audio is kept in.
//! - [`secret`] keeps keys out of logs.

pub mod http;
pub mod pcm;
pub mod pool;
pub mod retry;
pub mod secret;
pub mod sse;
pub mod stream;
pub mod timeline;
pub mod tls;
pub mod wav;
pub mod ws;
