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
//! A call that is over hands its connection to the pump, which lets go
//! of it as one that is closed and not as one that broke. What a call
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
        let line = Line {
            shared: Arc::clone(&self.shared),
            id,
            arrives,
            connection: None,
            offered: None,
            unknown: None,
            deaf: false,
        };
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

/// The sockets of one call of a server.
pub(crate) struct Line {
    shared: Arc<Shared>,
    id: u64,
    arrives: mpsc::Receiver<Inbound>,
    /// The connection the far end made, if it made one.
    connection: Option<Box<Framed>>,
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
    /// The far end's checks are addressed to `name` from here on.
    pub(crate) fn named(&self, name: &str) {
        self.shared.table().name(self.id, name);
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
        if let Some(connection) = &mut self.connection {
            match connection.poll_next(cx) {
                Poll::Ready(Some(datagram)) => {
                    return Poll::Ready(Arrived {
                        protocol: Protocol::Tcp,
                        from: connection.peer(),
                        to: self.shared.tcp_at.unwrap_or(self.shared.udp_at),
                        datagram,
                    });
                }
                Poll::Ready(None) => self.connection = None,
                Poll::Pending => {}
            }
        }
        Poll::Pending
    }

    fn judged(&mut self, from: SocketAddr, taken: bool) {
        if let Some(offered) = self.offered.take() {
            // A connection whose first word was the far end's takes the
            // place of one the far end made before, which it has given
            // up. Any other is let go of.
            if taken && offered.peer() == from {
                self.connection = Some(offered);
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
                if let Some(connection) = &mut self.connection
                    && connection.peer() == transmit.destination
                {
                    connection.send(&transmit.contents);
                }
            }
            _ => {}
        }
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) {
        if let Some(connection) = &mut self.connection {
            connection.poll_flush(cx);
        }
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        self.shared.table().forget(self.id);
        // The connection of a call is not closed under what was said
        // on it last, which is the goodbye: it is handed to what
        // carries the server's arrivals, and let go of there. With
        // nothing to hand it to, it is closed where it stands.
        if let Some(connection) = self.connection.take() {
            let _ = self.shared.leaving.try_send(connection);
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

    use super::{Config, Endpoint, Known, LEAVES_WITHIN, Line, NAMES, PEERS, Table};
    use crate::rtp::forged_check as check;
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
        let line = Line {
            shared: Arc::clone(&endpoint.shared),
            id: 7,
            arrives,
            connection: Some(Box::new(near)),
            offered: None,
            unknown: None,
            deaf: false,
        };
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
