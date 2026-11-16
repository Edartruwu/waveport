//! What is sent and what comes back, with no connection in sight.
//!
//! The provider calls the speech of one response a context. Text is
//! added to a context a piece at a time, and its audio comes back in
//! pieces that carry the context's name, with the times of the words
//! beside it. [`Speaking`] keeps the text that was given, finds each word
//! the provider times in it, and so turns the provider's times into
//! places in the text.

use serde::{Deserialize, Serialize};
use waveport_adapter_kit::pcm::{PcmAssembler, read_base64};
use waveport_adapter_kit::retry::error_of_status;
use waveport_adapter_kit::secret::Secret;
use waveport_domain::time::MediaDur;
use waveport_ports::audio::Rate;
use waveport_ports::error::PortError;
use waveport_ports::tts::{TtsEvent, TtsMark};

use crate::Config;

/// How far past the last word that was found the next one is looked for,
/// in bytes. A word the provider names differently from how it was
/// written, a number it spelt out, is not found, and must not be taken
/// for the same word much further on.
const REACH: usize = 96;

// ---------------------------------------------------------------------
// What is sent
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Request<'a> {
    model_id: &'a str,
    transcript: &'a str,
    voice: Voice<'a>,
    language: &'a str,
    context_id: &'a str,
    output_format: Format,
    add_timestamps: bool,
    /// The times of the words as they were written, not as the provider
    /// rewrote them to read them out.
    use_normalized_timestamps: bool,
    r#continue: bool,
    max_buffer_delay_ms: u32,
}

#[derive(Debug, Serialize)]
struct Voice<'a> {
    id: &'a str,
}

#[derive(Debug, Serialize)]
struct Format {
    container: &'static str,
    encoding: &'static str,
    sample_rate: u32,
}

/// The provider's code for a language, from its tag: the language alone,
/// in lower case.
pub(crate) fn language_code(tag: &str) -> String {
    let tag = tag.trim().replace('_', "-").to_ascii_lowercase();
    tag.split('-').next().unwrap_or_default().to_owned()
}

// ---------------------------------------------------------------------
// What comes back
// ---------------------------------------------------------------------

/// A message of the provider, as far as this adapter reads it.
#[derive(Debug, Default, Deserialize)]
struct Incoming {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    context_id: Option<String>,
    /// Audio, in base64.
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    word_timestamps: Option<Words>,
    #[serde(default)]
    status_code: Option<u16>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
}

/// Words, and when each begins and ends in the audio of the context, in
/// seconds.
#[derive(Debug, Default, Deserialize)]
struct Words {
    #[serde(default)]
    words: Vec<String>,
    #[serde(default)]
    end: Vec<f64>,
}

/// What a message of the provider meant for a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Heard {
    /// It was about this stream, which goes on.
    More,
    /// The speech of this stream is all there.
    Done,
    /// It was about something else.
    NotOurs,
}

/// A length of audio from the provider's seconds.
fn length(seconds: f64) -> MediaDur {
    let ticks = (seconds * 48_000.0).round();
    if ticks.is_nan() || ticks <= 0.0 {
        return MediaDur::ZERO;
    }
    if ticks >= f64::from(u32::MAX) {
        return MediaDur::from_ticks(u32::MAX);
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "between nought and the largest length, as was just seen to"
    )]
    MediaDur::from_ticks(ticks as u32)
}

// ---------------------------------------------------------------------
// The speech of one response
// ---------------------------------------------------------------------

/// The speech of one response: what was asked for, and what has come.
#[derive(Debug)]
pub(crate) struct Speaking {
    context: String,
    model: String,
    voice: String,
    language: String,
    buffer_ms: u32,
    rate: Rate,
    key: Option<Secret>,
    /// The text given so far.
    text: String,
    /// Where in the text the search for the next word begins.
    cursor: usize,
    /// Whether the provider was told that no more text follows.
    finished: bool,
    assembler: PcmAssembler,
    /// How many samples of audio were handed on.
    samples: usize,
    /// The newest mark, which the next may not go back from.
    last: TtsMark,
    /// Whether the provider has said anything about this speech.
    heard: bool,
}

impl Speaking {
    pub(crate) fn new(
        config: &Config,
        context: String,
        rate: Rate,
        language: Option<&str>,
    ) -> Self {
        Self {
            context,
            model: config.model.clone(),
            voice: config.voice.clone(),
            language: language.map_or_else(|| config.language.clone(), language_code),
            buffer_ms: config.max_buffer_delay_ms,
            rate,
            key: config.key.clone(),
            text: String::new(),
            cursor: 0,
            finished: false,
            assembler: PcmAssembler::new(),
            samples: 0,
            last: TtsMark {
                text_end: 0,
                audio_end: MediaDur::ZERO,
            },
            heard: false,
        }
    }

    /// Whether the provider has said anything about this speech yet.
    pub(crate) const fn was_heard(&self) -> bool {
        self.heard
    }

    /// Whether the provider was told that no more text follows.
    pub(crate) const fn is_finished(&self) -> bool {
        self.finished
    }

    /// The message that adds `text` to the speech.
    pub(crate) fn speak(&mut self, text: &str) -> Result<String, PortError> {
        self.text.push_str(text);
        self.request(text, true)
    }

    /// The message that says no more text follows.
    pub(crate) fn finish(&mut self) -> Result<String, PortError> {
        self.finished = true;
        self.request("", false)
    }

    fn request(&self, transcript: &str, more: bool) -> Result<String, PortError> {
        let request = Request {
            model_id: &self.model,
            transcript,
            voice: Voice { id: &self.voice },
            language: &self.language,
            context_id: &self.context,
            output_format: Format {
                container: "raw",
                encoding: "pcm_s16le",
                sample_rate: self.rate.hz(),
            },
            add_timestamps: true,
            use_normalized_timestamps: false,
            r#continue: more,
            max_buffer_delay_ms: self.buffer_ms,
        };
        serde_json::to_string(&request).map_err(|error| {
            PortError::permanent(format!("the speech cannot be asked for: {error}"))
        })
    }

    /// Takes a message of the provider, and adds to `out` what it means
    /// for whoever listens.
    ///
    /// # Errors
    ///
    /// The provider says that it cannot speak this, or sent audio that
    /// cannot be read.
    pub(crate) fn heard(
        &mut self,
        message: &str,
        out: &mut Vec<TtsEvent>,
    ) -> Result<Heard, PortError> {
        // What cannot be read is something this adapter does not know
        // yet, and was not waiting for.
        let Ok(incoming) = serde_json::from_str::<Incoming>(message) else {
            return Ok(Heard::NotOurs);
        };
        // A failure that names no context is about the connection, and
        // so about this speech too.
        let ours = match incoming.context_id.as_deref() {
            Some(context) => context == self.context,
            None => incoming.r#type == "error",
        };
        if !ours {
            return Ok(Heard::NotOurs);
        }
        self.heard = true;
        match incoming.r#type.as_str() {
            "chunk" => {
                let data = incoming.data.unwrap_or_default();
                if let Some(pcm) = self.assembler.push(read_base64(&data)?) {
                    self.samples += pcm.len();
                    out.push(TtsEvent::Audio(pcm));
                }
            }
            "timestamps" => {
                let Words { words, end } = incoming.word_timestamps.unwrap_or_default();
                for (word, end) in words.iter().zip(end) {
                    self.word(word, length(end), out);
                }
            }
            "done" => {
                self.end(out);
                return Ok(Heard::Done);
            }
            "error" => {
                let said = [incoming.title, incoming.message, incoming.error_code]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(": ");
                let said = self
                    .key
                    .as_ref()
                    .map_or_else(|| said.clone(), |key| key.strike(&said));
                return Err(error_of_status(
                    incoming.status_code.unwrap_or(500),
                    None,
                    &said,
                ));
            }
            _ => {}
        }
        Ok(Heard::More)
    }

    /// The provider says when `word` has been spoken. The word is looked
    /// for in the text that follows the word before it. One that is not
    /// there, because the provider says it otherwise than it was written,
    /// gives no mark; the next one that is found covers it.
    fn word(&mut self, word: &str, audio_end: MediaDur, out: &mut Vec<TtsEvent>) {
        let word = word.trim();
        if word.is_empty() {
            return;
        }
        let ahead = self.text.get(self.cursor..).unwrap_or_default();
        let Some(found) = ahead.find(word).filter(|found| *found <= REACH) else {
            return;
        };
        self.cursor += found + word.len();
        // A mark is handed on as soon as it is known, and not held for
        // the next one to follow: whoever gives this stream its text a
        // sentence at a time waits for the last word of a sentence to be
        // marked before it gives the next.
        self.last = TtsMark {
            text_end: u32::try_from(self.cursor).unwrap_or(u32::MAX),
            // Marks do not go back, whatever the provider's times do.
            audio_end: audio_end.max(self.last.audio_end),
        };
        out.push(TtsEvent::Mark(self.last));
    }

    /// All the audio is there, and all the text was spoken by the time it
    /// ends.
    fn end(&mut self, out: &mut Vec<TtsEvent>) {
        let spoken = self.rate.length_of(self.samples);
        let all = u32::try_from(self.text.len()).unwrap_or(u32::MAX);
        if all > self.last.text_end && self.samples > 0 {
            self.last = TtsMark {
                text_end: all,
                audio_end: spoken.max(self.last.audio_end),
            };
            out.push(TtsEvent::Mark(self.last));
        }
        out.push(TtsEvent::Done);
    }
}

#[cfg(test)]
mod tests {
    use waveport_adapter_kit::pcm::write_base64;
    use waveport_adapter_kit::secret::Secret;
    use waveport_domain::time::MediaDur;
    use waveport_ports::audio::Rate;
    use waveport_ports::error::RetryClass;
    use waveport_ports::tts::{TtsEvent, TtsMark};

    use super::{Heard, Speaking, language_code, length};
    use crate::Config;

    fn speaking() -> Speaking {
        let config = Config::new("sonic-3.6", "voice-1", Some(Secret::new("sk-not-a-key")));
        Speaking::new(&config, "ctx-1".to_owned(), Rate::Hz8000, None)
    }

    /// A message that carries `millis` of audio for `context`.
    fn chunk(context: &str, millis: u32) -> String {
        let samples = vec![100_i16; Rate::Hz8000.samples_in_millis(millis)];
        let mut data = String::new();
        write_base64(&samples, &mut data);
        format!(
            "{{\"type\":\"chunk\",\"data\":\"{data}\",\"done\":false,\"status_code\":206,\
             \"step_time\":12,\"context_id\":\"{context}\"}}"
        )
    }

    fn timestamps(context: &str, words: &[(&str, f64)]) -> String {
        let names: Vec<String> = words
            .iter()
            .map(|(word, _)| format!("\"{word}\""))
            .collect();
        let ends: Vec<String> = words.iter().map(|(_, end)| end.to_string()).collect();
        format!(
            "{{\"type\":\"timestamps\",\"done\":false,\"status_code\":206,\"context_id\":\
             \"{context}\",\"word_timestamps\":{{\"words\":[{}],\"start\":[],\"end\":[{}]}}}}",
            names.join(","),
            ends.join(",")
        )
    }

    fn done(context: &str) -> String {
        format!(
            "{{\"type\":\"done\",\"done\":true,\"status_code\":206,\"context_id\":\"{context}\"}}"
        )
    }

    const fn mark(text_end: u32, millis: u32) -> TtsEvent {
        TtsEvent::Mark(TtsMark {
            text_end,
            audio_end: MediaDur::from_millis(millis),
        })
    }

    /// Gives the speech these messages, and returns the marks and the
    /// end that came of them, with how much audio there was.
    fn hear(speaking: &mut Speaking, messages: &[String]) -> (Vec<TtsEvent>, usize) {
        let mut out = Vec::new();
        for message in messages {
            speaking.heard(message, &mut out).unwrap();
        }
        let samples = out
            .iter()
            .map(|event| match event {
                TtsEvent::Audio(pcm) => pcm.len(),
                _ => 0,
            })
            .sum();
        out.retain(|event| !matches!(event, TtsEvent::Audio(_)));
        (out, samples)
    }

    #[test]
    fn text_is_added_to_a_context_and_the_context_is_ended() {
        let mut speaking = speaking();
        assert_eq!(
            speaking.speak("Hello,").unwrap(),
            "{\"model_id\":\"sonic-3.6\",\"transcript\":\"Hello,\",\"voice\":{\"id\":\"voice-1\"},\
             \"language\":\"en\",\"context_id\":\"ctx-1\",\"output_format\":{\"container\":\"raw\",\
             \"encoding\":\"pcm_s16le\",\"sample_rate\":8000},\"add_timestamps\":true,\
             \"use_normalized_timestamps\":false,\"continue\":true,\"max_buffer_delay_ms\":0}"
        );
        assert!(!speaking.is_finished());
        let last = speaking.finish().unwrap();
        assert!(last.contains("\"transcript\":\"\""), "{last}");
        assert!(last.contains("\"continue\":false"), "{last}");
        assert!(speaking.is_finished());
        // What has to be written with care is.
        let quoted = speaking.speak("She said \"no\".\n").unwrap();
        assert!(
            quoted.contains("\"transcript\":\"She said \\\"no\\\".\\n\""),
            "{quoted}"
        );
    }

    #[test]
    fn a_language_is_asked_for_by_the_providers_code() {
        for (tag, code) in [
            ("en", "en"),
            ("en-US", "en"),
            ("es_PE", "es"),
            (" FR ", "fr"),
        ] {
            assert_eq!(language_code(tag), code, "{tag}");
        }
        let config = Config::new("sonic-3.6", "voice-1", None);
        let mut speaking = Speaking::new(&config, "c".to_owned(), Rate::Hz24000, Some("es-PE"));
        let asked = speaking.speak("Hola.").unwrap();
        assert!(asked.contains("\"language\":\"es\""), "{asked}");
        assert!(asked.contains("\"sample_rate\":24000"), "{asked}");
    }

    #[test]
    fn the_providers_seconds_are_lengths_of_audio() {
        assert_eq!(length(0.5), MediaDur::from_millis(500));
        assert_eq!(length(0.0), MediaDur::ZERO);
        assert_eq!(length(-1.0), MediaDur::ZERO);
        assert_eq!(length(f64::NAN), MediaDur::ZERO);
        assert_eq!(length(1e12), MediaDur::from_ticks(u32::MAX));
    }

    #[test]
    fn the_times_of_words_become_places_in_the_text() {
        let mut speaking = speaking();
        speaking.speak("Hello, world!").unwrap();
        speaking.speak(" How are you?").unwrap();
        speaking.finish().unwrap();
        assert!(!speaking.was_heard());
        let (events, samples) = hear(
            &mut speaking,
            &[
                chunk("ctx-1", 500),
                timestamps("ctx-1", &[("Hello", 0.4), ("world", 0.9)]),
                chunk("ctx-1", 500),
                chunk("ctx-1", 500),
                timestamps("ctx-1", &[("How", 1.1), ("are", 1.2), ("you", 1.6)]),
                done("ctx-1"),
            ],
        );
        assert!(speaking.was_heard());
        assert_eq!(samples, 12_000);
        assert_eq!(
            events,
            [
                // Each mark is where its word ends in the text.
                mark(5, 400),
                mark(12, 900),
                mark(17, 1_100),
                mark(21, 1_200),
                // The provider's time for the last word is past the end
                // of the audio. It is handed on as it came: a mark is not
                // held until the audio has ended to be set right.
                mark(25, 1_600),
                // And the mark of the end takes in what follows it. It
                // does not go back.
                mark(26, 1_600),
                TtsEvent::Done,
            ]
        );
    }

    #[test]
    fn a_mark_is_handed_on_before_anything_follows_it() {
        // The first sentence of a response, and nothing after it yet:
        // whoever gives the text waits for its last word to be marked
        // before the next sentence is given.
        let mut speaking = speaking();
        speaking.speak("It is ten past three. ").unwrap();
        let (events, _) = hear(
            &mut speaking,
            &[
                chunk("ctx-1", 900),
                timestamps("ctx-1", &[("It", 0.1), ("is", 0.2), ("ten", 0.4)]),
                timestamps("ctx-1", &[("past", 0.6), ("three.", 0.9)]),
            ],
        );
        assert_eq!(
            events,
            [
                mark(2, 100),
                mark(5, 200),
                mark(9, 400),
                mark(14, 600),
                mark(21, 900),
            ]
        );
    }

    #[test]
    fn a_word_the_provider_names_otherwise_gives_no_mark() {
        let mut speaking = speaking();
        speaking.speak("It costs 5 dollars today.").unwrap();
        let (events, _) = hear(
            &mut speaking,
            &[
                chunk("ctx-1", 2_000),
                timestamps(
                    "ctx-1",
                    &[
                        ("It", 0.2),
                        ("costs", 0.5),
                        // Spelt out by the provider: not in the text.
                        ("five", 0.8),
                        ("dollars", 1.2),
                        // Times that go back do not take the marks back.
                        ("today", 1.0),
                        ("", 1.9),
                    ],
                ),
                done("ctx-1"),
            ],
        );
        assert_eq!(
            events,
            [
                mark(2, 200),
                mark(8, 500),
                mark(18, 1_200),
                mark(24, 1_200),
                mark(25, 2_000),
                TtsEvent::Done,
            ]
        );
    }

    #[test]
    fn a_word_is_not_looked_for_far_from_the_one_before() {
        let mut speaking = speaking();
        let filler = "la ".repeat(40);
        speaking.speak(&format!("One {filler}end.")).unwrap();
        let (events, _) = hear(
            &mut speaking,
            &[
                chunk("ctx-1", 1_000),
                // "end" is there, but 120 bytes on: not this word.
                timestamps("ctx-1", &[("One", 0.2), ("end", 0.4)]),
                done("ctx-1"),
            ],
        );
        assert_eq!(events, [mark(3, 200), mark(128, 1_000), TtsEvent::Done]);
    }

    #[test]
    fn what_is_about_another_context_is_passed_over() {
        let mut speaking = speaking();
        speaking.speak("Yes.").unwrap();
        let mut out = Vec::new();
        for message in [
            chunk("ctx-0", 100),
            timestamps("ctx-0", &[("Yes", 0.1)]),
            done("ctx-0"),
            "{\"type\":\"flush_done\",\"flush_id\":1,\"context_id\":\"ctx-0\"}".to_owned(),
            "not json".to_owned(),
            "{\"type\":\"error\",\"status_code\":400,\"title\":\"Bad\",\"context_id\":\"ctx-0\"}"
                .to_owned(),
        ] {
            assert_eq!(speaking.heard(&message, &mut out), Ok(Heard::NotOurs));
        }
        assert_eq!(out, []);
        assert!(!speaking.was_heard());
        // Something about this one that says nothing new.
        let flushed = "{\"type\":\"flush_done\",\"flush_id\":1,\"context_id\":\"ctx-1\"}";
        assert_eq!(speaking.heard(flushed, &mut out), Ok(Heard::More));
        assert!(speaking.was_heard());
    }

    #[test]
    fn a_speech_with_no_audio_ends_without_a_mark() {
        let mut speaking = speaking();
        speaking.finish().unwrap();
        let mut out = Vec::new();
        assert_eq!(speaking.heard(&done("ctx-1"), &mut out), Ok(Heard::Done));
        assert_eq!(out, [TtsEvent::Done]);
    }

    #[test]
    fn what_the_provider_says_went_wrong_is_sorted_by_its_status() {
        for (status, class) in [
            (400, RetryClass::Permanent),
            (401, RetryClass::Auth),
            (429, RetryClass::RateLimited),
            (500, RetryClass::Transient),
        ] {
            let mut speaking = speaking();
            // With the context named, and without: both are about this
            // speech.
            for context in ["\"context_id\":\"ctx-1\",", ""] {
                let message = format!(
                    "{{\"type\":\"error\",\"done\":true,{context}\"title\":\"Invalid model\",\
                     \"message\":\"The key sk-not-a-key cannot use it.\",\
                     \"error_code\":\"model_not_found\",\"status_code\":{status}}}"
                );
                let error = speaking.heard(&message, &mut Vec::new()).unwrap_err();
                assert_eq!(error.class, class, "{status}");
                assert!(error.message.contains("Invalid model"), "{error}");
                assert!(error.message.contains("model_not_found"), "{error}");
                assert!(!error.message.contains("sk-not-a-key"), "{error}");
            }
        }
        // Audio that is not audio.
        let mut speaking = speaking();
        let broken = "{\"type\":\"chunk\",\"data\":\"***\",\"context_id\":\"ctx-1\"}";
        let error = speaking.heard(broken, &mut Vec::new()).unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
    }
}
