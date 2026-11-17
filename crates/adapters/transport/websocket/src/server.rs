//! The server's side: a listener, the clients that connect to it, and
//! the transport each of them becomes.

use core::fmt;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;

use bytes::{Bytes, BytesMut};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use waveport_adapter_kit::stream::{self, Commands, Either, Events, Receiver, Sender};
use waveport_adapter_kit::ws::{WsConn, WsMessage};
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::{Full, PortError};
use waveport_ports::transport::{
    AudioCaps, FrameOut, RemoteBuffer, Transport, TransportCaps, TransportControl, TransportEvent,
};

use crate::wire::{self, FromClient, FromServer, PATH, PROTOCOL};

/// How long a client may take to ask for a session and say hello, unless
/// told otherwise.
pub const GREETING_TIMEOUT: Duration = Duration::from_secs(5);

/// How many frames and requests may wait for the connection.
const ROOM: usize = 64;

/// How many lines about the call may wait for the connection. One that
/// finds no room is not sent.
const LINES: usize = 64;

/// How many responses are remembered, to know which one a client says it
/// has played.
const MOST_RUNS: usize = 64;

/// The close code for a client that did not keep to the protocol.
const NOT_ALLOWED: u16 = 1008;

/// How long a client that has been told that its connection is over is
/// still read from, so that it gets to read what it was told. A client
/// sends audio all the time, and a connection that is let go of with
/// some of that unread is reset, which can cost the client the goodbye.
const LAST_WORDS: Duration = Duration::from_millis(300);

/// Why a listener could not be set up.
#[derive(Debug)]
pub enum ListenError {
    /// The address is not one of this machine alone, and nobody said
    /// that the listener may be reached from elsewhere.
    Exposed(SocketAddr),
    /// The system refused.
    System(io::Error),
}

impl fmt::Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exposed(address) => write!(
                f,
                "{address} can be reached from other machines, and whoever connects is given a \
                 session: nothing asks who they are"
            ),
            Self::System(error) => write!(f, "the address cannot be listened on: {error}"),
        }
    }
}

impl core::error::Error for ListenError {}

/// Takes the connections of clients.
#[derive(Debug)]
pub struct Listener {
    tcp: TcpListener,
}

impl Listener {
    /// Listens on `address`. Port 0 is any port that is free.
    ///
    /// # Errors
    ///
    /// The address can be reached from other machines and `exposed` is
    /// not set, or the system refused it.
    pub async fn bind(address: SocketAddr, exposed: bool) -> Result<Self, ListenError> {
        if !exposed && !address.ip().is_loopback() {
            return Err(ListenError::Exposed(address));
        }
        let tcp = TcpListener::bind(address)
            .await
            .map_err(ListenError::System)?;
        Ok(Self { tcp })
    }

    /// The address that is listened on.
    ///
    /// # Errors
    ///
    /// The system does not say.
    pub fn address(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    /// The next connection. Whoever made it has said nothing yet.
    ///
    /// # Errors
    ///
    /// The system could not take a connection.
    pub async fn accept(&self) -> io::Result<Knock> {
        let (tcp, from) = self.tcp.accept().await?;
        Ok(Knock { tcp, from })
    }
}

/// A connection whose maker has said nothing yet.
#[derive(Debug)]
pub struct Knock {
    tcp: TcpStream,
    from: SocketAddr,
}

impl Knock {
    /// Where the connection comes from.
    #[must_use]
    pub const fn from(&self) -> SocketAddr {
        self.from
    }

    /// Hears the client out: its request for a session and its hello. A
    /// listener that does this for one client at a time lets one slow
    /// client keep the others waiting.
    ///
    /// # Errors
    ///
    /// The client asked for something else, did not keep to the
    /// protocol, or took longer than `timeout`. It has been told, and the
    /// connection is closed.
    pub async fn greet(self, timeout: Duration) -> Result<Caller, PortError> {
        let by = Instant::now() + timeout;
        let (mut conn, _) = WsConn::accept(self.tcp, PATH, PROTOCOL, timeout).await?;
        let hello = match tokio::time::timeout_at(by, conn.next()).await {
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None) | Err(_)) => {
                return Err(PortError::transient("the client left before it said hello"));
            }
            Err(_) => {
                let why = "no hello came in time";
                turn_away(conn, why).await;
                return Err(PortError::timeout(why));
            }
        };
        let rate = match hello {
            WsMessage::Text(text) => match wire::read(text.as_str()) {
                Ok(FromClient::Hello { rate }) => Rate::from_hz(rate).ok_or_else(|| {
                    format!("audio at {rate} Hz is not taken: 8000, 16000, 24000 or 48000")
                }),
                Ok(_) | Err(_) => Err("the first message has to be a hello".to_owned()),
            },
            WsMessage::Binary(_) => Err("the first message has to be a hello".to_owned()),
        };
        match rate {
            Ok(rate) => Ok(Caller {
                conn,
                rate,
                from: self.from,
            }),
            Err(why) => {
                turn_away(conn, &why).await;
                Err(PortError::permanent(why))
            }
        }
    }
}

/// Tells a client why it is not served, and closes its connection.
async fn turn_away(mut conn: WsConn, why: &str) {
    let bye = FromServer::Bye {
        reason: why.to_owned(),
    };
    let _ = conn.send_text(wire::write(&bye)).await;
    conn.close_and_wait(Some((NOT_ALLOWED, why)), LAST_WORDS)
        .await;
}

/// A client that has said hello and has not been answered yet.
#[derive(Debug)]
pub struct Caller {
    conn: WsConn,
    rate: Rate,
    from: SocketAddr,
}

impl Caller {
    /// The rate of the client's audio, both ways.
    #[must_use]
    pub const fn rate(&self) -> Rate {
        self.rate
    }

    /// Where the client's connection comes from.
    #[must_use]
    pub const fn from(&self) -> SocketAddr {
        self.from
    }

    /// The transport of a session for this client. The client is told
    /// that the session is there, and what it is called, when the session
    /// first looks at its transport. `who` is what the session knows the
    /// client as.
    ///
    /// [`Lines`] tells the client about the call in words.
    #[must_use]
    pub fn into_transport(self, session: &str, who: ParticipantId) -> (WsTransport, Lines) {
        let Self { conn, rate, .. } = self;
        let (lines, told) = mpsc::channel(LINES);
        let session = session.to_owned();
        let (commands, events) = stream::pair(ROOM, move |commands, events| {
            let far = Far {
                conn,
                rate,
                who,
                runs: Runs::default(),
                inbound: Inbound::new(rate),
            };
            far.serve(session, commands, told, events)
        });
        let transport = WsTransport {
            rate,
            commands,
            events,
            bidden: false,
            over: false,
        };
        (transport, Lines { lines })
    }

    /// Tells the client that there is no session for it, and why, and
    /// closes the connection.
    pub async fn turn_away(self, why: &str) {
        turn_away(self.conn, why).await;
    }
}

/// Tells the far end of a transport about the call, in lines for a
/// person to read.
#[derive(Clone, Debug)]
pub struct Lines {
    lines: mpsc::Sender<String>,
}

impl Lines {
    /// Has `line` sent to the client. A line that finds the connection
    /// behind, or gone, is not sent: nothing waits for it.
    pub fn say(&self, line: impl Into<String>) {
        let _ = self.lines.try_send(line.into());
    }
}

/// What a session asks of its far end.
#[derive(Debug)]
enum Command {
    Frame {
        tag: OpTag,
        samples: Bytes,
        last: bool,
    },
    Clear {
        tag: OpTag,
    },
    Close,
}

/// The transport of one session: one client's connection.
#[derive(Debug)]
pub struct WsTransport {
    rate: Rate,
    commands: Sender<Command>,
    events: Receiver<TransportEvent>,
    /// Whether the far end has been asked to be told that the session is
    /// over.
    bidden: bool,
    /// Whether the connection is done with.
    over: bool,
}

impl Transport for WsTransport {
    fn caps(&self) -> TransportCaps {
        TransportCaps {
            audio: Some(AudioCaps {
                inbound: Codec::Pcm(self.rate),
                outbound: Codec::Pcm(self.rate),
                remote_buffer: RemoteBuffer {
                    clearable: true,
                    reports_played: true,
                },
            }),
            text: true,
            // This protocol has no message for a key.
            keys: None,
        }
    }

    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>> {
        if self.over {
            return Poll::Ready(None);
        }
        match self.events.poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(event)),
            Poll::Ready(Some(Err(_)) | None) => {
                self.over = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn try_send(&mut self, frame: FrameOut<'_>) -> Result<(), Full> {
        if self.over {
            return Ok(());
        }
        self.commands.try_send(Command::Frame {
            tag: frame.tag,
            samples: Bytes::copy_from_slice(frame.payload),
            last: frame.last,
        })
    }

    fn try_control(&mut self, control: TransportControl) -> Result<(), Full> {
        if self.over {
            return Ok(());
        }
        // It says that it presses no keys, and is not asked to.
        let TransportControl::Clear { tag } = control else {
            return Ok(());
        };
        self.commands.try_send(Command::Clear { tag })
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if self.over {
                return Poll::Ready(());
            }
            if !self.bidden {
                self.bidden = self.commands.try_send(Command::Close).is_ok();
            }
            // Running the connection is what says goodbye. Whatever the
            // far end says in the meantime is of no interest any more.
            match self.events.poll_next(cx) {
                Poll::Ready(Some(Ok(_))) => {}
                Poll::Ready(Some(Err(_)) | None) => self.over = true,
                Poll::Pending if self.bidden => return Poll::Pending,
                Poll::Pending => {
                    // The connection has taken what waited for it, so
                    // there is room for the request now.
                    self.bidden = self.commands.try_send(Command::Close).is_ok();
                    if !self.bidden {
                        return Poll::Pending;
                    }
                }
            }
        }
    }
}

/// A response on its way to the client.
#[derive(Clone, Copy, Debug)]
struct Run {
    tag: OpTag,
    /// The number the client knows it by.
    number: u32,
    /// How many of its samples were sent.
    sent: u32,
}

/// The responses the client was sent, the newest last.
#[derive(Debug, Default)]
struct Runs {
    known: VecDeque<Run>,
    /// The number of the next response.
    next: u32,
    /// The number of the response whose audio the client was last told
    /// follows.
    open: Option<u32>,
}

impl Runs {
    /// The response `tag`, which is a new one if it is not known.
    fn of(&mut self, tag: OpTag) -> &mut Run {
        let found = self.known.iter().position(|run| run.tag == tag);
        let at = found.unwrap_or_else(|| {
            if self.known.len() >= MOST_RUNS {
                self.known.pop_front();
            }
            self.known.push_back(Run {
                tag,
                number: self.next,
                sent: 0,
            });
            self.next = self.next.wrapping_add(1);
            self.known.len() - 1
        });
        // The place is one that was just found or made.
        #[expect(clippy::indexing_slicing, reason = "the index is of this queue")]
        &mut self.known[at]
    }

    fn numbered(&self, number: u32) -> Option<Run> {
        self.known.iter().copied().find(|run| run.number == number)
    }

    fn tagged(&self, tag: OpTag) -> Option<Run> {
        self.known.iter().copied().find(|run| run.tag == tag)
    }
}

/// The client's audio, cut into the packets a session takes: 20 ms each,
/// numbered, and each with the place of its first sample in the stream.
#[derive(Debug)]
struct Inbound {
    rate: Rate,
    /// What has arrived and does not fill a packet yet.
    held: BytesMut,
    sequence: u16,
    /// How many samples came before the next packet.
    position: u64,
    /// When the first audio arrived.
    began: Option<Instant>,
}

impl Inbound {
    fn new(rate: Rate) -> Self {
        Self {
            rate,
            held: BytesMut::new(),
            sequence: 0,
            position: 0,
            began: None,
        }
    }

    /// How many bytes a packet holds.
    const fn packet(&self) -> usize {
        self.rate.frame() * 2
    }

    /// Takes what a message holds, which arrived at `now`.
    ///
    /// # Errors
    ///
    /// The message is not whole samples, or holds more than a second.
    fn take(&mut self, samples: &[u8], now: Instant) -> Result<(), &'static str> {
        let a_second = u64::from(self.rate.hz());
        if !samples.len().is_multiple_of(2) {
            return Err("a message of audio has to hold whole samples");
        }
        let arriving = (samples.len() / 2) as u64;
        if arriving > a_second {
            return Err("a message of audio may hold a second at most");
        }
        let began = *self.began.get_or_insert(now);
        let since = now.saturating_duration_since(began);
        let due =
            u64::try_from(since.as_micros() * u128::from(a_second) / 1_000_000).unwrap_or(u64::MAX);
        let have = self.position + (self.held.len() / 2) as u64 + arriving;
        if due > have + a_second {
            // The client stopped sending for a while. What it sends now
            // was heard now, and not when the count of samples says.
            self.held.clear();
            self.position = due.saturating_sub(arriving);
        }
        self.held.extend_from_slice(samples);
        Ok(())
    }

    /// The next packet that is full: its number, the place of its first
    /// sample, and its samples.
    fn next(&mut self) -> Option<(u16, u32, Bytes)> {
        let packet = self.packet();
        if self.held.len() < packet {
            return None;
        }
        let samples = self.held.split_to(packet).freeze();
        let sequence = self.sequence;
        // Both wrap round, as the port has them.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the place in a stream wraps round"
        )]
        let timestamp = self.position as u32;
        self.sequence = self.sequence.wrapping_add(1);
        self.position += (packet / 2) as u64;
        Some((sequence, timestamp, samples))
    }
}

/// How a connection came to its end.
enum End {
    /// The session said goodbye.
    Closed,
    /// The client left, or its connection dropped.
    Gone,
    /// The client did not keep to the protocol.
    Broke(&'static str),
}

/// The far end of a transport, and what is known of it.
struct Far {
    conn: WsConn,
    rate: Rate,
    who: ParticipantId,
    runs: Runs,
    inbound: Inbound,
}

impl Far {
    /// Runs the connection for as long as there is a session and a
    /// client.
    async fn serve(
        mut self,
        session: String,
        mut commands: Commands<Command>,
        mut lines: mpsc::Receiver<String>,
        mut events: Events<TransportEvent>,
    ) -> Result<(), PortError> {
        let who = self.who;
        let ready = FromServer::Ready {
            session,
            rate: self.rate.hz(),
        };
        if self.say(&ready).await.is_err() {
            // The client left between its hello and the answer: nobody
            // ever joined.
            return Ok(());
        }
        events.send(TransportEvent::Joined { who }).await;
        let mut lines_open = true;
        let end = loop {
            let asked = async {
                if lines_open {
                    stream::first(commands.next(), lines.recv()).await
                } else {
                    Either::Left(commands.next().await)
                }
            };
            let done = match stream::first(asked, self.conn.next()).await {
                Either::Left(Either::Left(Some(command))) => self.carry_out(command).await,
                // The transport is gone, and with it the session.
                Either::Left(Either::Left(None)) => Some(End::Closed),
                Either::Left(Either::Right(Some(text))) => self
                    .say(&FromServer::Event { text })
                    .await
                    .err()
                    .map(|_| End::Gone),
                Either::Left(Either::Right(None)) => {
                    lines_open = false;
                    None
                }
                Either::Right(Ok(Some(message))) => self.hear(message, &mut events).await,
                Either::Right(Ok(None) | Err(_)) => Some(End::Gone),
            };
            if let Some(end) = done {
                break end;
            }
        };
        match end {
            End::Closed => {
                // What was told of the call before it ended is said
                // before the goodbye, and the last of it is how it ended.
                while lines_open && let Ok(text) = lines.try_recv() {
                    if self.say(&FromServer::Event { text }).await.is_err() {
                        break;
                    }
                }
                let bye = FromServer::Bye {
                    reason: "the call is over".to_owned(),
                };
                let _ = self.say(&bye).await;
                self.conn.close_and_wait(None, LAST_WORDS).await;
            }
            End::Gone => events.send(TransportEvent::Left { who }).await,
            End::Broke(why) => {
                turn_away(self.conn, why).await;
                events.send(TransportEvent::Left { who }).await;
            }
        }
        Ok(())
    }

    async fn say(&mut self, message: &FromServer) -> Result<(), PortError> {
        self.conn.send_text(wire::write(message)).await
    }

    /// Does what the session asked. Returns how the connection ended, if
    /// it did.
    async fn carry_out(&mut self, command: Command) -> Option<End> {
        let sent = match command {
            Command::Frame { tag, samples, last } => self.send_frame(tag, samples, last).await,
            Command::Clear { tag } => self.clear(tag).await,
            Command::Close => return Some(End::Closed),
        };
        sent.err().map(|_| End::Gone)
    }

    async fn send_frame(
        &mut self,
        tag: OpTag,
        samples: Bytes,
        last: bool,
    ) -> Result<(), PortError> {
        let length = u32::try_from(samples.len() / 2).unwrap_or(u32::MAX);
        let run = self.runs.of(tag);
        run.sent = run.sent.saturating_add(length);
        let run = run.number;
        if self.runs.open != Some(run) {
            self.runs.open = Some(run);
            self.say(&FromServer::Mark { run, end: false }).await?;
        }
        if !samples.is_empty() {
            self.conn.send_binary(samples).await?;
        }
        if last {
            self.runs.open = None;
            self.say(&FromServer::Mark { run, end: true }).await?;
        }
        Ok(())
    }

    async fn clear(&mut self, tag: OpTag) -> Result<(), PortError> {
        // A response of which nothing was sent has nothing to discard.
        let Some(run) = self.runs.tagged(tag) else {
            return Ok(());
        };
        if self.runs.open == Some(run.number) {
            self.runs.open = None;
        }
        self.say(&FromServer::Clear { run: run.number }).await
    }

    /// Takes what the client said. Returns how the connection ended, if
    /// it did.
    async fn hear(
        &mut self,
        message: WsMessage,
        events: &mut Events<TransportEvent>,
    ) -> Option<End> {
        let who = self.who;
        let text = match message {
            WsMessage::Binary(samples) => {
                if let Err(why) = self.inbound.take(&samples, Instant::now()) {
                    return Some(End::Broke(why));
                }
                while let Some((sequence, timestamp, payload)) = self.inbound.next() {
                    let audio = TransportEvent::Audio {
                        who,
                        sequence,
                        timestamp,
                        payload,
                    };
                    events.send(audio).await;
                }
                return None;
            }
            WsMessage::Text(text) => text,
        };
        match wire::read(text.as_str()) {
            Ok(FromClient::Played { run, samples }) => {
                // A response the client was never sent, or one of long
                // ago, is nothing a session can be told about.
                if let Some(run) = self.runs.numbered(run) {
                    // No more can have been played than was sent.
                    let samples = samples.min(run.sent) as usize;
                    let played = TransportEvent::Played {
                        tag: run.tag,
                        played: self.rate.length_of(samples),
                    };
                    events.send(played).await;
                }
                None
            }
            Ok(FromClient::Text { text }) => {
                events.send(TransportEvent::Text { who, text }).await;
                None
            }
            Ok(FromClient::Bye) => Some(End::Gone),
            Ok(FromClient::Hello { .. }) => Some(End::Broke("hello is said once")),
            Err(_) => Some(End::Broke("that is not a message of the protocol")),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use tokio::time::Instant;
    use waveport_domain::ids::{Epoch, GenId, OpTag};
    use waveport_ports::audio::Rate;

    use super::{Inbound, MOST_RUNS, Runs};

    fn tag(generation: u32) -> OpTag {
        OpTag {
            generation: GenId(generation),
            epoch: Epoch(1),
        }
    }

    #[test]
    fn audio_is_cut_into_packets_whatever_the_pieces_it_arrives_in() {
        let mut inbound = Inbound::new(Rate::Hz16000);
        let now = Instant::now();
        // 20 ms at 16 kHz is 320 samples, 640 bytes.
        let piece: Vec<u8> = (0..500_u16).flat_map(u16::to_le_bytes).collect();
        inbound.take(&piece, now).unwrap();
        let (sequence, timestamp, first) = inbound.next().unwrap();
        assert_eq!((sequence, timestamp, first.len()), (0, 0, 640));
        assert_eq!(first[..4], [0, 0, 1, 0]);
        assert!(inbound.next().is_none());
        inbound.take(&piece, now).unwrap();
        let (sequence, timestamp, second) = inbound.next().unwrap();
        assert_eq!((sequence, timestamp, second.len()), (1, 320, 640));
        // It goes on where the first packet ended: sample 320 of the
        // first piece.
        assert_eq!(second[..2], 320_u16.to_le_bytes());
        let (sequence, timestamp, _) = inbound.next().unwrap();
        assert_eq!((sequence, timestamp), (2, 640));
        assert!(inbound.next().is_none());
    }

    #[test]
    fn audio_that_is_not_whole_samples_or_is_too_long_is_refused() {
        let mut inbound = Inbound::new(Rate::Hz8000);
        let now = Instant::now();
        assert!(inbound.take(&[0; 3], now).is_err());
        assert!(inbound.take(&[0; 16_002], now).is_err());
        // A second is taken.
        inbound.take(&[0; 16_000], now).unwrap();
        // Nothing is nothing.
        inbound.take(&[], now).unwrap();
    }

    #[test]
    fn a_client_that_paused_is_heard_when_it_speaks_again() {
        let mut inbound = Inbound::new(Rate::Hz8000);
        let start = Instant::now();
        // 20 ms at 8 kHz is 160 samples, 320 bytes. Half a second
        // arrives, and a part of a packet with it.
        inbound.take(&[0; 8_100], start).unwrap();
        let mut last = None;
        while let Some(packet) = inbound.next() {
            last = Some(packet);
        }
        let (sequence, timestamp, _) = last.unwrap();
        assert_eq!((sequence, timestamp), (24, 24 * 160));

        // Nothing for five seconds, and then a packet: it is placed at
        // now, five seconds on, and the part that was held is let go.
        let later = start + Duration::from_secs(5);
        inbound.take(&[0; 320], later).unwrap();
        let (sequence, timestamp, _) = inbound.next().unwrap();
        assert_eq!((sequence, timestamp), (25, 5 * 8_000 - 160));

        // A client that is a little late is not taken to have paused.
        let mut inbound = Inbound::new(Rate::Hz8000);
        inbound.take(&[0; 320], start).unwrap();
        inbound
            .take(&[0; 320], start + Duration::from_millis(900))
            .unwrap();
        assert_eq!(inbound.next().unwrap().1, 0);
        assert_eq!(inbound.next().unwrap().1, 160);
    }

    #[test]
    fn responses_are_numbered_as_they_come_and_the_oldest_are_forgotten() {
        let mut runs = Runs::default();
        assert_eq!(runs.of(tag(7)).number, 0);
        runs.of(tag(7)).sent = 960;
        assert_eq!(runs.of(tag(9)).number, 1);
        // A response that is known keeps its number, and what was sent.
        assert_eq!(runs.of(tag(7)).number, 0);
        assert_eq!(runs.numbered(0).unwrap().sent, 960);
        assert_eq!(runs.tagged(tag(9)).unwrap().number, 1);
        assert!(runs.numbered(2).is_none());
        assert!(runs.tagged(tag(8)).is_none());

        for (_, generation) in (0..MOST_RUNS).zip(100_u32..) {
            let _ = runs.of(tag(generation));
        }
        assert!(runs.tagged(tag(7)).is_none());
        assert!(runs.numbered(0).is_none());
        assert_eq!(runs.known.len(), MOST_RUNS);
    }
}
