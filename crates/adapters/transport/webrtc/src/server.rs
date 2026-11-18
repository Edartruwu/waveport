//! The server's side of a call: the far end that connects, and the
//! transport of a session, which it is attached to.

use core::fmt;
use core::future::{pending, poll_fn};
use core::pin::pin;
use core::task::{Context, Poll};
use core::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use waveport_adapter_kit::client::{Hello, Welcome};
use waveport_adapter_kit::line::Runs;
use waveport_adapter_kit::stream::{self, Commands, Either, Events, Receiver, Sender};
use waveport_domain::event::Key;
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::{Full, PortError};
use waveport_ports::transport::{
    AudioCaps, FrameOut, KeysOut, RemoteBuffer, ToneOut, Transport, TransportCaps,
    TransportControl, TransportEvent,
};
use waveport_protocol::client::v1::{ByeReason, ErrorCode, FromClient, FromServer};

use crate::call::{Call, Client, Told, Unanswered};
use crate::endpoint::Line;
use crate::net::{Driven, now};

/// How many frames and requests may wait for the call.
const ROOM: usize = 64;

/// How many messages for the client may wait for the call. Whoever
/// tells more waits for room.
const TOLD: usize = 256;

/// How many things a client asked may wait for whoever does them. One
/// that finds no room is refused, and can be asked again.
const ASKED: usize = 16;

/// How many things the far end said where it made its offer may wait
/// for the call.
const SIGNALS: usize = 4;

/// How long a far end that has been told that its call is over is
/// still served before the call is ended: until it says that it has
/// what it was told, or hangs up itself. The end of a call is another
/// datagram than the goodbye and can overtake it on the way, and a far
/// end that sees the end first has not been told why.
///
/// A goodbye that is lost is not sent again in so short a time. Its
/// far end then finds its call ended with no word, comes back with its
/// token, and is told there.
const LAST_WORDS: Duration = Duration::from_millis(300);

/// How long a session that is over still takes what its client is to
/// be told, before the goodbye. Whoever tells the client of the session
/// is told of its end on another thread, a moment after the transport
/// is. With [`LAST_WORDS`] it is less than a session waits for its
/// transport's goodbye.
const LAST_TOLD: Duration = Duration::from_millis(150);

/// Something the far end said of its call where it made its offer.
pub(crate) enum Signal {
    /// Another offer of the same call, and where its answer goes.
    Offer(String, oneshot::Sender<Result<String, Unanswered>>),
    /// That the call is over.
    End,
}

/// What the far end says of its call later, where it made its offer:
/// that it is somewhere else and begins anew, or that it hangs up.
/// Whoever carried the offer holds this for as long as the far end may
/// come back there.
#[derive(Clone)]
pub struct Signals {
    said: mpsc::Sender<Signal>,
}

impl fmt::Debug for Signals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signals")
            .field("over", &self.is_over())
            .finish_non_exhaustive()
    }
}

impl Signals {
    /// Answers another offer of the same call: the far end is somewhere
    /// else than it was, or its line dropped, and its checks begin
    /// anew (an ICE restart). The call goes on as it was, with its
    /// session and everything said in it.
    ///
    /// It is answered by whoever runs the call, and waits for them: a
    /// call that connected and was attached to no session yet answers
    /// once it is.
    ///
    /// # Errors
    ///
    /// It is no offer the call can answer, and the call goes on as it
    /// was; or the call is over.
    pub async fn offer(&self, offer: &str) -> Result<String, Unanswered> {
        let (reply, answer) = oneshot::channel();
        let said = Signal::Offer(offer.to_owned(), reply);
        if self.said.send(said).await.is_err() {
            return Err(Unanswered::Over);
        }
        answer.await.unwrap_or(Err(Unanswered::Over))
    }

    /// The far end hung up. Its session is told that it left.
    pub async fn end(&self) {
        let _ = self.said.send(Signal::End).await;
    }

    /// Whether the call is over, and nothing can be said of it.
    #[must_use]
    pub fn is_over(&self) -> bool {
        self.said.is_closed()
    }
}

/// A call and what is said of it from outside.
struct Live {
    /// It is large, and is kept where it does not have to be moved.
    line: Box<Driven<Call, Line>>,
    signals: mpsc::Receiver<Signal>,
    /// Whether anybody can still say anything.
    signalled: bool,
}

impl Live {
    /// Does what the far end said where it made its offer. Returns
    /// whether the call goes on.
    fn signal(&mut self, signal: Signal) -> bool {
        match signal {
            Signal::Offer(offer, reply) => {
                let answer = self.line.core.answer_again(&offer);
                if answer.is_ok() {
                    self.line.wire.named(self.line.core.name());
                }
                let _ = reply.send(answer);
                true
            }
            Signal::End => false,
        }
    }

    /// Tells the far end why it is not served, and that it is not.
    fn turn_away(&mut self, code: ErrorCode, why: &str) {
        let message = why.to_owned();
        self.line.core.tell(&FromServer::Error { code, message });
        self.leave(ByeReason::Refused);
    }

    /// Tells the far end goodbye. The call is ended when it is let go
    /// of: once the far end had the time to hear it
    /// ([`last_words`](Self::last_words)), or at once.
    fn leave(&mut self, reason: ByeReason) {
        self.line.core.tell(&FromServer::Bye { reason });
        self.line.flush();
    }

    /// Serves a call whose far end was told goodbye until it has what
    /// it was told or has hung up, or for a moment. The call is ended
    /// then, as any that is let go of is.
    async fn last_words(mut self) {
        let heard = poll_fn(|cx| {
            // Running the call is what has it hear that the far end
            // has what it was told. What the far end says meanwhile is
            // of no interest any more.
            while self.line.poll_told(cx).is_ready() {}
            if self.line.core.has_told_all() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        let _ = tokio::time::timeout(LAST_WORDS, heard).await;
    }
}

impl Drop for Live {
    /// A call that is let go of is ended, at once. Its far end is told
    /// so by the call itself, and is not left to find it silent.
    fn drop(&mut self) {
        self.line.core.close();
        self.line.flush();
    }
}

/// A call that was answered and has not begun: the far end has not
/// been heard from.
pub struct Knock {
    live: Live,
}

impl fmt::Debug for Knock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Knock").finish_non_exhaustive()
    }
}

impl Knock {
    pub(crate) fn of(call: Driven<Call, Line>) -> (Self, Signals) {
        let (said, heard) = mpsc::channel(SIGNALS);
        let knock = Self {
            live: Live {
                line: Box::new(call),
                signals: heard,
                signalled: true,
            },
        };
        (knock, Signals { said })
    }

    /// Waits for the far end: for its checks, for the handshake, and,
    /// from a client that speaks the protocol, for its hello. It is
    /// work that takes a few milliseconds of a processor, and is done
    /// by whoever calls this, which is not the thread a session runs
    /// on.
    ///
    /// # Errors
    ///
    /// The far end did not come within `timeout`, hung up, or did not
    /// begin with a hello of this version. It has been told, where it
    /// could be, and the call is over.
    pub async fn connect(self, timeout: Duration) -> Result<Caller, PortError> {
        let mut live = self.live;
        let by = Instant::now() + timeout;
        let client = live.line.core.client();
        let mut up = false;
        let mut hello = None;
        while !(up && (client == Client::Plain || hello.is_some())) {
            let next = tokio::time::timeout_at(by, next(&mut live)).await;
            let Ok(next) = next else {
                let why = if up {
                    "no hello came in time"
                } else {
                    "the far end did not come in time"
                };
                live.turn_away(ErrorCode::OutOfPlace, why);
                if up {
                    live.last_words().await;
                }
                return Err(PortError::timeout(why));
            };
            let said = match next {
                Heard::Signal(Some(signal)) => {
                    if !live.signal(signal) {
                        live.leave(ByeReason::Ended);
                        return Err(PortError::transient("the far end hung up"));
                    }
                    continue;
                }
                Heard::Signal(None) => {
                    live.signalled = false;
                    continue;
                }
                Heard::Told(Told::Up) => {
                    up = true;
                    continue;
                }
                Heard::Told(Told::Over) => {
                    return Err(PortError::transient(
                        "the call ended before the far end was there",
                    ));
                }
                Heard::Told(Told::Said(said)) => said,
                // What it says and plays before there is a session to
                // hear it is not kept.
                Heard::Told(_) => continue,
            };
            let why = "the first message has to be a hello";
            if !matches!(said, FromClient::Hello { .. }) {
                live.turn_away(ErrorCode::OutOfPlace, why);
                live.last_words().await;
                return Err(PortError::permanent(why));
            }
            // Its version, and that its tokens are tokens.
            if let Err(refusal) = said.check() {
                let why = refusal.to_string();
                live.turn_away(refusal.code(), &why);
                live.last_words().await;
                return Err(PortError::permanent(why));
            }
            if let FromClient::Hello {
                token,
                audio,
                features,
                resume,
                ..
            } = said
            {
                hello = Some(Hello {
                    token,
                    audio,
                    features,
                    resume,
                });
            }
        }
        Ok(Caller { live, hello })
    }
}

/// What a call that nobody is attached to waits for.
enum Heard {
    Signal(Option<Signal>),
    Told(Told),
}

async fn next(live: &mut Live) -> Heard {
    poll_fn(|cx| {
        if live.signalled
            && let Poll::Ready(signal) = live.signals.poll_recv(cx)
        {
            return Poll::Ready(Heard::Signal(signal));
        }
        live.line.poll_told(cx).map(Heard::Told)
    })
    .await
}

/// A far end that is there, and that no session has been told of.
pub struct Caller {
    live: Live,
    hello: Option<Hello>,
}

impl fmt::Debug for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Caller")
            .field("client", &self.client())
            .field("hello", &self.hello)
            .finish_non_exhaustive()
    }
}

impl Caller {
    /// What kind of client it is.
    #[must_use]
    pub const fn client(&self) -> Client {
        self.live.line.core.client()
    }

    /// What it said as it came. `None` from one that does not speak the
    /// protocol.
    ///
    /// The audio a hello names is of no account on a call, whose audio
    /// is what its offer and answer agreed on.
    #[must_use]
    pub const fn hello(&self) -> Option<&Hello> {
        self.hello.as_ref()
    }

    /// Whether keys can be pressed at it as keys, and not as tones in
    /// the audio.
    fn has_keys(&self) -> bool {
        let kinds = self.live.line.core.kinds();
        kinds.is_some_and(|kinds| kinds.keys.is_some())
    }

    /// The transport of a session for a far end like this one. `who` is
    /// what the session knows its far end as.
    ///
    /// Nobody is on the transport yet. The session is started with it,
    /// and then this far end is attached ([`Handle::attach`]) with what
    /// the session was started as.
    #[must_use]
    pub fn transport(&self, who: ParticipantId) -> (RtcTransport, Handle) {
        let client = self.client();
        let keys = self.has_keys();
        let (attach, attached) = mpsc::channel(1);
        let (commands, events) = stream::pair(ROOM, move |commands, events| {
            let far = Far {
                who,
                client,
                runs: Runs::default(),
                joined: false,
            };
            far.serve(commands, attached, events)
        });
        let transport = RtcTransport {
            client,
            keys,
            commands,
            events,
            bidden: false,
            over: false,
        };
        let handle = Handle {
            attach,
            client,
            keys,
        };
        (transport, handle)
    }

    /// Tells the far end that it is not served, and why, where it can
    /// be told, and ends the call.
    pub async fn refuse(mut self, code: ErrorCode, why: &str) {
        self.live.turn_away(code, why);
        self.live.last_words().await;
    }
}

/// The call of a far end is over: it dropped for good, another took
/// its place, or the session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gone;

impl fmt::Display for Gone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the call of the client is over")
    }
}

impl core::error::Error for Gone {}

/// The rest of what a call carries, besides what the session and its
/// far end say to each other through the transport: what the client is
/// told of the session, and what it asks that a transport cannot do.
///
/// It is of one call. A client that comes back on another is attached
/// anew, and whoever attaches it is given another.
#[derive(Debug)]
pub struct Link {
    told: mpsc::Sender<FromServer>,
    asked: mpsc::Receiver<FromClient>,
}

impl Link {
    /// Tells the client `message`, after what it was told before. Waits
    /// while the client is behind. A client that does not speak the
    /// protocol is told nothing, and the message is let go of.
    ///
    /// When the session is over, the client is told goodbye once
    /// whoever holds this has let go of it, or after a moment: so what
    /// is told last is told before the goodbye.
    ///
    /// A goodbye that is told here ([`FromServer::Bye`]) is the last
    /// thing the call carries, and ends it and not the session: the
    /// session is told that its line dropped. That is how a server
    /// that is going away lets go of a client whose session is kept.
    ///
    /// # Errors
    ///
    /// The call is over, and the message was not sent.
    pub async fn tell(&self, message: FromServer) -> Result<(), Gone> {
        self.told.send(message).await.map_err(|_| Gone)
    }

    /// What the client asked next that is for whoever runs the session
    /// to do, and not for the transport: that the agent stop
    /// ([`FromClient::Interrupt`]). `None` when the call is over.
    pub async fn asked(&mut self) -> Option<FromClient> {
        self.asked.recv().await
    }
}

/// A far end that was not attached to a transport, and why.
pub struct Unattached {
    caller: Caller,
    code: ErrorCode,
    why: &'static str,
}

impl fmt::Debug for Unattached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Unattached")
            .field("code", &self.code)
            .field("why", &self.why)
            .finish_non_exhaustive()
    }
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

    /// Tells the far end why, and ends its call.
    pub async fn refuse(self) {
        self.caller.refuse(self.code, self.why).await;
    }
}

/// A far end on its way to a transport.
struct Attachment {
    live: Live,
    welcome: FromServer,
    told: mpsc::Receiver<FromServer>,
    asked: mpsc::Sender<FromClient>,
}

/// Gives a transport its far end: the first time, and each time the
/// client comes back on another call.
#[derive(Clone)]
pub struct Handle {
    attach: mpsc::Sender<Box<Attachment>>,
    client: Client,
    keys: bool,
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle")
            .field("client", &self.client)
            .field("over", &self.is_over())
            .finish_non_exhaustive()
    }
}

impl Handle {
    /// Whether the transport is gone, and with it its session: nobody
    /// can be attached to it.
    #[must_use]
    pub fn is_over(&self) -> bool {
        self.attach.is_closed()
    }

    /// Attaches `caller` to the transport, and has a client that speaks
    /// the protocol welcomed with `welcome`. The session is told that
    /// its far end is there: that it joined, the first time, and that
    /// the line is back after that.
    ///
    /// A far end that is on the transport already is told that another
    /// took its place, and the session that the line dropped and came
    /// back: what the new one hears begins with it.
    ///
    /// # Errors
    ///
    /// The session is over, or the far end is not of the kind the
    /// transport was made for, which a session cannot change. The far
    /// end has been told nothing yet.
    pub async fn attach(&self, caller: Caller, welcome: Welcome) -> Result<Link, Box<Unattached>> {
        let unattached = |caller, code, why| Box::new(Unattached { caller, code, why });
        if caller.client() != self.client || caller.has_keys() != self.keys {
            let why = "the session is under way with a client of another kind than this one";
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
            live: caller.live,
            // The audio of a call is what was agreed as it was set up.
            welcome: welcome.said(None),
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
        /// How many samples it holds, on the clock of the call.
        samples: u32,
        payload: Bytes,
        last: bool,
    },
    /// Audio that is of no response.
    Tone {
        samples: u32,
        payload: Bytes,
    },
    Clear {
        tag: OpTag,
    },
    Press {
        key: Key,
    },
    Close,
}

/// The transport of one session: the call of one far end, and of the
/// same client when it comes back on another.
#[derive(Debug)]
pub struct RtcTransport {
    client: Client,
    /// Whether keys are pressed at the far end as keys.
    keys: bool,
    commands: Sender<Command>,
    events: Receiver<TransportEvent>,
    /// Whether the far end has been asked to be told that the session is
    /// over.
    bidden: bool,
    /// Whether the transport is done with.
    over: bool,
}

/// How many samples of the call's clock `length` is.
fn samples_in(length: waveport_domain::time::MediaDur) -> u32 {
    u32::try_from(Rate::Hz48000.samples_in(length)).unwrap_or(u32::MAX)
}

impl Transport for RtcTransport {
    fn caps(&self) -> TransportCaps {
        // Only a client that speaks the protocol is told to discard
        // what it holds, and says what it has played.
        let told = self.client == Client::V1;
        TransportCaps {
            audio: Some(AudioCaps {
                inbound: Codec::Opus,
                outbound: Codec::Opus,
                remote_buffer: RemoteBuffer {
                    clearable: told,
                    reports_played: told,
                },
            }),
            text: told,
            keys: Some(if self.keys {
                KeysOut::OutOfBand
            } else {
                KeysOut::Tones
            }),
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
            samples: samples_in(frame.length),
            payload: Bytes::copy_from_slice(frame.payload),
            last: frame.last,
        })
    }

    fn try_control(&mut self, control: TransportControl) -> Result<(), Full> {
        if self.over {
            return Ok(());
        }
        self.commands.try_send(match control {
            TransportControl::Clear { tag } => Command::Clear { tag },
            TransportControl::Dtmf { key } => Command::Press { key },
        })
    }

    fn try_send_tone(&mut self, tone: ToneOut<'_>) -> Result<(), Full> {
        if self.over {
            return Ok(());
        }
        self.commands.try_send(Command::Tone {
            samples: samples_in(tone.length),
            payload: Bytes::copy_from_slice(tone.payload),
        })
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if self.over {
                return Poll::Ready(());
            }
            if !self.bidden {
                self.bidden = self.commands.try_send(Command::Close).is_ok();
            }
            // Running the call is what says goodbye. Whatever the far
            // end says in the meantime is of no interest any more.
            match self.events.poll_next(cx) {
                Poll::Ready(Some(Ok(_))) => {}
                Poll::Ready(Some(Err(_)) | None) => self.over = true,
                Poll::Pending if self.bidden => return Poll::Pending,
                Poll::Pending => {
                    // The call has taken what waited for it, so there
                    // is room for the request now.
                    self.bidden = self.commands.try_send(Command::Close).is_ok();
                    if !self.bidden {
                        return Poll::Pending;
                    }
                }
            }
        }
    }
}

/// The call that is attached, and what is told and asked over it.
struct Here {
    live: Live,
    told: mpsc::Receiver<FromServer>,
    /// Whether whoever tells the client still holds its end.
    telling: bool,
    asked: mpsc::Sender<FromClient>,
    /// Whether the session takes the far end to be there.
    up: bool,
}

/// What a transport waits for.
enum Next {
    Command(Option<Command>),
    Attach(Option<Box<Attachment>>),
    Signal(Option<Signal>),
    Told(Option<FromServer>),
    Happened(Told),
}

/// The next thing a transport has to deal with: what the session
/// asks, a far end to attach, and, of the one that is attached, what
/// happened on its call, what it said where it made its offer, and
/// what it is to be told.
async fn next_of(
    commands: &mut Commands<Command>,
    attaching: Option<&mut mpsc::Receiver<Box<Attachment>>>,
    mut here: Option<&mut Here>,
) -> Next {
    let mut command = pin!(commands.next());
    let mut attach = pin!(async {
        match attaching {
            Some(attaching) => attaching.recv().await,
            None => pending().await,
        }
    });
    poll_fn(|cx| {
        // The call first: what it wants sent is sent by asking it.
        if let Some(here) = &mut here {
            if let Poll::Ready(happened) = here.live.line.poll_told(cx) {
                return Poll::Ready(Next::Happened(happened));
            }
            if here.live.signalled
                && let Poll::Ready(signal) = here.live.signals.poll_recv(cx)
            {
                return Poll::Ready(Next::Signal(signal));
            }
        }
        if let Poll::Ready(command) = command.as_mut().poll(cx) {
            return Poll::Ready(Next::Command(command));
        }
        if let Poll::Ready(attachment) = attach.as_mut().poll(cx) {
            return Poll::Ready(Next::Attach(attachment));
        }
        // A client that is behind is told no more until it is not.
        if let Some(here) = &mut here
            && here.telling
            && here.live.line.core.has_room()
            && let Poll::Ready(message) = here.told.poll_recv(cx)
        {
            return Poll::Ready(Next::Told(message));
        }
        Poll::Pending
    })
    .await
}

/// How a transport came to its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The session said goodbye.
    Closed,
    /// The far end said goodbye, or is gone and nobody can bring it
    /// back.
    Left,
}

/// The far end of a transport, and what is known of it.
struct Far {
    who: ParticipantId,
    client: Client,
    runs: Runs,
    /// Whether a far end was ever attached.
    joined: bool,
}

impl Far {
    /// Runs the transport for as long as there is a session, with
    /// whatever call is attached to it.
    async fn serve(
        mut self,
        mut commands: Commands<Command>,
        mut attached: mpsc::Receiver<Box<Attachment>>,
        mut events: Events<TransportEvent>,
    ) -> Result<(), PortError> {
        let who = self.who;
        let mut here: Option<Here> = None;
        // Whether anybody can still attach a far end.
        let mut attachable = true;
        let end = loop {
            if here.is_none() && !attachable {
                // Nobody is there, and nobody can bring anyone.
                break End::Left;
            }
            let attaching = attachable.then_some(&mut attached);
            match next_of(&mut commands, attaching, here.as_mut()).await {
                // The transport is gone, or the session is over.
                Next::Command(None | Some(Command::Close)) => break End::Closed,
                Next::Command(Some(command)) => {
                    // With nobody there, there is nobody to send it to.
                    if let Some(here) = &mut here {
                        self.carry_out(&mut here.live.line.core, command);
                    }
                }
                Next::Attach(None) => attachable = false,
                Next::Attach(Some(attachment)) => {
                    self.attach(&mut here, *attachment, &mut events).await;
                }
                Next::Signal(None) => {
                    if let Some(here) = &mut here {
                        here.live.signalled = false;
                    }
                }
                Next::Signal(Some(signal)) => {
                    if let Some(here) = &mut here
                        && !here.live.signal(signal)
                    {
                        // It hung up.
                        break End::Left;
                    }
                }
                Next::Told(None) => {
                    if let Some(here) = &mut here {
                        here.telling = false;
                    }
                }
                Next::Told(Some(FromServer::Bye { reason })) => {
                    // It ends the call, and not the session.
                    if let Some(mut there) = here.take() {
                        there.live.leave(reason);
                        there.live.last_words().await;
                        if there.up {
                            events.send(TransportEvent::Detached).await;
                        }
                    }
                }
                Next::Told(Some(message)) => {
                    if let Some(there) = &mut here {
                        there.live.line.core.tell(&message);
                    }
                }
                Next::Happened(happened) => {
                    let Some(there) = &mut here else {
                        continue;
                    };
                    match self.take(there, happened, &mut events).await {
                        After::Goes => {}
                        After::Dropped => {
                            // Whoever told this client is told that it
                            // is gone by having nobody to tell.
                            let was_up = there.up;
                            here = None;
                            if was_up {
                                events.send(TransportEvent::Detached).await;
                            }
                        }
                        After::Left => break End::Left,
                    }
                }
            }
        };
        match (end, here) {
            (End::Closed, Some(here)) => farewell(here).await,
            // It said goodbye, or hung up, and is told nothing more:
            // its call is ended by being let go of.
            (End::Left, Some(_)) | (_, None) => {}
        }
        // A client that was on its way to the transport finds it over,
        // and is told so.
        attached.close();
        while let Ok(attachment) = attached.try_recv() {
            let mut live = attachment.live;
            live.turn_away(ErrorCode::NoSession, "the session is over");
            live.last_words().await;
        }
        if end == End::Left && self.joined {
            events.send(TransportEvent::Left { who }).await;
        }
        Ok(())
    }

    /// Puts a far end on the transport, in place of whoever was there.
    async fn attach(
        &mut self,
        here: &mut Option<Here>,
        attachment: Attachment,
        events: &mut Events<TransportEvent>,
    ) {
        let Attachment {
            mut live,
            welcome,
            told,
            asked,
        } = attachment;
        // Nobody has looked at its call since it said hello. One whose
        // call ended meanwhile was never there, and takes nobody's
        // place.
        poll_fn(|cx| {
            live.line.catch_up(cx);
            Poll::Ready(())
        })
        .await;
        if live.line.core.is_over() {
            return;
        }
        if let Some(mut before) = here.take() {
            // What the new one hears begins with it: the session is
            // told that the line dropped, if it has not been. The one
            // before is told why its call ends and is not waited for:
            // it is most likely gone, which is why another came, and
            // the one that came is not kept waiting for it.
            before.live.leave(ByeReason::Replaced);
            let was_up = before.up;
            drop(before);
            if was_up {
                events.send(TransportEvent::Detached).await;
            }
        }
        live.line.core.tell(&welcome);
        // It holds nothing of what the one before it was sent.
        self.runs.begin_anew();
        *here = Some(Here {
            live,
            told,
            telling: true,
            asked,
            up: true,
        });
        let back = if self.joined {
            TransportEvent::Attached
        } else {
            TransportEvent::Joined { who: self.who }
        };
        self.joined = true;
        events.send(back).await;
    }

    /// Does what the session asked.
    fn carry_out(&mut self, call: &mut Call, command: Command) {
        match command {
            Command::Frame {
                tag,
                samples,
                payload,
                last,
            } => call.frame(now(), &mut self.runs, tag, samples, &payload, last),
            Command::Tone { samples, payload } => call.tone(now(), samples, &payload),
            Command::Clear { tag } => call.clear(&mut self.runs, tag),
            Command::Press { key } => {
                // A far end that keys cannot be pressed at was said to
                // be one, and is not asked.
                call.press(now(), key);
            }
            // It ends the transport, and is taken before this.
            Command::Close => {}
        }
    }

    /// Takes what happened on the call.
    async fn take(
        &mut self,
        here: &mut Here,
        happened: Told,
        events: &mut Events<TransportEvent>,
    ) -> After {
        let who = self.who;
        let event = match happened {
            Told::Up => {
                if here.up {
                    return After::Goes;
                }
                here.up = true;
                TransportEvent::Attached
            }
            Told::Down => {
                if !here.up {
                    return After::Goes;
                }
                here.up = false;
                TransportEvent::Detached
            }
            Told::Audio {
                sequence,
                timestamp,
                payload,
            } => TransportEvent::Audio {
                who,
                sequence,
                timestamp,
                payload,
            },
            Told::Key(key) => TransportEvent::Dtmf { who, key },
            Told::Played { tag, played } => TransportEvent::Played { tag, played },
            Told::Lost(percent) => TransportEvent::Loss { percent },
            Told::Said(said) => return self.hear(here, said, events).await,
            // A client that speaks the protocol and did not say goodbye
            // may come back on another call. Any other has hung up.
            Told::Over => {
                return match self.client {
                    Client::V1 => After::Dropped,
                    Client::Plain => After::Left,
                };
            }
        };
        events.send(event).await;
        After::Goes
    }

    /// Takes what the client said in the protocol.
    async fn hear(
        &mut self,
        here: &mut Here,
        said: FromClient,
        events: &mut Events<TransportEvent>,
    ) -> After {
        let who = self.who;
        let refuse = |here: &mut Here, code, why: &str| {
            let message = why.to_owned();
            here.live
                .line
                .core
                .tell(&FromServer::Error { code, message });
        };
        match said {
            FromClient::Text { text } => {
                events.send(TransportEvent::Text { who, text }).await;
            }
            FromClient::Dtmf { keys } => {
                for key in keys.chars().filter_map(Key::from_char) {
                    events.send(TransportEvent::Dtmf { who, key }).await;
                }
            }
            FromClient::Interrupt => match here.asked.try_send(FromClient::Interrupt) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let why = "too much that was asked is waiting to be done";
                    refuse(here, ErrorCode::Busy, why);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    let why = "nobody here stops an agent for its client";
                    refuse(here, ErrorCode::Unsupported, why);
                }
            },
            FromClient::Bye => return After::Left,
            // It is said once, as a call begins.
            FromClient::Hello { .. } => refuse(here, ErrorCode::OutOfPlace, "hello is said once"),
            // What it has played the call has taken, and has answered
            // what it took for nothing.
            _ => {}
        }
        After::Goes
    }
}

/// What became of a call after something happened on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
    /// It goes on.
    Goes,
    /// It is over, and its client may come back on another.
    Dropped,
    /// The far end said goodbye.
    Left,
}

/// Says goodbye to a far end whose session is over: what whoever tells
/// it of the session still has to say first, then the goodbye, and the
/// call is ended.
async fn farewell(mut here: Here) {
    let by = Instant::now() + LAST_TOLD;
    while here.telling {
        let said = stream::first(here.told.recv(), here.live.line.told());
        match tokio::time::timeout_at(by, said).await {
            Ok(Either::Left(Some(message))) => {
                here.live.line.core.tell(&message);
            }
            Ok(Either::Left(None)) | Err(_) => break,
            Ok(Either::Right(Told::Over)) => return,
            Ok(Either::Right(_)) => {}
        }
    }
    here.live.leave(ByeReason::Ended);
    here.live.last_words().await;
}

#[cfg(test)]
mod tests {
    use core::future::poll_fn;
    use core::pin::pin;
    use core::task::Poll;
    use core::time::Duration;
    use std::io::ErrorKind;

    use bytes::Bytes;
    use tokio::net::{TcpStream, UdpSocket};
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;
    use waveport_adapter_kit::client::{Hello, Welcome};
    use waveport_domain::event::Key;
    use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId};
    use waveport_domain::time::MediaDur;
    use waveport_ports::error::{PortError, RetryClass};
    use waveport_ports::transport::{
        FrameOut, KeysOut, ToneOut, Transport, TransportControl, TransportEvent,
    };
    use waveport_protocol::client::v1::{ByeReason, ErrorCode, FromClient, FromServer};

    use super::{Caller, Handle, Link, RtcTransport, Signals};
    use crate::call::Client;
    use crate::dial::{Dialer, Heard, Unconnected};
    use crate::endpoint::{Config, Endpoint};
    use crate::net::now;
    use crate::peer::{Like, PeerHeard};
    use crate::rtp::forged_check;

    const CALLER: ParticipantId = ParticipantId(3);
    const SOON: Duration = Duration::from_secs(10);

    fn packet(n: u8) -> Bytes {
        Bytes::from(vec![0xF8, n, 7])
    }

    fn welcome() -> Welcome {
        Welcome {
            session: "s-7".to_owned(),
            life: 1,
            resumed: false,
            resume_token: None,
            features: Vec::new(),
            next_seq: 1,
            grace_ms: None,
        }
    }

    /// The next `events` things the transport says, but for what the
    /// far end lost. Fewer when it closes first.
    async fn says(transport: &mut RtcTransport, events: usize) -> Vec<TransportEvent> {
        let mut said = Vec::new();
        let enough = poll_fn(|cx| {
            while said.len() < events {
                match transport.poll_event(cx) {
                    Poll::Ready(Some(TransportEvent::Loss { .. })) => {}
                    Poll::Ready(Some(event)) => said.push(event),
                    Poll::Ready(None) => break,
                    Poll::Pending => return Poll::Pending,
                }
            }
            Poll::Ready(())
        });
        let enough = tokio::time::timeout(SOON, enough).await;
        assert!(enough.is_ok(), "the transport said only {said:?}");
        said
    }

    #[tokio::test]
    async fn a_far_end_that_does_not_speak_the_protocol_is_sent_audio_and_told_nothing() {
        let local = "127.0.0.1:0".parse().unwrap();
        let (endpoint, pump) = Endpoint::bind(Config::at(local)).await.unwrap();
        tokio::spawn(pump.run());
        let (dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
        let answered = endpoint.offer(&offer, Client::Plain).unwrap();

        // The far end: it says three packets when it is asked to,
        // says what it hears, and hangs up when the asking ends.
        let (heard, mut at_the_far_end) = mpsc::unbounded_channel();
        let (speak, mut asked) = mpsc::unbounded_channel::<()>();
        let answer = answered.answer;
        tokio::spawn(async move {
            let mut driven = dialing.driven;
            assert!(driven.core.answered(&answer));
            // It hangs up when it is asked to and its channel is there:
            // a call whose channel is still being set up cannot be
            // ended so that the server hears of it.
            let (mut open, mut leaving) = (false, false);
            while !(open && leaving) {
                let next = poll_fn(|cx| {
                    if !leaving && let Poll::Ready(asked) = asked.poll_recv(cx) {
                        return Poll::Ready(Err(asked));
                    }
                    driven.poll_told(cx).map(Ok)
                });
                match next.await {
                    Ok(told) => {
                        open |= told == PeerHeard::Open;
                        heard.send(told).unwrap();
                    }
                    Err(Some(())) => {
                        for n in 0..3 {
                            assert!(driven.core.speak(now(), &packet(n)));
                        }
                    }
                    Err(None) => leaving = true,
                }
            }
            driven.core.close();
            driven.flush();
        });

        let caller = answered.knock.connect(SOON).await.unwrap();
        assert_eq!(caller.client(), Client::Plain);
        assert_eq!(caller.hello(), None);
        let (mut transport, handle) = caller.transport(CALLER);
        // It is not told to discard what it holds, and does not say
        // what it played; it types nothing; keys are pressed at it as
        // keys.
        let caps = transport.caps();
        let audio = caps.audio.unwrap();
        assert!(!audio.remote_buffer.clearable && !audio.remote_buffer.reports_played);
        assert!(!caps.text);
        assert_eq!(caps.keys, Some(KeysOut::OutOfBand));

        let link = handle.attach(caller, welcome()).await.unwrap();
        assert_eq!(
            says(&mut transport, 1).await,
            [TransportEvent::Joined { who: CALLER }]
        );
        speak.send(()).unwrap();
        let said = says(&mut transport, 3).await;
        let audio: Vec<&Bytes> = said
            .iter()
            .map(|event| match event {
                TransportEvent::Audio { payload, .. } => payload,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(audio, [&packet(0), &packet(1), &packet(2)]);

        // What the session says reaches it as audio. What it would be
        // told of that, and of the session, is told to nobody.
        let tag = OpTag {
            generation: GenId(1),
            epoch: Epoch(1),
        };
        link.tell(FromServer::Gap { resumes: 2 }).await.unwrap();
        for (n, last) in [(8, false), (9, true)] {
            let frame = FrameOut {
                tag,
                at: MediaDur::from_millis(0),
                length: MediaDur::from_millis(20),
                payload: &packet(n),
                last,
            };
            transport.try_send(frame).unwrap();
        }
        transport
            .try_control(TransportControl::Clear { tag })
            .unwrap();
        let mut got = Vec::new();
        while got.len() < 2 {
            let next = async {
                tokio::select! {
                    heard = at_the_far_end.recv() => heard,
                    _ = says(&mut transport, 1) => None,
                }
            };
            match tokio::time::timeout(SOON, next).await.unwrap() {
                Some(PeerHeard::Audio { payload, .. }) => got.push(payload),
                Some(PeerHeard::Open | PeerHeard::Up | PeerHeard::Down) => {}
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, [packet(8), packet(9)]);

        // It hangs up, which is how one that says no goodbye leaves.
        drop(speak);
        let said = says(&mut transport, 2).await;
        assert_eq!(said, [TransportEvent::Left { who: CALLER }]);
        assert!(handle.is_over());
        assert_eq!(endpoint.calls(), 0);
    }

    /// An endpoint on this machine, with its datagrams carried.
    async fn endpoint() -> Endpoint {
        let local = "127.0.0.1:0".parse().unwrap();
        let (endpoint, pump) = Endpoint::bind(Config::at(local)).await.unwrap();
        tokio::spawn(pump.run());
        endpoint
    }

    /// One that far ends can connect to as well.
    async fn endpoint_with_a_listener() -> Endpoint {
        let local = "127.0.0.1:0".parse().unwrap();
        let config = Config {
            tcp: Some(local),
            ..Config::at(local)
        };
        let (endpoint, pump) = Endpoint::bind(config).await.unwrap();
        tokio::spawn(pump.run());
        endpoint
    }

    /// A client that is on its call when it has been welcomed.
    type Dialed = JoinHandle<Result<Dialer, Unconnected>>;

    /// A client of the protocol calls: the caller as a server has it
    /// once it said hello, and the client, which waits to be welcomed.
    async fn calls(endpoint: &Endpoint, like: Like) -> (Caller, Dialed, Signals) {
        calls_over(endpoint, like, false).await
    }

    /// As [`calls`], over a connection or not.
    async fn calls_over(endpoint: &Endpoint, like: Like, tcp: bool) -> (Caller, Dialed, Signals) {
        let local = "127.0.0.1:0".parse().unwrap();
        let (dialing, offer) = Dialer::offer(like, local, tcp).await.unwrap();
        let answered = endpoint.offer(&offer, Client::V1).unwrap();
        let answer = answered.answer;
        let dialed = tokio::spawn(async move {
            let hello = Hello::new(None);
            dialing.answered(&answer, &hello, SOON).await
        });
        let caller = answered.knock.connect(SOON).await.unwrap();
        (caller, dialed, answered.signals)
    }

    /// What `far` comes to, while the transport is served as a session
    /// serves it: a transport does what it does when it is asked what
    /// happened. Returns that too, but for what the far end lost.
    async fn meanwhile<T>(
        transport: &mut RtcTransport,
        far: impl Future<Output = T>,
    ) -> (T, Vec<TransportEvent>) {
        let mut far = pin!(far);
        let mut said = Vec::new();
        let done = poll_fn(|cx| {
            while let Poll::Ready(Some(event)) = transport.poll_event(cx) {
                if !matches!(event, TransportEvent::Loss { .. }) {
                    said.push(event);
                }
            }
            far.as_mut().poll(cx)
        });
        let done = tokio::time::timeout(SOON, done).await.unwrap();
        (done, said)
    }

    /// A client of the protocol that is on a call and on the transport
    /// of its session.
    async fn on_a_call(endpoint: &Endpoint, like: Like) -> (RtcTransport, Handle, Link, Dialer) {
        on_a_call_over(endpoint, like, false).await
    }

    /// As [`on_a_call`], over a connection or not.
    async fn on_a_call_over(
        endpoint: &Endpoint,
        like: Like,
        tcp: bool,
    ) -> (RtcTransport, Handle, Link, Dialer) {
        let (caller, dialed, _signals) = calls_over(endpoint, like, tcp).await;
        let (mut transport, handle) = caller.transport(CALLER);
        let link = handle.attach(caller, welcome()).await.unwrap();
        assert_eq!(
            says(&mut transport, 1).await,
            [TransportEvent::Joined { who: CALLER }]
        );
        let (dialed, said) = meanwhile(&mut transport, dialed).await;
        assert_eq!(said, []);
        (transport, handle, link, dialed.unwrap().unwrap())
    }

    /// The next thing a client hears, which is something.
    async fn hears(dialer: &mut Dialer) -> Heard {
        let next = tokio::time::timeout(SOON, dialer.next()).await;
        next.unwrap().unwrap().unwrap()
    }

    /// What a far end is told that opens its call with `first`, until
    /// its call is over, and what whoever waited for it is told.
    async fn greets(first: FromClient) -> (Result<Caller, PortError>, Vec<FromServer>) {
        let endpoint = endpoint().await;
        let local = "127.0.0.1:0".parse().unwrap();
        let (dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
        let answered = endpoint.offer(&offer, Client::V1).unwrap();
        let answer = answered.answer;
        let far = tokio::spawn(async move {
            let mut driven = dialing.driven;
            assert!(driven.core.answered(&answer));
            let mut told = Vec::new();
            loop {
                match driven.told().await {
                    PeerHeard::Open => assert!(driven.core.say(&first)),
                    PeerHeard::Said(said) => told.push(said),
                    PeerHeard::Over => return told,
                    _ => {}
                }
            }
        });
        let caller = answered.knock.connect(SOON).await;
        let told = tokio::time::timeout(SOON, far).await.unwrap().unwrap();
        assert_eq!(endpoint.calls(), usize::from(caller.is_ok()));
        (caller, told)
    }

    #[tokio::test]
    async fn a_far_end_whose_first_word_is_no_hello_is_turned_away_and_told_why() {
        // Something else than a hello.
        let text = FromClient::Text {
            text: "is anybody there".to_owned(),
        };
        let (caller, told) = greets(text).await;
        let refused = caller.unwrap_err();
        assert_eq!(refused.class, RetryClass::Permanent);
        let [FromServer::Error { code, .. }, FromServer::Bye { reason }] = told[..] else {
            panic!("{told:?}");
        };
        assert_eq!((code, reason), (ErrorCode::OutOfPlace, ByeReason::Refused));

        // A hello in a version that is not spoken here.
        let FromClient::Hello {
            token,
            audio,
            features,
            resume,
            ..
        } = Hello::new(None).said()
        else {
            panic!("a hello is a hello");
        };
        let newer = FromClient::Hello {
            v: 2,
            token,
            audio,
            features,
            resume,
        };
        let (caller, told) = greets(newer).await;
        let refused = caller.unwrap_err();
        assert_eq!(refused.class, RetryClass::Permanent);
        let [FromServer::Error { code, .. }, FromServer::Bye { reason }] = told[..] else {
            panic!("{told:?}");
        };
        assert_eq!((code, reason), (ErrorCode::Version, ByeReason::Refused));
    }

    #[tokio::test]
    async fn a_far_end_that_hangs_up_before_it_is_there_is_waited_for_no_longer() {
        let endpoint = endpoint().await;
        let local = "127.0.0.1:0".parse().unwrap();
        // It offers, and takes its offer back where it made it, before
        // it has sent anything.
        let (_dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
        let answered = endpoint.offer(&offer, Client::V1).unwrap();
        assert_eq!(endpoint.calls(), 1);
        answered.signals.end().await;
        let waited = Duration::from_secs(2);
        let caller = tokio::time::timeout(waited, answered.knock.connect(SOON)).await;
        let gone = caller.unwrap().unwrap_err();
        assert_eq!(gone.class, RetryClass::Transient);
        assert_eq!(endpoint.calls(), 0);
        assert!(answered.signals.is_over());
    }

    #[tokio::test]
    async fn a_client_of_another_kind_than_the_session_began_with_is_not_attached() {
        let endpoint = endpoint().await;
        // The session began with a client of the protocol at which keys
        // are pressed as keys.
        let (first, first_dialed, _first_signals) = calls(&endpoint, Like::Chrome).await;
        let (mut transport, handle) = first.transport(CALLER);
        assert_eq!(transport.caps().keys, Some(KeysOut::OutOfBand));

        // One that takes no keys: the session would press them at
        // nobody.
        let (bare, bare_dialed, _bare_signals) = calls(&endpoint, Like::Bare).await;
        let unattached = handle.attach(bare, welcome()).await.unwrap_err();
        assert_eq!(unattached.code(), ErrorCode::Unsupported);
        unattached.refuse().await;
        let refused = bare_dialed.await.unwrap().unwrap_err();
        assert!(
            matches!(
                refused,
                Unconnected::Refused {
                    code: ErrorCode::Unsupported,
                    ..
                }
            ),
            "{refused:?}"
        );

        // One that does not speak the protocol: the session would wait
        // to be told what it played.
        let local = "127.0.0.1:0".parse().unwrap();
        let (dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
        let answered = endpoint.offer(&offer, Client::Plain).unwrap();
        let answer = answered.answer;
        let far = tokio::spawn(async move {
            let mut driven = dialing.driven;
            assert!(driven.core.answered(&answer));
            while driven.told().await != PeerHeard::Over {}
        });
        let plain = answered.knock.connect(SOON).await.unwrap();
        let unattached = handle.attach(plain, welcome()).await.unwrap_err();
        assert_eq!(unattached.code(), ErrorCode::Unsupported);
        unattached.refuse().await;
        tokio::time::timeout(SOON, far).await.unwrap().unwrap();

        // Nobody was on the transport for any of that, and the client
        // it was made for still is welcome.
        let _link = handle.attach(first, welcome()).await.unwrap();
        assert_eq!(
            says(&mut transport, 1).await,
            [TransportEvent::Joined { who: CALLER }]
        );
        first_dialed.await.unwrap().unwrap();
        assert_eq!(endpoint.calls(), 1);
    }

    #[tokio::test]
    async fn a_transport_nobody_came_to_ends_and_nobody_has_left() {
        let endpoint = endpoint().await;
        let (caller, dialed, _signals) = calls(&endpoint, Like::Chrome).await;
        let (mut transport, handle) = caller.transport(CALLER);
        // Whoever was to attach the client gives up.
        drop(handle);
        drop(caller);
        assert_eq!(says(&mut transport, 1).await, []);
        dialed.abort();
    }

    #[tokio::test]
    async fn a_client_whose_call_ends_with_no_goodbye_has_dropped_and_comes_back_on_another() {
        let endpoint = endpoint().await;
        let (mut transport, handle, _link, mut first) = on_a_call(&endpoint, Like::Chrome).await;
        // The page is closed: the call ends, and nothing was said.
        first.driven.core.close();
        first.driven.flush();
        assert_eq!(says(&mut transport, 1).await, [TransportEvent::Detached]);
        assert!(!handle.is_over());
        // The client comes back on another call, to the session it
        // left.
        let (caller, again, _signals) = calls(&endpoint, Like::Chrome).await;
        let _link = handle.attach(caller, welcome()).await.unwrap();
        assert_eq!(says(&mut transport, 1).await, [TransportEvent::Attached]);
        let (welcomed, said) = meanwhile(&mut transport, again).await;
        welcomed.unwrap().unwrap();
        assert_eq!(said, []);
    }

    #[tokio::test]
    async fn a_far_end_that_falls_silent_is_a_line_that_dropped() {
        let endpoint = endpoint().await;
        let (mut transport, handle, _link, dialer) = on_a_call(&endpoint, Like::Chrome).await;
        // The far end is gone with no word, as one whose network is.
        let since = now();
        dialer.drop_line();
        assert_eq!(says(&mut transport, 1).await, [TransportEvent::Detached]);
        // Not at once: a far end that is there may say nothing for a
        // while.
        assert!(
            since.elapsed() >= Duration::from_secs(4),
            "{:?}",
            since.elapsed()
        );
        assert!(!handle.is_over());
    }

    #[tokio::test]
    async fn what_a_client_says_that_no_transport_can_do_is_passed_on_or_answered() {
        let endpoint = endpoint().await;
        let (mut transport, _handle, mut link, mut dialer) =
            on_a_call(&endpoint, Like::Chrome).await;
        // A hello is said once.
        dialer.say(&Hello::new(None).said()).unwrap();
        // Keys that are written are keys, each of them.
        let keys = FromClient::Dtmf {
            keys: "12".to_owned(),
        };
        dialer.say(&keys).unwrap();
        // Whoever serves the client is asked to stop the agent, as
        // often as there is room to keep that: sixteen times.
        for _ in 0..17 {
            dialer.interrupt().unwrap();
        }
        let pressed = [Key::One, Key::Two].map(|key| TransportEvent::Dtmf { who: CALLER, key });
        let answered = async { [hears(&mut dialer).await, hears(&mut dialer).await] };
        let (answered, said) = meanwhile(&mut transport, answered).await;
        assert_eq!(said, pressed);
        let codes = answered.map(|heard| match heard {
            Heard::Error { code, .. } => code,
            other => panic!("{other:?}"),
        });
        assert_eq!(codes, [ErrorCode::OutOfPlace, ErrorCode::Busy]);
        for _ in 0..16 {
            assert_eq!(link.asked().await, Some(FromClient::Interrupt));
        }
        // And when nobody serves it any more, it is told that.
        drop(link);
        dialer.interrupt().unwrap();
        let (heard, _) = meanwhile(&mut transport, hears(&mut dialer)).await;
        let Heard::Error { code, .. } = heard else {
            panic!("{heard:?}");
        };
        assert_eq!(code, ErrorCode::Unsupported);
    }

    #[tokio::test]
    async fn a_far_end_with_no_keys_of_its_own_is_sent_their_tones() {
        let endpoint = endpoint().await;
        let (mut transport, _handle, _link, mut dialer) = on_a_call(&endpoint, Like::Bare).await;
        // The session is told to make the tones itself.
        assert_eq!(transport.caps().keys, Some(KeysOut::Tones));
        let tone = ToneOut {
            length: MediaDur::from_millis(20),
            payload: &packet(5),
        };
        transport.try_send_tone(tone).unwrap();
        // It is audio of no response.
        let (heard, _) = meanwhile(&mut transport, hears(&mut dialer)).await;
        assert_eq!(heard, Heard::Audio(packet(5)));
    }

    #[tokio::test]
    async fn a_session_is_told_what_its_far_end_lost_of_what_it_was_sent() {
        let endpoint = endpoint().await;
        let (mut transport, _handle, _link, mut dialer) = on_a_call(&endpoint, Like::Chrome).await;
        // The far end hears, which is when it says what arrived.
        let far = tokio::spawn(async move { while let Ok(Some(_)) = dialer.next().await {} });
        let tag = OpTag {
            generation: GenId(1),
            epoch: Epoch(1),
        };
        // A few seconds of a response, at the pace it is heard at.
        let mut lost = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        for n in 0..200_u8 {
            tick.tick().await;
            let frame = FrameOut {
                tag,
                at: MediaDur::from_millis(u32::from(n) * 20),
                length: MediaDur::from_millis(20),
                payload: &packet(n),
                last: false,
            };
            transport.try_send(frame).unwrap();
            poll_fn(|cx| {
                while let Poll::Ready(Some(event)) = transport.poll_event(cx) {
                    if let TransportEvent::Loss { percent } = event {
                        lost.push(percent);
                    }
                }
                Poll::Ready(())
            })
            .await;
        }
        // On this machine nothing is lost, and that is said, more than
        // once.
        assert!(lost.len() >= 2, "{lost:?}");
        assert!(lost.iter().all(|percent| *percent == 0), "{lost:?}");
        far.abort();
    }

    #[tokio::test]
    async fn what_a_session_says_of_itself_as_it_ends_is_told_before_the_goodbye() {
        let endpoint = endpoint().await;
        let (mut transport, _handle, link, mut dialer) = on_a_call(&endpoint, Like::Chrome).await;
        // The session closes its transport with something still to be
        // told to its client.
        link.tell(FromServer::Gap { resumes: 7 }).await.unwrap();
        let closed = async {
            poll_fn(|cx| transport.poll_close(cx)).await;
            drop(link);
        };
        let heard = tokio::join!(closed, async {
            [hears(&mut dialer).await, hears(&mut dialer).await]
        });
        assert_eq!(
            heard.1,
            [Heard::Gap { resumes: 7 }, Heard::Bye(ByeReason::Ended)]
        );
    }

    /// The one packet of audio the transport says it heard next.
    async fn hears_audio(transport: &mut RtcTransport) -> Bytes {
        let said = says(transport, 1).await;
        let [TransportEvent::Audio { payload, .. }] = &said[..] else {
            panic!("{said:?}");
        };
        payload.clone()
    }

    #[tokio::test]
    async fn somebody_who_knows_the_name_of_a_call_does_not_take_its_far_ends_place() {
        let endpoint = endpoint().await;
        let (mut transport, _handle, _link, mut dialer) = on_a_call(&endpoint, Like::Chrome).await;
        let names = endpoint.names();
        let [name] = &names[..] else {
            panic!("{names:?}");
        };
        let reached = endpoint.reached_from();
        assert_eq!(reached.len(), 1);
        // Checks that are addressed to the call, from more places than
        // a call is reached from at once, by whoever has not its
        // password.
        let mut strangers = Vec::new();
        for _ in 0..12 {
            let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let check = forged_check(name);
            stranger.send_to(&check, endpoint.udp()).await.unwrap();
            strangers.push(stranger);
        }
        // The far end is heard as it was, from where it was: the call
        // is reached from nowhere else.
        dialer.audio(&packet(4)).unwrap();
        assert_eq!(hears_audio(&mut transport).await, packet(4));
        assert_eq!(endpoint.reached_from(), reached);
    }

    #[tokio::test]
    async fn a_connection_that_is_not_the_far_ends_does_not_take_the_place_of_the_one_that_is() {
        let endpoint = endpoint_with_a_listener().await;
        let (mut transport, _handle, link, mut dialer) =
            on_a_call_over(&endpoint, Like::Chrome, true).await;
        let names = endpoint.names();
        let [name] = &names[..] else {
            panic!("{names:?}");
        };
        // Somebody connects, and the first thing they say is a check
        // that is addressed to the call.
        let check = forged_check(name);
        let mut first = u16::try_from(check.len()).unwrap().to_be_bytes().to_vec();
        first.extend_from_slice(&check);
        let stranger = TcpStream::connect(endpoint.tcp().unwrap()).await.unwrap();
        stranger.writable().await.unwrap();
        assert_eq!(stranger.try_write(&first).unwrap(), first.len());
        // They are let go of: nothing is said to them, and their
        // connection ends.
        let let_go = async {
            let mut said = [0_u8; 64];
            loop {
                stranger.readable().await.unwrap();
                match stranger.try_read(&mut said) {
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    read => break read.unwrap_or(0),
                }
            }
        };
        let (said, happened) = meanwhile(&mut transport, let_go).await;
        assert_eq!((said, happened), (0, Vec::new()));
        // The far end is heard as it was, and is told what it is told.
        dialer.audio(&packet(4)).unwrap();
        assert_eq!(hears_audio(&mut transport).await, packet(4));
        link.tell(FromServer::Gap { resumes: 2 }).await.unwrap();
        let (heard, _) = meanwhile(&mut transport, hears(&mut dialer)).await;
        assert!(matches!(heard, Heard::Gap { resumes: 2 }), "{heard:?}");
    }

    #[tokio::test]
    async fn connections_that_say_nothing_do_not_keep_a_far_end_out() {
        let endpoint = endpoint_with_a_listener().await;
        // More of them than may wait to say which call they are for.
        let mut idle = Vec::new();
        for _ in 0..80 {
            idle.push(TcpStream::connect(endpoint.tcp().unwrap()).await.unwrap());
        }
        // A far end that can send no datagram is on its call before
        // any of them is given up on.
        let since = now();
        let (mut transport, _handle, _link, mut dialer) =
            on_a_call_over(&endpoint, Like::Chrome, true).await;
        assert!(
            since.elapsed() < Duration::from_secs(3),
            "{:?}",
            since.elapsed()
        );
        dialer.audio(&packet(4)).unwrap();
        assert_eq!(hears_audio(&mut transport).await, packet(4));
        drop(idle);
    }

    /// How long a transport takes to say goodbye.
    async fn closes(transport: &mut RtcTransport) -> Duration {
        let since = now();
        let closed = poll_fn(|cx| transport.poll_close(cx));
        tokio::time::timeout(SOON, closed).await.unwrap();
        since.elapsed()
    }

    #[tokio::test]
    async fn a_far_end_that_does_not_say_it_has_the_goodbye_is_waited_for_a_moment() {
        let endpoint = endpoint().await;
        let (mut transport, _handle, link, dialer) = on_a_call(&endpoint, Like::Chrome).await;
        // Nobody has anything more to tell it of the session.
        drop(link);
        // It is gone with no word, and the line has not dropped yet:
        // nothing says that the goodbye arrived. The call is ended
        // after a moment all the same.
        dialer.drop_line();
        let took = closes(&mut transport).await;
        assert!(
            (Duration::from_millis(250)..Duration::from_secs(2)).contains(&took),
            "{took:?}"
        );
        assert_eq!(endpoint.calls(), 0);
    }

    #[tokio::test]
    async fn a_far_end_that_is_behind_as_its_call_ends_still_has_the_goodbye() {
        for tcp in [false, true] {
            let endpoint = endpoint_with_a_listener().await;
            let (mut transport, _handle, link, mut dialer) =
                on_a_call_over(&endpoint, Like::Chrome, tcp).await;
            drop(link);
            // It goes on sending what its microphone hears, and reads
            // nothing of what it is sent for longer than it is waited
            // for: a machine that has too much to do.
            let far = tokio::spawn(async move {
                let behind_until = now() + 2 * super::LAST_WORDS;
                while now() < behind_until {
                    dialer.audio(&packet(1)).unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                loop {
                    match dialer.next().await.unwrap() {
                        Some(Heard::Bye(reason)) => return Some(reason),
                        Some(_) => {}
                        None => return None,
                    }
                }
            });
            // It is waited for a moment, and no longer.
            let took = closes(&mut transport).await;
            assert!(
                (Duration::from_millis(250)..Duration::from_secs(2)).contains(&took),
                "{took:?}"
            );
            drop(transport);
            assert_eq!(endpoint.calls(), 0);
            // What it was told last is there when it comes to read it,
            // though the call is gone by then: a connection is not
            // closed under what was said on it.
            let told = tokio::time::timeout(SOON, far).await;
            assert_eq!(told.unwrap().unwrap(), Some(ByeReason::Ended), "{tcp}");
        }
    }

    #[tokio::test]
    async fn a_far_end_that_hangs_up_at_the_goodbye_is_not_waited_for() {
        let endpoint = endpoint().await;
        let (mut transport, _handle, link, mut dialer) = on_a_call(&endpoint, Like::Chrome).await;
        drop(link);
        // It hangs up as soon as it is told goodbye.
        let far = tokio::spawn(async move {
            loop {
                match dialer.next().await.unwrap() {
                    Some(Heard::Bye(reason)) => {
                        dialer.driven.core.close();
                        dialer.driven.flush();
                        return Some(reason);
                    }
                    Some(_) => {}
                    None => return None,
                }
            }
        });
        let took = closes(&mut transport).await;
        assert!(took < super::LAST_WORDS, "{took:?}");
        let reason = tokio::time::timeout(SOON, far).await.unwrap().unwrap();
        assert_eq!(reason, Some(ByeReason::Ended));
    }

    #[tokio::test]
    async fn a_client_that_says_goodbye_waits_a_moment_for_a_server_that_does_not_hear_it() {
        let endpoint = endpoint().await;
        let (mut transport, _handle, _link, dialer) = on_a_call(&endpoint, Like::Chrome).await;
        // Nobody runs the session, so nothing says that the goodbye
        // arrived. The client ends its call after a moment.
        let since = now();
        tokio::time::timeout(SOON, dialer.bye()).await.unwrap();
        let took = since.elapsed();
        assert!(
            (Duration::from_millis(250)..Duration::from_secs(2)).contains(&took),
            "{took:?}"
        );
        // The session finds that it left.
        assert_eq!(
            says(&mut transport, 1).await,
            [TransportEvent::Left { who: CALLER }]
        );

        // A server that hears it ends the call, and is not waited for.
        let (mut transport, _handle, _link, dialer) = on_a_call(&endpoint, Like::Chrome).await;
        let since = now();
        let ((), said) = meanwhile(&mut transport, dialer.bye()).await;
        let took = since.elapsed();
        assert!(took < Duration::from_millis(300), "{took:?}");
        assert!(
            said.is_empty() || said == [TransportEvent::Left { who: CALLER }],
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn a_call_whose_transport_is_let_go_of_is_ended_for_its_far_end() {
        let endpoint = endpoint().await;
        let (transport, handle, _link, mut dialer) = on_a_call(&endpoint, Like::Chrome).await;
        // The session is gone without a goodbye, as one whose thread
        // stopped is. The far end's call ends: it is not left to find
        // out from the silence.
        let since = now();
        drop(transport);
        let ended = async {
            loop {
                match dialer.next().await.unwrap() {
                    Some(Heard::Bye(reason)) => panic!("{reason:?}"),
                    Some(_) => {}
                    None => break,
                }
            }
        };
        tokio::time::timeout(SOON, ended).await.unwrap();
        assert!(
            since.elapsed() < Duration::from_secs(2),
            "{:?}",
            since.elapsed()
        );
        assert!(handle.is_over());
        assert_eq!(endpoint.calls(), 0);
    }

    #[tokio::test]
    async fn a_client_whose_call_ended_before_it_was_attached_takes_nobodys_place() {
        let endpoint = endpoint().await;
        let (mut transport, handle, _link, mut first) = on_a_call(&endpoint, Like::Chrome).await;
        // Another call, whose far end says hello and hangs up when it
        // is asked to.
        let local = "127.0.0.1:0".parse().unwrap();
        let (dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
        let answered = endpoint.offer(&offer, Client::V1).unwrap();
        let answer = answered.answer;
        let (hang_up, mut asked) = mpsc::unbounded_channel::<()>();
        let far = tokio::spawn(async move {
            let mut driven = dialing.driven;
            assert!(driven.core.answered(&answer));
            loop {
                let next = poll_fn(|cx| {
                    if asked.poll_recv(cx).is_ready() {
                        return Poll::Ready(None);
                    }
                    driven.poll_told(cx).map(Some)
                });
                match next.await {
                    Some(PeerHeard::Open) => assert!(driven.core.say(&Hello::new(None).said())),
                    Some(_) => {}
                    None => break,
                }
            }
            driven.core.close();
            driven.flush();
        });
        let caller = answered.knock.connect(SOON).await.unwrap();
        hang_up.send(()).unwrap();
        tokio::time::timeout(SOON, far).await.unwrap().unwrap();
        // What says so has arrived by the time it is attached.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let link = handle.attach(caller, welcome()).await.unwrap();
        // The one that is there stays, and the session is told
        // nothing: it hears it as before.
        first.audio(&packet(4)).unwrap();
        assert_eq!(hears_audio(&mut transport).await, packet(4));
        let (heard, said) = meanwhile(&mut transport, async {
            tokio::time::timeout(Duration::from_millis(300), first.next()).await
        })
        .await;
        assert!(heard.is_err(), "{heard:?}");
        assert_eq!(said, []);
        // Whoever would have told the one that came has nobody to
        // tell, and its call is let go of.
        assert!(link.tell(FromServer::Gap { resumes: 2 }).await.is_err());
        assert_eq!(endpoint.calls(), 1);
    }

    #[tokio::test]
    async fn a_client_that_comes_as_its_session_ends_is_told_that_it_is_over() {
        let endpoint = endpoint().await;
        let (mut transport, handle, _link, first) = on_a_call(&endpoint, Like::Chrome).await;
        // The client is back on another call, and is attached to the
        // transport of its session at the moment the session ends:
        // the transport is closed before it has looked at who came.
        first.drop_line();
        let (caller, again, _signals) = calls(&endpoint, Like::Chrome).await;
        let _link = handle.attach(caller, welcome()).await.unwrap();
        closes(&mut transport).await;
        // It is told why it is not served. It is not left on a call
        // that nobody looks at, and its call does not end with no
        // word, which would have it come back once more.
        let again = tokio::time::timeout(SOON, again).await.unwrap().unwrap();
        let Err(Unconnected::Refused { code, .. }) = again else {
            panic!("{again:?}");
        };
        assert_eq!(code, ErrorCode::NoSession);
        assert!(handle.is_over());
        assert_eq!(endpoint.calls(), 0);
    }
}
