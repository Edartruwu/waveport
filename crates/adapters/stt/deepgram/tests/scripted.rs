//! The adapter against a server on this machine that answers in the
//! shapes the provider's documentation gives, and, when asked, against
//! the provider.
//!
//! Nothing here was recorded: there has been no key to record with. The
//! messages are made up, as the documentation of 15 November 2026 has
//! them. The checks of `waveport_testkit::conformance` are the same ones
//! that the fakes pass.
//!
//! The tests named `live_*` talk to the provider. They are ignored
//! unless asked for, and need `DEEPGRAM_API_KEY`:
//!
//! ```sh
//! cargo test -p waveport-stt-deepgram -- --ignored live_
//! ```

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;

use waveport_adapter_kit::secret::Secret;
use waveport_domain::event::RetryClass;
use waveport_domain::time::{MediaSpan, MediaTime};
use waveport_ports::audio::{AudioRef, Rate};
use waveport_ports::stt::{Stt, SttConfig, SttEvent, SttStream, Transcript};
use waveport_stt_deepgram::{Config, DeepgramStt};
use waveport_testkit::conformance::{Talk, stt_utterance};
use waveport_testkit::drive::next_stt;
use waveport_testkit::servers::{WsHeard, WsSeen, WsServer, WsStep};
use waveport_testkit::speech::{WHAT_TIME_IS_IT, framed, what_time_is_it};

const RATE: Rate = Rate::Hz16000;
const MODEL: &str = "nova-3";

const fn ms(millis: u64) -> MediaTime {
    MediaTime::from_millis(millis)
}

/// The question, with the quiet before and after it that a caller
/// leaves: 2.735 s in all.
fn question() -> Vec<i16> {
    framed(RATE, 300, &what_time_is_it(RATE), 700)
}

/// Where audio that began at `at` ends.
fn end_of(at: MediaTime, samples: &[i16]) -> MediaTime {
    at.after(RATE.length_of(samples.len()))
}

/// Waits for a second of audio, as the checks send it: in pieces of
/// 20 ms. The provider cannot write down what it has not been sent.
fn a_second_of_audio() -> String {
    [">b"; 50].join("\n")
}

/// A transcript as the provider sends one.
fn results(
    start: f64,
    duration: f64,
    flags: &str,
    transcript: &str,
    words: &[(f64, f64)],
) -> String {
    let words: Vec<String> = words
        .iter()
        .map(|(start, end)| {
            format!("{{\"word\":\"w\",\"start\":{start},\"end\":{end},\"confidence\":0.99}}")
        })
        .collect();
    format!(
        "< {{\"type\":\"Results\",\"channel_index\":[0,1],\"duration\":{duration},\
         \"start\":{start},{flags}\"channel\":{{\"alternatives\":[{{\"transcript\":\
         \"{transcript}\",\"confidence\":0.99,\"words\":[{}]}}]}},\"metadata\":\
         {{\"request_id\":\"made-up\",\"model_info\":{{\"name\":\"general-nova-3\",\
         \"version\":\"made-up\",\"arch\":\"nova-3\"}},\"model_uuid\":\"made-up\"}}}}",
        words.join(",")
    )
}

/// What the provider says at the very end, and how it closes.
const SUMMARY: &str = "< {\"type\":\"Metadata\",\"transaction_key\":\"deprecated\",\
    \"request_id\":\"made-up\",\"sha256\":\"made-up\",\"created\":\"2026-11-15T12:00:00.000Z\",\
    \"duration\":2.735,\"channels\":1}\n! 1000 ";

/// The script of a session in which the question is asked once: two
/// transcripts that are not final as the audio arrives, and the final
/// one when it is asked for.
fn question_script() -> String {
    [
        a_second_of_audio(),
        results(
            0.0,
            1.0,
            "\"is_final\":false,\"speech_final\":false,",
            "what time",
            &[(0.35, 0.55), (0.6, 0.85)],
        ),
        a_second_of_audio(),
        results(
            0.0,
            2.0,
            "\"is_final\":false,\"speech_final\":false,",
            "what time is it right now",
            &[(0.35, 0.55), (1.65, 1.95)],
        ),
        "> \"type\":\"Finalize\"".to_owned(),
        results(
            0.0,
            2.735,
            "\"is_final\":true,\"speech_final\":true,\"from_finalize\":true,",
            "What time is it right now?",
            &[(0.35, 0.55), (1.65, 2.0)],
        ),
        "> \"type\":\"CloseStream\"".to_owned(),
        SUMMARY.to_owned(),
    ]
    .join("\n")
}

async fn server(script: &str) -> WsServer {
    WsServer::start(vec![WsStep::script(script)]).await
}

/// An adapter that talks to `server`, and that does not wait long.
fn adapter(server: &WsServer) -> DeepgramStt {
    DeepgramStt::new(Config {
        url: server.url("/v1/listen"),
        timeout: Duration::from_secs(2),
        answer_timeout: Duration::from_millis(300),
        ..Config::new(MODEL, Some(Secret::new("dg-not-a-key")))
    })
}

async fn open(stt: &DeepgramStt) -> SttStream {
    let config = SttConfig {
        language: Some("en".to_owned()),
        ..SttConfig::new(RATE)
    };
    stt.open(config).await.unwrap()
}

/// Sends `samples` from `at` on in pieces of a tenth of a second, and
/// returns where they end.
fn send(stream: &mut SttStream, at: MediaTime, samples: &[i16]) -> MediaTime {
    let mut at = at;
    for piece in samples.chunks(RATE.samples_in_millis(100)) {
        let audio = AudioRef {
            at,
            rate: RATE,
            samples: piece,
        };
        stream.tx.try_send(audio).unwrap();
        at = audio.span().end;
    }
    at
}

/// Reads the stream up to its next final transcript.
async fn next_final(stream: &mut SttStream) -> Transcript {
    loop {
        let next = tokio::time::timeout(Duration::from_secs(10), next_stt(&mut stream.rx));
        match next.await.unwrap() {
            Some(Ok(SttEvent::Transcript(transcript))) if transcript.is_final => return transcript,
            Some(Ok(_)) => {}
            other => panic!("no final transcript: {other:?}"),
        }
    }
}

/// The audio a server was sent on a connection, put together.
fn audio_sent(seen: &WsSeen) -> Vec<i16> {
    seen.messages
        .iter()
        .filter_map(|message| match message {
            WsHeard::Bytes(bytes) => Some(bytes),
            WsHeard::Text(_) => None,
        })
        .flat_map(|bytes| bytes.as_chunks::<2>().0.iter().copied())
        .map(i16::from_le_bytes)
        .collect()
}

#[tokio::test]
async fn a_question_is_heard_as_the_port_says() {
    let server = server(&question_script()).await;
    let stt = adapter(&server);
    let audio = question();
    let talk = Talk {
        at: ms(1_000),
        rate: RATE,
        samples: &audio,
        paced: false,
    };
    let seen = stt_utterance(&stt, talk).await.unwrap();
    assert_eq!(seen.text(), WHAT_TIME_IS_IT);
    let end = end_of(ms(1_000), &audio);
    assert_eq!(end, ms(3_735));

    // The words are placed where the provider says they were spoken, on
    // the clock of the audio that was sent, which began at one second.
    let grew: Vec<(&str, MediaSpan)> = seen
        .interim
        .iter()
        .map(|t| (t.text.as_str(), t.span))
        .collect();
    assert_eq!(
        grew,
        [
            ("what time", MediaSpan::new(ms(1_350), ms(1_850))),
            (
                "what time is it right now",
                MediaSpan::new(ms(1_350), ms(2_950))
            ),
        ]
    );
    let finals: Vec<(&str, MediaSpan)> = seen
        .finals
        .iter()
        .map(|t| (t.text.as_str(), t.span))
        .collect();
    assert_eq!(
        finals,
        [
            (WHAT_TIME_IS_IT, MediaSpan::new(ms(1_350), ms(3_000))),
            // The words end before the audio does. That all of it was
            // heard is said apart.
            ("", MediaSpan::new(end, end)),
        ]
    );

    // What the provider was sent: what the session asked for in the
    // address, the key in a header, all the audio as it was, one request
    // for a final transcript, and then that no more audio follows.
    let connections = server.over(1).await;
    assert_eq!(connections.len(), 1);
    let sent = &connections[0];
    assert_eq!(
        sent.path,
        "/v1/listen?model=nova-3&language=multi&encoding=linear16&sample_rate=16000&channels=1\
         &interim_results=true&smart_format=true"
    );
    assert_eq!(sent.header("authorization"), Some("Token dg-not-a-key"));
    assert_eq!(audio_sent(sent), audio);
    let texts: Vec<&str> = sent.texts().collect();
    assert_eq!(
        texts,
        ["{\"type\":\"Finalize\"}", "{\"type\":\"CloseStream\"}"]
    );
}

#[tokio::test]
async fn two_turns_on_one_stream_are_each_placed_in_their_own_audio() {
    let script = [
        "> \"type\":\"Finalize\"".to_owned(),
        results(
            0.0,
            2.735,
            "\"is_final\":true,\"from_finalize\":true,",
            "What time is it right now?",
            &[(0.35, 2.0)],
        ),
        "> \"type\":\"Finalize\"".to_owned(),
        // The provider counts on through the audio it was sent, and not
        // through the minute in which it was sent nothing.
        results(
            2.735,
            2.735,
            "\"is_final\":true,\"from_finalize\":true,",
            "And the date?",
            &[(3.085, 4.185)],
        ),
    ]
    .join("\n");
    let server = server(&script).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let audio = question();

    let end = send(&mut stream, ms(1_000), &audio);
    assert_eq!(end, end_of(ms(1_000), &audio));
    stream.tx.try_finalize().unwrap();
    let first = next_final(&mut stream).await;
    assert_eq!(first.text, WHAT_TIME_IS_IT);
    assert_eq!(first.span, MediaSpan::new(ms(1_350), ms(3_000)));
    assert_eq!(next_final(&mut stream).await.span, MediaSpan::new(end, end));

    // A minute later, on the session's clock.
    send(&mut stream, ms(61_000), &audio);
    stream.tx.try_finalize().unwrap();
    let second = next_final(&mut stream).await;
    assert_eq!(second.text, "And the date?");
    assert_eq!(second.span, MediaSpan::new(ms(61_350), ms(62_450)));
}

#[tokio::test]
async fn a_request_the_provider_does_not_answer_is_answered_with_nothing() {
    // The provider takes the audio and the request, and says nothing.
    let server = server("> \"type\":\"Finalize\"").await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let end = send(&mut stream, ms(0), &question());
    stream.tx.try_finalize().unwrap();
    let began = std::time::Instant::now();
    let answer = next_final(&mut stream).await;
    assert_eq!(answer.text, "");
    assert_eq!(answer.span, MediaSpan::new(end, end));
    // Not at once, and not long after: the provider had its time.
    let took = began.elapsed();
    assert!(
        (Duration::from_millis(250)..Duration::from_millis(2_000)).contains(&took),
        "{took:?}"
    );
}

#[tokio::test]
async fn a_connection_nothing_is_sent_on_is_kept() {
    let server = server("> \"type\":\"CloseStream\"\n! 1000 ").await;
    let stt = DeepgramStt::new(Config {
        keep_alive: Duration::from_millis(60),
        ..adapter(&server).config().clone()
    });
    let mut stream = open(&stt).await;
    // A quarter of a second in which the session sends nothing.
    let quiet = tokio::time::timeout(Duration::from_millis(250), next_stt(&mut stream.rx));
    assert!(quiet.await.is_err(), "nothing was to be heard");
    drop(stream.tx);
    // The provider is told that this is all, and the stream ends when it
    // has closed.
    let last = tokio::time::timeout(Duration::from_secs(5), next_stt(&mut stream.rx));
    assert!(last.await.unwrap().is_none());
    let connections = server.over(1).await;
    let texts: Vec<&str> = connections[0].texts().collect();
    let kept = texts
        .iter()
        .filter(|text| **text == "{\"type\":\"KeepAlive\"}")
        .count();
    assert!((2..=5).contains(&kept), "{texts:?}");
    assert_eq!(texts.last(), Some(&"{\"type\":\"CloseStream\"}"));
}

#[tokio::test]
async fn a_provider_that_does_not_close_is_not_waited_for_long() {
    // It takes everything, and neither answers nor closes.
    let server = server("").await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let end = send(&mut stream, ms(0), &question());
    stream.tx.try_finalize().unwrap();
    let SttStream { tx, mut rx } = stream;
    drop(tx);
    // Whoever asked is still answered, and then the stream ends.
    let all = async {
        let mut all = Vec::new();
        while let Some(next) = next_stt(&mut rx).await {
            all.push(next.unwrap());
        }
        all
    };
    let all = tokio::time::timeout(Duration::from_secs(8), all)
        .await
        .unwrap();
    let answer = SttEvent::Transcript(Transcript {
        span: MediaSpan::new(end, end),
        text: String::new(),
        is_final: true,
    });
    assert_eq!(all, [answer]);
}

#[tokio::test]
async fn a_provider_that_refuses_the_connection_answers_by_its_status() {
    for (status, class) in [
        (401, RetryClass::Auth),
        (400, RetryClass::Permanent),
        (429, RetryClass::RateLimited),
        (503, RetryClass::Transient),
    ] {
        // The provider says why in the body, and may repeat what it was
        // sent.
        let said = "{\"err_code\":\"INVALID_AUTH\",\"err_msg\":\"Invalid credentials: \
                    dg-not-a-key\"}";
        let server = server(&format!("? {status} {said}")).await;
        let error = adapter(&server)
            .open(SttConfig::new(RATE))
            .await
            .unwrap_err();
        assert_eq!(error.class, class, "{status}: {error}");
        assert!(!error.message.contains("dg-not-a-key"), "{error}");
    }
}

#[tokio::test]
async fn a_provider_that_closes_with_a_complaint_fails_the_stream() {
    for (close, class) in [
        // It heard nothing for too long, or failed inside.
        ("! 1011 NET-0001", RetryClass::Transient),
        // What it was sent was not audio it can read.
        ("! 1008 DATA-0000", RetryClass::Permanent),
    ] {
        let server = server(&format!(">b\n{close}")).await;
        let stt = adapter(&server);
        let mut stream = open(&stt).await;
        send(&mut stream, ms(0), &[0; 1_600]);
        let error = next_stt(&mut stream.rx).await.unwrap().unwrap_err();
        assert_eq!(error.class, class, "{close}: {error}");
        assert!(error.message.contains(&close[7..]), "{error}");
        // That was the last of the stream.
        assert!(next_stt(&mut stream.rx).await.is_none());
    }
}

#[tokio::test]
async fn a_provider_that_did_not_understand_fails_the_stream() {
    let said = "< {\"type\":\"Error\",\"variant\":\"SchemaError\",\"description\":\
                \"unknown variant\",\"message\":\"{}\"}";
    let server = server(&format!(">b\n{said}")).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    send(&mut stream, ms(0), &[0; 1_600]);
    let error = next_stt(&mut stream.rx).await.unwrap().unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent, "{error}");
    assert!(error.message.contains("SchemaError"), "{error}");
}

#[tokio::test]
async fn a_stream_that_is_behind_refuses_audio_and_takes_it_again_later() {
    let server = server("").await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    // Nobody runs the stream: what is sent waits, and there is room for
    // only so much.
    let piece = [0_i16; 320];
    let mut at = ms(0);
    let mut taken = 0;
    loop {
        let audio = AudioRef {
            at,
            rate: RATE,
            samples: &piece,
        };
        if stream.tx.try_send(audio).is_err() {
            break;
        }
        at = audio.span().end;
        taken += 1;
        assert!(
            taken < 1_000,
            "a stream that is not run takes audio for ever"
        );
    }
    assert!(taken >= 8, "{taken}");
    // Run, it takes more.
    let run = tokio::time::timeout(Duration::from_millis(100), next_stt(&mut stream.rx));
    assert!(run.await.is_err());
    let audio = AudioRef {
        at,
        rate: RATE,
        samples: &piece,
    };
    stream.tx.try_send(audio).unwrap();
}

/// An adapter that talks to the provider.
fn live() -> DeepgramStt {
    let key = Secret::from_env("DEEPGRAM_API_KEY").unwrap();
    DeepgramStt::new(Config::new(MODEL, Some(key)))
}

/// What was said, without the things two recognisers may differ in:
/// capitals and punctuation.
fn plain(text: &str) -> String {
    let words: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    words
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[tokio::test]
#[ignore = "talks to the provider; needs DEEPGRAM_API_KEY"]
async fn live_a_question_is_heard_as_the_port_says() {
    let stt = live();
    let audio = question();
    let talk = Talk {
        at: ms(1_000),
        rate: RATE,
        samples: &audio,
        paced: true,
    };
    let seen = stt_utterance(&stt, talk).await.unwrap();
    assert_eq!(plain(&seen.text()), plain(WHAT_TIME_IS_IT), "{seen:?}");
    // The words are placed in the speech, which begins 0.3 s into the
    // audio and lasts a second and three quarters.
    let spoken = seen.finals.iter().find(|t| !t.text.is_empty()).unwrap();
    assert!(
        spoken.span.start >= ms(1_100) && spoken.span.start <= ms(1_700),
        "{:?}",
        spoken.span
    );
}

#[tokio::test]
#[ignore = "talks to the provider; needs DEEPGRAM_API_KEY"]
async fn live_two_turns_on_one_stream_are_each_heard() {
    let stt = live();
    let mut stream = open(&stt).await;
    let audio = question();
    let mut at = ms(0);
    for turn in 0..2 {
        let began = at;
        for piece in audio.chunks(RATE.samples_in_millis(100)) {
            let audio = AudioRef {
                at,
                rate: RATE,
                samples: piece,
            };
            stream.tx.try_send(audio).unwrap();
            at = audio.span().end;
            // At the pace it is spoken, with the loop run meanwhile.
            let _ = tokio::time::timeout(Duration::from_millis(100), async {
                loop {
                    match next_stt(&mut stream.rx).await {
                        Some(Ok(_)) => {}
                        other => panic!("the stream ended: {other:?}"),
                    }
                }
            })
            .await;
        }
        stream.tx.try_finalize().unwrap();
        // The words, and then that all of the audio was heard.
        let mut heard = String::new();
        loop {
            let next = next_final(&mut stream).await;
            assert!(
                next.span.start >= began && next.span.end <= at,
                "turn {turn}"
            );
            if next.text.is_empty() {
                break;
            }
            heard.push_str(&next.text);
            heard.push(' ');
        }
        assert_eq!(plain(&heard), plain(WHAT_TIME_IS_IT), "turn {turn}");
    }
}

#[tokio::test]
#[ignore = "talks to the provider; needs no key"]
async fn live_a_wrong_key_is_refused() {
    let key = Secret::new("dg-not-a-key");
    let stt = DeepgramStt::new(Config::new(MODEL, Some(key)));
    let error = stt.open(SttConfig::new(RATE)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Auth, "{error}");
    assert!(!error.message.contains("dg-not-a-key"), "{error}");
}
