//! The Waveport daemon: agents that are reached over HTTP.
//!
//! Stability: experimental
//!
//! A daemon reads its agents from a file ([`Config`]), puts each
//! together once, and runs their sessions for whoever asks over the
//! control API ([`waveport_protocol::control::v1`]). The far end of a
//! session speaks the client protocol
//! ([`waveport_protocol::client::v1`]): what the server says of it is a
//! stream of server-sent events, and what the client says is posted.
//!
//! Here a session is one of text: it is followed and written to, and is
//! on no line. The same session, begun over a line that carries audio,
//! hears and speaks, and the lines are what comes next.
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
//! A daemon that listens on this machine alone may be given no control
//! key, and then lets in whoever is on the machine. It still looks at
//! where a request says it is going and where it comes from: a page in
//! a browser on the same machine is not on the machine, and a name that
//! was pointed at it is not its name. A daemon that listens anywhere
//! else does not start without a key.
//!
//! # Stopping
//!
//! A daemon that is told to stop starts nothing more, lets the answers
//! that are under way come to their end for a while, tells whoever
//! follows a session that it is going away, and stops. What its
//! sessions had stored is in the journal, and a session is gone on with
//! by the next daemon when its far end comes back.

mod auth;
pub mod config;
mod daemon;
mod events;
mod http;
mod serve;

pub use crate::config::Config;
pub use crate::daemon::{Daemon, Running};
