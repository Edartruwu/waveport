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

use bytes::Bytes;
use waveport_adapter_kit::ws::{WsConn, WsMessage, WsRequest};
use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId};
use waveport_domain::time::MediaDur;
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::RetryClass;
use waveport_ports::transport::{FrameOut, Transport, TransportControl, TransportEvent};
use waveport_transport_websocket::{
    Client, GREETING_TIMEOUT, Heard, Lines, ListenError, Listener, PATH, PROTOCOL, WsTransport,
};

const CALLER: ParticipantId = ParticipantId(3);

/// How long anything here may take. It all happens on one machine.
const SOON: Duration = Duration::from_secs(5);

fn tag(generation: u32) -> OpTag {
    OpTag {
        generation: GenId(generation),
        epoch: Epoch(1),
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
    lines: Lines,
    /// What the transport has said so far.
    seen: Vec<TransportEvent>,
    /// Whether the transport has said that it is closed.
    closed: bool,
}

impl Session {
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

    /// Sends 20 ms of a response at 24 kHz, every sample of it `fill`.
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
}

/// A session and its client, connected, with audio at `rate`.
async fn call(rate: Rate) -> (Session, Client) {
    let (listener, address) = listener().await;
    let connecting =
        tokio::spawn(async move { Client::connect(&address, rate, SOON).await.unwrap() });
    let caller = listener
        .accept()
        .await
        .unwrap()
        .greet(GREETING_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(caller.rate(), rate);
    assert!(caller.from().ip().is_loopback());
    let (transport, lines) = caller.into_transport("s-7", CALLER);
    let mut session = Session {
        transport,
        lines,
        seen: Vec::new(),
        closed: false,
    };
    // The client is answered when the session first looks at its
    // transport.
    let client = session.during(async { connecting.await.unwrap() }).await;
    assert_eq!(client.session(), "s-7");
    session.until(1).await;
    assert_eq!(session.seen, [TransportEvent::Joined { who: CALLER }]);
    (session, client)
}

/// The next thing a client hears.
async fn hears(session: &mut Session, client: &mut Client) -> Heard {
    session
        .during(async { client.next().await.unwrap().unwrap() })
        .await
}

#[tokio::test]
async fn a_transport_says_what_its_client_asked_for() {
    let (session, _client) = call(Rate::Hz16000).await;
    let caps = session.transport.caps();
    assert_eq!(caps.inbound, Codec::Pcm(Rate::Hz16000));
    assert_eq!(caps.outbound, Codec::Pcm(Rate::Hz16000));
    assert!(caps.remote_buffer.clearable && caps.remote_buffer.reports_played);
    assert!(caps.text);
}

#[tokio::test]
async fn what_the_microphone_heard_arrives_in_packets() {
    let (mut session, mut client) = call(Rate::Hz16000).await;
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
async fn a_response_is_marked_and_what_was_played_of_it_is_told() {
    let (mut session, mut client) = call(Rate::Hz24000).await;
    session.send(tag(7), 0, 1, false);
    session.send(tag(7), 20, 2, false);
    session.send(tag(7), 40, 3, true);
    assert_eq!(hears(&mut session, &mut client).await, Heard::Begins(0));
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
        })
        .await;
    session.until(4).await;
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
        ]
    );
}

#[tokio::test]
async fn a_response_that_is_discarded_is_said_to_be() {
    let (mut session, mut client) = call(Rate::Hz24000).await;
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
    assert_eq!(
        heard,
        [
            Heard::Begins(0),
            Heard::Audio(Bytes::from_static(&[1; 960])),
            Heard::Clear(0),
            Heard::Begins(1),
            Heard::Audio(Bytes::from_static(&[2; 960])),
            Heard::Ends(1),
        ]
    );
}

#[tokio::test]
async fn the_client_is_told_about_the_call_in_lines() {
    let (mut session, mut client) = call(Rate::Hz48000).await;
    session.lines.say("caller: What time is it?");
    session.lines.clone().say("agent: Ten past three.");
    assert_eq!(
        hears(&mut session, &mut client).await,
        Heard::Line("caller: What time is it?".to_owned())
    );
    assert_eq!(
        hears(&mut session, &mut client).await,
        Heard::Line("agent: Ten past three.".to_owned())
    );
    // Whoever told the lines can go: the call goes on.
    let Session { lines, .. } = &session;
    drop(lines.clone());
    session.send(tag(1), 0, 9, true);
    assert_eq!(hears(&mut session, &mut client).await, Heard::Begins(0));
}

#[tokio::test]
async fn a_client_that_says_goodbye_has_left() {
    let (mut session, client) = call(Rate::Hz24000).await;
    session.during(client.bye()).await;
    session.until(3).await;
    assert_eq!(session.seen[1..], [TransportEvent::Left { who: CALLER }]);
    assert!(session.closed);
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
async fn a_client_whose_connection_drops_has_left() {
    let (mut session, client) = call(Rate::Hz24000).await;
    drop(client);
    session.until(3).await;
    assert_eq!(session.seen[1..], [TransportEvent::Left { who: CALLER }]);
    assert!(session.closed);
    let said = poll_fn(|cx| session.transport.poll_close(cx));
    tokio::time::timeout(SOON, said).await.unwrap();
}

#[tokio::test]
async fn a_session_that_is_over_says_goodbye() {
    let (mut session, mut client) = call(Rate::Hz24000).await;
    session.send(tag(1), 0, 4, true);
    // The session has closed: it looks at its transport no more, and
    // asks it to say goodbye. What it sent before still arrives.
    let leaving = async {
        poll_fn(|cx| session.transport.poll_close(cx)).await;
        // Asking again changes nothing.
        poll_fn(|cx| session.transport.poll_close(cx)).await;
    };
    let hearing = async {
        let mut heard = Vec::new();
        while let Some(next) = client.next().await.unwrap() {
            heard.push(next);
        }
        heard
    };
    let (heard, ()) = tokio::time::timeout(SOON, async { tokio::join!(hearing, leaving) })
        .await
        .unwrap();
    assert_eq!(
        heard,
        [
            Heard::Begins(0),
            Heard::Audio(Bytes::from_static(&[4; 960])),
            Heard::Ends(0),
            Heard::Bye("the call is over".to_owned()),
        ]
    );
    // The transport is closed, and says so.
    let next = poll_fn(|cx| session.transport.poll_event(cx));
    assert_eq!(tokio::time::timeout(SOON, next).await.unwrap(), None);
}

#[tokio::test]
async fn a_client_that_is_still_talking_when_the_call_ends_is_told_how_it_ended() {
    let (mut session, mut client) = call(Rate::Hz48000).await;
    session.lines.say("the call ended: it was asked to");
    // The session has closed, and its transport says goodbye. The client
    // is in the middle of sending what its microphone hears, and reads
    // nothing meanwhile.
    let leaving = poll_fn(|cx| session.transport.poll_close(cx));
    let talking = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        for _ in 0..40 {
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
    assert_eq!(
        heard,
        [
            Heard::Line("the call ended: it was asked to".to_owned()),
            Heard::Bye("the call is over".to_owned()),
        ]
    );
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

/// What a client that is turned away is told, and the complaint its
/// connection is closed with.
async fn turned_away(conn: &mut WsConn) -> (String, String) {
    let Some(WsMessage::Text(bye)) = conn.next().await.unwrap() else {
        panic!("no goodbye");
    };
    let complaint = conn.next().await.unwrap_err();
    assert_eq!(complaint.class, RetryClass::Permanent, "{complaint}");
    (bye.as_str().to_owned(), complaint.message.into())
}

#[tokio::test]
async fn a_client_that_does_not_begin_with_a_hello_is_turned_away() {
    for (first, why) in [
        ("{\"type\":\"bye\"}", "the first message has to be a hello"),
        ("good morning", "the first message has to be a hello"),
        (
            "{\"type\":\"hello\",\"rate\":44100}",
            "44100 Hz is not taken",
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
        let (bye, complaint) = client.await.unwrap();
        assert!(
            bye.contains("\"type\":\"bye\"") && bye.contains(why),
            "{bye}"
        );
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
    let (bye, _) = client.await.unwrap();
    assert!(bye.contains("no hello came in time"), "{bye}");
}

/// Something a client must not send in the middle of a call.
enum Wrong {
    Words(&'static str),
    Audio(Vec<u8>),
}

#[tokio::test]
async fn a_client_that_breaks_the_protocol_in_a_call_is_turned_away() {
    for (wrong, why) in [
        (Wrong::Audio(vec![0; 3]), "whole samples"),
        (Wrong::Audio(vec![0; 48_002]), "a second at most"),
        (
            Wrong::Words("{\"type\":\"hello\",\"rate\":24000}"),
            "said once",
        ),
        (
            Wrong::Words("{\"type\":\"dance\"}"),
            "not a message of the protocol",
        ),
    ] {
        let (listener, address) = listener().await;
        let client = tokio::spawn(async move {
            let mut conn = stranger(&address).await;
            conn.send_text("{\"type\":\"hello\",\"rate\":24000}".to_owned())
                .await
                .unwrap();
            let Some(WsMessage::Text(ready)) = conn.next().await.unwrap() else {
                panic!("no answer");
            };
            assert_eq!(
                ready.as_str(),
                "{\"type\":\"ready\",\"session\":\"s-1\",\"rate\":24000}"
            );
            match wrong {
                Wrong::Words(text) => conn.send_text(text.to_owned()).await.unwrap(),
                Wrong::Audio(bytes) => conn.send_binary(bytes.into()).await.unwrap(),
            }
            turned_away(&mut conn).await
        });
        let caller = listener
            .accept()
            .await
            .unwrap()
            .greet(GREETING_TIMEOUT)
            .await
            .unwrap();
        let (transport, lines) = caller.into_transport("s-1", CALLER);
        let mut session = Session {
            transport,
            lines,
            seen: Vec::new(),
            closed: false,
        };
        let (bye, _) = session.during(async { client.await.unwrap() }).await;
        assert!(bye.contains(why), "{bye}");
        // To the session the client has left.
        session.until(3).await;
        assert_eq!(
            session.seen,
            [
                TransportEvent::Joined { who: CALLER },
                TransportEvent::Left { who: CALLER },
            ]
        );
        assert!(session.closed);
    }
}

#[tokio::test]
async fn a_client_that_asks_for_another_protocol_is_refused() {
    let (listener, address) = listener().await;
    let client = tokio::spawn(async move {
        let url = format!("{address}{PATH}");
        let request = WsRequest {
            headers: &[("Sec-WebSocket-Protocol", "waveport.ws.v9")],
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
        Client::connect(&address, Rate::Hz24000, SOON)
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
    caller.turn_away("there is no room for another call").await;
    let error = client.await.unwrap();
    assert_eq!(error.class, RetryClass::Permanent, "{error}");
    assert!(
        error.message.contains("no room for another call"),
        "{error}"
    );
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
