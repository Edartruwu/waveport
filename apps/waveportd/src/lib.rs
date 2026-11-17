//! The Waveport daemon: agents that are reached over HTTP.
//!
//! Stability: experimental
//!
//! A daemon reads its agents from a file ([`Config`]), puts each
//! together once, and runs their sessions for whoever asks over the
//! control API ([`waveport_protocol::control::v1`]). The far end of a
//! session speaks the client protocol
//! ([`waveport_protocol::client::v1`]), in one of two ways, or is a
//! caller on the telephone.
//!
//! - **On no line.** What the server says is a stream of server-sent
//!   events, and what the client says is posted. Such a session reads
//!   and writes.
//! - **On a line.** The far end is on a WebSocket (`GET /v1/client`)
//!   that carries the whole protocol, and audio if its hello names any.
//!   Such a session hears and speaks if its agent does either, and
//!   reads and writes over the line otherwise. A client whose
//!   connection drops comes back and is on the line again.
//!
//! - **On a call.** A carrier asks the daemon what to do with a call
//!   (`POST /v1/telephony/{carrier}/voice`), and is answered with the
//!   address of a stream of the daemon's and a ticket that is good once
//!   (`GET /v1/telephony/{carrier}/stream`). The session of the call is
//!   started on that stream, and hears and speaks. Nobody comes back to
//!   a call.
//!
//! Which of these a session is, is decided by how its far end first
//! comes, and holds for all of its lives. The session that was made for
//! a call waits for the call.
//!
//! # Who may ask
//!
//! Three things let a request in, each said as `Authorization: Bearer`.
//!
//! - A **control key** lets its holder do everything: it is the
//!   application's, and stays on its server. The daemon is told the
//!   names of up to two environment variables that hold one, so that a
//!   key can be changed without a moment in which none is good.
//! - A **client token** is what the application is given with a new
//!   session, to hand to the far end. It lets its holder connect to that
//!   session, once, within a minute.
//! - A **resume token** is what the far end is given as it connects. It
//!   lets its holder follow that session and write to it, from then on
//!   and after the daemon was started again.
//!
//! A client on a WebSocket says its token in its first message, the
//! hello, and never in the address it asks for.
//!
//! A daemon that listens on this machine alone may be given no control
//! key, and then lets in whoever is on the machine. It still looks at
//! where a request says it is going and where it comes from: a page in
//! a browser on the same machine is not on the machine, and a name that
//! was pointed at it is not its name. A daemon that listens anywhere
//! else does not start without a key.
//!
//! # Calls
//!
//! A daemon answers the calls a carrier brings when its file says where
//! carriers reach it and which agent answers (`[telephony]`,
//! [`config::Telephony`]). What lets a carrier's request in is its
//! signature, made with the auth token of the account: the file names
//! the environment variable the token is in, and never holds it. A
//! request that was signed once is good again, since nothing in the
//! signature says when it was made, so a call is answered once: the
//! daemon remembers the calls it answered, and a stream is let in by a
//! ticket that only the answer to its call holds.
//!
//! None of this has been held against a carrier yet: it is written
//! from Twilio's documentation, and tested with a stand-in.
//!
//! # The page
//!
//! A daemon serves a page to try its agents on, at `/playground/`,
//! with the client for JavaScript beside it (`sdks/js`). Both are part
//! of the program: nothing is read from a disk or fetched when it runs.
//! The page asks as anyone asks, so it can do what whoever opens it
//! could do without it. A daemon that listens on this machine alone
//! serves it, and one that listens anywhere else only if its file says
//! so (`playground`).
//!
//! # Where its threads run
//!
//! Sessions run on threads of their own, the shards, and each session
//! stays on the one it was started on. A daemon can be told a processor
//! for each shard (`processors`, on Linux). The thread of a shard is
//! then kept on its processor, and every other thread of the daemon off
//! all of them ([`Threads`]). Nothing is kept anywhere unless the file
//! says where: only whoever runs the machine knows which processors are
//! whose.
//!
//! # Stopping
//!
//! A daemon that is told to stop starts nothing more, lets the answers
//! that are under way come to their end for a while, tells whoever
//! follows a session that it is going away, and stops. What its
//! sessions had stored is in the journal, and a session is gone on with
//! by the next daemon when its far end comes back.

mod auth;
mod calls;
pub mod config;
mod daemon;
mod events;
mod http;
mod line;
mod playground;
mod serve;
mod threads;

pub use crate::config::Config;
pub use crate::daemon::{Daemon, Running};
pub use crate::threads::Threads;
