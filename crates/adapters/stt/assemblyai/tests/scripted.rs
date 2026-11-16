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
//! unless asked for, and need `ASSEMBLYAI_API_KEY`:
//!
//! ```sh
//! cargo test -p waveport-stt-assemblyai -- --ignored live_
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
use waveport_stt_assemblyai::{AssemblyAiStt, Config};
use waveport_testkit::conformance::{Talk, stt_utterance};
use waveport_testkit::drive::next_stt;
use waveport_testkit::servers::{WsHeard, WsSeen, WsServer, WsStep};
use waveport_testkit::speech::{WHAT_TIME_IS_IT, framed, what_time_is_it};

const RATE: Rate = Rate::Hz16000;
const MODEL: &str = "universal-3-6-pro";

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

/// The provider's first message: a session has begun, with `model`.
fn begin(model: &str) -> String {
    format!(
        "< {{\"type\":\"Begin\",\"id\":\"made-up\",\"expires_at\":1772570132,\
         \"configuration\":{{\"model\":\"{model}\",\"mode\":\"balanced\",\
         \"api_version\":\"2025-05-12\"}}}}"
    )
}

/// The provider says that something is wrong, and closes with the same
/// code.
fn refusal(code: u16, said: &str) -> String {
    format!("< {{\"type\":\"Error\",\"error_code\":{code},\"error\":\"{said}\"}}\n! {code} {said}")
}

/// Waits for `count` messages of audio. The adapter puts the pieces of
/// 20 ms that the checks send together by threes, so that is 60 ms each.
/// The provider cannot write down what it has not been sent.
fn audio(count: usize) -> String {
    vec![">b"; count].join("\n")
}

const PARTIAL: &str = "\"turn_is_formatted\":false,\"end_of_turn\":false,";
const OVER: &str = "\"turn_is_formatted\":true,\"end_of_turn\":true,";

/// What the provider says about a turn. `words` are where each begins
/// and ends, in milliseconds of the audio.
fn turn(order: u64, flags: &str, transcript: &str, words: &[(u32, u32)]) -> String {
    let words: Vec<String> = words
        .iter()
        .map(|(start, end)| {
            format!(
                "{{\"start\":{start},\"end\":{end},\"text\":\"w\",\"confidence\":0.99,\
                 \"word_is_final\":true}}"
            )
        })
        .collect();
    format!(
        "< {{\"turn_order\":{order},{flags}\"transcript\":\"{transcript}\",\
         \"end_of_turn_confidence\":0.5,\"words\":[{}],\"utterance\":\"\",\"type\":\"Turn\"}}",
        words.join(",")
    )
}

/// What the provider says at the very end, and how it closes.
const SUMMARY: &str = "< {\"type\":\"Termination\",\"audio_duration_seconds\":3,\
    \"session_duration_seconds\":3}\n! 1000 ";

/// The script of a session in which the question is asked once: the
/// turn as it grows while the audio arrives, and the turn ended when
/// that is asked for.
fn question_script() -> String {
    [
        begin(MODEL),
        // Nine tenths of a second.
        audio(15),
        "< {\"type\":\"SpeechStarted\",\"timestamp\":350,\"confidence\":0.98}".to_owned(),
        turn(0, PARTIAL, "what time", &[(350, 550), (600, 850)]),
        // Two seconds, nearly.
        audio(18),
        turn(
            0,
            PARTIAL,
            "what time is it right now",
            &[(350, 550), (1_650, 1_950)],
        ),
        "> \"type\":\"ForceEndpoint\"".to_owned(),
        turn(
            0,
            OVER,
            "What time is it right now?",
            &[(350, 550), (1_650, 2_000)],
        ),
        "> \"type\":\"Terminate\"".to_owned(),
        SUMMARY.to_owned(),
    ]
    .join("\n")
}

async fn server(script: &str) -> WsServer {
    WsServer::start(vec![WsStep::script(script)]).await
}

/// An adapter that talks to `server`, and that does not wait long.
fn adapter(server: &WsServer) -> AssemblyAiStt {
    AssemblyAiStt::new(Config {
        url: server.url("/v3/ws"),
        timeout: Duration::from_secs(2),
        answer_timeout: Duration::from_millis(300),
        ..Config::new(MODEL, Some(Secret::new("aai-not-a-key")))
    })
}

async fn open(stt: &AssemblyAiStt) -> SttStream {
    let config = SttConfig {
        language: Some("en-US".to_owned()),
        ..SttConfig::new(RATE)
    };
    stt.open(config).await.unwrap()
}

/// Sends `samples` from `at` on in pieces of `piece_ms`, and returns
/// where they end.
fn send(stream: &mut SttStream, at: MediaTime, samples: &[i16], piece_ms: u32) -> MediaTime {
    let mut at = at;
    for piece in samples.chunks(RATE.samples_in_millis(piece_ms)) {
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

/// How many bytes each message of audio held that a server was sent.
fn sizes(seen: &WsSeen) -> Vec<usize> {
    seen.messages
        .iter()
        .filter_map(|message| match message {
            WsHeard::Bytes(bytes) => Some(bytes.len()),
            WsHeard::Text(_) => None,
        })
        .collect()
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

/// At 16 kHz, what the provider takes in a message: 20 ms to a second.
const TAKEN: core::ops::RangeInclusive<usize> = 640..=32_000;

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
    // address, the key in a header, all the audio as it was and in
    // messages of a size it takes, one request to end the turn, and then
    // that the session is over.
    let connections = server.over(1).await;
    assert_eq!(connections.len(), 1);
    let sent = &connections[0];
    assert_eq!(
        sent.path,
        "/v3/ws?sample_rate=16000&encoding=pcm_s16le&speech_model=universal-3-6-pro"
    );
    assert_eq!(sent.header("authorization"), Some("aai-not-a-key"));
    assert_eq!(audio_sent(sent), audio);
    let sizes = sizes(sent);
    assert!(sizes.iter().all(|size| TAKEN.contains(size)), "{sizes:?}");
    // By threes, and what was left when the turn was to end.
    assert_eq!(sizes.len(), 46, "{sizes:?}");
    let texts: Vec<&str> = sent.texts().collect();
    assert_eq!(
        texts,
        ["{\"type\":\"ForceEndpoint\"}", "{\"type\":\"Terminate\"}"]
    );
}

#[tokio::test]
async fn two_turns_on_one_stream_are_each_placed_in_their_own_audio() {
    let script = [
        begin(MODEL),
        "> \"type\":\"ForceEndpoint\"".to_owned(),
        turn(0, OVER, "What time is it right now?", &[(350, 2_000)]),
        "> \"type\":\"ForceEndpoint\"".to_owned(),
        // The provider counts on through the audio it was sent, and not
        // through the minute in which it was sent nothing.
        turn(1, OVER, "And the date?", &[(3_085, 4_185)]),
    ]
    .join("\n");
    let server = server(&script).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let audio = question();

    let end = send(&mut stream, ms(1_000), &audio, 100);
    assert_eq!(end, end_of(ms(1_000), &audio));
    stream.tx.try_finalize().unwrap();
    let first = next_final(&mut stream).await;
    assert_eq!(first.text, WHAT_TIME_IS_IT);
    assert_eq!(first.span, MediaSpan::new(ms(1_350), ms(3_000)));
    assert_eq!(next_final(&mut stream).await.span, MediaSpan::new(end, end));

    // A minute later, on the session's clock.
    send(&mut stream, ms(61_000), &audio, 100);
    stream.tx.try_finalize().unwrap();
    let second = next_final(&mut stream).await;
    assert_eq!(second.text, "And the date?");
    assert_eq!(second.span, MediaSpan::new(ms(61_350), ms(62_450)));

    // The language the session named went in the address, as the
    // provider knows it.
    drop(stream);
    let connections = server.over(1).await;
    assert_eq!(
        connections[0].path,
        "/v3/ws?sample_rate=16000&encoding=pcm_s16le&speech_model=universal-3-6-pro\
         &language_codes=%5B%22en%22%5D"
    );
}

#[tokio::test]
async fn audio_in_small_pieces_goes_in_messages_the_provider_takes() {
    let script = [begin(MODEL), "> \"type\":\"ForceEndpoint\"".to_owned()].join("\n");
    let server = server(&script).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    // Pieces of 5 ms, each less than the provider takes, and a last one
    // that leaves less than it takes over.
    let audio = &question()[..8_000 + 100];
    let mut at = ms(0);
    for pieces in audio.chunks(800) {
        at = send(&mut stream, at, pieces, 5);
        // The loop is run, so that there is room for more.
        let run = tokio::time::timeout(Duration::from_millis(20), next_stt(&mut stream.rx));
        assert!(run.await.is_err());
    }
    stream.tx.try_finalize().unwrap();
    // Nothing ends a turn here, so the request is answered with nothing,
    // at the end of everything the session sent.
    let answer = next_final(&mut stream).await;
    assert_eq!(answer.span, MediaSpan::new(at, at));
    drop(stream);

    let connections = server.over(1).await;
    let sizes = sizes(&connections[0]);
    assert!(sizes.iter().all(|size| TAKEN.contains(size)), "{sizes:?}");
    // All of it but the last 100 samples, which are 6 ms.
    let sent = audio_sent(&connections[0]);
    assert_eq!(sent, audio[..8_000]);
}

#[tokio::test]
async fn a_request_the_provider_does_not_answer_is_answered_with_nothing() {
    // The provider takes the audio and the request, and says nothing.
    let script = [begin(MODEL), "> \"type\":\"ForceEndpoint\"".to_owned()].join("\n");
    let server = server(&script).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let end = send(&mut stream, ms(0), &question(), 100);
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
async fn a_provider_that_does_not_say_it_is_over_is_not_waited_for_long() {
    // It takes everything, and neither answers nor closes.
    let server = server(&begin(MODEL)).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let end = send(&mut stream, ms(0), &question(), 100);
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
    // The provider was told that the session is over all the same: it
    // charges for as long as one is open.
    let connections = server.over(1).await;
    let texts: Vec<&str> = connections[0].texts().collect();
    assert_eq!(texts.last(), Some(&"{\"type\":\"Terminate\"}"));
}

#[tokio::test]
async fn what_the_provider_writes_down_after_the_end_is_still_passed_on() {
    let script = [
        begin(MODEL),
        "> \"type\":\"Terminate\"".to_owned(),
        // The turn that was going on when the session ended.
        turn(0, OVER, "Never mind.", &[(400, 1_100)]),
        SUMMARY.to_owned(),
    ]
    .join("\n");
    let server = server(&script).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    send(&mut stream, ms(0), &question(), 100);
    let SttStream { tx, mut rx } = stream;
    drop(tx);
    let mut all = Vec::new();
    while let Some(next) = next_stt(&mut rx).await {
        all.push(next.unwrap());
    }
    let last = SttEvent::Transcript(Transcript {
        span: MediaSpan::new(ms(400), ms(1_100)),
        text: "Never mind.".to_owned(),
        is_final: true,
    });
    assert_eq!(all, [last]);
}

#[tokio::test]
async fn a_provider_that_refuses_the_session_fails_the_opening() {
    for (script, class) in [
        // A key it does not take. The provider may repeat what it was
        // sent.
        (
            refusal(
                1008,
                "Unauthorized Connection: Invalid API key aai-not-a-key",
            ),
            RetryClass::Auth,
        ),
        (
            refusal(1008, "Unauthorized Connection: Insufficient funds"),
            RetryClass::Auth,
        ),
        // More sessions begun in a minute than the account may.
        (
            refusal(
                1008,
                "Unauthorized Connection: Too many concurrent sessions",
            ),
            RetryClass::RateLimited,
        ),
        (
            refusal(1011, "Error authenticating connection"),
            RetryClass::Transient,
        ),
        // A model's name it did not know, for which it took its own.
        (begin("universal-streaming-english"), RetryClass::Permanent),
        // The address of an interface it has given up.
        ("? 410 gone".to_owned(), RetryClass::Permanent),
        ("? 503 busy".to_owned(), RetryClass::Transient),
        // It closes without a word.
        ("! 1000 ".to_owned(), RetryClass::Transient),
    ] {
        let server = server(&script).await;
        let error = adapter(&server)
            .open(SttConfig::new(RATE))
            .await
            .unwrap_err();
        assert_eq!(error.class, class, "{script}: {error}");
        assert!(!error.message.contains("aai-not-a-key"), "{error}");
    }
}

#[tokio::test]
async fn a_provider_that_does_not_begin_a_session_is_not_waited_for() {
    // The connection is made, and nothing is said on it.
    let server = server("~ 5000").await;
    let stt = AssemblyAiStt::new(Config {
        timeout: Duration::from_millis(200),
        ..adapter(&server).config().clone()
    });
    let began = std::time::Instant::now();
    let error = stt.open(SttConfig::new(RATE)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Timeout, "{error}");
    assert!(began.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_provider_that_ends_the_session_with_a_complaint_fails_the_stream() {
    for (code, said, class) in [
        // It failed inside.
        (
            3005,
            "Session Cancelled: An error occurred",
            RetryClass::Transient,
        ),
        // The session has lasted the three hours one may.
        (
            3008,
            "Session Expired: Maximum session duration exceeded",
            RetryClass::Transient,
        ),
        // What it was sent was wrong.
        (
            3007,
            "Input duration violation: 10 ms. Expected between 20 and 1000 ms",
            RetryClass::Permanent,
        ),
    ] {
        let script = [begin(MODEL), ">b".to_owned(), refusal(code, said)].join("\n");
        let server = server(&script).await;
        let stt = adapter(&server);
        let mut stream = open(&stt).await;
        send(&mut stream, ms(0), &[0; 1_600], 100);
        let error = next_stt(&mut stream.rx).await.unwrap().unwrap_err();
        assert_eq!(error.class, class, "{code}: {error}");
        assert!(error.message.contains(said), "{error}");
        // That was the last of the stream.
        assert!(next_stt(&mut stream.rx).await.is_none());
    }
}

#[tokio::test]
async fn a_stream_that_is_behind_refuses_audio_and_takes_it_again_later() {
    let server = server(&begin(MODEL)).await;
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
fn live() -> AssemblyAiStt {
    let key = Secret::from_env("ASSEMBLYAI_API_KEY").unwrap();
    AssemblyAiStt::new(Config::new(MODEL, Some(key)))
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
#[ignore = "talks to the provider; needs ASSEMBLYAI_API_KEY"]
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
#[ignore = "talks to the provider; needs ASSEMBLYAI_API_KEY"]
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
        // The words, and then that all of the audio was heard. The
        // provider may have ended the turn by itself before it was
        // asked to, in which case the words came during the audio.
        let last = next_final(&mut stream).await;
        assert!(
            last.span.start >= began && last.span.end <= at,
            "turn {turn}"
        );
    }
}

#[tokio::test]
#[ignore = "talks to the provider; needs no key"]
async fn live_a_wrong_key_is_refused() {
    let key = Secret::new("aai-not-a-key");
    let stt = AssemblyAiStt::new(Config::new(MODEL, Some(key)));
    let error = stt.open(SttConfig::new(RATE)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Auth, "{error}");
    assert!(!error.message.contains("aai-not-a-key"), "{error}");
}
