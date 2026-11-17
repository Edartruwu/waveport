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
//! - [`mod@line`] is what the transports share that carry audio over a
//!   connection of messages: the responses a far end was sent, and its
//!   audio cut into the packets a session takes.
//! - [`client`] is what the transports share whose far end speaks
//!   client protocol v1: what the two ends say as a connection begins,
//!   and its messages read and written.
//! - [`sse`] reads the event streams language models answer with.
//! - [`timeline`] turns a speech recogniser's times into the session's.
//! - [`turns`] adds up what a session says of its turns into how long
//!   each part took, for whoever reports on sessions.
//! - [`pcm`] puts samples together from audio that arrives cut anywhere,
//!   and reads and writes audio that travels as text.
//! - [`wav`] reads and writes the files audio is kept in.
//! - [`secret`] keeps keys out of logs.

pub mod client;
pub mod http;
pub mod line;
pub mod pcm;
pub mod pool;
pub mod retry;
pub mod secret;
pub mod sse;
pub mod stream;
pub mod timeline;
pub mod tls;
pub mod turns;
pub mod wav;
pub mod ws;
