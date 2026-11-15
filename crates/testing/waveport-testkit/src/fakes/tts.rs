//! A speech synthesiser that follows a script.
//!
//! Its speech is a tone, and every character of the text takes the same
//! time to say. That makes it easy to know, in a test, which text a
//! place in the audio belongs to.

use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use waveport_adapter_kit::stream::{self, Events, Sender};
use waveport_domain::time::MediaDur;
use waveport_ports::audio::{PcmBytes, Rate, RateSet};
use waveport_ports::error::{BoxFuture, Full, PortError};
use waveport_ports::tts::{
    Alignment, Tts, TtsCaps, TtsConfig, TtsEvent, TtsInput, TtsMark, TtsStream, TtsTx,
};

/// What a scripted synthesiser does.
#[derive(Clone, Debug)]
pub struct TtsScript {
    /// What it says it can do, which it then does.
    pub caps: TtsCaps,
    /// How long it takes to give the first audio of a stream.
    pub first: Duration,
    /// How many times faster than it is spoken the audio is made.
    pub speed: u32,
    /// How long each character of the text takes to say.
    pub per_char: MediaDur,
    /// How much audio it gives at a time.
    pub chunk: MediaDur,
    /// Whether a word is marked where the word itself ends, as a provider
    /// marks it, and not where the white space after it ends. The white
    /// space after the last word of a piece then has no mark at all until
    /// the stream has finished.
    pub bare_words: bool,
    /// With [`bare_words`](Self::bare_words): a word that has this in it
    /// gets no mark, as a word does that the provider names otherwise
    /// than it was written.
    pub unmarked: Option<String>,
    /// Opening a stream fails with this.
    pub fail_open: Option<PortError>,
    /// After this many pieces of audio a stream fails with this.
    pub fail_after: Option<(usize, PortError)>,
}

impl Default for TtsScript {
    fn default() -> Self {
        Self {
            caps: TtsCaps {
                rates: RateSet::of(&[Rate::Hz16000, Rate::Hz24000]),
                input: TtsInput::Incremental,
                alignment: Alignment::Word,
            },
            first: Duration::from_millis(100),
            speed: 4,
            per_char: MediaDur::from_millis(60),
            chunk: MediaDur::from_millis(100),
            bare_words: false,
            unmarked: None,
            fail_open: None,
            fail_after: None,
        }
    }
}

/// What a scripted synthesiser was asked to do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TtsLog {
    /// For each stream, the pieces of text it was given.
    pub spoken: Vec<Vec<String>>,
    /// How many streams were told that no more text follows.
    pub finished: usize,
}

/// A speech synthesiser that follows a script.
#[derive(Clone, Debug, Default)]
pub struct ScriptedTts {
    script: Arc<TtsScript>,
    log: Arc<Mutex<TtsLog>>,
}

impl ScriptedTts {
    /// A synthesiser that follows `script`.
    #[must_use]
    pub fn new(script: TtsScript) -> Self {
        Self {
            script: Arc::new(script),
            log: Arc::default(),
        }
    }

    /// What it was asked to do so far.
    #[must_use]
    pub fn log(&self) -> TtsLog {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Tts for ScriptedTts {
    fn caps(&self) -> TtsCaps {
        self.script.caps
    }

    fn open(&self, config: TtsConfig) -> BoxFuture<'_, Result<TtsStream, PortError>> {
        Box::pin(async move {
            if let Some(error) = &self.script.fail_open {
                return Err(error.clone());
            }
            if !self.script.caps.rates.contains(config.rate) {
                return Err(PortError::permanent(format!(
                    "audio at {} Hz is not given",
                    config.rate.hz()
                )));
            }
            let stream = {
                let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
                log.spoken.push(Vec::new());
                log.spoken.len() - 1
            };
            let script = Arc::clone(&self.script);
            let log = Arc::clone(&self.log);
            let (tx, rx) = stream::pair(64, |mut commands, events| async move {
                let mut voice = Voice {
                    script,
                    rate: config.rate,
                    events,
                    text: 0,
                    audio: 0,
                    given: 0,
                    wait: None,
                };
                while let Some(command) = commands.next().await {
                    match command {
                        Command::Speak(text) => {
                            if let Some(pieces) = log
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .spoken
                                .get_mut(stream)
                            {
                                pieces.push(text.clone());
                            }
                            voice.speak(&text).await?;
                        }
                        Command::Finish => {
                            log.lock().unwrap_or_else(PoisonError::into_inner).finished += 1;
                            voice.events.send(TtsEvent::Done).await;
                            return Ok(());
                        }
                    }
                }
                Ok(())
            });
            Ok(TtsStream {
                tx: Box::new(Tx { tx }),
                rx: Box::pin(rx),
            })
        })
    }
}

enum Command {
    Speak(String),
    Finish,
}

struct Tx {
    tx: Sender<Command>,
}

impl TtsTx for Tx {
    fn try_speak(&mut self, text: &str) -> Result<(), Full> {
        self.tx.try_send(Command::Speak(text.to_owned()))
    }

    fn try_finish(&mut self) -> Result<(), Full> {
        self.tx.try_send(Command::Finish)
    }
}

/// One stream's state.
struct Voice {
    script: Arc<TtsScript>,
    rate: Rate,
    events: Events<TtsEvent>,
    /// Bytes of text spoken so far.
    text: u32,
    /// Samples given so far.
    audio: usize,
    /// Pieces of audio given so far.
    given: usize,
    /// How long the next piece of audio takes to make. `None` before the
    /// first.
    wait: Option<Duration>,
}

impl Voice {
    /// Speaks a piece of text: gives its audio, a little at a time, and
    /// the marks its alignment promises.
    async fn speak(&mut self, text: &str) -> Result<(), PortError> {
        let per_char = self.rate.samples_in(self.script.per_char);
        // Where each mark falls, in samples from the start of this piece.
        let mut marks: Vec<(u32, usize)> = Vec::new();
        let mut samples = 0;
        let mut bytes = 0_u32;
        let mut in_word = false;
        let bare = self.script.bare_words && self.script.caps.alignment == Alignment::Word;
        // Where the word in hand begins and ends, as far as it has been
        // read.
        let mut word_start = 0;
        let mut word_end = (0, 0);
        let marked = |from: u32, to: u32| {
            let word = text.get(from as usize..to as usize).unwrap_or_default();
            let skipped = self.script.unmarked.as_deref();
            !skipped.is_some_and(|skipped| word.contains(skipped))
        };
        for character in text.chars() {
            let ends_word = in_word && character.is_whitespace();
            if bare && ends_word && marked(word_start, word_end.0) {
                marks.push(word_end);
            }
            if !in_word && !character.is_whitespace() {
                word_start = bytes;
            }
            samples += per_char;
            bytes += u32::try_from(character.len_utf8()).unwrap_or(0);
            in_word = !character.is_whitespace();
            if in_word {
                word_end = (bytes, samples);
            }
            match self.script.caps.alignment {
                Alignment::Char => marks.push((bytes, samples)),
                // A word is marked where the white space after it ends.
                Alignment::Word if ends_word && !bare => marks.push((bytes, samples)),
                _ => {}
            }
        }
        let whole = self.script.caps.input == TtsInput::Whole;
        let last_is_marked = marks.last().is_some_and(|(at, _)| *at == bytes);
        if bare {
            // The last word, if the piece ends in one. What follows it is
            // left unmarked.
            if in_word && marked(word_start, word_end.0) {
                marks.push(word_end);
            }
        } else if !last_is_marked && (self.script.caps.alignment != Alignment::None || whole) {
            // The end of the piece: its last word, or all that is known
            // of a piece that is spoken on its own.
            marks.push((bytes, samples));
        }

        let chunk = self.rate.samples_in(self.script.chunk).max(1);
        let mut marks = marks.into_iter().peekable();
        let mut done = 0;
        while done < samples {
            let now = chunk.min(samples - done);
            if let Some((after, error)) = &self.script.fail_after
                && self.given >= *after
            {
                return Err(error.clone());
            }
            let making = self.rate.length_of(now).as_millis() / self.script.speed.max(1);
            let wait = self
                .wait
                .replace(Duration::from_millis(u64::from(making)))
                .unwrap_or(self.script.first);
            tokio::time::sleep(wait).await;
            // The marks that fall in this piece come before it.
            while let Some((text_end, at)) = marks.next_if(|(_, at)| *at <= done + now) {
                let mark = TtsMark {
                    text_end: self.text + text_end,
                    audio_end: self.rate.length_of(self.audio + at),
                };
                self.events.send(TtsEvent::Mark(mark)).await;
            }
            let pcm: Vec<i16> = (self.audio + done..self.audio + done + now)
                .map(tone)
                .collect();
            self.events
                .send(TtsEvent::Audio(PcmBytes::from_samples(&pcm)))
                .await;
            self.given += 1;
            done += now;
        }
        self.text += bytes;
        self.audio += samples;
        Ok(())
    }
}

/// Sample `n` of the tone a scripted synthesiser speaks in: never
/// silent, so that speech can be told from the silence around it.
#[must_use]
pub fn tone(n: usize) -> i16 {
    // A triangle wave with a period of 64 samples, between 2,000 and 8,000
    // away from zero.
    let phase = i16::try_from(n % 64).unwrap_or(0);
    let ramp = if phase < 32 { phase } else { 63 - phase };
    let level = 2_000 + ramp * 190;
    if n % 128 < 64 { level } else { -level }
}
