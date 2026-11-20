//! A session whose far end is on a WebRTC call.
//!
//! The far end is the test, with the client the transport has for
//! itself: it posts its offer as a browser would (`POST /v1/webrtc`),
//! makes the call the answer describes over the sockets of this
//! machine, and says hello on it. What it says aloud is Opus, a packet
//! every 20 ms. The providers are the stand-ins `aloud` has.

use core::time::Duration;

use waveport_media::codec::opus::{MAX_PACKET, OpusDecoder, OpusEncoder, OpusSettings};
use waveport_ports::audio::Rate;
use waveport_protocol::client::v1::{ByeReason, ErrorCode, Fact, Resume};
use waveport_testkit::servers::{HttpServer, WsServer, WsStep};
use waveport_testkit::speech;
use waveport_testkit::stand_in::{HEARD, SAID, spoken};
use waveport_transport_webrtc::{Dialer, Dialing, Heard, Like, Unconnected, fragment};
use waveport_transport_websocket::{Client, Hello};

use crate::aloud::{Call, assets};
use crate::far_end::{Answer, ask};
use crate::{Served, answer, desk};

/// How many samples a packet holds: 20 ms at 48 kHz.
pub(crate) const PACKET: usize = 960;

/// How long anything on one machine may take.
pub(crate) const SOON: Duration = Duration::from_secs(10);

/// What a daemon is told to have it answer calls, on ports that are
/// free.
pub(crate) const CALLED: &str = "[webrtc]\nudp = \"127.0.0.1:0\"\ntcp = \"127.0.0.1:0\"\n";

/// The providers an agent that hears and speaks is given.
pub(crate) struct Providers {
    pub(crate) stt: WsServer,
    pub(crate) llm: HttpServer,
    pub(crate) tts: WsServer,
}

impl Providers {
    /// The ones of a call in which the agent is asked the time, says
    /// it, and ends the call.
    pub(crate) async fn of_one_call() -> Self {
        Self {
            stt: WsServer::start(vec![WsStep::script(HEARD)]).await,
            llm: HttpServer::start(vec![
                answer(&[], Some("get_time")),
                answer(&SAID, Some("end_call")),
                answer(&[], None),
            ])
            .await,
            tts: WsServer::start(vec![spoken()]).await,
        }
    }

    /// The configuration of a daemon whose agent `desk` hears and
    /// speaks with these, and has an agent `clerk` that only writes.
    /// `webrtc` is what it is told of calls.
    pub(crate) fn config(&self, webrtc: &str) -> String {
        format!(
            "listen = \"127.0.0.1:0\"\n\
             assets = '{}'\n\
             {webrtc}\n\
             [agents.clerk.llm]\n\
             url = \"{llm}\"\n\
             model = \"a-model\"\n\
             [agents.desk.llm]\n\
             url = \"{llm}\"\n\
             model = \"a-model\"\n\
             [agents.desk.speech.stt]\n\
             provider = \"openai\"\n\
             url = \"{}\"\n\
             [agents.desk.speech.tts]\n\
             provider = \"cartesia\"\n\
             url = \"{}\"\n",
            assets().display(),
            self.stt.url("/v1/realtime"),
            self.tts.url("/tts/websocket"),
            llm = self.llm.url("/v1"),
        )
    }
}

/// A daemon that answers calls, with these providers.
pub(crate) async fn daemon(providers: &Providers, webrtc: &str) -> Served {
    Served::start(&desk(), &providers.config(webrtc), &[]).await
}

/// Posts `offer` as whoever holds `token`.
async fn posts(daemon: &Served, token: &str, offer: &str) -> Answer {
    let bearer = format!("Bearer {token}");
    let headers = [
        ("Authorization", bearer.as_str()),
        ("Content-Type", "application/sdp"),
    ];
    ask(&daemon.address, "POST", "/v1/webrtc", &headers, Some(offer)).await
}

/// Asks about the call at `at` as whoever holds `token`. `more` is what
/// the request says besides.
async fn asks(
    daemon: &Served,
    token: &str,
    method: &str,
    at: &str,
    more: &[(&str, &str)],
    body: Option<&str>,
) -> Answer {
    let bearer = format!("Bearer {token}");
    let mut headers = vec![("Authorization", bearer.as_str())];
    headers.extend_from_slice(more);
    ask(&daemon.address, method, at, &headers, body).await
}

/// The hello of a client that holds `token`.
fn hello(token: &str) -> Hello {
    Hello {
        token: Some(token.to_owned()),
        ..Hello::new(None)
    }
}

/// A call that was offered and answered, and has not been made.
struct Offered {
    dialing: Dialing,
    /// The offer, as it was posted.
    offer: String,
    answered: Answer,
}

impl Offered {
    /// Where the call is.
    fn at(&self) -> &str {
        self.answered.header("location").unwrap()
    }
}

/// A client that holds `token` offers a call, over datagrams or over a
/// connection, and is answered.
async fn offers(daemon: &Served, token: &str, tcp: bool) -> Offered {
    let local = "127.0.0.1:0".parse().unwrap();
    let (dialing, offer) = Dialer::offer(Like::Chrome, local, tcp).await.unwrap();
    let answered = posts(daemon, token, &offer).await;
    assert_eq!(answered.status, 201, "{answered:?}");
    Offered {
        dialing,
        offer,
        answered,
    }
}

/// A client that holds `token` calls, and is welcomed.
async fn calls(daemon: &Served, token: &str, tcp: bool) -> (Dialer, Made) {
    let Offered {
        dialing,
        offer,
        answered,
    } = offers(daemon, token, tcp).await;
    let dialer = dialing
        .answered(&answered.body, &hello(token), SOON)
        .await
        .unwrap_or_else(|unconnected| panic!("{unconnected}: {}", daemon.errors()));
    (dialer, Made { offer, answered })
}

/// What is left of an offer once its call is made.
struct Made {
    offer: String,
    answered: Answer,
}

impl Made {
    fn at(&self) -> &str {
        self.answered.header("location").unwrap()
    }
}

/// Says `speech` to the agent, a packet every 20 ms, and then nothing,
/// for as long as the call lasts. What the agent says is decoded, and
/// said to have been played as it arrives.
async fn talks(dialer: &mut Dialer, speech: &[i16]) -> Call {
    let mut encoder = OpusEncoder::new(Rate::Hz48000, OpusSettings::default()).unwrap();
    let mut decoder = OpusDecoder::new(Rate::Hz48000).unwrap();
    let mut frames = speech.chunks(PACKET);
    let mut every = tokio::time::interval(Duration::from_millis(20));
    let mut call = Call::default();
    let mut open: Option<(u32, u32)> = None;
    loop {
        tokio::select! {
            _ = every.tick() => {
                let mut frame = [0_i16; PACKET];
                if let Some(next) = frames.next() {
                    frame[..next.len()].copy_from_slice(next);
                }
                let mut packet = [0_u8; MAX_PACKET];
                let length = encoder.encode(&frame, &mut packet).unwrap();
                // A call that is over takes nothing, and says so below.
                let _ = dialer.audio(&packet[..length]);
            }
            heard = dialer.next() => match heard.unwrap() {
                Some(Heard::Begins { run, .. }) => {
                    open = Some((run, 0));
                    call.runs.0 += 1;
                }
                Some(Heard::Audio(packet)) => {
                    let mut pcm = [0_i16; 5_760];
                    let samples = decoder.decode(&packet, &mut pcm).unwrap();
                    call.loud += pcm[..samples]
                        .iter()
                        .filter(|sample| sample.unsigned_abs() > 1_000)
                        .count();
                    let (run, played) = open.as_mut().unwrap();
                    *played += u32::try_from(samples).unwrap();
                    let _ = dialer.played(*run, *played);
                }
                Some(Heard::Ends(_)) => call.runs.1 += 1,
                Some(Heard::Fact { fact, .. }) => call.facts.push(fact),
                Some(Heard::Bye(reason)) => {
                    call.reason = Some(reason);
                    return call;
                }
                Some(_) => {}
                None => return call,
            },
        }
    }
}

/// The question, after a moment of nothing.
pub(crate) fn question() -> Vec<i16> {
    let mut speech = vec![0_i16; 48 * 400];
    speech.extend(speech::what_time_is_it(Rate::Hz48000));
    speech
}

/// Holds a call in which the agent was asked the time to what such a
/// call comes to: it heard the question, answered it aloud, and ended
/// the call as it was asked to.
fn was_asked_and_answered(call: &Call, errors: &str) {
    let all = format!("{call:#?}\n{errors}");
    assert_eq!(call.reason, Some(ByeReason::Ended), "{all}");
    let asked = call.facts.iter().find_map(|fact| match fact {
        Fact::TurnCommitted { text, .. } => Some(text.as_str()),
        _ => None,
    });
    assert_eq!(asked, Some(speech::WHAT_TIME_IS_IT), "{all}");
    let answered = call.facts.iter().find_map(|fact| match fact {
        Fact::UtteranceCommitted {
            text, truncated, ..
        } => Some((text.as_str(), *truncated)),
        _ => None,
    });
    assert_eq!(
        answered,
        Some(("It is ten past three. Goodbye!", false)),
        "{all}"
    );
    // What it said arrived as Opus that decodes to its voice.
    assert_eq!(call.runs, (1, 1), "{all}");
    assert!(call.loud > 40_000, "{all}");
}

/// A whole call, over datagrams or over a connection.
async fn asks_aloud_and_hears_the_answer(tcp: bool) {
    let providers = Providers::of_one_call().await;
    let daemon = daemon(&providers, CALLED).await;
    let (name, token) = daemon.session().await;

    let (mut dialer, offered) = calls(&daemon, &token, tcp).await;
    // The offer was answered as WHIP answers one: with an answer, where
    // the call is, and the name of its checks. Nothing between the two
    // keeps it.
    let answered = &offered.answered;
    assert_eq!(answered.header("content-type"), Some("application/sdp"));
    assert_eq!(answered.header("etag"), Some("\"1\""));
    assert_eq!(answered.header("cache-control"), Some("no-store"));
    let at = offered.at();
    let call = at.strip_prefix("/v1/webrtc/").unwrap();
    assert!(
        call.len() == 32 && call.chars().all(|c| c.is_ascii_hexdigit()),
        "{at}"
    );
    assert!(answered.body.contains("a=ice-lite"), "{answered:?}");
    assert_eq!(dialer.welcome().session, name);
    assert!(!dialer.welcome().resumed);
    // The session hears and speaks.
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{name}"), None)
        .await
        .json();
    assert_eq!(info["mode"], "voice", "{info}");
    // The call is there for whoever made its offer.
    let there = asks(&daemon, &token, "GET", at, &[], None).await;
    assert_eq!((there.status, there.body.as_str()), (204, ""), "{there:?}");

    let call = tokio::time::timeout(Duration::from_secs(60), talks(&mut dialer, &question())).await;
    let errors = daemon.errors();
    let call = call.unwrap_or_else(|_| panic!("the call did not end: {errors}"));
    was_asked_and_answered(&call, &errors);
    // The recogniser was sent what the far end said, all the while.
    let heard = providers.stt.connections();
    assert_eq!(heard.len(), 1, "{errors}");
    let sent = heard[0]
        .texts()
        .filter(|text| text.contains("input_audio_buffer.append"))
        .count();
    assert!(sent > 20, "{sent}\n{errors}");

    // A call that is over is let go of.
    let began = std::time::Instant::now();
    loop {
        let there = asks(&daemon, &token, "GET", at, &[], None).await;
        if there.status == 404 {
            assert_eq!(there.code(), "no_session");
            break;
        }
        assert!(began.elapsed() < SOON, "{there:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_client_on_a_call_asks_aloud_and_hears_the_answer() {
    asks_aloud_and_hears_the_answer(false).await;
}

#[tokio::test]
async fn a_client_whose_network_lets_no_datagram_out_calls_over_a_connection() {
    asks_aloud_and_hears_the_answer(true).await;
}

/// `offer` without the stream `kind` (`audio` or `application`).
fn without(offer: &str, kind: &str) -> String {
    let begins = format!("m={kind}");
    let mut kept = String::new();
    let mut inside = false;
    let mut mid = None;
    for line in offer.split_inclusive("\r\n") {
        if line.starts_with("m=") {
            inside = line.starts_with(&begins);
        }
        if inside {
            if let Some(named) = line.trim_end().strip_prefix("a=mid:") {
                mid = Some(named.to_owned());
            }
        } else {
            kept.push_str(line);
        }
    }
    let mid = mid.unwrap();
    assert!(kept.contains(&format!(" {mid}")), "{kept}");
    kept.replacen(&format!(" {mid}"), "", 1)
}

#[tokio::test]
async fn an_offer_is_answered_for_whoever_holds_a_token_of_a_session_that_can_be_called() {
    let providers = Providers::of_one_call().await;
    let daemon = daemon(&providers, CALLED).await;
    let (_, token) = daemon.session().await;
    let local = "127.0.0.1:0".parse().unwrap();
    let (_dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
    let sdp = ("Content-Type", "application/sdp");

    // Nobody: an offer says who makes it.
    let nobody = ask(&daemon.address, "POST", "/v1/webrtc", &[sdp], Some(&offer)).await;
    assert_eq!((nobody.status, nobody.code().as_str()), (401, "token"));
    assert_eq!(nobody.header("www-authenticate"), Some("Bearer"));
    // A token that is none, of either kind, and something that is no
    // token at all.
    for made_up in ["wpc1_0123", "wpr1_0123", "0123"] {
        let refused = posts(&daemon, made_up, &offer).await;
        assert_eq!(
            (refused.status, refused.code().as_str()),
            (401, "token"),
            "{made_up}"
        );
    }
    // What is no offer, and says so or does not.
    let json = [
        ("Authorization", format!("Bearer {token}")),
        ("Content-Type", "application/json".to_owned()),
    ];
    let json: Vec<(&str, &str)> = json.iter().map(|(n, v)| (*n, v.as_str())).collect();
    let refused = ask(&daemon.address, "POST", "/v1/webrtc", &json, Some("{}")).await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (415, "unsupported")
    );
    let refused = posts(&daemon, &token, "is anybody there").await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (400, "malformed")
    );
    // What it held is not said back.
    assert!(!refused.body.contains("anybody"), "{refused:?}");
    // An offer of a call with no audio, and one of a far end that has
    // no channel to speak the protocol on.
    for kind in ["audio", "application"] {
        let refused = posts(&daemon, &token, &without(&offer, kind)).await;
        assert_eq!(
            (refused.status, refused.code().as_str()),
            (422, "unsupported"),
            "{kind}: {refused:?}"
        );
    }
    // More than a body may hold.
    let long = format!("{offer}{}", "a=x\r\n".repeat(20_000));
    let refused = posts(&daemon, &token, &long).await;
    assert_eq!(refused.status, 413, "{refused:?}");

    // A session whose agent neither hears nor speaks is not called.
    let made = daemon
        .ask("POST", "/v1/sessions", Some(r#"{"agent":"clerk"}"#))
        .await
        .json();
    let writes = made["client_token"].as_str().unwrap();
    let refused = posts(&daemon, writes, &offer).await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (422, "unsupported")
    );
    let detail = refused.json()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("neither hears nor speaks"), "{detail}");

    // A session that is under way on no line is not called: it was
    // started by what was written to it, and is followed and written
    // to.
    let (written, writes) = daemon.session().await;
    let typed = r#"{"type":"text","text":"What time is it?"}"#;
    let begun = daemon
        .ask(
            "POST",
            &format!("/v1/sessions/{written}/input"),
            Some(typed),
        )
        .await;
    assert_eq!(begun.status, 202, "{begun:?}");
    let refused = posts(&daemon, &writes, &offer).await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (422, "unsupported")
    );
    let detail = refused.json()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("on no line"), "{detail}");

    // None of that used the token up, and neither does an offer that is
    // answered and whose call is never made: its holder comes another
    // way with it, on a WebSocket.
    let answered = posts(&daemon, &token, &offer).await;
    assert_eq!(answered.status, 201, "{answered:?}");
    let address = format!("ws://{}", daemon.address);
    let client = Client::connect(&address, &hello(&token), SOON).await;
    let client = client.unwrap_or_else(|unconnected| panic!("{unconnected}"));
    assert!(!client.welcome().resumed);
    // Now it is used up: no offer is answered for it.
    let refused = posts(&daemon, &token, &offer).await;
    assert_eq!((refused.status, refused.code().as_str()), (401, "token"));
    // And the session, which is under way on a WebSocket, is not
    // called by its client either.
    let back = client.welcome().resume_token.clone().unwrap();
    let refused = posts(&daemon, &back, &offer).await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (422, "unsupported")
    );
    let detail = refused.json()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("another kind of line"), "{detail}");
}

#[tokio::test]
async fn a_daemon_that_was_told_of_no_calls_answers_none() {
    let providers = Providers::of_one_call().await;
    let daemon = daemon(&providers, "").await;
    let (_, token) = daemon.session().await;
    let refused = posts(&daemon, &token, "v=0\r\n").await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (404, "unsupported")
    );
    let refused = asks(&daemon, &token, "DELETE", "/v1/webrtc/0123", &[], None).await;
    assert_eq!(refused.status, 404, "{refused:?}");
    let refused = ask(&daemon.address, "GET", "/v1/webrtc", &[], None).await;
    assert_eq!(refused.status, 404, "{refused:?}");
}

/// What a client is refused with as it says hello.
fn refused(connected: Result<Dialer, Unconnected>) -> (ErrorCode, String) {
    match connected {
        Err(Unconnected::Refused { code, message }) => (code, message),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("the client was welcomed"),
    }
}

#[tokio::test]
async fn a_hello_is_for_the_session_its_offer_was_made_for() {
    let providers = Providers::of_one_call().await;
    let daemon = daemon(&providers, CALLED).await;
    let (_, one) = daemon.session().await;
    let (other, others) = daemon.session().await;

    // The offer is made with the token of one session, and the hello
    // says the token of another.
    let offered = offers(&daemon, &one, false).await;
    let connected = offered
        .dialing
        .answered(&offered.answered.body, &hello(&others), SOON)
        .await;
    let (code, why) = refused(connected);
    assert_eq!(code, ErrorCode::Token, "{why}");
    assert!(why.contains("another session"), "{why}");
    // Neither session was started for it.
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{other}"), None)
        .await
        .json();
    assert_eq!(info["state"], "waiting", "{info}");

    // A hello that says no token at all, on a call whose offer said
    // one.
    let offered = offers(&daemon, &one, false).await;
    let connected = offered
        .dialing
        .answered(&offered.answered.body, &Hello::new(None), SOON)
        .await;
    let (code, why) = refused(connected);
    assert_eq!(code, ErrorCode::Token, "{why}");
}

/// The offer without what it says of where its far end is.
fn told_nowhere(offer: &str) -> String {
    let kept: String = offer
        .split_inclusive("\r\n")
        .filter(|line| !line.starts_with("a=candidate:") && !line.starts_with("a=end-of-"))
        .collect();
    assert_ne!(kept, offer);
    kept
}

#[tokio::test]
async fn a_client_that_is_somewhere_else_says_so_where_it_made_its_offer() {
    let providers = Providers::of_one_call().await;
    let daemon = daemon(&providers, CALLED).await;
    let (name, token) = daemon.session().await;
    let (_, others) = daemon.session().await;

    // A client that says nothing of where it is in its offer, as one
    // that has not found out yet: it is heard from where its checks
    // come from.
    let local = "127.0.0.1:0".parse().unwrap();
    let (dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
    let offer = told_nowhere(&offer);
    let answered = posts(&daemon, &token, &offer).await;
    assert_eq!(answered.status, 201, "{answered:?}");
    let at = answered.header("location").unwrap().to_owned();
    let mut dialer = dialing
        .answered(&answered.body, &hello(&token), SOON)
        .await
        .unwrap_or_else(|unconnected| panic!("{unconnected}: {}", daemon.errors()));
    assert_eq!(dialer.welcome().session, name);

    let kind = ("Content-Type", "application/trickle-ice-sdpfrag");
    let first = ("If-Match", "\"1\"");
    let any = ("If-Match", "*");
    // Where it is, said later: a fragment with the checks the call
    // has. It is taken, and answered with nothing.
    let here = fragment::of(&offer).unwrap();
    let heeded = asks(&daemon, &token, "PATCH", &at, &[kind, first], Some(&here)).await;
    assert_eq!(
        (heeded.status, heeded.body.as_str()),
        (204, ""),
        "{heeded:?}"
    );
    assert_eq!(heeded.header("etag"), None);
    // What a fragment is refused for: saying nothing of which checks
    // it is for, or naming others than the call has; being no
    // fragment, or saying so of something else; and being asked by
    // anybody but whoever made the offer.
    let refused = asks(&daemon, &token, "PATCH", &at, &[kind], Some(&here)).await;
    assert_eq!(refused.status, 428, "{refused:?}");
    let other = ("If-Match", "\"2\"");
    let refused = asks(&daemon, &token, "PATCH", &at, &[kind, other], Some(&here)).await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (412, "out_of_place")
    );
    let refused = asks(
        &daemon,
        &token,
        "PATCH",
        &at,
        &[kind, any],
        Some("a=mid:0\r\n"),
    )
    .await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (400, "malformed")
    );
    let sdp = ("Content-Type", "application/sdp");
    let refused = asks(&daemon, &token, "PATCH", &at, &[sdp, any], Some(&here)).await;
    assert_eq!(refused.status, 415, "{refused:?}");
    let refused = asks(&daemon, &others, "PATCH", &at, &[kind, any], Some(&here)).await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (404, "no_session")
    );
    let refused = ask(&daemon.address, "PATCH", &at, &[kind, any], Some(&here)).await;
    assert_eq!((refused.status, refused.code().as_str()), (401, "token"));

    // The client is somewhere else, and its checks begin anew. It does
    // not offer again: it sends what its new offer says of its checks,
    // and is answered with what the answer would have said of the
    // daemon's.
    let again = dialer.moves().await.unwrap();
    let sent = fragment::of(&again).unwrap();
    assert_ne!(fragment::credentials(&sent), fragment::credentials(&offer));
    let heeded = asks(&daemon, &token, "PATCH", &at, &[kind, any], Some(&sent)).await;
    assert_eq!(heeded.status, 200, "{heeded:?}");
    assert_eq!(
        heeded.header("content-type"),
        Some("application/trickle-ice-sdpfrag")
    );
    assert_eq!(heeded.header("etag"), Some("\"2\""));
    let before = fragment::credentials(&answered.body).unwrap();
    let after = fragment::credentials(&heeded.body).unwrap();
    assert_ne!(before, after);
    assert!(heeded.body.contains("a=ice-lite"), "{heeded:?}");
    assert!(heeded.body.contains("a=candidate:"), "{heeded:?}");
    assert!(!heeded.body.contains("a=fingerprint"), "{heeded:?}");
    assert!(dialer.moved(&fragment::into(&answered.body, &heeded.body).unwrap()));
    // The checks the call had are not the ones it has, and a fragment
    // for them is for none.
    let refused = asks(&daemon, &token, "PATCH", &at, &[kind, first], Some(&here)).await;
    assert_eq!(refused.status, 412, "{refused:?}");
    let second = ("If-Match", "\"2\"");
    let heeded = asks(&daemon, &token, "PATCH", &at, &[kind, second], Some(&sent)).await;
    assert_eq!(heeded.status, 204, "{heeded:?}");

    // The call is the one it was: the session goes on, and is asked and
    // answers on it.
    let call = tokio::time::timeout(Duration::from_secs(60), talks(&mut dialer, &question())).await;
    let errors = daemon.errors();
    let call = call.unwrap_or_else(|_| panic!("the call did not end: {errors}"));
    was_asked_and_answered(&call, &errors);
}

/// What a client hears until `last` is true of something, or its call
/// is over.
async fn until(dialer: &mut Dialer, last: impl Fn(&Heard) -> bool) -> Vec<Heard> {
    let mut heard = Vec::new();
    let all = async {
        while let Some(next) = dialer.next().await.unwrap() {
            let done = last(&next);
            heard.push(next);
            if done {
                break;
            }
        }
    };
    tokio::time::timeout(SOON, all)
        .await
        .unwrap_or_else(|_| panic!("what was waited for did not come"));
    heard
}

/// Whether the daemon still has the session `name`, asked until it has
/// not or a while has gone by.
async fn is_let_go_of(daemon: &Served, name: &str) -> bool {
    let began = std::time::Instant::now();
    while began.elapsed() < SOON {
        let info = daemon
            .ask("GET", &format!("/v1/sessions/{name}"), None)
            .await;
        if info.status == 404 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_client_that_hangs_up_where_it_made_its_offer_has_left() {
    let providers = Providers::of_one_call().await;
    let daemon = daemon(&providers, CALLED).await;
    let (name, token) = daemon.session().await;
    let (_, others) = daemon.session().await;
    let (mut dialer, offered) = calls(&daemon, &token, false).await;
    let at = offered.at().to_owned();

    // Nobody else ends the call: whoever says no token, and whoever
    // says another than the offer was made with.
    let refused = ask(&daemon.address, "DELETE", &at, &[], None).await;
    assert_eq!((refused.status, refused.code().as_str()), (401, "token"));
    let refused = asks(&daemon, &others, "DELETE", &at, &[], None).await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (404, "no_session")
    );
    let there = asks(&daemon, &token, "GET", &at, &[], None).await;
    assert_eq!(there.status, 204, "{there:?}");

    // Whoever made the offer does, and is told that it is done.
    let ended = asks(&daemon, &token, "DELETE", &at, &[], None).await;
    assert_eq!((ended.status, ended.body.as_str()), (200, ""), "{ended:?}");
    // The call is over for the client, and its session with it: a far
    // end that hangs up has left, and is not waited for.
    let heard = until(&mut dialer, |_| false).await;
    assert!(
        !heard
            .iter()
            .any(|heard| matches!(heard, Heard::Error { .. })),
        "{heard:?}"
    );
    assert!(is_let_go_of(&daemon, &name).await, "{}", daemon.errors());
    // There is no such call any more.
    let gone = asks(&daemon, &token, "DELETE", &at, &[], None).await;
    assert_eq!((gone.status, gone.code().as_str()), (404, "no_session"));
    let kind = ("Content-Type", "application/trickle-ice-sdpfrag");
    let here = fragment::of(&offered.offer).unwrap();
    let gone = asks(
        &daemon,
        &token,
        "PATCH",
        &at,
        &[kind, ("If-Match", "*")],
        Some(&here),
    )
    .await;
    assert_eq!(gone.status, 404, "{gone:?}");

    // A call that is ended before its far end came is not waited for
    // either, and its session is as it was: nobody came to it.
    let (waiting, token) = daemon.session().await;
    let offered = offers(&daemon, &token, false).await;
    let ended = asks(&daemon, &token, "DELETE", offered.at(), &[], None).await;
    assert_eq!(ended.status, 200, "{ended:?}");
    let connected = offered
        .dialing
        .answered(
            &offered.answered.body,
            &hello(&token),
            Duration::from_secs(2),
        )
        .await;
    assert!(connected.is_err());
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{waiting}"), None)
        .await
        .json();
    assert_eq!(info["state"], "waiting", "{info}");
}

#[tokio::test]
async fn a_client_whose_call_dropped_comes_back_on_another() {
    let providers = Providers::of_one_call().await;
    let daemon = daemon(&providers, CALLED).await;
    let (name, token) = daemon.session().await;
    let (dialer, _) = calls(&daemon, &token, false).await;
    let welcome = dialer.welcome().clone();
    let back = welcome.resume_token.clone().unwrap();
    // Its network is gone, and it with it.
    dialer.drop_line();

    // It comes back on another call, whose offer it makes with the
    // token it was given to come back with, and says where it was.
    let again = Hello {
        resume: Some(Resume {
            token: back.clone(),
            life: welcome.life,
            seen: 0,
        }),
        ..Hello::new(None)
    };
    // A token that is one of its kind and was given to nobody lets no
    // offer in.
    let mut forged = back.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == '0' { '1' } else { '0' });
    let local = "127.0.0.1:0".parse().unwrap();
    let (_dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
    let refused = posts(&daemon, &forged, &offer).await;
    assert_eq!((refused.status, refused.code().as_str()), (401, "token"));

    let offered = offers(&daemon, &back, false).await;
    let mut dialer = offered
        .dialing
        .answered(&offered.answered.body, &again, SOON)
        .await
        .unwrap_or_else(|unconnected| panic!("{unconnected}: {}", daemon.errors()));
    assert_eq!(dialer.welcome().session, name);
    assert!(dialer.welcome().resumed);
    assert_eq!(dialer.welcome().life, welcome.life);

    // On a WebSocket it does not: the session is on a call, and its
    // client comes back the way it came.
    let address = format!("ws://{}", daemon.address);
    let other_way = Client::connect(&address, &again, SOON).await;
    let Err(waveport_transport_websocket::Unconnected::Refused { code, message }) = other_way
    else {
        panic!("a client was put on a WebSocket of a session that is on a call");
    };
    assert_eq!(code, ErrorCode::Unsupported, "{message}");
    assert!(message.contains("another kind of line"), "{message}");

    // The session went on, and is asked and answers on the call it is
    // on now.
    let call = tokio::time::timeout(Duration::from_secs(60), talks(&mut dialer, &question())).await;
    let errors = daemon.errors();
    let call = call.unwrap_or_else(|_| panic!("the call did not end: {errors}"));
    was_asked_and_answered(&call, &errors);
}

/// How long a call is on its way in the test of how many may be: long
/// enough for seven offers to be made in less, on a machine that is
/// busy with everything else too.
const ON_ITS_WAY: Duration = Duration::from_secs(5);

/// Posts `offer` as whoever holds each of `tokens`, one after the
/// other, and returns what the daemon answered to each.
///
/// The limits are on calls that are on their way together, and a call
/// is on its way for [`ON_ITS_WAY`] here. On a machine that is busy the
/// offers can take longer than that, and what was answered then says
/// nothing of the limits: the offers are made again, once the calls
/// before them have had their time.
async fn together(daemon: &Served, offer: &str, tokens: &[&str]) -> Vec<Answer> {
    for _ in 0..5 {
        let began = std::time::Instant::now();
        let mut answers = Vec::new();
        for token in tokens {
            answers.push(posts(daemon, token, offer).await);
        }
        if began.elapsed() + Duration::from_secs(1) < ON_ITS_WAY {
            return answers;
        }
        tokio::time::sleep(ON_ITS_WAY + Duration::from_secs(1)).await;
    }
    panic!("the offers took longer than a call is on its way, five times over");
}

#[tokio::test]
async fn calls_that_are_never_made_are_not_waited_for_and_keep_nobody_out() {
    let providers = Providers::of_one_call().await;
    let few = format!(
        "{CALLED}setting_up = 5\nthere_within_ms = {}\n",
        ON_ITS_WAY.as_millis()
    );
    let daemon = daemon(&providers, &few).await;
    let (_, one) = daemon.session().await;
    let (_, other) = daemon.session().await;
    let (one, other) = (one.as_str(), other.as_str());
    let local = "127.0.0.1:0".parse().unwrap();
    let (_dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
    let answered = |answers: &[Answer]| -> Vec<(u16, String)> {
        answers
            .iter()
            .map(|answer| {
                let code = if answer.status == 201 {
                    String::new()
                } else {
                    answer.code()
                };
                (answer.status, code)
            })
            .collect()
    };
    let made = (201, String::new());
    let busy = (503, "busy".to_owned());

    // One session has four calls on their way, and no more. All
    // sessions together have five.
    let answers = together(&daemon, &offer, &[one, one, one, one, one, other, other]).await;
    assert_eq!(
        answered(&answers),
        [
            made.clone(),
            made.clone(),
            made.clone(),
            made.clone(),
            busy.clone(),
            made.clone(),
            busy.clone()
        ]
    );
    // Whoever is turned away is told when to ask again.
    assert_eq!(answers[4].header("retry-after"), Some("1"));
    assert_eq!(answers[6].header("retry-after"), Some("1"));

    // None of them is made. They are let go of when their far ends had
    // their time, and the next offer is answered.
    tokio::time::sleep(ON_ITS_WAY + Duration::from_secs(1)).await;
    let (mut dialer, _) = calls(&daemon, one, false).await;
    // A call whose far end is there is not one on its way: the session
    // of this one has four more, for a client that tries again, and no
    // fifth.
    let back = dialer.welcome().resume_token.clone().unwrap();
    let back = back.as_str();
    let answers = together(&daemon, &offer, &[back, back, back, back, back]).await;
    assert_eq!(
        answered(&answers),
        [made.clone(), made.clone(), made.clone(), made, busy]
    );
    // The client is on its call, and on no other: the session goes on
    // with it.
    let call = tokio::time::timeout(Duration::from_secs(60), talks(&mut dialer, &question())).await;
    let errors = daemon.errors();
    let call = call.unwrap_or_else(|_| panic!("the call did not end: {errors}"));
    was_asked_and_answered(&call, &errors);
}

/// The servers a far end was told of with `answer`.
fn links(answer: &Answer) -> Vec<&str> {
    answer
        .headers
        .iter()
        .filter(|(name, _)| name == "link")
        .map(|(_, value)| value.as_str())
        .collect()
}

#[tokio::test]
async fn a_far_end_is_told_of_the_servers_that_help_it_get_through() {
    let providers = Providers::of_one_call().await;
    let told = format!(
        "{CALLED}\
         [[webrtc.ice_servers]]\n\
         urls = [\"stun:stun.example.net\"]\n\
         [[webrtc.ice_servers]]\n\
         urls = [\"turn:turn.example.net?transport=udp\", \"turns:turn.example.net:443?transport=tcp\"]\n\
         secret_env = \"WAVEPORT_TEST_TURN\"\n\
         ttl_s = 600\n"
    );
    let variables = [("WAVEPORT_TEST_TURN", "north")];
    let daemon = Served::start(&desk(), &providers.config(&told), &variables).await;
    let (name, token) = daemon.session().await;
    let offered = offers(&daemon, &token, false).await;

    let told = links(&offered.answered);
    assert_eq!(told.len(), 3, "{told:?}");
    assert_eq!(told[0], "<stun:stun.example.net>; rel=\"ice-server\"");
    // A name that says until when it is good and whose it is, and a
    // password made of it. The same for both addresses of the server.
    let who = told[1]
        .strip_prefix("<turn:turn.example.net?transport=udp>; rel=\"ice-server\"; username=\"")
        .unwrap_or_else(|| panic!("{told:?}"));
    let (username, rest) = who.split_once("\"; credential=\"").unwrap();
    let (until, whose) = username.split_once(':').unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let until: u64 = until.parse().unwrap();
    assert!((now + 590..=now + 610).contains(&until), "{username}");
    assert_eq!(whose, name);
    let credential = rest.strip_suffix('"').unwrap();
    assert_eq!(credential.len(), 28, "{credential}");
    assert!(!credential.contains("north"));
    assert!(
        told[2].starts_with("<turns:turn.example.net:443?transport=tcp>; rel=\"ice-server\"; ")
            && told[2].ends_with(rest),
        "{told:?}"
    );

    // What is posted where, for whoever asks before it posts, and the
    // servers for whoever says who they are: a far end that has to know
    // them before it can offer.
    let asked = ask(&daemon.address, "OPTIONS", "/v1/webrtc", &[], None).await;
    assert_eq!(asked.status, 204, "{asked:?}");
    assert_eq!(asked.header("accept-post"), Some("application/sdp"));
    assert_eq!(links(&asked), Vec::<&str>::new());
    let asked = asks(&daemon, &token, "OPTIONS", "/v1/webrtc", &[], None).await;
    assert_eq!(asked.status, 204, "{asked:?}");
    assert_eq!(links(&asked).len(), 3, "{asked:?}");
    assert_eq!(asked.header("cache-control"), Some("no-store"));
    let asked = ask(&daemon.address, "OPTIONS", offered.at(), &[], None).await;
    assert_eq!(asked.status, 204, "{asked:?}");
    assert_eq!(
        asked.header("accept-patch"),
        Some("application/trickle-ice-sdpfrag")
    );
    // Nothing is where offers are posted.
    let nothing = ask(&daemon.address, "GET", "/v1/webrtc", &[], None).await;
    assert_eq!((nothing.status, nothing.body.as_str()), (204, ""));
    // And nothing else is asked there.
    let refused = ask(&daemon.address, "PUT", "/v1/webrtc", &[], None).await;
    assert_eq!(refused.status, 405, "{refused:?}");
}

#[tokio::test]
async fn a_page_the_daemon_was_told_of_may_ask_what_a_call_takes() {
    let providers = Providers::of_one_call().await;
    let told = format!("allowed_origins = [\"https://app.example\"]\n{CALLED}");
    let daemon = daemon(&providers, &told).await;
    let (_, token) = daemon.session().await;
    let offered = offers(&daemon, &token, false).await;

    // A page from where the daemon was told of asks first whether it
    // may, and is told that it may say a token, change a call and end
    // it. With the answer to its offer it is shown where the call is,
    // the name of its checks and the servers.
    let page = [
        ("Origin", "https://app.example"),
        ("Access-Control-Request-Method", "PATCH"),
        ("Access-Control-Request-Headers", "authorization, if-match"),
    ];
    let may = ask(&daemon.address, "OPTIONS", offered.at(), &page, None).await;
    assert_eq!(may.status, 204, "{may:?}");
    let methods = may.header("access-control-allow-methods").unwrap();
    assert!(
        methods.contains("PATCH") && methods.contains("DELETE"),
        "{methods}"
    );
    let headers = may.header("access-control-allow-headers").unwrap();
    assert!(
        headers.contains("if-match") && headers.contains("authorization"),
        "{headers}"
    );
    let local = "127.0.0.1:0".parse().unwrap();
    let (_dialing, offer) = Dialer::offer(Like::Chrome, local, false).await.unwrap();
    let bearer = format!("Bearer {token}");
    let from_the_page = [
        ("Origin", "https://app.example"),
        ("Authorization", bearer.as_str()),
        ("Content-Type", "application/sdp"),
    ];
    let answered = ask(
        &daemon.address,
        "POST",
        "/v1/webrtc",
        &from_the_page,
        Some(&offer),
    )
    .await;
    assert_eq!(answered.status, 201, "{answered:?}");
    let shown = answered.header("access-control-expose-headers").unwrap();
    for name in ["ETag", "Link", "Location"] {
        assert!(shown.contains(name), "{shown}");
    }
    // A page from anywhere else is not answered.
    let elsewhere = [
        ("Origin", "https://elsewhere.example"),
        ("Authorization", bearer.as_str()),
        ("Content-Type", "application/sdp"),
    ];
    let refused = ask(
        &daemon.address,
        "POST",
        "/v1/webrtc",
        &elsewhere,
        Some(&offer),
    )
    .await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (403, "forbidden")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_daemon_that_is_told_to_stop_says_so_on_its_calls() {
    use waveport_testkit::stand_in::slow_answer;

    // A model that takes its time, so that an answer is under way when
    // the daemon is told to stop, and the daemon waits for it.
    let providers = Providers {
        stt: WsServer::start(vec![WsStep::script(HEARD)]).await,
        llm: HttpServer::start(vec![slow_answer(&SAID, Duration::from_millis(150))]).await,
        tts: WsServer::start(vec![spoken()]).await,
    };
    let mut daemon = daemon(&providers, CALLED).await;
    let (_, token) = daemon.session().await;
    let (mut dialer, _) = calls(&daemon, &token, false).await;
    // An offer that was answered and whose call was not made yet: it is
    // not waited for.
    let (_, other) = daemon.session().await;
    let offered = offers(&daemon, &other, false).await;

    dialer.text("What time is it?").unwrap();
    let began = std::time::Instant::now();
    while providers.llm.requests().is_empty() {
        assert!(began.elapsed() < SOON, "{}", daemon.errors());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    daemon.tell_to_stop();
    // It says that it is going where whatever stands before it asks,
    // and from then on answers no offer: the far end is told to come
    // again, which it does to the daemon that is started next.
    let began = std::time::Instant::now();
    while daemon.ask("GET", "/healthz", None).await.status != 503 {
        assert!(began.elapsed() < SOON, "{}", daemon.errors());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let refused = posts(&daemon, &other, &offered.offer).await;
    assert_eq!((refused.status, refused.code().as_str()), (503, "busy"));
    let detail = refused.json()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("the daemon is stopping"), "{detail}");
    assert_eq!(refused.header("retry-after"), Some("1"));

    let heard = until(&mut dialer, |heard| matches!(heard, Heard::Bye(_))).await;
    assert!(
        matches!(heard.last(), Some(Heard::Bye(ByeReason::GoingAway))),
        "{heard:?}"
    );
    let status = daemon.ended().await;
    assert!(status.success(), "{}", daemon.errors());
}
