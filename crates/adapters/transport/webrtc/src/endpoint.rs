//! The server's sockets: one for the datagrams of every call, and a
//! listener for the calls that can send none.
//!
//! Every call's datagrams arrive on one UDP socket. A [`Pump`] reads it
//! and gives each datagram to the call it is for, which it knows in
//! two ways: by where the datagram comes from, once a call was reached
//! from there, and before that by the name the far end's checks are
//! addressed to, which is in the answer the far end was given and in
//! every check it sends. The pump checks nothing else. Whether a
//! datagram is the far end's is for the call to say, which has the
//! keys. So a call is reached from an address, or over a connection,
//! only once the call has said that a check from there was its own:
//! somebody who knows a call's name and not its password can have what
//! looks like a check given to the call, where it is dropped, and can
//! do no more. They cannot take the place of where the far end is, and
//! they do not make a far end that fell silent look as if it were
//! there.
//!
//! A call that is behind is not waited for: a datagram that finds its
//! call with too many waiting is dropped, as a network would drop it.
//!
//! A far end that sends no datagrams makes connections, and makes more
//! than one. A browser makes one from every address its machine has,
//! says its checks on each, sends on one of them, and keeps the others
//! for as long as the call lasts. A call therefore keeps every
//! connection its far end made, and says a thing on the one it is to
//! go over. One that is kept in place of another, and not beside it,
//! closes the connection the far end is using.
//!
//! A call that is over hands its connections to the pump, which lets go
//! of each as one that is closed and not as one that broke. What a call
//! says last is its goodbye, and its far end sends all the time: a
//! connection that is closed while something is unread is reset by the
//! system, and a reset can take what was just said with it.

use core::fmt;
use core::future::poll_fn;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use str0m::net::{Protocol, Transmit};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::call::{Call, Client, Setup, Unanswered};
use crate::crypto::{Certificate, certificate};
use crate::net::{Arrived, Driven, Wire, now};
use crate::rtp::{Datagram, asked_of};
use crate::server::{Knock, Signals};
use crate::tcp::Framed;

/// How many datagrams may wait for a call. A far end sends fifty a
/// second, so it is what arrives in a few seconds.
const INBOX: usize = 256;

/// How many addresses a call may be reached from at once: a far end
/// tries every way it has.
const PEERS: usize = 8;

/// How many connections a call may have at once. A far end makes one
/// from every address it has, so it is as many as the addresses a call
/// may be reached from. One that comes when a call has as many takes
/// the place of the one the call has heard nothing on for longest: the
/// one the far end sends on is never that one.
const CONNECTIONS: usize = PEERS;

/// How many names a call may have at once: it gets another each time
/// its far end begins its checks anew, and checks to the one before
/// may still be on their way.
const NAMES: usize = 4;

/// How many connections may wait to say which call they are for. One
/// that comes when as many are waiting takes the place of the one that
/// has waited longest: a far end says which call it is for as soon as
/// it has connected, so whoever only holds places has to take them all
/// again and again to be in its way.
const WAITING: usize = 64;

/// How long a connection may take to say which call it is for.
const FIRST_WORD: Duration = Duration::from_secs(5);

/// How many connections of calls that are over may be on their way out
/// at once. One that comes when as many are is closed where it stands.
const LEAVING: usize = 64;

/// How long the connection of a call that is over is given to end as
/// one that was closed: for what was said last on it to arrive, and for
/// its far end to close it too.
const LEAVES_WITHIN: Duration = Duration::from_secs(1);

/// The longest datagram that is read whole.
const LONGEST: usize = 2048;

/// How many datagrams are carried before whatever else the thread has
/// to do gets a turn.
const AT_ONCE: usize = 64;

/// Where a server takes its calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Where the datagrams of every call are taken. Port 0 is any port
    /// that is free.
    pub udp: SocketAddr,
    /// Where connections are taken, for the calls of far ends whose
    /// network lets no datagram out. `None` for nowhere.
    pub tcp: Option<SocketAddr>,
    /// The address far ends are told to reach the server at, where it
    /// is not the one above: for a server that takes datagrams at every
    /// address it has, or that is behind an address of somebody else's.
    /// It is one address. A server with several is reached at this one.
    pub reached_at: Option<IpAddr>,
    /// Whether a far end that can may make its handshake in DTLS 1.3.
    /// It is off unless it is set: the library the handshake is made
    /// with has faults in it that are open.
    pub dtls13: bool,
}

impl Config {
    /// Datagrams at `udp`, and nothing else.
    #[must_use]
    pub const fn at(udp: SocketAddr) -> Self {
        Self {
            udp,
            tcp: None,
            reached_at: None,
            dtls13: false,
        }
    }
}

/// Why a server's sockets could not be set up.
#[derive(Debug)]
pub enum BindError {
    /// The address is every address of the machine, and nobody said at
    /// which one far ends reach it ([`Config::reached_at`]).
    Nowhere,
    /// No certificate could be made for the handshakes.
    Certificate,
    /// The system refused.
    System(io::Error),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nowhere => f.write_str(
                "calls are taken at every address of the machine, and nothing says at which \
                 one a far end is to reach them",
            ),
            Self::Certificate => f.write_str("no certificate could be made for the calls"),
            Self::System(error) => write!(f, "the address cannot be used: {error}"),
        }
    }
}

impl core::error::Error for BindError {}

/// What arrives for a call.
enum Inbound {
    Datagram {
        from: SocketAddr,
        datagram: Bytes,
        /// Whether the call is already reached from there. A datagram
        /// that is not found its call by the name in it.
        reached: bool,
    },
    /// A connection the far end made, and the first thing it said on
    /// it.
    Connection(Box<Framed>, Bytes),
}

/// A call, as the pump knows it.
struct Known {
    inbox: mpsc::Sender<Inbound>,
    names: Vec<String>,
    peers: Vec<SocketAddr>,
}

/// Which call a datagram is for.
#[derive(Default)]
struct Table {
    calls: HashMap<u64, Known>,
    by_name: HashMap<String, u64>,
    by_peer: HashMap<SocketAddr, u64>,
}

impl Table {
    /// The call `id` is known by `name` from here on, too.
    fn name(&mut self, id: u64, name: &str) {
        let Some(known) = self.calls.get_mut(&id) else {
            return;
        };
        if known.names.iter().any(|known| known == name) {
            return;
        }
        if known.names.len() >= NAMES {
            let oldest = known.names.remove(0);
            self.by_name.remove(&oldest);
        }
        known.names.push(name.to_owned());
        self.by_name.insert(name.to_owned(), id);
    }

    /// The call `id` is reached from `peer`.
    fn reach(&mut self, id: u64, peer: SocketAddr) {
        match self.by_peer.insert(peer, id) {
            Some(before) if before == id => return,
            // The address is another call's now.
            Some(before) => {
                if let Some(known) = self.calls.get_mut(&before) {
                    known.peers.retain(|known| *known != peer);
                }
            }
            None => {}
        }
        let Some(known) = self.calls.get_mut(&id) else {
            self.by_peer.remove(&peer);
            return;
        };
        if known.peers.len() >= PEERS {
            let oldest = known.peers.remove(0);
            self.by_peer.remove(&oldest);
        }
        known.peers.push(peer);
    }

    /// The call a check is addressed to.
    fn named(&self, datagram: &[u8]) -> Option<u64> {
        if Datagram::of(datagram) != Datagram::Stun {
            return None;
        }
        self.by_name.get(asked_of(datagram)?).copied()
    }

    fn forget(&mut self, id: u64) {
        let Some(known) = self.calls.remove(&id) else {
            return;
        };
        for name in known.names {
            self.by_name.remove(&name);
        }
        for peer in known.peers {
            self.by_peer.remove(&peer);
        }
    }
}

/// What a server's calls share.
struct Shared {
    udp: UdpSocket,
    /// Where far ends are told to send their datagrams.
    udp_at: SocketAddr,
    /// And where to connect.
    tcp_at: Option<SocketAddr>,
    certificate: Certificate,
    dtls13: bool,
    table: Mutex<Table>,
    /// The number of the next call.
    next: AtomicU64,
    /// Where the connection of a call that is over goes, to be let go
    /// of by what carries the server's arrivals.
    leaving: mpsc::Sender<Box<Framed>>,
}

impl Shared {
    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Gives a datagram to the call it is for, if it is for one.
    fn carry(&self, from: SocketAddr, datagram: &[u8]) {
        let table = self.table();
        let there = table.by_peer.get(&from).copied();
        // A check says which call it is for, wherever it comes from.
        // Whether the call is reached from there is for the call to
        // say, when it has looked at the check.
        let id = table.named(datagram).or(there);
        if let Some(known) = id.and_then(|id| table.calls.get(&id)) {
            let arrived = Inbound::Datagram {
                from,
                datagram: Bytes::copy_from_slice(datagram),
                reached: id == there,
            };
            // A call that is behind loses it.
            let _ = known.inbox.try_send(arrived);
        }
    }

    /// Gives a connection to the call that the first thing said on it
    /// is addressed to. One that is addressed to no call is let go of.
    fn connect(&self, connection: Box<Framed>, first: Bytes) {
        let table = self.table();
        if let Some(known) = table.named(&first).and_then(|id| table.calls.get(&id)) {
            let _ = known.inbox.try_send(Inbound::Connection(connection, first));
        }
    }
}

/// Where a server's calls are taken.
#[derive(Clone)]
pub struct Endpoint {
    shared: Arc<Shared>,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("udp", &self.shared.udp_at)
            .field("tcp", &self.shared.tcp_at)
            .finish_non_exhaustive()
    }
}

/// An offer that was answered.
#[derive(Debug)]
pub struct Answered {
    /// The answer, for the far end.
    pub answer: String,
    /// The call, which has not begun.
    pub knock: Knock,
    /// What the far end says of the call later, where it made its
    /// offer.
    pub signals: Signals,
}

impl Endpoint {
    /// Sets up the sockets. Calls are taken once the [`Pump`] runs.
    ///
    /// # Errors
    ///
    /// The address is every address of the machine and nothing says
    /// where far ends reach it, no certificate could be made, or the
    /// system refused.
    pub async fn bind(config: Config) -> Result<(Self, Pump), BindError> {
        let certificate = certificate().ok_or(BindError::Certificate)?;
        let udp = UdpSocket::bind(config.udp)
            .await
            .map_err(BindError::System)?;
        let bound = udp.local_addr().map_err(BindError::System)?;
        let at = config.reached_at.unwrap_or_else(|| bound.ip());
        if at.is_unspecified() {
            return Err(BindError::Nowhere);
        }
        let (tcp, tcp_at) = match config.tcp {
            Some(address) => {
                let tcp = TcpListener::bind(address)
                    .await
                    .map_err(BindError::System)?;
                let port = tcp.local_addr().map_err(BindError::System)?.port();
                (Some(tcp), Some(SocketAddr::new(at, port)))
            }
            None => (None, None),
        };
        let (leaving, left) = mpsc::channel(LEAVING);
        let shared = Arc::new(Shared {
            udp,
            udp_at: SocketAddr::new(at, bound.port()),
            tcp_at,
            certificate,
            dtls13: config.dtls13,
            table: Mutex::new(Table::default()),
            next: AtomicU64::new(1),
            leaving,
        });
        let pump = Pump {
            shared: Arc::clone(&shared),
            tcp,
            left,
        };
        Ok((Self { shared }, pump))
    }

    /// Where far ends are told to send their datagrams.
    #[must_use]
    pub fn udp(&self) -> SocketAddr {
        self.shared.udp_at
    }

    /// Where far ends are told to connect, if they are told to.
    #[must_use]
    pub fn tcp(&self) -> Option<SocketAddr> {
        self.shared.tcp_at
    }

    /// Answers the offer of a far end that is a `client` of this kind.
    /// The answer is for whoever carried the offer to give back; the
    /// call begins when the far end has it ([`Knock::connect`]).
    ///
    /// # Errors
    ///
    /// The offer is not one that can be answered. Nothing is said of
    /// what it held.
    pub fn offer(&self, offer: &str, client: Client) -> Result<Answered, Unanswered> {
        let setup = Setup {
            certificate: &self.shared.certificate,
            dtls13: self.shared.dtls13,
            client,
            udp: Some(self.shared.udp_at),
            tcp: self.shared.tcp_at,
        };
        let (call, answer) = Call::answer(&setup, offer, now())?;
        let (inbox, arrives) = mpsc::channel(INBOX);
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed);
        {
            let mut table = self.shared.table();
            let known = Known {
                inbox,
                names: Vec::new(),
                peers: Vec::new(),
            };
            table.calls.insert(id, known);
            table.name(id, call.name());
        }
        let line = Line::of(Arc::clone(&self.shared), id, arrives);
        let (knock, signals) = Knock::of(Driven::new(call, line));
        Ok(Answered {
            answer,
            knock,
            signals,
        })
    }

    /// How many calls there are: begun or not, and not yet let go of.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.shared.table().calls.len()
    }

    /// The names its calls are known by.
    #[cfg(test)]
    pub(crate) fn names(&self) -> Vec<String> {
        self.shared.table().by_name.keys().cloned().collect()
    }

    /// Where its calls are reached from.
    #[cfg(test)]
    pub(crate) fn reached_from(&self) -> Vec<SocketAddr> {
        self.shared.table().by_peer.keys().copied().collect()
    }
}

/// A connection the far end of a call made.
struct Held {
    connection: Box<Framed>,
    /// When something last arrived on it, by the call's count of what
    /// arrived over its connections.
    heard: u64,
}

/// The sockets of one call of a server.
pub(crate) struct Line {
    shared: Arc<Shared>,
    id: u64,
    arrives: mpsc::Receiver<Inbound>,
    /// The connections the far end made, if it made any.
    connections: Vec<Held>,
    /// How much has arrived over them, and how many of them were made.
    heard: u64,
    /// The connection that is asked first for what arrived, so that
    /// none of them is asked last every time.
    first: usize,
    /// A connection that somebody made and addressed to the call, which
    /// the call has not said is the far end's yet.
    offered: Option<Box<Framed>>,
    /// Where the datagram that arrived last came from, if the call is
    /// not reached from there yet.
    unknown: Option<SocketAddr>,
    /// Whether the pump is gone, and nothing more arrives.
    deaf: bool,
}

impl Line {
    /// The sockets of the call `id`, which is given what arrives for it
    /// through `arrives`.
    fn of(shared: Arc<Shared>, id: u64, arrives: mpsc::Receiver<Inbound>) -> Self {
        Self {
            shared,
            id,
            arrives,
            connections: Vec::new(),
            heard: 0,
            first: 0,
            offered: None,
            unknown: None,
            deaf: false,
        }
    }

    /// The far end's checks are addressed to `name` from here on.
    pub(crate) fn named(&self, name: &str) {
        self.shared.table().name(self.id, name);
    }

    /// Keeps a connection the far end made, beside the ones it made
    /// before.
    fn keep(&mut self, connection: Box<Framed>) {
        self.heard += 1;
        let held = Held {
            connection,
            heard: self.heard,
        };
        let from = held.connection.peer();
        // One from where another came from is that one made again.
        let again = self
            .connections
            .iter()
            .position(|one| one.connection.peer() == from);
        let goes = again.or_else(|| {
            // With as many as a call may have, the one that nothing
            // has arrived on for longest goes.
            (self.connections.len() >= CONNECTIONS)
                .then(|| {
                    let quietest = self.connections.iter().enumerate();
                    quietest.min_by_key(|(_, one)| one.heard).map(|(at, _)| at)
                })
                .flatten()
        });
        if let Some(at) = goes {
            let gone = self.connections.swap_remove(at);
            self.let_go(gone);
        }
        self.connections.push(held);
    }

    /// Lets go of a connection as one that is closed, and not as one
    /// that broke: it is handed to what carries the server's arrivals.
    /// With nothing to hand it to, it is closed where it stands.
    fn let_go(&self, held: Held) {
        let _ = self.shared.leaving.try_send(held.connection);
    }

    /// The next datagram that arrived over a connection of the far
    /// end's, from the first that has one. A connection that has ended
    /// is let go of.
    fn poll_connections(&mut self, cx: &mut Context<'_>) -> Poll<(SocketAddr, Bytes)> {
        let mut at = self.first;
        let mut asked = 0;
        while asked < self.connections.len() {
            at %= self.connections.len();
            let Some(one) = self.connections.get_mut(at) else {
                break;
            };
            match one.connection.poll_next(cx) {
                Poll::Ready(Some(datagram)) => {
                    self.heard += 1;
                    one.heard = self.heard;
                    // The one after it is asked first the next time.
                    self.first = at + 1;
                    return Poll::Ready((one.connection.peer(), datagram));
                }
                // Those after it move up: the next is where it was.
                Poll::Ready(None) => drop(self.connections.remove(at)),
                Poll::Pending => {
                    at += 1;
                    asked += 1;
                }
            }
        }
        Poll::Pending
    }
}

impl Wire for Line {
    fn poll_arrived(&mut self, cx: &mut Context<'_>) -> Poll<Arrived> {
        if !self.deaf {
            match self.arrives.poll_recv(cx) {
                Poll::Ready(Some(Inbound::Datagram {
                    from,
                    datagram,
                    reached,
                })) => {
                    self.unknown = (!reached).then_some(from);
                    return Poll::Ready(Arrived {
                        protocol: Protocol::Udp,
                        from,
                        to: self.shared.udp_at,
                        datagram,
                    });
                }
                Poll::Ready(Some(Inbound::Connection(connection, datagram))) => {
                    // It is the far end's when the call has said so
                    // of what was said on it first, and not before.
                    let from = connection.peer();
                    self.offered = Some(connection);
                    return Poll::Ready(Arrived {
                        protocol: Protocol::Tcp,
                        from,
                        to: self.shared.tcp_at.unwrap_or(self.shared.udp_at),
                        datagram,
                    });
                }
                Poll::Ready(None) => self.deaf = true,
                Poll::Pending => {}
            }
        }
        self.poll_connections(cx).map(|(from, datagram)| Arrived {
            protocol: Protocol::Tcp,
            from,
            to: self.shared.tcp_at.unwrap_or(self.shared.udp_at),
            datagram,
        })
    }

    fn judged(&mut self, from: SocketAddr, taken: bool) {
        if let Some(offered) = self.offered.take() {
            // A connection whose first word was the far end's is kept
            // beside the ones the far end made before: it may be using
            // any of them. Any other is let go of.
            if taken && offered.peer() == from {
                self.keep(offered);
            }
            return;
        }
        if self.unknown.take() == Some(from) && taken {
            self.shared.table().reach(self.id, from);
        }
    }

    fn send(&mut self, transmit: &Transmit) {
        match transmit.proto {
            Protocol::Udp => {
                // A socket that takes no more drops it, as a network
                // would.
                let _ = self
                    .shared
                    .udp
                    .try_send_to(&transmit.contents, transmit.destination);
            }
            Protocol::Tcp => {
                // It goes over the connection it is addressed to. What
                // is addressed to one that is gone is dropped, as a
                // network would drop it.
                let to = self
                    .connections
                    .iter_mut()
                    .find(|one| one.connection.peer() == transmit.destination);
                if let Some(one) = to {
                    one.connection.send(&transmit.contents);
                }
            }
            _ => {}
        }
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) {
        for one in &mut self.connections {
            one.connection.poll_flush(cx);
        }
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        self.shared.table().forget(self.id);
        // A connection of a call is not closed under what was said on
        // it last, which is the goodbye: each is handed to what carries
        // the server's arrivals, and let go of there.
        for held in core::mem::take(&mut self.connections) {
            self.let_go(held);
        }
    }
}

/// A connection that has not said which call it is for.
struct Waiting {
    connection: Option<Box<Framed>>,
    by: Instant,
}

/// Carries what arrives at a server's sockets to its calls. It is a
/// future for the server to run: nothing arrives at a call while it
/// does not.
pub struct Pump {
    shared: Arc<Shared>,
    tcp: Option<TcpListener>,
    /// The connections of calls that are over.
    left: mpsc::Receiver<Box<Framed>>,
}

impl fmt::Debug for Pump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pump").finish_non_exhaustive()
    }
}

impl Pump {
    /// Carries datagrams and connections to their calls for as long as
    /// it is run. It ends only when the system fails the socket, and
    /// returns why.
    pub async fn run(self) -> io::Error {
        let Self {
            shared,
            tcp,
            mut left,
        } = self;
        let mut waiting: Vec<Waiting> = Vec::new();
        let mut leaving: Vec<Waiting> = Vec::new();
        let mut datagram = vec![0_u8; LONGEST];
        // What wakes it to let go of connections that say nothing, and
        // of those that do not end.
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        poll_fn(|cx| {
            let mut carried = 0;
            loop {
                if carried >= AT_ONCE {
                    cx.waker().wake_by_ref();
                    break;
                }
                match shared.udp.try_recv_from(&mut datagram) {
                    Ok((length, from)) => {
                        carried += 1;
                        shared.carry(from, datagram.get(..length).unwrap_or(&[]));
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        match shared.udp.poll_recv_ready(cx) {
                            Poll::Ready(Ok(())) => {}
                            Poll::Ready(Err(error)) => return Poll::Ready(error),
                            Poll::Pending => break,
                        }
                    }
                    // What some systems say of a datagram sent before
                    // that nobody took. It is nothing about this one.
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::ConnectionReset | ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Poll::Ready(error),
                }
            }
            if let Some(tcp) = &tcp {
                // One that the system could not give is passed over,
                // and asked for again at the next tick.
                while let Poll::Ready(Ok((stream, peer))) = tcp.poll_accept(cx) {
                    if waiting.len() >= WAITING {
                        waiting.remove(0);
                    }
                    waiting.push(Waiting {
                        connection: Some(Box::new(Framed::new(stream, peer))),
                        by: Instant::now() + FIRST_WORD,
                    });
                }
            }
            let now = Instant::now();
            waiting.retain_mut(|waiting| {
                let Some(connection) = &mut waiting.connection else {
                    return false;
                };
                match connection.poll_next(cx) {
                    Poll::Ready(Some(first)) => {
                        if let Some(connection) = waiting.connection.take() {
                            shared.connect(connection, first);
                        }
                        false
                    }
                    Poll::Ready(None) => false,
                    Poll::Pending => waiting.by > now,
                }
            });
            while let Poll::Ready(Some(connection)) = left.poll_recv(cx) {
                if leaving.len() < LEAVING {
                    leaving.push(Waiting {
                        connection: Some(connection),
                        by: now + LEAVES_WITHIN,
                    });
                }
            }
            leaving.retain_mut(|leaving| {
                let ended = leaving
                    .connection
                    .as_mut()
                    .is_none_or(|connection| connection.poll_let_go(cx).is_ready());
                !ended && leaving.by > now
            });
            while tick.poll_tick(cx).is_ready() {}
            Poll::Pending
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use tokio::time::Instant;

    use core::future::poll_fn;
    use core::task::Poll;

    use bytes::Bytes;
    use str0m::net::{Protocol, Transmit};

    use super::{
        CONNECTIONS, Config, Endpoint, Inbound, Known, LEAVES_WITHIN, Line, NAMES, PEERS, Table,
    };
    use crate::net::Wire as _;
    use crate::rtp::forged_check as check;
    use crate::tcp::Framed;
    use crate::tcp::testing::{connection, has_something_unread, next, says, sends_no_more};

    #[tokio::test]
    async fn the_connection_of_a_call_that_is_over_is_closed_and_not_broken() {
        let local = "127.0.0.1:0".parse().unwrap();
        let config = Config {
            tcp: Some(local),
            ..Config::at(local)
        };
        let (endpoint, pump) = Endpoint::bind(config).await.unwrap();
        tokio::spawn(pump.run());
        // A call with the connection its far end made, which sends all
        // the time: something of it is unread when the call says its
        // last, and is over.
        let (mut near, mut far) = connection().await;
        says(&mut far, b"what the microphone hears").await;
        has_something_unread(&near).await;
        says(&mut near, b"goodbye").await;
        let (_inbox, arrives) = mpsc::channel(1);
        let mut line = Line::of(Arc::clone(&endpoint.shared), 7, arrives);
        line.keep(Box::new(near));
        drop(line);
        // Nothing refuses what the far end sends next, as a connection
        // that was closed with something unread would.
        says(&mut far, b"more of it").await;
        assert!(!sends_no_more(&far));
        // It has the goodbye, and then that nothing follows.
        assert_eq!(next(&mut far).await.unwrap(), b"goodbye"[..]);
        assert_eq!(next(&mut far).await, None);
        says(&mut far, b"and more").await;
        assert!(!sends_no_more(&far));
        // A far end that does not close it too is waited for a moment,
        // and no longer: what it sends is refused then.
        let since = Instant::now();
        while !sends_no_more(&far) {
            assert!(since.elapsed() < Duration::from_secs(10));
            tokio::time::sleep(Duration::from_millis(50)).await;
            says(&mut far, b"still there").await;
        }
        assert!(since.elapsed() > LEAVES_WITHIN / 2, "{:?}", since.elapsed());
    }

    /// A server that takes connections, and its pump, which is not run:
    /// what a call lets go of goes nowhere.
    async fn listening() -> (Endpoint, super::Pump) {
        let local = "127.0.0.1:0".parse().unwrap();
        let config = Config {
            tcp: Some(local),
            ..Config::at(local)
        };
        Endpoint::bind(config).await.unwrap()
    }

    /// A connection somebody made and said `first` on, as the pump
    /// gives it to its call. Returns the other end of it, and where the
    /// call sees it come from.
    async fn knocks(inbox: &mpsc::Sender<Inbound>, first: &'static [u8]) -> (Framed, SocketAddr) {
        let (near, far) = connection().await;
        let from = near.peer();
        let first = Bytes::from_static(first);
        let given = Inbound::Connection(Box::new(near), first);
        assert!(inbox.send(given).await.is_ok());
        (far, from)
    }

    /// The next thing that arrives for a call: where from, and what.
    async fn arrives(line: &mut Line) -> (SocketAddr, Bytes) {
        let wait = Duration::from_secs(10);
        let arrived = tokio::time::timeout(wait, poll_fn(|cx| line.poll_arrived(cx)));
        let arrived = arrived.await.unwrap();
        assert_eq!(arrived.protocol, Protocol::Tcp);
        (arrived.from, arrived.datagram)
    }

    /// The call says `said` to `to`, and what it says is on its way.
    async fn tells(line: &mut Line, to: SocketAddr, said: &[u8]) {
        line.send(&Transmit {
            proto: Protocol::Tcp,
            source: line.shared.tcp_at.unwrap(),
            destination: to,
            contents: said.to_vec().into(),
        });
        poll_fn(|cx| {
            line.poll_flush(cx);
            Poll::Ready(())
        })
        .await;
    }

    /// A call that has the `connections` its far end made, each with
    /// the other end of it and where the call sees it come from.
    async fn called(
        endpoint: &Endpoint,
        connections: usize,
    ) -> (Line, mpsc::Sender<Inbound>, Vec<(Framed, SocketAddr)>) {
        let (inbox, arrives_at) = mpsc::channel(4);
        let mut line = Line::of(Arc::clone(&endpoint.shared), 7, arrives_at);
        let mut made = Vec::new();
        for _ in 0..connections {
            let (far, from) = knocks(&inbox, b"a check").await;
            assert_eq!(
                arrives(&mut line).await,
                (from, Bytes::from_static(b"a check"))
            );
            line.judged(from, true);
            made.push((far, from));
        }
        (line, inbox, made)
    }

    #[tokio::test]
    async fn a_call_keeps_every_connection_its_far_end_made() {
        // A browser makes a connection from every address it has, says
        // its checks on each, and then sends on one of them: which, it
        // does not say.
        let (endpoint, _pump) = listening().await;
        let (mut line, _inbox, mut made) = called(&endpoint, 3).await;
        assert_eq!(line.connections.len(), 3);
        // What the call says goes over the connection it is addressed
        // to, the first as well as the last.
        for (n, (far, from)) in made.iter_mut().enumerate() {
            let said = format!("an answer to {n}");
            tells(&mut line, *from, said.as_bytes()).await;
            assert_eq!(next(far).await.unwrap(), said.as_bytes());
        }
        // And what the far end says on any of them arrives, from where
        // it was said.
        for (n, (far, from)) in made.iter_mut().enumerate().rev() {
            let said = format!("audio over {n}");
            says(far, said.as_bytes()).await;
            assert_eq!(arrives(&mut line).await, (*from, Bytes::from(said)));
        }
        // None of them was closed for another.
        for (far, _) in &mut made {
            says(far, b"more").await;
            assert!(!sends_no_more(far));
        }
    }

    #[tokio::test]
    async fn a_connection_that_is_not_the_far_ends_is_not_kept() {
        let (endpoint, _pump) = listening().await;
        let (mut line, inbox, mut made) = called(&endpoint, 1).await;
        // Somebody who knows the name of the call and not its password.
        let (mut other, from) = knocks(&inbox, b"no check of this call").await;
        assert_eq!(arrives(&mut line).await.0, from);
        line.judged(from, false);
        assert_eq!(line.connections.len(), 1);
        assert_eq!(next(&mut other).await, None);
        // The far end's own goes on.
        let (far, from) = &mut made[0];
        tells(&mut line, *from, b"still here").await;
        assert_eq!(next(far).await.unwrap(), b"still here"[..]);
        // What is addressed to where no connection comes from is
        // dropped.
        tells(&mut line, peer(9), b"to nobody").await;
        tells(&mut line, *from, b"and here").await;
        assert_eq!(next(far).await.unwrap(), b"and here"[..]);
    }

    #[tokio::test]
    async fn a_connection_that_ended_is_let_go_of_and_the_others_go_on() {
        let (endpoint, _pump) = listening().await;
        let (mut line, _inbox, mut made) = called(&endpoint, 3).await;
        // The far end closes the one in the middle, as a browser does
        // with one it has no more use for.
        let (gone, _) = made.remove(1);
        drop(gone);
        for (n, (far, from)) in made.iter_mut().enumerate() {
            let said = format!("audio over {n}");
            says(far, said.as_bytes()).await;
            assert_eq!(arrives(&mut line).await, (*from, Bytes::from(said)));
        }
        // Asked again, the call has found the one that ended.
        let nothing = poll_fn(|cx| Poll::Ready(line.poll_arrived(cx).is_pending())).await;
        assert!(nothing);
        let left: Vec<SocketAddr> = line
            .connections
            .iter()
            .map(|one| one.connection.peer())
            .collect();
        assert_eq!(left, [made[0].1, made[1].1]);
        for (far, from) in &mut made {
            tells(&mut line, *from, b"an answer").await;
            assert_eq!(next(far).await.unwrap(), b"an answer"[..]);
        }
    }

    #[tokio::test]
    async fn a_call_has_no_more_connections_than_it_may() {
        let (endpoint, pump) = listening().await;
        tokio::spawn(pump.run());
        let (mut line, inbox, mut made) = called(&endpoint, CONNECTIONS).await;
        // The far end sends on all but the third: that is the one
        // nothing has arrived on for longest.
        for (n, (far, from)) in made.iter_mut().enumerate() {
            if n != 2 {
                says(far, b"audio").await;
                assert_eq!(arrives(&mut line).await.0, *from);
            }
        }
        let (mut newest, from) = knocks(&inbox, b"a check").await;
        assert_eq!(arrives(&mut line).await.0, from);
        // The third says something as it is given up, which the call
        // does not come to read.
        says(&mut made[2].0, b"what the microphone hears").await;
        has_something_unread(&line.connections[2].connection).await;
        line.judged(from, true);
        assert_eq!(line.connections.len(), CONNECTIONS);
        // The newest is the call's, and the third is not any more.
        tells(&mut line, from, b"an answer").await;
        assert_eq!(next(&mut newest).await.unwrap(), b"an answer"[..]);
        let quiet = made[2].1;
        assert!(
            line.connections
                .iter()
                .all(|one| one.connection.peer() != quiet)
        );
        // It was closed, and not broken: nothing refuses what its far
        // end sends next, as a connection that was closed with
        // something unread would, and nothing follows on it.
        says(&mut made[2].0, b"more of it").await;
        assert!(!sends_no_more(&made[2].0));
        assert_eq!(next(&mut made[2].0).await, None);
        for (n, (far, from)) in made.iter_mut().enumerate() {
            if n != 2 {
                tells(&mut line, *from, b"an answer").await;
                assert_eq!(next(far).await.unwrap(), b"an answer"[..]);
            }
        }
    }

    #[tokio::test]
    async fn every_connection_of_a_call_that_is_over_is_let_go_of_as_one_that_was_closed() {
        let local = "127.0.0.1:0".parse().unwrap();
        let config = Config {
            tcp: Some(local),
            ..Config::at(local)
        };
        let (endpoint, pump) = Endpoint::bind(config).await.unwrap();
        tokio::spawn(pump.run());
        let (mut line, _inbox, mut made) = called(&endpoint, 2).await;
        // Each far end sends all the time: something of it is unread
        // when the call says its last, and is over.
        for (n, (far, from)) in made.iter_mut().enumerate() {
            says(far, b"what the microphone hears").await;
            has_something_unread(&line.connections[n].connection).await;
            tells(&mut line, *from, b"goodbye").await;
        }
        drop(line);
        // Nothing refuses what a far end sends next, on the second as
        // little as on the first. Each has the goodbye, and then that
        // nothing follows.
        for (far, _) in &mut made {
            says(far, b"more of it").await;
            assert!(!sends_no_more(far));
            assert_eq!(next(far).await.unwrap(), b"goodbye"[..]);
            assert_eq!(next(far).await, None);
        }
    }

    #[tokio::test]
    async fn a_connection_that_always_has_something_keeps_no_other_waiting() {
        let (endpoint, _pump) = listening().await;
        let (mut line, _inbox, mut made) = called(&endpoint, 2).await;
        // The far end sends a lot on one, and a check on the other.
        for said in [&b"audio 1"[..], b"audio 2", b"audio 3"] {
            says(&mut made[0].0, said).await;
        }
        says(&mut made[1].0, b"a check").await;
        for held in &line.connections {
            has_something_unread(&held.connection).await;
        }
        // The check does not wait for all of the audio: the connection
        // after the one that was heard is asked first.
        let busy = made[0].1;
        let other = made[1].1;
        assert_eq!(
            arrives(&mut line).await,
            (busy, Bytes::from_static(b"audio 1"))
        );
        assert_eq!(
            arrives(&mut line).await,
            (other, Bytes::from_static(b"a check"))
        );
        assert_eq!(
            arrives(&mut line).await,
            (busy, Bytes::from_static(b"audio 2"))
        );
        assert_eq!(
            arrives(&mut line).await,
            (busy, Bytes::from_static(b"audio 3"))
        );
    }

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::from(([198, 51, 100, 7], port))
    }

    fn known(table: &mut Table, id: u64, name: &str) -> mpsc::Receiver<super::Inbound> {
        let (inbox, arrives) = mpsc::channel(1);
        let known = Known {
            inbox,
            names: Vec::new(),
            peers: Vec::new(),
        };
        table.calls.insert(id, known);
        table.name(id, name);
        arrives
    }

    #[test]
    fn a_call_is_known_by_its_names_and_by_where_it_is_reached_from() {
        let mut table = Table::default();
        let _one = known(&mut table, 1, "one");
        let _two = known(&mut table, 2, "two");
        assert_eq!(table.named(&check("one")), Some(1));
        assert_eq!(table.named(&check("two")), Some(2));
        assert_eq!(table.named(&check("three")), None);
        // What is no check names no call, whatever it holds.
        let mut audio = check("one");
        audio[0] = 0x80;
        assert_eq!(table.named(&audio), None);

        table.reach(1, peer(1));
        table.reach(1, peer(1));
        table.reach(2, peer(2));
        assert_eq!(table.by_peer.get(&peer(1)), Some(&1));
        assert_eq!(table.calls[&1].peers, [peer(1)]);
        // An address is one call's: the one that was reached from it
        // last.
        table.reach(2, peer(1));
        assert_eq!(table.by_peer.get(&peer(1)), Some(&2));
        assert_eq!(table.calls[&1].peers, []);
        assert_eq!(table.calls[&2].peers, [peer(2), peer(1)]);
        // A call that there is none of is reached from nowhere.
        table.reach(9, peer(9));
        assert_eq!(table.by_peer.get(&peer(9)), None);

        // No more addresses and names are kept for a call than a far
        // end has use for: the oldest go.
        for port in 10..30 {
            table.reach(1, peer(port));
        }
        assert_eq!(table.calls[&1].peers.len(), PEERS);
        assert_eq!(table.by_peer.get(&peer(10)), None);
        assert_eq!(table.by_peer.get(&peer(29)), Some(&1));
        for again in 0..10 {
            table.name(1, &format!("again{again}"));
        }
        assert_eq!(table.calls[&1].names.len(), NAMES);
        assert_eq!(table.named(&check("one")), None);
        assert_eq!(table.named(&check("again9")), Some(1));

        // A call that is let go of leaves nothing behind.
        table.forget(1);
        table.forget(1);
        assert_eq!(table.named(&check("again9")), None);
        assert_eq!(table.by_peer.get(&peer(29)), None);
        assert_eq!(table.by_peer.len(), 2);
        assert_eq!(table.by_name.len(), 1);
        table.forget(2);
        assert!(table.by_peer.is_empty() && table.by_name.is_empty());
    }
}
