//! Calls that a carrier brings, with a stand-in for the carrier.
//!
//! The carrier is the test: it asks the daemon what to do with a call,
//! in a form signed as Twilio signs one, reads the answer as a carrier
//! does, and brings the call to the stream the answer names. On the
//! stream it is the stand-in of the carrier transport, which holds what
//! it is sent and plays it at the pace of a telephone. Nothing here is
//! Twilio, and nothing leaves the machine.

use core::time::Duration;
use std::time::Instant;

use waveport_adapter_kit::secret::Secret;
use waveport_media::codec::g711;
use waveport_ports::audio::{Law, Rate};
use waveport_protocol::client::v1::{AudioFormat, ByeReason, Codec, Fact};
use waveport_testkit::servers::{HttpAnswer, HttpServer, WsServer, WsStep};
use waveport_testkit::speech;
use waveport_testkit::stand_in::{HEARD, SAID, greeted_at, spoken_at};
use waveport_transport_telephony_ws::Carrier;
use waveport_transport_telephony_ws::sim::{Answered, Ringing, Sent, Sim};
use waveport_transport_websocket::{Client, Hello};

use crate::aloud::{assets, talks};
use crate::far_end::{Answer, Event, Events, ask};
use crate::{Served, answer, desk, is_fact, refused_to_start};

const TWILIO: Carrier = Carrier::Twilio;

/// The variable the daemon is told holds the account's token, and a
/// token that was made up.
const TOKEN_ENV: &str = "A_CARRIER_TOKEN";
const TOKEN: &str = "a-made-up-token-of-a-carrier";

/// Where the carrier is told the daemon is, and what it asks there.
const PUBLIC: &str = "https://voice.example";
const VOICE: &str = "/v1/telephony/twilio/voice";
const STREAM: &str = "/v1/telephony/twilio/stream";

/// How many samples 20 ms of a telephone's audio are.
const PIECE: usize = 160;

/// The stand-ins for the providers of an agent that hears and speaks.
struct Providers {
    stt: WsServer,
    llm: HttpServer,
    tts: WsServer,
}

impl Providers {
    /// With answers for a session that hears and speaks on a line of
    /// each of `lines`, in that order, and for `written` sessions that
    /// do neither. Every session goes one way: the agent greets, is
    /// asked the time, answers and ends the session.
    ///
    /// A synthesiser speaks at the rate a session asks it for, which is
    /// the rate of its line where it has that one: 8 kHz on a telephone.
    async fn start(lines: &[Rate], written: usize) -> Self {
        let answers = (0..lines.len() + written)
            .map(|_| answer(&SAID, Some("end_call")))
            .collect();
        Self::answering(lines, answers).await
    }

    /// The same, with a model that answers as `answers` say.
    async fn answering(lines: &[Rate], answers: Vec<HttpAnswer>) -> Self {
        let voice = |rate: &Rate| [greeted_at(*rate), spoken_at(*rate)].concat();
        // The daemon keeps its connection to the synthesiser from one
        // session to the next, or opens another for the next: the
        // first has a voice for every session, and each after it for
        // the sessions that are left.
        let voices = (0..lines.len().max(1))
            .map(|from| lines.iter().skip(from).flat_map(voice).collect())
            .collect();
        Self {
            stt: WsServer::start(vec![WsStep::script(HEARD); lines.len()]).await,
            llm: HttpServer::start(answers).await,
            tts: WsServer::start(voices).await,
        }
    }

    /// The configuration of a daemon whose agent `desk` is made of
    /// these and answers Twilio's calls. `twilio` is what it says of
    /// Twilio besides, and `more` what it says of anything else.
    fn config(&self, twilio: &str, more: &str) -> String {
        format!(
            "listen = \"127.0.0.1:0\"\n\
             assets = '{}'\n\
             [telephony]\n\
             public = \"{PUBLIC}\"\n\
             [telephony.twilio]\n\
             agent = \"desk\"\n\
             token_env = \"{TOKEN_ENV}\"\n\
             {twilio}\n\
             {more}\n\
             [agents.desk]\n\
             greeting = \"Good morning.\"\n\
             [agents.desk.llm]\n\
             url = \"{}\"\n\
             model = \"a-model\"\n\
             [agents.desk.speech.stt]\n\
             provider = \"openai\"\n\
             url = \"{}\"\n\
             [agents.desk.speech.tts]\n\
             provider = \"cartesia\"\n\
             url = \"{}\"\n",
            assets().display(),
            self.llm.url("/v1"),
            self.stt.url("/v1/realtime"),
            self.tts.url("/tts/websocket"),
        )
    }

    /// A daemon that answers Twilio's calls.
    async fn daemon(&self, twilio: &str) -> Served {
        Served::start(&desk(), &self.config(twilio, ""), &[(TOKEN_ENV, TOKEN)]).await
    }
}

/// The caller is sent the voice of the agent: the first of it.
async fn hears_the_agent(sim: &mut Sim, daemon: &Served) {
    let spoken = async {
        loop {
            match sim.next().await {
                Ok(Some(Sent::Audio(_))) => return,
                Ok(Some(_)) => {}
                other => panic!("the stream ended before the agent spoke: {other:?}"),
            }
        }
    };
    let heard = tokio::time::timeout(Duration::from_secs(20), spoken).await;
    heard.unwrap_or_else(|_| panic!("the agent said nothing: {}", daemon.errors()));
}

/// The stream is over for the carrier, with or without a goodbye.
async fn is_over(sim: &mut Sim) {
    let over = async {
        loop {
            match sim.next().await {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    };
    let over = tokio::time::timeout(Duration::from_secs(20), over).await;
    over.unwrap_or_else(|_| panic!("the stream of a call that is over stayed open"));
}

/// Follows the session of a call, which begins when the call is
/// brought: a moment after the carrier has said which call it brings.
async fn follows(daemon: &Served, name: &str) -> Events {
    let patience = Instant::now() + Duration::from_secs(10);
    loop {
        match daemon.follow(name, "", None, &[]).await {
            Ok(events) => return events,
            Err(waits) if waits.status == 409 && Instant::now() < patience => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(refused) => panic!("the session of a call is not followed: {refused:?}"),
        }
    }
}

/// The sessions that broke off, as the daemon lists them.
async fn broken(daemon: &Served) -> serde_json::Value {
    let listed = daemon.ask("GET", "/v1/sessions?state=broken", None).await;
    listed.json()["sessions"].clone()
}

/// A name or a value of a form, as it is posted.
fn encoded(text: &str) -> String {
    use core::fmt::Write as _;
    text.bytes().fold(String::new(), |mut out, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
        out
    })
}

fn posted(form: &[(String, String)]) -> String {
    let fields: Vec<String> = form
        .iter()
        .map(|(name, value)| format!("{}={}", encoded(name), encoded(value)))
        .collect();
    fields.join("&")
}

/// Asks the daemon about a call with `form`, signed with `token` for
/// the address `signed_for`, as a carrier posts it: a form that says
/// how it is written, too.
async fn posts(
    daemon: &Served,
    form: &[(String, String)],
    token: &str,
    signed_for: &str,
) -> Answer {
    let kind = "application/x-www-form-urlencoded; charset=UTF-8";
    posts_as(daemon, form, token, signed_for, kind).await
}

/// The same, with the body said to be of the kind `kind`.
async fn posts_as(
    daemon: &Served,
    form: &[(String, String)],
    token: &str,
    signed_for: &str,
    kind: &str,
) -> Answer {
    let signature = TWILIO.sign(&Secret::new(token), signed_for, form);
    let headers = [
        ("Host", "voice.example"),
        ("Content-Type", kind),
        ("X-Twilio-Signature", signature.as_str()),
    ];
    ask(
        &daemon.address,
        "POST",
        VOICE,
        &headers,
        Some(&posted(form)),
    )
    .await
}

/// Asks the daemon what to do with `ringing`, as Twilio does.
async fn asks(daemon: &Served, ringing: &Ringing) -> Answer {
    let asked_at = format!("{PUBLIC}{VOICE}");
    posts(daemon, &ringing.form(TWILIO), TOKEN, &asked_at).await
}

/// What the carrier makes of an answer: the stream is where the daemon
/// was said to be.
fn read(answer: &Answer) -> Answered {
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        answer.header("content-type"),
        Some("text/xml; charset=utf-8")
    );
    assert_eq!(answer.header("cache-control"), Some("no-store"));
    let answered = Answered::read(TWILIO, &answer.body).unwrap();
    assert_eq!(answered.stream, format!("wss://voice.example{STREAM}"));
    answered
}

/// Connects to the stream of the daemon as the carrier of `ringing`,
/// with its request signed or not, and says nothing yet.
async fn connects(daemon: &Served, ringing: Ringing, signed: bool) -> Sim {
    let at = format!("ws://{}{STREAM}", daemon.address);
    let signed_for = format!("wss://voice.example{STREAM}");
    let signature = TWILIO.sign(&Secret::new(TOKEN), &signed_for, &[]);
    let headers = [("x-twilio-signature", signature.as_str())];
    let headers = if signed { &headers[..] } else { &[] };
    Sim::connects(TWILIO, &at, headers, ringing).await.unwrap()
}

/// Brings `ringing` to the stream its answer names, saying what the
/// answer gave the stream to say.
async fn brings(daemon: &Served, ringing: Ringing, answered: &Answered) -> Sim {
    let mut sim = connects(daemon, ringing, true).await;
    sim.begins(&answered.parameters).await.unwrap();
    sim
}

/// Whether the daemon closes the stream without the call having been
/// taken: nothing is sent on it first. The daemons these tests run have
/// an agent with a voice, so a stream that is let in is sent it.
async fn is_turned_away(sim: &mut Sim) -> bool {
    let closed = tokio::time::timeout(Duration::from_secs(10), sim.next()).await;
    matches!(closed, Ok(Ok(None)))
}

/// The names of the sessions the daemon has, once there are `many`: a
/// session that said goodbye is let go of a moment after.
async fn settled(daemon: &Served, many: usize) -> Vec<String> {
    let patience = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = sessions(daemon).await;
        if listed.len() == many || Instant::now() > patience {
            return listed;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The names of the sessions the daemon has.
async fn sessions(daemon: &Served) -> Vec<String> {
    let listed = daemon.ask("GET", "/v1/sessions", None).await.json();
    let listed = listed["sessions"].as_array().unwrap();
    listed
        .iter()
        .map(|session| session["session"].as_str().unwrap().to_owned())
        .collect()
}

/// What a caller heard on the telephone.
#[derive(Debug, Default)]
struct OnThePhone {
    /// How many samples the caller was played, and how many of them
    /// were of a voice.
    played: usize,
    loud: usize,
}

/// The caller says `speech` and then nothing, 20 ms at a time, for as
/// long as the call lasts or until `enough`, and the carrier plays what
/// it holds at the pace it is heard.
async fn on_the_telephone(sim: &mut Sim, speech: &[i16], enough: impl Fn() -> bool) -> OnThePhone {
    let mut said = vec![0_u8; speech.len()];
    g711::encode(Law::Mu, speech, &mut said);
    let mut pieces = said.chunks(PIECE);
    let mut heard = OnThePhone::default();
    let mut due = tokio::time::Instant::now();
    loop {
        match tokio::time::timeout_at(due, sim.next()).await {
            Ok(Ok(Some(_))) => {}
            // The session ended the call, or the stream is gone.
            Ok(Ok(None) | Err(_)) => return heard,
            Err(_) if enough() => return heard,
            Err(_) => {
                due += Duration::from_millis(20);
                // Silence, as µ-law writes it, once the question is
                // asked.
                let mut piece = [0xff_u8; PIECE];
                if let Some(next) = pieces.next() {
                    piece[..next.len()].copy_from_slice(next);
                }
                if sim.says(&piece).await.is_err() {
                    return heard;
                }
                let Ok(played) = sim.plays(PIECE).await else {
                    return heard;
                };
                let mut pcm = [0_i16; PIECE];
                let samples = g711::decode(Law::Mu, &played, &mut pcm);
                heard.played += samples;
                heard.loud += pcm[..samples]
                    .iter()
                    .filter(|sample| sample.unsigned_abs() > 1_000)
                    .count();
            }
        }
    }
}

/// What was said in a session, as its facts have it: each turn and
/// each utterance, and whether an utterance was cut short.
fn conversation(facts: &[Fact]) -> Vec<(&'static str, &str, bool)> {
    facts
        .iter()
        .filter_map(|fact| match fact {
            Fact::TurnCommitted { text, .. } => Some(("caller", text.as_str(), false)),
            Fact::UtteranceCommitted {
                text, truncated, ..
            } => Some(("agent", text.as_str(), *truncated)),
            _ => None,
        })
        .collect()
}

/// The facts among the events of a stream.
fn facts_of(events: &[Event]) -> Vec<Fact> {
    events
        .iter()
        .filter(|event| event.kind == "fact")
        .filter_map(|event| match event.message() {
            waveport_protocol::client::v1::FromServer::Fact { fact, .. } => Some(fact),
            _ => None,
        })
        .collect()
}

/// How every session of the agent goes, whatever its far end is on.
const AS_EVER: [(&str, &str, bool); 3] = [
    ("agent", "Good morning.", false),
    ("caller", speech::WHAT_TIME_IS_IT, false),
    ("agent", "It is ten past three. Goodbye!", false),
];

#[tokio::test]
async fn one_agent_answers_in_writing_on_a_line_and_on_the_telephone() {
    // A client's line takes 24 kHz of a synthesiser, and a telephone
    // 8 kHz.
    let providers = Providers::start(&[Rate::Hz24000, Rate::Hz8000], 1).await;
    let daemon = providers.daemon("").await;
    assert!(
        daemon
            .noted("calls of twilio are answered")
            .await
            .contains("https://voice.example/v1/telephony/twilio/voice")
    );

    // In writing: an application makes a session, follows it, and
    // types the question.
    let (name, _) = daemon.session().await;
    let mut events = daemon.follow(&name, "", None, &[]).await.unwrap();
    events
        .until(|event| is_fact(event, "utterance_committed"))
        .await;
    let typed = serde_json::json!({"type": "text", "text": speech::WHAT_TIME_IS_IT}).to_string();
    let path = format!("/v1/sessions/{name}/input");
    assert_eq!(daemon.ask("POST", &path, Some(&typed)).await.status, 202);
    let written = events.rest().await;
    // What was told before the greeting was committed is not looked at
    // twice: the rest holds the question and the answer.
    let written = facts_of(&written);
    assert_eq!(conversation(&written), AS_EVER[1..], "{}", daemon.errors());

    // On a line: a client on a WebSocket, which speaks Opus.
    let (name, token) = daemon.session().await;
    let audio = AudioFormat {
        codec: Codec::Opus,
        rate: 48_000,
    };
    let hello = Hello {
        token: Some(token),
        ..Hello::new(Some(audio))
    };
    let address = format!("ws://{}", daemon.address);
    let mut client = Client::connect(&address, &hello, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(client.welcome().session, name);
    // The time a greeting takes of nothing, and the question.
    let rate = Rate::Hz48000;
    let question = speech::framed(rate, 2_000, &speech::what_time_is_it(rate), 600);
    let call = tokio::time::timeout(Duration::from_secs(60), talks(&mut client, &question)).await;
    let call = call.unwrap_or_else(|_| panic!("the call did not end: {}", daemon.errors()));
    assert_eq!(call.reason, Some(ByeReason::Ended), "{call:#?}");
    assert_eq!(conversation(&call.facts), AS_EVER, "{}", daemon.errors());
    assert_eq!(call.runs, (2, 2), "{call:#?}");

    // On the telephone: Twilio asks what to do with a call, and brings
    // it to the stream it is answered with.
    let ringing = Ringing::made_up(1);
    assert_eq!(settled(&daemon, 0).await, [] as [String; 0]);
    let answered = read(&asks(&daemon, &ringing).await);
    let made = sessions(&daemon).await;
    let [name] = made.as_slice() else {
        panic!("a call that is answered has a session, and the others are over: {made:?}");
    };
    let mut sim = brings(&daemon, ringing, &answered).await;
    let mut events = follows(&daemon, name).await;
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{name}"), None)
        .await
        .json();
    assert_eq!(info["mode"], "voice", "{info}");

    let rate = Rate::Hz8000;
    let question = speech::framed(rate, 2_000, &speech::what_time_is_it(rate), 600);
    let began = Instant::now();
    let heard = tokio::time::timeout(
        Duration::from_secs(60),
        on_the_telephone(&mut sim, &question, || false),
    )
    .await;
    let heard = heard.unwrap_or_else(|_| panic!("the call did not end: {}", daemon.errors()));
    let all = format!("{heard:?}\n{}", daemon.errors());

    // The same conversation, with every word of the agent's heard to
    // its end: what the carrier said it played is what the session
    // went by.
    let told = facts_of(&events.rest().await);
    assert_eq!(conversation(&told), AS_EVER, "{all}");
    // The caller heard a voice: 0.4 s of a greeting and 1.2 s of an
    // answer, at the rate of a telephone, and most of it above a
    // whisper.
    assert_eq!(heard.played, 12_800, "{all}");
    assert!(heard.loud > 8_000, "{all}");
    // It took as long as it takes to say, and not as long as a call
    // that nobody ends.
    assert!(began.elapsed() < Duration::from_secs(30), "{all}");
    // The carrier was sent nothing it warns of.
    assert_eq!(sim.complaints(), [] as [String; 0], "{all}");
    // The recogniser was sent what each of the two that spoke said.
    let heard_by = providers.stt.connections();
    assert_eq!(heard_by.len(), 2, "{all}");
    for connection in &heard_by {
        let sent = connection
            .texts()
            .filter(|text| text.contains("input_audio_buffer.append"))
            .count();
        assert!(sent > 20, "{sent}\n{all}");
    }
    // Nothing is left of the three, and the daemon had nothing to say
    // of them.
    assert_eq!(settled(&daemon, 0).await, [] as [String; 0]);
    assert!(!daemon.errors().contains("could not"), "{all}");
}

#[tokio::test]
async fn the_session_of_a_call_begins_when_the_call_is_brought_and_not_before() {
    let providers = Providers::start(&[Rate::Hz8000], 0).await;
    let daemon = providers.daemon("").await;
    let ringing = Ringing::made_up(11);
    let answered = read(&asks(&daemon, &ringing).await);
    let made = sessions(&daemon).await;
    let [name] = made.as_slice() else {
        panic!("a call that is answered has a session: {made:?}");
    };
    let at = format!("/v1/sessions/{name}");
    let waits = |refused: &Answer| {
        assert_eq!(
            (refused.status, refused.code().as_str()),
            (409, "out_of_place"),
            "{refused:?}"
        );
        assert!(refused.body.contains("made for a call"), "{refused:?}");
    };

    // Whoever follows the session, or writes to it, does not make it
    // one that is written to: it waits for its call.
    waits(&daemon.follow(name, "", None, &[]).await.err().unwrap());
    let written = r#"{"type":"text","text":"Anybody?"}"#;
    let input = format!("{at}/input");
    waits(&daemon.ask("POST", &input, Some(written)).await);
    let info = daemon.ask("GET", &at, None).await.json();
    assert_eq!(info["state"], "waiting", "{info}");

    // Its far end is a telephone: a client that was given a token for
    // it is put on no line.
    let minted = daemon.ask("POST", &format!("{at}/tokens"), None).await;
    assert_eq!(minted.status, 201, "{minted:?}");
    let token = minted.json()["client_token"].as_str().unwrap().to_owned();
    let hello = Hello {
        token: Some(token),
        ..Hello::new(None)
    };
    let address = format!("ws://{}", daemon.address);
    let refused = Client::connect(&address, &hello, Duration::from_secs(10)).await;
    let refused = format!("{:?}", refused.err().unwrap());
    assert!(refused.contains("OutOfPlace"), "{refused}");
    let info = daemon.ask("GET", &at, None).await.json();
    assert_eq!(info["state"], "waiting", "{info}");

    // The call is brought, and the session begins: the caller hears
    // the agent, and whoever follows the session is told of it.
    let mut sim = brings(&daemon, ringing, &answered).await;
    let mut events = follows(&daemon, name).await;
    hears_the_agent(&mut sim, &daemon).await;
    let info = daemon.ask("GET", &at, None).await.json();
    assert_eq!(
        (&info["state"], &info["mode"]),
        (&"running".into(), &"voice".into()),
        "{info}"
    );
    assert!(events.next().await.is_some());
    // The model was asked nothing by any of this.
    assert_eq!(providers.llm.requests().len(), 0);
}

#[tokio::test]
async fn a_caller_who_hangs_up_ends_the_session_of_the_call() {
    let providers = Providers::start(&[Rate::Hz8000], 0).await;
    let daemon = providers.daemon("").await;
    let ringing = Ringing::made_up(9);
    let answered = read(&asks(&daemon, &ringing).await);
    let made = sessions(&daemon).await;
    let [name] = made.as_slice() else {
        panic!("a call that is answered has a session: {made:?}");
    };
    let mut sim = brings(&daemon, ringing, &answered).await;
    let mut events = follows(&daemon, name).await;

    // The agent is greeting when the caller hangs up.
    hears_the_agent(&mut sim, &daemon).await;
    sim.hangs_up().await.unwrap();

    // Nobody comes back to a telephone call: the session is over at
    // once, whoever follows it is told so, and nothing is kept of it.
    let rest = tokio::time::timeout(Duration::from_secs(20), events.rest()).await;
    let rest = rest.unwrap_or_else(|_| panic!("the session went on: {}", daemon.errors()));
    let last = rest.last().unwrap();
    assert_eq!(
        last.json(),
        serde_json::json!({"type": "bye", "reason": "ended"}),
        "{rest:#?}"
    );
    assert_eq!(settled(&daemon, 0).await, [] as [String; 0]);
    assert_eq!(broken(&daemon).await, serde_json::json!([]));
    // The model was asked nothing: nobody had said anything.
    assert_eq!(providers.llm.requests().len(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn a_daemon_that_is_told_to_stop_takes_no_call_and_ends_the_one_it_has() {
    // An answer that takes a while to be written: long enough for a
    // machine that is slow today to ask the daemon three things before
    // it has ended.
    let slowly = crate::slow_answer(&SAID, Duration::from_millis(400));
    let providers = Providers::answering(&[Rate::Hz8000], vec![slowly]).await;
    // The caller is played nothing more once the daemon is told to
    // stop, so the answer does not end by itself: the daemon waits this
    // long for it, which is the time there is to ask it things.
    let config = providers.config("", "[limits]\ndrain_ms = 3000\n");
    let mut daemon = Served::start(&desk(), &config, &[(TOKEN_ENV, TOKEN)]).await;
    let ringing = Ringing::made_up(12);
    let answered = read(&asks(&daemon, &ringing).await);
    let mut sim = brings(&daemon, ringing, &answered).await;
    // The caller asks, and the answer is under way when the daemon is
    // told to stop.
    let rate = Rate::Hz8000;
    let question = speech::framed(rate, 2_000, &speech::what_time_is_it(rate), 600);
    let asked = || !providers.llm.requests().is_empty();
    let call = on_the_telephone(&mut sim, &question, asked);
    let call = tokio::time::timeout(Duration::from_secs(60), call).await;
    call.unwrap_or_else(|_| panic!("the model was not asked: {}", daemon.errors()));
    assert_eq!(providers.llm.requests().len(), 1, "{}", daemon.errors());
    daemon.tell_to_stop();
    let mut health = daemon.ask("GET", "/healthz", None).await;
    for _ in 0..200 {
        if health.status == 503 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        health = daemon.ask("GET", "/healthz", None).await;
    }
    assert_eq!(health.status, 503, "{health:?}");

    // From then on it answers no call, and takes no stream: a carrier
    // that is told so tries again, or does what it was told to do when
    // nobody answers.
    let busy = |refused: &Answer| {
        assert_eq!(
            (refused.status, refused.code().as_str()),
            (503, "busy"),
            "{refused:?}"
        );
        assert_eq!(refused.header("retry-after"), Some("1"));
    };
    busy(&asks(&daemon, &Ringing::made_up(13)).await);
    let upgrade = [
        ("Host", "voice.example"),
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];
    busy(&ask(&daemon.address, "GET", STREAM, &upgrade, None).await);

    // The call it has is over for its caller when the daemon goes.
    is_over(&mut sim).await;
    let ended = daemon.ended().await;
    assert!(ended.success(), "{ended:?}: {}", daemon.errors());
    let errors = daemon.errors();
    assert!(errors.contains("sessions that were stopped: 1"), "{errors}");
    assert!(errors.ends_with("waveportd: stopped"), "{errors}");
}

#[tokio::test]
async fn a_call_whose_daemon_was_killed_is_over_and_is_kept_until_it_is_let_go_of() {
    let providers = Providers::start(&[Rate::Hz8000], 0).await;
    let desk = desk();
    let config = providers.config("", "[journal]\nfile = \"sessions.redb\"\n");
    let token = [(TOKEN_ENV, TOKEN)];
    let mut daemon = Served::start(&desk, &config, &token).await;
    let ringing = Ringing::made_up(10);
    let answered = read(&asks(&daemon, &ringing).await);
    let made = sessions(&daemon).await;
    let [name] = made.as_slice() else {
        panic!("a call that is answered has a session: {made:?}");
    };
    let mut sim = brings(&daemon, ringing.clone(), &answered).await;
    hears_the_agent(&mut sim, &daemon).await;
    daemon.kill();
    // The stream is gone with the daemon, and the call with it.
    is_over(&mut sim).await;
    drop(daemon);

    // The daemon that is started next has the session as one that
    // broke off on a line. Nobody goes on with it: its far end was a
    // telephone, and whoever follows it does not make it one that is
    // written to.
    let daemon = Served::start(&desk, &config, &token).await;
    let at = format!("/v1/sessions/{name}");
    let kept = serde_json::json!({"session": name, "agent": "desk", "state": "broken"});
    assert_eq!(broken(&daemon).await, serde_json::json!([kept]));
    let followed = daemon.follow(name, "", None, &[]).await.err().unwrap();
    assert_eq!(
        (followed.status, followed.code().as_str()),
        (409, "out_of_place")
    );
    // A stream that says what the answer of the daemon that is gone
    // gave it is let into nothing.
    let mut late = brings(&daemon, ringing, &answered).await;
    assert!(is_turned_away(&mut late).await);
    assert_eq!(broken(&daemon).await, serde_json::json!([kept]));

    // Whoever made the daemon lets go of it.
    assert_eq!(daemon.ask("DELETE", &at, None).await.status, 204);
    assert_eq!(broken(&daemon).await, serde_json::json!([]));
}

#[tokio::test]
async fn a_request_that_is_not_the_carrier_s_is_not_answered() {
    let providers = Providers::start(&[Rate::Hz8000], 0).await;
    let daemon = providers
        .daemon("account = \"AC00000000000000000000000000000001\"")
        .await;
    let ringing = Ringing::made_up(2);
    let form = ringing.form(TWILIO);
    let asked_at = format!("{PUBLIC}{VOICE}");

    // Signed with another token, for another address, and with a field
    // that was changed on the way.
    let other = posts(&daemon, &form, "another-made-up-token", &asked_at).await;
    assert_eq!((other.status, other.code().as_str()), (403, "forbidden"));
    let elsewhere = format!("https://elsewhere.example{VOICE}");
    let moved = posts(&daemon, &form, TOKEN, &elsewhere).await;
    assert_eq!(moved.status, 403, "{moved:?}");
    let signature = TWILIO.sign(&Secret::new(TOKEN), &asked_at, &form);
    let mut changed = form.clone();
    changed.push(("Extra".to_owned(), "1".to_owned()));
    let signed = [
        ("Host", "voice.example"),
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("X-Twilio-Signature", signature.as_str()),
    ];
    let tampered = ask(
        &daemon.address,
        "POST",
        VOICE,
        &signed,
        Some(&posted(&changed)),
    )
    .await;
    assert_eq!(tampered.status, 403, "{tampered:?}");
    // What the request was signed with is in no answer.
    for refused in [&other, &moved, &tampered] {
        assert!(!refused.body.contains(&signature), "{refused:?}");
        assert!(!refused.body.contains(TOKEN), "{refused:?}");
    }

    // Not signed at all, and not a form.
    let unsigned = [
        ("Host", "voice.example"),
        ("Content-Type", "application/x-www-form-urlencoded"),
    ];
    let bare = ask(
        &daemon.address,
        "POST",
        VOICE,
        &unsigned,
        Some(&posted(&form)),
    )
    .await;
    assert_eq!(bare.status, 403, "{bare:?}");
    let json = [
        ("Host", "voice.example"),
        ("X-Twilio-Signature", signature.as_str()),
    ];
    let no_form = ask(&daemon.address, "POST", VOICE, &json, Some("{}")).await;
    assert_eq!(no_form.status, 415, "{no_form:?}");
    // Said to be a form and none, and larger than a body may be.
    let broken = ask(&daemon.address, "POST", VOICE, &signed, Some("CallSid=%zz")).await;
    assert_eq!(
        (broken.status, broken.code().as_str()),
        (400, "malformed"),
        "{broken:?}"
    );
    let large = format!("{}&Note={}", posted(&form), "x".repeat(64 * 1024));
    let large = ask(&daemon.address, "POST", VOICE, &signed, Some(&large)).await;
    assert_eq!(large.status, 413, "{large:?}");

    // Signed, and of another account than the one whose calls are
    // answered, or of no call at all.
    let mut theirs = Ringing::made_up(3);
    theirs.account = "AC00000000000000000000000000000002".to_owned();
    let theirs = asks(&daemon, &theirs).await;
    assert_eq!(theirs.status, 403, "{theirs:?}");
    let nameless: Vec<(String, String)> = form
        .iter()
        .filter(|(name, _)| name != "CallSid")
        .cloned()
        .collect();
    let nameless = posts(&daemon, &nameless, TOKEN, &asked_at).await;
    assert_eq!(
        (nameless.status, nameless.code().as_str()),
        (400, "malformed")
    );

    // A carrier the daemon was not told of, and a name that is none.
    for path in ["/v1/telephony/telnyx/voice", "/v1/telephony/Twilio/voice"] {
        let nobody = ask(&daemon.address, "POST", path, &signed, Some(&posted(&form))).await;
        assert_eq!(nobody.status, 404, "{path}: {nobody:?}");
    }
    // None of these made a session.
    assert_eq!(sessions(&daemon).await, [] as [String; 0]);

    // And the one that is the carrier's is answered.
    let answered = read(&asks(&daemon, &ringing).await);
    assert_eq!(sessions(&daemon).await.len(), 1);

    // Its stream says which account it is of, too: one of another is
    // not let in, though it says what the answer gave it to say.
    let mut stray = ringing.clone();
    stray.account = "AC00000000000000000000000000000002".to_owned();
    let mut stray = brings(&daemon, stray, &answered).await;
    assert!(is_turned_away(&mut stray).await);
}

#[tokio::test]
async fn a_call_is_answered_once_and_its_stream_is_let_in_once() {
    let providers = Providers::start(&[Rate::Hz8000], 0).await;
    let daemon = providers.daemon("").await;

    // A carrier that asks twice before its stream has come is told the
    // same, and there is one session for the call.
    let ringing = Ringing::made_up(4);
    let first = asks(&daemon, &ringing).await;
    // A form is one however the name of its kind is written, with or
    // without more said of it.
    let asked_at = format!("{PUBLIC}{VOICE}");
    let kind = "Application/X-WWW-Form-URLEncoded";
    let again = posts_as(&daemon, &ringing.form(TWILIO), TOKEN, &asked_at, kind).await;
    let answered = read(&first);
    assert_eq!(again.body, first.body);
    assert_eq!(sessions(&daemon).await.len(), 1);
    let [(name, ticket)] = answered.parameters.as_slice() else {
        panic!("a stream is given one thing to say: {answered:?}");
    };
    assert_eq!(name, "ticket");
    assert!(ticket.starts_with("wpc1_"), "a ticket is a client token");

    // A stream that says the ticket of another call's answer is not of
    // that call, and uses the ticket up: the call it was made for is
    // not let in after it.
    let mut thief = brings(&daemon, Ringing::made_up(5), &answered).await;
    assert!(is_turned_away(&mut thief).await);
    let mut late = brings(&daemon, ringing.clone(), &answered).await;
    assert!(is_turned_away(&mut late).await);

    // A stream that says no ticket, or one that is none.
    let other = Ringing::made_up(6);
    read(&asks(&daemon, &other).await);
    let mut silent = connects(&daemon, other.clone(), true).await;
    silent.begins(&[]).await.unwrap();
    assert!(is_turned_away(&mut silent).await);
    let made_up = [("ticket".to_owned(), "wpc1_madeup".to_owned())];
    let mut guessing = connects(&daemon, other.clone(), true).await;
    guessing.begins(&made_up).await.unwrap();
    assert!(is_turned_away(&mut guessing).await);

    // Once something has come with the ticket of a call, whoever asks
    // about the call again is refused: a request that was signed once
    // is good for ever, and is not taken twice.
    let replayed = asks(&daemon, &ringing).await;
    assert_eq!(
        (replayed.status, replayed.code().as_str()),
        (409, "out_of_place")
    );
    assert_eq!(sessions(&daemon).await.len(), 2);

    // A ticket lets in the stream of its own call and of no other,
    // though the other is one the daemon answered: and the other's own
    // stream is still let in after that, and hears the agent.
    let third = Ringing::made_up(14);
    let for_third = read(&asks(&daemon, &third).await);
    let other_answered = read(&asks(&daemon, &other).await);
    let mut crossed = brings(&daemon, other.clone(), &for_third).await;
    assert!(is_turned_away(&mut crossed).await);
    let mut sim = brings(&daemon, other, &other_answered).await;
    hears_the_agent(&mut sim, &daemon).await;

    // The address of a stream says nothing.
    let asked = format!("{STREAM}?ticket={ticket}");
    let upgrade = [
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];
    let said = ask(&daemon.address, "GET", &asked, &upgrade, None).await;
    assert_eq!((said.status, said.code().as_str()), (400, "malformed"));
    // And what asks for a stream asks for a WebSocket.
    let plain = ask(&daemon.address, "GET", STREAM, &[], None).await;
    assert_eq!(plain.status, 426, "{plain:?}");
}

#[tokio::test]
async fn a_stream_that_is_not_signed_is_said_once_or_turned_away_if_the_daemon_is_told_to() {
    // Left as it is, a stream is let in by its ticket, and one whose
    // request is not signed is said where the daemon runs.
    let providers = Providers::start(&[Rate::Hz8000], 0).await;
    let daemon = providers.daemon("").await;
    let ringing = Ringing::made_up(7);
    read(&asks(&daemon, &ringing).await);
    let mut unsigned = connects(&daemon, ringing.clone(), false).await;
    let said = daemon.noted("was not signed as twilio signs").await;
    assert!(said.contains("signed_streams"), "{said}");
    // It is still to say its ticket, and one that says none is closed.
    unsigned.begins(&[]).await.unwrap();
    assert!(is_turned_away(&mut unsigned).await);
    // A second one is not said again.
    let _second = connects(&daemon, ringing.clone(), false).await;
    let _third = connects(&daemon, ringing.clone(), true).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let times = daemon.errors().matches("was not signed").count();
    assert_eq!(times, 1, "{}", daemon.errors());

    // Told to, the daemon takes no stream that is not signed.
    let strict = providers.daemon("signed_streams = true").await;
    let at = format!("ws://{}{STREAM}", strict.address);
    let refused = Sim::connects(TWILIO, &at, &[], ringing.clone()).await;
    let refused = refused.unwrap_err();
    assert!(refused.message.contains("403"), "{refused}");
    let signed = connects(&strict, ringing, true).await;
    drop(signed);
    assert!(
        !strict.errors().contains("was not signed"),
        "{}",
        strict.errors()
    );
}

#[tokio::test]
async fn a_daemon_that_is_told_of_no_carrier_answers_no_calls() {
    let llm = HttpServer::start(Vec::new()).await;
    let daemon = Served::start(&desk(), &crate::config(&llm, ""), &[]).await;
    let form = Ringing::made_up(8).form(TWILIO);
    let headers = [("Content-Type", "application/x-www-form-urlencoded")];
    let asked = ask(
        &daemon.address,
        "POST",
        VOICE,
        &headers,
        Some(&posted(&form)),
    )
    .await;
    assert_eq!(asked.status, 404, "{asked:?}");
    let stream = ask(&daemon.address, "GET", STREAM, &[], None).await;
    assert_eq!(stream.status, 404, "{stream:?}");
}

#[test]
fn a_daemon_that_cannot_answer_the_calls_it_is_told_to_does_not_start() {
    // An agent that hears and speaks with servers on this machine,
    // which need no key and are not asked anything here.
    let speaks = "[agents.desk.llm]\nurl = \"http://127.0.0.1:9/v1\"\nmodel = \"a-model\"\n\
                  [agents.desk.speech.stt]\nprovider = \"openai\"\n\
                  url = \"ws://127.0.0.1:9/v1/realtime\"\n\
                  [agents.desk.speech.tts]\nprovider = \"cartesia\"\n\
                  url = \"ws://127.0.0.1:9/tts/websocket\"\n";
    let twilio = |more: &str| {
        format!(
            "[telephony]\npublic = \"{PUBLIC}\"\n\
             [telephony.twilio]\nagent = \"desk\"\ntoken_env = \"{TOKEN_ENV}\"\n{more}"
        )
    };
    // The variable that is to hold the token holds nothing. What a
    // variable holds is not said, and neither is a token that somebody
    // wrote where the name of its variable belongs.
    let config = format!("assets = '{}'\n{}{speaks}", assets().display(), twilio(""));
    let (status, said) = refused_to_start(&config, &[(TOKEN_ENV, " ")]);
    assert!(!status.success());
    assert!(
        said.contains(TOKEN_ENV) && said.contains("no auth token"),
        "{said}"
    );

    for (config, why) in [
        // An agent that only reads and writes, and one there is none of.
        (
            format!("{}[agents.desk]\n", twilio("")),
            "neither hears nor speaks",
        ),
        (
            format!("{}[agents.other]\n", twilio("")),
            "no agent of that name",
        ),
        // Nowhere for the carrier to reach the daemon, and somewhere it
        // cannot be reached for a stream.
        (
            format!("[telephony.twilio]\nagent = \"desk\"\ntoken_env = \"X\"\n{speaks}"),
            "`telephony.public` does not say",
        ),
        (
            format!("[telephony]\npublic = \"http://voice.example\"\n{speaks}"),
            "`telephony.public` is the address",
        ),
        (
            format!("[telephony]\npublic = \"https://voice.example/calls\"\n{speaks}"),
            "`telephony.public` is the address",
        ),
        (
            format!("{}token = \"{TOKEN}\"\n{speaks}", twilio("")),
            "at line",
        ),
    ] {
        let (status, said) = refused_to_start(&config, &[(TOKEN_ENV, TOKEN)]);
        assert!(!status.success(), "{config}");
        assert!(said.contains(why), "{config}\n{said}");
        assert!(!said.contains(TOKEN), "{said}");
    }
}
