//! What is sent and what comes back, with no connection in sight.
//!
//! The provider calls the speech of one response a context. Text is
//! added to a context a piece at a time, and its audio comes back in
//! pieces that carry the context's name, each with the characters that
//! are spoken in it and when. [`Speaking`] keeps the text that was given,
//! follows the provider's characters through it, and so turns the
//! provider's times into places in the text.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use waveport_adapter_kit::pcm::{PcmAssembler, read_base64};
use waveport_adapter_kit::secret::Secret;
use waveport_domain::time::MediaDur;
use waveport_ports::audio::Rate;
use waveport_ports::error::PortError;
use waveport_ports::tts::{TtsEvent, TtsMark};

/// How much of what the provider says of a failure is passed on.
const SAID_AT_MOST: usize = 300;

// ---------------------------------------------------------------------
// What is sent
// ---------------------------------------------------------------------

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "the shape serde asks of such a function"
)]
const fn is_false(flag: &bool) -> bool {
    !*flag
}

#[derive(Debug, Serialize)]
struct Outgoing<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    context_id: &'a str,
    /// Speak what there is, and do not wait for more to go with it.
    #[serde(skip_serializing_if = "is_false")]
    flush: bool,
    #[serde(skip_serializing_if = "is_false")]
    close_context: bool,
}

// ---------------------------------------------------------------------
// What comes back
// ---------------------------------------------------------------------

/// A message of the provider, as far as this adapter reads it.
#[derive(Debug, Default, Deserialize)]
struct Incoming {
    /// Audio, in base64.
    #[serde(default)]
    audio: Option<String>,
    /// The provider writes this name in two ways in its own examples.
    #[serde(default, rename = "contextId", alias = "context_id")]
    context: Option<String>,
    /// And this one.
    #[serde(default, rename = "isFinal", alias = "is_final")]
    last: Option<bool>,
    #[serde(default)]
    alignment: Option<Alignment>,
    /// A failure, by a name for its kind.
    #[serde(default)]
    error: Option<Value>,
    /// What went wrong, in words.
    #[serde(default)]
    message: Option<Value>,
}

/// The characters spoken in a piece of audio, and when each begins and
/// how long it lasts, in milliseconds on a clock of the provider's that
/// begins with the speech of the response and runs on from one piece to
/// the next.
#[derive(Debug, Default, Deserialize)]
struct Alignment {
    #[serde(default)]
    chars: Vec<String>,
    #[serde(default, rename = "charStartTimesMs")]
    starts: Vec<f64>,
    #[serde(default, rename = "charDurationsMs")]
    lengths: Vec<f64>,
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

/// A length of audio from the provider's milliseconds.
fn length(millis: f64) -> MediaDur {
    let ticks = (millis * 48.0).round();
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

/// What the provider says in words, whatever shape it says it in.
fn words(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// The failure the provider tells of in a message. Its documentation
/// does not say how it tells of one, so this goes by the name it gives
/// the failure and the words it adds: whatever speaks of the key is the
/// key's fault, whatever speaks of too much or too long may pass, and
/// everything else would fail again if it were tried again.
fn failure(kind: &str, said: &str) -> PortError {
    let all = format!("{kind} {said}").to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| all.contains(word));
    let said: String = said.chars().take(SAID_AT_MOST).collect();
    let message = format!("the provider said ({kind}): {said}");
    if has(&[
        "api_key",
        "api key",
        "unauthorized",
        "authoriz",
        "authenticat",
    ]) {
        PortError::auth(message)
    } else if has(&[
        "rate_limit",
        "rate limit",
        "too_many",
        "too many",
        "concurrent",
    ]) {
        PortError::rate_limited(None, message)
    } else if has(&["timeout", "timed out", "busy", "overloaded", "unavailable"]) {
        PortError::transient(message)
    } else {
        PortError::permanent(message)
    }
}

// ---------------------------------------------------------------------
// The speech of one response
// ---------------------------------------------------------------------

/// The speech of one response: what was asked for, and what has come.
#[derive(Debug)]
pub(crate) struct Speaking {
    context: String,
    rate: Rate,
    key: Option<Secret>,
    /// The text given so far.
    text: String,
    /// How far into the text the provider's characters have been
    /// followed.
    cursor: usize,
    /// Whether the provider was told that no more text follows.
    finished: bool,
    assembler: PcmAssembler,
    /// How many bytes of audio have come.
    bytes: usize,
    /// How many samples of audio were handed on.
    samples: usize,
    /// The newest mark, which the next may not go back from.
    last: TtsMark,
    /// Whether the provider has said anything about this speech.
    heard: bool,
}

impl Speaking {
    pub(crate) fn new(context: String, rate: Rate, key: Option<Secret>) -> Self {
        Self {
            context,
            rate,
            key,
            text: String::new(),
            cursor: 0,
            finished: false,
            assembler: PcmAssembler::new(),
            bytes: 0,
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

    /// The message that begins the speech. The provider asks for one
    /// space as its text.
    pub(crate) fn begin(&self) -> Result<String, PortError> {
        self.message(Some(" "), false, false)
    }

    /// The message that adds `text` to the speech, to be spoken without
    /// waiting for more.
    pub(crate) fn speak(&mut self, text: &str) -> Result<String, PortError> {
        self.text.push_str(text);
        self.message(Some(text), true, false)
    }

    /// The message that says no more text follows.
    pub(crate) fn finish(&mut self) -> Result<String, PortError> {
        self.finished = true;
        self.message(None, false, true)
    }

    fn message(&self, text: Option<&str>, flush: bool, close: bool) -> Result<String, PortError> {
        let outgoing = Outgoing {
            text,
            context_id: &self.context,
            flush,
            close_context: close,
        };
        serde_json::to_string(&outgoing).map_err(|error| {
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
        // What names no context is about the connection, and so about
        // this speech too: there is one at a time on it.
        let ours = incoming
            .context
            .as_deref()
            .is_none_or(|context| context == self.context);
        if !ours {
            return Ok(Heard::NotOurs);
        }
        if incoming.error.as_ref().is_some_and(|kind| !kind.is_null()) {
            let kind = words(incoming.error.as_ref());
            let said = words(incoming.message.as_ref());
            let said = self
                .key
                .as_ref()
                .map_or_else(|| said.clone(), |key| key.strike(&said));
            return Err(failure(&kind, &said));
        }
        let audio = incoming.audio.unwrap_or_default();
        let last = incoming.last == Some(true);
        if audio.is_empty() && incoming.alignment.is_none() && !last {
            // Nothing this adapter reads.
            return Ok(Heard::NotOurs);
        }
        self.heard = true;
        // Where this piece of audio begins in all the audio of the
        // speech, and where it ends.
        let begins = self.rate.length_of(self.bytes / 2);
        if !audio.is_empty() {
            let piece = read_base64(&audio)?;
            self.bytes += piece.len();
            if let Some(pcm) = self.assembler.push(piece) {
                self.samples += pcm.len();
                out.push(TtsEvent::Audio(pcm));
            }
        }
        let ends = self.rate.length_of(self.bytes / 2);
        if let Some(Alignment {
            chars,
            starts,
            lengths,
        }) = incoming.alignment
        {
            // The provider times the characters on a clock of its own,
            // which begins with the speech and runs a little ahead of
            // the audio it sends: the characters of a piece begin where
            // those of the piece before ended on that clock, some
            // milliseconds past the end of that piece's audio. So a
            // character is placed by how far into its piece the provider
            // says it is, from where the audio of the piece begins, and
            // never past the audio there is.
            let origin = starts.first().copied().unwrap_or_default();
            for ((spoken, start), long) in chars.iter().zip(starts).zip(lengths) {
                let into = length((start - origin).max(0.0) + long);
                self.spoken(spoken, begins.saturating_add(into).min(ends), out);
            }
        }
        // The provider says that a context is over when it has been told
        // that no more text follows and has spoken everything. Said
        // before that, it is not about the end of this speech.
        if last && self.finished {
            self.end(out);
            return Ok(Heard::Done);
        }
        Ok(Heard::More)
    }

    /// The provider says when `spoken`, a character of the text, has been
    /// said. It is the next one of the text that was given, or the next
    /// after the space there; a character of the provider's that is not,
    /// such as the space it is given to begin with, gives no mark.
    fn spoken(&mut self, spoken: &str, audio_end: MediaDur, out: &mut Vec<TtsEvent>) {
        if spoken.is_empty() {
            return;
        }
        let ahead = self.text.get(self.cursor..).unwrap_or_default();
        let found = if ahead.starts_with(spoken) {
            0
        } else {
            // Space that the provider passed over.
            let space = ahead.len() - ahead.trim_start().len();
            if space == 0 || !ahead.get(space..).unwrap_or_default().starts_with(spoken) {
                return;
            }
            space
        };
        self.cursor += found + spoken.len();
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

    use super::{Heard, Speaking, length};

    fn speaking() -> Speaking {
        let key = Some(Secret::new("sk-not-a-key"));
        Speaking::new("ctx-1".to_owned(), Rate::Hz16000, key)
    }

    /// A message that carries `millis` of audio for `context`, in which
    /// `text` is spoken, a character every 10 ms from `from` on the
    /// provider's clock, which begins with the speech.
    fn audio(context: &str, millis: u32, text: &str, from: u32) -> String {
        let samples = vec![100_i16; Rate::Hz16000.samples_in_millis(millis)];
        let mut data = String::new();
        write_base64(&samples, &mut data);
        let chars: Vec<String> = text.chars().map(|c| format!("\"{c}\"")).collect();
        let starts: Vec<String> = (0..text.chars().count())
            .map(|n| (u32::try_from(n).unwrap() * 10 + from).to_string())
            .collect();
        let lengths = vec!["10"; text.chars().count()];
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
            "{{\"audio\":\"{data}\",\"contextId\":\"{context}\",\"alignment\":{alignment},\
             \"normalizedAlignment\":{alignment}}}"
        )
    }

    fn over(context: &str) -> String {
        format!("{{\"isFinal\":true,\"contextId\":\"{context}\"}}")
    }

    const fn mark(text_end: u32, millis: u32) -> TtsEvent {
        TtsEvent::Mark(TtsMark {
            text_end,
            audio_end: MediaDur::from_millis(millis),
        })
    }

    /// What `out` holds besides audio, and how many samples of audio.
    fn told(out: &[TtsEvent]) -> (Vec<TtsEvent>, usize) {
        let samples = out
            .iter()
            .map(|event| match event {
                TtsEvent::Audio(pcm) => pcm.len(),
                _ => 0,
            })
            .sum();
        let rest = out
            .iter()
            .filter(|event| !matches!(event, TtsEvent::Audio(_)))
            .cloned()
            .collect();
        (rest, samples)
    }

    #[test]
    fn what_is_sent_is_what_the_provider_asks_for() {
        let mut speaking = speaking();
        assert_eq!(
            speaking.begin().unwrap(),
            "{\"text\":\" \",\"context_id\":\"ctx-1\"}"
        );
        assert_eq!(
            speaking.speak("He said \"hi\".").unwrap(),
            "{\"text\":\"He said \\\"hi\\\".\",\"context_id\":\"ctx-1\",\"flush\":true}"
        );
        assert!(!speaking.is_finished());
        assert_eq!(
            speaking.finish().unwrap(),
            "{\"context_id\":\"ctx-1\",\"close_context\":true}"
        );
        assert!(speaking.is_finished());
    }

    #[test]
    fn every_character_is_marked_where_it_ends_in_all_the_audio() {
        let mut speaking = speaking();
        speaking.speak("Hi, ").unwrap();
        speaking.speak("you.").unwrap();
        speaking.finish().unwrap();
        let mut out = Vec::new();
        // The first piece of audio: 100 ms, in which "Hi, " is spoken
        // from the beginning.
        let first = audio("ctx-1", 100, "Hi, ", 0);
        assert_eq!(speaking.heard(&first, &mut out).unwrap(), Heard::More);
        // The second: 200 ms, which begin 100 ms into the speech. The
        // provider's clock goes on from the first piece and is a little
        // ahead of the audio by now, at 106 ms. It adds a space of its
        // own at the end of what it was given, which is not of the text.
        let second = audio("ctx-1", 200, "you. ", 106);
        assert_eq!(speaking.heard(&second, &mut out).unwrap(), Heard::More);
        assert_eq!(
            speaking.heard(&over("ctx-1"), &mut out).unwrap(),
            Heard::Done
        );
        let (rest, samples) = told(&out);
        assert_eq!(samples, 4_800);
        assert_eq!(
            rest,
            [
                mark(1, 10),
                mark(2, 20),
                mark(3, 30),
                mark(4, 40),
                mark(5, 110),
                mark(6, 120),
                mark(7, 130),
                mark(8, 140),
                TtsEvent::Done,
            ]
        );
    }

    #[test]
    fn a_character_is_never_placed_past_the_audio_there_is() {
        let mut speaking = speaking();
        speaking.speak("abcd").unwrap();
        let mut out = Vec::new();
        // The provider's clock runs past the end of the audio it sends:
        // 25 ms of audio, and four characters of 10 ms each.
        speaking
            .heard(&audio("ctx-1", 25, "abcd", 0), &mut out)
            .unwrap();
        let (rest, _) = told(&out);
        assert_eq!(rest, [mark(1, 10), mark(2, 20), mark(3, 25), mark(4, 25)]);
    }

    #[test]
    fn characters_that_are_not_of_the_text_are_passed_over() {
        let mut speaking = speaking();
        speaking.speak("It is  3 pm.").unwrap();
        speaking.finish().unwrap();
        let mut out = Vec::new();
        // The provider leaves out one of two spaces, and says a thing
        // that was not written.
        let said = audio("ctx-1", 300, "It is 3 §pm.", 0);
        speaking.heard(&said, &mut out).unwrap();
        speaking.heard(&over("ctx-1"), &mut out).unwrap();
        let (rest, _) = told(&out);
        let ends: Vec<u32> = rest
            .iter()
            .filter_map(|event| match event {
                TtsEvent::Mark(mark) => Some(mark.text_end),
                _ => None,
            })
            .collect();
        // "It is" (5), then both spaces at once with the 3 (8), the space
        // after it (9), and "pm." (12).
        assert_eq!(ends, [1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn text_whose_characters_never_come_is_marked_at_the_end() {
        let mut speaking = speaking();
        speaking.speak("Hello.").unwrap();
        speaking.finish().unwrap();
        let mut out = Vec::new();
        // Audio with no word of where its characters are.
        speaking
            .heard(&audio("ctx-1", 250, "", 0), &mut out)
            .unwrap();
        assert_eq!(told(&out).0, []);
        speaking.heard(&over("ctx-1"), &mut out).unwrap();
        assert_eq!(told(&out).0, [mark(6, 250), TtsEvent::Done]);

        // A speech that gave no audio at all marks nothing.
        let mut speaking = self::speaking();
        speaking.speak("Hello.").unwrap();
        speaking.finish().unwrap();
        let mut out = Vec::new();
        speaking.heard(&over("ctx-1"), &mut out).unwrap();
        assert_eq!(out, [TtsEvent::Done]);
    }

    #[test]
    fn marks_do_not_go_back() {
        let mut speaking = speaking();
        speaking.speak("abc").unwrap();
        let mut out = Vec::new();
        // The provider's times go back within a piece.
        let samples = vec![100_i16; Rate::Hz16000.samples_in_millis(100)];
        let mut data = String::new();
        write_base64(&samples, &mut data);
        let odd = format!(
            "{{\"audio\":\"{data}\",\"contextId\":\"ctx-1\",\"alignment\":{{\"chars\":[\"a\",\
             \"b\",\"c\"],\"charStartTimesMs\":[50,10,-70],\"charDurationsMs\":[60,10,10]}}}}"
        );
        speaking.heard(&odd, &mut out).unwrap();
        let (rest, _) = told(&out);
        assert_eq!(rest, [mark(1, 60), mark(2, 60), mark(3, 60)]);
    }

    #[test]
    fn the_end_is_the_end_only_once_the_text_is_over() {
        let mut speaking = speaking();
        speaking.speak("One.").unwrap();
        let mut out = Vec::new();
        speaking
            .heard(&audio("ctx-1", 100, "One.", 0), &mut out)
            .unwrap();
        // Said before the provider was told that the text is over, it is
        // not about this speech's end.
        assert_eq!(
            speaking.heard(&over("ctx-1"), &mut out).unwrap(),
            Heard::More
        );
        speaking.speak(" Two.").unwrap();
        speaking.finish().unwrap();
        speaking
            .heard(&audio("ctx-1", 100, " Two.", 0), &mut out)
            .unwrap();
        // The provider's other way of writing it.
        let last = "{\"is_final\":true,\"context_id\":\"ctx-1\"}";
        assert_eq!(speaking.heard(last, &mut out).unwrap(), Heard::Done);
        assert_eq!(out.last(), Some(&TtsEvent::Done));
    }

    #[test]
    fn what_is_about_another_speech_is_not_taken() {
        let mut speaking = speaking();
        speaking.speak("Hello.").unwrap();
        speaking.finish().unwrap();
        let mut out = Vec::new();
        for message in [
            audio("ctx-0", 100, "Bye.", 0),
            over("ctx-0"),
            "{\"error\":\"input_timeout_exceeded\",\"message\":\"too late\",\"contextId\":\
             \"ctx-0\"}"
                .to_owned(),
            "{\"something\":\"new\"}".to_owned(),
            "not JSON".to_owned(),
        ] {
            assert_eq!(speaking.heard(&message, &mut out).unwrap(), Heard::NotOurs);
        }
        assert_eq!(out, []);
        assert!(!speaking.was_heard());
        // What names no speech is about the one there is.
        let nameless = "{\"audio\":\"\",\"isFinal\":true}";
        assert_eq!(speaking.heard(nameless, &mut out).unwrap(), Heard::Done);
        assert!(speaking.was_heard());
    }

    #[test]
    fn a_failure_is_worth_another_try_or_not_by_what_it_is_called() {
        for (kind, said, class) in [
            (
                "invalid_api_key",
                "Invalid API key sk-not-a-key",
                RetryClass::Auth,
            ),
            (
                "voice_id_does_not_exist",
                "A voice with that id does not exist.",
                RetryClass::Permanent,
            ),
            (
                "quota_exceeded",
                "This request exceeds your quota.",
                RetryClass::Permanent,
            ),
            (
                "too_many_concurrent_requests",
                "Slow down.",
                RetryClass::RateLimited,
            ),
            (
                "input_timeout_exceeded",
                "Have not received a new text input within the timeout of 20.0 seconds.",
                RetryClass::Transient,
            ),
            ("system_busy", "Try again.", RetryClass::Transient),
        ] {
            let message = format!("{{\"message\":\"{said}\",\"error\":\"{kind}\",\"code\":1008}}");
            let error = speaking().heard(&message, &mut Vec::new()).unwrap_err();
            assert_eq!(error.class, class, "{error}");
            assert!(error.message.contains(kind), "{error}");
            // The provider may repeat what it was sent.
            assert!(!error.message.contains("sk-not-a-key"), "{error}");
        }
        // A failure that is not told in words is still one.
        let odd = "{\"error\":{\"status\":\"bad\"},\"contextId\":\"ctx-1\"}";
        let error = speaking().heard(odd, &mut Vec::new()).unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent, "{error}");
        // And one that is said at length is cut short.
        let long = format!(
            "{{\"error\":\"model_failed\",\"message\":\"{}\"}}",
            "x".repeat(5_000)
        );
        let error = speaking().heard(&long, &mut Vec::new()).unwrap_err();
        assert!(error.message.len() < 400);
        // Audio that cannot be read fails the stream too.
        let bad = "{\"audio\":\"!!!\",\"contextId\":\"ctx-1\"}";
        assert!(speaking().heard(bad, &mut Vec::new()).is_err());
    }

    #[test]
    fn a_time_that_is_no_time_is_no_length() {
        assert_eq!(length(250.0), MediaDur::from_millis(250));
        assert_eq!(length(-5.0), MediaDur::ZERO);
        assert_eq!(length(f64::NAN), MediaDur::ZERO);
        assert_eq!(length(f64::INFINITY), MediaDur::from_ticks(u32::MAX));
    }
}
