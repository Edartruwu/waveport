//! The adapter against a server on this machine that speaks as the
//! provider's documentation says the provider does, and, when asked,
//! against the provider.
//!
//! Nothing here was recorded. The messages are made up in the shapes the
//! documentation of 15 November 2026 gives, with a tone for audio. They
//! hold the adapter to its own reading of that documentation, and to the
//! rules of the port, which are the same ones the fakes keep. Whether
//! the reading is right is for the tests named `live_*` to say. Those
//! talk to the provider, are ignored unless asked for, and need
//! `CARTESIA_API_KEY`, and a voice in `CARTESIA_VOICE` to use another
//! than the one named here:
//!
//! ```sh
//! cargo test -p waveport-tts-cartesia -- --ignored live_
//! ```

#![expect(
    clippy::unwrap_used,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;

use waveport_adapter_kit::pcm::write_base64;
use waveport_adapter_kit::secret::Secret;
use waveport_domain::event::RetryClass;
use waveport_domain::time::MediaDur;
use waveport_ports::audio::Rate;
use waveport_ports::tts::{Tts, TtsConfig, TtsEvent, TtsMark, TtsStream};
use waveport_testkit::conformance::{tts_abandoned, tts_utterance};
use waveport_testkit::drive::next_tts;
use waveport_testkit::servers::{WsServer, WsStep};
use waveport_tts_cartesia::{CartesiaTts, Config};

const RATE: Rate = Rate::Hz8000;
const MODEL: &str = "sonic-3.6";
/// A voice the provider's documentation uses in its examples.
const VOICE: &str = "228fca29-3a0a-435c-8728-5cb483251068";
const PIECES: [&str; 2] = ["Hello,", " world!"];

/// Waits for the message that says no more text follows.
const UNTIL_FINISHED: &str = "> \"continue\":false";

/// A message that carries `millis` of a tone for `context`.
fn chunk(context: &str, millis: u32) -> String {
    let samples: Vec<i16> = (0..RATE.samples_in_millis(millis))
        .map(|n| if n % 16 < 8 { 2_000 } else { -2_000 })
        .collect();
    let mut data = String::new();
    write_base64(&samples, &mut data);
    format!(
        "< {{\"type\":\"chunk\",\"data\":\"{data}\",\"done\":false,\"status_code\":206,\
         \"step_time\":12,\"context_id\":\"{context}\"}}"
    )
}

/// A message that says when each of these words ends, in seconds.
fn timestamps(context: &str, words: &[(&str, f64)]) -> String {
    let names: Vec<String> = words
        .iter()
        .map(|(word, _)| format!("\"{word}\""))
        .collect();
    let starts: Vec<String> = words
        .iter()
        .map(|(_, end)| (end - 0.1).to_string())
        .collect();
    let ends: Vec<String> = words.iter().map(|(_, end)| end.to_string()).collect();
    format!(
        "< {{\"type\":\"timestamps\",\"done\":false,\"status_code\":206,\"context_id\":\
         \"{context}\",\"word_timestamps\":{{\"words\":[{}],\"start\":[{}],\"end\":[{}]}}}}",
        names.join(","),
        starts.join(","),
        ends.join(",")
    )
}

fn done(context: &str) -> String {
    format!(
        "< {{\"type\":\"done\",\"done\":true,\"status_code\":206,\"context_id\":\"{context}\"}}"
    )
}

/// What the provider says for "Hello, world!" in `context`, once it has
/// all the text: a second of audio in three pieces, and its two words.
fn hello(context: &str) -> String {
    [
        UNTIL_FINISHED.to_owned(),
        chunk(context, 300),
        timestamps(context, &[("Hello", 0.4)]),
        chunk(context, 400),
        timestamps(context, &[("world", 0.9)]),
        chunk(context, 300),
        done(context),
    ]
    .join("\n")
}

/// A server whose connections follow these scripts, one after the other.
async fn server(scripts: &[&str]) -> WsServer {
    WsServer::start(
        scripts
            .iter()
            .map(|script| WsStep::script(script))
            .collect(),
    )
    .await
}

fn config(server: &WsServer) -> Config {
    Config {
        url: server.url("/tts/websocket"),
        timeout: Duration::from_secs(2),
        stall_timeout: Duration::from_secs(2),
        context_prefix: "test".to_owned(),
        ..Config::new(MODEL, VOICE, Some(Secret::new("sk-not-a-key")))
    }
}

/// An adapter that talks to `server`, names its contexts `test-1`,
/// `test-2` and so on, and does not wait long.
fn adapter(server: &WsServer) -> CartesiaTts {
    CartesiaTts::new(config(server))
}

const fn mark(text_end: u32, millis: u32) -> TtsMark {
    TtsMark {
        text_end,
        audio_end: MediaDur::from_millis(millis),
    }
}

/// Opens a stream and gives it the text of "Hello, world!".
async fn say_hello(tts: &CartesiaTts) -> TtsStream {
    let mut stream = tts.open(TtsConfig::new(RATE)).await.unwrap();
    for piece in PIECES {
        stream.tx.try_speak(piece).unwrap();
    }
    stream.tx.try_finish().unwrap();
    stream
}

/// Reads a stream to where it ends or fails, and returns what it gave
/// besides audio, with how many samples of audio there were.
async fn read(stream: &mut TtsStream) -> (Vec<Result<TtsEvent, RetryClass>>, usize) {
    let mut events = Vec::new();
    let mut samples = 0;
    loop {
        let next = tokio::time::timeout(Duration::from_secs(10), next_tts(&mut stream.rx));
        match next.await.unwrap() {
            Some(Ok(TtsEvent::Audio(pcm))) => samples += pcm.len(),
            Some(Ok(event)) => events.push(Ok(event)),
            Some(Err(error)) => events.push(Err(error.class)),
            None => return (events, samples),
        }
    }
}

#[tokio::test]
async fn speech_in_the_documented_shapes_is_read_as_the_port_says() {
    let server = server(&[&hello("test-1")]).await;
    let tts = adapter(&server);
    let seen = tts_utterance(&tts, RATE, &PIECES).await.unwrap();
    assert_eq!(seen.length, MediaDur::from_millis(1_000));
    assert_eq!(seen.samples.len(), 8_000);
    // A mark where each word ends, and one for the end of it all.
    assert_eq!(seen.marks, [mark(5, 400), mark(12, 900), mark(13, 1_000)]);

    // What the provider was sent.
    let connections = server.over(0).await;
    assert_eq!(connections.len(), 1);
    let sent = &connections[0];
    assert_eq!(sent.path, "/tts/websocket?cartesia_version=2026-08-14");
    assert_eq!(sent.header("x-api-key"), Some("sk-not-a-key"));
    let texts: Vec<&str> = sent.texts().collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(
        texts[0],
        "{\"model_id\":\"sonic-3.6\",\"transcript\":\"Hello,\",\"voice\":\
         {\"id\":\"228fca29-3a0a-435c-8728-5cb483251068\"},\"language\":\"en\",\
         \"context_id\":\"test-1\",\"output_format\":{\"container\":\"raw\",\
         \"encoding\":\"pcm_s16le\",\"sample_rate\":8000},\"add_timestamps\":true,\
         \"use_normalized_timestamps\":false,\"continue\":true,\"max_buffer_delay_ms\":0}"
    );
    assert!(
        texts[1].contains("\"transcript\":\" world!\""),
        "{}",
        texts[1]
    );
    assert!(texts[1].contains("\"continue\":true"), "{}", texts[1]);
    assert!(texts[2].contains("\"transcript\":\"\""), "{}", texts[2]);
    assert!(texts[2].contains("\"continue\":false"), "{}", texts[2]);
}

#[tokio::test]
async fn a_connection_is_kept_for_the_next_response() {
    // One connection, and the provider speaks two responses on it.
    let script = format!("{}\n{}", hello("test-1"), hello("test-2"));
    let server = server(&[&script]).await;
    let tts = adapter(&server);
    assert_eq!(tts.kept(), 0);
    for _ in 0..2 {
        let seen = tts_utterance(&tts, RATE, &PIECES).await.unwrap();
        assert_eq!(seen.samples.len(), 8_000);
        assert_eq!(tts.kept(), 1);
    }
    drop(tts);
    let connections = server.over(1).await;
    assert_eq!(
        connections.len(),
        1,
        "the second response made no connection"
    );
    assert_eq!(connections[0].texts().count(), 6);
}

#[tokio::test]
async fn a_response_that_is_abandoned_takes_its_connection_with_it() {
    // The first connection gets some audio out and is dropped by this
    // side. The second speaks the next response whole.
    let begun = format!("> \"transcript\":\" world!\"\n{}", chunk("test-1", 200));
    let server = server(&[&begun, &hello("test-2")]).await;
    let tts = adapter(&server);
    let seen = tts_abandoned(&tts, RATE, &PIECES).await.unwrap();
    assert_eq!(seen.samples.len(), 8_000);
    assert_eq!(seen.marks.last(), Some(&mark(13, 1_000)));

    let connections = server.over(1).await;
    assert_eq!(connections.len(), 2);
    // Nothing was kept of the first, and the second is kept.
    assert_eq!(tts.kept(), 1);
}

#[tokio::test]
async fn a_connection_made_ahead_is_the_one_the_first_response_uses() {
    let server = server(&[&hello("test-1")]).await;
    let tts = adapter(&server);
    tts.warm().await.unwrap();
    assert_eq!(tts.kept(), 1);
    let seen = tts_utterance(&tts, RATE, &PIECES).await.unwrap();
    assert_eq!(seen.samples.len(), 8_000);
    assert_eq!(server.connections().len(), 1);
}

#[tokio::test]
async fn a_kept_connection_that_is_gone_is_replaced_without_anything_lost() {
    // The provider drops the first connection, or closes it, while it is
    // kept. The response that takes it is spoken on a second.
    for gone in ["x", "! 1000 idle for too long"] {
        let server = server(&[gone, &hello("test-1")]).await;
        let tts = adapter(&server);
        tts.warm().await.unwrap();
        // The provider has let go of it by the time it is wanted. Nobody
        // reads a connection that is kept, so this side does not know.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut stream = say_hello(&tts).await;
        let (events, samples) = read(&mut stream).await;
        assert_eq!(samples, 8_000, "{gone}");
        assert_eq!(events.last(), Some(&Ok(TtsEvent::Done)), "{gone}");

        let connections = server.connections();
        assert_eq!(connections.len(), 2, "{gone}");
        // All three messages reached the connection that was alive.
        let texts: Vec<&str> = connections[1].texts().collect();
        assert_eq!(texts.len(), 3, "{gone}: {texts:?}");
        assert!(texts[0].contains("\"transcript\":\"Hello,\""), "{gone}");
        assert!(texts[2].contains("\"continue\":false"), "{gone}");
    }
}

#[tokio::test]
async fn a_connection_kept_for_too_long_is_not_used() {
    let server = server(&[&hello("test-1")]).await;
    let tts = CartesiaTts::new(Config {
        keep: Duration::from_millis(30),
        ..config(&server)
    });
    tts.warm().await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let mut stream = say_hello(&tts).await;
    let (events, _) = read(&mut stream).await;
    assert_eq!(events.last(), Some(&Ok(TtsEvent::Done)));
    assert_eq!(server.connections().len(), 2);
}

#[tokio::test]
async fn a_sending_half_that_goes_away_has_said_that_the_text_is_over() {
    let server = server(&[&hello("test-1")]).await;
    let tts = adapter(&server);
    let TtsStream { mut tx, rx } = tts.open(TtsConfig::new(RATE)).await.unwrap();
    tx.try_speak("Hello,").unwrap();
    tx.try_speak("").unwrap();
    tx.try_speak(" world!").unwrap();
    drop(tx);
    let mut stream = TtsStream {
        tx: Box::new(Deaf),
        rx,
    };
    let (events, samples) = read(&mut stream).await;
    assert_eq!(samples, 8_000);
    assert_eq!(events.last(), Some(&Ok(TtsEvent::Done)));
    // An empty piece was not sent, and the end was.
    let texts = server.connections()[0].texts().count();
    assert_eq!(texts, 3);
}

/// A sending half that stands in for one that was dropped.
struct Deaf;

impl waveport_ports::tts::TtsTx for Deaf {
    fn try_speak(&mut self, _: &str) -> Result<(), waveport_ports::error::Full> {
        Err(waveport_ports::error::Full)
    }

    fn try_finish(&mut self) -> Result<(), waveport_ports::error::Full> {
        Err(waveport_ports::error::Full)
    }
}

#[tokio::test]
async fn what_the_provider_will_not_speak_fails_the_stream() {
    // The shape is the one the documentation gives for an error.
    let refusal = "< {\"type\":\"error\",\"done\":true,\"title\":\"Invalid model\",\"message\":\
                   \"The model is not valid, make sure it is a valid model ID.\",\
                   \"error_code\":\"model_not_found\",\"status_code\":400,\
                   \"request_id\":\"2ff8af53\",\"context_id\":\"test-1\"}";
    let server = server(&[&format!("> \"transcript\":\"Hello,\"\n{refusal}")]).await;
    let tts = adapter(&server);
    let mut stream = say_hello(&tts).await;
    let (events, samples) = read(&mut stream).await;
    assert_eq!((events, samples), (vec![Err(RetryClass::Permanent)], 0));
    // A connection that failed is not kept.
    assert_eq!(tts.kept(), 0);
}

#[tokio::test]
async fn a_provider_that_refuses_the_connection_answers_by_its_status() {
    for (status, class) in [
        (401, RetryClass::Auth),
        (429, RetryClass::RateLimited),
        (503, RetryClass::Transient),
    ] {
        let server = server(&[&format!("? {status} {{\"message\":\"no\"}}")]).await;
        let tts = adapter(&server);
        let error = tts.open(TtsConfig::new(RATE)).await.unwrap_err();
        assert_eq!(error.class, class, "{status}: {error}");
        assert_eq!(tts.warm().await.unwrap_err().class, class, "{status}");
        assert!(!format!("{tts:?}").contains("sk-not-a-key"));
    }
}

#[tokio::test]
async fn a_provider_that_goes_quiet_with_the_speech_not_over_is_not_waited_for() {
    let quiet = format!("{UNTIL_FINISHED}\n{}", chunk("test-1", 200));
    let server = server(&[&quiet]).await;
    let tts = CartesiaTts::new(Config {
        stall_timeout: Duration::from_millis(150),
        ..config(&server)
    });
    let mut stream = say_hello(&tts).await;
    let (events, samples) = read(&mut stream).await;
    assert_eq!(samples, 1_600);
    assert_eq!(events, [Err(RetryClass::Timeout)]);
}

#[tokio::test]
async fn a_provider_that_waits_for_text_is_waited_for() {
    // Nothing says the text is over, so the provider's silence is not a
    // failure: the model may be thinking.
    let server = server(&[&hello("test-1")]).await;
    let tts = CartesiaTts::new(Config {
        stall_timeout: Duration::from_millis(50),
        ..config(&server)
    });
    let mut stream = tts.open(TtsConfig::new(RATE)).await.unwrap();
    stream.tx.try_speak("Hello,").unwrap();
    let waited = tokio::time::timeout(Duration::from_millis(200), next_tts(&mut stream.rx));
    assert!(
        waited.await.is_err(),
        "nothing has come, and nothing failed"
    );
    stream.tx.try_speak(" world!").unwrap();
    stream.tx.try_finish().unwrap();
    let (events, samples) = read(&mut stream).await;
    assert_eq!(samples, 8_000);
    assert_eq!(events.last(), Some(&Ok(TtsEvent::Done)));
}

#[tokio::test]
async fn a_connection_that_drops_in_the_middle_fails_the_stream() {
    for gone in ["x", "! 1000 bye"] {
        let dropped = format!("{UNTIL_FINISHED}\n{}\n{gone}", chunk("test-1", 200));
        let server = server(&[&dropped]).await;
        let tts = adapter(&server);
        let mut stream = say_hello(&tts).await;
        let (events, samples) = read(&mut stream).await;
        assert_eq!(samples, 1_600, "{gone}");
        assert_eq!(events, [Err(RetryClass::Transient)], "{gone}");
        assert_eq!(tts.kept(), 0, "{gone}");
    }
}

#[tokio::test]
async fn a_stream_says_the_language_and_the_rate_it_was_opened_with() {
    let server = server(&[&hello("test-1")]).await;
    let tts = adapter(&server);
    let config = TtsConfig {
        language: Some("es-PE".to_owned()),
        ..TtsConfig::new(Rate::Hz24000)
    };
    let mut stream = tts.open(config).await.unwrap();
    stream.tx.try_speak("Hola.").unwrap();
    stream.tx.try_finish().unwrap();
    let _ = read(&mut stream).await;
    let connections = server.connections();
    let asked = connections[0].texts().next().unwrap();
    assert!(asked.contains("\"language\":\"es\""), "{asked}");
    assert!(asked.contains("\"sample_rate\":24000"), "{asked}");
    assert_eq!(tts.caps().rates.iter().count(), 4);
}

// ---------------------------------------------------------------------
// Against the provider
// ---------------------------------------------------------------------

fn live() -> CartesiaTts {
    let key = Secret::from_env("CARTESIA_API_KEY").unwrap();
    let voice = std::env::var("CARTESIA_VOICE").unwrap_or_else(|_| VOICE.to_owned());
    CartesiaTts::new(Config::new(MODEL, &voice, Some(key)))
}

const SENTENCES: [&str; 2] = ["It is ten past four.", " Is there anything else I can do?"];

#[tokio::test]
#[ignore = "talks to the provider; needs CARTESIA_API_KEY"]
async fn live_two_sentences_are_spoken_as_the_port_says() {
    let tts = live();
    tts.warm().await.unwrap();
    let seen = tts_utterance(&tts, Rate::Hz24000, &SENTENCES)
        .await
        .unwrap();
    // Eleven words at the pace of speech.
    let millis = seen.length.as_millis();
    assert!((1_500..9_000).contains(&millis), "{millis} ms of audio");
    assert!(seen.marks.len() >= 8, "{:?}", seen.marks);
    let loud = seen
        .samples
        .iter()
        .filter(|s| s.unsigned_abs() > 500)
        .count();
    assert!(
        loud > seen.samples.len() / 20,
        "the audio is all but silent"
    );
    // The connection is kept, and the next response is spoken on it.
    assert_eq!(tts.kept(), 1);
    let again = tts_utterance(&tts, Rate::Hz16000, &SENTENCES)
        .await
        .unwrap();
    assert!((1_500..9_000).contains(&again.length.as_millis()));
}

#[tokio::test]
#[ignore = "talks to the provider; needs CARTESIA_API_KEY"]
async fn live_a_response_that_is_abandoned_leaves_the_next_one_whole() {
    let seen = tts_abandoned(&live(), Rate::Hz24000, &SENTENCES)
        .await
        .unwrap();
    assert!(seen.marks.len() >= 8, "{:?}", seen.marks);
}

#[tokio::test]
#[ignore = "talks to the provider; needs no key"]
async fn live_a_wrong_key_is_refused() {
    let tts = CartesiaTts::new(Config::new(MODEL, VOICE, Some(Secret::new("sk-not-a-key"))));
    let failed = match tts.open(TtsConfig::new(Rate::Hz24000)).await {
        Err(error) => error,
        // A provider that takes the connection says no once it is asked
        // for something.
        Ok(mut stream) => {
            stream.tx.try_speak("Hello.").unwrap();
            stream.tx.try_finish().unwrap();
            loop {
                match next_tts(&mut stream.rx).await {
                    Some(Err(error)) => break error,
                    Some(Ok(_)) => {}
                    None => panic!("a wrong key was taken"),
                }
            }
        }
    };
    assert_eq!(failed.class, RetryClass::Auth, "{failed}");
    assert!(!failed.message.contains("sk-not-a-key"), "{failed}");
}
