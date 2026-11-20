//! Calls in memory, on a clock the tests move.

use core::time::Duration;
use std::collections::BTreeSet;
use std::net::SocketAddr;

use waveport_adapter_kit::client::{Hello, Welcome, write};
use waveport_domain::event::Key;
use waveport_domain::ids::{Epoch, GenId, OpTag};
use waveport_domain::time::MediaDur;
use waveport_protocol::client::v1::{ByeReason, ErrorCode, FromClient, FromServer};

use crate::call::{Call, Client, Setup, Step, Told, Unanswered};
use crate::crypto::certificate;
use crate::engine::{AGAIN, TURNS};
use crate::fragment;
use crate::loopback::{Pair, Weather};
use crate::peer::{Like, Offers, PeerHeard, PeerStep};
use crate::rtp::forged_check;

const FRAME: Duration = Duration::from_millis(20);

/// What a client of the protocol offers that offers as Chrome does.
const OFFERS: Offers = Offers {
    like: Like::Chrome,
    speaks: true,
    tcp: false,
    dtls13: false,
};

const fn tag(generation: u32) -> OpTag {
    OpTag {
        generation: GenId(generation),
        epoch: Epoch(1),
    }
}

/// An Opus packet of 20 ms that is like no other: its first byte says
/// what it is, and the rest which one.
fn frame(n: u32) -> Vec<u8> {
    let mut packet = vec![0xF8];
    packet.extend_from_slice(&n.to_be_bytes());
    packet.extend_from_slice(&[0x5A; 40]);
    packet
}

/// Which frame a packet is.
fn which(packet: &[u8]) -> u32 {
    u32::from_be_bytes(packet[1..5].try_into().unwrap())
}

impl Pair {
    /// A pair that is connected, with nothing said yet of what
    /// happened on the way.
    fn fresh(like: Like, client: Client) -> Self {
        let mut pair = Self::connected(like, client).unwrap();
        pair.run_for(Duration::from_millis(50));
        pair.told.clear();
        pair.heard.clear();
        pair
    }

    /// The client says the frame `n`, and a frame's time goes by.
    fn client_says(&mut self, n: u32) {
        assert!(self.peer.speak(self.now(), &frame(n)));
        self.run_for(FRAME);
    }

    /// The server sends the frame `n` of the response `tag`, and a
    /// frame's time goes by.
    fn server_sends(&mut self, tag: OpTag, n: u32, last: bool) {
        let now = self.now();
        self.call
            .frame(now, &mut self.runs, tag, 960, &frame(n), last);
        self.run_for(FRAME);
    }

    /// The audio the server was told of since it was last asked: the
    /// number, the place and the frame of each.
    fn audio(&mut self) -> Vec<(u16, u32, u32)> {
        let told = core::mem::take(&mut self.told);
        let mut audio = Vec::new();
        for told in told {
            match told {
                Told::Audio {
                    sequence,
                    timestamp,
                    payload,
                } => audio.push((sequence, timestamp, which(&payload))),
                other => self.told.push(other),
            }
        }
        audio
    }

    /// The audio the client heard since it was last asked: the place
    /// and the frame of each.
    fn heard_audio(&mut self) -> Vec<(u32, u32)> {
        let heard = core::mem::take(&mut self.heard);
        let mut audio = Vec::new();
        for heard in heard {
            match heard {
                PeerHeard::Audio { time, payload } => audio.push((time, which(&payload))),
                other => self.heard.push(other),
            }
        }
        audio
    }

    /// What the client was told in the protocol since it was last
    /// asked.
    fn heard_said(&mut self) -> Vec<FromServer> {
        let heard = core::mem::take(&mut self.heard);
        let mut said = Vec::new();
        for heard in heard {
            match heard {
                PeerHeard::Said(message) => said.push(message),
                other => self.heard.push(other),
            }
        }
        said
    }

    /// The client says `message`, and a moment goes by.
    fn client_tells(&mut self, message: &FromClient) {
        assert!(self.peer.say(message));
        self.run_for(Duration::from_millis(5));
    }
}

fn mark(run: u32, generation: u32, end: bool, ts: u32) -> FromServer {
    FromServer::Mark {
        run,
        generation,
        end,
        ts: Some(ts),
    }
}

#[test]
fn two_ends_connect_in_memory_as_each_browser_offers() {
    for like in [Like::Chrome, Like::Firefox] {
        for client in [Client::V1, Client::Plain] {
            let pair = Pair::connected(like, client).unwrap();
            assert_eq!(pair.told, [Told::Up], "{like:?} {client:?}");
            // The client is told of its channel when it has one.
            let open = pair.heard.contains(&PeerHeard::Open);
            assert_eq!(open, client == Client::V1, "{like:?} {client:?}");
            let (server, far) = (pair.call.kinds().unwrap(), pair.peer.kinds().unwrap());
            // Redundant audio where it was offered, and where it was
            // not, what was lost is asked for again: one or the other.
            assert_eq!(server.red.is_some(), like == Like::Chrome);
            assert_eq!(server.nack, like == Like::Firefox);
            assert_eq!((server.opus, server.red), (far.opus, far.red));
            // The keys on the clock of the audio where that is offered.
            let clock = if like == Like::Chrome { 48_000 } else { 8_000 };
            assert_eq!(server.keys.map(|(_, clock)| clock), Some(clock));
            assert_eq!(server.keys, far.keys);
        }
    }
}

#[test]
fn a_call_carries_what_its_two_ends_say_and_whose_audio_it_is() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    // The client says hello where the protocol is spoken, and is
    // welcomed there.
    let hello = Hello::new(None).said();
    pair.client_tells(&hello);
    assert_eq!(pair.told, [Told::Said(hello)]);
    pair.told.clear();
    let welcome = Welcome {
        session: "s-1".to_owned(),
        next_seq: 1,
        ..Welcome::default()
    };
    let welcome = welcome.said(None);
    assert!(pair.call.tell(&welcome));
    pair.run_for(Duration::from_millis(5));
    assert_eq!(pair.heard_said(), [welcome]);

    // What the client says arrives numbered one after the other, each
    // where the one before it ended.
    for n in 0..50 {
        pair.client_says(n);
    }
    let audio = pair.audio();
    assert_eq!(audio.len(), 50);
    let first = audio[0].1;
    for (n, (sequence, timestamp, frame)) in audio.into_iter().enumerate() {
        let n = u32::try_from(n).unwrap();
        assert_eq!(u32::from(sequence), n);
        assert_eq!(timestamp, first.wrapping_add(n * 960));
        assert_eq!(frame, n);
    }
    assert!(pair.told.is_empty(), "{:?}", pair.told);

    // A response: the client is told where in the call's audio it
    // begins, hears it, and is told where it ended.
    for n in 0..10 {
        pair.server_sends(tag(7), 100 + n, n == 9);
    }
    let audio = pair.heard_audio();
    let begins = audio[0].0;
    let places: Vec<u32> = (0..10).map(|n| begins.wrapping_add(n * 960)).collect();
    assert_eq!(
        audio.iter().map(|(time, _)| *time).collect::<Vec<_>>(),
        places
    );
    let frames: Vec<u32> = (100..110).collect();
    assert_eq!(
        audio.iter().map(|(_, frame)| *frame).collect::<Vec<_>>(),
        frames
    );
    let ends = begins.wrapping_add(9_600);
    assert_eq!(
        pair.heard_said(),
        [mark(0, 7, false, begins), mark(0, 7, true, ends)]
    );

    // The next one is another run. After a pause its place is as much
    // later as the pause was long, so that the browser does not play
    // it early.
    pair.run_for(Duration::from_secs(2));
    pair.server_sends(tag(8), 200, true);
    let audio = pair.heard_audio();
    let later = audio[0].0.wrapping_sub(ends);
    assert!((95_000..=97_000).contains(&later), "{later}");
    let (begins, ends) = (audio[0].0, audio[0].0.wrapping_add(960));
    assert_eq!(
        pair.heard_said(),
        [mark(1, 8, false, begins), mark(1, 8, true, ends)]
    );
    assert!(pair.heard.is_empty(), "{:?}", pair.heard);
}

/// How many of `sent` frames are missing from `got`, which holds each
/// frame once at most.
fn missing(got: &[u32], sent: u32) -> u32 {
    let distinct: BTreeSet<u32> = got.iter().copied().collect();
    assert_eq!(distinct.len(), got.len(), "a frame was handed on twice");
    sent - u32::try_from(got.len()).unwrap()
}

#[test]
fn redundancy_fills_what_a_network_loses_and_every_frame_keeps_its_place() {
    let sent = 2_000;
    let mut left = Vec::new();
    for like in [Like::Chrome, Like::Firefox] {
        let mut pair = Pair::fresh(like, Client::V1);
        pair.set_weather(Weather {
            lost_to_server: 0.1,
            lost_to_client: 0.1,
            seed: 20_261_117,
            ..Weather::default()
        });
        for n in 0..sent {
            pair.client_says(n);
        }
        let audio = pair.audio();
        // Whatever was lost, a frame that arrives has the number of its
        // place: by its own packet or beside a later one.
        let (first_number, _, first_frame) = audio[0];
        for (sequence, _, frame) in &audio {
            let place = frame - first_frame + u32::from(first_number);
            assert_eq!(u32::from(*sequence), place & 0xffff, "{like:?} {frame}");
        }
        let to_server = missing(&audio.iter().map(|a| a.2).collect::<Vec<_>>(), sent);

        for n in 0..sent {
            pair.server_sends(tag(1), n, false);
        }
        let heard = pair.heard_audio();
        let (first_time, first_frame) = heard[0];
        for (time, frame) in &heard {
            assert_eq!(*time, first_time.wrapping_add((frame - first_frame) * 960));
        }
        let to_client = missing(&heard.iter().map(|a| a.1).collect::<Vec<_>>(), sent);
        left.push((like, to_server, to_client));
    }
    let [(_, chrome_in, chrome_out), (_, firefox_in, firefox_out)] = left[..] else {
        panic!("two browsers");
    };
    // A frame is lost with redundancy when its packet and the next are
    // both lost: one in a hundred at this loss, and one in ten without.
    for with in [chrome_in, chrome_out] {
        assert!((5..=45).contains(&with), "{with}");
    }
    for without in [firefox_in, firefox_out] {
        assert!((150..=250).contains(&without), "{without}");
    }
}

fn played_at(ts: u32, delay_ms: u32) -> FromClient {
    FromClient::PlayedRtp {
        ts: Some(ts),
        emitted: None,
        concealed: None,
        delay_ms,
    }
}

fn played(tag: OpTag, millis: u32) -> Told {
    Told::Played {
        tag,
        played: MediaDur::from_millis(millis),
    }
}

#[test]
fn what_came_out_of_a_far_ends_buffer_is_heard_when_its_time_has_passed() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    for n in 0..5 {
        pair.server_sends(tag(1), n, n == 4);
    }
    let marks = pair.heard_said();
    let [
        FromServer::Mark {
            ts: Some(first), ..
        },
        FromServer::Mark { ts: Some(end), .. },
    ] = marks[..]
    else {
        panic!("{marks:?}");
    };
    assert_eq!(end, first.wrapping_add(4_800));
    // All of the response came out, and its output takes 40 ms over
    // it: 60 ms of it are heard.
    pair.client_tells(&played_at(end, 40));
    assert_eq!(pair.told, [played(tag(1), 60)]);
    pair.told.clear();
    // The call asks to be asked again when that time has come: nothing
    // else has it asked then.
    let now = pair.now();
    let Step::Wait(due) = pair.call.poll(now) else {
        panic!("the call has more to say or to send");
    };
    assert!(due <= now + Duration::from_millis(40), "{:?}", due - now);
    // The far end has nothing more to say of it. The rest is heard
    // when the 40 ms have passed, and is said once.
    pair.run_for(Duration::from_millis(30));
    assert_eq!(pair.told, []);
    pair.run_for(Duration::from_millis(10));
    assert_eq!(pair.told, [played(tag(1), 100)]);
    pair.told.clear();
    pair.run_for(Duration::from_secs(2));
    assert_eq!(pair.told, []);

    // What the far end says next is what holds: the time of what it
    // said before is not waited for.
    for n in 0..5 {
        pair.server_sends(tag(2), n, false);
    }
    let marks = pair.heard_said();
    let [
        FromServer::Mark {
            ts: Some(second), ..
        },
    ] = marks[..]
    else {
        panic!("{marks:?}");
    };
    // 40 ms out and 30 ms to go, and 60 ms out a moment later.
    pair.client_tells(&played_at(second.wrapping_add(1_920), 30));
    assert_eq!(pair.told, [played(tag(2), 10)]);
    pair.told.clear();
    pair.client_tells(&played_at(second.wrapping_add(2_880), 0));
    assert_eq!(pair.told, [played(tag(2), 60)]);
    pair.told.clear();
    pair.run_for(Duration::from_secs(1));
    assert_eq!(pair.told, []);
    // An output that says it takes longer than any does is held to a
    // second: what came out of it is heard then.
    pair.client_tells(&played_at(second.wrapping_add(4_800), 3_600_000));
    pair.run_for(Duration::from_millis(900));
    assert!(!pair.told.contains(&played(tag(2), 100)), "{:?}", pair.told);
    pair.run_for(Duration::from_millis(100));
    assert_eq!(pair.told.last(), Some(&played(tag(2), 100)));
}

#[test]
fn where_the_far_end_is_in_the_stream_says_how_much_of_which_response_it_played() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    for n in 0..5 {
        pair.server_sends(tag(1), n, n == 4);
    }
    pair.run_for(Duration::from_secs(1));
    for n in 0..5 {
        pair.server_sends(tag(2), n, false);
    }
    let marks = pair.heard_said();
    let [
        FromServer::Mark {
            ts: Some(first), ..
        },
        _,
        FromServer::Mark {
            ts: Some(second), ..
        },
    ] = marks[..]
    else {
        panic!("{marks:?}");
    };

    // By the time of the audio that was played last.
    pair.client_tells(&played_at(first.wrapping_add(1_920), 0));
    assert_eq!(pair.told, [played(tag(1), 40)]);
    // In the pause between the two, all of the first was played and
    // none of the second: that says nothing new once it was said.
    pair.told.clear();
    pair.client_tells(&played_at(first.wrapping_add(4_800), 0));
    assert_eq!(pair.told, [played(tag(1), 100)]);
    pair.told.clear();
    pair.client_tells(&played_at(first.wrapping_add(20_000), 0));
    pair.client_tells(&played_at(second, 0));
    assert_eq!(pair.told, []);
    // Into the second.
    pair.client_tells(&played_at(second.wrapping_add(960), 0));
    assert_eq!(pair.told, [played(tag(2), 20)]);
    pair.told.clear();
    // What came out of the buffer is heard later by as much as the far
    // end says: 60 ms out and 40 ms to go is 20 ms heard.
    pair.client_tells(&played_at(second.wrapping_add(2_880), 40));
    assert_eq!(pair.told, []);
    pair.client_tells(&played_at(second.wrapping_add(2_880), 0));
    assert_eq!(pair.told, [played(tag(2), 60)]);
    pair.told.clear();
    // No more was played than was sent, and a time before the call's
    // audio says nothing.
    pair.client_tells(&played_at(second.wrapping_add(480_000), 0));
    assert_eq!(pair.told, [played(tag(2), 100)]);
    pair.told.clear();
    pair.client_tells(&played_at(first.wrapping_sub(48_000), 0));
    assert_eq!(pair.told, []);

    // A browser that cannot say which audio it played says how much
    // came out, and how much of that it made up.
    let counted = |emitted, concealed| FromClient::PlayedRtp {
        ts: None,
        emitted: Some(emitted),
        concealed,
        delay_ms: 0,
    };
    pair.client_tells(&counted(4_800 + 1_920 + 5_000, Some(5_000)));
    assert_eq!(pair.told, [played(tag(2), 40)]);
    pair.told.clear();
    pair.client_tells(&counted(960, None));
    assert_eq!(pair.told, [played(tag(1), 20), played(tag(2), 0)]);
    pair.told.clear();
    // One that says neither says nothing.
    pair.client_tells(&FromClient::PlayedRtp {
        ts: None,
        emitted: None,
        concealed: Some(7),
        delay_ms: 0,
    });
    assert_eq!(pair.told, []);

    // What was played is not said by its run on a call, and what is no
    // message of the protocol is answered and not passed on.
    pair.client_tells(&FromClient::Played { run: 0, samples: 5 });
    let refused = |said: &[FromServer]| match said {
        [FromServer::Error { code, .. }] => *code,
        other => panic!("{other:?}"),
    };
    assert_eq!(refused(&pair.heard_said()), ErrorCode::Unsupported);
    pair.client_tells(&FromClient::Text {
        text: String::new(),
    });
    assert_eq!(refused(&pair.heard_said()), ErrorCode::Malformed);
    assert_eq!(pair.told, []);
    // What is the session's to hear is passed on.
    let typed = FromClient::Text {
        text: "What time is it?".to_owned(),
    };
    for said in [typed, FromClient::Interrupt, FromClient::Bye] {
        pair.client_tells(&said);
        assert_eq!(pair.told, [Told::Said(said)]);
        pair.told.clear();
    }
    assert_eq!(pair.heard_said(), []);
}

#[test]
fn keys_are_pressed_both_ways_beside_the_audio() {
    for like in [Like::Chrome, Like::Firefox] {
        let mut pair = Pair::fresh(like, Client::V1);
        // The client speaks all the while, and presses two keys.
        for n in 0..10 {
            pair.client_says(n);
        }
        let now = pair.now();
        assert!(pair.peer.press(now, Key::Seven));
        assert!(pair.peer.press(now, Key::Hash));
        for n in 10..50 {
            pair.client_says(n);
        }
        assert!(pair.peer.has_pressed());
        let audio = pair.audio();
        // The packets of a key take numbers of the stream. The audio
        // is numbered by its place, and has no holes for them.
        let numbers: Vec<u16> = audio.iter().map(|(number, ..)| *number).collect();
        assert_eq!(numbers, (0..50).collect::<Vec<u16>>(), "{like:?}");
        // Each key once, however often its packets said it.
        assert_eq!(pair.told, [Told::Key(Key::Seven), Told::Key(Key::Hash)]);
        pair.told.clear();

        // The session presses keys, while it says nothing and while it
        // speaks.
        assert!(pair.call.press(pair.now(), Key::One));
        pair.run_for(Duration::from_millis(400));
        let now = pair.now();
        assert!(pair.call.press(now, Key::Two));
        assert!(pair.call.press(now, Key::Two));
        for n in 0..40 {
            pair.server_sends(tag(1), n, false);
        }
        let keys = [Key::One, Key::Two, Key::Two].map(PeerHeard::Key);
        let frames: Vec<u32> = pair.heard_audio().iter().map(|(_, frame)| *frame).collect();
        assert_eq!(frames, (0..40).collect::<Vec<u32>>());
        pair.heard_said();
        assert_eq!(pair.heard, keys, "{like:?}");
    }
}

#[test]
fn no_two_keys_are_taken_for_one_whatever_was_said_between_them() {
    for like in [Like::Chrome, Like::Firefox] {
        // Keys on a line nothing was ever said on.
        let mut pair = Pair::fresh(like, Client::V1);
        let now = pair.now();
        for key in [Key::One, Key::Two, Key::Two] {
            assert!(pair.call.press(now, key));
            assert!(pair.peer.press(now, key));
        }
        pair.run_for(Duration::from_secs(1));
        let keys = [Key::One, Key::Two, Key::Two];
        assert_eq!(pair.told, keys.map(Told::Key), "{like:?}");
        assert_eq!(pair.heard, keys.map(PeerHeard::Key), "{like:?}");
        pair.told.clear();
        pair.heard.clear();

        // And after audio that was sent all at once, ahead of its
        // time, as a program does that has it all: the clock of the
        // audio is where the last of it ends, and stands there for as
        // long as it takes the time to catch up.
        let now = pair.now();
        for n in 0..10 {
            assert!(pair.peer.speak(now, &frame(n)));
        }
        for key in [Key::Four, Key::Two] {
            assert!(pair.peer.press(now, key));
        }
        pair.run_for(Duration::from_secs(1));
        assert_eq!(pair.audio().len(), 10);
        assert_eq!(pair.told, [Key::Four, Key::Two].map(Told::Key), "{like:?}");
    }
}

#[test]
fn a_response_that_is_discarded_is_said_to_be() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    // One nothing was sent of has nothing to discard.
    pair.call.clear(&mut pair.runs, tag(9));
    pair.run_for(FRAME);
    assert_eq!(pair.heard, []);
    pair.server_sends(tag(1), 0, false);
    pair.call.clear(&mut pair.runs, tag(1));
    pair.run_for(FRAME);
    let said = pair.heard_said();
    assert!(matches!(
        said[..],
        [
            FromServer::Mark {
                run: 0,
                end: false,
                ..
            },
            FromServer::Clear { run: 0 }
        ]
    ));
    // The next response is told apart from it, and its first packet
    // does not carry the frame that was discarded beside it.
    pair.server_sends(tag(2), 1, true);
    let said = pair.heard_said();
    assert!(matches!(
        said[..],
        [
            FromServer::Mark {
                run: 1,
                end: false,
                ..
            },
            FromServer::Mark {
                run: 1,
                end: true,
                ..
            }
        ]
    ));
    let frames: Vec<u32> = pair.heard_audio().iter().map(|(_, frame)| *frame).collect();
    assert_eq!(frames, [0, 1]);
    // A last frame that holds nothing ends its response all the same.
    pair.server_sends(tag(3), 2, false);
    let now = pair.now();
    pair.call.frame(now, &mut pair.runs, tag(3), 0, &[], true);
    pair.run_for(FRAME);
    let said = pair.heard_said();
    let [
        FromServer::Mark {
            run: 2,
            end: false,
            ts: Some(begins),
            ..
        },
        FromServer::Mark {
            run: 2,
            end: true,
            ts: ends,
            ..
        },
    ] = said[..]
    else {
        panic!("{said:?}");
    };
    // A response that ends with a frame of nothing ends where its
    // audio did.
    assert_eq!(ends, Some(begins.wrapping_add(960)));
}

#[test]
fn a_client_that_does_not_speak_the_protocol_is_told_nothing() {
    for like in [Like::Chrome, Like::Firefox] {
        let mut pair = Pair::fresh(like, Client::Plain);
        let welcome = Welcome::default().said(None);
        assert!(!pair.call.tell(&welcome));
        for n in 0..5 {
            pair.client_says(n);
            pair.server_sends(tag(1), n, n == 4);
        }
        pair.call.clear(&mut pair.runs, tag(1));
        pair.run_for(FRAME);
        assert_eq!(pair.audio().len(), 5);
        assert_eq!(pair.heard_audio().len(), 5);
        assert!(pair.told.is_empty() && pair.heard.is_empty());
        // It has no channel to say anything on.
        assert!(!pair.peer.say(&FromClient::Interrupt));
    }
}

#[test]
fn a_server_that_may_use_the_newer_handshake_connects_with_a_far_end_that_does_not() {
    // The setting is off unless it is set, and a browser is as likely
    // to speak the one as the other: every pairing has to connect.
    for (server13, client13) in [(false, false), (true, false), (false, true), (true, true)] {
        let offers = Offers {
            dtls13: client13,
            ..OFFERS
        };
        let begun = Pair::begun_as(offers, Client::V1, Weather::default(), server13);
        let Some(mut pair) = begun.and_then(Pair::connects) else {
            panic!("server {server13}, client {client13}");
        };
        pair.client_says(1);
        pair.server_sends(tag(1), 2, true);
        assert_eq!(pair.audio().len(), 1);
        assert_eq!(pair.heard_audio().len(), 1);
    }
}

#[test]
fn a_far_end_that_falls_silent_has_dropped_and_is_back_when_it_is_heard() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    // A far end that says nothing for a while, and is there, has not
    // dropped: its checks arrive.
    pair.run_for(Duration::from_secs(20));
    assert_eq!(pair.told, []);
    let cut = Weather {
        cut: true,
        ..Weather::default()
    };
    pair.set_weather(cut);
    pair.run_for(Duration::from_millis(4_900));
    assert_eq!(pair.told, []);
    pair.run_for(Duration::from_millis(200));
    assert_eq!(pair.told, [Told::Down]);
    // What is sent meanwhile goes nowhere, and does no harm.
    pair.server_sends(tag(1), 0, true);
    pair.told.clear();
    pair.set_weather(Weather::default());
    pair.run_for(Duration::from_secs(3));
    assert_eq!(pair.told, [Told::Up]);
    pair.told.clear();
    pair.heard.clear();
    pair.client_says(1);
    pair.server_sends(tag(2), 2, true);
    assert_eq!(pair.audio().len(), 1);
    let frames: Vec<u32> = pair.heard_audio().iter().map(|(_, frame)| *frame).collect();
    assert_eq!(frames, [2]);
}

#[test]
fn a_far_end_that_is_somewhere_else_offers_again_and_the_call_goes_on() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    for n in 0..5 {
        pair.client_says(n);
        pair.server_sends(tag(1), n, false);
    }
    let cut = Weather {
        cut: true,
        ..Weather::default()
    };
    pair.set_weather(cut);
    // Long enough for both ends to give each other up.
    pair.run_for(Duration::from_secs(30));
    assert_eq!(pair.told.last(), Some(&Told::Down));
    assert_eq!(pair.heard.last(), Some(&PeerHeard::Down));
    pair.set_weather(Weather::default());
    pair.run_for(Duration::from_secs(5));
    assert_eq!(pair.told.last(), Some(&Told::Down));

    // The client has another address, and offers again from there.
    let before = pair.call.name().to_owned();
    pair.client = "203.0.113.9:61000".parse().unwrap();
    let offer = pair.peer.offer_again(pair.client).unwrap();
    let answer = pair.call.answer_again(&offer).unwrap();
    assert!(pair.peer.answered(&answer));
    // Its checks are addressed to another name from here on, which is
    // how they find the call.
    assert_ne!(pair.call.name(), before);
    pair.told.clear();
    pair.heard.clear();
    pair.run_for(Duration::from_secs(2));
    assert_eq!(pair.told, [Told::Up]);
    assert_eq!(pair.heard, [PeerHeard::Up]);
    pair.told.clear();
    pair.heard.clear();
    // The call is the one it was: no new handshake, the same channel,
    // and audio that is numbered on from where it was.
    for n in 5..10 {
        pair.client_says(n);
        pair.server_sends(tag(1), n, n == 9);
    }
    let frames: Vec<u32> = pair.audio().iter().map(|(.., frame)| *frame).collect();
    assert_eq!(frames, [5, 6, 7, 8, 9]);
    let frames: Vec<u32> = pair.heard_audio().iter().map(|(_, frame)| *frame).collect();
    assert_eq!(frames, [5, 6, 7, 8, 9]);
    assert!(matches!(
        pair.heard_said()[..],
        [FromServer::Mark { end: true, .. }]
    ));
    // An offer that is none leaves the call as it is.
    assert_eq!(
        pair.call.answer_again("v=0\r\n"),
        Err(Unanswered::Unreadable)
    );
    pair.client_says(10);
    assert_eq!(pair.audio().len(), 1);
}

#[test]
fn a_call_that_either_end_closes_is_over_for_both() {
    for server_closes in [true, false] {
        let mut pair = Pair::fresh(Like::Chrome, Client::V1);
        pair.server_sends(tag(1), 0, true);
        pair.heard.clear();
        // What is said last is said before the end.
        let bye = FromServer::Bye {
            reason: ByeReason::Ended,
        };
        if server_closes {
            assert!(pair.call.tell(&bye));
            pair.call.close();
        } else {
            pair.client_tells(&FromClient::Bye);
            pair.peer.close();
        }
        pair.run_for(Duration::from_secs(1));
        if server_closes {
            assert_eq!(pair.heard, [PeerHeard::Said(bye), PeerHeard::Over]);
            assert_eq!(pair.told, [Told::Over]);
        } else {
            assert_eq!(pair.told, [Told::Said(FromClient::Bye), Told::Over]);
            assert_eq!(pair.heard, [PeerHeard::Over]);
        }
        assert!(pair.call.is_over() && pair.peer.is_over());
        // A call that is over takes what it is given without a word.
        pair.told.clear();
        pair.heard.clear();
        let now = pair.now();
        pair.call
            .frame(now, &mut pair.runs, tag(2), 960, &frame(1), true);
        pair.call.clear(&mut pair.runs, tag(2));
        pair.call.close();
        assert!(!pair.call.tell(&FromServer::Clear { run: 0 }));
        assert!(!pair.call.press(now, Key::One));
        assert!(!pair.peer.speak(now, &frame(1)));
        assert!(!pair.peer.say(&FromClient::Interrupt));
        assert_eq!(pair.call.answer_again("v=0"), Err(Unanswered::Over));
        pair.run_for(Duration::from_secs(20));
        assert!(pair.told.is_empty() && pair.heard.is_empty());
    }
}

#[test]
fn the_far_end_says_how_much_of_what_it_was_sent_did_not_arrive() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    let lost = |pair: &mut Pair| core::mem::take(&mut pair.lost);
    // Ten seconds on a clean line: nothing is lost, and that is said.
    for n in 0..500 {
        pair.server_sends(tag(1), n, false);
    }
    let clean = lost(&mut pair);
    assert!(
        !clean.is_empty() && clean.iter().all(|lost| *lost == 0),
        "{clean:?}"
    );
    // A line that loses one in ten: the reports say about as much.
    pair.set_weather(Weather {
        lost_to_client: 0.1,
        seed: 3,
        ..Weather::default()
    });
    // What the far end says of every packet comes every second, long
    // before a report does: a line that begins to lose is said to
    // within a few of them.
    for n in 500..700 {
        pair.server_sends(tag(1), n, false);
    }
    let soon = lost(&mut pair);
    assert!(
        soon.len() >= 3 && soon.iter().any(|lost| *lost > 0),
        "{soon:?}"
    );
    for n in 700..2_000 {
        pair.server_sends(tag(1), n, false);
    }
    let lossy = lost(&mut pair);
    // Once a receiver report has counted them. Before that, this far
    // end says less than it lost: what it says of every packet leaves
    // out the ones between two of its messages.
    let settled = &lossy[lossy.len() / 2..];
    let mean = settled.iter().map(|lost| u32::from(*lost)).sum::<u32>()
        / u32::try_from(settled.len()).unwrap();
    assert!((7..=13).contains(&mean), "{lossy:?}");
}

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/offers/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path).unwrap()
}

/// The payload types an answer names for its audio, in its order, and
/// the lines that say what each is.
fn audio_of(answer: &str) -> (String, Vec<&str>) {
    let line = answer
        .lines()
        .find(|line| line.starts_with("m=audio"))
        .unwrap();
    let kinds = line.split(' ').skip(3).collect::<Vec<_>>().join(" ");
    let about = |line: &&str| {
        ["a=rtpmap:", "a=rtcp-fb:", "a=fmtp:"]
            .iter()
            .any(|what| line.starts_with(what))
    };
    (kinds, answer.lines().filter(about).collect())
}

#[test]
fn what_two_browsers_offer_is_answered_as_was_decided() {
    let certificate = certificate().unwrap();
    let setup = |client| Setup {
        certificate: &certificate,
        dtls13: false,
        client,
        udp: Some("192.0.2.10:40000".parse().unwrap()),
        tcp: Some("192.0.2.10:40000".parse().unwrap()),
    };
    let now = std::time::Instant::now();
    let (call, answer) = Call::answer(&setup(Client::V1), &fixture("chrome.sdp"), now).unwrap();
    // Redundant audio first, so that Chrome sends it; nothing asked for
    // again; the keys on the clock of the audio and on no other.
    let (kinds, about) = audio_of(&answer);
    assert_eq!(kinds, "63 111 110");
    assert_eq!(
        about,
        [
            "a=rtpmap:111 opus/48000/2",
            "a=rtcp-fb:111 transport-cc",
            "a=fmtp:111 minptime=10;useinbandfec=1",
            "a=rtpmap:63 red/48000/2",
            "a=fmtp:63 111/111",
            "a=rtpmap:110 telephone-event/48000",
            "a=fmtp:110 0-16",
        ]
    );
    // One address, reached over UDP and, where that is blocked, over a
    // TCP connection the browser makes; the handshake is the browser's
    // to begin; and the checks of ICE are only answered.
    let candidates: Vec<&str> = answer
        .lines()
        .filter(|l| l.starts_with("a=candidate"))
        .collect();
    assert_eq!(candidates.len(), 2, "{candidates:?}");
    assert!(candidates[0].contains(" udp ") && candidates[0].contains("192.0.2.10 40000 typ host"));
    assert!(
        candidates[1].contains(" tcp ")
            && candidates[1].contains("192.0.2.10 40000 typ host tcptype passive"),
        "{candidates:?}"
    );
    assert!(answer.contains("a=ice-lite\r\n"));
    assert_eq!(answer.matches("a=setup:passive\r\n").count(), 2);
    assert!(answer.contains("m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n"));
    // The name its checks will be addressed to is the one the answer
    // says.
    assert!(answer.contains(&format!("a=ice-ufrag:{}\r\n", call.name())));

    let (_, answer) = Call::answer(&setup(Client::Plain), &fixture("firefox.sdp"), now).unwrap();
    // No redundant audio was offered, so what is lost is asked for
    // again; and the keys are on the one clock that was offered.
    let (kinds, about) = audio_of(&answer);
    assert_eq!(kinds, "109 101");
    assert_eq!(
        about,
        [
            "a=rtpmap:109 opus/48000/2",
            "a=rtcp-fb:109 transport-cc",
            "a=rtcp-fb:109 nack",
            "a=fmtp:109 minptime=10;useinbandfec=1",
            "a=rtpmap:101 telephone-event/8000",
            "a=fmtp:101 0-15",
        ]
    );

    // An offer that cannot be answered, and what is said of it, which
    // repeats nothing of it.
    let no_audio = fixture("chrome.sdp").replace("m=audio", "m=video");
    let no_channel = fixture("chrome.sdp").replace("BUNDLE 0 1", "BUNDLE 0");
    let no_channel = no_channel.split("m=application").next().unwrap();
    for (offer, client, why) in [
        (no_audio.as_str(), Client::V1, Unanswered::NoAudio),
        (no_channel, Client::V1, Unanswered::NoChannel),
        ("wpc1_madeup", Client::Plain, Unanswered::NoAudio),
        (
            "m=audio wpc1_madeup\r\n",
            Client::Plain,
            Unanswered::Unreadable,
        ),
    ] {
        let refused = Call::answer(&setup(client), offer, now).unwrap_err();
        assert_eq!(refused, why);
        assert!(!refused.to_string().contains("madeup"));
    }
    // A client that does not speak the protocol needs no channel.
    assert!(Call::answer(&setup(Client::Plain), no_channel, now).is_ok());
}

/// The packet of audio among `datagrams`, which are sealed but for
/// their headers: whether it is marked as the first after a break, and
/// how long it is.
fn packet_of(datagrams: &[Vec<u8>]) -> (bool, usize) {
    let packet = audio_in(datagrams);
    (packet[1] & 0x80 != 0, packet.len())
}

/// The one packet of the stream among `datagrams`.
fn audio_in(datagrams: &[Vec<u8>]) -> &[u8] {
    let audio = |datagram: &&Vec<u8>| {
        let is_report = |kind: u8| (192..=223).contains(&kind);
        matches!(datagram[..], [128..=191, kind, ..] if !is_report(kind))
    };
    let packets: Vec<&Vec<u8>> = datagrams.iter().filter(audio).collect();
    assert_eq!(packets.len(), 1, "{datagrams:?}");
    packets[0]
}

#[test]
fn the_first_packet_after_a_break_says_so_and_carries_nothing_from_before_it() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    let sent = |pair: &mut Pair, n: u32| {
        let made = pair.send(&frame(n));
        pair.tick();
        packet_of(&made)
    };
    let first = sent(&mut pair, 0);
    let second = sent(&mut pair, 1);
    let third = sent(&mut pair, 2);
    // A break, and a short one: the frame before it is still near
    // enough to be put beside the next.
    pair.run_for(Duration::from_millis(200));
    let after = sent(&mut pair, 3);
    let next = sent(&mut pair, 4);
    // A browser takes the mark for where speech begins anew.
    assert_eq!(
        [first.0, second.0, third.0, after.0, next.0],
        [true, false, false, true, false]
    );
    // Every packet but the first of a stretch carries the frame before
    // it: its 45 bytes, and four that say where it belongs.
    assert_eq!(second.1, first.1 + 49);
    assert_eq!([third.1, after.1, next.1], [second.1, first.1, second.1]);
}

#[test]
fn keys_wait_their_turn_and_no_more_wait_than_there_is_room_for() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    let began = pair.now();
    let pressed: Vec<bool> = (0..40).map(|_| pair.call.press(began, Key::Five)).collect();
    let room: Vec<bool> = (0..40).map(|n| n < 32).collect();
    assert_eq!(pressed, room);
    // Each is heard when it begins: a key is held for 160 ms and
    // followed by 80 ms of none, so that two of the same are two.
    let mut at = Vec::new();
    for _ in 0..100 {
        pair.run_for(Duration::from_millis(10));
        let now = pair.now();
        for heard in pair.heard.drain(..) {
            assert_eq!(heard, PeerHeard::Key(Key::Five));
            at.push(now.saturating_duration_since(began));
        }
    }
    assert_eq!(at.len(), 5, "{at:?}");
    for pair in at.windows(2) {
        // To within the 10 ms that were looked at a time.
        let apart = pair[1].saturating_sub(pair[0]).as_millis();
        assert!((230..=240).contains(&apart), "{at:?}");
    }
}

#[test]
fn what_a_client_is_told_before_its_channel_is_open_is_told_when_it_is() {
    let mut pair = Pair::begun(Like::Chrome, Client::V1, Weather::default()).unwrap();
    let told = |n| FromServer::Gap { resumes: n };
    // A far end that was told nothing has all it was told.
    assert!(pair.call.has_told_all());
    // As much as there is room for is kept, and no more.
    let kept: Vec<bool> = (0..40).map(|n| pair.call.tell(&told(n))).collect();
    let room: Vec<bool> = (0..40).map(|n| n < 32).collect();
    assert_eq!(kept, room);
    assert!(!pair.call.has_told_all());
    let mut pair = pair.connects().unwrap();
    pair.run_for(Duration::from_millis(200));
    let all: Vec<FromServer> = (0..32).map(told).collect();
    assert_eq!(pair.heard_said(), all);
    // It has it when it has said so, which a far end does a moment
    // after it has it.
    pair.run_for(Duration::from_millis(300));
    assert!(pair.call.has_told_all());
}

#[test]
fn what_is_said_and_is_no_message_is_answered_with_what_was_wrong() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    let malformed = |pair: &mut Pair| {
        pair.run_for(Duration::from_millis(5));
        assert_eq!(pair.told, []);
        let said = pair.heard_said();
        let [FromServer::Error { code, .. }] = said[..] else {
            panic!("{said:?}");
        };
        assert_eq!(code, ErrorCode::Malformed);
    };
    // A message of the protocol is text: the same bytes sent as
    // anything else are none.
    let hello = write(&Hello::new(None).said());
    assert!(pair.peer.say_raw(true, hello.as_bytes()));
    malformed(&mut pair);
    assert!(pair.peer.say_raw(false, b"{"));
    malformed(&mut pair);
    assert!(pair.peer.say_raw(false, &[0xff, 0xfe]));
    malformed(&mut pair);
    // The call goes on, and what is said next is heard.
    assert!(pair.peer.say_raw(false, hello.as_bytes()));
    pair.run_for(Duration::from_millis(5));
    assert_eq!(pair.told, [Told::Said(Hello::new(None).said())]);
}

#[test]
fn audio_that_comes_after_its_response_was_ended_says_again_whose_it_is() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    let begins = |message: &FromServer, of: u32| matches!(message, FromServer::Mark { run, end: false, .. } if *run == of);
    // A frame that was on its way when its response was discarded.
    pair.server_sends(tag(1), 0, false);
    pair.call.clear(&mut pair.runs, tag(1));
    pair.server_sends(tag(1), 1, false);
    let said = pair.heard_said();
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(begins(&said[0], 0) && begins(&said[2], 0), "{said:?}");
    assert_eq!(said[1], FromServer::Clear { run: 0 });
    // One that comes after the last of its response.
    pair.server_sends(tag(2), 2, true);
    pair.server_sends(tag(2), 3, false);
    let said = pair.heard_said();
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(begins(&said[0], 1) && begins(&said[2], 1), "{said:?}");
    assert!(matches!(
        said[1],
        FromServer::Mark {
            run: 1,
            end: true,
            ..
        }
    ));
    // A response that ends and of which nothing was sent ends nowhere
    // in the stream, and not where the one before it did.
    let now = pair.now();
    pair.call.frame(now, &mut pair.runs, tag(3), 0, &[], true);
    pair.run_for(FRAME);
    let ended = FromServer::Mark {
        run: 2,
        generation: 3,
        end: true,
        ts: None,
    };
    assert_eq!(pair.heard_said(), [ended]);
}

#[test]
fn a_far_end_that_offers_audio_and_nothing_else_is_answered_with_that() {
    for client in [Client::V1, Client::Plain] {
        let mut pair = Pair::fresh(Like::Bare, client);
        let kinds = pair.call.kinds().unwrap();
        assert_eq!((kinds.red, kinds.keys), (None, None), "{client:?}");
        assert!(kinds.nack);
        // No key can be pressed at it, and none it could press is
        // waited for.
        assert!(!pair.call.press(pair.now(), Key::One));
        assert!(!pair.peer.press(pair.now(), Key::One));
        pair.client_says(1);
        pair.server_sends(tag(1), 2, true);
        assert_eq!(pair.audio().len(), 1);
        let frames: Vec<u32> = pair.heard_audio().iter().map(|(_, frame)| *frame).collect();
        assert_eq!(frames, [2]);
        // The tone of a key is audio like any other.
        let now = pair.now();
        pair.call.tone(now, 960, &frame(3));
        pair.run_for(FRAME);
        let frames: Vec<u32> = pair.heard_audio().iter().map(|(_, frame)| *frame).collect();
        assert_eq!(frames, [3]);
    }
}

/// The time in the stream that a packet says in its header, which is
/// not sealed.
fn time_of(datagram: &[u8]) -> u32 {
    u32::from_be_bytes(datagram[4..8].try_into().unwrap())
}

#[test]
fn a_key_is_placed_in_the_stream_where_its_moment_is() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    let (keys, _) = pair.call.kinds().unwrap().keys.unwrap();
    let key_times = |made: Vec<Vec<u8>>| {
        let of_a_key = |datagram: &&Vec<u8>| matches!(datagram[..], [128..=191, kind, ..] if kind & 0x7f == *keys);
        let times: BTreeSet<u32> = made.iter().filter(of_a_key).map(|d| time_of(d)).collect();
        times.into_iter().collect::<Vec<u32>>()
    };
    // Half a second of a response is sent at once, ahead of its time,
    // and a key is pressed in the same moment: it is where the
    // response begins, and not after all of it.
    let begins = time_of(audio_in(&pair.send(&frame(0))));
    for n in 1..25 {
        pair.send(&frame(n));
    }
    assert!(pair.call.press(pair.now(), Key::One));
    let pressed = key_times(pair.made_in(Duration::from_millis(400)));
    assert_eq!(pressed, [begins]);
    // Long after the response is over: as much later in the stream as
    // it is later. Two seconds after the first, to the 20 ms a key is
    // looked at in.
    pair.run_for(Duration::from_millis(1_600));
    assert!(pair.call.press(pair.now(), Key::Two));
    let pressed = key_times(pair.made_in(Duration::from_millis(400)));
    let [pressed] = pressed[..] else {
        panic!("{pressed:?}");
    };
    let later = pressed.wrapping_sub(begins);
    assert!((96_000..=96_960).contains(&later), "{later}");
}

#[test]
fn what_a_far_end_of_no_protocol_says_on_a_channel_is_not_heard() {
    // It offers the channel and speaks on it. It was answered as one
    // that speaks no protocol, so nothing is agreed on.
    let begun = Pair::begun_as(OFFERS, Client::Plain, Weather::default(), false);
    let mut pair = begun.and_then(Pair::connects).unwrap();
    pair.run_for(Duration::from_millis(200));
    pair.told.clear();
    pair.heard.clear();
    assert!(pair.peer.say(&Hello::new(None).said()));
    pair.run_for(Duration::from_millis(200));
    assert_eq!(pair.told, []);
    assert_eq!(pair.heard, []);
}

/// Somewhere the far end is not, in a range that is set aside for
/// examples (RFC 5737).
fn elsewhere() -> SocketAddr {
    SocketAddr::from(([203, 0, 113, 9], 4_000))
}

/// The first word of a handshake, as anybody may send it: a record of
/// DTLS 1.2 that holds a hello.
fn forged_hello() -> Vec<u8> {
    let mut record = vec![22, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 48];
    record.extend_from_slice(&[1; 48]);
    record
}

/// A packet of audio, as anybody may send it.
fn forged_audio() -> Vec<u8> {
    let mut packet = vec![0x80, 111, 0, 1, 0, 0, 3, 0xC0, 1, 2, 3, 4];
    packet.extend_from_slice(&frame(1));
    packet
}

#[test]
fn what_is_not_the_far_ends_is_not_shown_to_a_call() {
    for like in [Like::Chrome, Like::Firefox, Like::Bare] {
        let client = if like == Like::Bare {
            Client::Plain
        } else {
            Client::V1
        };
        // Nothing of the far end's has arrived yet.
        let cut = Weather {
            cut: true,
            ..Weather::default()
        };
        let mut pair = Pair::begun(like, client, cut).unwrap();
        let name = pair.call.name().to_owned();
        let far = pair.client;
        // Somebody who knows the name of a call and not its password:
        // their check is not the call's, and neither is the handshake
        // they begin, from wherever they send it. The far end's own
        // handshake finds its place free.
        assert!(
            !pair.intrudes(elsewhere(), &forged_check(&name)),
            "{like:?}"
        );
        assert!(!pair.intrudes(elsewhere(), &forged_hello()), "{like:?}");
        assert!(!pair.intrudes(far, &forged_hello()), "{like:?}");
        pair.set_weather(Weather::default());
        let mut pair = pair.connects().unwrap();
        // Nothing the two ends sent each other on the way was taken
        // for somebody else's.
        assert_eq!(pair.refused, 0, "{like:?}");
        // On a call that is up, what comes from where the far end is
        // is the call's to look at, and what comes from elsewhere is
        // not.
        assert!(
            !pair.intrudes(elsewhere(), &forged_check(&name)),
            "{like:?}"
        );
        assert!(!pair.intrudes(elsewhere(), &forged_audio()), "{like:?}");
        assert!(!pair.intrudes(elsewhere(), &forged_hello()), "{like:?}");
        assert!(pair.intrudes(far, &forged_audio()), "{like:?}");
        // What is no datagram of a call at all is nobody's.
        assert!(!pair.intrudes(far, &[]), "{like:?}");
        pair.run_for(Duration::from_secs(2));
        assert_eq!(pair.refused, 0, "{like:?}");
    }
    // On a network that takes its time, too: an end's handshake comes
    // after the checks that say where it is.
    let slow = Weather {
        delay: Duration::from_millis(120),
        ..Weather::default()
    };
    for like in [Like::Chrome, Like::Firefox] {
        let pair = Pair::begun(like, Client::V1, slow).unwrap();
        let pair = pair.connects().unwrap();
        assert_eq!(pair.refused, 0, "{like:?}");
    }
}

#[test]
fn a_far_end_that_fell_silent_is_not_heard_because_somebody_else_is() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    let name = pair.call.name().to_owned();
    let cut = Weather {
        cut: true,
        ..Weather::default()
    };
    pair.set_weather(cut);
    // Whoever knows the name of the call sends it checks and audio all
    // the while. The line has dropped as soon as it would have with
    // nobody sending anything.
    for _ in 0..49 {
        pair.run_for(Duration::from_millis(100));
        assert!(!pair.intrudes(elsewhere(), &forged_check(&name)));
        assert!(!pair.intrudes(elsewhere(), &forged_audio()));
    }
    assert_eq!(pair.told, []);
    pair.run_for(Duration::from_millis(200));
    assert_eq!(pair.told, [Told::Down]);
    // And it is not back because they go on.
    for _ in 0..20 {
        pair.run_for(Duration::from_millis(100));
        assert!(!pair.intrudes(elsewhere(), &forged_check(&name)));
    }
    assert_eq!(pair.told, [Told::Down]);
}

#[test]
fn what_a_channel_has_no_room_for_is_said_not_to_be_told() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    // More at one moment than a channel holds: nothing of it has been
    // said to have arrived, so nothing makes room.
    let long = |n: u32| FromServer::Error {
        code: ErrorCode::Malformed,
        message: format!("{n:04}{}", "x".repeat(8_000)),
    };
    let told: Vec<bool> = (0..40).map(|n| pair.call.tell(&long(n))).collect();
    let taken = told.iter().filter(|taken| **taken).count();
    // Some of it is taken and some is not, and once there is no room
    // there is none for what comes after.
    assert!((8..40).contains(&taken), "{told:?}");
    assert!(told.iter().skip(taken).all(|taken| !taken), "{told:?}");
    assert!(!pair.call.has_told_all());
    // What was said to be taken is what the far end is told, all of
    // it and in its order, and what was said not to be is not.
    pair.run_for(Duration::from_secs(5));
    let all: Vec<FromServer> = (0..u32::try_from(taken).unwrap()).map(long).collect();
    assert_eq!(pair.heard_said(), all);
    assert!(pair.call.has_told_all());
    // There is room again.
    assert!(pair.call.tell(&long(99)));

    // The same the other way.
    pair.run_for(Duration::from_secs(1));
    pair.told.clear();
    assert!(pair.peer.has_said_all());
    let long = |n: u32| FromClient::Text {
        text: format!("{n:04}{}", "x".repeat(8_000)),
    };
    let said: Vec<bool> = (0..40).map(|n| pair.peer.say(&long(n))).collect();
    let taken = said.iter().filter(|taken| **taken).count();
    assert!((8..40).contains(&taken), "{said:?}");
    assert!(said.iter().skip(taken).all(|taken| !taken), "{said:?}");
    assert!(!pair.peer.has_said_all());
    pair.run_for(Duration::from_secs(5));
    let all: Vec<Told> = (0..u32::try_from(taken).unwrap())
        .map(|n| Told::Said(long(n)))
        .collect();
    assert_eq!(pair.told, all);
    assert!(pair.peer.has_said_all());
    // An end whose call is over waits for nothing to arrive.
    assert!(pair.peer.say(&long(99)));
    assert!(!pair.peer.has_said_all());
    pair.peer.close();
    assert!(pair.peer.has_said_all());
    assert!(pair.call.tell(&FromServer::Gap { resumes: 1 }));
    assert!(!pair.call.has_told_all());
    pair.call.close();
    assert!(pair.call.has_told_all());
}

#[test]
fn an_end_whose_library_does_not_move_on_gives_the_thread_back() {
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    let now = pair.now();
    // The library says a thousand times over that its time is now. The
    // call asks it so many times, and then says when it wants to be
    // asked again, which is later than now: whoever runs it has other
    // calls.
    let before = pair.call.stalls(1_000);
    let wait = loop {
        if let Step::Wait(until) = pair.call.poll(now) {
            break until;
        }
    };
    assert_eq!(wait, now + AGAIN);
    assert_eq!(pair.call.stalls(0) - before, TURNS);
    // And so does the other end.
    let before = pair.peer.stalls(1_000);
    let wait = loop {
        if let PeerStep::Wait(until) = pair.peer.poll(now) {
            break until;
        }
    };
    assert_eq!(wait, now + AGAIN);
    assert_eq!(pair.peer.stalls(0) - before, TURNS);
    // The call goes on when the library does.
    pair.told.clear();
    pair.client_says(1);
    assert_eq!(pair.audio().len(), 1);
}

/// A pair whose handshake is done at the server, on a network that
/// takes its time: the client has not heard so yet, and a channel that
/// was agreed on is still being set up.
fn half_way(client: Client) -> Pair {
    let slow = Weather {
        delay: Duration::from_millis(10),
        ..Weather::default()
    };
    let mut pair = Pair::begun(Like::Chrome, client, slow).unwrap();
    while !pair.told.contains(&Told::Up) {
        pair.run_for(Duration::from_millis(1));
    }
    assert!(!pair.heard.contains(&PeerHeard::Open));
    pair
}

#[test]
fn a_call_that_is_ended_while_it_is_set_up_is_over_for_whoever_ended_it() {
    for client in [Client::V1, Client::Plain] {
        // The server ends it. The client is told, whatever is still
        // being set up.
        let mut pair = half_way(client);
        pair.told.clear();
        pair.call.close();
        pair.run_for(Duration::from_millis(1));
        assert_eq!(pair.told, [Told::Over], "{client:?}");
        pair.run_for(Duration::from_millis(100));
        assert!(pair.heard.contains(&PeerHeard::Over), "{client:?}");

        // The client ends it before it has heard that the handshake is
        // done. Its call is over at once. The server is told nothing,
        // and finds the line dropped.
        let mut pair = half_way(client);
        pair.told.clear();
        pair.peer.close();
        pair.run_for(Duration::from_millis(1));
        assert_eq!(pair.heard, [PeerHeard::Over], "{client:?}");
        pair.run_for(Duration::from_secs(6));
        assert_eq!(pair.told, [Told::Down], "{client:?}");
    }
    // The client ends it when the handshake is done and its channel is
    // still being set up. `str0m` ends no such call: it says that it
    // cannot, and has told the far end nothing. The call is over at
    // once all the same, and is not left half ended.
    let mut pair = half_way(Client::V1);
    pair.told.clear();
    pair.run_for(Duration::from_millis(25));
    assert_eq!(pair.heard, [PeerHeard::Up]);
    pair.peer.close();
    pair.run_for(Duration::from_millis(1));
    assert_eq!(pair.heard, [PeerHeard::Up, PeerHeard::Over]);
    pair.run_for(Duration::from_secs(6));
    assert_eq!(pair.told, [Told::Down]);
    // Once the channel is there, the server is told.
    let mut pair = half_way(Client::V1);
    pair.told.clear();
    while !pair.heard.contains(&PeerHeard::Open) {
        pair.run_for(Duration::from_millis(1));
    }
    pair.peer.close();
    pair.run_for(Duration::from_millis(100));
    assert_eq!(pair.told, [Told::Over]);
}

#[test]
fn a_far_end_that_is_somewhere_else_says_only_what_is_about_its_checks() {
    // As a call that was set up the way WHIP sets one up goes on: the
    // client does not offer again, and sends the part of its new offer
    // that is about its checks. Each side puts what it is sent into
    // the description it had.
    let mut pair = Pair::fresh(Like::Chrome, Client::V1);
    for n in 0..5 {
        pair.client_says(n);
        pair.server_sends(tag(1), n, false);
    }
    let cut = Weather {
        cut: true,
        ..Weather::default()
    };
    pair.set_weather(cut);
    pair.run_for(Duration::from_secs(30));
    assert_eq!(pair.told.last(), Some(&Told::Down));
    pair.set_weather(Weather::default());

    let (offer, answer) = pair.first.clone();
    let before = pair.call.name().to_owned();
    pair.client = "203.0.113.9:61000".parse().unwrap();
    let again = pair.peer.offer_again(pair.client).unwrap();
    // What the client sends: other checks than it had, and nothing of
    // the audio or the channel.
    let sent = fragment::of(&again).unwrap();
    assert_ne!(fragment::credentials(&sent), fragment::credentials(&offer));
    assert!(!sent.contains("a=rtpmap") && !sent.contains("a=fingerprint"));
    // The server answers the offer it had, with those checks.
    let offered = fragment::into(&offer, &sent).unwrap();
    let answered = pair.call.answer_again(&offered).unwrap();
    assert_ne!(pair.call.name(), before);
    // And is answered with the part of its answer that is about its
    // own: that it only answers checks, and where it is reached.
    let back = fragment::of(&answered).unwrap();
    assert_eq!(
        fragment::credentials(&back).map(|(name, _)| name),
        Some(pair.call.name())
    );
    assert!(back.contains("a=ice-lite\r\n"), "{back}");
    assert_eq!(back.matches("a=candidate:").count(), 1, "{back}");
    assert!(pair.peer.answered(&fragment::into(&answer, &back).unwrap()));

    pair.told.clear();
    pair.heard.clear();
    pair.run_for(Duration::from_secs(2));
    assert_eq!(pair.told, [Told::Up]);
    assert_eq!(pair.heard, [PeerHeard::Up]);
    pair.told.clear();
    pair.heard.clear();
    // The call is the one it was.
    for n in 5..10 {
        pair.client_says(n);
        pair.server_sends(tag(1), n, n == 9);
    }
    let frames: Vec<u32> = pair.audio().iter().map(|(.., frame)| *frame).collect();
    assert_eq!(frames, [5, 6, 7, 8, 9]);
    let frames: Vec<u32> = pair.heard_audio().iter().map(|(_, frame)| *frame).collect();
    assert_eq!(frames, [5, 6, 7, 8, 9]);

    // A fragment whose checks are the ones the call has is no new
    // beginning, and one that is put into the first offer is that
    // offer: the call is answered as it was, and goes on.
    let same = fragment::of(&offered).unwrap();
    assert_eq!(fragment::into(&offered, &same).unwrap(), offered);
}
