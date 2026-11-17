//! The transport and its client, calling each other on this machine.
//!
//! Nothing here leaves the machine. The clock is the real one: the
//! sockets are real, and a clock that a test moves does not move them.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a helper that cannot go on has failed the test"
)]

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;
use core::time::Duration;

use bytes::Bytes;
use waveport_adapter_kit::client::{Hello, Welcome};
use waveport_domain::event::Key;
use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId};
use waveport_domain::time::MediaDur;
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::PortError;
use waveport_ports::transport::{FrameOut, KeysOut, Transport, TransportControl, TransportEvent};
use waveport_protocol::client::v1::{ByeReason, ErrorCode, Fact, FromClient, FromServer};
use waveport_testkit::conformance::{
    FarEnd, FarHeard, transport_hears, transport_is_left, transport_says_goodbye, transport_speaks,
};
use waveport_transport_webrtc::{
    BindError, Caller, Client, Config, Dialer, Endpoint, Gone, Handle, Heard, Like, Link,
    RtcTransport, Signals, Unanswered, Unconnected,
};

const CALLER: ParticipantId = ParticipantId(3);

/// How long anything here may take. It all happens on one machine.
const SOON: Duration = Duration::from_secs(10);

fn tag(generation: u32) -> OpTag {
    OpTag {
        generation: GenId(generation),
        epoch: Epoch(1),
    }
}

/// What a client is welcomed with to the session these tests have.
fn welcome(resumed: bool) -> Welcome {
    Welcome {
        session: "s-7".to_owned(),
        life: 1,
        resumed,
        resume_token: Some("wpr1_madeup".to_owned()),
        features: Vec::new(),
        next_seq: 1,
        grace_ms: Some(30_000),
    }
}

fn hello() -> Hello {
    Hello {
        token: Some("wpc1_madeup".to_owned()),
        features: vec!["said".to_owned()],
        ..Hello::new(None)
    }
}

/// A fact to tell a client.
fn fact(seq: u64, text: &str) -> FromServer {
    FromServer::Fact {
        seq,
        fact: Fact::TurnCommitted {
            turn: 1,
            who: CALLER.get(),
            text: text.to_owned(),
            addressed: true,
        },
    }
}

/// A packet of 20 ms that is told from others by `n`.
fn packet(n: u8) -> Bytes {
    Bytes::from(vec![0xF8, n, 7])
}

/// Sockets on free ports of this machine, with what carries their
/// datagrams running.
async fn endpoint() -> Endpoint {
    let local = "127.0.0.1:0".parse().unwrap();
    let config = Config {
        tcp: Some(local),
        ..Config::at(local)
    };
    let (endpoint, pump) = Endpoint::bind(config).await.unwrap();
    tokio::spawn(pump.run());
    endpoint
}

type Dialed = tokio::task::JoinHandle<Result<Dialer, Unconnected>>;

/// A client calls, as `like` does and over TCP or not, and the server
/// waits for it. Returns the far end that came, the client, which is
/// on the call once it is welcomed, and what the client says later
/// where it made its offer.
async fn calls(
    endpoint: &Endpoint,
    like: Like,
    tcp: bool,
    hello: Hello,
) -> (Caller, Dialed, Signals) {
    let local = "127.0.0.1:0".parse().unwrap();
    let (dialing, offer) = Dialer::offer(like, local, tcp).await.unwrap();
    let answered = endpoint.offer(&offer, Client::V1).unwrap();
    let answer = answered.answer;
    let welcomed = tokio::spawn(async move { dialing.answered(&answer, &hello, SOON).await });
    let caller = answered.knock.connect(SOON).await.unwrap();
    (caller, welcomed, answered.signals)
}

/// One side of a call.
struct Session {
    transport: RtcTransport,
    handle: Handle,
    /// What the client that is there is told through, while somebody
    /// tells it.
    link: Option<Link>,
    signals: Signals,
    endpoint: Endpoint,
    /// What the transport has said so far, but for what the far end
    /// lost.
    seen: Vec<TransportEvent>,
    /// What the far end said it lost.
    lost: Vec<u8>,
    /// Whether the transport has said that it is closed.
    closed: bool,
}

impl Session {
    fn link(&mut self) -> &mut Link {
        self.link.as_mut().unwrap()
    }

    fn look(&mut self, cx: &mut core::task::Context<'_>) {
        while !self.closed {
            match self.transport.poll_event(cx) {
                Poll::Ready(Some(TransportEvent::Loss { percent })) => self.lost.push(percent),
                Poll::Ready(Some(event)) => self.seen.push(event),
                Poll::Ready(None) => self.closed = true,
                Poll::Pending => break,
            }
        }
    }

    /// Does `work`, which is the client's, while the session looks at
    /// its transport as a running session does.
    async fn during<T>(&mut self, work: impl Future<Output = T>) -> T {
        let mut work = pin!(work);
        let both = poll_fn(|cx| {
            self.look(cx);
            work.as_mut().poll(cx)
        });
        tokio::time::timeout(SOON, both)
            .await
            .unwrap_or_else(|_| panic!("it took too long; the transport had said {:?}", self.seen))
    }

    /// Looks at the transport until it has said `events` things, or has
    /// closed.
    async fn until(&mut self, events: usize) {
        let enough = poll_fn(|cx| {
            self.look(cx);
            if self.closed || self.seen.len() >= events {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        let done = tokio::time::timeout(SOON, enough).await;
        assert!(done.is_ok(), "the transport said only {:?}", self.seen);
    }

    /// Sends 20 ms of a response.
    fn send(&mut self, tag: OpTag, at_ms: u32, n: u8, last: bool) {
        let frame = FrameOut {
            tag,
            at: MediaDur::from_millis(at_ms),
            length: MediaDur::from_millis(20),
            payload: &packet(n),
            last,
        };
        self.transport.try_send(frame).unwrap();
    }
}

/// A session and its client, on a call.
async fn call(like: Like, tcp: bool) -> (Session, Dialer) {
    let endpoint = endpoint().await;
    let (caller, welcomed, signals) = calls(&endpoint, like, tcp, hello()).await;
    // What it said as it came is there for whoever decides to take it.
    assert_eq!(caller.hello(), Some(&hello()));
    assert_eq!(caller.client(), Client::V1);
    let (transport, handle) = caller.transport(CALLER);
    assert!(!handle.is_over());
    let link = handle.attach(caller, welcome(false)).await.unwrap();
    let mut session = Session {
        transport,
        handle,
        link: Some(link),
        signals,
        endpoint,
        seen: Vec::new(),
        lost: Vec::new(),
        closed: false,
    };
    // The client is welcomed when the session first looks at its
    // transport.
    let dialer = session
        .during(async { welcomed.await.unwrap().unwrap() })
        .await;
    assert_eq!(dialer.session(), "s-7");
    assert_eq!(*dialer.welcome(), welcome(false));
    session.until(1).await;
    assert_eq!(session.seen, [TransportEvent::Joined { who: CALLER }]);
    (session, dialer)
}

/// The goodbye a client is told next, whatever it hears before it.
/// `None` for a call that ends with none.
async fn goodbye(dialer: &mut Dialer) -> Option<ByeReason> {
    loop {
        match dialer.next().await.unwrap() {
            Some(Heard::Bye(reason)) => return Some(reason),
            Some(_) => {}
            None => return None,
        }
    }
}

/// The next thing a client hears.
async fn hears(session: &mut Session, dialer: &mut Dialer) -> Heard {
    session
        .during(async { dialer.next().await.unwrap().unwrap() })
        .await
}

/// The client of this crate, as the far end that the checks of every
/// transport have act. It is being welcomed when a check begins: a
/// client is welcomed when the session first looks at its transport,
/// which is what a check does first.
struct Far {
    welcomed: Option<Dialed>,
    dialer: Option<Dialer>,
}

impl Far {
    async fn dialer(&mut self) -> &mut Dialer {
        if let Some(welcomed) = self.welcomed.take() {
            self.dialer = Some(welcomed.await.unwrap().unwrap());
        }
        // A client that has left is asked nothing more.
        self.dialer.as_mut().unwrap()
    }
}

impl FarEnd for Far {
    async fn speak(&mut self, n: u16) -> Result<Bytes, PortError> {
        let piece = packet(n.to_le_bytes()[0]);
        self.dialer().await.audio(&piece)?;
        Ok(piece)
    }

    async fn played(&mut self, run: u32, played: MediaDur) -> Result<(), PortError> {
        let samples = u32::try_from(Rate::Hz48000.samples_in(played)).unwrap();
        self.dialer().await.played(run, samples)
    }

    async fn write(&mut self, text: &str) -> Result<(), PortError> {
        self.dialer().await.text(text)
    }

    async fn press(&mut self, keys: &str) -> Result<bool, PortError> {
        self.dialer().await.dtmf(keys).await
    }

    async fn hear(&mut self) -> Result<Option<FarHeard>, PortError> {
        loop {
            let heard = match self.dialer().await.next().await? {
                None => return Ok(None),
                Some(Heard::Begins { run, .. }) => FarHeard::Begins(run),
                Some(Heard::Audio(audio)) => FarHeard::Audio(audio),
                Some(Heard::Ends(run)) => FarHeard::Ends(run),
                Some(Heard::Clear(run)) => FarHeard::Clear(run),
                Some(Heard::Bye(_)) => FarHeard::Bye,
                Some(_) => continue,
            };
            return Ok(Some(heard));
        }
    }

    async fn leave(&mut self) -> Result<(), PortError> {
        self.dialer().await;
        if let Some(dialer) = self.dialer.take() {
            dialer.bye().await;
        }
        Ok(())
    }
}

/// A transport that nobody has looked at yet, and its far end. What
/// attaches a client to it is kept for as long as the transport is: a
/// client that drops is waited for.
async fn unseen(like: Like, tcp: bool) -> (RtcTransport, Far, Handle, Link, Endpoint) {
    let endpoint = endpoint().await;
    let (caller, welcomed, _signals) = calls(&endpoint, like, tcp, hello()).await;
    let (transport, handle) = caller.transport(CALLER);
    let link = handle.attach(caller, welcome(false)).await.unwrap();
    let far = Far {
        welcomed: Some(welcomed),
        dialer: None,
    };
    (transport, far, handle, link, endpoint)
}

#[tokio::test]
async fn the_transport_keeps_the_rules_of_every_transport() {
    // As each browser offers, over datagrams and over a connection.
    for (like, tcp) in [
        (Like::Chrome, false),
        (Like::Firefox, false),
        (Like::Chrome, true),
    ] {
        let (mut transport, mut far, _handle, _link, _endpoint) = unseen(like, tcp).await;
        let hears = transport_hears(&mut transport, &mut far, CALLER).await;
        assert_eq!(hears, Ok(()), "{like:?} {tcp}");

        let (mut transport, mut far, _handle, _link, _endpoint) = unseen(like, tcp).await;
        let speaks = transport_speaks(&mut transport, &mut far, CALLER).await;
        assert_eq!(speaks, Ok(()), "{like:?} {tcp}");

        let (mut transport, mut far, _handle, _link, _endpoint) = unseen(like, tcp).await;
        let goodbye = transport_says_goodbye(&mut transport, &mut far, CALLER).await;
        assert_eq!(goodbye, Ok(()), "{like:?} {tcp}");

        let (mut transport, mut far, _handle, _link, _endpoint) = unseen(like, tcp).await;
        let left = transport_is_left(&mut transport, &mut far, CALLER).await;
        assert_eq!(left, Ok(()), "{like:?} {tcp}");
    }
}

#[tokio::test]
async fn a_transport_says_what_its_far_end_can_do() {
    let (session, _dialer) = call(Like::Chrome, false).await;
    let caps = session.transport.caps();
    let audio = caps.audio.unwrap();
    assert_eq!((audio.inbound, audio.outbound), (Codec::Opus, Codec::Opus));
    // A client that speaks the protocol says what it played, and
    // discards what it is told to.
    assert!(audio.remote_buffer.clearable && audio.remote_buffer.reports_played);
    assert!(caps.text);
    // Keys are pressed at it as keys.
    assert_eq!(caps.keys, Some(KeysOut::OutOfBand));
    assert_eq!(session.endpoint.calls(), 1);
}

#[tokio::test]
async fn a_call_carries_a_conversation_over_datagrams_and_over_a_connection() {
    for tcp in [false, true] {
        let (mut session, mut dialer) = call(Like::Chrome, tcp).await;
        // What the far end says, and types, and presses.
        for n in 0..10 {
            session
                .during(async { dialer.audio(&packet(n)).unwrap() })
                .await;
        }
        session
            .during(async { dialer.text("What time is it?").unwrap() })
            .await;
        session.until(12).await;
        let mut due = None;
        for (n, event) in session.seen[1..11].iter().enumerate() {
            let TransportEvent::Audio {
                who,
                sequence,
                timestamp,
                payload,
            } = event
            else {
                panic!("{event:?}");
            };
            assert_eq!(*who, CALLER);
            assert_eq!(*payload, packet(u8::try_from(n).unwrap()));
            if let Some(due) = due {
                assert_eq!((*sequence, *timestamp), due);
            }
            due = Some((sequence.wrapping_add(1), timestamp.wrapping_add(960)));
        }
        let typed = TransportEvent::Text {
            who: CALLER,
            text: "What time is it?".to_owned(),
        };
        assert_eq!(session.seen[11], typed);
        assert!(session.during(dialer.dtmf("42")).await.unwrap());
        session.until(14).await;
        let pressed = |key| TransportEvent::Dtmf { who: CALLER, key };
        assert_eq!(session.seen[12..], [pressed(Key::Four), pressed(Key::Two)]);

        // What the session says, what it tells its client, and the key
        // it presses.
        session.send(tag(1), 0, 1, false);
        session.send(tag(1), 20, 2, true);
        session
            .transport
            .try_control(TransportControl::Dtmf { key: Key::Nine })
            .unwrap();
        session
            .link()
            .tell(fact(1, "What time is it?"))
            .await
            .unwrap();
        let mut heard = Vec::new();
        while heard.len() < 6 {
            heard.push(hears(&mut session, &mut dialer).await);
        }
        // The key and the fact travel beside the audio, and come when
        // they come.
        let of_the_run: Vec<&Heard> = heard
            .iter()
            .filter(|heard| !matches!(heard, Heard::Key(_) | Heard::Fact { .. }))
            .collect();
        let Heard::Begins { run, generation: 1 } = of_the_run[0] else {
            panic!("{heard:?}");
        };
        assert_eq!(
            of_the_run[1..],
            [
                &Heard::Audio(packet(1)),
                &Heard::Audio(packet(2)),
                &Heard::Ends(*run)
            ]
        );
        assert!(heard.contains(&Heard::Key(Key::Nine)), "{heard:?}");
        assert!(
            heard
                .iter()
                .any(|heard| matches!(heard, Heard::Fact { seq: 1, .. }))
        );

        // What it played of that is told of the response.
        session.seen.clear();
        session
            .during(async { dialer.played(*run, 960).unwrap() })
            .await;
        session.until(1).await;
        let played = TransportEvent::Played {
            tag: tag(1),
            played: MediaDur::from_millis(20),
        };
        assert_eq!(session.seen, [played], "{tcp}");

        asks_and_is_told_goodbye(&mut session, &mut dialer).await;
    }
}

/// The client asks that the agent stop, which is for whoever runs the
/// session to do, and the session ends: the client is told goodbye once
/// whoever told it of the session is done.
async fn asks_and_is_told_goodbye(session: &mut Session, dialer: &mut Dialer) {
    session.during(async { dialer.interrupt().unwrap() }).await;
    let mut link = session.link.take().unwrap();
    let asked = session.during(link.asked()).await;
    assert_eq!(asked, Some(FromClient::Interrupt));
    drop(link);
    let closed = poll_fn(|cx| session.transport.poll_close(cx));
    let bye = goodbye(dialer);
    let (reason, ()) = tokio::time::timeout(SOON, async { tokio::join!(bye, closed) })
        .await
        .unwrap();
    assert_eq!(reason, Some(ByeReason::Ended));
    assert!(session.handle.is_over());
    assert_eq!(session.endpoint.calls(), 0);
}

#[tokio::test]
async fn a_client_that_is_somewhere_else_offers_again_where_it_made_its_offer() {
    let (mut session, mut dialer) = call(Like::Chrome, false).await;
    session
        .during(async { dialer.audio(&packet(0)).unwrap() })
        .await;
    session.until(2).await;
    // It has another socket, and what it sends from there finds the
    // call by the name the answer gives it.
    let offer = dialer.moves().await.unwrap();
    let signals = session.signals.clone();
    let answer = session
        .during(async { signals.offer(&offer).await })
        .await
        .unwrap();
    assert!(dialer.moved(&answer));
    // An offer that is none leaves the call as it is.
    let refused = session.during(async { signals.offer("v=0").await }).await;
    assert_eq!(refused, Err(Unanswered::Unreadable));
    // Its checks go out from where it is now and are answered, which
    // takes a moment. What is sent to it meanwhile is lost, as on any
    // line that dropped.
    let settles = async {
        let checks = async { while let Ok(Some(_)) = dialer.next().await {} };
        let _ = tokio::time::timeout(Duration::from_millis(500), checks).await;
    };
    session.during(settles).await;
    // The call is the one it was: the same session, with what follows
    // numbered on from where it was.
    for n in 1..4 {
        session
            .during(async { dialer.audio(&packet(n)).unwrap() })
            .await;
    }
    session.send(tag(1), 0, 9, true);
    let mut heard = Vec::new();
    while heard.len() < 3 {
        match hears(&mut session, &mut dialer).await {
            Heard::Down | Heard::Up => {}
            other => heard.push(other),
        }
    }
    assert!(matches!(
        heard[..],
        [Heard::Begins { .. }, Heard::Audio(_), Heard::Ends(_)]
    ));
    session.until(5).await;
    let audio: Vec<(u16, Bytes)> = session
        .seen
        .iter()
        .filter_map(|event| match event {
            TransportEvent::Audio {
                sequence, payload, ..
            } => Some((*sequence, payload.clone())),
            _ => None,
        })
        .collect();
    let first = audio[0].0;
    for (n, (sequence, payload)) in audio.iter().enumerate() {
        let n = u8::try_from(n).unwrap();
        assert_eq!(*payload, packet(n));
        // What was not said while it moved leaves a gap in time, and
        // none in what was said.
        assert!(sequence.wrapping_sub(first) >= u16::from(n), "{audio:?}");
    }
    assert_eq!(audio.len(), 4);

    // It hangs up where it made its offer: it has left.
    session.signals.end().await;
    session.until(usize::MAX).await;
    assert_eq!(
        session.seen.last(),
        Some(&TransportEvent::Left { who: CALLER })
    );
    assert!(session.closed);
    assert_eq!(session.signals.offer("v=0").await, Err(Unanswered::Over));
}

#[tokio::test]
async fn a_client_that_comes_back_on_another_call_takes_the_place_of_the_one_before() {
    let (mut session, mut dialer) = call(Like::Chrome, false).await;
    session.send(tag(1), 0, 1, false);
    let begins = hears(&mut session, &mut dialer).await;
    let Heard::Begins { run: first, .. } = begins else {
        panic!("{begins:?}");
    };
    assert_eq!(
        hears(&mut session, &mut dialer).await,
        Heard::Audio(packet(1))
    );

    // The same client calls again, as one does whose first call is of
    // no use to it any more.
    let resumes = Hello {
        token: None,
        ..hello()
    };
    let (caller, welcomed, _signals) = calls(&session.endpoint, Like::Chrome, false, resumes).await;
    assert_eq!(session.endpoint.calls(), 2);
    let link = session.handle.attach(caller, welcome(true)).await.unwrap();
    session.link = Some(link);
    let mut again = session
        .during(async { welcomed.await.unwrap().unwrap() })
        .await;
    assert!(again.welcome().resumed);
    // The session is told that its line dropped and is back, and the
    // one before that another took its place.
    session.until(3).await;
    assert_eq!(
        session.seen[1..],
        [TransportEvent::Detached, TransportEvent::Attached]
    );
    let replaced = session.during(goodbye(&mut dialer)).await;
    assert_eq!(replaced, Some(ByeReason::Replaced));
    assert_eq!(session.endpoint.calls(), 1);

    // What follows of the response is said again to be of it, to the
    // one that is there now.
    session.send(tag(1), 20, 2, true);
    let begins = hears(&mut session, &mut again).await;
    assert_eq!(
        begins,
        Heard::Begins {
            run: first,
            generation: 1
        }
    );
    assert_eq!(
        hears(&mut session, &mut again).await,
        Heard::Audio(packet(2))
    );
    assert_eq!(hears(&mut session, &mut again).await, Heard::Ends(first));

    // A server that is going away lets go of the client and keeps the
    // session: the client is told, and the session that its line
    // dropped.
    let going = FromServer::Bye {
        reason: ByeReason::GoingAway,
    };
    session.link().tell(going).await.unwrap();
    let told = session.during(goodbye(&mut again)).await;
    assert_eq!(told, Some(ByeReason::GoingAway));
    session.until(4).await;
    assert_eq!(session.seen[3], TransportEvent::Detached);
    assert!(!session.closed && !session.handle.is_over());
    let gone = session.link().tell(fact(2, "nobody hears this")).await;
    assert_eq!(gone, Err(Gone));
}

#[tokio::test]
async fn a_client_that_is_not_served_is_told_why() {
    let endpoint = endpoint().await;
    // One that the server will not have.
    let (caller, welcomed, _signals) = calls(&endpoint, Like::Chrome, false, hello()).await;
    caller
        .refuse(ErrorCode::Token, "the token is not one of ours")
        .await;
    let Err(Unconnected::Refused { code, message }) = welcomed.await.unwrap() else {
        panic!("it was served");
    };
    assert_eq!(code, ErrorCode::Token);
    assert_eq!(message, "the token is not one of ours");
    assert_eq!(endpoint.calls(), 0);

    // One that comes to a session that is over.
    let (caller, _dialed, _signals) = calls(&endpoint, Like::Chrome, false, hello()).await;
    let (transport, handle) = caller.transport(CALLER);
    drop(transport);
    let unattached = handle.attach(caller, welcome(false)).await.unwrap_err();
    assert_eq!(unattached.code(), ErrorCode::NoSession);
    assert_eq!(unattached.why(), "the session is over");
    unattached.refuse().await;

    // An offer that is none, of which nothing is said back.
    let refused = endpoint.offer("wpc1_madeup", Client::V1).unwrap_err();
    assert_eq!(refused, Unanswered::NoAudio);
    // And a client whose answer is not one.
    let local = "127.0.0.1:0".parse().unwrap();
    let (dialing, _offer) = Dialer::offer(Like::Firefox, local, false).await.unwrap();
    let failed = dialing.answered("v=0", &hello(), SOON).await;
    assert!(matches!(failed, Err(Unconnected::Failed(_))));
}

#[tokio::test]
async fn a_far_end_that_never_comes_is_not_waited_for() {
    let endpoint = endpoint().await;
    let local = "127.0.0.1:0".parse().unwrap();
    let (_dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
    let answered = endpoint.offer(&offer, Client::V1).unwrap();
    assert_eq!(endpoint.calls(), 1);
    let never = answered.knock.connect(Duration::from_millis(200)).await;
    assert!(
        never
            .unwrap_err()
            .to_string()
            .contains("did not come in time")
    );
    assert_eq!(endpoint.calls(), 0);
    assert!(answered.signals.is_over());
}

#[tokio::test]
async fn sockets_say_where_they_are_reached() {
    let anywhere = "0.0.0.0:0".parse().unwrap();
    let nowhere = Endpoint::bind(Config::at(anywhere)).await;
    assert!(matches!(nowhere, Err(BindError::Nowhere)));
    let config = Config {
        tcp: Some(anywhere),
        reached_at: Some("192.0.2.10".parse().unwrap()),
        ..Config::at(anywhere)
    };
    let (endpoint, _pump) = Endpoint::bind(config).await.unwrap();
    assert_eq!(endpoint.udp().ip().to_string(), "192.0.2.10");
    assert_eq!(endpoint.tcp().unwrap().ip().to_string(), "192.0.2.10");
    assert!(endpoint.udp().port() != 0 && endpoint.tcp().unwrap().port() != 0);
    let (endpoint, _pump) = Endpoint::bind(Config::at("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    assert_eq!(endpoint.tcp(), None);
}
