//! What is sent and what comes back, with no connection in sight.
//!
//! The provider calls a stretch of audio an item, and gives it a name
//! when it first has something to say about it. Everything it says about
//! the words in that audio carries the name. [`Hearing`] keeps the other
//! side of the bargain: which audio each name stands for, on the
//! session's clock, and in what order the answers are owed.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use waveport_adapter_kit::secret::Secret;
use waveport_adapter_kit::timeline::Timeline;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
use waveport_ports::error::{PortError, RetryClass};
use waveport_ports::stt::{SttEvent, Transcript};

use crate::Config;

/// The rate the provider takes audio at. It takes no other.
pub(crate) const HZ: u32 = 24_000;

/// The least audio the provider will make an item of: a tenth of a
/// second, in ticks of the media clock.
const LEAST: u64 = 4_800;

/// How many names of items that are done with are remembered, so that
/// something said late about one of them is not taken to be about new
/// audio.
const REMEMBERED: usize = 8;

/// How much of what the provider says about a failure is passed on.
const SAID_AT_MOST: usize = 300;

// ---------------------------------------------------------------------
// What is sent
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Update<'a> {
    r#type: &'static str,
    session: Session<'a>,
}

#[derive(Debug, Serialize)]
struct Session<'a> {
    r#type: &'static str,
    audio: Audio<'a>,
}

#[derive(Debug, Serialize)]
struct Audio<'a> {
    input: Input<'a>,
}

#[derive(Debug, Serialize)]
struct Input<'a> {
    format: Format,
    transcription: Transcription<'a>,
    /// Always there and always `null`: left out, the provider decides for
    /// itself where a speaker has stopped, and cuts the audio there.
    turn_detection: Option<()>,
}

#[derive(Debug, Serialize)]
struct Format {
    r#type: &'static str,
    rate: u32,
}

#[derive(Debug, Serialize)]
struct Transcription<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    languages: Option<[String; 1]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay: Option<&'a str>,
}

/// The message that sets a session up: transcription only, plain samples
/// at the one rate there is, and no judgement by the provider of where a
/// turn ends.
pub(crate) fn setup(config: &Config, language: Option<&str>) -> Result<String, PortError> {
    let update = Update {
        r#type: "session.update",
        session: Session {
            r#type: "transcription",
            audio: Audio {
                input: Input {
                    format: Format {
                        r#type: "audio/pcm",
                        rate: HZ,
                    },
                    transcription: Transcription {
                        model: &config.model,
                        languages: language.map(|tag| [language_code(tag)]),
                        prompt: config.prompt.as_deref(),
                        delay: config.delay.as_deref(),
                    },
                    turn_detection: None,
                },
            },
        },
    };
    serde_json::to_string(&update)
        .map_err(|error| PortError::permanent(format!("the session cannot be asked for: {error}")))
}

/// The provider's code for a language, from its tag: the language alone,
/// in lower case, without the country or the script. Chinese is the
/// exception: the provider tells three kinds of it apart, by region.
fn language_code(tag: &str) -> String {
    let tag = tag.trim().replace('_', "-").to_ascii_lowercase();
    if matches!(tag.as_str(), "zh-cn" | "zh-tw" | "zh-hk") {
        return tag;
    }
    tag.split('-').next().unwrap_or_default().to_owned()
}

/// The start of a message that hands audio over. The audio follows, in
/// base64, and then [`APPEND_END`].
pub(crate) const APPEND: &str = "{\"type\":\"input_audio_buffer.append\",\"audio\":\"";

/// The end of a message that hands audio over.
pub(crate) const APPEND_END: &str = "\"}";

// ---------------------------------------------------------------------
// What comes back
// ---------------------------------------------------------------------

/// A message of the provider, as far as this adapter reads it.
#[derive(Debug, Default, Deserialize)]
struct Incoming {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    transcript: Option<String>,
    #[serde(default)]
    error: Option<Problem>,
}

/// What the provider says went wrong.
#[derive(Debug, Default, Deserialize)]
struct Problem {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// The name of the message of ours that it is about.
    #[serde(default)]
    event_id: Option<String>,
}

impl Problem {
    fn code(&self) -> &str {
        self.code.as_deref().unwrap_or_default()
    }

    /// The failure this is, in the classes the ports use. The provider
    /// repeats the key it was given when it does not know it, so the key
    /// is struck from what it says.
    fn into_error(self, key: Option<&Secret>) -> PortError {
        let kind = self.r#type.as_deref().unwrap_or_default();
        let code = self.code();
        let class = if code == "session_expired" {
            // A session lasts an hour. The next one is as good.
            RetryClass::Transient
        } else if code == "invalid_api_key" || kind == "authentication_error" {
            RetryClass::Auth
        } else if code.starts_with("rate_limit") || kind.starts_with("rate_limit") {
            RetryClass::RateLimited
        } else if kind == "invalid_request_error" {
            RetryClass::Permanent
        } else {
            RetryClass::Transient
        };
        let said = self.message.as_deref().unwrap_or("nothing more");
        let said = key.map_or_else(|| said.to_owned(), |key| key.strike(said));
        let said: String = said.chars().take(SAID_AT_MOST).collect();
        let code = if code.is_empty() { kind } else { code };
        PortError::new(class, format!("the provider said ({code}): {said}"))
    }
}

/// What a message that arrives while a session is set up means for it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Setup {
    /// The session is as it was asked for.
    Ready,
    /// The message is about something else.
    NotYet,
    /// The provider will not have it.
    Refused(PortError),
}

/// Reads a message that arrives while a session is set up.
pub(crate) fn setup_answer(message: &str, key: Option<&Secret>) -> Setup {
    let Ok(incoming) = serde_json::from_str::<Incoming>(message) else {
        return Setup::NotYet;
    };
    match incoming.r#type.as_str() {
        "session.updated" => Setup::Ready,
        "error" => Setup::Refused(incoming.error.unwrap_or_default().into_error(key)),
        _ => Setup::NotYet,
    }
}

// ---------------------------------------------------------------------
// Which audio the words are of
// ---------------------------------------------------------------------

/// A request for a final transcript that has not been answered.
#[derive(Debug)]
struct Asked {
    /// The audio it is about.
    window: MediaSpan,
    /// The number the request was sent to the provider under. `None` for
    /// one that was not sent, there being too little audio to send it
    /// for.
    commit: Option<u64>,
    /// The provider's name for the audio, once it is known.
    item: Option<String>,
    /// What the provider has written of the audio so far.
    text: String,
    /// The final transcript, once it is there.
    done: Option<String>,
    /// When it was asked.
    at: Instant,
}

/// The audio sent on one stream, and what the provider owes for it.
#[derive(Debug)]
pub(crate) struct Hearing {
    timeline: Timeline,
    /// How much audio the provider holds that no request has been made
    /// for, in ticks.
    held: u64,
    /// The provider's name for that audio, once it has said something
    /// about it, and what it has written of it.
    item: Option<String>,
    text: String,
    /// The requests that are not answered, oldest first. They are
    /// answered in this order, whatever order the provider answers in.
    asked: VecDeque<Asked>,
    /// How many requests were sent to the provider.
    commits: u64,
    /// The names of the items that were last done with.
    over: VecDeque<String>,
    key: Option<Secret>,
}

impl Hearing {
    pub(crate) fn new(key: Option<Secret>) -> Self {
        Self {
            timeline: Timeline::new(),
            held: 0,
            item: None,
            text: String::new(),
            asked: VecDeque::new(),
            commits: 0,
            over: VecDeque::with_capacity(REMEMBERED),
            key,
        }
    }

    /// Notes a piece of audio that is handed to the provider.
    pub(crate) fn sent(&mut self, at: MediaTime, length: MediaDur) {
        self.timeline.sent(at, length);
        self.held += u64::from(length.ticks());
    }

    /// A final transcript of the audio sent so far is asked for. Returns
    /// the message to send to the provider, when there is one to send,
    /// and adds to `out` what can be said at once.
    ///
    /// The provider refuses a request for less than a tenth of a second
    /// of audio. Such a request is answered here, with no words: nobody
    /// says anything in so short a time. The audio stays with the
    /// provider and is heard with what follows it.
    pub(crate) fn finalize(&mut self, now: Instant, out: &mut Vec<SttEvent>) -> Option<String> {
        let window = self.timeline.take_window().unwrap_or_else(|| {
            let end = self.timeline.end();
            MediaSpan::new(end, end)
        });
        if self.held < LEAST {
            self.asked.push_back(Asked {
                window,
                commit: None,
                item: None,
                text: String::new(),
                done: Some(String::new()),
                at: now,
            });
            self.answer(out);
            return None;
        }
        self.held = 0;
        self.commits += 1;
        self.asked.push_back(Asked {
            window,
            commit: Some(self.commits),
            item: self.item.take(),
            text: core::mem::take(&mut self.text),
            done: None,
            at: now,
        });
        Some(format!(
            "{{\"type\":\"input_audio_buffer.commit\",\"event_id\":\"commit_{}\"}}",
            self.commits
        ))
    }

    /// When the oldest request that the provider has not answered was
    /// made.
    pub(crate) fn waiting_since(&self) -> Option<Instant> {
        self.asked
            .iter()
            .find(|asked| asked.done.is_none())
            .map(|asked| asked.at)
    }

    /// Takes a message of the provider, and adds to `out` what it means
    /// for whoever listens.
    ///
    /// # Errors
    ///
    /// The provider says that it has failed, or that it could not make
    /// words of an item. The stream is of no more use; another one may
    /// be.
    pub(crate) fn heard(
        &mut self,
        message: &str,
        out: &mut Vec<SttEvent>,
    ) -> Result<(), PortError> {
        // What cannot be read is something this adapter does not know
        // yet, and was not waiting for.
        let Ok(incoming) = serde_json::from_str::<Incoming>(message) else {
            return Ok(());
        };
        match incoming.r#type.as_str() {
            "conversation.item.input_audio_transcription.delta" => {
                if let (Some(item), Some(delta)) = (incoming.item_id, incoming.delta) {
                    self.written(&item, &delta, out);
                }
            }
            "input_audio_buffer.committed" => {
                // Its name is all this says, and looking for the request
                // gives the request its name.
                if let Some(item) = incoming.item_id {
                    self.asked_about(&item);
                }
            }
            "conversation.item.input_audio_transcription.completed" => {
                if let Some(item) = incoming.item_id
                    && let Some(asked) = self.asked_about(&item)
                {
                    asked.done = Some(incoming.transcript.unwrap_or_default());
                    self.answer(out);
                }
            }
            "conversation.item.input_audio_transcription.failed" => {
                let problem = incoming.error.unwrap_or_default();
                let said: String = problem
                    .message
                    .as_deref()
                    .unwrap_or("nothing more")
                    .chars()
                    .take(SAID_AT_MOST)
                    .collect();
                // The audio is still with whoever sent it, and another
                // stream can be given it again.
                return Err(PortError::transient(format!(
                    "the provider could not make words of what it heard: {said}"
                )));
            }
            "error" => {
                let problem = incoming.error.unwrap_or_default();
                if problem.code() != "input_audio_buffer_commit_empty" {
                    return Err(problem.into_error(self.key.as_ref()));
                }
                // By the provider's count there was too little audio for
                // the request. Then there were no words in it either.
                self.refused(problem.event_id.as_deref());
                self.answer(out);
            }
            _ => {}
        }
        Ok(())
    }

    /// The provider has written `delta` more of the audio it calls
    /// `item`.
    fn written(&mut self, item: &str, delta: &str, out: &mut Vec<SttEvent>) {
        if self.over.iter().any(|over| over == item) {
            return;
        }
        let (span, text) = if let Some(asked) = self.asked_about(item) {
            asked.text.push_str(delta);
            (Some(asked.window), asked.text.as_str())
        } else {
            // The audio no request has been made for.
            if self.item.as_deref() != Some(item) {
                self.item = Some(item.to_owned());
                self.text.clear();
            }
            self.text.push_str(delta);
            (self.timeline.window(), self.text.as_str())
        };
        let text = text.trim();
        if let Some(span) = span
            && !text.is_empty()
        {
            out.push(SttEvent::Transcript(Transcript {
                span,
                text: text.to_owned(),
                is_final: false,
            }));
        }
    }

    /// The request that is about the audio the provider calls `item`:
    /// the one known by that name, or else the oldest one sent to the
    /// provider that has no name yet, which is given it. The provider
    /// names its items in the order the requests were made in.
    fn asked_about(&mut self, item: &str) -> Option<&mut Asked> {
        if self.item.as_deref() == Some(item) || self.over.iter().any(|over| over == item) {
            return None;
        }
        let named = self
            .asked
            .iter()
            .position(|asked| asked.item.as_deref() == Some(item));
        let at = named.or_else(|| {
            self.asked
                .iter()
                .position(|asked| asked.commit.is_some() && asked.item.is_none())
        })?;
        let asked = self.asked.get_mut(at)?;
        if asked.item.is_none() {
            asked.item = Some(item.to_owned());
        }
        Some(asked)
    }

    /// The provider refused a request for want of audio: the one it
    /// names, or else the oldest it has said nothing about.
    fn refused(&mut self, named: Option<&str>) {
        let number = named
            .and_then(|name| name.strip_prefix("commit_"))
            .and_then(|number| number.parse::<u64>().ok());
        let asked = self.asked.iter_mut().find(|asked| {
            asked.done.is_none()
                && match number {
                    Some(number) => asked.commit == Some(number),
                    None => asked.commit.is_some() && asked.item.is_none(),
                }
        });
        if let Some(asked) = asked {
            asked.done = Some(String::new());
        }
    }

    /// Gives the final transcripts that are there, as far as they are
    /// there in the order they were asked for.
    fn answer(&mut self, out: &mut Vec<SttEvent>) {
        while let Some(asked) = self.asked.pop_front_if(|asked| asked.done.is_some()) {
            if let Some(item) = asked.item {
                if self.over.len() == REMEMBERED {
                    self.over.pop_front();
                }
                self.over.push_back(item);
            }
            out.push(SttEvent::Transcript(Transcript {
                span: asked.window,
                text: asked.done.unwrap_or_default().trim().to_owned(),
                is_final: true,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::Instant;
    use waveport_adapter_kit::secret::Secret;
    use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
    use waveport_ports::error::RetryClass;
    use waveport_ports::stt::{SttEvent, Transcript};

    use super::{Hearing, Setup, language_code, setup, setup_answer};
    use crate::Config;

    const fn ms(millis: u64) -> MediaTime {
        MediaTime::from_millis(millis)
    }

    fn span(from: u64, to: u64) -> MediaSpan {
        MediaSpan::new(ms(from), ms(to))
    }

    /// Hands over `count` pieces of 100 ms from `from` on.
    fn send(hearing: &mut Hearing, from: u64, count: u64) {
        for n in 0..count {
            hearing.sent(ms(from + 100 * n), MediaDur::from_millis(100));
        }
    }

    fn delta(item: &str, text: &str) -> String {
        format!(
            "{{\"type\":\"conversation.item.input_audio_transcription.delta\",\
             \"item_id\":\"{item}\",\"content_index\":0,\"delta\":\"{text}\"}}"
        )
    }

    fn committed(item: &str) -> String {
        format!("{{\"type\":\"input_audio_buffer.committed\",\"item_id\":\"{item}\"}}")
    }

    fn completed(item: &str, text: &str) -> String {
        format!(
            "{{\"type\":\"conversation.item.input_audio_transcription.completed\",\
             \"item_id\":\"{item}\",\"content_index\":0,\"transcript\":\"{text}\"}}"
        )
    }

    fn words(span: MediaSpan, text: &str, is_final: bool) -> SttEvent {
        SttEvent::Transcript(Transcript {
            span,
            text: text.to_owned(),
            is_final,
        })
    }

    /// Gives the hearing these messages and returns what came of them.
    fn hear(hearing: &mut Hearing, messages: &[String]) -> Vec<SttEvent> {
        let mut out = Vec::new();
        for message in messages {
            hearing.heard(message, &mut out).unwrap();
        }
        out
    }

    #[test]
    fn a_session_is_asked_for_with_the_providers_judgement_off() {
        let config = Config::new("gpt-live-transcribe", None);
        assert_eq!(
            setup(&config, None).unwrap(),
            "{\"type\":\"session.update\",\"session\":{\"type\":\"transcription\",\"audio\":\
             {\"input\":{\"format\":{\"type\":\"audio/pcm\",\"rate\":24000},\"transcription\":\
             {\"model\":\"gpt-live-transcribe\"},\"turn_detection\":null}}}}"
        );
        let config = Config {
            prompt: Some("A call to a clinic.".to_owned()),
            delay: Some("low".to_owned()),
            ..config
        };
        let asked = setup(&config, Some("es-PE")).unwrap();
        assert!(
            asked.contains(
                "\"transcription\":{\"model\":\"gpt-live-transcribe\",\"languages\":[\"es\"],\
                 \"prompt\":\"A call to a clinic.\",\"delay\":\"low\"}"
            ),
            "{asked}"
        );
    }

    #[test]
    fn a_language_is_asked_for_by_the_providers_code() {
        for (tag, code) in [
            ("en", "en"),
            ("en-US", "en"),
            ("es_PE", "es"),
            ("pt-BR", "pt"),
            ("zh-CN", "zh-cn"),
            ("zh-TW", "zh-tw"),
            ("zh-Hans", "zh"),
            (" de ", "de"),
        ] {
            assert_eq!(language_code(tag), code, "{tag}");
        }
    }

    #[test]
    fn a_session_is_ready_when_the_provider_says_it_is_as_asked() {
        let key = Secret::new("sk-not-a-key");
        assert_eq!(
            setup_answer("{\"type\":\"session.created\",\"session\":{}}", None),
            Setup::NotYet
        );
        assert_eq!(setup_answer("not json", None), Setup::NotYet);
        assert_eq!(
            setup_answer("{\"type\":\"session.updated\",\"session\":{}}", None),
            Setup::Ready
        );
        let refused = "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\
                       \"code\":\"invalid_api_key\",\"message\":\"Incorrect API key provided: \
                       sk-not-a-key. You can find your API key at the usual place.\"}}";
        let Setup::Refused(error) = setup_answer(refused, Some(&key)) else {
            panic!("not refused");
        };
        assert_eq!(error.class, RetryClass::Auth);
        assert!(error.message.contains("invalid_api_key"), "{error}");
        // The provider repeated the key. It goes no further.
        assert!(!error.message.contains("sk-not-a-key"), "{error}");
    }

    #[test]
    fn what_the_provider_says_went_wrong_is_sorted_into_classes() {
        for (kind, code, class) in [
            (
                "invalid_request_error",
                "invalid_value",
                RetryClass::Permanent,
            ),
            (
                "invalid_request_error",
                "session_expired",
                RetryClass::Transient,
            ),
            ("invalid_request_error", "invalid_api_key", RetryClass::Auth),
            ("authentication_error", "", RetryClass::Auth),
            ("rate_limit_error", "", RetryClass::RateLimited),
            (
                "invalid_request_error",
                "rate_limit_exceeded",
                RetryClass::RateLimited,
            ),
            ("server_error", "", RetryClass::Transient),
            ("", "", RetryClass::Transient),
        ] {
            let mut hearing = Hearing::new(None);
            let message = format!(
                "{{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"code\":\"{code}\",\
                 \"message\":\"it went wrong\"}}}}"
            );
            let error = hearing.heard(&message, &mut Vec::new()).unwrap_err();
            assert_eq!(error.class, class, "{kind} {code}");
            assert!(error.message.contains("it went wrong"), "{error}");
        }
    }

    #[test]
    fn words_are_of_the_audio_between_two_requests() {
        let mut hearing = Hearing::new(None);
        let now = Instant::now();
        send(&mut hearing, 1_000, 20);
        // What is written before the request is of the audio so far.
        let heard = hear(
            &mut hearing,
            &[delta("item_a", " What"), delta("item_a", " time")],
        );
        assert_eq!(
            heard,
            [
                words(span(1_000, 3_000), "What", false),
                words(span(1_000, 3_000), "What time", false),
            ]
        );
        send(&mut hearing, 3_000, 5);
        let mut out = Vec::new();
        let commit = hearing.finalize(now, &mut out).unwrap();
        assert_eq!(
            commit,
            "{\"type\":\"input_audio_buffer.commit\",\"event_id\":\"commit_1\"}"
        );
        assert_eq!(out, [], "nothing can be said yet");
        assert_eq!(hearing.waiting_since(), Some(now));

        // Audio goes on, and is of the next request.
        send(&mut hearing, 3_500, 10);
        let heard = hear(
            &mut hearing,
            &[
                // More of the first, after its request.
                delta("item_a", " is it?"),
                committed("item_a"),
                // And the first of the second.
                delta("item_b", " Hello"),
                completed("item_a", "What time is it?"),
            ],
        );
        assert_eq!(
            heard,
            [
                words(span(1_000, 3_500), "What time is it?", false),
                words(span(3_500, 4_500), "Hello", false),
                words(span(1_000, 3_500), "What time is it?", true),
            ]
        );
        assert_eq!(hearing.waiting_since(), None);

        // Something said late about an item that is done with is not
        // taken to be about new audio.
        assert_eq!(hear(&mut hearing, &[delta("item_a", " Late")]), []);
        let heard = hear(&mut hearing, &[delta("item_b", " there")]);
        assert_eq!(heard, [words(span(3_500, 4_500), "Hello there", false)]);
    }

    #[test]
    fn an_item_is_named_by_whatever_the_provider_says_of_it_first() {
        // The request first, and only then anything about the audio: so
        // it is when audio is sent faster than it is spoken.
        let mut hearing = Hearing::new(None);
        send(&mut hearing, 0, 20);
        let mut out = Vec::new();
        assert!(hearing.finalize(Instant::now(), &mut out).is_some());
        let heard = hear(
            &mut hearing,
            &[
                committed("item_a"),
                delta("item_a", " Yes"),
                completed("item_a", "Yes."),
            ],
        );
        assert_eq!(
            heard,
            [
                words(span(0, 2_000), "Yes", false),
                words(span(0, 2_000), "Yes.", true)
            ]
        );

        // Words before the provider has said that it has the request.
        let mut hearing = Hearing::new(None);
        send(&mut hearing, 0, 20);
        assert!(hearing.finalize(Instant::now(), &mut out).is_some());
        send(&mut hearing, 2_000, 10);
        let heard = hear(
            &mut hearing,
            &[delta("item_a", " No"), completed("item_a", "No.")],
        );
        assert_eq!(
            heard,
            [
                words(span(0, 2_000), "No", false),
                words(span(0, 2_000), "No.", true)
            ]
        );
    }

    #[test]
    fn final_transcripts_come_in_the_order_they_were_asked_for() {
        let mut hearing = Hearing::new(None);
        let mut out = Vec::new();
        let first = Instant::now();
        send(&mut hearing, 0, 10);
        assert!(hearing.finalize(first, &mut out).is_some());
        send(&mut hearing, 1_000, 10);
        let second = first + core::time::Duration::from_secs(1);
        assert!(hearing.finalize(second, &mut out).is_some());
        // Nothing follows the second request, so a third finds no audio,
        // and is answered here. Not before the two it follows, though.
        assert_eq!(hearing.finalize(second, &mut out), None);
        assert_eq!(out, []);

        // The provider answers the second first.
        let heard = hear(
            &mut hearing,
            &[
                committed("item_a"),
                committed("item_b"),
                completed("item_b", "Second."),
            ],
        );
        assert_eq!(heard, []);
        assert_eq!(hearing.waiting_since(), Some(first));
        let heard = hear(&mut hearing, &[completed("item_a", " First. ")]);
        assert_eq!(
            heard,
            [
                words(span(0, 1_000), "First.", true),
                words(span(1_000, 2_000), "Second.", true),
                words(span(2_000, 2_000), "", true),
            ]
        );
        assert_eq!(hearing.waiting_since(), None);
    }

    #[test]
    fn a_request_for_too_little_audio_is_answered_without_the_provider() {
        let mut hearing = Hearing::new(None);
        let mut out = Vec::new();
        // Nothing was sent at all.
        assert_eq!(hearing.finalize(Instant::now(), &mut out), None);
        assert_eq!(out, [words(span(0, 0), "", true)]);

        // Less than a tenth of a second.
        out.clear();
        hearing.sent(ms(5_000), MediaDur::from_millis(60));
        assert_eq!(hearing.finalize(Instant::now(), &mut out), None);
        assert_eq!(out, [words(span(5_000, 5_060), "", true)]);
        assert_eq!(hearing.waiting_since(), None);

        // The provider still holds it, and it counts towards the next.
        out.clear();
        hearing.sent(ms(5_060), MediaDur::from_millis(60));
        let commit = hearing.finalize(Instant::now(), &mut out);
        assert!(commit.is_some_and(|commit| commit.contains("commit_1")));
        assert_eq!(out, []);
        let heard = hear(
            &mut hearing,
            &[committed("item_a"), completed("item_a", "")],
        );
        assert_eq!(heard, [words(span(5_060, 5_120), "", true)]);
    }

    #[test]
    fn a_request_the_provider_refuses_for_want_of_audio_has_no_words() {
        let refusal = |named: &str| {
            format!(
                "{{\"type\":\"error\",\"error\":{{\"type\":\"invalid_request_error\",\
                 \"code\":\"input_audio_buffer_commit_empty\",\"message\":\"buffer too small\",\
                 \"event_id\":{named}}}}}"
            )
        };
        for named in ["\"commit_1\"", "null"] {
            let mut hearing = Hearing::new(None);
            let mut out = Vec::new();
            send(&mut hearing, 0, 2);
            assert!(hearing.finalize(Instant::now(), &mut out).is_some());
            let heard = hear(&mut hearing, &[refusal(named)]);
            assert_eq!(heard, [words(span(0, 200), "", true)], "{named}");
            assert_eq!(hearing.waiting_since(), None);
            // The stream is as good as before.
            send(&mut hearing, 200, 10);
            assert!(hearing.finalize(Instant::now(), &mut out).is_some());
            let heard = hear(
                &mut hearing,
                &[committed("item_a"), completed("item_a", "Yes.")],
            );
            assert_eq!(heard, [words(span(200, 1_200), "Yes.", true)], "{named}");
        }
    }

    #[test]
    fn an_item_the_provider_could_not_make_words_of_ends_the_stream() {
        let mut hearing = Hearing::new(None);
        let mut out = Vec::new();
        send(&mut hearing, 0, 10);
        assert!(hearing.finalize(Instant::now(), &mut out).is_some());
        let failed = "{\"type\":\"conversation.item.input_audio_transcription.failed\",\
                      \"item_id\":\"item_a\",\"content_index\":0,\"error\":{\"type\":\
                      \"transcription_error\",\"code\":\"audio_unintelligible\",\
                      \"message\":\"The audio could not be transcribed.\"}}";
        let error = hearing.heard(failed, &mut out).unwrap_err();
        assert_eq!(error.class, RetryClass::Transient);
        assert!(
            error.message.contains("could not be transcribed"),
            "{error}"
        );
        assert_eq!(out, []);
    }

    #[test]
    fn what_is_not_about_words_is_passed_over() {
        let mut hearing = Hearing::new(None);
        send(&mut hearing, 0, 10);
        let heard = hear(
            &mut hearing,
            &[
                "{\"type\":\"session.created\",\"session\":{}}".to_owned(),
                "{\"type\":\"conversation.item.added\",\"item\":{\"id\":\"item_a\"}}".to_owned(),
                "{\"type\":\"something.new\",\"item_id\":\"item_z\"}".to_owned(),
                "not json at all".to_owned(),
                // Words of nothing but white space are no words.
                delta("item_a", " "),
                // An answer nobody asked for.
                completed("item_q", "Unasked."),
            ],
        );
        assert_eq!(heard, []);
    }
}
