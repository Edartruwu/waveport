//! A session whose far end connects over a WebSocket.
//!
//! Stability: experimental
//!
//! The transport of a session that runs in one process and is talked to
//! from another: a command at a developer's desk, a page in a browser, a
//! test that kills the server in the middle of a call. A [`Listener`]
//! takes connections, each connection that says hello becomes a
//! [`Caller`], and a caller becomes the [`WsTransport`] of one session.
//! [`Client`] is the other side.
//!
//! **This is not the protocol clients will be promised.** It is version
//! zero, it changes without notice, and it carries no key: a listener
//! takes only an address of this machine unless it is told otherwise.
//!
//! # The protocol
//!
//! A client connects to `/v0/session` and names the protocol
//! `waveport.ws.v0`. Text messages hold one JSON object each, and binary
//! messages hold audio: one channel of 16-bit samples, the low byte
//! first, at the rate the client named.
//!
//! | Who says it | Message | What it means |
//! |---|---|---|
//! | client | `{"type":"hello","rate":24000}` | The first message. The rate is 8,000, 16,000, 24,000 or 48,000, both ways |
//! | server | `{"type":"ready","session":"…","rate":24000}` | The session is there |
//! | client | binary | What the microphone heard, in pieces of any length up to a second |
//! | server | `{"type":"mark","run":7}` | The audio that follows is that of response 7 |
//! | server | binary | A piece of a response, 20 ms long, to be played after the piece before |
//! | server | `{"type":"mark","run":7,"end":true}` | All of response 7 has been sent |
//! | client | `{"type":"played","run":7,"samples":4800}` | How much of response 7 has been played so far |
//! | server | `{"type":"clear","run":7}` | Discard what is held of response 7, and of any before it, and has not been played |
//! | client | `{"type":"text","text":"…"}` | Something typed in place of speech |
//! | server | `{"type":"event","text":"…"}` | A line about the call, for a person to read |
//! | either | `{"type":"bye"}` | The call is over. A server gives a `reason` |
//!
//! What a client has to do for the session to know what was heard:
//!
//! - **Say what it played.** A session takes a response as heard up to
//!   what the client says it played, and waits for the client to say that
//!   it played all of it. A client says so as it plays, a few times a
//!   second, and once more when a response has been played to its end.
//! - **Discard at once.** After `clear` the client plays nothing more of
//!   that response, and says how much of it had been played.
//! - **Send its audio all the time,** silence too, at the pace it is
//!   heard. A client that falls more than a second behind the clock is
//!   taken to have paused: what it sends next is taken as heard when it
//!   arrives.
//!
//! A connection that closes without a `bye` from the server means that
//! the server is gone, not that the call is over.

mod client;
mod server;
mod wire;

pub use crate::client::{Client, Heard};
pub use crate::server::{
    Caller, GREETING_TIMEOUT, Knock, Lines, ListenError, Listener, WsTransport,
};
pub use crate::wire::{FromClient, FromServer, PATH, PROTOCOL};
