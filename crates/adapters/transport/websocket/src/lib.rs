//! A session whose far end connects over a WebSocket.
//!
//! Stability: experimental
//!
//! The transport of a session that runs in one process and is talked to
//! from another: a command at a developer's desk, a page in a browser, a
//! test that kills the server in the middle of a call. It speaks client
//! protocol v1 ([`waveport_protocol::client::v1`]), which says what
//! every message means. This page says how the protocol is carried
//! here, and what a server does with this crate.
//!
//! # On the wire
//!
//! A client connects to `/v1/client` and names the subprotocol
//! `waveport.client.v1`. A text message holds one message of the
//! protocol as JSON. A binary message holds audio, as the client's
//! `hello` said it would be written, the same both ways:
//!
//! | `audio.codec` | A binary message holds |
//! |---|---|
//! | `pcm16` | One channel of 16-bit samples, the low byte first, at `audio.rate`. From the client: any whole number of samples, up to a second. To the client: 20 ms |
//! | `opus` | One Opus packet, of 120 ms at most. `audio.rate` is the rate the client plays and records at, and what it says it played is counted at that rate |
//!
//! A hello with no `audio` is a line of text: nothing binary is taken,
//! and none is sent.
//!
//! What a client has to do for the session to know what was heard:
//!
//! - **Say what it played.** A session takes a response as heard up to
//!   what the client says it played (`played`), and waits for the client
//!   to say that it played all of it. A client says so as it plays, a
//!   few times a second, and once more when a run has been played to its
//!   end.
//! - **Discard at once.** After `clear` the client plays nothing more of
//!   that run, and says how much of it had been played.
//! - **Send its audio all the time,** silence too, at the pace it is
//!   heard. A client that falls more than a second behind the clock is
//!   taken to have paused: what it sends next is taken as heard when it
//!   arrives.
//!
//! A message that cannot be taken is answered with an `error`, and the
//! connection goes on. Of the protocol's messages one is not taken
//! here: `played_rtp`, which is how a WebRTC call says what it played.
//!
//! # A connection that drops
//!
//! A connection that ends without a `bye` from either side has dropped.
//! The session is not over: the transport tells it that the line is
//! gone ([`TransportEvent::Detached`]), and the session decides how
//! long to wait. The client comes back on a new connection and says
//! where it was (`hello.resume`), and whoever serves it attaches it to
//! the transport of its session, which tells the session that the line
//! is back ([`TransportEvent::Attached`]). Run numbers go on from where
//! they were, and the audio that follows says again whose it is.
//!
//! A client that is attached while another is still there takes its
//! place: the one before is told `bye` with `replaced`.
//!
//! # For a server
//!
//! A [`Listener`] takes connections, and each connection that says hello
//! becomes a [`Caller`]. A server that has an HTTP server of its own
//! hands the upgraded connection to [`Caller::of`] instead. What the
//! hello holds is the server's to judge ([`Caller::hello`]): this crate
//! checks that a token looks like one, and never whether it is good.
//!
//! ```text
//! let caller = listener.accept().await?.greet(GREETING_TIMEOUT).await?;
//! let (transport, handle) = caller.transport(who);   // nobody is on it yet
//! // ... start the session with the transport ...
//! let link = handle.attach(caller, welcome).await?;   // the client is welcomed
//! // ... and when it comes back on another connection:
//! let link = handle.attach(caller_again, welcome).await?;
//! ```
//!
//! The [`Link`] is the rest of the connection: what the client is told
//! of the session (`fact`, `told`, `gap`, `state`), which whoever
//! follows the session says through it, and what the client asks that
//! is not the transport's to do (`interrupt`). When the session ends,
//! the client is told `bye` once the link is let go of, so that what is
//! told last, how the session ended, is told before the goodbye.
//!
//! [`Client`] is the other side, for a command and for tests.
//!
//! **A listener asks nobody who they are.** It takes only an address of
//! this machine unless it is told otherwise.
//!
//! [`TransportEvent::Detached`]: waveport_ports::transport::TransportEvent::Detached
//! [`TransportEvent::Attached`]: waveport_ports::transport::TransportEvent::Attached

mod client;
mod server;
mod wire;

pub use crate::client::{Client, Heard, Unconnected};
pub use crate::server::{
    Caller, GREETING_TIMEOUT, Gone, Handle, Knock, Link, ListenError, Listener, Unattached,
    WsTransport,
};
pub use crate::wire::{Hello, PATH, PROTOCOL, Welcome};
