//! What the provider says, and what it means for a stream.
//!
//! Nothing here touches a connection: messages come in as text and
//! transcripts go out, so that every rule can be tried without one.

use core::time::Duration;

use bytes::{Bytes, BytesMut};
use serde::Deserialize;
use tokio::time::Instant;
use waveport_adapter_kit::timeline::Timeline;
use waveport_domain::audio::Rate;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
use waveport_ports::error::PortError;
use waveport_ports::stt::{SttEvent, Transcript};

/// Asks the provider to end the turn it is writing down, now.
pub(crate) const FORCE_ENDPOINT: &str = "{\"type\":\"ForceEndpoint\"}";

/// Tells the provider that the session is over.
pub(crate) const TERMINATE: &str = "{\"type\":\"Terminate\"}";

/// How much of what the provider says of a failure is passed on.
const SAID_AT_MOST: usize = 300;

/// How much audio a message holds before it is sent while more is on its
/// way, in milliseconds. The provider asks for 50 to 100.
const GOOD_MS: u32 = 50;

/// The least audio the provider takes in a message, in milliseconds. It
/// ends a session that is sent less.
const LEAST_MS: u32 = 20;

/// The most audio that is put into a message here, in milliseconds. The
/// provider takes up to a second.
const MOST_MS: u32 = 500;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Message {
    Begin {
        #[serde(default)]
        configuration: Configuration,
    },
    Turn(Turn),
    Termination,
    Error {
        #[serde(default)]
        error_code: u16,
        #[serde(default)]
        error: String,
    },
    /// What is of no interest here: where speech begins, and the
    /// provider's notes on how the session is doing.
    #[serde(other)]
    Other,
}

/// A message, as far as saying what kind it is.
#[derive(Debug, Deserialize)]
struct Kind {
    #[serde(rename = "type")]
    kind: String,
}

/// What the provider set a session up with, which need not be what it
/// was asked for.
#[derive(Debug, Default, Deserialize)]
struct Configuration {
    #[serde(default)]
    model: String,
}

#[derive(Debug, Deserialize)]
struct Turn {
    /// Which turn of the session this is about. Every message about a
    /// turn replaces the one before it.
    #[serde(rename = "turn_order")]
    order: u64,
    /// Whether it is written out properly, with capitals and punctuation.
    #[serde(rename = "turn_is_formatted", default)]
    formatted: bool,
    /// Whether the provider takes the turn to be over.
    #[serde(rename = "end_of_turn")]
    over: bool,
    #[serde(default)]
    transcript: String,
    #[serde(default)]
    words: Vec<Word>,
}

#[derive(Debug, Deserialize)]
struct Word {
    /// Where the word begins, in milliseconds of the audio that was sent.
    start: f64,
    end: f64,
}

/// A time the provider gives, in milliseconds. One that is no time at all
/// is taken as the beginning.
fn millis(time: f64) -> Duration {
    Duration::try_from_secs_f64(time / 1_000.0).unwrap_or_default()
}

/// Reads a message, or says why it cannot be read. `None` is a message of
/// a kind that is not known: the provider adds to what it says without
/// notice.
fn read(text: &str) -> Result<Option<Message>, PortError> {
    match serde_json::from_str(text) {
        Ok(Message::Other) => Ok(None),
        Ok(message) => Ok(Some(message)),
        Err(error) => match serde_json::from_str::<Kind>(text) {
            // A transcript that cannot be read is another matter. Passed
            // over, the session would be deaf and nobody would know why.
            Ok(Kind { kind }) if ["Begin", "Turn", "Error"].contains(&kind.as_str()) => {
                Err(PortError::permanent(format!(
                    "what the provider said ({kind}) cannot be read: {error}"
                )))
            }
            _ => Ok(None),
        },
    }
}

/// What a failure the provider tells of means for whoever might try
/// again. The provider closes the connection with the same code.
fn failure(code: u16, said: &str) -> PortError {
    let said: String = said.chars().take(SAID_AT_MOST).collect();
    let message = format!("the provider said ({code}): {said}");
    match code {
        // One code for everything that is not allowed. Too many sessions
        // begun in a minute is the one thing among them that passes.
        1008 if said.contains("Too many concurrent sessions") => {
            PortError::rate_limited(None, message)
        }
        1008 => PortError::auth(message),
        // What was sent was wrong, and sent again would be wrong again.
        3006 | 3007 => PortError::permanent(message),
        // The provider failed (1011, 3005), or the session reached the
        // three hours one may last (3008): a new one is worth a try.
        _ => PortError::transient(message),
    }
}

/// Takes the first message of a connection, which says that a session
/// has begun and what with.
///
/// # Errors
///
/// The provider refuses the session, or began one with another model
/// than `model`: it passes over what it does not know in the address, so
/// that a name written wrongly gets the model it would have chosen
/// itself.
pub(crate) fn begun(text: &str, model: &str) -> Result<(), PortError> {
    match read(text)? {
        Some(Message::Begin { configuration }) => {
            if configuration.model.is_empty() || configuration.model == model {
                Ok(())
            } else {
                let got: String = configuration.model.chars().take(SAID_AT_MOST).collect();
                Err(PortError::permanent(format!(
                    "the provider began a session with the model {got:?}, and was asked for \
                     {model:?}"
                )))
            }
        }
        Some(Message::Error { error_code, error }) => Err(failure(error_code, &error)),
        _ => Err(PortError::permanent(
            "the provider did not begin a session with its first message",
        )),
    }
}

/// Audio on its way to the provider, which takes it only in messages of
/// a size it likes.
#[derive(Debug)]
pub(crate) struct Outbox {
    waiting: BytesMut,
    good: usize,
    least: usize,
    most: usize,
}

impl Outbox {
    /// For audio at `rate`, of two bytes a sample.
    pub(crate) fn new(rate: Rate) -> Self {
        let bytes_in = |ms| rate.samples_in_millis(ms) * 2;
        Self {
            waiting: BytesMut::new(),
            good: bytes_in(GOOD_MS),
            least: bytes_in(LEAST_MS),
            most: bytes_in(MOST_MS),
        }
    }

    /// Takes a piece of audio.
    pub(crate) fn put(&mut self, samples: &[u8]) {
        self.waiting.extend_from_slice(samples);
    }

    /// The next message to send, if there is one. While more audio is on
    /// its way a message waits until it is of a good size. With `now`,
    /// everything that the provider would take is sent: what is then
    /// left is less than it takes, and goes with the audio that follows.
    pub(crate) fn take(&mut self, now: bool) -> Option<Bytes> {
        let enough = if now { self.least } else { self.good };
        if self.waiting.len() < enough {
            return None;
        }
        let size = self.waiting.len().min(self.most);
        Some(self.waiting.split_to(size).freeze())
    }
}

/// A final transcript that was asked for and has not come.
#[derive(Clone, Copy, Debug)]
struct Owed {
    since: Instant,
    /// How much audio had been sent when it was asked for.
    upto: Duration,
}

/// What one stream has sent and heard.
#[derive(Debug)]
pub(crate) struct Hearing {
    timeline: Timeline,
    owed: Option<Owed>,
    /// Where the last final transcript began: none begins before it.
    last: MediaTime,
    /// The last turn that was given as final. Nothing more is taken
    /// about it or about one before it.
    settled: Option<u64>,
    /// Whether the provider was asked to write turns out properly, which
    /// some of its models do in a second message at the end of a turn.
    formats: bool,
}

impl Hearing {
    pub(crate) fn new(formats: bool) -> Self {
        Self {
            timeline: Timeline::new(),
            owed: None,
            last: MediaTime::ZERO,
            settled: None,
            formats,
        }
    }

    /// Notes a piece of audio that is sent.
    pub(crate) fn sent(&mut self, at: MediaTime, length: MediaDur) {
        self.timeline.sent(at, length);
    }

    /// Takes a request for a final transcript of everything so far.
    /// Returns whether the provider has to be asked: of no audio at all
    /// there is nothing to write down, and that is said at once.
    pub(crate) fn finalize(&mut self, now: Instant, out: &mut Vec<SttEvent>) -> bool {
        let upto = self.timeline.total();
        if upto.is_zero() {
            self.nothing_more(upto, out);
            return false;
        }
        // A request that is still owed keeps its place in time: the
        // provider has had since then to answer.
        let since = self.owed.map_or(now, |owed| owed.since);
        self.owed = Some(Owed { since, upto });
        true
    }

    /// When the request that is still owed an answer was made.
    pub(crate) fn waiting_since(&self) -> Option<Instant> {
        self.owed.map(|owed| owed.since)
    }

    /// Answers the request that is owed an answer with nothing. The
    /// provider ends a turn it is writing down when it is asked to; of
    /// silence it is writing none, and says nothing.
    pub(crate) fn give_up(&mut self, out: &mut Vec<SttEvent>) {
        if let Some(owed) = self.owed.take() {
            self.nothing_more(owed.upto, out);
        }
    }

    /// A final transcript with no words, at the place `upto` into the
    /// audio: everything before it has been heard.
    fn nothing_more(&mut self, upto: Duration, out: &mut Vec<SttEvent>) {
        let at = self.place(upto).max(self.last);
        self.last = at;
        out.push(SttEvent::Transcript(Transcript {
            span: MediaSpan::new(at, at),
            text: String::new(),
            is_final: true,
        }));
    }

    /// The place on the media clock of the provider's time `offset`. The
    /// provider cannot have heard what it was not sent.
    fn place(&self, offset: Duration) -> MediaTime {
        self.timeline.at(offset).min(self.timeline.end())
    }

    /// Takes a message of the provider. Returns whether it was the last
    /// one of the session.
    ///
    /// # Errors
    ///
    /// The provider says that something went wrong, or says something
    /// about a turn that cannot be read.
    pub(crate) fn heard(&mut self, text: &str, out: &mut Vec<SttEvent>) -> Result<bool, PortError> {
        let turn = match read(text)? {
            Some(Message::Turn(turn)) => turn,
            Some(Message::Termination) => return Ok(true),
            Some(Message::Error { error_code, error }) => return Err(failure(error_code, &error)),
            Some(Message::Begin { .. } | Message::Other) | None => return Ok(false),
        };
        // A turn that has been given is not given again, in other words
        // or the same.
        if self.settled.is_some_and(|settled| turn.order <= settled) {
            return Ok(false);
        }
        let text = turn.transcript.trim().to_owned();
        // Where the words are, when the provider says; otherwise the end
        // of what it was sent.
        let (first, last) = match (turn.words.first(), turn.words.last()) {
            (Some(first), Some(last)) => (millis(first.start), millis(last.end)),
            _ => (self.timeline.total(), self.timeline.total()),
        };
        let start = self.place(first).max(self.last);
        let span = MediaSpan::new(start, self.place(last).max(start));

        // A model that writes a turn out properly at its end says twice
        // that the turn is over. The second time is the one that counts.
        let over = turn.over && (turn.formatted || !self.formats);
        if !over {
            if !text.is_empty() {
                out.push(SttEvent::Transcript(Transcript {
                    span,
                    text,
                    is_final: false,
                }));
            }
            return Ok(false);
        }
        self.settled = Some(turn.order);
        if !text.is_empty() {
            self.last = span.start;
            out.push(SttEvent::Transcript(Transcript {
                span,
                text,
                is_final: true,
            }));
        }
        if let Some(owed) = self.owed.take() {
            // Words that end where the speaker stopped say by themselves
            // that all of it was heard. Silence has to be said.
            if out.last().is_none_or(|event| {
                let SttEvent::Transcript(last) = event;
                !last.is_final || last.span.end < self.place(owed.upto)
            }) {
                self.nothing_more(owed.upto, out);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use tokio::time::Instant;
    use waveport_domain::audio::Rate;
    use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
    use waveport_ports::error::RetryClass;
    use waveport_ports::stt::{SttEvent, Transcript};

    use super::{Hearing, Outbox, begun};

    /// A stream that has been sent `ms` of audio, which began a second
    /// into the session.
    fn sent(ms: u32) -> Hearing {
        let mut hearing = Hearing::new(false);
        let mut at = MediaTime::from_millis(1_000);
        for _ in 0..ms / 20 {
            let length = MediaDur::from_millis(20);
            hearing.sent(at, length);
            at = at.after(length);
        }
        hearing
    }

    /// What the provider says about a turn. `words` are where the first
    /// word begins and the last one ends, in milliseconds.
    fn turn(order: u64, flags: &str, transcript: &str, words: Option<(u32, u32)>) -> String {
        let words = words.map_or_else(String::new, |(start, end)| {
            let middle = start.midpoint(end);
            format!(
                "{{\"start\":{start},\"end\":{middle},\"text\":\"a\",\"confidence\":0.9,\
                 \"word_is_final\":true}},{{\"start\":{middle},\"end\":{end},\"text\":\"b\",\
                 \"confidence\":0.9,\"word_is_final\":false}}"
            )
        });
        format!(
            "{{\"turn_order\":{order},{flags}\"transcript\":\"{transcript}\",\
             \"end_of_turn_confidence\":0.5,\"words\":[{words}],\"utterance\":\"\",\
             \"type\":\"Turn\"}}"
        )
    }

    const PARTIAL: &str = "\"turn_is_formatted\":false,\"end_of_turn\":false,";
    const UNFORMATTED: &str = "\"turn_is_formatted\":false,\"end_of_turn\":true,";
    const OVER: &str = "\"turn_is_formatted\":true,\"end_of_turn\":true,";

    fn transcript(from_ms: u64, to_ms: u64, text: &str, is_final: bool) -> SttEvent {
        SttEvent::Transcript(Transcript {
            span: MediaSpan::new(
                MediaTime::from_millis(from_ms),
                MediaTime::from_millis(to_ms),
            ),
            text: text.to_owned(),
            is_final,
        })
    }

    #[test]
    fn words_are_placed_where_they_were_spoken_on_the_sessions_clock() {
        let mut hearing = sent(3_000);
        let mut out = Vec::new();
        let said = [
            "{\"type\":\"SpeechStarted\",\"timestamp\":400,\"confidence\":0.98}".to_owned(),
            // Each of these is the whole turn so far, and replaces the
            // one before it.
            turn(0, PARTIAL, "what", Some((400, 600))),
            turn(0, PARTIAL, "what time is", Some((400, 1_050))),
            turn(0, OVER, "What time is it?", Some((400, 1_300))),
        ];
        for text in said {
            assert!(!hearing.heard(&text, &mut out).unwrap());
        }
        // The audio began a second into the session.
        assert_eq!(
            out,
            [
                transcript(1_400, 1_600, "what", false),
                transcript(1_400, 2_050, "what time is", false),
                transcript(1_400, 2_300, "What time is it?", true),
            ]
        );
    }

    #[test]
    fn a_turn_that_is_over_is_given_once() {
        // A model that does not write turns out properly says once that a
        // turn is over.
        let mut hearing = sent(3_000);
        let mut out = Vec::new();
        for text in [
            turn(0, UNFORMATTED, "my name is sonny", Some((200, 900))),
            // Whatever follows about that turn is not taken.
            turn(0, OVER, "My name is Sonny.", Some((200, 900))),
            turn(0, PARTIAL, "my name", Some((200, 500))),
            turn(1, PARTIAL, "and", Some((1_500, 1_700))),
        ] {
            hearing.heard(&text, &mut out).unwrap();
        }
        assert_eq!(
            out,
            [
                transcript(1_200, 1_900, "my name is sonny", true),
                transcript(2_500, 2_700, "and", false),
            ]
        );

        // Asked to write them out properly, it says it twice, and the
        // second time is the one to keep.
        let mut hearing = sent(3_000);
        hearing.formats = true;
        let mut out = Vec::new();
        for text in [
            turn(0, UNFORMATTED, "my name is sonny", Some((200, 900))),
            turn(0, OVER, "My name is Sonny.", Some((200, 900))),
        ] {
            hearing.heard(&text, &mut out).unwrap();
        }
        assert_eq!(
            out,
            [
                transcript(1_200, 1_900, "my name is sonny", false),
                transcript(1_200, 1_900, "My name is Sonny.", true),
            ]
        );
    }

    #[test]
    fn a_request_is_answered_by_the_turn_that_ends() {
        let mut hearing = sent(2_000);
        let mut out = Vec::new();
        let now = Instant::now();
        assert!(hearing.finalize(now, &mut out));
        assert_eq!(hearing.waiting_since(), Some(now));
        // What is not final is no answer.
        hearing
            .heard(&turn(0, PARTIAL, "tell me", Some((500, 900))), &mut out)
            .unwrap();
        assert!(hearing.waiting_since().is_some());
        hearing
            .heard(
                &turn(0, OVER, "Tell me a story.", Some((500, 1_800))),
                &mut out,
            )
            .unwrap();
        assert_eq!(hearing.waiting_since(), None);
        assert_eq!(
            out,
            [
                transcript(1_500, 1_900, "tell me", false),
                transcript(1_500, 2_800, "Tell me a story.", true),
                // The words stop before the audio does: that all of it
                // was heard is said apart.
                transcript(3_000, 3_000, "", true),
            ]
        );

        // Words that reach the end of the audio say it themselves.
        let mut hearing = sent(2_000);
        let mut out = Vec::new();
        assert!(hearing.finalize(now, &mut out));
        hearing
            .heard(&turn(0, OVER, "Go on.", Some((1_500, 2_000))), &mut out)
            .unwrap();
        assert_eq!(out, [transcript(2_500, 3_000, "Go on.", true)]);
    }

    #[test]
    fn a_turn_that_ends_with_nothing_said_answers_a_request_and_nothing_else() {
        let mut hearing = sent(2_000);
        let mut out = Vec::new();
        // Nobody asked: silence is not passed on.
        hearing
            .heard(&turn(0, UNFORMATTED, "", None), &mut out)
            .unwrap();
        assert_eq!(out, []);
        assert!(hearing.finalize(Instant::now(), &mut out));
        hearing
            .heard(&turn(1, UNFORMATTED, "", None), &mut out)
            .unwrap();
        assert_eq!(out, [transcript(3_000, 3_000, "", true)]);
        assert_eq!(hearing.waiting_since(), None);
    }

    #[test]
    fn a_request_the_provider_never_answers_is_answered_with_nothing() {
        // Of no audio at all there is nothing to ask the provider.
        let mut hearing = Hearing::new(false);
        let mut out = Vec::new();
        assert!(!hearing.finalize(Instant::now(), &mut out));
        assert_eq!(out, [transcript(0, 0, "", true)]);

        let mut hearing = sent(2_000);
        let mut out = Vec::new();
        let asked = Instant::now();
        assert!(hearing.finalize(asked, &mut out));
        // Asked again while the first is owed: the provider has had since
        // the first to answer.
        hearing.sent(MediaTime::from_millis(3_000), MediaDur::from_millis(500));
        assert!(hearing.finalize(asked + Duration::from_millis(300), &mut out));
        assert_eq!(hearing.waiting_since(), Some(asked));
        hearing.give_up(&mut out);
        assert_eq!(out, [transcript(3_500, 3_500, "", true)]);
        assert_eq!(hearing.waiting_since(), None);
        // Giving up twice gives up once.
        hearing.give_up(&mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn nothing_is_placed_outside_the_audio_or_before_what_came_before() {
        let mut hearing = sent(1_000);
        let mut out = Vec::new();
        for text in [
            // Words the provider puts after the end of what it was sent.
            turn(0, OVER, "well", Some((900, 1_400))),
            // And then words it puts before those.
            turn(1, OVER, "oh", Some((100, 300))),
            // Times that are no times.
            "{\"type\":\"Turn\",\"turn_order\":2,\"end_of_turn\":false,\"transcript\":\"hm\",\
             \"words\":[{\"start\":-3000,\"end\":-2000.5,\"text\":\"hm\"}]}"
                .to_owned(),
            // Words without their times are at the end of the audio.
            turn(3, OVER, "so", None),
        ] {
            hearing.heard(&text, &mut out).unwrap();
        }
        assert_eq!(
            out,
            [
                transcript(1_900, 2_000, "well", true),
                transcript(1_900, 1_900, "oh", true),
                transcript(1_900, 1_900, "hm", false),
                transcript(2_000, 2_000, "so", true),
            ]
        );
    }

    #[test]
    fn what_is_not_about_words_is_passed_over() {
        let mut hearing = sent(1_000);
        let mut out = Vec::new();
        for text in [
            "{\"type\":\"Begin\",\"id\":\"made-up\",\"expires_at\":1772570132,\
             \"configuration\":{\"model\":\"universal-3-6-pro\"}}",
            "{\"type\":\"SpeechStarted\",\"timestamp\":1216,\"confidence\":0.987654}",
            "{\"type\":\"Heartbeat\",\"total_audio_received_ms\":45000,\
             \"total_duration_ms\":45205,\"realtime_factor\":0.9964}",
            "{\"type\":\"SomethingNew\",\"with\":[1,2,3]}",
            "{\"no\":\"type\"}",
            "not JSON",
        ] {
            assert!(!hearing.heard(text, &mut out).unwrap(), "{text}");
        }
        assert_eq!(out, []);
        // The last message of a session is told apart.
        let last = "{\"type\":\"Termination\",\"audio_duration_seconds\":13,\
                    \"session_duration_seconds\":14}";
        assert!(hearing.heard(last, &mut out).unwrap());
    }

    #[test]
    fn a_turn_that_cannot_be_read_fails_the_stream() {
        let mut hearing = sent(1_000);
        let mut out = Vec::new();
        // It does not say which turn it is about.
        let text = "{\"type\":\"Turn\",\"end_of_turn\":true,\"transcript\":\"hello\"}";
        let error = hearing.heard(text, &mut out).unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
        assert!(error.message.contains("Turn"), "{error}");
        // What the provider said is not repeated: only what was wrong.
        assert!(!error.message.contains("hello"), "{error}");
        assert_eq!(out, []);
    }

    #[test]
    fn a_failure_is_worth_another_try_or_not_by_its_code() {
        let long = "x".repeat(5_000);
        for (code, said, class) in [
            (
                1008,
                "Unauthorized Connection: Invalid API key",
                RetryClass::Auth,
            ),
            (
                1008,
                "Unauthorized Connection: Insufficient funds",
                RetryClass::Auth,
            ),
            (
                1008,
                "Unauthorized Connection: Too many concurrent sessions",
                RetryClass::RateLimited,
            ),
            (
                1011,
                "Error authenticating connection",
                RetryClass::Transient,
            ),
            (
                3005,
                "Session Cancelled: An error occurred",
                RetryClass::Transient,
            ),
            (
                3006,
                "Invalid Message Type: Finalize",
                RetryClass::Permanent,
            ),
            (
                3007,
                "Input duration violation: 10 ms. Expected between 20 and 1000 ms",
                RetryClass::Permanent,
            ),
            (
                3008,
                "Session Expired: Maximum session duration exceeded",
                RetryClass::Transient,
            ),
            (3999, long.as_str(), RetryClass::Transient),
        ] {
            let text = format!("{{\"type\":\"Error\",\"error_code\":{code},\"error\":\"{said}\"}}");
            let mut hearing = sent(1_000);
            let error = hearing.heard(&text, &mut Vec::new()).unwrap_err();
            assert_eq!(error.class, class, "{error}");
            assert!(error.message.contains(&code.to_string()), "{error}");
            assert!(error.message.len() < 400);
            // The same when it is said in place of beginning a session.
            assert_eq!(begun(&text, "any").unwrap_err().class, class);
        }
    }

    #[test]
    fn a_session_begins_with_the_model_that_was_asked_for() {
        let begin = |model: &str| {
            format!(
                "{{\"type\":\"Begin\",\"id\":\"made-up\",\"expires_at\":1772570132,\
                 \"configuration\":{{\"model\":\"{model}\",\"mode\":\"balanced\",\
                 \"api_version\":\"2025-05-12\"}}}}"
            )
        };
        begun(&begin("universal-3-6-pro"), "universal-3-6-pro").unwrap();
        // A name the provider did not know got the model of its choice.
        let error = begun(&begin("universal-3-6-pro"), "universal-streaming-englsh").unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
        assert!(error.message.contains("universal-3-6-pro"), "{error}");
        assert!(
            error.message.contains("universal-streaming-englsh"),
            "{error}"
        );
        // A provider that does not say what it chose is believed.
        begun("{\"type\":\"Begin\",\"id\":\"made-up\"}", "any").unwrap();
        // Anything else is no beginning.
        for text in [
            "{\"type\":\"Termination\"}",
            "{\"type\":\"Heartbeat\"}",
            "not JSON",
        ] {
            let error = begun(text, "any").unwrap_err();
            assert_eq!(error.class, RetryClass::Permanent, "{text}");
        }
    }

    #[test]
    fn audio_goes_in_messages_of_a_size_the_provider_takes() {
        // At 16 kHz a millisecond is 32 bytes.
        let mut outbox = Outbox::new(Rate::Hz16000);
        let frame = [0_u8; 640];
        // Pieces of 20 ms wait until there are 50 ms of them.
        outbox.put(&frame);
        assert_eq!(outbox.take(false), None);
        outbox.put(&frame);
        assert_eq!(outbox.take(false), None);
        outbox.put(&frame);
        assert_eq!(outbox.take(false).map(|message| message.len()), Some(1_920));
        assert_eq!(outbox.take(false), None);

        // When it has to go now, whatever the provider takes goes.
        outbox.put(&frame);
        assert_eq!(outbox.take(true).map(|message| message.len()), Some(640));
        // Less than it takes waits for what follows.
        outbox.put(&[0; 320]);
        assert_eq!(outbox.take(true), None);
        outbox.put(&frame);
        assert_eq!(outbox.take(true).map(|message| message.len()), Some(960));

        // A long piece goes in several messages, none too long and none
        // too short.
        outbox.put(&vec![0; 32 * 1_210]);
        let sizes: Vec<usize> = core::iter::from_fn(|| outbox.take(false))
            .map(|message| message.len())
            .collect();
        assert_eq!(sizes, [16_000, 16_000, 6_720]);
        outbox.put(&vec![0; 32 * 510]);
        let sizes: Vec<usize> = core::iter::from_fn(|| outbox.take(true))
            .map(|message| message.len())
            .collect();
        // Ten milliseconds are left over, and wait.
        assert_eq!(sizes, [16_000]);
        assert_eq!(outbox.take(true), None);
    }
}
