//! The transport and a stand-in for a carrier, talking to each other on
//! this machine.
//!
//! Nothing here leaves the machine, and nothing here is a carrier: the
//! stand-in does what the documentation says one does. The clock is the
//! real one: the connections are real, and a clock that a test moves
//! does not move them.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a helper that cannot go on, or a test that finds something missing, has failed"
)]

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;
use core::time::Duration;

use bytes::Bytes;
use tokio::net::TcpListener;
use waveport_adapter_kit::ws::{WsConn, WsMessage};
use waveport_domain::event::Key;
use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId};
use waveport_domain::time::MediaDur;
use waveport_ports::audio::{Codec, Law, Rate};
use waveport_ports::error::{PortError, RetryClass};
use waveport_ports::transport::{
    FrameOut, KeysOut, ToneOut, Transport, TransportControl, TransportEvent,
};
use waveport_testkit::conformance::{
    FarEnd, FarHeard, transport_hears, transport_is_left, transport_says_goodbye, transport_speaks,
};
use waveport_transport_telephony_ws::sim::{Answered, Ringing, Sent, Sim};
use waveport_transport_telephony_ws::{Call, Carrier, CarrierTransport, STARTS_WITHIN};

const CALLER: ParticipantId = ParticipantId(3);
const TWILIO: Carrier = Carrier::Twilio;

/// How long anything here may take. It all happens on one machine.
const SOON: Duration = Duration::from_secs(5);

/// How many samples 20 ms of a telephone's audio are, and how many
/// bytes.
const PIECE: usize = 160;

fn tag(generation: u32) -> OpTag {
    OpTag {
        generation: GenId(generation),
        epoch: Epoch(1),
    }
}

const fn ms(millis: u32) -> MediaDur {
    MediaDur::from_millis(millis)
}

const fn played(of: OpTag, millis: u32) -> TransportEvent {
    TransportEvent::Played {
        tag: of,
        played: ms(millis),
    }
}

/// What the name of a mark says, as the crate's documentation has it:
/// `r<run>.<samples>`, with `.end` after the last of a response.
fn mark(name: &str) -> (u32, u32, bool) {
    let (name, last) = match name.strip_suffix(".end") {
        Some(name) => (name, true),
        None => (name, false),
    };
    let (run, samples) = name.strip_prefix('r').unwrap().split_once('.').unwrap();
    (run.parse().unwrap(), samples.parse().unwrap(), last)
}

/// A place on this machine for a carrier to connect to, and its
/// address.
async fn stream() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("ws://{}/stream", listener.local_addr().unwrap());
    (listener, address)
}

/// The server's end of the next connection.
async fn taken(listener: &TcpListener) -> WsConn {
    let (tcp, _) = listener.accept().await.unwrap();
    let (conn, asked) = WsConn::accept(tcp, "/stream", None, SOON).await.unwrap();
    assert_eq!(asked.path, "/stream");
    conn
}

/// A stand-in that has connected and said nothing yet, and the server's
/// end of its connection.
async fn connected() -> (WsConn, Sim) {
    let (listener, address) = stream().await;
    let connecting = tokio::spawn(async move {
        Sim::connects(TWILIO, &address, &[], Ringing::made_up(7))
            .await
            .unwrap()
    });
    let conn = taken(&listener).await;
    (conn, connecting.await.unwrap())
}

/// A call that a stand-in brings, before it has a session.
async fn rings(parameters: &[(String, String)]) -> (Call, Sim) {
    let (conn, mut sim) = connected().await;
    sim.begins(parameters).await.unwrap();
    let call = Call::of(TWILIO, conn, STARTS_WITHIN).await.unwrap();
    (call, sim)
}

/// The session's side of a call.
struct Line {
    transport: CarrierTransport,
    /// What the transport has said so far.
    seen: Vec<TransportEvent>,
    /// Whether the transport has said that it is closed.
    closed: bool,
}

impl Line {
    /// Does `work`, which is the carrier's, while the session looks at
    /// its transport as a running session does.
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

    /// Sends 20 ms of a response, every byte of it `fill`.
    fn send(&mut self, of: OpTag, at: u32, fill: u8, last: bool) {
        let frame = FrameOut {
            tag: of,
            at: ms(at * 20),
            length: ms(20),
            payload: &[fill; PIECE],
            last,
        };
        self.transport.try_send(frame).unwrap();
    }

    /// The next `count` things the carrier is sent.
    async fn sends(&mut self, sim: &mut Sim, count: usize) -> Vec<Sent> {
        let mut sent = Vec::new();
        for _ in 0..count {
            let next = self.during(sim.next()).await.unwrap();
            sent.push(next.unwrap_or_else(|| panic!("the stream closed after {sent:?}")));
        }
        sent
    }

    /// The caller presses a key, and that is the next thing the session
    /// is told: whatever the carrier said before it was told to nobody.
    async fn hears_nothing_before_a_key(&mut self, sim: &mut Sim, key: Key) {
        let before = self.seen.len();
        self.during(sim.presses(key)).await.unwrap();
        self.until(before + 1).await;
        let pressed = TransportEvent::Dtmf { who: CALLER, key };
        assert_eq!(self.seen[before..], [pressed]);
    }
}

/// A call with a session and a caller.
async fn call() -> (Line, Sim) {
    let (call, sim) = rings(&[]).await;
    let mut line = Line {
        transport: call.transport(CALLER),
        seen: Vec::new(),
        closed: false,
    };
    // The caller is there from the first look at the transport.
    line.until(1).await;
    assert_eq!(line.seen, [TransportEvent::Joined { who: CALLER }]);
    (line, sim)
}

/// The stand-in, as the far end that the checks of every transport have
/// act. A check has its far end say what it played in so many words, so
/// this one says no mark back by itself.
struct Far {
    sim: Sim,
    /// The run whose audio follows, and whether it has ended.
    run: Option<(u32, bool)>,
}

impl FarEnd for Far {
    async fn speak(&mut self, n: u16) -> Result<Bytes, PortError> {
        // 20 ms, no two pieces of it alike.
        let piece: Vec<u8> = (0..160).map(|at| (n * 160 + at).to_le_bytes()[0]).collect();
        self.sim.says(&piece).await?;
        Ok(piece.into())
    }

    async fn played(&mut self, run: u32, played: MediaDur) -> Result<(), PortError> {
        let samples = Rate::Hz8000.samples_in(played);
        self.sim.echoes(&format!("r{run}.{samples}")).await
    }

    fn write(&mut self, _text: &str) -> impl Future<Output = Result<(), PortError>> {
        core::future::ready(Err(PortError::permanent("a telephone does not write")))
    }

    async fn press(&mut self, keys: &str) -> Result<bool, PortError> {
        for key in keys.chars() {
            self.sim.presses(Key::from_char(key).unwrap()).await?;
        }
        Ok(true)
    }

    async fn hear(&mut self) -> Result<Option<FarHeard>, PortError> {
        loop {
            let heard = match self.sim.next().await? {
                // A carrier is told nothing: its stream is closed.
                None => return Ok(None),
                Some(Sent::Audio(audio)) => FarHeard::Audio(audio),
                Some(Sent::Mark(name)) => match (mark(&name), self.run) {
                    ((run, _, true), _) => {
                        self.run = Some((run, true));
                        FarHeard::Ends(run)
                    }
                    ((run, _, false), Some((open, false))) if run == open => continue,
                    ((run, _, false), _) => {
                        self.run = Some((run, false));
                        FarHeard::Begins(run)
                    }
                },
                // It lets go of all it holds, the newest run with it.
                Some(Sent::Clear) => match self.run {
                    Some((run, _)) => FarHeard::Clear(run),
                    None => continue,
                },
            };
            return Ok(Some(heard));
        }
    }

    async fn leave(&mut self) -> Result<(), PortError> {
        self.sim.hangs_up().await
    }
}

/// A transport that nobody has looked at yet, and its far end.
async fn unseen() -> (CarrierTransport, Far) {
    let (call, mut sim) = rings(&[]).await;
    sim.waits_to_be_told();
    (call.transport(CALLER), Far { sim, run: None })
}

#[tokio::test]
async fn the_transport_keeps_the_rules_of_every_transport() {
    let (mut transport, mut far) = unseen().await;
    let hears = transport_hears(&mut transport, &mut far, CALLER).await;
    assert_eq!(hears, Ok(()));
    assert_eq!(far.sim.complaints(), [] as [String; 0]);

    let (mut transport, mut far) = unseen().await;
    let speaks = transport_speaks(&mut transport, &mut far, CALLER).await;
    assert_eq!(speaks, Ok(()));
    assert_eq!(far.sim.complaints(), [] as [String; 0]);

    let (mut transport, mut far) = unseen().await;
    let goodbye = transport_says_goodbye(&mut transport, &mut far, CALLER).await;
    assert_eq!(goodbye, Ok(()));
    assert_eq!(far.sim.complaints(), [] as [String; 0]);

    let (mut transport, mut far) = unseen().await;
    let left = transport_is_left(&mut transport, &mut far, CALLER).await;
    assert_eq!(left, Ok(()));
    assert_eq!(far.sim.complaints(), [] as [String; 0]);
}

#[tokio::test]
async fn a_transport_says_what_a_telephone_can_do() {
    let (line, _sim) = call().await;
    let caps = line.transport.caps();
    let audio = caps.audio.unwrap();
    assert_eq!(audio.inbound, Codec::G711(Law::Mu));
    assert_eq!(audio.outbound, Codec::G711(Law::Mu));
    assert!(audio.remote_buffer.clearable && audio.remote_buffer.reports_played);
    assert!(!caps.text);
    assert_eq!(caps.keys, Some(KeysOut::Tones));
}

#[tokio::test]
async fn a_call_says_which_it_is_and_what_its_answer_gave_it() {
    // What a server answers with is what the carrier reads, and what the
    // carrier reads is what its stream says as it begins.
    let given = [("once", "a-made-up-ticket"), ("note", "a<b & \"c\" 'd'")];
    let answer = TWILIO.answer("wss://voice.example/stream", &given).unwrap();
    assert_eq!(answer.content_type, "text/xml; charset=utf-8");
    let answered = Answered::read(TWILIO, &answer.body).unwrap();
    assert_eq!(answered.stream, "wss://voice.example/stream");

    let (call, sim) = rings(&answered.parameters).await;
    assert_eq!(call.carrier(), TWILIO);
    let started = call.started();
    assert_eq!(started.call, sim.ringing().call);
    assert_eq!(started.stream, sim.ringing().stream);
    assert_eq!(started.account.as_ref(), Some(&sim.ringing().account));
    let said: Vec<(&str, &str)> = started
        .parameters
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        said,
        [("note", "a<b & \"c\" 'd'"), ("once", "a-made-up-ticket")]
    );
}

#[tokio::test]
async fn what_the_caller_says_arrives_in_packets_wherever_the_carrier_cuts_it() {
    let (mut line, mut sim) = call().await;
    // 480 samples, no two alike that are near each other, in pieces
    // that end where no packet does.
    let said: Vec<u8> = (0..480_u16).map(|at| (at % 251).to_le_bytes()[0]).collect();
    for piece in [&said[..100], &said[100..400], &said[400..]] {
        line.during(sim.says(piece)).await.unwrap();
    }
    line.until(4).await;
    let packet = |n: u16| TransportEvent::Audio {
        who: CALLER,
        sequence: n,
        timestamp: u32::from(n) * 160,
        payload: Bytes::copy_from_slice(&said[usize::from(n) * PIECE..][..PIECE]),
    };
    assert_eq!(line.seen[1..], [packet(0), packet(1), packet(2)]);

    // More than a second in one message is not audio of a call. It is
    // passed over, and what follows is heard where it belongs.
    line.during(sim.says(&[0x7f; 8_001])).await.unwrap();
    line.during(sim.says(&said[..PIECE])).await.unwrap();
    line.until(5).await;
    assert_eq!(
        line.seen[4],
        TransportEvent::Audio {
            who: CALLER,
            sequence: 3,
            timestamp: 480,
            payload: Bytes::copy_from_slice(&said[..PIECE]),
        }
    );
}

#[tokio::test]
async fn the_session_is_told_what_the_caller_was_played_as_the_carrier_plays_it() {
    let (mut line, mut sim) = call().await;
    for (at, fill) in [(0, 1), (1, 2), (2, 3)] {
        line.send(tag(1), at, fill, fill == 3);
    }
    // A mark where the response begins, and one after every frame.
    let sent = line.sends(&mut sim, 7).await;
    let audio = |fill: u8| Sent::Audio(Bytes::from(vec![fill; PIECE]));
    let marked = |name: &str| Sent::Mark(name.to_owned());
    assert_eq!(
        sent,
        [
            marked("r0.0"),
            audio(1),
            marked("r0.160"),
            audio(2),
            marked("r0.320"),
            audio(3),
            marked("r0.480.end"),
        ]
    );
    assert_eq!(sim.holds(), 3 * PIECE);

    // The carrier held nothing when the response began: the caller is
    // about to hear it, and has heard nothing of it.
    line.until(2).await;
    assert_eq!(line.seen[1..], [played(tag(1), 0)]);

    // What is said to have been played is what was, to within a frame.
    let first = line.during(sim.plays(PIECE)).await.unwrap();
    line.until(3).await;
    assert_eq!(line.seen[2..], [played(tag(1), 20)]);
    let more = line.during(sim.plays(100)).await.unwrap();
    line.hears_nothing_before_a_key(&mut sim, Key::Five).await;
    let rest = line.during(sim.plays(60)).await.unwrap();
    line.until(5).await;
    assert_eq!(line.seen[4..], [played(tag(1), 40)]);
    let last = line.during(sim.plays(10_000)).await.unwrap();
    line.until(6).await;
    assert_eq!(line.seen[5..], [played(tag(1), 60)]);

    // And the caller was played what the session sent, in order.
    let heard = [first, more, rest, last].concat();
    assert_eq!(heard, [[1_u8; PIECE], [2; PIECE], [3; PIECE]].concat());
    assert_eq!(sim.holds(), 0);
    assert_eq!(sim.complaints(), [] as [String; 0]);
}

#[tokio::test]
async fn a_response_that_ends_without_audio_is_said_to_end() {
    let (mut line, mut sim) = call().await;
    line.send(tag(1), 0, 9, false);
    let ends = FrameOut {
        tag: tag(1),
        at: ms(20),
        length: ms(0),
        payload: &[],
        last: true,
    };
    line.transport.try_send(ends).unwrap();
    // No audio is sent that holds none: the end is a mark alone.
    let sent = line.sends(&mut sim, 4).await;
    assert_eq!(
        sent,
        [
            Sent::Mark("r0.0".to_owned()),
            Sent::Audio(Bytes::from(vec![9; PIECE])),
            Sent::Mark("r0.160".to_owned()),
            Sent::Mark("r0.160.end".to_owned()),
        ]
    );
    line.during(sim.plays(PIECE)).await.unwrap();
    line.until(4).await;
    assert_eq!(
        line.seen[1..],
        [played(tag(1), 0), played(tag(1), 20), played(tag(1), 20)]
    );
    assert_eq!(sim.complaints(), [] as [String; 0]);
}

#[tokio::test]
async fn what_a_carrier_throws_away_is_not_said_to_have_been_played() {
    let (mut line, mut sim) = call().await;
    for at in 0..5 {
        line.send(tag(1), at, 1, false);
    }
    line.sends(&mut sim, 11).await;
    line.during(sim.plays(2 * PIECE)).await.unwrap();
    line.until(4).await;
    assert_eq!(
        line.seen[1..],
        [played(tag(1), 0), played(tag(1), 20), played(tag(1), 40)]
    );

    // The caller speaks, and the session lets go of the response. The
    // carrier says back the three marks it throws away with the audio.
    let discard = TransportControl::Clear { tag: tag(1) };
    line.transport.try_control(discard).unwrap();
    assert_eq!(line.sends(&mut sim, 1).await, [Sent::Clear]);
    assert_eq!(sim.holds(), 0);

    // The next response is believed from where it begins.
    line.send(tag(2), 0, 2, true);
    let sent = line.sends(&mut sim, 3).await;
    assert_eq!(sent[0], Sent::Mark("r1.0".to_owned()));
    line.during(sim.plays(PIECE)).await.unwrap();
    line.until(6).await;
    // Nothing was said of the first response after it was let go of:
    // what the caller heard of it is 40 ms.
    assert_eq!(line.seen[4..], [played(tag(2), 0), played(tag(2), 20)]);
    assert_eq!(sim.complaints(), [] as [String; 0]);
}

#[tokio::test]
async fn what_was_left_of_an_earlier_response_goes_with_the_one_that_is_discarded() {
    let (mut line, mut sim) = call().await;
    // A response that was sent to its end, and the carrier has played
    // half of it when the next one is begun and then let go of.
    line.send(tag(1), 0, 1, false);
    line.send(tag(1), 1, 1, true);
    line.send(tag(2), 0, 2, false);
    line.send(tag(2), 1, 2, false);
    line.sends(&mut sim, 10).await;
    line.during(sim.plays(PIECE)).await.unwrap();
    line.until(3).await;
    assert_eq!(line.seen[1..], [played(tag(1), 0), played(tag(1), 20)]);

    let discard = TransportControl::Clear { tag: tag(2) };
    line.transport.try_control(discard).unwrap();
    assert_eq!(line.sends(&mut sim, 1).await, [Sent::Clear]);
    // The carrier said back the end of the first response and all of
    // the second. The caller heard neither.
    line.hears_nothing_before_a_key(&mut sim, Key::Zero).await;

    // A response that nothing was sent of has nothing to let go of: the
    // carrier is told nothing, and the next thing it is sent is audio.
    let nothing = TransportControl::Clear { tag: tag(9) };
    line.transport.try_control(nothing).unwrap();
    line.send(tag(3), 0, 3, true);
    let sent = line.sends(&mut sim, 3).await;
    assert_eq!(
        sent,
        [
            Sent::Mark("r2.0".to_owned()),
            Sent::Audio(Bytes::from(vec![3; PIECE])),
            Sent::Mark("r2.160.end".to_owned()),
        ]
    );
    assert_eq!(sim.complaints(), [] as [String; 0]);
}

#[tokio::test]
async fn a_mark_that_is_none_of_the_session_s_is_told_to_nobody() {
    let (mut line, mut sim) = call().await;
    line.send(tag(1), 0, 1, true);
    line.sends(&mut sim, 3).await;
    line.until(2).await;
    for none in [
        "my label",
        "",
        "r",
        "r0",
        "r0.",
        "r0.160.fin",
        "r-0.160",
        "r7.160",
    ] {
        line.during(sim.echoes(none)).await.unwrap();
    }
    line.hears_nothing_before_a_key(&mut sim, Key::Star).await;
    // One that says more than was sent says all of it, and no more.
    line.during(sim.echoes("r0.4000000000")).await.unwrap();
    line.until(4).await;
    assert_eq!(line.seen[3..], [played(tag(1), 20)]);
}

#[tokio::test]
async fn keys_arrive_as_keys_and_leave_as_tones_in_the_audio() {
    let (mut line, mut sim) = call().await;
    for (n, key) in "0123456789*#".chars().enumerate() {
        line.during(sim.presses(Key::from_char(key).unwrap()))
            .await
            .unwrap();
        line.until(n + 2).await;
        let pressed = TransportEvent::Dtmf {
            who: CALLER,
            key: Key::from_char(key).unwrap(),
        };
        assert_eq!(line.seen[n + 1], pressed);
    }

    // A key the session presses is a tone it makes. It is of no
    // response, so no mark follows it. Asking the transport to press
    // one in another way sends nothing.
    let tone = ToneOut {
        length: ms(20),
        payload: &[0x55; PIECE],
    };
    line.transport.try_send_tone(tone).unwrap();
    let press = TransportControl::Dtmf { key: Key::Nine };
    line.transport.try_control(press).unwrap();
    line.send(tag(1), 0, 1, true);
    let sent = line.sends(&mut sim, 4).await;
    assert_eq!(
        sent,
        [
            Sent::Audio(Bytes::from(vec![0x55; PIECE])),
            Sent::Mark("r0.0".to_owned()),
            Sent::Audio(Bytes::from(vec![1; PIECE])),
            Sent::Mark("r0.160.end".to_owned()),
        ]
    );
    // The response is behind the tone at the carrier: the caller is not
    // about to hear it until the tone has been played.
    line.hears_nothing_before_a_key(&mut sim, Key::One).await;
    line.during(sim.plays(PIECE)).await.unwrap();
    line.until(15).await;
    assert_eq!(line.seen[14..], [played(tag(1), 0)]);
    assert_eq!(sim.complaints(), [] as [String; 0]);
}

#[tokio::test]
async fn what_a_carrier_says_that_is_not_understood_does_not_end_the_call() {
    let (mut line, mut sim) = call().await;
    for odd in [
        "",
        "not a message",
        r#"{"event":"media","media":{"track":"inbound","payload":"not base64!"}}"#,
        r#"{"event":"transcript","text":"hello"}"#,
        r#"{"event":"connected","protocol":"Call","version":"1.0.0"}"#,
    ] {
        line.during(sim.tells(odd.to_owned())).await.unwrap();
    }
    line.during(sim.tells_bytes(Bytes::from_static(&[0x7f; PIECE])))
        .await
        .unwrap();
    // The beginning of the call, said again.
    line.during(sim.begins(&[])).await.unwrap();
    line.hears_nothing_before_a_key(&mut sim, Key::Hash).await;
    assert!(!line.closed);
}

#[tokio::test]
async fn a_carrier_that_is_gone_has_left_and_is_not_waited_for() {
    let (mut line, mut sim) = call().await;
    // No word that the call is over: the stream closes.
    line.during(sim.drops()).await;
    line.until(3).await;
    // A carrier does not bring a call back to a stream it left, so
    // nothing says that the line dropped.
    assert_eq!(
        line.seen,
        [
            TransportEvent::Joined { who: CALLER },
            TransportEvent::Left { who: CALLER }
        ]
    );
    assert!(line.closed);

    // What a session still gives a transport that is over is taken
    // without a word, and nobody is left to say goodbye to.
    let frame = FrameOut {
        tag: tag(1),
        at: ms(0),
        length: ms(20),
        payload: &[1; PIECE],
        last: true,
    };
    let tone = ToneOut {
        length: ms(20),
        payload: &[0x55; PIECE],
    };
    for _ in 0..200 {
        assert_eq!(line.transport.try_send(frame), Ok(()));
        assert_eq!(line.transport.try_send_tone(tone), Ok(()));
        let discard = TransportControl::Clear { tag: tag(1) };
        assert_eq!(line.transport.try_control(discard), Ok(()));
    }
    let said = poll_fn(|cx| line.transport.poll_close(cx));
    tokio::time::timeout(SOON, said).await.unwrap();
}

#[tokio::test]
async fn a_call_that_the_carrier_says_is_over_is_over() {
    let (mut line, mut sim) = call().await;
    // The carrier says so and leaves the stream as it is: the caller
    // has left all the same, and the stream is closed for it.
    line.during(sim.stops()).await.unwrap();
    assert_eq!(line.during(sim.next()).await, Ok(None));
    line.until(3).await;
    assert_eq!(
        line.seen,
        [
            TransportEvent::Joined { who: CALLER },
            TransportEvent::Left { who: CALLER }
        ]
    );
    assert!(line.closed);
}

#[tokio::test]
async fn a_session_that_ends_closes_the_stream_after_what_it_sent() {
    let (mut line, mut sim) = call().await;
    line.send(tag(1), 0, 4, true);
    let mut sent = Vec::new();
    {
        // What was sent before the end arrives before it, and then the
        // stream closes.
        let mut hung_up = pin!(async {
            while let Some(next) = sim.next().await.unwrap() {
                sent.push(next);
            }
        });
        let (mut said, mut heard) = (false, false);
        let both = poll_fn(|cx| {
            said = said || line.transport.poll_close(cx).is_ready();
            heard = heard || hung_up.as_mut().poll(cx).is_ready();
            if said && heard {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        tokio::time::timeout(SOON, both).await.unwrap();
    }
    assert_eq!(
        sent,
        [
            Sent::Mark("r0.0".to_owned()),
            Sent::Audio(Bytes::from(vec![4; PIECE])),
            Sent::Mark("r0.160.end".to_owned()),
        ]
    );
    // And the transport is over.
    line.until(usize::MAX).await;
    assert!(line.closed);
}

#[tokio::test]
async fn a_call_that_is_not_served_is_closed() {
    let (call, mut sim) = rings(&[]).await;
    let both = async { tokio::join!(call.refuse(), sim.next()) };
    let ((), heard) = tokio::time::timeout(SOON, both).await.unwrap();
    assert_eq!(heard, Ok(None));
}

/// A stream that begins as `begin` has the stand-in begin it, and is
/// not taken for a call within `patience`. Returns why, once the
/// stand-in has seen its stream closed: whatever was wrong, it is.
async fn no_call<F>(patience: Duration, begin: F) -> PortError
where
    F: AsyncFnOnce(&mut Sim),
{
    let (conn, mut sim) = connected().await;
    begin(&mut sim).await;
    let both = async { tokio::join!(Call::of(TWILIO, conn, patience), sim.next()) };
    let (call, closed) = tokio::time::timeout(SOON, both).await.unwrap();
    assert_eq!(closed, Ok(None));
    call.unwrap_err()
}

const START_OF_ALAW: &str = r#"{"event":"start","sequenceNumber":"1","start":{"accountSid":"AC1","streamSid":"MZ1","callSid":"CA1","tracks":["inbound"],"mediaFormat":{"encoding":"audio/x-alaw","sampleRate":8000,"channels":1}},"streamSid":"MZ1"}"#;

#[tokio::test]
async fn a_stream_that_does_not_begin_as_a_carrier_s_is_no_call() {
    // A carrier that says nothing is not waited for, and neither is one
    // that only says it is there.
    let brief = Duration::from_millis(50);
    let silent = no_call(brief, async |_| {}).await;
    assert_eq!(silent.class, RetryClass::Timeout);
    let hello = r#"{"event":"connected","protocol":"Call","version":"1.0.0"}"#;
    let only_there = no_call(brief, async |sim| {
        sim.tells(hello.to_owned()).await.unwrap();
    })
    .await;
    assert_eq!(only_there.class, RetryClass::Timeout);

    // Something of a call before the call is said to begin, audio that
    // is not a telephone's, and what is no message at all.
    for (wrong, why) in [
        (
            r#"{"event":"media","media":{"track":"inbound","payload":"no+J"}}"#,
            "which call",
        ),
        (r#"{"event":"mark","mark":{"name":"r0.160"}}"#, "which call"),
        (r#"{"event":"stop"}"#, "which call"),
        (START_OF_ALAW, "audio that is not taken"),
        ("not a message", "no message"),
    ] {
        let error = no_call(STARTS_WITHIN, async |sim| {
            sim.tells(wrong.to_owned()).await.unwrap();
        })
        .await;
        assert_eq!(error.class, RetryClass::Permanent, "{wrong}");
        assert!(error.message.contains(why), "{wrong}: {error}");
        // What a carrier said is not repeated to whoever reads a log.
        assert!(!error.message.contains("no+J"), "{error}");
    }
    let bytes = no_call(STARTS_WITHIN, async |sim| {
        sim.tells_bytes(Bytes::from_static(b"\x01\x02"))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(bytes.class, RetryClass::Permanent);

    // A carrier that leaves before the call begins may come again.
    let left = no_call(STARTS_WITHIN, async |sim| sim.drops().await).await;
    assert_eq!(left.class, RetryClass::Transient);

    // What a carrier may come to say before a call begins is passed
    // over, and the call begins.
    let (conn, mut sim) = connected().await;
    let news = r#"{"event":"region","name":"ie1"}"#;
    sim.tells(news.to_owned()).await.unwrap();
    sim.begins(&[]).await.unwrap();
    let call = Call::of(TWILIO, conn, STARTS_WITHIN).await.unwrap();
    assert_eq!(call.started().call, sim.ringing().call);
}

/// The name of the next mark a carrier says back.
async fn said_back(conn: &mut WsConn) -> String {
    let next = tokio::time::timeout(SOON, conn.next()).await.unwrap();
    let Some(WsMessage::Text(text)) = next.unwrap() else {
        panic!("a carrier's stream is text, and was not closed");
    };
    let said: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
    assert_eq!(said["event"], "mark");
    said["mark"]["name"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn the_stand_in_holds_and_plays_and_throws_away_as_a_carrier_does() {
    let (mut conn, mut sim) = connected().await;
    let stream = sim.ringing().stream.clone();
    let mark = |name: &str| {
        format!(r#"{{"event":"mark","streamSid":"{stream}","mark":{{"name":"{name}"}}}}"#)
    };
    // 160 samples of silence, as µ-law writes it.
    let media = format!(
        r#"{{"event":"media","streamSid":"{stream}","media":{{"payload":"{}"}}}}"#,
        "/".repeat(212) + "/w=="
    );
    let clear = format!(r#"{{"event":"clear","streamSid":"{stream}"}}"#);
    let script = [
        mark("first"),
        media.clone(),
        mark("second"),
        media.clone(),
        mark("third"),
        media,
        mark("fourth"),
    ];
    for message in script {
        conn.send_text(message).await.unwrap();
        sim.next().await.unwrap().unwrap();
    }
    assert_eq!(sim.holds(), 480);
    // "Twilio sends back a mark event with a matching name when the
    // audio ends (or if there is no audio buffered)."
    assert_eq!(said_back(&mut conn).await, "first");
    assert_eq!(sim.plays(100).await.unwrap().len(), 100);
    assert_eq!(sim.plays(60).await.unwrap().len(), 60);
    assert_eq!(said_back(&mut conn).await, "second");
    assert_eq!(sim.holds(), 320);
    // "This empties all buffered audio and causes any mark messages to
    // be sent back."
    conn.send_text(clear).await.unwrap();
    assert_eq!(sim.next().await, Ok(Some(Sent::Clear)));
    assert_eq!(sim.holds(), 0);
    assert_eq!(said_back(&mut conn).await, "third");
    assert_eq!(said_back(&mut conn).await, "fourth");
    assert_eq!(sim.plays(100).await.unwrap().len(), 0);

    // One that is told to wait says nothing back by itself, and says
    // what a test has it say.
    sim.waits_to_be_told();
    conn.send_text(mark("fifth")).await.unwrap();
    sim.next().await.unwrap().unwrap();
    sim.echoes("sixth").await.unwrap();
    assert_eq!(said_back(&mut conn).await, "sixth");
    assert_eq!(sim.complaints(), [] as [String; 0]);
}

#[tokio::test]
async fn the_stand_in_complains_of_what_a_carrier_warns_of() {
    let (mut conn, mut sim) = connected().await;
    let stream = sim.ringing().stream.clone();
    let wrong = [
        // A field the documentation does not show, at either depth.
        format!(
            r#"{{"event":"media","streamSid":"{stream}","media":{{"payload":"AAAA","track":"outbound"}}}}"#
        ),
        format!(r#"{{"event":"clear","streamSid":"{stream}","sequenceNumber":"3"}}"#),
        // A message it does not take, another stream, audio that is
        // not written as audio is, and what is no message.
        format!(r#"{{"event":"dtmf","streamSid":"{stream}","dtmf":{{"digit":"1"}}}}"#),
        r#"{"event":"mark","streamSid":"MZother","mark":{"name":"r0.160"}}"#.to_owned(),
        format!(
            r#"{{"event":"media","streamSid":"{stream}","media":{{"payload":"not base64!"}}}}"#
        ),
        r#"{"event":"mark","mark":{"name":"r0.160"}}"#.to_owned(),
        "not a message".to_owned(),
    ];
    for message in &wrong {
        conn.send_text(message.clone()).await.unwrap();
    }
    conn.send_binary(Bytes::from_static(&[1, 2, 3]))
        .await
        .unwrap();
    // It goes on to the next thing a carrier takes, and has played and
    // held none of the rest.
    let clear = format!(r#"{{"event":"clear","streamSid":"{stream}"}}"#);
    conn.send_text(clear).await.unwrap();
    assert_eq!(sim.next().await, Ok(Some(Sent::Clear)));
    assert_eq!(
        sim.complaints().len(),
        wrong.len() + 1,
        "{:?}",
        sim.complaints()
    );
    assert_eq!(sim.holds(), 0);

    // What it says is what the documentation shows a carrier saying,
    // each message numbered after the one before.
    sim.begins(&[("once".to_owned(), "a-made-up-ticket".to_owned())])
        .await
        .unwrap();
    sim.says(&[0xff; 80]).await.unwrap();
    sim.says(&[0xff; 80]).await.unwrap();
    sim.presses(Key::Hash).await.unwrap();
    sim.echoes("r0.160").await.unwrap();
    sim.hangs_up().await.unwrap();
    let mut said = Vec::new();
    while let Some(message) = conn.next().await.unwrap() {
        let WsMessage::Text(text) = message else {
            panic!("a carrier's stream is text");
        };
        said.push(serde_json::from_str::<serde_json::Value>(text.as_str()).unwrap());
    }
    let events: Vec<&str> = said
        .iter()
        .map(|said| said["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        events,
        [
            "connected",
            "start",
            "media",
            "media",
            "dtmf",
            "mark",
            "stop"
        ]
    );
    let numbers: Vec<&str> = said[1..]
        .iter()
        .map(|said| said["sequenceNumber"].as_str().unwrap())
        .collect();
    assert_eq!(numbers, ["1", "2", "3", "4", "5", "6"]);
    assert_eq!(
        said[1]["start"]["customParameters"]["once"],
        "a-made-up-ticket"
    );
    assert_eq!(said[1]["start"]["mediaFormat"]["encoding"], "audio/x-mulaw");
    // The second piece of audio begins 10 ms after the first.
    assert_eq!(said[2]["media"]["timestamp"], "0");
    assert_eq!(said[3]["media"]["timestamp"], "10");
    assert_eq!(said[3]["media"]["chunk"], "2");
    assert_eq!(said[4]["dtmf"]["digit"], "#");
    assert_eq!(said[5]["mark"]["name"], "r0.160");
    for said in &said[1..] {
        assert_eq!(said["streamSid"], stream.as_str());
    }
}
