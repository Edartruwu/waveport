//! The adapter against a server on this machine that answers as the
//! provider was recorded to, and, when asked, against the provider.
//!
//! `fixtures/` holds sessions with the provider as they went over the
//! wire on 15 November 2026. What the provider said is kept as it said
//! it. Of what it was sent only the kind is kept, and the server waits
//! for a message of that kind before it says what the provider said
//! next. The checks of `waveport_testkit::conformance` are the same ones
//! that the fakes pass.
//!
//! The tests named `live_*` talk to the provider. They are ignored
//! unless asked for, and need `OPENAI_API_KEY`:
//!
//! ```sh
//! cargo test -p waveport-stt-openai -- --ignored live_
//! ```

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;

use waveport_adapter_kit::pcm::read_base64;
use waveport_adapter_kit::secret::Secret;
use waveport_domain::event::RetryClass;
use waveport_domain::time::{MediaSpan, MediaTime};
use waveport_ports::audio::{AudioRef, PcmBytes, Rate};
use waveport_ports::stt::{Stt, SttConfig, SttEvent, SttStream, Transcript};
use waveport_stt_openai::{Config, OpenAiStt};
use waveport_testkit::conformance::{Talk, stt_utterance};
use waveport_testkit::drive::next_stt;
use waveport_testkit::servers::{WsSeen, WsServer, WsStep};
use waveport_testkit::speech::{WHAT_TIME_IS_IT, framed, what_time_is_it};

const QUESTION: &str = include_str!("fixtures/question.ws");
const TWO_TURNS: &str = include_str!("fixtures/two_turns.ws");
const SILENCE: &str = include_str!("fixtures/silence.ws");
const AT_ONCE: &str = include_str!("fixtures/at_once.ws");
const TOO_LITTLE: &str = include_str!("fixtures/too_little.ws");
const UNKNOWN_LANGUAGE: &str = include_str!("fixtures/unknown_language.ws");
const WRONG_KEY: &str = include_str!("fixtures/wrong_key.ws");
const EXPIRED: &str = include_str!("fixtures/expired.ws");

const RATE: Rate = Rate::Hz24000;
const MODEL: &str = "gpt-live-transcribe";

const fn ms(millis: u64) -> MediaTime {
    MediaTime::from_millis(millis)
}

/// The question, with the quiet before and after it that a caller
/// leaves.
fn question() -> Vec<i16> {
    framed(RATE, 300, &what_time_is_it(RATE), 700)
}

/// A server that follows `script` on every connection.
async fn server(script: &str) -> WsServer {
    WsServer::start(vec![WsStep::script(script)]).await
}

/// The first lines of a recorded session, up to where the provider says
/// that the session is as asked, and then `rest`.
fn after_setup(rest: &str) -> String {
    let setup: Vec<&str> = QUESTION
        .lines()
        .filter(|line| !line.starts_with('#'))
        .take(3)
        .collect();
    let last = setup.last().unwrap();
    assert!(last.contains("session.updated"), "{last}");
    format!("{}\n{rest}\n", setup.join("\n"))
}

/// An adapter that talks to `server`, and that does not wait long.
fn adapter(server: &WsServer) -> OpenAiStt {
    OpenAiStt::new(Config {
        url: server.url("/v1/realtime"),
        timeout: Duration::from_secs(2),
        answer_timeout: Duration::from_secs(2),
        ..Config::new(MODEL, Some(Secret::new("sk-not-a-key")))
    })
}

/// A stream of `stt` for audio at the one rate there is.
async fn open(stt: &OpenAiStt) -> SttStream {
    stt.open(SttConfig::new(RATE)).await.unwrap()
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
    let mut samples = Vec::new();
    for text in seen.texts() {
        let Some(rest) = text.strip_prefix("{\"type\":\"input_audio_buffer.append\",\"audio\":\"")
        else {
            continue;
        };
        let bytes = read_base64(rest.strip_suffix("\"}").unwrap()).unwrap();
        samples.extend(PcmBytes::new(bytes).unwrap().samples());
    }
    samples
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
async fn a_recorded_question_is_heard_as_the_port_says() {
    let server = server(QUESTION).await;
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

    // Its words are of all the audio that was sent, there being no times.
    let all = MediaSpan::new(ms(1_000), ms(1_000).after(RATE.length_of(audio.len())));
    assert_eq!(seen.finals.len(), 1);
    assert_eq!(seen.finals[0].span, all);
    // What came before the final transcript grew a word at a time.
    let grew: Vec<&str> = seen.interim.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(
        grew,
        [
            "What",
            "What time",
            "What time is",
            "What time is it",
            "What time is it right",
            "What time is it right now",
            "What time is it right now?",
        ]
    );
    assert!(seen.interim.iter().all(|t| t.span.start == all.start));
    assert_eq!(seen.interim.last().unwrap().span, all);

    // What the provider was sent.
    let connections = server.over(1).await;
    assert_eq!(connections.len(), 1);
    let sent = &connections[0];
    assert_eq!(sent.path, "/v1/realtime?intent=transcription");
    assert_eq!(sent.header("authorization"), Some("Bearer sk-not-a-key"));
    let texts: Vec<&str> = sent.texts().collect();
    assert_eq!(
        texts[0],
        "{\"type\":\"session.update\",\"session\":{\"type\":\"transcription\",\"audio\":\
         {\"input\":{\"format\":{\"type\":\"audio/pcm\",\"rate\":24000},\"transcription\":\
         {\"model\":\"gpt-live-transcribe\"},\"turn_detection\":null}}}}"
    );
    // All the audio, as it was, and then the one request.
    assert_eq!(audio_sent(sent), audio);
    assert_eq!(
        *texts.last().unwrap(),
        "{\"type\":\"input_audio_buffer.commit\",\"event_id\":\"commit_1\"}"
    );
    assert_eq!(
        texts.iter().filter(|text| text.contains("commit")).count(),
        1
    );
}

#[tokio::test]
async fn a_question_sent_at_once_is_heard_the_same() {
    // The provider says it has the request before it has written a word.
    let server = server(AT_ONCE).await;
    let stt = adapter(&server);
    let audio = question();
    let talk = Talk {
        at: ms(0),
        rate: RATE,
        samples: &audio,
        paced: false,
    };
    let seen = stt_utterance(&stt, talk).await.unwrap();
    assert_eq!(seen.text(), WHAT_TIME_IS_IT);
    assert_eq!(seen.interim.len(), 7);
    let all = seen.finals[0].span;
    assert!(seen.interim.iter().all(|t| t.span == all));
}

#[tokio::test]
async fn silence_is_heard_as_no_words() {
    let server = server(SILENCE).await;
    let stt = adapter(&server);
    let audio = vec![0; RATE.samples_in_millis(2_000)];
    let talk = Talk {
        at: ms(40),
        rate: RATE,
        samples: &audio,
        paced: false,
    };
    let seen = stt_utterance(&stt, talk).await.unwrap();
    assert_eq!(seen.interim, []);
    assert_eq!(seen.finals.len(), 1);
    assert_eq!(seen.finals[0].text, "");
    assert_eq!(seen.finals[0].span, MediaSpan::new(ms(40), ms(2_040)));
}

#[tokio::test]
async fn two_turns_on_one_stream_are_each_of_their_own_audio() {
    let server = server(TWO_TURNS).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let audio = question();

    let first_end = send(&mut stream, ms(5_000), &audio);
    stream.tx.try_finalize().unwrap();
    let first = next_final(&mut stream).await;
    assert_eq!(first.text, WHAT_TIME_IS_IT);
    assert_eq!(first.span, MediaSpan::new(ms(5_000), first_end));

    // The caller is quiet for a while, and the audio of that is not sent.
    let second_end = send(&mut stream, ms(20_000), &audio);
    stream.tx.try_finalize().unwrap();
    let second = next_final(&mut stream).await;
    assert_eq!(second.text, WHAT_TIME_IS_IT);
    assert_eq!(second.span, MediaSpan::new(ms(20_000), second_end));

    drop(stream);
    let connections = server.over(1).await;
    let commits: Vec<&str> = connections[0]
        .texts()
        .filter(|text| text.contains("input_audio_buffer.commit"))
        .collect();
    assert_eq!(
        commits,
        [
            "{\"type\":\"input_audio_buffer.commit\",\"event_id\":\"commit_1\"}",
            "{\"type\":\"input_audio_buffer.commit\",\"event_id\":\"commit_2\"}",
        ]
    );
    assert_eq!(audio_sent(&connections[0]).len(), audio.len() * 2);
}

#[tokio::test]
async fn a_request_the_provider_refuses_for_want_of_audio_gives_no_words() {
    // The provider's count of the audio is the one that decides. Here it
    // finds too little where the adapter found enough.
    let server = server(TOO_LITTLE).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;

    let quiet = vec![0; RATE.samples_in_millis(200)];
    let quiet_end = send(&mut stream, ms(0), &quiet);
    stream.tx.try_finalize().unwrap();
    let nothing = next_final(&mut stream).await;
    assert_eq!(nothing.text, "");
    assert_eq!(nothing.span, MediaSpan::new(ms(0), quiet_end));

    // The stream is as good as before.
    let audio = question();
    let end = send(&mut stream, quiet_end, &audio);
    stream.tx.try_finalize().unwrap();
    let asked = next_final(&mut stream).await;
    assert_eq!(asked.text, WHAT_TIME_IS_IT);
    assert_eq!(asked.span, MediaSpan::new(quiet_end, end));
}

#[tokio::test]
async fn a_request_for_no_audio_is_answered_without_asking_the_provider() {
    let server = server(&after_setup("")).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    stream.tx.try_finalize().unwrap();
    let nothing = next_final(&mut stream).await;
    assert_eq!((nothing.text.as_str(), nothing.span.is_empty()), ("", true));

    drop(stream);
    let connections = server.over(1).await;
    let texts: Vec<&str> = connections[0].texts().collect();
    assert_eq!(texts.len(), 1, "only the session was asked for: {texts:?}");
}

#[tokio::test]
async fn a_final_transcript_that_was_asked_for_still_comes_when_no_more_audio_will() {
    let server = server(QUESTION).await;
    let stt = adapter(&server);
    let SttStream { mut tx, mut rx } = open(&stt).await;
    let audio = question();
    let mut at = ms(0);
    for piece in audio.chunks(RATE.samples_in_millis(100)) {
        let audio = AudioRef {
            at,
            rate: RATE,
            samples: piece,
        };
        tx.try_send(audio).unwrap();
        at = audio.span().end;
    }
    tx.try_finalize().unwrap();
    drop(tx);

    let mut finals = Vec::new();
    while let Some(event) = next_stt(&mut rx).await {
        let SttEvent::Transcript(transcript) = event.unwrap();
        if transcript.is_final {
            finals.push(transcript.text);
        }
    }
    assert_eq!(finals, [WHAT_TIME_IS_IT]);
    // The stream is over, and the connection was closed in good order.
    assert!(next_stt(&mut rx).await.is_none());
    assert_eq!(server.over(1).await.len(), 1);
}

#[tokio::test]
async fn a_language_the_provider_does_not_know_fails_the_stream_as_it_opens() {
    let server = server(UNKNOWN_LANGUAGE).await;
    let stt = adapter(&server);
    let config = SttConfig {
        language: Some("klingon".to_owned()),
        ..SttConfig::new(RATE)
    };
    let error = stt.open(config).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent, "{error}");
    assert!(error.message.contains("'klingon'"), "{error}");
    // The language was asked for by the provider's code.
    let connections = server.over(1).await;
    let asked = connections[0].texts().next().unwrap();
    assert!(asked.contains("\"languages\":[\"klingon\"]"), "{asked}");
}

#[tokio::test]
async fn a_key_the_provider_does_not_know_fails_the_stream_as_it_opens() {
    let server = server(WRONG_KEY).await;
    let stt = adapter(&server);
    let error = stt.open(SttConfig::new(RATE)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Auth, "{error}");
    // The provider repeats the key. What is passed on does not.
    assert!(WRONG_KEY.contains("sk-not-a-key"));
    assert!(!error.message.contains("sk-not-a-key"), "{error}");
    assert!(!format!("{stt:?}").contains("sk-not-a-key"));
}

#[tokio::test]
async fn a_provider_that_refuses_the_connection_answers_by_its_status() {
    for (status, class) in [
        (401, RetryClass::Auth),
        (429, RetryClass::RateLimited),
        (503, RetryClass::Transient),
    ] {
        let server = server(&format!("? {status} {{\"error\":{{\"message\":\"no\"}}}}")).await;
        let error = adapter(&server)
            .open(SttConfig::new(RATE))
            .await
            .unwrap_err();
        assert_eq!(error.class, class, "{status}: {error}");
    }
}

#[tokio::test]
async fn a_provider_that_does_not_set_the_session_up_is_not_waited_for() {
    // It takes the connection, greets, and then says nothing.
    let greeting = QUESTION.lines().nth(2).unwrap();
    assert!(greeting.contains("session.created"));
    let server = server(greeting).await;
    let stt = OpenAiStt::new(Config {
        url: server.url("/v1/realtime"),
        timeout: Duration::from_millis(150),
        ..Config::new(MODEL, None)
    });
    let error = stt.open(SttConfig::new(RATE)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Timeout, "{error}");

    // One that closes instead is worth another try.
    let server = self::server(&format!("{greeting}\n! 1000 bye")).await;
    let error = adapter(&server)
        .open(SttConfig::new(RATE))
        .await
        .unwrap_err();
    assert_eq!(error.class, RetryClass::Transient, "{error}");
}

#[tokio::test]
async fn audio_at_another_rate_is_refused_before_anything_is_sent() {
    let server = server(QUESTION).await;
    let stt = adapter(&server);
    assert!(stt.caps().rates.contains(RATE));
    assert_eq!(stt.caps().rates.iter().count(), 1);
    let error = stt.open(SttConfig::new(Rate::Hz16000)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent, "{error}");
    assert_eq!(server.connections(), []);
}

#[tokio::test]
async fn a_connection_that_drops_ends_the_stream_with_a_failure_worth_another_try() {
    let server = server(&after_setup("> \"type\":\"input_audio_buffer.append\"\nx")).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    send(&mut stream, ms(0), &[0; 2_400]);
    let error = next_stt(&mut stream.rx).await.unwrap().unwrap_err();
    assert_eq!(error.class, RetryClass::Transient, "{error}");
    assert!(next_stt(&mut stream.rx).await.is_none());
}

#[tokio::test]
async fn a_provider_that_does_not_answer_a_request_fails_the_stream() {
    // It has the request, and says nothing about it.
    let server = server(&after_setup("> \"type\":\"input_audio_buffer.commit\"")).await;
    let stt = OpenAiStt::new(Config {
        url: server.url("/v1/realtime"),
        answer_timeout: Duration::from_millis(150),
        ..Config::new(MODEL, None)
    });
    let mut stream = open(&stt).await;
    send(&mut stream, ms(0), &[0; 4_800]);
    stream.tx.try_finalize().unwrap();
    let error = next_stt(&mut stream.rx).await.unwrap().unwrap_err();
    assert_eq!(error.class, RetryClass::Timeout, "{error}");
    // No key was configured, and none was sent.
    let connections = server.over(1).await;
    assert_eq!(connections[0].header("authorization"), None);
}

#[tokio::test]
async fn a_provider_that_says_it_failed_ends_the_stream() {
    // Not recorded: the provider did not fail while it was listened to.
    // The shape is the one all its errors have.
    let failure = "< {\"type\":\"error\",\"event_id\":\"event_1\",\"error\":{\"type\":\
                   \"server_error\",\"code\":null,\"message\":\"The server had an error while \
                   processing your request.\",\"param\":null,\"event_id\":null}}";
    let script = after_setup(&format!(
        "> \"type\":\"input_audio_buffer.append\"\n{failure}"
    ));
    let server = server(&script).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    send(&mut stream, ms(0), &[0; 2_400]);
    let error = next_stt(&mut stream.rx).await.unwrap().unwrap_err();
    assert_eq!(error.class, RetryClass::Transient, "{error}");
    assert!(error.message.contains("The server had an error"), "{error}");
}

#[tokio::test]
async fn a_session_the_provider_ends_after_its_hour_is_worth_another() {
    // The provider calls it a request that was wrong. It was not: the
    // next session is as good as this one was.
    let server = server(EXPIRED).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let error = next_stt(&mut stream.rx).await.unwrap().unwrap_err();
    assert_eq!(error.class, RetryClass::Transient, "{error}");
    assert!(error.message.contains("session_expired"), "{error}");
    assert!(error.message.contains("60 minutes"), "{error}");
    // That was the last of the stream.
    assert!(next_stt(&mut stream.rx).await.is_none());
}

#[tokio::test]
async fn a_stream_that_is_behind_refuses_audio_and_takes_it_again_later() {
    let server = server(&after_setup("")).await;
    let stt = adapter(&server);
    let mut stream = open(&stt).await;
    let piece = [0_i16; 480];
    let audio = |at: u64| AudioRef {
        at: ms(at),
        rate: RATE,
        samples: &piece,
    };
    // Nothing runs the loop, so the room fills.
    let mut taken = 0_u64;
    while stream.tx.try_send(audio(taken * 20)).is_ok() {
        taken += 1;
        assert!(taken < 1_000, "there is no end to the room");
    }
    assert!(taken >= 16, "{taken}");
    assert!(stream.tx.try_finalize().is_err());
    // An empty piece asks for no room.
    let nothing = AudioRef {
        samples: &[],
        ..audio(0)
    };
    stream.tx.try_send(nothing).unwrap();

    // Running the loop makes room.
    let ran = tokio::time::timeout(Duration::from_millis(100), next_stt(&mut stream.rx));
    assert!(ran.await.is_err(), "the provider has nothing to say yet");
    stream.tx.try_send(audio(taken * 20)).unwrap();
    stream.tx.try_finalize().unwrap();
}

// ---------------------------------------------------------------------
// Against the provider
// ---------------------------------------------------------------------

fn live() -> OpenAiStt {
    let key = Secret::from_env("OPENAI_API_KEY").unwrap();
    OpenAiStt::new(Config::new(MODEL, Some(key)))
}

#[tokio::test]
#[ignore = "talks to the provider; needs OPENAI_API_KEY"]
async fn live_a_question_is_heard_as_the_port_says() {
    let audio = question();
    let talk = Talk {
        at: ms(3_000),
        rate: RATE,
        samples: &audio,
        paced: true,
    };
    let seen = stt_utterance(&live(), talk).await.unwrap();
    assert_eq!(plain(&seen.text()), plain(WHAT_TIME_IS_IT));
    assert_eq!(seen.finals.len(), 1);
    assert!(
        !seen.interim.is_empty(),
        "words came before they were final"
    );
    let last = seen.interim.last().unwrap();
    assert_eq!(plain(&last.text), plain(WHAT_TIME_IS_IT));
}

#[tokio::test]
#[ignore = "talks to the provider; needs OPENAI_API_KEY"]
async fn live_two_turns_on_one_stream_are_each_heard() {
    let stt = live();
    let config = SttConfig {
        language: Some("en-US".to_owned()),
        ..SttConfig::new(RATE)
    };
    let mut stream = stt.open(config).await.unwrap();
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
        let heard = next_final(&mut stream).await;
        assert_eq!(plain(&heard.text), plain(WHAT_TIME_IS_IT), "turn {turn}");
        assert_eq!(heard.span, MediaSpan::new(began, at), "turn {turn}");
    }
}

#[tokio::test]
#[ignore = "talks to the provider; needs no key"]
async fn live_a_wrong_key_is_refused() {
    let key = Secret::new("sk-not-a-key");
    let stt = OpenAiStt::new(Config::new(MODEL, Some(key)));
    let error = stt.open(SttConfig::new(RATE)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Auth, "{error}");
    assert!(!error.message.contains("sk-not-a-key"), "{error}");
}
