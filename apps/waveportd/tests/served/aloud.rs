//! A session that hears and speaks, with its far end on a WebSocket
//! that carries Opus.
//!
//! The far end is the test: it codes what it says with the Opus the
//! runtime uses, a packet every 20 ms as a browser sends them, and
//! decodes what it is sent. The recogniser is a server on this machine
//! that replays what OpenAI once said of this question, and the
//! synthesiser one that answers in the shapes Cartesia's documentation
//! gives, with a tone for a voice.

use core::time::Duration;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use waveport_media::codec::opus::{MAX_PACKET, OpusDecoder, OpusEncoder, OpusSettings};
use waveport_ports::audio::Rate;
use waveport_protocol::client::v1::{AudioFormat, ByeReason, Codec, Fact};
use waveport_testkit::servers::{HttpServer, WsServer, WsStep};
use waveport_testkit::speech;
use waveport_testkit::stand_in::{HEARD, SAID, spoken};
use waveport_transport_websocket::{Client, Heard, Hello};

use crate::{Served, answer, desk};

/// How many samples a packet holds: 20 ms at 48 kHz.
const PACKET: usize = 960;

/// Where the models that listen are.
fn assets() -> PathBuf {
    std::env::var_os("WAVEPORT_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/assets"),
        PathBuf::from,
    )
}

/// The configuration of a daemon whose agent hears and speaks, with
/// these three for its providers.
fn config(stt: &WsServer, llm: &HttpServer, tts: &WsServer) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n\
         assets = '{}'\n\
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
        llm.url("/v1"),
        stt.url("/v1/realtime"),
        tts.url("/tts/websocket"),
    )
}

/// What a client heard of a call.
#[derive(Debug, Default)]
struct Call {
    facts: Vec<Fact>,
    /// How many of the samples it decoded were of a voice.
    loud: usize,
    /// How many runs of audio began, and how many ended.
    runs: (usize, usize),
    reason: Option<ByeReason>,
}

/// Says `speech` to the agent, a packet every 20 ms, and then nothing,
/// for as long as the call lasts. What the agent says is decoded, and
/// said to have been played as it arrives.
async fn talks(client: &mut Client, speech: &[i16]) -> Call {
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
                // A line that is over takes nothing, and says so below.
                let _ = client.audio(Bytes::copy_from_slice(&packet[..length])).await;
            }
            heard = client.next() => match heard.unwrap() {
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
                    client.played(*run, *played).await.unwrap();
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

#[tokio::test]
async fn a_client_that_speaks_opus_asks_aloud_and_hears_the_answer() {
    let stt = WsServer::start(vec![WsStep::script(HEARD)]).await;
    let llm = HttpServer::start(vec![
        answer(&[], Some("get_time")),
        answer(&SAID, Some("end_call")),
        answer(&[], None),
    ])
    .await;
    let tts = WsServer::start(vec![spoken()]).await;
    let daemon = Served::start(&desk(), &config(&stt, &llm, &tts), &[]).await;
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
    // The session hears and speaks, because its far end does.
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{name}"), None)
        .await
        .json();
    assert_eq!(info["mode"], "voice", "{info}");

    // A moment of nothing, and the question.
    let mut speech = vec![0_i16; 48 * 400];
    speech.extend(speech::what_time_is_it(Rate::Hz48000));
    let call = tokio::time::timeout(Duration::from_secs(60), talks(&mut client, &speech)).await;
    let errors = daemon.errors();
    let call = call.unwrap_or_else(|_| panic!("the call did not end: {errors}"));
    let all = format!("{call:#?}\n{errors}");

    // The agent heard the question, answered it aloud, and ended the
    // call as it was asked to.
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
    // What it said arrived as Opus that decodes to its voice: 1.2 s of
    // it, and most of that above a whisper.
    assert_eq!(call.runs, (1, 1), "{all}");
    assert!(call.loud > 40_000, "{all}");
    // The recogniser was sent what the far end said, all the while.
    let heard = stt.connections();
    assert_eq!(heard.len(), 1, "{all}");
    let sent = heard[0]
        .texts()
        .filter(|text| text.contains("input_audio_buffer.append"))
        .count();
    assert!(sent > 20, "{sent}\n{all}");
}
