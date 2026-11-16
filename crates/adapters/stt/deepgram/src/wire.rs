//! What the provider says, and what it means for a stream.
//!
//! Nothing here touches a connection: messages come in as text and
//! transcripts go out, so that every rule can be tried without one.

use core::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;
use waveport_adapter_kit::timeline::Timeline;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
use waveport_ports::error::PortError;
use waveport_ports::stt::{SttEvent, Transcript};

/// Asks the provider to write down what it has heard so far, now.
pub(crate) const FINALIZE: &str = "{\"type\":\"Finalize\"}";

/// Tells the provider that the connection is still wanted, while no audio
/// is sent on it.
pub(crate) const KEEP_ALIVE: &str = "{\"type\":\"KeepAlive\"}";

/// Tells the provider that no more audio follows.
pub(crate) const CLOSE_STREAM: &str = "{\"type\":\"CloseStream\"}";

/// How far short of the audio that was sent the provider's count of what
/// it has written down may stop, and still be taken to have reached it.
const NEAR_ENOUGH: Duration = Duration::from_millis(60);

/// How much of what the provider says of a failure is passed on.
const SAID_AT_MOST: usize = 300;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Message {
    Results(Results),
    Error {
        #[serde(default)]
        variant: String,
        #[serde(default)]
        description: String,
    },
    /// What is of no interest here: the summary at the end, and the
    /// provider's own notes on where speech begins and ends.
    #[serde(other)]
    Other,
}

/// A message, as far as saying what kind it is.
#[derive(Debug, Deserialize)]
struct Kind {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct Results {
    /// Where the audio this is about begins, in seconds of the audio that
    /// was sent.
    start: f64,
    /// How long that audio is.
    duration: f64,
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    from_finalize: bool,
    channel: Channel,
}

#[derive(Debug, Deserialize)]
struct Channel {
    #[serde(default)]
    alternatives: Vec<Alternative>,
}

#[derive(Debug, Deserialize)]
struct Alternative {
    #[serde(default)]
    transcript: String,
    #[serde(default)]
    words: Vec<Word>,
}

#[derive(Debug, Deserialize)]
struct Word {
    start: f64,
    end: f64,
}

/// A time the provider gives, in seconds. One that is no time at all is
/// taken as the beginning.
fn seconds(time: f64) -> Duration {
    Duration::try_from_secs_f64(time).unwrap_or_default()
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
    /// How much of the audio the provider has written down for good.
    covered: Duration,
    owed: Option<Owed>,
    /// Where the last final transcript began: none begins before it.
    last: MediaTime,
}

impl Hearing {
    pub(crate) fn new() -> Self {
        Self {
            timeline: Timeline::new(),
            covered: Duration::ZERO,
            owed: None,
            last: MediaTime::ZERO,
        }
    }

    /// Notes a piece of audio that is sent.
    pub(crate) fn sent(&mut self, at: MediaTime, length: MediaDur) {
        self.timeline.sent(at, length);
    }

    /// Takes a request for a final transcript of everything so far.
    /// Returns whether the provider has to be asked: when it has written
    /// down all it was sent, the answer is that there is nothing more,
    /// and it is given at once.
    pub(crate) fn finalize(&mut self, now: Instant, out: &mut Vec<SttEvent>) -> bool {
        let upto = self.timeline.total();
        if self.covered + NEAR_ENOUGH >= upto {
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

    /// Answers the request that is owed an answer with nothing: the
    /// provider does not promise one when it has little left to write
    /// down, and what it had it has given.
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

    /// Takes a message of the provider.
    ///
    /// # Errors
    ///
    /// The provider says that something went wrong.
    pub(crate) fn heard(&mut self, text: &str, out: &mut Vec<SttEvent>) -> Result<(), PortError> {
        let results = match serde_json::from_str(text) {
            Ok(Message::Results(results)) => results,
            Ok(Message::Error {
                variant,
                description,
            }) => {
                let said: String = description.chars().take(SAID_AT_MOST).collect();
                // The provider did not understand something it was sent.
                // Sent again it would be no clearer.
                return Err(PortError::permanent(format!(
                    "the provider said ({variant}): {said}"
                )));
            }
            // What is not known is passed over: the provider adds to what
            // it says without notice.
            Ok(Message::Other) => return Ok(()),
            Err(error) => {
                return match serde_json::from_str::<Kind>(text) {
                    // A transcript that cannot be read is another matter.
                    // Passed over, the session would be deaf and nobody
                    // would know why.
                    Ok(Kind { kind }) if kind == "Results" || kind == "Error" => {
                        Err(PortError::permanent(format!(
                            "what the provider said ({kind}) cannot be read: {error}"
                        )))
                    }
                    _ => Ok(()),
                };
            }
        };
        let from = seconds(results.start);
        let to = from + seconds(results.duration);
        let best = results.channel.alternatives.into_iter().next();
        let (text, words) = best.map_or_else(Default::default, |best| {
            (best.transcript.trim().to_owned(), best.words)
        });
        // Where the words are, when the provider says; otherwise the
        // audio they were heard in.
        let (first, last) = match (words.first(), words.last()) {
            (Some(first), Some(last)) => (seconds(first.start), seconds(last.end)),
            _ => (from, to),
        };
        let start = self.place(first).max(self.last);
        let span = MediaSpan::new(start, self.place(last).max(start));

        if !results.is_final {
            if !text.is_empty() {
                out.push(SttEvent::Transcript(Transcript {
                    span,
                    text,
                    is_final: false,
                }));
            }
            return Ok(());
        }
        self.covered = self.covered.max(to);
        let answers = self
            .owed
            .is_some_and(|owed| results.from_finalize || self.covered + NEAR_ENOUGH >= owed.upto);
        if !text.is_empty() {
            self.last = span.start;
            out.push(SttEvent::Transcript(Transcript {
                span,
                text,
                is_final: true,
            }));
        }
        if answers && let Some(owed) = self.owed.take() {
            // Words that end where the speaker stopped say by themselves
            // that all of it was heard. Silence has to be said.
            if out.last().is_none_or(|event| {
                let SttEvent::Transcript(last) = event;
                !last.is_final || last.span.end < self.place(owed.upto)
            }) {
                self.nothing_more(owed.upto, out);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use tokio::time::Instant;
    use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
    use waveport_ports::error::RetryClass;
    use waveport_ports::stt::{SttEvent, Transcript};

    use super::Hearing;

    /// A stream that has been sent `ms` of audio, which began a second
    /// into the session.
    fn sent(ms: u32) -> Hearing {
        let mut hearing = Hearing::new();
        let mut at = MediaTime::from_millis(1_000);
        for _ in 0..ms / 20 {
            let length = MediaDur::from_millis(20);
            hearing.sent(at, length);
            at = at.after(length);
        }
        hearing
    }

    fn results(start: f64, duration: f64, flags: &str, transcript: &str, words: &str) -> String {
        format!(
            "{{\"type\":\"Results\",\"channel_index\":[0,1],\"duration\":{duration},\
             \"start\":{start},{flags}\"channel\":{{\"alternatives\":[{{\"transcript\":\
             \"{transcript}\",\"confidence\":0.99,\"words\":[{words}]}}]}},\"metadata\":{{}}}}"
        )
    }

    fn word(word: &str, start: f64, end: f64) -> String {
        format!("{{\"word\":\"{word}\",\"start\":{start},\"end\":{end},\"confidence\":0.9}}")
    }

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
        // Not final yet: everything heard in the stretch so far.
        let words = [word("what", 0.4, 0.6), word("time", 0.62, 0.9)].join(",");
        let interim = results(0.0, 1.0, "\"is_final\":false,", "what time", &words);
        hearing.heard(&interim, &mut out).unwrap();
        // Final: the provider will not revise it.
        let words = [
            word("what", 0.4, 0.6),
            word("time", 0.62, 0.9),
            word("is", 0.95, 1.05),
            word("it", 1.1, 1.3),
        ]
        .join(",");
        let last = results(
            0.0,
            1.5,
            "\"is_final\":true,\"speech_final\":true,",
            "What time is it?",
            &words,
        );
        hearing.heard(&last, &mut out).unwrap();
        // The audio began a second into the session.
        assert_eq!(
            out,
            [
                transcript(1_400, 1_900, "what time", false),
                transcript(1_400, 2_300, "What time is it?", true),
            ]
        );
    }

    #[test]
    fn silence_is_not_passed_on_unless_it_was_asked_about() {
        let mut hearing = sent(2_000);
        let mut out = Vec::new();
        for flags in ["\"is_final\":false,", "\"is_final\":true,"] {
            hearing
                .heard(&results(0.0, 1.0, flags, "", ""), &mut out)
                .unwrap();
        }
        assert_eq!(out, []);
    }

    #[test]
    fn a_request_is_answered_by_the_words_that_end_the_audio() {
        let mut hearing = sent(2_000);
        let mut out = Vec::new();
        let now = Instant::now();
        assert!(hearing.finalize(now, &mut out));
        assert_eq!(hearing.waiting_since(), Some(now));
        // What the provider had written down before it was asked is not
        // the answer: it stops a second short.
        let early = results(
            0.0,
            1.0,
            "\"is_final\":true,",
            "tell me",
            &word("me", 0.5, 0.9),
        );
        hearing.heard(&early, &mut out).unwrap();
        assert!(hearing.waiting_since().is_some());
        let flags = "\"is_final\":true,\"from_finalize\":true,";
        let rest = results(1.0, 1.0, flags, "a story", &word("story", 1.2, 1.98));
        hearing.heard(&rest, &mut out).unwrap();
        assert_eq!(hearing.waiting_since(), None);
        assert_eq!(
            out,
            [
                transcript(1_500, 1_900, "tell me", true),
                transcript(2_200, 2_980, "a story", true),
                // The words stop a little before the audio does: that all
                // of it was heard is said apart.
                transcript(3_000, 3_000, "", true),
            ]
        );
    }

    #[test]
    fn a_request_is_answered_with_nothing_when_there_was_silence() {
        let mut hearing = sent(2_000);
        let mut out = Vec::new();
        assert!(hearing.finalize(Instant::now(), &mut out));
        let flags = "\"is_final\":true,\"from_finalize\":true,";
        hearing
            .heard(&results(0.0, 2.0, flags, "", ""), &mut out)
            .unwrap();
        assert_eq!(out, [transcript(3_000, 3_000, "", true)]);
        assert_eq!(hearing.waiting_since(), None);

        // The provider has now written down all it was sent: asked again,
        // it need not be, and the answer is given at once.
        out.clear();
        assert!(!hearing.finalize(Instant::now(), &mut out));
        assert_eq!(out, [transcript(3_000, 3_000, "", true)]);
    }

    #[test]
    fn a_request_is_answered_without_the_providers_word_for_it() {
        // The provider does not always say that a transcript is the
        // answer to a request. One that reaches the end of the audio is.
        let mut hearing = sent(2_000);
        let mut out = Vec::new();
        assert!(hearing.finalize(Instant::now(), &mut out));
        let all = results(
            0.0,
            1.96,
            "\"is_final\":true,",
            "go on",
            &word("on", 1.5, 1.96),
        );
        hearing.heard(&all, &mut out).unwrap();
        assert_eq!(hearing.waiting_since(), None);
        assert_eq!(
            out,
            [
                transcript(2_500, 2_960, "go on", true),
                transcript(3_000, 3_000, "", true),
            ]
        );
    }

    #[test]
    fn a_request_the_provider_never_answers_is_answered_with_nothing() {
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
        // Words the provider puts after the end of what it was sent.
        let late = results(
            0.5,
            0.9,
            "\"is_final\":true,",
            "well",
            &word("well", 0.9, 1.4),
        );
        hearing.heard(&late, &mut out).unwrap();
        // And then words it puts before those.
        let early = results(0.0, 0.5, "\"is_final\":true,", "oh", &word("oh", 0.1, 0.3));
        hearing.heard(&early, &mut out).unwrap();
        // Times that are no times.
        let odd = results(
            -1.0,
            0.5,
            "\"is_final\":false,",
            "hm",
            &word("hm", -3.0, -2.0),
        );
        hearing.heard(&odd, &mut out).unwrap();
        assert_eq!(
            out,
            [
                transcript(1_900, 2_000, "well", true),
                transcript(1_900, 1_900, "oh", true),
                transcript(1_900, 1_900, "hm", false),
            ]
        );
    }

    #[test]
    fn what_is_not_about_words_is_passed_over() {
        let mut hearing = sent(1_000);
        let mut out = Vec::new();
        for text in [
            "{\"type\":\"Metadata\",\"transaction_key\":\"deprecated\",\"request_id\":\
             \"8c8ebea9-dbec-45fa-a035-e4632cb05b5f\",\"sha256\":\"e3b0\",\"created\":\
             \"2026-11-15T22:37:55.202Z\",\"duration\":0.0,\"channels\":0}",
            "{\"type\":\"UtteranceEnd\",\"channel\":[0,1],\"last_word_end\":2.4}",
            "{\"type\":\"SpeechStarted\",\"channel\":[0,1],\"timestamp\":0.2}",
            "{\"type\":\"SomethingNew\",\"with\":[1,2,3]}",
            "{\"no\":\"type\"}",
            "not JSON",
            // A transcript with no alternative in it says nothing.
            "{\"type\":\"Results\",\"start\":0.0,\"duration\":0.5,\"is_final\":false,\
             \"channel\":{\"alternatives\":[]}}",
        ] {
            hearing.heard(text, &mut out).unwrap();
        }
        assert_eq!(out, []);
    }

    #[test]
    fn a_transcript_that_cannot_be_read_fails_the_stream() {
        let mut hearing = sent(1_000);
        let mut out = Vec::new();
        // The times are not where they are said to be.
        let text = "{\"type\":\"Results\",\"is_final\":true,\"channel\":\
                    {\"alternatives\":[{\"transcript\":\"hello\"}]}}";
        let error = hearing.heard(text, &mut out).unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
        assert!(error.message.contains("Results"), "{error}");
        // What the provider said is not repeated: only what was wrong.
        assert!(!error.message.contains("hello"), "{error}");
        assert_eq!(out, []);
    }

    #[test]
    fn what_the_provider_did_not_understand_fails_the_stream() {
        let mut hearing = sent(1_000);
        let mut out = Vec::new();
        let long = "x".repeat(5_000);
        let text = format!(
            "{{\"type\":\"Error\",\"variant\":\"SchemaError\",\"description\":\
             \"unknown variant, {long}\",\"message\":\"{{}}\"}}"
        );
        let error = hearing.heard(&text, &mut out).unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
        assert!(error.message.contains("SchemaError"), "{error}");
        assert!(error.message.len() < 400);
    }
}
