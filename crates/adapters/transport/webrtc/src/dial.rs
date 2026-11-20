//! The other side of a call: a client that calls a server, for a
//! command and for tests.
//!
//! It offers what a browser offers, as far as a server can tell, and
//! speaks client protocol v1 on the data channel. It has no microphone
//! and no loudspeaker: what it says is the packets it is given, and
//! what it hears it hands on, each packet with the response it is of.

use core::fmt;
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;

use bytes::Bytes;
use str0m::net::{Protocol, Transmit};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::Instant;
use waveport_adapter_kit::client::{Hello, Welcome};
use waveport_adapter_kit::line::opus_samples;
use waveport_domain::event::Key;
use waveport_ports::error::PortError;
use waveport_protocol::client::v1::{
    self as client, ByeReason, ErrorCode, FromClient, FromServer, News,
};

use crate::crypto::certificate;
use crate::net::{Arrived, Driven, Wire, now};
use crate::peer::{Like, Offers, Peer, PeerHeard};
use crate::tcp::Framed;

/// How long audio that no response was said to begin with waits to be
/// told whose it is. What says so travels beside the audio, and may
/// come a moment after it.
const HOLD: Duration = Duration::from_millis(500);

/// How often keys that are being pressed are looked at.
const PRESSING: Duration = Duration::from_millis(20);

/// How many responses are remembered, to say what was played of them.
const RUNS: usize = 64;

/// How many datagrams may wait for a connection that is being made.
const WAITING: usize = 16;

/// How long a server that has been told goodbye is given to hear it
/// before the call is ended: until it says that it has, or hangs up
/// itself. The end of a call can overtake the goodbye on its way.
const LAST_WORDS: Duration = Duration::from_millis(300);

/// The longest datagram that is read whole.
const LONGEST: usize = 2048;

/// The port a far end that makes its connections itself says it is at
/// (RFC 6544).
const DISCARD: u16 = 9;

/// Something a client was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// The audio that follows is of this run.
    Begins {
        /// The run.
        run: u32,
        /// The response it is the audio of.
        generation: u32,
    },
    /// One packet of audio.
    Audio(Bytes),
    /// This run is complete.
    Ends(u32),
    /// What is held of this run and has not been played is to be
    /// discarded.
    Clear(u32),
    /// The server pressed a key.
    Key(Key),
    /// Something that happened in the session, and is kept.
    Fact {
        /// Its place among the facts.
        seq: u64,
        /// What happened.
        fact: client::Fact,
    },
    /// Something that is told as it happens, and is not kept.
    Told(News),
    /// Facts were missed: what follows says where the session is.
    Gap {
        /// The first fact that follows.
        resumes: u64,
    },
    /// Where the session is.
    State(Box<client::State>),
    /// Something the client said was not taken.
    Error {
        /// Why, for a program.
        code: ErrorCode,
        /// Why, for a person.
        message: String,
    },
    /// The server says goodbye.
    Bye(ByeReason),
    /// The server cannot be reached. It may be again.
    Down,
    /// The server is reached again.
    Up,
}

/// Why a client is not on a call.
#[derive(Debug)]
pub enum Unconnected {
    /// The server turned it away.
    Refused {
        /// Why, for a program.
        code: ErrorCode,
        /// Why, for a person.
        message: String,
    },
    /// The call could not be made.
    Failed(PortError),
}

impl fmt::Display for Unconnected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { message, .. } => {
                write!(f, "the server turned the client away: {message}")
            }
            Self::Failed(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for Unconnected {}

impl From<PortError> for Unconnected {
    fn from(error: PortError) -> Self {
        Self::Failed(error)
    }
}

type Connecting = Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send>>;

/// The connection of a client that reaches its server over TCP.
enum Connection {
    /// It has none, and makes one when there is something to send on
    /// it.
    None,
    Making {
        to: SocketAddr,
        connecting: Connecting,
        /// What is to be sent once it is made.
        waiting: Vec<Bytes>,
    },
    Made(Box<Framed>),
}

/// The sockets of a client.
pub(crate) struct Own {
    udp: UdpSocket,
    /// Where it says it is.
    at: SocketAddr,
    /// Where it says its connections come from.
    connects_from: SocketAddr,
    connection: Connection,
    datagram: Vec<u8>,
}

impl Wire for Own {
    fn poll_arrived(&mut self, cx: &mut Context<'_>) -> Poll<Arrived> {
        if let Connection::Making {
            to,
            connecting,
            waiting,
        } = &mut self.connection
            && let Poll::Ready(made) = connecting.as_mut().poll(cx)
        {
            self.connection = match made {
                Ok(stream) => {
                    let mut connection = Box::new(Framed::new(stream, *to));
                    for datagram in waiting.drain(..) {
                        connection.send(&datagram);
                    }
                    Connection::Made(connection)
                }
                // The call makes another when it sends again.
                Err(_) => Connection::None,
            };
        }
        loop {
            match self.udp.try_recv_from(&mut self.datagram) {
                Ok((length, from)) => {
                    let datagram = self.datagram.get(..length).unwrap_or(&[]);
                    return Poll::Ready(Arrived {
                        protocol: Protocol::Udp,
                        from,
                        to: self.at,
                        datagram: Bytes::copy_from_slice(datagram),
                    });
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    match self.udp.poll_recv_ready(cx) {
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(_)) | Poll::Pending => break,
                    }
                }
                // What some systems say of a datagram sent before that
                // nobody took.
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionReset | ErrorKind::Interrupted
                    ) => {}
                Err(_) => break,
            }
        }
        if let Connection::Made(connection) = &mut self.connection {
            match connection.poll_next(cx) {
                Poll::Ready(Some(datagram)) => {
                    return Poll::Ready(Arrived {
                        protocol: Protocol::Tcp,
                        from: connection.peer(),
                        to: self.connects_from,
                        datagram,
                    });
                }
                Poll::Ready(None) => self.connection = Connection::None,
                Poll::Pending => {}
            }
        }
        Poll::Pending
    }

    fn send(&mut self, transmit: &Transmit) {
        let to = transmit.destination;
        match transmit.proto {
            Protocol::Udp => {
                let _ = self.udp.try_send_to(&transmit.contents, to);
            }
            Protocol::Tcp => match &mut self.connection {
                Connection::Made(connection) if connection.peer() == to => {
                    connection.send(&transmit.contents);
                }
                Connection::Making {
                    to: making,
                    waiting,
                    ..
                } if *making == to => {
                    if waiting.len() < WAITING {
                        waiting.push(Bytes::copy_from_slice(&transmit.contents));
                    }
                }
                // To somewhere else than before, or for the first time.
                _ => {
                    self.connection = Connection::Making {
                        to,
                        connecting: Box::pin(TcpStream::connect(to)),
                        waiting: vec![Bytes::copy_from_slice(&transmit.contents)],
                    };
                }
            },
            _ => {}
        }
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) {
        if let Connection::Made(connection) = &mut self.connection {
            connection.poll_flush(cx);
        }
    }
}

/// Whether `time` is before `limit` in a stream whose clock wraps
/// round.
const fn before(time: u32, limit: u32) -> bool {
    time.wrapping_sub(limit) >= 1 << 31
}

/// How many stretches of one run are remembered: a run is one stretch
/// unless its audio came with a break in it.
const STRETCHES: usize = 16;

/// A run a client was told of: where it is in the stream, and what was
/// given of it.
///
/// The audio of a run is not always one piece of the stream. A server
/// that was late with a frame goes on later in the stream by as much,
/// and a packet that did not arrive leaves a hole. So how much of a run
/// was played says where in the stream the client is only together
/// with which audio it was that the client was given.
#[derive(Debug)]
struct Run {
    number: u32,
    /// Where it begins.
    begins: u32,
    /// The audio that was given of it, as stretches with no break in
    /// them: where each begins, and how many samples it holds.
    given: VecDeque<(u32, u32)>,
    /// How many samples were given in stretches that are no longer
    /// remembered.
    forgotten: u32,
    /// Where it ends, once the server said that it was sent whole.
    ends: Option<u32>,
}

impl Run {
    /// Audio of the run was given to the client: `samples` from `time`
    /// on.
    fn took(&mut self, time: u32, samples: u32) {
        match self.given.back_mut() {
            Some((began, length)) if began.wrapping_add(*length) == time => {
                *length = length.saturating_add(samples);
            }
            _ => {
                if self.given.len() >= STRETCHES
                    && let Some((_, length)) = self.given.pop_front()
                {
                    self.forgotten = self.forgotten.saturating_add(length);
                }
                self.given.push_back((time, samples));
            }
        }
    }

    /// Where in the stream the client is when it has played `samples`
    /// of what it was given of the run.
    fn place(&self, samples: u32) -> u32 {
        let mut left = samples.saturating_sub(self.forgotten);
        let mut at = self.begins;
        let mut stretches = self.given.iter().peekable();
        while let Some((time, length)) = stretches.next() {
            // The end of a stretch that another follows is where it
            // ends, and not where the next begins: nothing of the next
            // was played.
            if left < *length || (left == *length && stretches.peek().is_some()) {
                return time.wrapping_add(left);
            }
            left -= *length;
            at = time.wrapping_add(*length);
        }
        // Everything it was given was played. Of a run that was sent
        // whole no more is coming: what did not arrive of it will not
        // be played, and the run has been played to its end.
        self.ends.unwrap_or(at)
    }
}

/// What a client was sent, put in the order it means: the audio of a
/// response after what says that it begins, and before what says that
/// it ends or is discarded. The audio and what is said of it travel
/// beside each other, and arrive in either order.
#[derive(Default)]
struct Order {
    /// What the server said, the oldest first, and when it came.
    said: VecDeque<(FromServer, Instant)>,
    /// The audio that arrived: where it is in the stream, and when it
    /// came.
    audio: VecDeque<(u32, Bytes, Instant)>,
    /// Whether a run is being heard.
    hearing: bool,
    /// Where in the stream the run that is being heard has been heard
    /// to.
    heard_to: Option<u32>,
    /// The run that is being heard, if it was said where it begins.
    open: Option<u32>,
    /// The runs that were heard of, the oldest first.
    begun: VecDeque<Run>,
    /// What needs no ordering.
    aside: VecDeque<Heard>,
    /// Until when something that is held back waits for what is to
    /// come before it.
    held_until: Option<Instant>,
}

impl Order {
    /// The next packet of audio, if it is before `limit` in the stream.
    fn audio_before(&mut self, limit: Option<u32>) -> Option<Heard> {
        let (time, ..) = self.audio.front()?;
        if limit.is_some_and(|limit| !before(*time, limit)) {
            return None;
        }
        let (time, audio, _) = self.audio.pop_front()?;
        if self.hearing {
            let samples = opus_samples(&audio).unwrap_or(0);
            self.heard_to = Some(time.wrapping_add(samples));
            if let Some(open) = self.open
                && let Some(run) = self.run(open)
            {
                run.took(time, samples);
            }
        }
        Some(Heard::Audio(audio))
    }

    /// The run `run`, if the client was told where it begins.
    fn run(&mut self, run: u32) -> Option<&mut Run> {
        self.begun
            .iter_mut()
            .rev()
            .find(|known| known.number == run)
    }

    /// Holds something back until `until`.
    const fn hold(&mut self, until: Instant) -> Option<Heard> {
        self.held_until = Some(until);
        None
    }

    /// The next thing to give, at `now`. `Err` for a server that does
    /// what none does.
    fn next(&mut self, now: Instant) -> Result<Option<Heard>, PortError> {
        self.held_until = None;
        if let Some(heard) = self.aside.pop_front() {
            return Ok(Some(heard));
        }
        loop {
            let Some((said, came)) = self.said.front() else {
                // The audio of a run that is being heard is given as it
                // comes. Audio of no run waits to be told whose it is,
                // for a while.
                let Some((.., came)) = self.audio.front() else {
                    return Ok(None);
                };
                let until = *came + HOLD;
                return Ok(if self.hearing || until <= now {
                    self.audio_before(None)
                } else {
                    self.hold(until)
                });
            };
            let until = *came + HOLD;
            // What is before it in the stream comes before it.
            let limit = match *said {
                FromServer::Mark { ts, .. } => ts,
                _ => None,
            };
            let ordered = matches!(said, FromServer::Mark { .. } | FromServer::Clear { .. });
            let ends = matches!(said, FromServer::Mark { end: true, .. });
            if (ordered || self.hearing)
                && let Some(audio) = self.audio_before(limit)
            {
                return Ok(Some(audio));
            }
            // The end of a run comes after its audio: it waits for what
            // has not come of that, for a while.
            if let (true, Some(limit), Some(heard_to)) = (ends, limit, self.heard_to)
                && before(heard_to, limit)
                && now < until
            {
                return Ok(self.hold(until));
            }
            let Some((said, _)) = self.said.pop_front() else {
                return Ok(None);
            };
            let heard = match said {
                FromServer::Mark {
                    run,
                    generation,
                    end: false,
                    ts,
                } => {
                    self.hearing = true;
                    self.heard_to = ts;
                    self.open = ts.map(|_| run);
                    if let Some(begins) = ts {
                        if self.begun.len() >= RUNS {
                            self.begun.pop_front();
                        }
                        self.begun.push_back(Run {
                            number: run,
                            begins,
                            given: VecDeque::new(),
                            forgotten: 0,
                            ends: None,
                        });
                    }
                    Heard::Begins { run, generation }
                }
                FromServer::Mark {
                    run, end: true, ts, ..
                } => {
                    self.hearing = false;
                    self.open = None;
                    if let Some(known) = self.run(run) {
                        known.ends = ts;
                    }
                    Heard::Ends(run)
                }
                FromServer::Clear { run } => {
                    self.hearing = false;
                    self.open = None;
                    Heard::Clear(run)
                }
                FromServer::Fact { seq, fact } => Heard::Fact { seq, fact },
                FromServer::Told(news) => Heard::Told(news),
                FromServer::Gap { resumes } => Heard::Gap { resumes },
                FromServer::State(state) => Heard::State(Box::new(state)),
                FromServer::Error { code, message } => Heard::Error { code, message },
                FromServer::Bye { reason } => Heard::Bye(reason),
                FromServer::Welcome { .. } => {
                    return Err(PortError::permanent(
                        "the server welcomed the client a second time",
                    ));
                }
                // A message of a later version: it is passed over.
                FromServer::Unknown => continue,
            };
            return Ok(Some(heard));
        }
    }

    /// Where in the stream a client is that has played `samples` of
    /// what it was given of the run `run`. `None` for a run it was
    /// never told of.
    fn place(&self, run: u32, samples: u32) -> Option<u32> {
        let known = self.begun.iter().rev().find(|known| known.number == run);
        known.map(|known| known.place(samples))
    }
}

/// A client that has made an offer and has no answer yet.
pub struct Dialing {
    pub(crate) driven: Box<Driven<Peer, Own>>,
}

impl fmt::Debug for Dialing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dialing").finish_non_exhaustive()
    }
}

/// A client on a call.
pub struct Dialer {
    pub(crate) driven: Box<Driven<Peer, Own>>,
    welcome: Welcome,
    order: Order,
    over: bool,
}

impl fmt::Debug for Dialer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dialer")
            .field("welcome", &self.welcome)
            .field("over", &self.over)
            .finish_non_exhaustive()
    }
}

/// The address a client is at with a socket of its own bound to
/// `local`.
async fn socket(local: SocketAddr) -> Result<(UdpSocket, SocketAddr), PortError> {
    let failed = |error: io::Error| PortError::transient(format!("no socket for a call: {error}"));
    let udp = UdpSocket::bind(local).await.map_err(failed)?;
    let at = udp.local_addr().map_err(failed)?;
    if at.ip().is_unspecified() {
        let why = "a client has to say at which of the machine's addresses it is";
        return Err(PortError::permanent(why));
    }
    Ok((udp, at))
}

impl Dialer {
    /// Makes an offer as `like` does, from the address `local`, which
    /// is one address of this machine; port 0 is any port that is
    /// free. With `tcp` the client sends no datagrams, as on a network
    /// that lets none out, and reaches its server over a connection it
    /// makes.
    ///
    /// The offer is for whoever carries it to the server, whose answer
    /// is given to what this returns.
    ///
    /// # Errors
    ///
    /// The system gave no socket, or `local` is every address of the
    /// machine.
    pub async fn offer(
        like: Like,
        local: SocketAddr,
        tcp: bool,
    ) -> Result<(Dialing, String), PortError> {
        Self::offers(like, local, tcp, None).await
    }

    /// Makes the offer of a client that speaks another protocol than
    /// client protocol v1, on a data channel it opens itself under the
    /// name `opens`: as a browser does that was written for another
    /// server. The answer is given to [`Dialing::opened`].
    ///
    /// # Errors
    ///
    /// As for [`offer`](Self::offer).
    pub async fn offer_as_a_stranger(
        like: Like,
        local: SocketAddr,
        tcp: bool,
        opens: &'static str,
    ) -> Result<(Dialing, String), PortError> {
        Self::offers(like, local, tcp, Some(opens)).await
    }

    async fn offers(
        like: Like,
        local: SocketAddr,
        tcp: bool,
        opens: Option<&'static str>,
    ) -> Result<(Dialing, String), PortError> {
        let (udp, at) = socket(local).await?;
        let connects_from = SocketAddr::new(at.ip(), DISCARD);
        let from = if tcp { connects_from } else { at };
        let offers = Offers {
            like,
            speaks: opens.is_none(),
            opens,
            tcp,
            dtls13: false,
        };
        let made = certificate().and_then(|own| Peer::offer(&own, offers, from, now()));
        let Some((peer, offer)) = made else {
            return Err(PortError::permanent("no offer could be made"));
        };
        let own = Own {
            udp,
            at,
            connects_from,
            connection: Connection::None,
            datagram: vec![0; LONGEST],
        };
        let driven = Box::new(Driven::new(peer, own));
        Ok((Dialing { driven }, offer))
    }

    /// What the server answered the hello with.
    #[must_use]
    pub const fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// The session the client is in.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.welcome.session
    }

    /// Files what happened on the call next.
    async fn hear(&mut self) {
        match self.driven.told().await {
            PeerHeard::Audio { time, payload } => {
                self.order.audio.push_back((time, payload, Instant::now()));
            }
            PeerHeard::Said(said) => self.order.said.push_back((said, Instant::now())),
            PeerHeard::Key(key) => self.order.aside.push_back(Heard::Key(key)),
            PeerHeard::Down => self.order.aside.push_back(Heard::Down),
            PeerHeard::Up => self.order.aside.push_back(Heard::Up),
            // A client of the protocol opens no channel of its own.
            PeerHeard::Open | PeerHeard::Text(_) => {}
            PeerHeard::Over => self.over = true,
        }
    }

    /// The next thing the server sent. `None` when the call is over.
    ///
    /// The call goes on only while this is waited for, or something is
    /// said: a client that does neither for some seconds is taken by
    /// its server to have dropped.
    ///
    /// # Errors
    ///
    /// The server did what no server does.
    pub async fn next(&mut self) -> Result<Option<Heard>, PortError> {
        loop {
            if let Some(heard) = self.order.next(Instant::now())? {
                return Ok(Some(heard));
            }
            if self.over {
                return Ok(None);
            }
            match self.order.held_until {
                Some(until) => {
                    let _ = tokio::time::timeout_at(until, self.hear()).await;
                }
                None => self.hear().await,
            }
        }
    }

    /// Says `message` now.
    fn say_now(&mut self, message: &FromClient) -> Result<(), PortError> {
        let said = self.driven.core.say(message);
        self.driven.flush();
        if said {
            Ok(())
        } else {
            Err(PortError::transient("the call is over"))
        }
    }

    /// Sends one Opus packet of what the microphone heard.
    ///
    /// # Errors
    ///
    /// It is no packet of Opus, or the call is over.
    pub fn audio(&mut self, audio: &[u8]) -> Result<(), PortError> {
        let said = self.driven.core.speak(now(), audio);
        self.driven.flush();
        if said {
            Ok(())
        } else {
            Err(PortError::transient(
                "the packet is not one of Opus, or the call is over",
            ))
        }
    }

    /// Says how many samples of what the client was given of the run
    /// `run` have been played so far, on the clock of the call: as a
    /// place in the stream, which is how it is said on a call. A run
    /// the client was never told of is said nothing about.
    ///
    /// The place is that of the audio that was played, wherever in the
    /// stream it was: audio that came after a break, or after a packet
    /// that did not arrive, is further on than what was played before
    /// it adds up to. A run that was sent whole has been played to its
    /// end once everything that arrived of it has been.
    ///
    /// # Errors
    ///
    /// The call is over.
    pub fn played(&mut self, run: u32, samples: u32) -> Result<(), PortError> {
        let Some(place) = self.order.place(run, samples) else {
            return Ok(());
        };
        self.say_now(&FromClient::PlayedRtp {
            ts: Some(place),
            emitted: None,
            concealed: None,
            delay_ms: 0,
        })
    }

    /// Sends text in place of speech.
    ///
    /// # Errors
    ///
    /// The call is over.
    pub fn text(&mut self, text: &str) -> Result<(), PortError> {
        let text = text.to_owned();
        self.say_now(&FromClient::Text { text })
    }

    /// Presses keys of a telephone, one after the other, as a browser
    /// does: as events in the stream of its audio. It returns when the
    /// last of them has been pressed. Returns whether the call has a
    /// way to press any.
    ///
    /// # Errors
    ///
    /// The call is over.
    pub async fn dtmf(&mut self, keys: &str) -> Result<bool, PortError> {
        for key in keys.chars().filter_map(Key::from_char) {
            if !self.driven.core.press(now(), key) {
                return Ok(false);
            }
        }
        // Pressing is the call's own doing, and nothing says when it
        // is done: it is looked at as often as a key is said.
        while !self.driven.core.has_pressed() && !self.over {
            let _ = tokio::time::timeout(PRESSING, self.hear()).await;
        }
        if self.over {
            return Err(PortError::transient("the call is over"));
        }
        Ok(true)
    }

    /// Asks that the agent stop.
    ///
    /// # Errors
    ///
    /// The call is over.
    pub fn interrupt(&mut self) -> Result<(), PortError> {
        self.say_now(&FromClient::Interrupt)
    }

    /// Says `message` as it is.
    ///
    /// # Errors
    ///
    /// The call is over.
    pub fn say(&mut self, message: &FromClient) -> Result<(), PortError> {
        self.say_now(message)
    }

    /// The client is somewhere else than it was: it has another socket
    /// from here on, and offers again from there. The offer is for
    /// whoever carried the first, and the answer for
    /// [`moved`](Self::moved).
    ///
    /// # Errors
    ///
    /// The system gave no socket, or the call is over.
    pub async fn moves(&mut self) -> Result<String, PortError> {
        let local = SocketAddr::new(self.driven.wire.at.ip(), 0);
        let (udp, at) = socket(local).await?;
        let Some(offer) = self.driven.core.offer_again(at) else {
            return Err(PortError::transient("the call is over"));
        };
        self.driven.wire.udp = udp;
        self.driven.wire.at = at;
        Ok(offer)
    }

    /// Takes the answer to an offer that was made again. Returns
    /// whether it is one.
    pub fn moved(&mut self, answer: &str) -> bool {
        let taken = self.driven.core.answered(answer);
        self.driven.flush();
        taken
    }

    /// Says goodbye, which ends the session, and ends the call.
    pub async fn bye(mut self) {
        let _ = self.say_now(&FromClient::Bye);
        self.driven.flush();
        let heard = poll_fn(|cx| {
            // What the server says meanwhile is of no interest any
            // more.
            while self.driven.poll_told(cx).is_ready() {}
            if self.driven.core.has_said_all() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        let _ = tokio::time::timeout(LAST_WORDS, heard).await;
        self.driven.core.close();
        self.driven.flush();
    }

    /// Lets go of the call without a word, as a client does whose
    /// network is gone.
    pub fn drop_line(self) {
        drop(self.driven);
    }
}

impl Dialing {
    /// Takes the answer and makes the call: the checks, the handshake,
    /// the hello, and the server's welcome.
    ///
    /// # Errors
    ///
    /// The answer is none to the offer, the server was not reached or
    /// did not welcome the client within `timeout`, or it turned the
    /// client away.
    pub async fn answered(
        self,
        answer: &str,
        hello: &Hello,
        timeout: Duration,
    ) -> Result<Dialer, Unconnected> {
        let mut driven = self.driven;
        if !driven.core.answered(answer) {
            return Err(PortError::permanent("the answer is not one to the offer").into());
        }
        let by = Instant::now() + timeout;
        let mut order = Order::default();
        let welcome = loop {
            let Ok(told) = tokio::time::timeout_at(by, driven.told()).await else {
                let error = PortError::timeout(format!(
                    "the server did not welcome the client within {} ms",
                    timeout.as_millis()
                ));
                return Err(error.into());
            };
            let failed = |what: String| Err(Unconnected::Failed(PortError::permanent(what)));
            match told {
                // The channel is there to say hello on.
                PeerHeard::Open => {
                    driven.core.say(&hello.said());
                }
                PeerHeard::Said(FromServer::Welcome {
                    v,
                    session,
                    life,
                    resumed,
                    resume_token,
                    features,
                    next_seq,
                    grace_ms,
                    ..
                }) => {
                    if v != client::VERSION {
                        return failed(format!(
                            "the server answered in version {v} of the protocol"
                        ));
                    }
                    break Welcome {
                        session,
                        life,
                        resumed,
                        resume_token,
                        features,
                        next_seq,
                        grace_ms,
                    };
                }
                PeerHeard::Said(FromServer::Error { code, message }) => {
                    return Err(Unconnected::Refused { code, message });
                }
                PeerHeard::Said(FromServer::Bye { reason }) => {
                    return failed(format!(
                        "the server answered a hello with goodbye: {reason:?}"
                    ));
                }
                PeerHeard::Said(_) => {
                    return failed("the server answered a hello with something else".to_owned());
                }
                PeerHeard::Over => {
                    let error = PortError::transient("the call ended before it began");
                    return Err(error.into());
                }
                // Audio that outran the welcome is kept.
                PeerHeard::Audio { time, payload } => {
                    order.audio.push_back((time, payload, Instant::now()));
                }
                PeerHeard::Key(_) | PeerHeard::Up | PeerHeard::Down | PeerHeard::Text(_) => {}
            }
        };
        Ok(Dialer {
            driven,
            welcome,
            order,
            over: false,
        })
    }

    /// Takes the answer to the offer of a client that speaks another
    /// protocol, and makes the call: the checks, the handshake, and the
    /// data channel the client opens. Nothing is said on it yet.
    ///
    /// # Errors
    ///
    /// The answer is none to the offer, or the call and its channel
    /// were not there within `timeout`.
    pub async fn opened(self, answer: &str, timeout: Duration) -> Result<Stranger, Unconnected> {
        let mut driven = self.driven;
        if !driven.core.answered(answer) {
            return Err(PortError::permanent("the answer is not one to the offer").into());
        }
        let by = Instant::now() + timeout;
        let mut early = VecDeque::new();
        loop {
            let Ok(told) = tokio::time::timeout_at(by, driven.told()).await else {
                let error = PortError::timeout(format!(
                    "the call and its channel were not there within {} ms",
                    timeout.as_millis()
                ));
                return Err(error.into());
            };
            match told {
                PeerHeard::Open => break,
                PeerHeard::Over => {
                    let error = PortError::transient("the call ended before it began");
                    return Err(error.into());
                }
                // What outran the channel is kept.
                PeerHeard::Audio { payload, .. } => early.push_back(Strange::Audio(payload)),
                PeerHeard::Key(key) => early.push_back(Strange::Key(key)),
                PeerHeard::Text(text) => early.push_back(Strange::Text(text)),
                PeerHeard::Up | PeerHeard::Down | PeerHeard::Said(_) => {}
            }
        }
        Ok(Stranger {
            driven,
            early,
            over: false,
        })
    }
}

/// Something a [`Stranger`] was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Strange {
    /// What the server said on the data channel, as the text it sent.
    Text(String),
    /// One packet of audio.
    Audio(Bytes),
    /// The server pressed a key.
    Key(Key),
    /// The server cannot be reached. It may be again.
    Down,
    /// The server is reached again.
    Up,
}

/// A client on a call that speaks another protocol than client
/// protocol v1, on a data channel it opened itself: what a server that
/// takes such clients is tried with. It knows nothing of the protocol
/// it speaks: what is said is text, both ways.
pub struct Stranger {
    driven: Box<Driven<Peer, Own>>,
    early: VecDeque<Strange>,
    over: bool,
}

impl fmt::Debug for Stranger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stranger")
            .field("over", &self.over)
            .finish_non_exhaustive()
    }
}

impl Stranger {
    /// The next thing the server sent. `None` when the call is over.
    ///
    /// The call goes on only while this is waited for, or something is
    /// said: a client that does neither for some seconds is taken by
    /// its server to have dropped.
    pub async fn next(&mut self) -> Option<Strange> {
        loop {
            if let Some(early) = self.early.pop_front() {
                return Some(early);
            }
            if self.over {
                return None;
            }
            match self.driven.told().await {
                PeerHeard::Audio { payload, .. } => return Some(Strange::Audio(payload)),
                PeerHeard::Text(text) => return Some(Strange::Text(text)),
                PeerHeard::Key(key) => return Some(Strange::Key(key)),
                PeerHeard::Down => return Some(Strange::Down),
                PeerHeard::Up => return Some(Strange::Up),
                PeerHeard::Open | PeerHeard::Said(_) => {}
                PeerHeard::Over => self.over = true,
            }
        }
    }

    /// Says `text` on the data channel.
    ///
    /// # Errors
    ///
    /// The call is over, or its channel takes no more.
    pub fn say(&mut self, text: &str) -> Result<(), PortError> {
        let said = self.driven.core.say_raw(false, text.as_bytes());
        self.driven.flush();
        if said {
            Ok(())
        } else {
            Err(PortError::transient("the call is over"))
        }
    }

    /// Sends one Opus packet of what the microphone heard.
    ///
    /// # Errors
    ///
    /// It is no packet of Opus, or the call is over.
    pub fn audio(&mut self, audio: &[u8]) -> Result<(), PortError> {
        let said = self.driven.core.speak(now(), audio);
        self.driven.flush();
        if said {
            Ok(())
        } else {
            Err(PortError::transient(
                "the packet is not one of Opus, or the call is over",
            ))
        }
    }

    /// Ends the call, as a page does that closes it: the server is
    /// told so by the call itself, and nothing is said in whatever is
    /// spoken on it.
    pub fn hang_up(mut self) {
        self.driven.core.close();
        self.driven.flush();
    }

    /// Lets go of the call without a word, as a client does whose
    /// network is gone.
    pub fn drop_line(self) {
        drop(self.driven);
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use bytes::Bytes;
    use tokio::time::Instant;
    use waveport_adapter_kit::client::Welcome;
    use waveport_protocol::client::v1::FromServer;

    use super::{HOLD, Heard, Order, STRETCHES, before};

    /// A packet of 20 ms that is told from others by `n`.
    fn packet(n: u8) -> Bytes {
        Bytes::from(vec![0xF8, n])
    }

    fn audio(n: u8) -> Heard {
        Heard::Audio(packet(n))
    }

    fn mark(run: u32, end: bool, ts: u32) -> FromServer {
        FromServer::Mark {
            run,
            generation: run + 10,
            end,
            ts: Some(ts),
        }
    }

    /// Everything an order gives at `now`.
    fn given(order: &mut Order, now: Instant) -> Vec<Heard> {
        let mut given = Vec::new();
        while let Some(heard) = order.next(now).unwrap() {
            given.push(heard);
        }
        given
    }

    #[test]
    fn what_is_told_of_a_session_keeps_its_place_before_audio_nothing_was_said_of() {
        let now = Instant::now();
        let mut order = Order::default();
        // Audio came first, then something that says nothing of audio,
        // then whose the audio is: the audio waits for that.
        order.audio.push_back((1_000, packet(1), now));
        order.said.push_back((FromServer::Gap { resumes: 2 }, now));
        order.said.push_back((mark(4, false, 1_000), now));
        let begins = Heard::Begins {
            run: 4,
            generation: 14,
        };
        assert_eq!(
            given(&mut order, now),
            [Heard::Gap { resumes: 2 }, begins, audio(1)]
        );
        // A client is welcomed once, before any of this. A server that
        // says it again is not one to go on with.
        let welcome = Welcome::default().said(None);
        order.said.push_back((welcome, now));
        assert!(order.next(now).is_err());
    }

    #[test]
    fn audio_is_given_between_what_says_whose_it_is() {
        let now = Instant::now();
        let just_before = now + HOLD - Duration::from_millis(1);
        let mut order = Order::default();
        let hears = |order: &mut Order, time: u32, n: u8| {
            order.audio.push_back((time, packet(n), now));
        };
        let says = |order: &mut Order, said: FromServer| order.said.push_back((said, now));
        // Audio comes before what says that its run begins, and waits.
        hears(&mut order, 1_000, 1);
        assert_eq!(given(&mut order, now), []);
        assert_eq!(order.held_until, Some(now + HOLD));
        says(&mut order, mark(4, false, 1_000));
        let begins = Heard::Begins {
            run: 4,
            generation: 14,
        };
        assert_eq!(given(&mut order, now), [begins, audio(1)]);
        // What follows of the run is given as it comes.
        hears(&mut order, 1_960, 2);
        assert_eq!(given(&mut order, now), [audio(2)]);
        assert_eq!(order.held_until, None);
        // The end of a run comes after its audio, and before the audio
        // of the next, whichever arrived first.
        hears(&mut order, 2_920, 3);
        hears(&mut order, 9_000, 4);
        says(&mut order, mark(4, true, 3_880));
        says(&mut order, mark(5, false, 9_000));
        says(&mut order, FromServer::Clear { run: 5 });
        let begins = Heard::Begins {
            run: 5,
            generation: 15,
        };
        assert_eq!(
            given(&mut order, now),
            [audio(3), Heard::Ends(4), begins, audio(4), Heard::Clear(5)]
        );
        assert_eq!(
            (order.place(4, 0), order.place(5, 0)),
            (Some(1_000), Some(9_000))
        );
        assert_eq!(order.place(6, 0), None);

        // Audio that nothing is said about is given after a while, as
        // it is.
        hears(&mut order, 20_000, 5);
        assert_eq!(given(&mut order, just_before), []);
        assert_eq!(given(&mut order, now + HOLD), [audio(5)]);

        // A run is said to have ended before the last of its audio
        // came: its end waits for that, and not for ever.
        let begins = |run| Heard::Begins {
            run,
            generation: run + 10,
        };
        says(&mut order, mark(6, false, 30_000));
        hears(&mut order, 30_000, 6);
        says(&mut order, mark(6, true, 31_920));
        assert_eq!(given(&mut order, now), [begins(6), audio(6)]);
        assert_eq!(order.held_until, Some(now + HOLD));
        hears(&mut order, 30_960, 7);
        assert_eq!(given(&mut order, now), [audio(7), Heard::Ends(6)]);
        says(&mut order, mark(7, false, 40_000));
        says(&mut order, mark(7, true, 40_960));
        assert_eq!(given(&mut order, just_before), [begins(7)]);
        assert_eq!(given(&mut order, now + HOLD), [Heard::Ends(7)]);
        // What needs no place among the audio is given as it comes.
        says(&mut order, FromServer::Gap { resumes: 3 });
        order.aside.push_back(Heard::Down);
        assert_eq!(
            given(&mut order, now),
            [Heard::Down, Heard::Gap { resumes: 3 }]
        );
    }

    #[test]
    fn how_much_of_a_run_was_played_is_a_place_among_the_audio_that_was_given() {
        let now = Instant::now();
        let mut order = Order::default();
        let hears = |order: &mut Order, time: u32, n: u8| {
            order.audio.push_back((time, packet(n), now));
        };
        // A run of four packets. The server was late with the third,
        // by 480 ms: it is that much further on in the stream.
        order.said.push_back((mark(4, false, 1_000), now));
        hears(&mut order, 1_000, 1);
        hears(&mut order, 1_960, 2);
        hears(&mut order, 26_000, 3);
        hears(&mut order, 26_960, 4);
        assert_eq!(given(&mut order, now).len(), 5);
        let place = |order: &Order, samples| order.place(4, samples).unwrap();
        assert_eq!(place(&order, 0), 1_000);
        assert_eq!(place(&order, 1), 1_001);
        // Two packets were played: the client is where they end, and
        // not where the audio goes on, of which it has played nothing.
        assert_eq!(place(&order, 1_920), 2_920);
        assert_eq!(place(&order, 1_921), 26_001);
        assert_eq!(place(&order, 3_839), 27_919);
        assert_eq!(place(&order, 3_840), 27_920);
        // No more was played than was given.
        assert_eq!(place(&order, 9_999), 27_920);

        // The run was sent whole, and ends a packet later than the
        // client has heard it to: one did not arrive. Its end waits for
        // that packet, and then is given without it.
        order.said.push_back((mark(4, true, 28_880), now));
        assert_eq!(given(&mut order, now), []);
        assert_eq!(given(&mut order, now + HOLD), [Heard::Ends(4)]);
        // What was played of it is where it was. Once all that arrived
        // has been played, the run has been played to its end: nothing
        // more of it is coming.
        assert_eq!(place(&order, 3_839), 27_919);
        assert_eq!(place(&order, 3_840), 28_880);
        assert_eq!(place(&order, 9_999), 28_880);

        // Audio that comes after the end of its run is of no run, and
        // changes nothing of where the run was played to.
        hears(&mut order, 27_920, 5);
        assert_eq!(given(&mut order, now + HOLD), [audio(5)]);
        assert_eq!(place(&order, 3_840), 28_880);
        // A run of which nothing arrived, and one that was discarded.
        order.said.push_back((mark(5, false, 40_000), now));
        order.said.push_back((mark(5, true, 40_960), now));
        order.said.push_back((mark(6, false, 50_000), now));
        hears(&mut order, 50_000, 6);
        order.said.push_back((FromServer::Clear { run: 6 }, now));
        assert_eq!(given(&mut order, now + HOLD).len(), 5);
        // What comes of a run after it was discarded is not its audio.
        hears(&mut order, 50_960, 7);
        assert_eq!(given(&mut order, now + HOLD), [audio(7)]);
        assert_eq!(order.place(5, 0), Some(40_960));
        assert_eq!(order.place(6, 0), Some(50_000));
        assert_eq!(order.place(6, 960), Some(50_960));
        assert_eq!(order.place(6, 1_920), Some(50_960));
        assert_eq!(order.place(7, 0), None);
    }

    #[test]
    fn a_run_in_many_pieces_is_still_counted_from_its_beginning() {
        let now = Instant::now();
        let mut order = Order::default();
        order.said.push_back((mark(1, false, 0), now));
        // Twenty packets with a break before each: more pieces than
        // are remembered.
        let pieces = 20_u32;
        for n in 0..pieces {
            let time = n * 10_000;
            order.audio.push_back((time, packet(1), now));
        }
        assert_eq!(given(&mut order, now).len(), 21);
        // The pieces that are remembered are where they are.
        let remembered = u32::try_from(STRETCHES).unwrap();
        for n in (pieces - remembered)..pieces {
            let played = n * 960;
            assert_eq!(order.place(1, played + 1), Some(n * 10_000 + 1), "{n}");
            assert_eq!(order.place(1, played + 960), Some(n * 10_000 + 960), "{n}");
        }
        // What was played of the ones that are not is before the first
        // that is.
        let first = (pieces - remembered) * 10_000;
        assert_eq!(order.place(1, 0), Some(first));
        assert_eq!(order.place(1, 960), Some(first));
        assert_eq!(order.place(1, 20 * 960), Some(19 * 10_000 + 960));
    }

    #[test]
    fn a_place_in_a_stream_is_before_another_across_the_wrap() {
        assert!(before(10, 11) && !before(11, 11) && !before(12, 11));
        assert!(before(u32::MAX - 5, 3) && !before(3, u32::MAX - 5));
    }
}
