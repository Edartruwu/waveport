//! A session whose far end is a browser on a WebRTC call.
//!
//! Stability: experimental
//!
//! **This transport has not been held against a browser yet.** It has
//! been run against its own client ([`Dialer`]), over real sockets and
//! in memory, and that client offers what Chrome 157 and Firefox 158
//! offered when they were measured (decision 0005). What the two
//! browsers then do with the answers is tried when a server answers
//! their calls with it.
//!
//! The call itself is `str0m`'s: the checks of ICE, the handshake, the
//! keys the audio is sealed with and the data channel. What is here is
//! what a session needs of a call, and what `str0m` leaves to whoever
//! uses it: the sockets, the clock, and the ciphers.
//!
//! # What a call carries
//!
//! Opus, both ways, on the clock of 48 kHz; the keys of a telephone, as
//! events in the stream of the audio (RFC 4733) where the far end
//! offered them, and as tones in it where it did not; and, for a client
//! that speaks client protocol v1, a data channel that the client and
//! the server agree on before the call ([`CHANNEL`], by the name
//! [`CHANNEL_LABEL`]), so that neither waits for the other to open it.
//! The protocol is spoken there and on no other channel: one that a
//! client opens by itself is not answered on yet.
//!
//! A far end is one of two kinds ([`Client`]), which whoever answers
//! its offer says:
//!
//! - One that speaks the protocol says hello on the channel, is told
//!   of its session, and is told which response the audio it is sent is
//!   of: a `mark` says where in the stream a response begins and where
//!   it ends, by RTP time. It says what it has played as a place in
//!   that stream (`played_rtp`), and discards what it holds when it is
//!   told to (`clear`). So the transport says of it that it reports
//!   what was played and can discard.
//! - Any other is sent audio and keys, and told nothing. The transport
//!   says so, and a session estimates what such a far end has heard
//!   from the pace of what it was sent.
//!
//! # What the audio is made to survive
//!
//! - Where the far end offers redundant audio (RED, RFC 2198), the
//!   answer lists it first, which is what has Chrome send it, and every
//!   packet either way carries the frame before beside its own. Of two
//!   thousand frames on a line that lost a tenth, between one and two
//!   in a hundred did not arrive, where a tenth did not without it.
//! - The far end is told that Opus may carry a copy of the frame before
//!   in each packet. What this end sends carries one when the far end
//!   says it loses packets: the transport tells the session
//!   (`TransportEvent::Loss`), from what the far end says of every
//!   packet it was sent and from its receiver reports, whichever says
//!   more, and the session tells its encoder.
//! - Where no redundant audio is offered, the far end is asked to send
//!   again what did not arrive. **A packet that is sent again is not
//!   waited for yet**: it is taken if it comes before the frame it
//!   holds was made up for, and is of no use after.
//! - Audio is numbered for the session by its place in the stream's
//!   time, and not by the numbers of its packets. Padding, keys, a
//!   frame that arrives twice and a packet that was sent again take
//!   numbers of the stream, and none of them leaves a hole in the
//!   audio or is taken for a loss.
//!
//! # A line that drops, and a client that comes back
//!
//! A far end that nothing arrives from for five seconds has dropped:
//! the session is told, and told again when it is back. `str0m` says so
//! itself only after fifteen.
//!
//! A client whose network changed offers again where it made its offer
//! ([`Signals::offer`]): its checks begin anew from where it is now,
//! and the call goes on as it was. A client that makes another call
//! instead is attached to the transport of its session
//! ([`Handle::attach`]) and takes the place of the call before, which
//! is told so. One whose call ended before it was attached takes
//! nobody's place.
//!
//! # How a call ends
//!
//! A far end that is told goodbye is served until it says that it has
//! what it was told, or hangs up, and for a third of a second at most.
//! The end of a call is another datagram than the goodbye and can
//! overtake it, and a far end that sees the end first was not told why.
//! A goodbye that is lost is not sent again in so short a time: its far
//! end finds its call ended with no word, which a client of the
//! protocol takes for a line that dropped, and comes back.
//!
//! A call that is let go of in any other way is ended at once, so that
//! its far end is not left to find it silent. A client that comes to a
//! transport as its session ends is told that there is no session.
//!
//! # For a server
//!
//! One UDP socket takes the datagrams of every call, and a listener
//! takes the connections of far ends whose network lets no datagram out
//! (RFC 6544, RFC 4571). The server only answers the checks of ICE and
//! makes none (ICE lite), so it has to be reached at an address a far
//! end can send to: one address ([`Config::reached_at`]).
//!
//! ```text
//! let (endpoint, pump) = Endpoint::bind(config).await?;
//! spawn(pump.run());                                   // carries datagrams to their calls
//! let answered = endpoint.offer(&offer, Client::V1)?;  // the answer goes back to the far end
//! let caller = answered.knock.connect(timeout).await?; // checks, handshake, hello
//! let (transport, handle) = caller.transport(who);     // nobody is on it yet
//! // ... start the session with the transport ...
//! let link = handle.attach(caller, welcome).await?;    // the client is welcomed
//! ```
//!
//! Waiting for a far end ([`Knock::connect`]) is where the handshake is
//! made, which takes a few milliseconds of a processor: it is done by
//! whoever calls it, and not on the thread a session runs on. The
//! [`Link`] is the rest of the call: what the client is told of the
//! session, and what it asks that is not the transport's to do.
//!
//! What a hello holds is the server's to judge ([`Caller::hello`]):
//! this crate checks that a token looks like one, and never whether it
//! is good. **An endpoint asks nobody who they are**: whoever is given
//! an answer can call.
//!
//! The pump tells calls apart by where a datagram comes from, and
//! before that by the name the far end's checks are addressed to. A
//! name is no secret: it is in every check, in clear. So a call is
//! reached from an address, or over a connection, only once a check
//! from there knew the password of the call, and from anywhere else
//! nothing but such a check is shown to it. Somebody who knows the name
//! of a call can have what looks like a check carried to it, where it
//! is dropped. They cannot take the place of its far end, begin a
//! handshake in its stead, or make a far end that fell silent look as
//! if it were there. A far end whose address changes without its
//! offering again is heard again at its next check, a few seconds
//! later.
//!
//! Sixty-four connections may wait to say which call they are for, five
//! seconds each, and one more takes the place of the one that has
//! waited longest. **Nothing limits how many one address makes**: that
//! is for whatever stands before the server.
//!
//! # The ciphers
//!
//! `str0m` is given a provider written here, in Rust, so that nothing
//! of a call is C and nothing needs a C build. The handshake is DTLS
//! 1.2; 1.3 is a setting that is off ([`Config::dtls13`]). There is one
//! certificate for all the calls of an endpoint, made when it is.
//!
//! `str0m` 0.25.0 still stops the program on some things a far end can
//! send. Everything it is asked is asked where such a stop is caught: a
//! call it happens to is over, and its session is told that its line
//! dropped. The release after it takes those out.
//!
//! It also ends no call whose data channel is still being set up by the
//! end that ends it: it says that it cannot, and has told the far end
//! nothing by then. That is the client's end, since the client sets
//! the channel up. Such a call is over at once for whoever ended it,
//! and the other end finds the line silent.
//!
//! # What is not known
//!
//! - What a browser makes of a key that is pressed while audio is being
//!   sent: the events are sent beside the audio, which is not held back
//!   for them. A browser's own keys take the place of its audio.
//! - Safari, which was not measured.
//! - How much of what it was sent a far end has lately lost, where it
//!   says nothing of single packets and its receiver reports are five
//!   seconds apart: the figure is that old.
//!
//! [`Dialer`] is the other side, for a command and for tests, and
//! [`loopback`] a call with no socket at all, which the tests of this
//! crate lose packets on and the benchmarks count instructions on.

mod call;
mod crypto;
mod dial;
mod endpoint;
mod engine;
pub mod loopback;
mod media;
mod net;
mod peer;
mod red;
mod rtp;
mod sent;
mod server;
mod stack;
mod tcp;
#[cfg(test)]
mod tests;

pub use crate::call::{CHANNEL, CHANNEL_LABEL, Client, Unanswered};
pub use crate::dial::{Dialer, Dialing, Heard, Unconnected};
pub use crate::endpoint::{Answered, BindError, Config, Endpoint, Pump};
pub use crate::peer::Like;
pub use crate::server::{Caller, Gone, Handle, Knock, Link, RtcTransport, Signals, Unattached};
