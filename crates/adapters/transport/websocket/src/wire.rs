//! The connection as it is on the wire, and what its two ends say as it
//! begins.
//!
//! Everything that is not audio is a text message that holds one JSON
//! object: a message of client protocol v1. Audio is a binary message
//! that holds audio and nothing else.

pub use waveport_adapter_kit::client::{Hello, Welcome};
pub(crate) use waveport_adapter_kit::client::{read, write};
use waveport_protocol::client::v1 as client;

/// The name of the protocol, which a client gives as it connects.
pub const PROTOCOL: &str = client::NAME;

/// The path a client asks for.
pub const PATH: &str = "/v1/client";
