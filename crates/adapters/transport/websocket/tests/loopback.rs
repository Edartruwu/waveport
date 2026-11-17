//! The transport and its client, talking to each other on this machine.
//!
//! Nothing here leaves the machine. The clock is the real one: the
//! connections are real, and a clock that a test moves does not move
//! them.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a helper that cannot go on has failed the test"
)]

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;
use core::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use waveport_adapter_kit::ws::{WsConn, WsMessage, WsRequest};
use waveport_domain::event::Key;
use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId};
use waveport_domain::time::MediaDur;
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::{PortError, RetryClass};
use waveport_ports::transport::{FrameOut, Transport, TransportControl, TransportEvent};
use waveport_protocol::client::v1::{
    self as client, AudioFormat, ByeReason, ErrorCode, Fact, FromClient, FromServer, News, Resume,
};
use waveport_testkit::conformance::{
    FarEnd, FarHeard, transport_hears, transport_is_left, transport_says_goodbye, transport_speaks,
};
use waveport_transport_websocket::{
    Caller, Client, GREETING_TIMEOUT, Gone, Handle, Heard, Hello, Link, ListenError, Listener,
    PATH, PROTOCOL, Unconnected, Welcome, WsTransport,
};

const CALLER: ParticipantId = ParticipantId(3);

/// How long anything here may take. It all happens on one machine.
const SOON: Duration = Duration::from_secs(5);

#[expect(
    clippy::unnecessary_wraps,
    reason = "as a hello has it: a client may name no audio"
)]
const fn pcm(rate: u32) -> Option<AudioFormat> {
    Some(AudioFormat {
        codec: client::Codec::Pcm16,
        rate,
    })
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "as a hello has it: a client may name no audio"
)]
const fn opus(rate: u32) -> Option<AudioFormat> {
    Some(AudioFormat {
        codec: client::Codec::Opus,
        rate,
    })
}

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

/// A listener on a free port of this machine, and the address a client
/// reaches it at.
async fn listener() -> (Listener, String) {
    let listener = Listener::bind("127.0.0.1:0".parse().unwrap(), false)
        .await
        .unwrap();
    let address = format!("ws://{}", listener.address().unwrap());
    (listener, address)
}

/// One side of a call.
struct Session {
    transport: WsTransport,
    /// What a client is attached with, while somebody holds it.
    handle: Option<Handle>,
    /// What the client that is there is told through, while somebody
    /// tells it.
    link: Option<Link>,
    listener: Listener,
    address: String,
    /// What the transport has said so far.
    seen: Vec<TransportEvent>,
    /// Whether the transport has said that it is closed.
    closed: bool,
}

impl Session {
    fn handle(&self) -> &Handle {
        self.handle.as_ref().unwrap()
    }

    fn link(&mut self) -> &mut Link {
        self.link.as_mut().unwrap()
    }

    /// Does `work`, which is the client's, while the session looks at its
    /// transport as a running session does.
    async fn during<T>(&mut self, work: impl Future<Output = T>) -> T {
        let mut work = pin!(work);
        let both = poll_fn(|cx| {
            while !self.closed {
                match self.transport.poll_event(cx) {
                    Poll::Ready(Some(event)) => self.seen.push(event),
                    Poll::Ready(None) => self.closed = true,
                    Poll::Pending => break,
                }
            }
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
            while !self.closed && self.seen.len() < events {
                match self.transport.poll_event(cx) {
                    Poll::Ready(Some(event)) => self.seen.push(event),
                    Poll::Ready(None) => self.closed = true,
                    Poll::Pending => return Poll::Pending,
                }
            }
            Poll::Ready(())
        });
        let done = tokio::time::timeout(SOON, enough).await;
        assert!(done.is_ok(), "the transport said only {:?}", self.seen);
    }

    /// Sends 20 ms of a response, every byte of it `fill`: 960 bytes,
    /// which is that long as samples at 24 kHz.
    fn send(&mut self, tag: OpTag, at_ms: u32, fill: u8, last: bool) {
        let frame = FrameOut {
            tag,
            at: MediaDur::from_millis(at_ms),
            length: MediaDur::from_millis(20),
            payload: &[fill; 960],
            last,
        };
        self.transport.try_send(frame).unwrap();
    }

    /// Another client connects with `hello`, and is heard out.
    async fn comes(&mut self, hello: Hello) -> (Caller, tokio::task::JoinHandle<Connected>) {
        let address = self.address.clone();
        let connecting = tokio::spawn(async move { Client::connect(&address, &hello, SOON).await });
        let knock = self.listener.accept().await.unwrap();
        let caller = knock.greet(GREETING_TIMEOUT).await.unwrap();
        (caller, connecting)
    }

    /// Another client connects with `hello` and is attached in place of
    /// whoever was there, if anybody was.
    async fn comes_back(&mut self, hello: Hello) -> Client {
        let (caller, connecting) = self.comes(hello).await;
        self.link = Some(self.handle().attach(caller, welcome(true)).await.unwrap());
        self.during(async { connecting.await.unwrap().unwrap() })
            .await
    }
}

type Connected = Result<Client, Unconnected>;

/// A session and its client, connected, with `audio`.
async fn call(audio: Option<AudioFormat>) -> (Session, Client) {
    let (listener, address) = listener().await;
    let hello = Hello {
        token: Some("wpc1_madeup".to_owned()),
        features: vec!["said".to_owned()],
        ..Hello::new(audio)
    };
    let connecting = {
        let (address, hello) = (address.clone(), hello.clone());
        tokio::spawn(async move { Client::connect(&address, &hello, SOON).await.unwrap() })
    };
    let caller = listener
        .accept()
        .await
        .unwrap()
        .greet(GREETING_TIMEOUT)
        .await
        .unwrap();
    // What it said as it came is there for whoever decides to take it.
    assert_eq!(*caller.hello(), hello);
    assert!(caller.from().unwrap().ip().is_loopback());
    let (transport, handle) = caller.transport(CALLER);
    assert_eq!(handle.audio(), audio);
    assert!(!handle.is_over());
    let link = handle.attach(caller, welcome(false)).await.unwrap();
    let mut session = Session {
        transport,
        handle: Some(handle),
        link: Some(link),
        listener,
        address,
        seen: Vec::new(),
        closed: false,
    };
    // The client is answered when the session first looks at its
    // transport.
    let client = session.during(async { connecting.await.unwrap() }).await;
    assert_eq!(client.session(), "s-7");
    assert_eq!(*client.welcome(), welcome(false));
    session.until(1).await;
    assert_eq!(session.seen, [TransportEvent::Joined { who: CALLER }]);
    (session, client)
}

/// The client of this crate, as the far end that the checks of every
/// transport have act. It is connecting when a check begins: a client
/// is welcomed when the session first looks at its transport, which is
/// what a check does first.
struct Far {
    connecting: Option<tokio::task::JoinHandle<Client>>,
    client: Option<Client>,
    audio: Option<AudioFormat>,
}

impl Far {
    async fn client(&mut self) -> &mut Client {
        if let Some(connecting) = self.connecting.take() {
            self.client = Some(connecting.await.unwrap());
        }
        // A client that has left is asked nothing more.
        self.client.as_mut().unwrap()
    }
}

impl FarEnd for Far {
    async fn speak(&mut self, n: u16) -> Result<Bytes, PortError> {
        let piece: Bytes = match self.audio {
            Some(AudioFormat {
                codec: client::Codec::Pcm16,
                rate,
            }) => {
                // 20 ms, every sample of it its own number.
                let samples = u16::try_from(rate / 50).unwrap();
                let samples = (0..samples).flat_map(|at| (n * samples + at).to_le_bytes());
                samples.collect::<Vec<u8>>().into()
            }
            // A packet of 20 ms, as its first byte says.
            Some(_) => vec![0xF8, n.to_le_bytes()[0], 7].into(),
            None => Bytes::new(),
        };
        self.client().await.audio(piece.clone()).await?;
        Ok(piece)
    }

    async fn played(&mut self, run: u32, played: MediaDur) -> Result<(), PortError> {
        let rate = self.audio.and_then(|audio| Rate::from_hz(audio.rate));
        let samples = rate.map_or(0, |rate| rate.samples_in(played));
        let samples = u32::try_from(samples).unwrap();
        self.client().await.played(run, samples).await
    }

    async fn write(&mut self, text: &str) -> Result<(), PortError> {
        self.client().await.text(text).await
    }

    async fn press(&mut self, keys: &str) -> Result<bool, PortError> {
        self.client().await.dtmf(keys).await?;
        Ok(true)
    }

    async fn hear(&mut self) -> Result<Option<FarHeard>, PortError> {
        loop {
            let heard = match self.client().await.next().await? {
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
        self.client().await;
        if let Some(client) = self.client.take() {
            client.bye().await;
        }
        Ok(())
    }
}

/// A transport that nobody has looked at yet, and its far end, with
/// `audio`. What attaches a client to it is kept for as long as the
/// transport is: a client that drops is waited for.
async fn unseen(audio: Option<AudioFormat>) -> (WsTransport, Far, Handle, Link) {
    let (listener, address) = listener().await;
    let hello = Hello::new(audio);
    let connecting =
        tokio::spawn(async move { Client::connect(&address, &hello, SOON).await.unwrap() });
    let knock = listener.accept().await.unwrap();
    let caller = knock.greet(GREETING_TIMEOUT).await.unwrap();
    let (transport, handle) = caller.transport(CALLER);
    let link = handle.attach(caller, welcome(false)).await.unwrap();
    let far = Far {
        connecting: Some(connecting),
        client: None,
        audio,
    };
    (transport, far, handle, link)
}

#[tokio::test]
async fn the_transport_keeps_the_rules_of_every_transport() {
    // With plain samples at two rates, with Opus, and on a line of text,
    // which carries what is typed and the keys and nothing else.
    for audio in [pcm(16_000), pcm(48_000), opus(24_000), None] {
        let (mut transport, mut far, _handle, _link) = unseen(audio).await;
        let hears = transport_hears(&mut transport, &mut far, CALLER).await;
        assert_eq!(hears, Ok(()), "{audio:?}");

        let (mut transport, mut far, _handle, _link) = unseen(audio).await;
        let speaks = transport_speaks(&mut transport, &mut far, CALLER).await;
        assert_eq!(speaks, Ok(()), "{audio:?}");

        let (mut transport, mut far, _handle, _link) = unseen(audio).await;
        let goodbye = transport_says_goodbye(&mut transport, &mut far, CALLER).await;
        assert_eq!(goodbye, Ok(()), "{audio:?}");

        let (mut transport, mut far, _handle, _link) = unseen(audio).await;
        let left = transport_is_left(&mut transport, &mut far, CALLER).await;
        assert_eq!(left, Ok(()), "{audio:?}");
    }
}

/// The next thing a client hears.
async fn hears(session: &mut Session, client: &mut Client) -> Heard {
    session
        .during(async { client.next().await.unwrap().unwrap() })
        .await
}

#[tokio::test]
async fn a_transport_says_what_its_client_asked_for() {
    let (session, _client) = call(pcm(16_000)).await;
    let caps = session.transport.caps();
    let audio = caps.audio.unwrap();
    assert_eq!(audio.inbound, Codec::Pcm(Rate::Hz16000));
    assert_eq!(audio.outbound, Codec::Pcm(Rate::Hz16000));
    assert!(audio.remote_buffer.clearable && audio.remote_buffer.reports_played);
    assert!(caps.text);
    assert_eq!(caps.keys, None);

    let (session, _client) = call(opus(48_000)).await;
    let audio = session.transport.caps().audio.unwrap();
    assert_eq!((audio.inbound, audio.outbound), (Codec::Opus, Codec::Opus));
    assert!(audio.remote_buffer.clearable && audio.remote_buffer.reports_played);

    // A client that names no audio is on a line of text.
    let (session, _client) = call(None).await;
    let caps = session.transport.caps();
    assert_eq!(caps.audio, None);
    assert!(caps.text);
}

#[tokio::test]
async fn what_the_microphone_heard_arrives_in_packets() {
    let (mut session, mut client) = call(pcm(16_000)).await;
    // Three tenths of a second, in pieces that are not packets: every
    // sample is its own number.
    let samples: Vec<u8> = (0..4_800_u16).flat_map(u16::to_le_bytes).collect();
    for piece in samples.chunks(2_222) {
        let piece = Bytes::copy_from_slice(piece);
        session
            .during(async { client.audio(piece).await.unwrap() })
            .await;
    }
    session.until(16).await;
    let mut heard = Vec::new();
    for (number, event) in session.seen[1..].iter().enumerate() {
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
        assert_eq!(usize::from(*sequence), number);
        assert_eq!(*timestamp as usize, number * 320);
        assert_eq!(payload.len(), 640);
        heard.extend_from_slice(payload);
    }
    assert_eq!(heard, samples);
}

#[tokio::test]
async fn opus_arrives_a_packet_a_message_and_is_placed_by_what_it_holds() {
    let (mut session, mut client) = call(opus(24_000)).await;
    // 20 ms, 10 ms and 20 ms, as the first byte of each says. What
    // follows that byte is the packet's own business.
    let packets: [&[u8]; 3] = [&[0xF8, 1, 2, 3], &[0xF0, 4], &[0xF8, 5, 6]];
    session
        .during(async {
            for packet in packets {
                client.audio(Bytes::from_static(packet)).await.unwrap();
            }
            // What is no packet is refused, and counts for nothing.
            client.audio(Bytes::from_static(&[0xFB])).await.unwrap();
            client.audio(Bytes::from_static(&[0xF8, 7])).await.unwrap();
        })
        .await;
    session.until(5).await;
    let audio = |sequence, timestamp, payload: &'static [u8]| TransportEvent::Audio {
        who: CALLER,
        sequence,
        timestamp,
        payload: Bytes::from_static(payload),
    };
    // Its place is counted at 48 kHz, whatever it is played at.
    assert_eq!(
        session.seen[1..],
        [
            audio(0, 0, packets[0]),
            audio(1, 960, packets[1]),
            audio(2, 1_440, packets[2]),
            audio(3, 2_400, &[0xF8, 7]),
        ]
    );
    let Heard::Error { code, message } = hears(&mut session, &mut client).await else {
        panic!("no refusal");
    };
    assert_eq!(code, ErrorCode::Malformed);
    assert!(message.contains("one Opus packet"), "{message}");
}

#[tokio::test]
async fn a_response_is_marked_and_what_was_played_of_it_is_told() {
    let (mut session, mut client) = call(pcm(24_000)).await;
    session.send(tag(7), 0, 1, false);
    session.send(tag(7), 20, 2, false);
    session.send(tag(7), 40, 3, true);
    let begins = Heard::Begins {
        run: 0,
        generation: 7,
    };
    assert_eq!(hears(&mut session, &mut client).await, begins);
    for fill in 1..=3 {
        let Heard::Audio(samples) = hears(&mut session, &mut client).await else {
            panic!("no audio");
        };
        assert_eq!(samples[..], [fill; 960]);
    }
    assert_eq!(hears(&mut session, &mut client).await, Heard::Ends(0));

    // The client says how far it has got: a frame, then more than it was
    // ever sent, then something of a response that never was.
    session
        .during(async {
            client.played(0, 480).await.unwrap();
            client.played(0, 99_999).await.unwrap();
            client.played(5, 10).await.unwrap();
            client.text("and so on").await.unwrap();
            client.dtmf("1#A").await.unwrap();
        })
        .await;
    session.until(7).await;
    let key = |key| TransportEvent::Dtmf { who: CALLER, key };
    assert_eq!(
        session.seen[1..],
        [
            TransportEvent::Played {
                tag: tag(7),
                played: MediaDur::from_millis(20),
            },
            // No more can have been played than was sent.
            TransportEvent::Played {
                tag: tag(7),
                played: MediaDur::from_millis(60),
            },
            TransportEvent::Text {
                who: CALLER,
                text: "and so on".to_owned(),
            },
            // Keys arrive one by one, in the order they were pressed.
            key(Key::One),
            key(Key::Hash),
            key(Key::A),
        ]
    );
}

#[tokio::test]
async fn what_was_played_of_opus_is_counted_at_the_rate_it_is_played_at() {
    let (mut session, mut client) = call(opus(48_000)).await;
    // Three packets of 20 ms: their bytes say nothing of their length.
    for (at, last) in [(0, false), (20, false), (40, true)] {
        let frame = FrameOut {
            tag: tag(4),
            at: MediaDur::from_millis(at),
            length: MediaDur::from_millis(20),
            payload: &[0xF8, 1, 2],
            last,
        };
        session.transport.try_send(frame).unwrap();
    }
    let begins = Heard::Begins {
        run: 0,
        generation: 4,
    };
    assert_eq!(hears(&mut session, &mut client).await, begins);
    for _ in 0..3 {
        // A packet a message, as it was made.
        let packet = Heard::Audio(Bytes::from_static(&[0xF8, 1, 2]));
        assert_eq!(hears(&mut session, &mut client).await, packet);
    }
    assert_eq!(hears(&mut session, &mut client).await, Heard::Ends(0));
    session
        .during(async {
            client.played(0, 1_920).await.unwrap();
            client.played(0, 99_999).await.unwrap();
        })
        .await;
    session.until(3).await;
    let played = |ms| TransportEvent::Played {
        tag: tag(4),
        played: MediaDur::from_millis(ms),
    };
    assert_eq!(session.seen[1..], [played(40), played(60)]);
}

#[tokio::test]
async fn a_response_that_is_discarded_is_said_to_be() {
    let (mut session, mut client) = call(pcm(24_000)).await;
    // Nothing was sent of this one: there is nothing to discard.
    session
        .transport
        .try_control(TransportControl::Clear { tag: tag(1) })
        .unwrap();
    session.send(tag(2), 0, 1, false);
    session
        .transport
        .try_control(TransportControl::Clear { tag: tag(2) })
        .unwrap();
    // The next response begins with a mark of its own.
    session.send(tag(3), 0, 2, true);
    let mut heard = Vec::new();
    for _ in 0..6 {
        heard.push(hears(&mut session, &mut client).await);
    }
    let begins = |run, generation| Heard::Begins { run, generation };
    assert_eq!(
        heard,
        [
            begins(0, 2),
            Heard::Audio(Bytes::from_static(&[1; 960])),
            Heard::Clear(0),
            begins(1, 3),
            Heard::Audio(Bytes::from_static(&[2; 960])),
            Heard::Ends(1),
        ]
    );
}

#[tokio::test]
async fn a_line_of_text_is_sent_no_audio_and_takes_none() {
    let (mut session, mut client) = call(None).await;
    // A session of text sends none. One that did would not be refused.
    session.send(tag(1), 0, 1, true);
    session
        .during(async {
            client.audio(Bytes::from_static(&[0; 640])).await.unwrap();
            client.text("typed").await.unwrap();
        })
        .await;
    session.until(2).await;
    let typed = TransportEvent::Text {
        who: CALLER,
        text: "typed".to_owned(),
    };
    assert_eq!(session.seen[1..], [typed]);
    let Heard::Error { code, message } = hears(&mut session, &mut client).await else {
        panic!("no refusal");
    };
    assert_eq!(code, ErrorCode::Unsupported);
    assert!(message.contains("carries no audio"), "{message}");
    // And nothing else is on its way: what is told next is next.
    session.link().tell(fact(1, "typed")).await.unwrap();
    assert_eq!(
        FromServer::Fact {
            seq: 1,
            fact: Fact::TurnCommitted {
                turn: 1,
                who: 3,
                text: "typed".to_owned(),
                addressed: true,
            },
        },
        fact(1, "typed")
    );
    let Heard::Fact { seq: 1, .. } = hears(&mut session, &mut client).await else {
        panic!("no fact");
    };
}

#[tokio::test]
async fn the_client_is_told_of_the_session_in_order_with_its_audio() {
    let (mut session, mut client) = call(pcm(48_000)).await;
    let said = News::Said {
        generation: 1,
        text: "Ten past three.".to_owned(),
    };
    session
        .link()
        .tell(fact(4, "What time is it?"))
        .await
        .unwrap();
    session
        .link()
        .tell(FromServer::Told(said.clone()))
        .await
        .unwrap();
    let Heard::Fact { seq, fact } = hears(&mut session, &mut client).await else {
        panic!("no fact");
    };
    assert_eq!(seq, 4);
    assert!(
        matches!(&fact, Fact::TurnCommitted { text, .. } if text == "What time is it?"),
        "{fact:?}"
    );
    assert_eq!(hears(&mut session, &mut client).await, Heard::Told(said));
    // A message of a version to come is passed over by a client.
    session.link().tell(FromServer::Unknown).await.unwrap();
    session
        .link()
        .tell(FromServer::Gap { resumes: 9 })
        .await
        .unwrap();
    let gap = Heard::Gap { resumes: 9 };
    assert_eq!(hears(&mut session, &mut client).await, gap);
    session.send(tag(1), 0, 9, true);
    let begins = Heard::Begins {
        run: 0,
        generation: 1,
    };
    assert_eq!(hears(&mut session, &mut client).await, begins);
}

#[tokio::test]
async fn what_a_client_asks_that_a_transport_cannot_do_is_handed_on() {
    let (mut session, mut client) = call(pcm(24_000)).await;
    session
        .during(async { client.interrupt().await.unwrap() })
        .await;
    let mut link = session.link.take().unwrap();
    let asked = session.during(link.asked()).await;
    session.link = Some(link);
    assert_eq!(asked, Some(FromClient::Interrupt));
    // To the session nothing happened: stopping an agent is not said
    // through a transport.
    assert_eq!(session.seen.len(), 1);

    // More than whoever does it has taken is refused, and can be asked
    // again.
    session
        .during(async {
            for _ in 0..17 {
                client.interrupt().await.unwrap();
            }
        })
        .await;
    let Heard::Error { code, .. } = hears(&mut session, &mut client).await else {
        panic!("no refusal");
    };
    assert_eq!(code, ErrorCode::Busy);
}

#[tokio::test]
async fn a_client_that_says_goodbye_has_left() {
    let (mut session, client) = call(pcm(24_000)).await;
    session.during(client.bye()).await;
    session.until(3).await;
    assert_eq!(session.seen[1..], [TransportEvent::Left { who: CALLER }]);
    assert!(session.closed);
    // Whoever told it of the session has nobody to tell.
    assert_eq!(session.link().tell(fact(1, "anyone?")).await, Err(Gone));
    assert_eq!(session.link().asked().await, None);
    assert!(session.handle().is_over());
    // What is sent now goes nowhere, and is not refused.
    session.send(tag(1), 0, 1, true);
    session
        .transport
        .try_control(TransportControl::Clear { tag: tag(1) })
        .unwrap();
    // There is nobody to say goodbye to.
    let said = poll_fn(|cx| session.transport.poll_close(cx));
    tokio::time::timeout(SOON, said).await.unwrap();
}

#[tokio::test]
async fn a_client_whose_connection_drops_is_waited_for_and_taken_back() {
    let (mut session, mut client) = call(pcm(24_000)).await;
    session.send(tag(7), 0, 1, false);
    let begins = Heard::Begins {
        run: 0,
        generation: 7,
    };
    assert_eq!(hears(&mut session, &mut client).await, begins);
    client.drop_line();
    session.until(2).await;
    // The line dropped. The session is not over, and its transport is
    // not closed.
    assert_eq!(session.seen[1..], [TransportEvent::Detached]);
    assert!(!session.closed && !session.handle().is_over());
    // Whoever told the client has nobody to tell, and what the session
    // sends goes nowhere.
    assert_eq!(session.link().tell(fact(1, "anyone?")).await, Err(Gone));
    session.send(tag(7), 20, 2, false);

    // It comes back and says where it was.
    let hello = Hello {
        resume: Some(Resume {
            token: "wpr1_madeup".to_owned(),
            life: 1,
            seen: 4,
        }),
        ..Hello::new(pcm(24_000))
    };
    let mut client = session.comes_back(hello).await;
    assert_eq!(*client.welcome(), welcome(true));
    session.until(3).await;
    assert_eq!(session.seen[2..], [TransportEvent::Attached]);

    // It holds nothing of what the connection before it was sent: the
    // audio that follows says again whose it is, under the number it
    // had. What was sent while nobody was there is not sent now.
    session.send(tag(7), 40, 3, true);
    assert_eq!(hears(&mut session, &mut client).await, begins);
    let audio = Heard::Audio(Bytes::from_static(&[3; 960]));
    assert_eq!(hears(&mut session, &mut client).await, audio);
    assert_eq!(hears(&mut session, &mut client).await, Heard::Ends(0));
    // What it played of it is taken as before: no more than it was sent
    // on either connection.
    session
        .during(async {
            client.played(0, 99_999).await.unwrap();
            client.audio(Bytes::from_static(&[0; 960])).await.unwrap();
        })
        .await;
    session.until(5).await;
    assert_eq!(
        session.seen[3],
        TransportEvent::Played {
            tag: tag(7),
            played: MediaDur::from_millis(40),
        }
    );
    // And what it says is heard after what the one before it said.
    assert!(
        matches!(session.seen[4], TransportEvent::Audio { sequence: 0, .. }),
        "{:?}",
        session.seen[4]
    );
    session.link().tell(fact(5, "back")).await.unwrap();
    assert!(matches!(
        hears(&mut session, &mut client).await,
        Heard::Fact { seq: 5, .. }
    ));
}

#[tokio::test]
async fn a_client_that_comes_while_another_is_there_takes_its_place() {
    let (mut session, mut first) = call(pcm(24_000)).await;
    let (caller, connecting) = session.comes(Hello::new(pcm(24_000))).await;
    let link = session
        .handle()
        .attach(caller, welcome(true))
        .await
        .unwrap();
    let before = session.link.replace(link).unwrap();
    let mut second = session
        .during(async { connecting.await.unwrap().unwrap() })
        .await;
    // The one that was there is told that it was replaced, and is not to
    // come back by itself.
    let heard = session
        .during(async {
            let bye = first.next().await.unwrap();
            (bye, first.next().await.unwrap())
        })
        .await;
    assert_eq!(heard, (Some(Heard::Bye(ByeReason::Replaced)), None));
    assert_eq!(before.tell(fact(1, "anyone?")).await, Err(Gone));
    // To the session the line dropped and came back: what the new one
    // hears begins with it.
    session.until(3).await;
    assert_eq!(
        session.seen[1..],
        [TransportEvent::Detached, TransportEvent::Attached]
    );
    session.send(tag(1), 0, 5, true);
    let begins = Heard::Begins {
        run: 0,
        generation: 1,
    };
    assert_eq!(hears(&mut session, &mut second).await, begins);
}

#[tokio::test]
async fn a_client_with_other_audio_than_the_session_has_is_not_attached() {
    let (mut session, mut client) = call(pcm(24_000)).await;
    for other in [pcm(16_000), opus(24_000), None] {
        let (caller, connecting) = session.comes(Hello::new(other)).await;
        let unattached = session
            .handle()
            .attach(caller, welcome(true))
            .await
            .unwrap_err();
        assert_eq!(unattached.code(), ErrorCode::Unsupported);
        assert!(unattached.why().contains("other audio"));
        unattached.refuse().await;
        let Err(Unconnected::Refused { code, message }) = connecting.await.unwrap() else {
            panic!("it was not refused");
        };
        assert_eq!(code, ErrorCode::Unsupported);
        assert!(message.contains("other audio"), "{message}");
    }
    // The one that is there was not disturbed.
    session.link().tell(fact(1, "still here")).await.unwrap();
    assert!(matches!(
        hears(&mut session, &mut client).await,
        Heard::Fact { seq: 1, .. }
    ));
    assert_eq!(session.seen.len(), 1);
}

#[tokio::test]
async fn nobody_is_attached_to_a_session_that_is_over() {
    let (mut session, client) = call(pcm(24_000)).await;
    client.drop_line();
    session.until(2).await;
    let (caller, connecting) = session.comes(Hello::new(pcm(24_000))).await;
    // The session ended while its client was away.
    let Session {
        transport, handle, ..
    } = session;
    let handle = handle.unwrap();
    drop(transport);
    assert!(handle.is_over());
    let unattached = handle.attach(caller, welcome(true)).await.unwrap_err();
    assert_eq!(unattached.code(), ErrorCode::NoSession);
    unattached.refuse().await;
    let Err(Unconnected::Refused { code, .. }) = connecting.await.unwrap() else {
        panic!("it was not refused");
    };
    assert_eq!(code, ErrorCode::NoSession);
}

#[tokio::test]
async fn a_client_nobody_can_bring_back_has_left() {
    let (mut session, client) = call(pcm(24_000)).await;
    // Whoever could attach a client is gone.
    session.handle = None;
    client.drop_line();
    session.until(4).await;
    assert_eq!(
        session.seen[1..],
        [
            TransportEvent::Detached,
            TransportEvent::Left { who: CALLER }
        ]
    );
    assert!(session.closed);
}

#[tokio::test]
async fn a_session_that_is_over_says_goodbye() {
    let (mut session, mut client) = call(pcm(24_000)).await;
    session.send(tag(1), 0, 4, true);
    // What the client is told last is told before the goodbye, though it
    // is told after the session asked its transport to say it.
    let Session {
        transport, link, ..
    } = &mut session;
    let link = link.as_mut().unwrap();
    // The session has closed: it looks at its transport no more, and
    // asks it to say goodbye. What it sent before still arrives.
    let leaving = async {
        poll_fn(|cx| transport.poll_close(cx)).await;
        // Asking again changes nothing.
        poll_fn(|cx| transport.poll_close(cx)).await;
    };
    let telling = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        link.tell(fact(9, "the last of it")).await.unwrap();
    };
    let hearing = async {
        let mut heard = Vec::new();
        while let Some(next) = client.next().await.unwrap() {
            heard.push(next);
        }
        heard
    };
    let all = async { tokio::join!(hearing, leaving, telling) };
    let (heard, (), ()) = tokio::time::timeout(SOON, all).await.unwrap();
    let Some(Heard::Fact { seq: 9, .. }) = heard.get(3) else {
        panic!("{heard:?}");
    };
    assert_eq!(
        heard[..3],
        [
            Heard::Begins {
                run: 0,
                generation: 1
            },
            Heard::Audio(Bytes::from_static(&[4; 960])),
            Heard::Ends(0),
        ]
    );
    assert_eq!(heard[4..], [Heard::Bye(ByeReason::Ended)]);
    // The transport is closed, and says so.
    let next = poll_fn(|cx| session.transport.poll_event(cx));
    assert_eq!(tokio::time::timeout(SOON, next).await.unwrap(), None);
    assert_eq!(session.link().tell(fact(10, "anyone?")).await, Err(Gone));
}

#[tokio::test]
async fn a_goodbye_does_not_wait_for_whoever_has_no_more_to_tell() {
    let (session, mut client) = call(pcm(24_000)).await;
    let Session {
        mut transport,
        link,
        ..
    } = session;
    let link = link.unwrap();
    link.tell(fact(9, "the last of it")).await.unwrap();
    // Whoever tells the client of the session is done.
    drop(link);
    let began = Instant::now();
    let leaving = poll_fn(|cx| transport.poll_close(cx));
    let hearing = async {
        let mut heard = Vec::new();
        while let Some(next) = client.next().await.unwrap() {
            heard.push(next);
        }
        (heard, began.elapsed())
    };
    let both = async { tokio::join!(hearing, leaving) };
    let ((heard, took), ()) = tokio::time::timeout(SOON, both).await.unwrap();
    assert!(matches!(heard[0], Heard::Fact { seq: 9, .. }), "{heard:?}");
    assert_eq!(heard[1..], [Heard::Bye(ByeReason::Ended)]);
    // Nothing was waited for: a session waits for its transport's
    // goodbye for half a second at most.
    assert!(took < Duration::from_millis(140), "{took:?}");
}

#[tokio::test]
async fn a_client_that_is_still_talking_when_the_call_ends_is_told_how_it_ended() {
    let (mut session, mut client) = call(pcm(48_000)).await;
    session
        .link()
        .tell(fact(9, "the last of it"))
        .await
        .unwrap();
    // The session has closed, and its transport says goodbye. The client
    // is in the middle of sending what its microphone hears, and reads
    // nothing meanwhile.
    let leaving = poll_fn(|cx| session.transport.poll_close(cx));
    let talking = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // It goes on for a length of time and not for a number of
        // pieces: a pause of 2 ms is 2 ms on one machine and 16 ms on
        // another, and the transport waits for its client only so long.
        let until = Instant::now() + Duration::from_millis(70);
        while Instant::now() < until {
            // Not one piece of audio fails to be sent: the connection is
            // not let go of while the client is at it.
            client.audio(Bytes::from_static(&[1; 1_920])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // And everything it was told is there when it reads: how the
        // call ended, and the goodbye.
        let mut heard = Vec::new();
        while let Some(next) = client.next().await.unwrap() {
            heard.push(next);
        }
        drop(client);
        heard
    };
    let (heard, ()) = tokio::time::timeout(SOON, async { tokio::join!(talking, leaving) })
        .await
        .unwrap();
    assert!(matches!(heard[0], Heard::Fact { seq: 9, .. }), "{heard:?}");
    assert_eq!(heard[1..], [Heard::Bye(ByeReason::Ended)]);
}

/// A client that speaks the WebSocket protocol and then whatever it
/// likes.
async fn stranger(address: &str) -> WsConn {
    let url = format!("{address}{PATH}");
    let request = WsRequest {
        headers: &[("Sec-WebSocket-Protocol", PROTOCOL)],
        ..WsRequest::new(&url)
    };
    WsConn::connect(request).await.unwrap()
}

/// The next message of words on a connection.
async fn words(conn: &mut WsConn) -> FromServer {
    let Some(WsMessage::Text(text)) = conn.next().await.unwrap() else {
        panic!("no words");
    };
    serde_json::from_str(text.as_str()).unwrap()
}

/// What a client that is turned away is told, and the complaint its
/// connection is closed with.
async fn turned_away(conn: &mut WsConn) -> (ErrorCode, String, String) {
    let FromServer::Error { code, message } = words(conn).await else {
        panic!("no refusal");
    };
    let bye = FromServer::Bye {
        reason: ByeReason::Refused,
    };
    assert_eq!(words(conn).await, bye);
    let complaint = conn.next().await.unwrap_err();
    assert_eq!(complaint.class, RetryClass::Permanent, "{complaint}");
    (code, message, complaint.message.into())
}

#[tokio::test]
async fn a_client_that_does_not_begin_with_a_hello_is_turned_away() {
    let hello = "the first message has to be a hello";
    for (first, code, why) in [
        (r#"{"type":"bye"}"#, ErrorCode::OutOfPlace, hello),
        ("good morning", ErrorCode::OutOfPlace, hello),
        // Version zero of this transport began so.
        (
            r#"{"type":"hello","rate":24000}"#,
            ErrorCode::OutOfPlace,
            hello,
        ),
        (r#"{"type":"hello","v":2}"#, ErrorCode::Version, "a version"),
        (
            r#"{"type":"hello","v":1,"audio":{"codec":"pcm16","rate":44100}}"#,
            ErrorCode::Unsupported,
            "a rate that is not offered",
        ),
        (
            r#"{"type":"hello","v":1,"token":"letmein"}"#,
            ErrorCode::Token,
            "does not begin as one does",
        ),
        (
            r#"{"type":"hello","v":1,"resume":{"token":"wpc1_madeup","life":1,"seen":0}}"#,
            ErrorCode::Token,
            "does not begin as one does",
        ),
    ] {
        let (listener, address) = listener().await;
        let first = first.to_owned();
        let client = tokio::spawn(async move {
            let mut conn = stranger(&address).await;
            conn.send_text(first).await.unwrap();
            turned_away(&mut conn).await
        });
        let knock = listener.accept().await.unwrap();
        let error = knock.greet(GREETING_TIMEOUT).await.unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent, "{error}");
        assert!(error.message.contains(why), "{error}");
        // What it said is not repeated: it may hold a token.
        assert!(!error.message.contains("letmein"), "{error}");
        let (told, message, complaint) = client.await.unwrap();
        assert_eq!(told, code, "{message}");
        assert!(message.contains(why), "{message}");
        assert!(complaint.contains("1008"), "{complaint}");
    }
}

#[tokio::test]
async fn a_client_that_never_says_hello_is_not_waited_for() {
    let (listener, address) = listener().await;
    let client = tokio::spawn(async move {
        let mut conn = stranger(&address).await;
        turned_away(&mut conn).await
    });
    let knock = listener.accept().await.unwrap();
    let error = knock.greet(Duration::from_millis(100)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Timeout, "{error}");
    let (code, message, _) = client.await.unwrap();
    assert_eq!(code, ErrorCode::OutOfPlace);
    assert!(message.contains("no hello came in time"), "{message}");
}

/// Something a client must not send in the middle of a call.
enum Wrong {
    Words(String),
    Audio(Vec<u8>),
}

/// A session whose client says whatever a test has it say, after a
/// hello with audio at 24 kHz.
async fn call_of_a_stranger() -> (Session, WsConn) {
    let (listener, address) = listener().await;
    let connecting = {
        let address = address.clone();
        tokio::spawn(async move {
            let mut conn = stranger(&address).await;
            let hello = r#"{"type":"hello","v":1,"audio":{"codec":"pcm16","rate":24000}}"#;
            conn.send_text(hello.to_owned()).await.unwrap();
            let Some(WsMessage::Text(welcome)) = conn.next().await.unwrap() else {
                panic!("no answer");
            };
            assert_eq!(
                welcome.as_str(),
                r#"{"type":"welcome","v":1,"session":"s-7","life":1,"resumed":false,"resume_token":"wpr1_madeup","audio":{"codec":"pcm16","rate":24000},"next_seq":1,"grace_ms":30000}"#
            );
            conn
        })
    };
    let knock = listener.accept().await.unwrap();
    let caller = knock.greet(GREETING_TIMEOUT).await.unwrap();
    let (transport, handle) = caller.transport(CALLER);
    let link = handle.attach(caller, welcome(false)).await.unwrap();
    let mut session = Session {
        transport,
        handle: Some(handle),
        link: Some(link),
        listener,
        address,
        seen: Vec::new(),
        closed: false,
    };
    let conn = session.during(async { connecting.await.unwrap() }).await;
    session.until(1).await;
    (session, conn)
}

#[tokio::test]
async fn a_message_that_is_not_taken_is_refused_and_the_call_goes_on() {
    let (mut session, mut conn) = call_of_a_stranger().await;
    let long = format!(
        r#"{{"type":"text","text":"{}"}}"#,
        "a".repeat(16 * 1024 + 1)
    );
    let words_of = |text: &str| Wrong::Words(text.to_owned());
    let wrongs = [
        (
            Wrong::Audio(vec![0; 3]),
            ErrorCode::Malformed,
            "whole samples",
        ),
        (
            Wrong::Audio(vec![0; 48_002]),
            ErrorCode::Malformed,
            "a second at most",
        ),
        (
            words_of(r#"{"type":"hello","v":1}"#),
            ErrorCode::OutOfPlace,
            "said once",
        ),
        // One that could not be a hello is still one out of place.
        (
            words_of(r#"{"type":"hello","v":7}"#),
            ErrorCode::OutOfPlace,
            "said once",
        ),
        (
            words_of(r#"{"type":"dance"}"#),
            ErrorCode::UnknownMessage,
            "not known here",
        ),
        (
            words_of("good morning"),
            ErrorCode::Malformed,
            "not a message of the protocol",
        ),
        (
            words_of(r#"{"type":"text","text":""}"#),
            ErrorCode::Malformed,
            "empty or too long",
        ),
        (words_of(&long), ErrorCode::Malformed, "empty or too long"),
        (
            words_of(r#"{"type":"dtmf","keys":"12x"}"#),
            ErrorCode::Malformed,
            "not keys of a telephone",
        ),
        (
            words_of(r#"{"type":"played_rtp","delay_ms":20}"#),
            ErrorCode::Unsupported,
            "by its run",
        ),
    ];
    let count = wrongs.len();
    for (number, (wrong, code, why)) in wrongs.into_iter().enumerate() {
        let heard = session
            .during(async {
                match wrong {
                    Wrong::Words(text) => conn.send_text(text).await.unwrap(),
                    Wrong::Audio(bytes) => conn.send_binary(bytes.into()).await.unwrap(),
                }
                // It is told what was wrong, and nothing else.
                let heard = words(&mut conn).await;
                // And is heard as before.
                let still = format!(r#"{{"type":"text","text":"still here {number}"}}"#);
                conn.send_text(still).await.unwrap();
                heard
            })
            .await;
        let FromServer::Error {
            code: told,
            message,
        } = heard
        else {
            panic!("{heard:?}");
        };
        assert_eq!(told, code, "{message}");
        assert!(message.contains(why), "{message}");
    }
    session.until(1 + count).await;
    let said: Vec<String> = session.seen[1..]
        .iter()
        .map(|event| match event {
            TransportEvent::Text { text, .. } => text.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    let expected: Vec<String> = (0..count).map(|n| format!("still here {n}")).collect();
    assert_eq!(said, expected);
    assert!(!session.closed);
}

#[tokio::test]
async fn a_client_whose_agent_nobody_stops_is_told_so() {
    let (mut session, mut client) = call(pcm(24_000)).await;
    // Whoever started the session took the transport and nothing else.
    session.link = None;
    let heard = session
        .during(async {
            client.interrupt().await.unwrap();
            client.next().await.unwrap().unwrap()
        })
        .await;
    let Heard::Error { code, .. } = heard else {
        panic!("{heard:?}");
    };
    assert_eq!(code, ErrorCode::Unsupported);
}

#[tokio::test]
async fn a_client_that_asks_for_another_protocol_is_refused() {
    let (listener, address) = listener().await;
    let client = tokio::spawn(async move {
        let url = format!("{address}{PATH}");
        let request = WsRequest {
            headers: &[("Sec-WebSocket-Protocol", "waveport.ws.v0")],
            ..WsRequest::new(&url)
        };
        WsConn::connect(request).await.unwrap_err()
    });
    let knock = listener.accept().await.unwrap();
    assert!(knock.from().ip().is_loopback());
    knock.greet(GREETING_TIMEOUT).await.unwrap_err();
    let error = client.await.unwrap();
    assert_eq!(error.class, RetryClass::Permanent, "{error}");
}

#[tokio::test]
async fn a_client_that_is_turned_away_after_its_hello_is_told_why() {
    let (listener, address) = listener().await;
    let client = tokio::spawn(async move {
        Client::connect(&address, &Hello::new(pcm(24_000)), SOON)
            .await
            .unwrap_err()
    });
    let caller = listener
        .accept()
        .await
        .unwrap()
        .greet(GREETING_TIMEOUT)
        .await
        .unwrap();
    caller
        .refuse(ErrorCode::Busy, "there is no room for another call")
        .await;
    let error = client.await.unwrap();
    let shown = error.to_string();
    let Unconnected::Refused { code, message } = error else {
        panic!("{error:?}");
    };
    assert_eq!(code, ErrorCode::Busy);
    assert_eq!(message, "there is no room for another call");
    assert!(shown.contains("turned the client away"), "{shown}");
}

#[tokio::test]
async fn a_server_that_cannot_be_reached_is_no_refusal() {
    let (listener, address) = listener().await;
    drop(listener);
    let error = Client::connect(&address, &Hello::new(None), SOON)
        .await
        .unwrap_err();
    assert!(matches!(error, Unconnected::Failed(_)), "{error:?}");
}

#[tokio::test]
async fn a_listener_cannot_be_reached_from_elsewhere_unless_it_is_said_that_it_may() {
    let everywhere = "0.0.0.0:0".parse().unwrap();
    let error = Listener::bind(everywhere, false).await.unwrap_err();
    assert!(matches!(error, ListenError::Exposed(_)), "{error}");
    assert!(error.to_string().contains("nothing asks who they are"));
    let listener = Listener::bind(everywhere, true).await.unwrap();
    assert_ne!(listener.address().unwrap().port(), 0);
}
