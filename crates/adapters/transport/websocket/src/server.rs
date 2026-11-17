//! The server's side: a listener, the clients that connect to it, and
//! the transport of a session, which a client is attached to.

use core::fmt;
use core::task::{Context, Poll};
use core::time::Duration;
use std::io;
use std::net::SocketAddr;

use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use waveport_adapter_kit::line::{Inbound, OpusInbound, Packet, Runs};
use waveport_adapter_kit::stream::{self, Commands, Either, Events, Receiver, Sender};
use waveport_adapter_kit::ws::{WsConn, WsMessage};
use waveport_domain::event::Key;
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::{Full, PortError};
use waveport_ports::transport::{
    AudioCaps, FrameOut, RemoteBuffer, Transport, TransportCaps, TransportControl, TransportEvent,
};
use waveport_protocol::client::v1::{
    self as client, AudioFormat, ByeReason, ErrorCode, FromClient, FromServer,
};

use crate::wire::{self, Hello, PATH, PROTOCOL, Welcome};

/// How long a client may take to ask for a session and say hello, unless
/// told otherwise.
pub const GREETING_TIMEOUT: Duration = Duration::from_secs(5);

/// How many frames and requests may wait for the connection.
const ROOM: usize = 64;

/// How many messages for the client may wait for the connection. Whoever
/// tells more waits for room.
const TOLD: usize = 256;

/// How many things a client asked may wait for whoever does them. One
/// that finds no room is refused, and can be asked again.
const ASKED: usize = 16;

/// The close code for a client that is not served.
const NOT_ALLOWED: u16 = 1008;

/// How long a client that has been told that its connection is over is
/// still read from, so that it gets to read what it was told. A client
/// sends audio all the time, and a connection that is let go of with
/// some of that unread is reset, which can cost the client the goodbye.
const LAST_WORDS: Duration = Duration::from_millis(300);

/// How long a session that is over still takes what its client is to be
/// told, before the goodbye. Whoever tells the client of the session is
/// told of its end on another thread, a moment after the transport is.
/// With [`LAST_WORDS`] it is less than a session waits for its
/// transport's goodbye.
const LAST_TOLD: Duration = Duration::from_millis(150);

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
        let (conn, _) = WsConn::accept(self.tcp, PATH, Some(PROTOCOL), timeout).await?;
        let left = by.saturating_duration_since(Instant::now());
        Caller::of(conn, Some(self.from), left).await
    }
}

/// The audio of a connection, as a session takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sound {
    /// Plain samples at this rate.
    Pcm(Rate),
    /// Opus, played and recorded at this rate.
    Opus(Rate),
}

impl Sound {
    fn of(format: AudioFormat) -> Option<Self> {
        let rate = Rate::from_hz(format.rate)?;
        Some(match format.codec {
            client::Codec::Pcm16 => Self::Pcm(rate),
            client::Codec::Opus => Self::Opus(rate),
        })
    }

    const fn codec(self) -> Codec {
        match self {
            Self::Pcm(rate) => Codec::Pcm(rate),
            Self::Opus(_) => Codec::Opus,
        }
    }

    /// The rate the far end counts what it has played at.
    const fn rate(self) -> Rate {
        match self {
            Self::Pcm(rate) | Self::Opus(rate) => rate,
        }
    }
}

/// Tells a client why it is not served, and closes its connection.
async fn turn_away(mut conn: WsConn, code: ErrorCode, why: &str) {
    let error = FromServer::Error {
        code,
        message: why.to_owned(),
    };
    let bye = FromServer::Bye {
        reason: ByeReason::Refused,
    };
    if conn.send_text(wire::write(&error)).await.is_ok() {
        let _ = conn.send_text(wire::write(&bye)).await;
    }
    conn.close_and_wait(Some((NOT_ALLOWED, why)), LAST_WORDS)
        .await;
}

/// A client that has said hello and has not been answered yet.
#[derive(Debug)]
pub struct Caller {
    conn: WsConn,
    hello: Hello,
    from: Option<SocketAddr>,
}

impl Caller {
    /// Hears the hello of a client on a connection that is a WebSocket
    /// already: one that [`Knock::greet`] took, or one that an HTTP
    /// server had and handed over. `from` is where it comes from, if
    /// that is known.
    ///
    /// # Errors
    ///
    /// The client did not begin with a hello of this version, said
    /// something in it that a hello cannot, or took longer than
    /// `timeout`. It has been told, and the connection is closed.
    pub async fn of(
        mut conn: WsConn,
        from: Option<SocketAddr>,
        timeout: Duration,
    ) -> Result<Self, PortError> {
        let first = match tokio::time::timeout(timeout, conn.next()).await {
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None) | Err(_)) => {
                return Err(PortError::transient("the client left before it said hello"));
            }
            Err(_) => {
                let why = "no hello came in time";
                turn_away(conn, ErrorCode::OutOfPlace, why).await;
                return Err(PortError::timeout(why));
            }
        };
        let hello = match &first {
            WsMessage::Text(text) => wire::read::<FromClient>(text.as_str()).ok(),
            WsMessage::Binary(_) => None,
        };
        let why = "the first message has to be a hello";
        let Some(said @ FromClient::Hello { .. }) = hello else {
            turn_away(conn, ErrorCode::OutOfPlace, why).await;
            return Err(PortError::permanent(why));
        };
        // Its version, its audio, and that its tokens are tokens.
        if let Err(refusal) = said.check() {
            let why = refusal.to_string();
            turn_away(conn, refusal.code(), &why).await;
            return Err(PortError::permanent(why));
        }
        let FromClient::Hello {
            token,
            audio,
            features,
            resume,
            ..
        } = said
        else {
            return Err(PortError::permanent(why));
        };
        let hello = Hello {
            token,
            audio,
            features,
            resume,
        };
        Ok(Self { conn, hello, from })
    }

    /// What the client said as it came.
    #[must_use]
    pub const fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Where the client's connection comes from, if that is known.
    #[must_use]
    pub const fn from(&self) -> Option<SocketAddr> {
        self.from
    }

    /// The transport of a session for a client that speaks as this one
    /// does: with its audio, or with none for a client that writes.
    /// `who` is what the session knows its far end as.
    ///
    /// Nobody is on the transport yet. The session is started with it,
    /// and then this client is attached ([`Handle::attach`]) with what
    /// the session was started as.
    #[must_use]
    pub fn transport(&self, who: ParticipantId) -> (WsTransport, Handle) {
        // The rate of a hello that was taken is one there is.
        let sound = self.hello.audio.and_then(Sound::of);
        let audio = self.hello.audio.filter(|_| sound.is_some());
        let (attach, attached) = mpsc::channel(1);
        let (commands, events) = stream::pair(ROOM, move |commands, events| {
            let far = Far {
                who,
                sound,
                runs: Runs::default(),
                pcm: match sound {
                    Some(Sound::Pcm(rate)) => Some(Inbound::pcm(rate)),
                    Some(Sound::Opus(_)) | None => None,
                },
                opus: OpusInbound::default(),
                joined: false,
            };
            far.serve(commands, attached, events)
        });
        let transport = WsTransport {
            sound,
            commands,
            events,
            bidden: false,
            over: false,
        };
        (transport, Handle { attach, audio })
    }

    /// Tells the client that it is not served, and why, and closes the
    /// connection.
    pub async fn refuse(self, code: ErrorCode, why: &str) {
        turn_away(self.conn, code, why).await;
    }
}

/// The connection of a client is over: it dropped, another took its
/// place, or the session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gone;

impl fmt::Display for Gone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the connection of the client is over")
    }
}

impl core::error::Error for Gone {}

/// The rest of what a connection carries, besides what the session and
/// its far end say to each other through the transport: what the client
/// is told of the session, and what it asks that a transport cannot do.
///
/// It is of one connection. A client that comes back is attached anew,
/// and whoever attaches it is given another.
#[derive(Debug)]
pub struct Link {
    told: mpsc::Sender<FromServer>,
    asked: mpsc::Receiver<FromClient>,
}

impl Link {
    /// Tells the client `message`, after what it was told before and in
    /// order with the audio. Waits while the connection is behind.
    ///
    /// When the session is over, the client is told goodbye once whoever
    /// holds this has let go of it, or after a moment: so what is told
    /// last is told before the goodbye.
    ///
    /// # Errors
    ///
    /// The connection is over, and the message was not sent.
    pub async fn tell(&self, message: FromServer) -> Result<(), Gone> {
        self.told.send(message).await.map_err(|_| Gone)
    }

    /// What the client asked next that is for whoever runs the session
    /// to do, and not for the transport: that the agent stop
    /// ([`FromClient::Interrupt`]). `None` when the connection is over.
    pub async fn asked(&mut self) -> Option<FromClient> {
        self.asked.recv().await
    }
}

/// A client that was not attached to a transport, and why.
#[derive(Debug)]
pub struct Unattached {
    caller: Caller,
    code: ErrorCode,
    why: &'static str,
}

impl Unattached {
    /// Why, for a program.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    /// Why, for a person.
    #[must_use]
    pub const fn why(&self) -> &'static str {
        self.why
    }

    /// Tells the client why, and closes its connection.
    pub async fn refuse(self) {
        self.caller.refuse(self.code, self.why).await;
    }
}

/// A client's connection on its way to a transport.
struct Attachment {
    conn: WsConn,
    welcome: FromServer,
    told: mpsc::Receiver<FromServer>,
    asked: mpsc::Sender<FromClient>,
}

/// Gives a transport its client: the first time, and each time the
/// client comes back.
#[derive(Clone)]
pub struct Handle {
    attach: mpsc::Sender<Box<Attachment>>,
    audio: Option<AudioFormat>,
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle")
            .field("audio", &self.audio)
            .field("over", &self.is_over())
            .finish_non_exhaustive()
    }
}

impl Handle {
    /// The audio of the transport, which is that of the client it was
    /// made for. None for one that carries writing only.
    #[must_use]
    pub const fn audio(&self) -> Option<AudioFormat> {
        self.audio
    }

    /// Whether the transport is gone, and with it its session: nobody
    /// can be attached to it.
    #[must_use]
    pub fn is_over(&self) -> bool {
        self.attach.is_closed()
    }

    /// Attaches `caller` to the transport and has it welcomed with
    /// `welcome`. The session is told that its far end is there: that it
    /// joined, the first time, and that the line is back after that.
    ///
    /// A client that is on the transport already is told that another
    /// took its place, and the session that the line dropped and came
    /// back: what the new one hears begins with it.
    ///
    /// # Errors
    ///
    /// The session is over, or the client's audio is not the
    /// transport's, which a session cannot change. The client has been
    /// told nothing yet.
    pub async fn attach(&self, caller: Caller, welcome: Welcome) -> Result<Link, Box<Unattached>> {
        let unattached = |caller, code, why| Box::new(Unattached { caller, code, why });
        if caller.hello.audio != self.audio {
            let why = "the session is under way with other audio than this client names";
            return Err(unattached(caller, ErrorCode::Unsupported, why));
        }
        let Ok(room) = self.attach.reserve().await else {
            return Err(unattached(
                caller,
                ErrorCode::NoSession,
                "the session is over",
            ));
        };
        let (told, telling) = mpsc::channel(TOLD);
        let (asking, asked) = mpsc::channel(ASKED);
        room.send(Box::new(Attachment {
            conn: caller.conn,
            welcome: welcome.said(self.audio),
            told: telling,
            asked: asking,
        }));
        Ok(Link { told, asked })
    }
}

/// What a session asks of its far end.
#[derive(Debug)]
enum Command {
    Frame {
        tag: OpTag,
        /// How many samples it holds, at the rate the far end plays at.
        samples: u32,
        payload: Bytes,
        last: bool,
    },
    Clear {
        tag: OpTag,
    },
    Close,
}

/// The transport of one session: the connection of one client, and of
/// the same client when it comes back.
#[derive(Debug)]
pub struct WsTransport {
    sound: Option<Sound>,
    commands: Sender<Command>,
    events: Receiver<TransportEvent>,
    /// Whether the far end has been asked to be told that the session is
    /// over.
    bidden: bool,
    /// Whether the transport is done with.
    over: bool,
}

impl Transport for WsTransport {
    fn caps(&self) -> TransportCaps {
        TransportCaps {
            audio: self.sound.map(|sound| AudioCaps {
                inbound: sound.codec(),
                outbound: sound.codec(),
                remote_buffer: RemoteBuffer {
                    clearable: true,
                    reports_played: true,
                },
            }),
            text: true,
            // A client presses keys, and is sent none.
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
        // A connection that carries writing only is sent no audio.
        let Some(sound) = self.sound.filter(|_| !self.over) else {
            return Ok(());
        };
        let samples = sound.rate().samples_in(frame.length);
        self.commands.try_send(Command::Frame {
            tag: frame.tag,
            samples: u32::try_from(samples).unwrap_or(u32::MAX),
            payload: Bytes::copy_from_slice(frame.payload),
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

/// The connection that is attached, and what is told and asked over it.
struct Here {
    conn: WsConn,
    told: mpsc::Receiver<FromServer>,
    /// Whether whoever tells the client still holds its end.
    telling: bool,
    asked: mpsc::Sender<FromClient>,
}

/// What a transport waits for.
enum Next {
    Command(Option<Command>),
    Attach(Option<Box<Attachment>>),
    Told(Option<FromServer>),
    Heard(Result<Option<WsMessage>, PortError>),
}

/// What became of a connection after something was done on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
    /// It goes on.
    Goes,
    /// It dropped, or cannot be gone on with.
    Dropped,
    /// The client said goodbye.
    Left,
}

/// How a transport came to its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The session said goodbye.
    Closed,
    /// The client said goodbye, or is gone and nobody can bring it back.
    Left,
}

/// The far end of a transport, and what is known of it.
struct Far {
    who: ParticipantId,
    sound: Option<Sound>,
    runs: Runs,
    pcm: Option<Inbound>,
    opus: OpusInbound,
    /// Whether a client was ever attached.
    joined: bool,
}

impl Far {
    /// Runs the transport for as long as there is a session, with
    /// whatever client is attached to it.
    async fn serve(
        mut self,
        mut commands: Commands<Command>,
        mut attached: mpsc::Receiver<Box<Attachment>>,
        mut events: Events<TransportEvent>,
    ) -> Result<(), PortError> {
        let who = self.who;
        let mut here: Option<Here> = None;
        // Whether anybody can still attach a client.
        let mut attachable = true;
        let end = loop {
            if here.is_none() && !attachable {
                // Nobody is there, and nobody can bring anyone.
                break End::Left;
            }
            let attaching = attachable.then_some(&mut attached);
            let after = match next(&mut commands, attaching, here.as_mut()).await {
                // The transport is gone, or the session is over.
                Next::Command(None | Some(Command::Close)) => break End::Closed,
                Next::Command(Some(command)) => match &mut here {
                    Some(here) => self.carry_out(here, command).await,
                    // Nobody is there to send it to.
                    None => After::Goes,
                },
                Next::Attach(None) => {
                    attachable = false;
                    After::Goes
                }
                Next::Attach(Some(attachment)) => {
                    if let Some(before) = here.take() {
                        // What the new client hears begins with it: the
                        // session is told that the line dropped.
                        replaced(before).await;
                        events.send(TransportEvent::Detached).await;
                    }
                    // One that left between its hello and the answer
                    // was never there.
                    if let Some(taken) = self.take_in(*attachment).await {
                        here = Some(taken);
                        let back = if self.joined {
                            TransportEvent::Attached
                        } else {
                            TransportEvent::Joined { who }
                        };
                        self.joined = true;
                        events.send(back).await;
                    }
                    After::Goes
                }
                Next::Told(None) => {
                    if let Some(here) = &mut here {
                        here.telling = false;
                    }
                    After::Goes
                }
                Next::Told(Some(message)) => match &mut here {
                    Some(here) => sent(here.conn.send_text(wire::write(&message)).await),
                    None => After::Goes,
                },
                Next::Heard(Ok(Some(message))) => match &mut here {
                    Some(here) => self.hear(here, message, &mut events).await,
                    None => After::Goes,
                },
                Next::Heard(Ok(None) | Err(_)) => After::Dropped,
            };
            match after {
                After::Goes => {}
                After::Dropped => {
                    // Whoever told this client is told that it is gone
                    // by having nobody to tell.
                    here = None;
                    events.send(TransportEvent::Detached).await;
                }
                After::Left => break End::Left,
            }
        };
        match (end, here) {
            (End::Closed, Some(here)) => farewell(here).await,
            // It said goodbye, and is not told anything more.
            (End::Left, Some(here)) => here.conn.close_and_wait(None, LAST_WORDS).await,
            (_, None) => {}
        }
        if end == End::Left && self.joined {
            events.send(TransportEvent::Left { who }).await;
        }
        Ok(())
    }

    /// Welcomes a client that is being attached. Returns its
    /// connection, unless it is gone already.
    async fn take_in(&mut self, attachment: Attachment) -> Option<Here> {
        let Attachment {
            mut conn,
            welcome,
            told,
            asked,
        } = attachment;
        conn.send_text(wire::write(&welcome)).await.ok()?;
        // It holds nothing of what the one before it was sent.
        self.runs.begin_anew();
        Some(Here {
            conn,
            told,
            telling: true,
            asked,
        })
    }

    /// Does what the session asked.
    async fn carry_out(&mut self, here: &mut Here, command: Command) -> After {
        let done = match command {
            Command::Frame {
                tag,
                samples,
                payload,
                last,
            } => self.send_frame(here, tag, samples, payload, last).await,
            Command::Clear { tag } => self.clear(here, tag).await,
            // It ends the transport, and is taken before this.
            Command::Close => Ok(()),
        };
        sent(done)
    }

    async fn send_frame(
        &mut self,
        here: &mut Here,
        tag: OpTag,
        samples: u32,
        payload: Bytes,
        last: bool,
    ) -> Result<(), PortError> {
        let generation = tag.generation.get();
        let run = self.runs.of(tag);
        run.sent = run.sent.saturating_add(samples);
        let run = run.number;
        if self.runs.begins(run) {
            let end = false;
            let mark = FromServer::Mark {
                run,
                generation,
                end,
            };
            here.conn.send_text(wire::write(&mark)).await?;
        }
        if !payload.is_empty() {
            here.conn.send_binary(payload).await?;
        }
        if last {
            self.runs.ends(run);
            let end = true;
            let mark = FromServer::Mark {
                run,
                generation,
                end,
            };
            here.conn.send_text(wire::write(&mark)).await?;
        }
        Ok(())
    }

    async fn clear(&mut self, here: &mut Here, tag: OpTag) -> Result<(), PortError> {
        // A response of which nothing was sent has nothing to discard.
        let Some(run) = self.runs.tagged(tag) else {
            return Ok(());
        };
        self.runs.ends(run.number);
        let clear = FromServer::Clear { run: run.number };
        here.conn.send_text(wire::write(&clear)).await
    }

    /// Takes what the client said.
    async fn hear(
        &mut self,
        here: &mut Here,
        message: WsMessage,
        events: &mut Events<TransportEvent>,
    ) -> After {
        let who = self.who;
        let text = match message {
            WsMessage::Binary(audio) => {
                return match self.heard(audio, events).await {
                    Ok(()) => After::Goes,
                    Err((code, why)) => refused(here, code, why).await,
                };
            }
            WsMessage::Text(text) => text,
        };
        let Ok(said) = wire::read::<FromClient>(text.as_str()) else {
            let why = "that is not a message of the protocol";
            return refused(here, ErrorCode::Malformed, why).await;
        };
        match said {
            // It is said once, as a connection begins.
            FromClient::Hello { .. } => {
                return refused(here, ErrorCode::OutOfPlace, "hello is said once").await;
            }
            _ => {
                if let Err(refusal) = said.check() {
                    return refused(here, refusal.code(), &refusal.to_string()).await;
                }
            }
        }
        match said {
            FromClient::Played { run, samples } => {
                // A run the client was never sent, or one of long ago,
                // is nothing a session can be told about.
                if let (Some(run), Some(sound)) = (self.runs.numbered(run), self.sound) {
                    // No more can have been played than was sent.
                    let samples = samples.min(run.sent) as usize;
                    let played = TransportEvent::Played {
                        tag: run.tag,
                        played: sound.rate().length_of(samples),
                    };
                    events.send(played).await;
                }
                After::Goes
            }
            FromClient::Text { text } => {
                events.send(TransportEvent::Text { who, text }).await;
                After::Goes
            }
            FromClient::Dtmf { keys } => {
                for key in keys.chars().filter_map(Key::from_char) {
                    events.send(TransportEvent::Dtmf { who, key }).await;
                }
                After::Goes
            }
            FromClient::Interrupt => match here.asked.try_send(FromClient::Interrupt) {
                Ok(()) => After::Goes,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let why = "too much that was asked is waiting to be done";
                    refused(here, ErrorCode::Busy, why).await
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    let why = "nobody here stops an agent for its client";
                    refused(here, ErrorCode::Unsupported, why).await
                }
            },
            FromClient::Bye => After::Left,
            FromClient::PlayedRtp { .. } => {
                let why = "on this connection what was played is said by its run";
                refused(here, ErrorCode::Unsupported, why).await
            }
            // Both were answered above.
            FromClient::Hello { .. } | FromClient::Unknown => After::Goes,
        }
    }

    /// Takes a message of the client's audio.
    async fn heard(
        &mut self,
        audio: Bytes,
        events: &mut Events<TransportEvent>,
    ) -> Result<(), (ErrorCode, &'static str)> {
        let who = self.who;
        let now = Instant::now();
        let malformed = |why| (ErrorCode::Malformed, why);
        match (self.sound, &mut self.pcm) {
            (Some(Sound::Pcm(_)), Some(pcm)) => {
                pcm.take(&audio, now).map_err(malformed)?;
                while let Some(packet) = pcm.next_packet() {
                    events.send(audio_of(who, packet)).await;
                }
            }
            (Some(Sound::Opus(_)), _) => {
                let packet = self.opus.take(audio, now).map_err(malformed)?;
                events.send(audio_of(who, packet)).await;
            }
            _ => {
                let why = "this connection carries no audio: its hello named none";
                return Err((ErrorCode::Unsupported, why));
            }
        }
        Ok(())
    }
}

/// What a transport does next: what its session asks, a client that is
/// to be attached, if anybody can still attach one, or what the client
/// that is there is told or says, if one is there. What comes first of
/// several is taken in that order.
async fn next(
    commands: &mut Commands<Command>,
    attached: Option<&mut mpsc::Receiver<Box<Attachment>>>,
    here: Option<&mut Here>,
) -> Next {
    let asked = async {
        match attached {
            Some(attached) => match stream::first(commands.next(), attached.recv()).await {
                Either::Left(command) => Next::Command(command),
                Either::Right(attachment) => Next::Attach(attachment),
            },
            None => Next::Command(commands.next().await),
        }
    };
    let Some(here) = here else {
        return asked.await;
    };
    let said = async {
        if here.telling {
            match stream::first(here.told.recv(), here.conn.next()).await {
                Either::Left(told) => Next::Told(told),
                Either::Right(heard) => Next::Heard(heard),
            }
        } else {
            Next::Heard(here.conn.next().await)
        }
    };
    match stream::first(asked, said).await {
        Either::Left(next) | Either::Right(next) => next,
    }
}

/// A packet of the client's audio, as the session is told of it.
fn audio_of(who: ParticipantId, packet: Packet) -> TransportEvent {
    let Packet {
        sequence,
        timestamp,
        payload,
    } = packet;
    TransportEvent::Audio {
        who,
        sequence,
        timestamp,
        payload,
    }
}

/// What became of a connection that something was sent on.
fn sent(done: Result<(), PortError>) -> After {
    done.map_or(After::Dropped, |()| After::Goes)
}

/// Tells the client that a message of its was not taken. The connection
/// goes on: the client may say it again, or something else.
async fn refused(here: &mut Here, code: ErrorCode, why: &str) -> After {
    let error = FromServer::Error {
        code,
        message: why.to_owned(),
    };
    sent(here.conn.send_text(wire::write(&error)).await)
}

/// Tells a client that another took its place, and closes its
/// connection.
async fn replaced(mut before: Here) {
    let bye = FromServer::Bye {
        reason: ByeReason::Replaced,
    };
    let _ = before.conn.send_text(wire::write(&bye)).await;
    before.conn.close_and_wait(None, LAST_WORDS).await;
}

/// Tells the client of a session that is over what it is still to be
/// told, and then goodbye.
async fn farewell(mut here: Here) {
    let by = Instant::now() + LAST_TOLD;
    while here.telling {
        match tokio::time::timeout_at(by, here.told.recv()).await {
            Ok(Some(message)) => {
                if here.conn.send_text(wire::write(&message)).await.is_err() {
                    return;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    let bye = FromServer::Bye {
        reason: ByeReason::Ended,
    };
    let _ = here.conn.send_text(wire::write(&bye)).await;
    here.conn.close_and_wait(None, LAST_WORDS).await;
}
