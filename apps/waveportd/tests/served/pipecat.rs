//! A session whose far end is a client of Pipecat's.
//!
//! The far end is the test, with the client the WebRTC transport has
//! for a far end that speaks another protocol: it asks as the stock
//! client's `SmallWebRTC` transport asks (`POST /start`, `POST
//! /api/offer`, `PATCH`), makes the call the answer describes, opens
//! the data channel `chat` itself, and speaks RTVI on it, in the words
//! the spike recorded of the stock client. No browser is here: the
//! stock client in one is the test of the JavaScript side.

use core::time::Duration;

use serde_json::{Value, json};
use waveport_media::codec::opus::{MAX_PACKET, OpusDecoder, OpusEncoder, OpusSettings};
use waveport_ports::audio::Rate;
use waveport_testkit::servers::{HttpServer, WsServer, WsStep};
use waveport_testkit::speech;
use waveport_testkit::stand_in::{HEARD, SAID, spoken};
use waveport_transport_webrtc::{Dialer, Like, Strange, Stranger};

use crate::dialled::{CALLED, PACKET, Providers, SOON, daemon, question};
use crate::far_end::{Answer, ask};
use crate::{Served, answer};

/// What the client's transport is told when its session is over.
const PEER_LEFT: &str = r#"{"type":"signalling","message":{"type":"peerLeft"}}"#;

/// What the stock client says when its channel is open, as the spike
/// recorded it.
const READY: &str = r#"{"label":"rtvi-ai","type":"client-ready","data":{"version":"2.3.0","about":{"library":"@pipecat-ai/client-js","library_version":"1.16.0","platform_details":{"browser":"Chrome","browser_version":"157.0.0.0","platform_type":"desktop","engine":"Blink"},"platform":"Linux"}},"id":"0dca44e2"}"#;

/// The providers of `sessions` sessions that are written to `turns`
/// times between them, answer each aloud, and are not ended by their
/// agent.
async fn written_to(turns: usize, sessions: usize) -> Providers {
    let voices = || (0..turns).flat_map(|_| spoken()).collect::<Vec<_>>();
    Providers {
        stt: WsServer::start(vec![WsStep::script(HEARD); sessions]).await,
        llm: HttpServer::start((0..turns.max(1)).map(|_| answer(&SAID, None)).collect()).await,
        tts: WsServer::start((0..sessions).map(|_| voices()).collect()).await,
    }
}

/// Asks the daemon as a page does that says `bearer`, if it says
/// anything of who it is.
async fn asks(
    daemon: &Served,
    bearer: Option<&str>,
    method: &str,
    path: &str,
    body: &Value,
) -> Answer {
    let bearer = bearer.map(|bearer| format!("Bearer {bearer}"));
    let headers: Vec<(&str, &str)> = bearer
        .iter()
        .map(|bearer| ("Authorization", bearer.as_str()))
        .collect();
    let body = body.to_string();
    ask(&daemon.address, method, path, &headers, Some(&body)).await
}

/// An offer as a client of Pipecat's makes one, of a call nobody makes.
async fn an_offer() -> String {
    let local = "127.0.0.1:0".parse().unwrap();
    let offered = Dialer::offer_as_a_stranger(Like::Chrome, local, false, "chat").await;
    offered.unwrap().1
}

/// The body of an offer, as the client posts it.
fn offer_of(sdp: &str, call: Option<&str>, restart: bool) -> Value {
    json!({"sdp": sdp, "type": "offer", "pc_id": call, "restart_pc": restart})
}

/// A client of Pipecat's on a call.
struct Client {
    stranger: Stranger,
    /// The name its call was answered with.
    call: String,
    /// The offer it made, as it posted it.
    offer: String,
    /// Whether its transport was told that its peer left.
    left: bool,
    /// How many of the samples it decoded were of a voice.
    loud: usize,
    decoder: OpusDecoder,
}

impl Client {
    /// Offers a call at `path` and makes it. `call` and `restart` are
    /// what the offer says of a call there was.
    async fn calls(
        daemon: &Served,
        bearer: Option<&str>,
        path: &str,
        call: Option<&str>,
        restart: bool,
    ) -> Self {
        let local = "127.0.0.1:0".parse().unwrap();
        let offered = Dialer::offer_as_a_stranger(Like::Chrome, local, false, "chat").await;
        let (dialing, offer) = offered.unwrap();
        let answered = asks(
            daemon,
            bearer,
            "POST",
            path,
            &offer_of(&offer, call, restart),
        )
        .await;
        assert_eq!(answered.status, 200, "{answered:?}\n{}", daemon.errors());
        // The answer is what a browser sets as its far end's
        // description, and names the call.
        let answer = answered.json();
        assert_eq!(answer["type"], "answer");
        assert_eq!(answered.header("cache-control"), Some("no-store"));
        let call = answer["pc_id"].as_str().unwrap().to_owned();
        assert!(
            call.len() == 32 && call.chars().all(|c| c.is_ascii_hexdigit()),
            "{call}"
        );
        let sdp = answer["sdp"].as_str().unwrap();
        let stranger = dialing
            .opened(sdp, SOON)
            .await
            .unwrap_or_else(|unconnected| panic!("{unconnected}: {}", daemon.errors()));
        Self {
            stranger,
            call,
            offer,
            left: false,
            loud: 0,
            decoder: OpusDecoder::new(Rate::Hz48000).unwrap(),
        }
    }

    /// Says a message of the kind `kind` with the id `id`.
    fn says(&mut self, kind: &str, id: &str, data: &Value) {
        let message = json!({"label": "rtvi-ai", "type": kind, "id": id, "data": data});
        self.stranger.say(&message.to_string()).unwrap();
    }

    /// Takes what the call brought. Returns a message of RTVI if it
    /// was one.
    fn takes(&mut self, brought: Strange) -> Option<Value> {
        match brought {
            Strange::Text(text) if text == PEER_LEFT => self.left = true,
            Strange::Text(text) => {
                let message: Value = serde_json::from_str(&text).unwrap();
                // Whatever else is said on the channel is RTVI.
                assert_eq!(message["label"], "rtvi-ai", "{text}");
                return Some(message);
            }
            Strange::Audio(packet) => {
                let mut pcm = [0_i16; 5_760];
                let samples = self.decoder.decode(&packet, &mut pcm).unwrap();
                self.loud += pcm[..samples]
                    .iter()
                    .filter(|sample| sample.unsigned_abs() > 1_000)
                    .count();
            }
            _ => {}
        }
        None
    }

    /// Says `speech`, a packet every 20 ms, and then silence, until it
    /// is told something that is `last`, or the call is over. Returns
    /// the messages it was told meanwhile. With no speech it sends
    /// nothing, as a page does whose microphone is off: the recogniser
    /// here is a recording, which says its words to whatever it is
    /// sent.
    async fn talks(&mut self, speech: &[i16], last: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut encoder = OpusEncoder::new(Rate::Hz48000, OpusSettings::default()).unwrap();
        let muted = speech.is_empty();
        let mut frames = speech.chunks(PACKET);
        let mut every = tokio::time::interval(Duration::from_millis(20));
        let mut told = Vec::new();
        let heard = async {
            loop {
                tokio::select! {
                    _ = every.tick(), if !muted => {
                        let mut frame = [0_i16; PACKET];
                        if let Some(next) = frames.next() {
                            frame[..next.len()].copy_from_slice(next);
                        }
                        let mut packet = [0_u8; MAX_PACKET];
                        let length = encoder.encode(&frame, &mut packet).unwrap();
                        // A call that is over takes nothing, and says
                        // so below.
                        let _ = self.stranger.audio(&packet[..length]);
                    }
                    brought = self.stranger.next() => {
                        let Some(brought) = brought else {
                            return;
                        };
                        if let Some(message) = self.takes(brought) {
                            let done = last(&message);
                            told.push(message);
                            if done {
                                return;
                            }
                        }
                    }
                }
            }
        };
        let in_time = tokio::time::timeout(Duration::from_secs(60), heard).await;
        assert!(in_time.is_ok(), "the call told only {told:#?}");
        told
    }

    /// Listens until it is told something that is `last`, or the call
    /// is over.
    async fn listens(&mut self, last: impl Fn(&Value) -> bool) -> Vec<Value> {
        self.talks(&[], last).await
    }

    /// Says that it is ready, and is answered.
    async fn is_ready(&mut self) -> Value {
        self.stranger.say(READY).unwrap();
        let told = self.listens(|message| message["type"] == "bot-ready").await;
        told.last().cloned().unwrap()
    }

    /// What it is told for `time`, which may be nothing.
    async fn hears_for(&mut self, time: Duration) -> Vec<Value> {
        let mut told = Vec::new();
        let listened = async {
            while let Some(brought) = self.stranger.next().await {
                told.extend(self.takes(brought));
            }
        };
        let _ = tokio::time::timeout(time, listened).await;
        told
    }

    /// The call is over for it.
    async fn is_over(&mut self) {
        let over = async { while self.stranger.next().await.is_some() {} };
        tokio::time::timeout(SOON, over).await.unwrap();
    }
}

/// The daemon said nothing but what it says as it starts: that it has
/// no control key, and where its calls are taken.
fn went_well(daemon: &Served) {
    let errors = daemon.errors();
    for line in errors.lines() {
        let note = line.contains("there is no control key") || line.contains("calls over WebRTC:");
        assert!(note, "{errors}");
    }
}

/// The kinds of the messages `told`, in order.
fn kinds(told: &[Value]) -> Vec<&str> {
    told.iter()
        .map(|message| message["type"].as_str().unwrap())
        .collect()
}

/// The messages of the kind `kind` among `told`.
fn of_kind<'a>(told: &'a [Value], kind: &str) -> Vec<&'a Value> {
    told.iter()
        .filter(|message| message["type"] == kind)
        .collect()
}

/// Where the first message of the kind `kind` is among `told`.
fn place(told: &[Value], kind: &str) -> usize {
    let found = told.iter().position(|message| message["type"] == kind);
    found.unwrap_or_else(|| panic!("no {kind} in {:?}", kinds(told)))
}

/// The sessions of the daemon, by name.
async fn sessions(daemon: &Served) -> Vec<String> {
    let listed = daemon.ask("GET", "/v1/sessions", None).await.json();
    let listed = listed["sessions"].as_array().unwrap();
    listed
        .iter()
        .map(|session| session["session"].as_str().unwrap().to_owned())
        .collect()
}

/// Waits until the daemon has no session.
async fn has_none(daemon: &Served) {
    let began = std::time::Instant::now();
    while !sessions(daemon).await.is_empty() {
        assert!(began.elapsed() < SOON, "{}", daemon.errors());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one call from its offer to its end, told in the order it happens"
)]
async fn a_client_of_pipecat_s_asks_aloud_and_is_told_in_rtvi_what_happens() {
    let providers = Providers::of_one_call().await;
    let daemon = daemon(&providers, CALLED).await;
    // An address is all the client is given: the agent is named in it.
    let mut client = Client::calls(&daemon, None, "/api/offer?agent=desk", None, false).await;
    // A session was made for it, which hears and speaks.
    let made = sessions(&daemon).await;
    assert_eq!(made.len(), 1, "{made:?}");
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{}", made[0]), None)
        .await
        .json();
    assert_eq!(
        (&info["mode"], &info["agent"]),
        (&json!("voice"), &json!("desk"))
    );
    // Neither the session nor a token of it is what the call is named.
    assert_ne!(client.call, made[0]);
    // Where the client says it is, is heard and not needed.
    let candidate = json!({"candidate": "candidate:1 1 udp 1 192.0.2.7 9 typ host", "sdp_mid": "0", "sdp_mline_index": 0});
    let trickled = json!({"pc_id": client.call, "candidates": [candidate]});
    let heard = asks(&daemon, None, "PATCH", "/api/offer", &trickled).await;
    assert_eq!(
        (heard.status, heard.json()),
        (200, json!({"status": "success"}))
    );

    // What the stock client says first on its channel: the state of its
    // three tracks, which is its transport's, and that it is ready.
    for index in 0..3 {
        let status = json!({"type": "trackStatus", "receiver_index": index, "enabled": index == 0});
        let said = json!({"type": "signalling", "message": status});
        client.stranger.say(&said.to_string()).unwrap();
    }
    let ready = client.is_ready().await;
    assert_eq!(ready["id"], "0dca44e2");
    assert_eq!(ready["data"]["version"], "2.3.0");
    assert_eq!(ready["data"]["about"]["library"], "waveport");
    let does = &ready["data"]["capabilities"];
    assert_eq!(
        (&does["audio_in"], &does["audio_out"]),
        (&json!(true), &json!(true))
    );
    assert_eq!(
        (&does["video_in"], &does["video_out"]),
        (&json!(false), &json!(false))
    );
    // And once a second after that, which is answered with nothing.
    client.stranger.say("ping: 1794862249583").unwrap();

    // It asks the time aloud, and is told what happens until its peer
    // leaves: the agent ends the call when it has answered.
    let told = client.talks(&question(), |_| false).await;
    let errors = daemon.errors();
    let all = format!("{:#?}\n{errors}", kinds(&told));
    assert!(client.left, "{all}");
    // What was said and what was answered, as the client is told them.
    let asked = of_kind(&told, "user-input");
    let last = asked.last().unwrap_or_else(|| panic!("{all}"));
    assert_eq!(last["data"]["text"], speech::WHAT_TIME_IS_IT, "{all}");
    assert_eq!(last["data"]["final"], true);
    assert_eq!(last["data"]["input_type"], "transcription");
    assert_eq!(last["data"]["user_id"], "0");
    let stamp = last["data"]["timestamp"].as_str().unwrap();
    assert!(
        stamp.len() == 24 && stamp.starts_with("20") && stamp.ends_with('Z'),
        "{stamp}"
    );
    // The deprecated message says the same, for the callback that
    // listens to it.
    let deprecated = of_kind(&told, "user-transcription");
    let last = deprecated.last().unwrap_or_else(|| panic!("{all}"));
    assert_eq!(last["data"]["text"], speech::WHAT_TIME_IS_IT);
    assert_eq!(last["data"]["final"], true);
    // The tool the agent called, by its name and the number of the
    // call, with nothing of what it was given or answered.
    let called = of_kind(&told, "llm-function-call-started");
    assert_eq!(
        called[0]["data"],
        json!({"function_name": "get_time"}),
        "{all}"
    );
    let stopped = of_kind(&told, "llm-function-call-stopped");
    assert_eq!(stopped[0]["data"]["function_name"], "get_time");
    assert_eq!(stopped[0]["data"]["cancelled"], false);
    assert_eq!(stopped[0]["data"].get("result"), None, "{all}");
    let sent = of_kind(&told, "llm-function-call-in-progress");
    assert_eq!(
        sent[0]["data"]["tool_call_id"],
        stopped[0]["data"]["tool_call_id"]
    );
    assert_eq!(sent[0]["data"].get("arguments"), None);
    // The answer: every stretch that went out was heard to its end, and
    // what was heard adds up to what was said.
    let outputs = of_kind(&told, "bot-output");
    let went_out: String = outputs
        .iter()
        .filter(|output| output["data"]["spoken_status"] == "new")
        .map(|output| output["data"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(went_out, "It is ten past three. Goodbye!", "{all}");
    let heard: String = outputs
        .iter()
        .filter(|output| output["data"]["spoken_status"] != "new")
        .map(|output| output["data"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(heard, went_out, "{outputs:#?}");
    // The same in the message that says only what was heard.
    let heard: String = of_kind(&told, "bot-tts-text")
        .iter()
        .map(|said| said["data"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(heard, went_out, "{all}");
    let stretches: std::collections::BTreeSet<u64> = outputs
        .iter()
        .map(|output| output["data"]["segment_id"].as_u64().unwrap())
        .collect();
    for stretch in &stretches {
        let last = outputs
            .iter()
            .rfind(|output| output["data"]["segment_id"] == *stretch);
        let data = &last.unwrap()["data"];
        assert_eq!(data["spoken_status"], "completed", "{outputs:#?}");
        assert_eq!(data["spoken_progress"]["remaining_text"], "");
    }
    // In the order things happen in: the question, the model, the
    // voice, and the end of it.
    let order = [
        "user-started-speaking",
        "user-stopped-speaking",
        "bot-llm-started",
        "bot-tts-started",
        "bot-started-speaking",
        "bot-tts-stopped",
        "bot-stopped-speaking",
    ];
    let places: Vec<usize> = order.iter().map(|kind| place(&told, kind)).collect();
    assert!(places.is_sorted(), "{all}");
    assert!(
        place(&told, "bot-llm-stopped") < place(&told, "bot-stopped-speaking"),
        "{all}"
    );
    // Nothing that was begun is left open, and nothing failed.
    for (begins, ends) in [
        ("bot-llm-started", "bot-llm-stopped"),
        ("bot-tts-started", "bot-tts-stopped"),
        ("bot-started-speaking", "bot-stopped-speaking"),
    ] {
        assert_eq!(
            of_kind(&told, begins).len(),
            of_kind(&told, ends).len(),
            "{begins}: {all}"
        );
    }
    assert_eq!(of_kind(&told, "error").len(), 0, "{all}");
    assert_eq!(of_kind(&told, "error-response").len(), 0, "{all}");
    // What it said arrived as audio that decodes to a voice.
    assert!(client.loud > 40_000, "{} {all}", client.loud);
    // The session is over, and so is the call.
    client.is_over().await;
    has_none(&daemon).await;
    let gone = json!({"pc_id": client.call, "candidates": []});
    let began = std::time::Instant::now();
    loop {
        let heard = asks(&daemon, None, "PATCH", "/api/offer", &gone).await;
        if heard.status == 404 {
            assert_eq!(heard.code(), "no_session");
            break;
        }
        assert!(began.elapsed() < SOON, "{heard:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    went_well(&daemon);
}

#[tokio::test]
async fn a_client_that_writes_presses_keys_and_says_goodbye_is_taken_at_its_word() {
    let providers = written_to(1, 1).await;
    let daemon = daemon(&providers, CALLED).await;
    let mut client = Client::calls(&daemon, None, "/api/offer?agent=desk", None, false).await;
    client.is_ready().await;
    // Text is a turn, and is told back as what the client sent, by the
    // id of its message, and not as something that was recognised.
    client.says("send-text", "c1", &json!({"content": "What time is it?"}));
    let told = client
        .listens(|message| message["type"] == "bot-stopped-speaking")
        .await;
    let all = format!("{:#?}\n{}", kinds(&told), daemon.errors());
    let typed = of_kind(&told, "user-input");
    assert_eq!(typed.len(), 1, "{all}");
    assert_eq!(typed[0]["data"]["text"], "What time is it?");
    assert_eq!(typed[0]["data"]["input_type"], "chat");
    assert_eq!(typed[0]["data"]["msg_id"], "c1");
    assert_eq!(of_kind(&told, "user-transcription").len(), 0, "{all}");
    // It is answered aloud.
    assert!(client.loud > 40_000, "{} {all}", client.loud);
    let model = providers.llm.requests();
    assert_eq!(model.len(), 1, "{all}");
    assert!(model[0].body.contains("What time is it?"), "{all}");

    // What a session cannot do with text is refused by the id of the
    // message, and the call goes on.
    let silent = json!({"content": "note this", "options": {"run_immediately": false}});
    client.says("send-text", "c2", &silent);
    client.says("client-message", "m1", &json!({"t": "anything"}));
    let told = client.listens(|message| message["id"] == "m1").await;
    assert_eq!(kinds(&told), ["error-response", "error-response"]);
    assert_eq!(told[0]["id"], "c2");
    assert_eq!(providers.llm.requests().len(), 1);

    // Keys are keys: the session is told of them as of a telephone's.
    client.says("dtmf", "d1", &json!({"buttons": ["1", "2", "#"]}));
    let name = sessions(&daemon).await.remove(0);
    let events = daemon.follow(&name, "", None, &[]).await;
    let mut events = events.unwrap_or_else(|answer| panic!("{answer:?}"));
    let seen = events
        .until(|event| crate::is_fact(event, "keys_committed"))
        .await;
    // The hash ends what was keyed, as on a telephone, and is no part
    // of it.
    assert_eq!(seen.last().unwrap().json()["keys"], "12");

    // Goodbye ends the session, and the client's transport is told that
    // its peer left: it does not call again.
    client.says("disconnect-bot", "b1", &json!({}));
    client.listens(|_| false).await;
    assert!(client.left);
    client.is_over().await;
    has_none(&daemon).await;
    went_well(&daemon);
}

#[tokio::test]
async fn a_client_that_writes_while_it_is_answered_is_told_that_the_answer_was_cut_off() {
    let providers = written_to(2, 1).await;
    let daemon = daemon(&providers, CALLED).await;
    let mut client = Client::calls(&daemon, None, "/api/offer?agent=desk", None, false).await;
    client.is_ready().await;
    client.says("send-text", "c1", &json!({"content": "What time is it?"}));
    let before = client
        .listens(|message| message["type"] == "bot-started-speaking")
        .await;
    // It writes again while the answer is being said.
    client.says("send-text", "c2", &json!({"content": "And tomorrow?"}));
    let told = client
        .listens(|message| message["type"] == "bot-stopped-speaking")
        .await;
    let all = format!(
        "{:#?}\n{:#?}\n{}",
        kinds(&before),
        kinds(&told),
        daemon.errors()
    );
    // The answer was cut off, and what was begun of it is ended.
    let cut = place(&told, "bot-interrupted");
    assert!(cut < place(&told, "bot-stopped-speaking"), "{all}");
    let whole: Vec<Value> = before.iter().chain(&told).cloned().collect();
    for (begins, ends) in [
        ("bot-llm-started", "bot-llm-stopped"),
        ("bot-tts-started", "bot-tts-stopped"),
        ("bot-started-speaking", "bot-stopped-speaking"),
    ] {
        let (begun, ended) = (of_kind(&whole, begins).len(), of_kind(&whole, ends).len());
        assert!(begun > 0 && begun == ended, "{begins}: {all}");
    }
    // No stretch of it is said to have been heard to its end that was
    // not: what was heard stops where it was cut.
    let first = of_kind(&whole, "bot-output");
    let went_out: String = first
        .iter()
        .filter(|output| output["data"]["spoken_status"] == "new")
        .map(|output| output["data"]["text"].as_str().unwrap())
        .collect();
    let heard: String = of_kind(&whole, "bot-tts-text")
        .iter()
        .map(|said| said["data"]["text"].as_str().unwrap())
        .collect();
    assert!(
        went_out.starts_with(&heard) && heard.len() < went_out.len(),
        "{heard:?} of {went_out:?}: {all}"
    );
    // What it wrote then is a turn, which is answered to its end.
    let told = client
        .listens(|message| message["type"] == "bot-stopped-speaking")
        .await;
    let all = format!("{:#?}\n{}", kinds(&told), daemon.errors());
    let typed = of_kind(&told, "user-input");
    let last = typed.last().unwrap_or_else(|| panic!("{all}"));
    assert_eq!(last["data"]["msg_id"], "c2", "{all}");
    assert_eq!(of_kind(&told, "bot-interrupted").len(), 0, "{all}");
    assert_eq!(providers.llm.requests().len(), 2, "{all}");
    client.says("disconnect-bot", "b1", &json!({}));
    client.is_over().await;
    went_well(&daemon);
}

#[tokio::test]
async fn a_client_that_calls_again_is_on_the_session_it_was_on_and_is_told_nothing_twice() {
    let providers = written_to(2, 1).await;
    let daemon = daemon(&providers, CALLED).await;
    let mut first = Client::calls(&daemon, None, "/api/offer?agent=desk", None, false).await;
    first.is_ready().await;
    first.says("send-text", "c1", &json!({"content": "What time is it?"}));
    let told = first
        .listens(|message| message["type"] == "bot-stopped-speaking")
        .await;
    assert_eq!(of_kind(&told, "user-input").len(), 1);
    let session = sessions(&daemon).await;
    assert_eq!(session.len(), 1);

    // The same description, offered for the call that is there: it is
    // agreed anew, and the call is the one it was.
    let again = offer_of(&first.offer, Some(&first.call), false);
    let agreed = asks(&daemon, None, "POST", "/api/offer", &again).await;
    assert_eq!(agreed.status, 200, "{agreed:?}");
    assert_eq!(agreed.json()["pc_id"], first.call.as_str());
    assert_eq!(agreed.json()["type"], "answer");
    assert_eq!(first.is_ready().await["type"], "bot-ready");

    // Its network failed, as it sees it: it makes another call in the
    // place of the one it names. No address says an agent this time,
    // and none is needed.
    let old = first.call.clone();
    let mut second = Client::calls(&daemon, None, "/api/offer", Some(&old), true).await;
    assert_ne!(second.call, old);
    // The call before is ended for it.
    first.is_over().await;
    assert!(!first.left);
    // It is on the session it was on, and no other was made.
    assert_eq!(sessions(&daemon).await, session);
    // What happened before it came is not told again: not as it comes,
    // and not after.
    second.stranger.say(READY).unwrap();
    let told = second
        .listens(|message| message["type"] == "bot-ready")
        .await;
    assert_eq!(kinds(&told), ["bot-ready"], "{told:#?}");
    let told = second.hears_for(Duration::from_millis(400)).await;
    assert_eq!(kinds(&told), Vec::<&str>::new(), "{told:#?}");
    // What happens from here on is.
    second.says("send-text", "c2", &json!({"content": "And tomorrow?"}));
    let told = second
        .listens(|message| message["type"] == "bot-stopped-speaking")
        .await;
    let typed = of_kind(&told, "user-input");
    assert_eq!(typed.len(), 1, "{:#?}\n{}", kinds(&told), daemon.errors());
    assert_eq!(typed[0]["data"]["msg_id"], "c2");
    assert_eq!(providers.llm.requests().len(), 2);

    // Its page is closed and opened again: the call is over, and the
    // name of it still says whose session it was, for as long as the
    // session waits.
    let old = second.call.clone();
    second.stranger.hang_up();
    let gone = json!({"pc_id": old, "candidates": []});
    let began = std::time::Instant::now();
    while asks(&daemon, None, "PATCH", "/api/offer", &gone)
        .await
        .status
        != 404
    {
        assert!(began.elapsed() < SOON, "{}", daemon.errors());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut third = Client::calls(&daemon, None, "/api/offer", Some(&old), true).await;
    assert_eq!(sessions(&daemon).await, session);
    assert_eq!(third.is_ready().await["type"], "bot-ready");
    // A name that was a call's stands for its session once more: the
    // one before it does not.
    let stale = offer_of(&first.offer, Some(&first.call), true);
    let refused = asks(&daemon, None, "POST", "/api/offer", &stale).await;
    assert_eq!(refused.status, 404, "{refused:?}");
    assert_eq!(refused.code(), "no_agent");
    assert_eq!(sessions(&daemon).await, session);
    third.says("disconnect-bot", "b1", &json!({}));
    third.is_over().await;
    has_none(&daemon).await;
    went_well(&daemon);
}

#[tokio::test]
async fn a_client_is_let_in_by_what_it_holds_and_a_name_in_an_address_is_no_token() {
    let providers = written_to(0, 2).await;
    let keyed = format!("[control]\nkey_env = [\"WAVEPORTD_KEY\"]\n{CALLED}");
    let key = "made-up-control-key-of-a-test";
    let config = providers.config(&keyed);
    let daemon = Served::start(&crate::desk(), &config, &[("WAVEPORTD_KEY", key)]).await;
    let start = json!({"transport": "webrtc"});
    let offer = an_offer().await;
    let offered = offer_of(&offer, None, false);

    // Whoever says nothing of who they are is let in nowhere.
    for (method, path, body) in [
        ("POST", "/start?agent=desk", &start),
        ("POST", "/api/offer?agent=desk", &offered),
    ] {
        let refused = asks(&daemon, None, method, path, body).await;
        assert_eq!(refused.status, 401, "{path}: {refused:?}");
        assert_eq!(refused.header("www-authenticate"), Some("Bearer"));
        let refused = asks(&daemon, Some("wpc1_madeup"), method, path, body).await;
        assert_eq!(refused.status, 401, "{path}: {refused:?}");
        assert!(!refused.body.contains("madeup"));
    }
    assert_eq!(sessions_as(&daemon, key).await, Vec::<String>::new());

    // Whoever holds a client token is started on its session, under a
    // name that is neither the session nor the token.
    let made = daemon
        .ask_as(key, "POST", "/v1/sessions", Some(r#"{"agent":"desk"}"#))
        .await
        .json();
    let (session, token) = (
        made["session"].as_str().unwrap(),
        made["client_token"].as_str().unwrap(),
    );
    let started = asks(&daemon, Some(token), "POST", "/start", &start).await;
    assert_eq!(started.status, 200, "{started:?}");
    assert_eq!(started.header("cache-control"), Some("no-store"));
    let started = started.json();
    let name = started["sessionId"].as_str().unwrap().to_owned();
    assert!(
        name.len() == 32 && name != session && !token.contains(&name),
        "{name}"
    );
    // A daemon that was told of no servers tells a client of none: it
    // keeps the ones it was set up with.
    assert_eq!(started.get("iceConfig"), None);
    let under = format!("/sessions/{name}/api/offer");
    // The name alone lets nobody in: it is good with what `/start` was
    // asked with, and under no other name.
    let elsewhere = format!("/sessions/{session}/api/offer");
    for (bearer, path) in [
        (None, &under),
        (Some(key), &under),
        (Some(token), &elsewhere),
    ] {
        let refused = asks(&daemon, bearer, "POST", path, &offered).await;
        let refused = (refused.status, refused.code());
        assert_eq!(refused, (404, "no_session".to_owned()), "{path}");
    }
    assert_eq!(sessions_as(&daemon, key).await, [session]);
    // An offer that cannot be answered leaves that session where it
    // was: it was there before the offer, and is not the offer's to
    // end.
    let no_audio = offer_of(&offer.replace("m=audio", "m=video"), None, false);
    let refused = asks(&daemon, Some(token), "POST", "/api/offer", &no_audio).await;
    assert_eq!(
        (refused.status, refused.code().as_str()),
        (422, "unsupported")
    );
    assert_eq!(sessions_as(&daemon, key).await, [session]);

    // With it, the client is on the session the token was made for.
    let mut client = Client::calls(&daemon, Some(token), &under, None, false).await;
    assert_eq!(client.is_ready().await["type"], "bot-ready");
    assert_eq!(sessions_as(&daemon, key).await, [session]);
    // The token was used up by the far end that came: it starts nothing
    // more, and makes no other call.
    let began = std::time::Instant::now();
    loop {
        let again = asks(&daemon, Some(token), "POST", "/start", &start).await;
        if again.status == 401 {
            break;
        }
        assert!(began.elapsed() < SOON, "{again:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let refused = asks(&daemon, Some(token), "POST", "/api/offer", &offered).await;
    assert_eq!(refused.status, 401, "{refused:?}");
    // What it says of where it is, is its own to say: nobody else's.
    let trickled = json!({"pc_id": client.call, "candidates": []});
    let heard = asks(&daemon, Some(token), "PATCH", &under, &trickled).await;
    assert_eq!(heard.status, 200, "{heard:?}");
    for other in [None, Some(key)] {
        let refused = asks(&daemon, other, "PATCH", &under, &trickled).await;
        assert_eq!(refused.status, 404, "{refused:?}");
    }
    // Nor can another make a call in the place of this one.
    let instead = offer_of(&offer, Some(&client.call), true);
    let refused = asks(&daemon, None, "POST", "/api/offer?agent=desk", &instead).await;
    assert_eq!(refused.status, 401, "{refused:?}");
    assert_eq!(client.is_ready().await["type"], "bot-ready");

    // Whoever holds a control key has a session made, of the agent the
    // address names.
    let started = asks(&daemon, Some(key), "POST", "/start?agent=desk", &start).await;
    assert_eq!(started.status, 200, "{started:?}");
    assert_eq!(sessions_as(&daemon, key).await.len(), 2);
    let under = format!(
        "/sessions/{}/api/offer",
        started.json()["sessionId"].as_str().unwrap()
    );
    let mut keyed = Client::calls(&daemon, Some(key), &under, None, false).await;
    assert_eq!(keyed.is_ready().await["type"], "bot-ready");
    client.says("disconnect-bot", "b1", &json!({}));
    keyed.says("disconnect-bot", "b2", &json!({}));
    client.is_over().await;
    keyed.is_over().await;
    went_well(&daemon);
}

/// The sessions of a daemon that has a control key.
async fn sessions_as(daemon: &Served, key: &str) -> Vec<String> {
    let listed = daemon.ask_as(key, "GET", "/v1/sessions", None).await.json();
    let listed = listed["sessions"].as_array().unwrap();
    listed
        .iter()
        .map(|session| session["session"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn what_is_asked_wrongly_is_refused_with_why_and_makes_no_session() {
    let providers = written_to(0, 1).await;
    let daemon = daemon(&providers, CALLED).await;
    let offer = an_offer().await;
    let start = json!({"transport": "webrtc"});
    let good = offer_of(&offer, None, false);
    let mut answer = good.clone();
    answer["type"] = json!("answer");
    let no_audio = offer_of(&offer.replace("m=audio", "m=video"), None, false);
    let no_channel = offer.split("m=application").next().unwrap();
    let no_channel = offer_of(&no_channel.replace("BUNDLE 0 1", "BUNDLE 0"), None, false);
    for (method, path, body, status, code) in [
        // The daemon has two agents, and the address names none, or
        // one it has not, or one that only writes.
        ("POST", "/api/offer", &good, 404, "no_agent"),
        ("POST", "/start", &start, 404, "no_agent"),
        ("POST", "/api/offer?agent=nobody", &good, 404, "no_agent"),
        ("POST", "/start?agent=clerk", &start, 422, "unsupported"),
        ("POST", "/api/offer?agent=clerk", &good, 422, "unsupported"),
        // Another transport of Pipecat's than the one that is served.
        (
            "POST",
            "/start?agent=desk",
            &json!({"transport": "daily"}),
            422,
            "unsupported",
        ),
        // What is no offer, or one that cannot be answered.
        ("POST", "/api/offer?agent=desk", &answer, 400, "malformed"),
        (
            "POST",
            "/api/offer?agent=desk",
            &offer_of("wpc1_madeup", None, false),
            400,
            "malformed",
        ),
        (
            "POST",
            "/api/offer?agent=desk",
            &json!({"type": "offer"}),
            400,
            "malformed",
        ),
        (
            "POST",
            "/api/offer?agent=desk",
            &json!([]),
            400,
            "malformed",
        ),
        (
            "POST",
            "/api/offer?agent=desk",
            &no_audio,
            422,
            "unsupported",
        ),
        (
            "POST",
            "/api/offer?agent=desk",
            &no_channel,
            422,
            "unsupported",
        ),
        // A name nothing was started under, and a call there is not.
        (
            "POST",
            "/sessions/0123456789abcdef0123456789abcdef/api/offer",
            &good,
            404,
            "no_session",
        ),
        (
            "PATCH",
            "/api/offer",
            &json!({"pc_id": "0123456789abcdef0123456789abcdef", "candidates": []}),
            404,
            "no_session",
        ),
        (
            "PATCH",
            "/api/offer",
            &json!({"candidates": []}),
            404,
            "no_session",
        ),
        (
            "PATCH",
            "/sessions/x/api/offer",
            &json!({"pc_id": null}),
            404,
            "no_session",
        ),
    ] {
        let refused = asks(&daemon, None, method, path, body).await;
        assert_eq!(refused.status, status, "{method} {path}: {refused:?}");
        assert_eq!(refused.code(), code, "{method} {path}: {refused:?}");
        assert_eq!(
            refused.header("content-type"),
            Some("application/problem+json")
        );
        // What was sent is not said back.
        assert!(!refused.body.contains("madeup"), "{refused:?}");
    }
    // Nothing of this left a session behind: one that was made for an
    // offer that could not be answered was let go of.
    assert_eq!(sessions(&daemon).await, Vec::<String>::new());

    went_well(&daemon);
}

#[cfg(unix)]
#[tokio::test]
async fn a_daemon_that_is_told_to_stop_takes_no_client_of_pipecat_s_and_tells_the_ones_it_has() {
    use waveport_testkit::stand_in::slow_answer;

    // A model that takes its time, so that an answer is under way when
    // the daemon is told to stop, and the daemon waits for it.
    let providers = Providers {
        stt: WsServer::start(vec![WsStep::script(HEARD)]).await,
        llm: HttpServer::start(vec![slow_answer(&SAID, Duration::from_millis(150))]).await,
        tts: WsServer::start(vec![spoken()]).await,
    };
    let mut daemon = daemon(&providers, CALLED).await;
    let mut client = Client::calls(&daemon, None, "/api/offer?agent=desk", None, false).await;
    client.is_ready().await;
    // A session that was made and that nobody came to yet.
    let (_, token) = daemon.session().await;
    client.says("send-text", "c1", &json!({"content": "What time is it?"}));
    let began = std::time::Instant::now();
    while providers.llm.requests().is_empty() {
        assert!(began.elapsed() < SOON, "{}", daemon.errors());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    daemon.tell_to_stop();
    let began = std::time::Instant::now();
    while daemon.ask("GET", "/healthz", None).await.status != 503 {
        assert!(began.elapsed() < SOON, "{}", daemon.errors());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // From then on it starts nothing and answers no offer: not for a
    // session it would have to make, and not for one that is there.
    // The client takes that for a server to try again, which it does
    // with the daemon that is started next.
    let offer = offer_of(&an_offer().await, None, false);
    let start = json!({"transport": "webrtc"});
    for (held, path, body) in [
        (None, "/start?agent=desk", &start),
        (None, "/api/offer?agent=desk", &offer),
        (Some(token.as_str()), "/start", &start),
        (Some(token.as_str()), "/api/offer", &offer),
    ] {
        let refused = asks(&daemon, held, "POST", path, body).await;
        assert_eq!(
            (refused.status, refused.code().as_str()),
            (503, "busy"),
            "{path}"
        );
        let detail = refused.json()["detail"].as_str().unwrap().to_owned();
        assert!(
            detail.contains("the daemon is stopping"),
            "{path}: {detail}"
        );
        assert_eq!(refused.header("retry-after"), Some("1"), "{path}");
    }
    // The client it has is told that its peer left, and calls no more.
    client.listens(|_| false).await;
    assert!(client.left);
    client.is_over().await;
    let status = daemon.ended().await;
    assert!(status.success(), "{}", daemon.errors());
}

#[tokio::test]
async fn a_daemon_that_was_told_of_no_calls_answers_no_client_of_pipecat_s() {
    let providers = written_to(0, 1).await;
    let silent = Served::start(&crate::desk(), &providers.config(""), &[]).await;
    let start = json!({"transport": "webrtc"});
    let offered = offer_of("v=0\r\n", None, false);
    for (method, path, body) in [
        ("POST", "/start?agent=desk", &start),
        ("POST", "/api/offer?agent=desk", &offered),
        ("PATCH", "/api/offer", &json!({"pc_id": "x"})),
    ] {
        let refused = asks(&silent, None, method, path, body).await;
        assert_eq!(refused.status, 404, "{method} {path}: {refused:?}");
        assert_eq!(refused.code(), "unsupported");
    }
    assert_eq!(sessions(&silent).await, Vec::<String>::new());
    went_well(&silent);
}

#[tokio::test]
async fn a_page_from_elsewhere_cannot_ask_as_a_client_of_pipecat_s() {
    let providers = written_to(0, 1).await;
    let daemon = daemon(&providers, CALLED).await;
    let body = offer_of("v=0\r\n", None, false).to_string();
    for (method, path) in [
        ("POST", "/start?agent=desk"),
        ("POST", "/api/offer?agent=desk"),
        ("PATCH", "/api/offer"),
        (
            "POST",
            "/sessions/0123456789abcdef0123456789abcdef/api/offer",
        ),
        (
            "PATCH",
            "/sessions/0123456789abcdef0123456789abcdef/api/offer",
        ),
    ] {
        // A body says that it is JSON: a page from elsewhere cannot say
        // so without asking first.
        let headers = [("Content-Type", "text/plain")];
        let refused = ask(&daemon.address, method, path, &headers, Some(&body)).await;
        assert_eq!(refused.status, 415, "{method} {path}: {refused:?}");
        // And a page the daemon was not told of may not ask at all.
        let headers = [("Origin", "https://elsewhere.example")];
        let refused = ask(&daemon.address, method, path, &headers, Some(&body)).await;
        assert_eq!(refused.status, 403, "{method} {path}: {refused:?}");
    }
    assert_eq!(sessions(&daemon).await, Vec::<String>::new());
    went_well(&daemon);
}
