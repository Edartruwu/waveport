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
//! `ELEVENLABS_API_KEY`, and a voice in `ELEVENLABS_VOICE` to use another
//! than the one named here:
//!
//! ```sh
//! cargo test -p waveport-tts-elevenlabs -- --ignored live_
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
use waveport_ports::tts::{Alignment, Tts, TtsConfig, TtsEvent, TtsMark, TtsStream};
use waveport_testkit::conformance::{tts_abandoned, tts_utterance};
use waveport_testkit::drive::next_tts;
use waveport_testkit::servers::{WsServer, WsStep};
use waveport_tts_elevenlabs::{Config, ElevenLabsTts};

const RATE: Rate = Rate::Hz16000;
const MODEL: &str = "eleven_flash_v2_5";
/// A voice the provider's documentation uses in its examples.
const VOICE: &str = "Xb7hH8MSUJpSbSDYk0k2";
const PIECES: [&str; 2] = ["Hello,", " world!"];

/// Waits for the message that says no more text follows.
const UNTIL_FINISHED: &str = "> \"close_context\":true";

/// A message that carries `millis` of a tone for `context`, in which
/// `text` is spoken, every character taking `each` ms from where the
/// tone begins.
fn audio(context: &str, millis: u32, text: &str, each: u32) -> String {
    let samples: Vec<i16> = (0..RATE.samples_in_millis(millis))
        .map(|n| if n % 16 < 8 { 2_000 } else { -2_000 })
        .collect();
    let mut data = String::new();
    write_base64(&samples, &mut data);
    let chars: Vec<String> = text.chars().map(|c| format!("\"{c}\"")).collect();
    let starts: Vec<String> = (0..chars.len())
        .map(|n| (u32::try_from(n).unwrap() * each).to_string())
        .collect();
    let lengths = vec![each.to_string(); chars.len()];
    let alignment = if text.is_empty() {
        "null".to_owned()
    } else {
        format!(
            "{{\"chars\":[{}],\"charStartTimesMs\":[{}],\"charDurationsMs\":[{}]}}",
            chars.join(","),
            starts.join(","),
            lengths.join(",")
        )
    };
    format!(
        "< {{\"audio\":\"{data}\",\"contextId\":\"{context}\",\"alignment\":{alignment},\
         \"normalizedAlignment\":{alignment}}}"
    )
}

fn over(context: &str) -> String {
    format!("< {{\"isFinal\":true,\"contextId\":\"{context}\"}}")
}

/// What the provider says for "Hello, world!" in `context`, once it has
/// all the text: a second of audio in three pieces, the first two with
/// the characters spoken in them.
fn hello(context: &str) -> String {
    [
        UNTIL_FINISHED.to_owned(),
        // The space a context is begun with is among the characters.
        audio(context, 300, " Hello,", 20),
        audio(context, 400, " world!", 30),
        audio(context, 300, "", 0),
        over(context),
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
        url: server.url("/v1/text-to-speech"),
        rate: RATE,
        timeout: Duration::from_secs(2),
        stall_timeout: Duration::from_secs(2),
        context_prefix: "test".to_owned(),
        ..Config::new(MODEL, VOICE, Some(Secret::new("sk-not-a-key")))
    }
}

/// An adapter that talks to `server`, names its contexts `test-1`,
/// `test-2` and so on, and does not wait long.
fn adapter(server: &WsServer) -> ElevenLabsTts {
    ElevenLabsTts::new(config(server)).unwrap()
}

const fn mark(text_end: u32, millis: u32) -> TtsMark {
    TtsMark {
        text_end,
        audio_end: MediaDur::from_millis(millis),
    }
}

/// Opens a stream and gives it the text of "Hello, world!".
async fn say_hello(tts: &ElevenLabsTts) -> TtsStream {
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
    assert_eq!(tts.caps().alignment, Alignment::Char);
    let seen = tts_utterance(&tts, RATE, &PIECES).await.unwrap();
    assert_eq!(seen.length, MediaDur::from_millis(1_000));
    assert_eq!(seen.samples.len(), 16_000);
    // A mark where each character ends. Those of the second piece of
    // audio are timed from where it begins, 300 ms into the speech.
    assert_eq!(
        seen.marks,
        [
            mark(1, 40),
            mark(2, 60),
            mark(3, 80),
            mark(4, 100),
            mark(5, 120),
            mark(6, 140),
            mark(7, 330),
            mark(8, 360),
            mark(9, 390),
            mark(10, 420),
            mark(11, 450),
            mark(12, 480),
            mark(13, 510),
        ]
    );

    // What the provider was sent.
    let connections = server.over(0).await;
    assert_eq!(connections.len(), 1);
    let sent = &connections[0];
    assert_eq!(
        sent.path,
        "/v1/text-to-speech/Xb7hH8MSUJpSbSDYk0k2/multi-stream-input?model_id=eleven_flash_v2_5\
         &output_format=pcm_16000&inactivity_timeout=180&sync_alignment=true"
    );
    assert_eq!(sent.header("xi-api-key"), Some("sk-not-a-key"));
    let texts: Vec<&str> = sent.texts().collect();
    assert_eq!(
        texts,
        [
            "{\"text\":\" \",\"context_id\":\"test-1\"}",
            "{\"text\":\"Hello,\",\"context_id\":\"test-1\",\"flush\":true}",
            "{\"text\":\" world!\",\"context_id\":\"test-1\",\"flush\":true}",
            "{\"context_id\":\"test-1\",\"close_context\":true}",
        ]
    );
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
        assert_eq!(seen.samples.len(), 16_000);
        assert_eq!(tts.kept(), 1);
    }
    drop(tts);
    let connections = server.over(1).await;
    assert_eq!(
        connections.len(),
        1,
        "the second response made no connection"
    );
    assert_eq!(connections[0].texts().count(), 8);
}

#[tokio::test]
async fn a_response_that_is_abandoned_takes_its_connection_with_it() {
    // The first connection gets some audio out and is dropped by this
    // side. The second speaks the next response whole.
    let begun = format!(
        "> \"text\":\" world!\"\n{}",
        audio("test-1", 200, " Hel", 20)
    );
    let server = server(&[&begun, &hello("test-2")]).await;
    let tts = adapter(&server);
    let seen = tts_abandoned(&tts, RATE, &PIECES).await.unwrap();
    assert_eq!(seen.samples.len(), 16_000);
    assert_eq!(seen.marks.last(), Some(&mark(13, 510)));

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
    assert_eq!(seen.samples.len(), 16_000);
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
        assert_eq!(samples, 16_000, "{gone}");
        assert_eq!(events.last(), Some(&Ok(TtsEvent::Done)), "{gone}");

        let connections = server.connections();
        assert_eq!(connections.len(), 2, "{gone}");
        // All four messages reached the connection that was alive.
        let texts: Vec<&str> = connections[1].texts().collect();
        assert_eq!(texts.len(), 4, "{gone}: {texts:?}");
        assert!(texts[0].contains("\"text\":\" \""), "{gone}");
        assert!(texts[1].contains("\"text\":\"Hello,\""), "{gone}");
        assert!(texts[3].contains("\"close_context\":true"), "{gone}");
    }
}

#[tokio::test]
async fn a_connection_kept_for_too_long_is_not_used() {
    let server = server(&[&hello("test-1")]).await;
    let tts = ElevenLabsTts::new(Config {
        keep: Duration::from_millis(30),
        ..config(&server)
    })
    .unwrap();
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
    assert_eq!(samples, 16_000);
    assert_eq!(events.last(), Some(&Ok(TtsEvent::Done)));
    // An empty piece was not sent, and the end was.
    let texts = server.connections()[0].texts().count();
    assert_eq!(texts, 4);
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
async fn the_end_of_a_context_said_too_early_does_not_end_the_speech() {
    // The provider says that the context is over after the first piece,
    // which it is not: more text follows.
    let script = [
        "> \"text\":\"Hello,\"".to_owned(),
        audio("test-1", 300, " Hello,", 20),
        over("test-1"),
        UNTIL_FINISHED.to_owned(),
        audio("test-1", 700, " world!", 30),
        over("test-1"),
    ]
    .join("\n");
    let server = server(&[&script]).await;
    let tts = adapter(&server);
    let mut stream = tts.open(TtsConfig::new(RATE)).await.unwrap();
    stream.tx.try_speak("Hello,").unwrap();
    // The first piece comes whole before the next is given.
    let mut marks = 0;
    while marks < 6 {
        match next_tts(&mut stream.rx).await {
            Some(Ok(TtsEvent::Mark(_))) => marks += 1,
            Some(Ok(TtsEvent::Audio(_))) => {}
            other => panic!("the speech ended early: {other:?}"),
        }
    }
    // And then the word that the context is over, which ends nothing.
    let early = tokio::time::timeout(Duration::from_millis(100), next_tts(&mut stream.rx));
    assert!(early.await.is_err());
    stream.tx.try_speak(" world!").unwrap();
    stream.tx.try_finish().unwrap();
    let (events, samples) = read(&mut stream).await;
    assert_eq!(samples, 11_200, "{events:?}");
    assert_eq!(events.len(), 8, "{events:?}");
    assert_eq!(events.last(), Some(&Ok(TtsEvent::Done)));
}

#[tokio::test]
async fn what_is_said_about_an_earlier_response_is_not_taken_for_this_one() {
    // On a kept connection, the provider still has something to say
    // about the response before.
    let script = [
        hello("test-1"),
        "> \"text\":\"Hello,\"".to_owned(),
        audio("test-1", 500, "late", 10),
        over("test-1"),
        hello("test-2"),
    ]
    .join("\n");
    let server = server(&[&script]).await;
    let tts = adapter(&server);
    for _ in 0..2 {
        let seen = tts_utterance(&tts, RATE, &PIECES).await.unwrap();
        assert_eq!(seen.samples.len(), 16_000);
        assert_eq!(seen.marks.len(), 13);
    }
}

#[tokio::test]
async fn what_the_provider_will_not_speak_fails_the_stream() {
    // The documentation does not show a failure. This is the shape the
    // adapter takes one to have: a name for it, and words.
    let refusal = "< {\"message\":\"A voice with that voice_id does not exist.\",\
                   \"error\":\"voice_id_does_not_exist\",\"code\":1008}";
    let server = server(&[&format!("> \"text\":\"Hello,\"\n{refusal}")]).await;
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
        // The provider may repeat what it was sent.
        let said = "{\"detail\":{\"status\":\"invalid_api_key\",\"message\":\"sk-not-a-key\"}}";
        let server = server(&[&format!("? {status} {said}")]).await;
        let tts = adapter(&server);
        let error = tts.open(TtsConfig::new(RATE)).await.unwrap_err();
        assert_eq!(error.class, class, "{status}: {error}");
        assert!(!error.message.contains("sk-not-a-key"), "{error}");
        assert_eq!(tts.warm().await.unwrap_err().class, class, "{status}");
        assert!(!format!("{tts:?}").contains("sk-not-a-key"));
    }
}

#[tokio::test]
async fn a_provider_that_goes_quiet_with_the_speech_not_over_is_not_waited_for() {
    let quiet = format!("{UNTIL_FINISHED}\n{}", audio("test-1", 200, "", 0));
    let server = server(&[&quiet]).await;
    let tts = ElevenLabsTts::new(Config {
        stall_timeout: Duration::from_millis(150),
        ..config(&server)
    })
    .unwrap();
    let mut stream = say_hello(&tts).await;
    let (events, samples) = read(&mut stream).await;
    assert_eq!(samples, 3_200);
    assert_eq!(events, [Err(RetryClass::Timeout)]);
}

#[tokio::test]
async fn a_provider_that_waits_for_text_is_waited_for() {
    // Nothing says the text is over, so the provider's silence is not a
    // failure: the model may be thinking.
    let server = server(&[&hello("test-1")]).await;
    let tts = ElevenLabsTts::new(Config {
        stall_timeout: Duration::from_millis(50),
        ..config(&server)
    })
    .unwrap();
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
    assert_eq!(samples, 16_000);
    assert_eq!(events.last(), Some(&Ok(TtsEvent::Done)));
}

#[tokio::test]
async fn a_connection_that_drops_in_the_middle_fails_the_stream() {
    for (gone, class) in [
        ("x", RetryClass::Transient),
        ("! 1000 bye", RetryClass::Transient),
        // The provider closes with a complaint.
        ("! 1008 not allowed", RetryClass::Permanent),
    ] {
        let dropped = format!("{UNTIL_FINISHED}\n{}\n{gone}", audio("test-1", 200, "", 0));
        let server = server(&[&dropped]).await;
        let tts = adapter(&server);
        let mut stream = say_hello(&tts).await;
        let (events, samples) = read(&mut stream).await;
        assert_eq!(samples, 3_200, "{gone}");
        assert_eq!(events, [Err(class)], "{gone}");
        assert_eq!(tts.kept(), 0, "{gone}");
    }
}

#[tokio::test]
async fn an_adapter_gives_audio_at_the_one_rate_it_was_set_up_with() {
    let server = server(&[]).await;
    let tts = adapter(&server);
    let rates: Vec<Rate> = tts.caps().rates.iter().collect();
    assert_eq!(rates, [RATE]);
    let error = tts.open(TtsConfig::new(Rate::Hz8000)).await.unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent, "{error}");
    // Nothing was asked of the provider for it.
    assert_eq!(server.connections().len(), 0);
}

// ---------------------------------------------------------------------
// Against the provider
// ---------------------------------------------------------------------

fn live() -> ElevenLabsTts {
    let key = Secret::from_env("ELEVENLABS_API_KEY").unwrap();
    let voice = std::env::var("ELEVENLABS_VOICE").unwrap_or_else(|_| VOICE.to_owned());
    ElevenLabsTts::new(Config::new(MODEL, &voice, Some(key))).unwrap()
}

const SENTENCES: [&str; 2] = ["It is ten past four.", " Is there anything else I can do?"];

#[tokio::test]
#[ignore = "talks to the provider; needs ELEVENLABS_API_KEY"]
async fn live_two_sentences_are_spoken_as_the_port_says() {
    let tts = live();
    tts.warm().await.unwrap();
    let seen = tts_utterance(&tts, Rate::Hz24000, &SENTENCES)
        .await
        .unwrap();
    // Eleven words at the pace of speech.
    let millis = seen.length.as_millis();
    assert!((1_500..9_000).contains(&millis), "{millis} ms of audio");
    // A mark for nearly every character, and the last one at the end of
    // the text.
    let all: usize = SENTENCES.iter().map(|sentence| sentence.len()).sum();
    assert!(seen.marks.len() >= all * 3 / 4, "{:?}", seen.marks);
    let last = seen.marks.last().unwrap();
    assert_eq!(usize::try_from(last.text_end).unwrap(), all);
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
    let again = tts_utterance(&tts, Rate::Hz24000, &SENTENCES)
        .await
        .unwrap();
    assert!((1_500..9_000).contains(&again.length.as_millis()));
}

#[tokio::test]
#[ignore = "talks to the provider; needs ELEVENLABS_API_KEY"]
async fn live_a_response_that_is_abandoned_leaves_the_next_one_whole() {
    let seen = tts_abandoned(&live(), Rate::Hz24000, &SENTENCES)
        .await
        .unwrap();
    assert!(seen.marks.len() >= 30, "{:?}", seen.marks);
}

#[tokio::test]
#[ignore = "talks to the provider; needs no key"]
async fn live_a_wrong_key_is_refused() {
    let key = Some(Secret::new("sk-not-a-key"));
    let tts = ElevenLabsTts::new(Config::new(MODEL, VOICE, key)).unwrap();
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
