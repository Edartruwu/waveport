//! A speech recogniser that follows a script.
//!
//! It does not listen to the audio it is sent. It is told beforehand what
//! is said and when on the media clock, and it watches the clock of the
//! audio that arrives: when the audio has passed a word, it has "heard"
//! that word.

use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::time::Instant;

use waveport_adapter_kit::stream::{self, Events, Sender};
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
use waveport_ports::audio::{AudioRef, Rate, RateSet};
use waveport_ports::error::{BoxFuture, Full, PortError};
use waveport_ports::stt::{
    Stt, SttCaps, SttConfig, SttEvent, SttFinalize, SttStream, SttTiming, SttTx, Transcript,
};

/// Something said, and the stretch of the media clock it is said in. Its
/// words are taken to be spread evenly over the stretch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spoken {
    /// When.
    pub span: MediaSpan,
    /// What.
    pub text: String,
    /// In whose voice: the number a scripted microphone was given. A
    /// script with more than one voice in it is heard by each stream as
    /// far as it is in the voice of the stream's own audio.
    pub voice: u8,
}

impl Spoken {
    /// `text`, said from `from_ms` to `to_ms` on the media clock.
    #[must_use]
    pub fn new(from_ms: u64, to_ms: u64, text: &str) -> Self {
        Self {
            span: MediaSpan::new(
                MediaTime::from_millis(from_ms),
                MediaTime::from_millis(to_ms),
            ),
            text: text.to_owned(),
            voice: 0,
        }
    }

    /// The same, said in the voice numbered `voice`.
    #[must_use]
    pub const fn by(mut self, voice: u8) -> Self {
        self.voice = voice;
        self
    }
}

/// What a scripted recogniser does.
#[derive(Clone, Debug)]
pub struct SttScript {
    /// What it says it can do, which it then does.
    pub caps: SttCaps,
    /// How long after the audio that completes it, or the request that
    /// asks for it, a transcript is given.
    pub latency: Duration,
    /// How much silence after speech makes it give a final transcript by
    /// itself, when it does that at all.
    pub endpoint: MediaDur,
    /// What is said.
    pub speech: Vec<Spoken>,
    /// Opening a stream fails with this.
    pub fail_open: Option<PortError>,
    /// After this many transcripts the stream fails with this.
    pub fail_after: Option<(usize, PortError)>,
}

impl SttScript {
    /// A recogniser with nothing to wish for, which hears `speech`.
    #[must_use]
    pub fn new(speech: Vec<Spoken>) -> Self {
        Self {
            caps: SttCaps {
                rates: RateSet::of(&[Rate::Hz16000]),
                interim: true,
                timing: SttTiming::Words,
                finalize: SttFinalize::Both,
            },
            latency: Duration::from_millis(50),
            endpoint: MediaDur::from_millis(700),
            speech,
            fail_open: None,
            fail_after: None,
        }
    }
}

/// What a scripted recogniser was asked to do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SttLog {
    /// How each stream was set up.
    pub opened: Vec<SttConfig>,
    /// How much audio arrived in all, in ticks of the media clock.
    pub audio_ticks: u64,
    /// How many times a final transcript was asked for.
    pub finalize_requests: usize,
    /// How many streams were let go of in good order: read to their end
    /// after their sending half was dropped. A stream that is dropped
    /// whole is not among them.
    pub let_go: usize,
}

/// A speech recogniser that follows a script.
#[derive(Clone, Debug)]
pub struct ScriptedStt {
    script: Arc<SttScript>,
    log: Arc<Mutex<SttLog>>,
}

impl ScriptedStt {
    /// A recogniser that follows `script`.
    #[must_use]
    pub fn new(script: SttScript) -> Self {
        Self {
            script: Arc::new(script),
            log: Arc::default(),
        }
    }

    /// What it was asked to do so far.
    #[must_use]
    pub fn log(&self) -> SttLog {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Stt for ScriptedStt {
    fn caps(&self) -> SttCaps {
        self.script.caps
    }

    fn open(&self, config: SttConfig) -> BoxFuture<'_, Result<SttStream, PortError>> {
        Box::pin(async move {
            if let Some(error) = &self.script.fail_open {
                return Err(error.clone());
            }
            if !self.script.caps.rates.contains(config.rate) {
                return Err(PortError::permanent(format!(
                    "audio at {} Hz is not taken",
                    config.rate.hz()
                )));
            }
            self.log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .opened
                .push(config);
            let script = Arc::clone(&self.script);
            let log = Arc::clone(&self.log);
            let (tx, rx) = stream::pair(512, |mut commands, events| async move {
                let mut hearing = Hearing::new(script, events);
                while let Some(command) = commands.next().await {
                    match command {
                        Command::Audio {
                            at,
                            length,
                            peak,
                            sent,
                        } => {
                            log.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .audio_ticks += u64::from(length.ticks());
                            hearing.since = sent;
                            hearing.audio(at, length, peak).await?;
                        }
                        Command::Finalize { asked } => {
                            log.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .finalize_requests += 1;
                            hearing.since = asked;
                            hearing.finalize().await?;
                        }
                    }
                }
                log.lock().unwrap_or_else(PoisonError::into_inner).let_go += 1;
                Ok(())
            });
            Ok(SttStream {
                tx: Box::new(Tx { tx }),
                rx: Box::pin(rx),
            })
        })
    }
}

enum Command {
    Audio {
        at: MediaTime,
        length: MediaDur,
        /// The loudest sample.
        peak: u16,
        /// When it was sent.
        sent: Instant,
    },
    Finalize {
        /// When it was asked for.
        asked: Instant,
    },
}

/// Whose voice audio with this loudest sample is: a scripted microphone
/// speaks in a tone that is the quieter the higher the number of its
/// voice. `None` for silence.
fn voice_of(peak: u16) -> Option<u8> {
    match peak {
        0..1_000 => None,
        1_000..3_266 => Some(2),
        3_266..5_657 => Some(1),
        _ => Some(0),
    }
}

struct Tx {
    tx: Sender<Command>,
}

impl SttTx for Tx {
    fn try_send(&mut self, audio: AudioRef<'_>) -> Result<(), Full> {
        let peak = audio.samples.iter().map(|sample| sample.unsigned_abs());
        self.tx.try_send(Command::Audio {
            at: audio.at,
            length: audio.length(),
            peak: peak.max().unwrap_or(0),
            sent: Instant::now(),
        })
    }

    fn try_finalize(&mut self) -> Result<(), Full> {
        self.tx.try_send(Command::Finalize {
            asked: Instant::now(),
        })
    }
}

/// A word of the script and where it ends.
struct Word {
    voice: u8,
    text: String,
    start: MediaTime,
    end: MediaTime,
    /// Whether it is the last word of what was said in one go.
    last: bool,
}

/// One stream's state: how far the audio has got and what was given.
struct Hearing {
    script: Arc<SttScript>,
    events: Events<SttEvent>,
    words: Vec<Word>,
    /// Words the audio has passed.
    heard: usize,
    /// Words given in a final transcript.
    finalized: usize,
    /// Where the audio sent so far ends.
    end: MediaTime,
    /// Where the audio sent since the last final transcript begins.
    window: Option<MediaTime>,
    /// Transcripts given.
    given: usize,
    /// When what is being dealt with was sent: a transcript follows it
    /// by the script's latency, however long the stream was busy.
    since: Instant,
    /// Where the first audio began, once any has arrived.
    began: Option<MediaTime>,
    /// Whose voice the stream hears, once that is known: at once when
    /// the script has one voice, and otherwise from the first audio that
    /// is not silence.
    voice: Option<u8>,
}

impl Hearing {
    fn new(script: Arc<SttScript>, events: Events<SttEvent>) -> Self {
        let mut words = Vec::new();
        for spoken in &script.speech {
            let texts: Vec<&str> = spoken.text.split_whitespace().collect();
            let count = u64::try_from(texts.len()).unwrap_or(u64::MAX).max(1);
            let start = spoken.span.start.ticks();
            let length = u64::from(spoken.span.len().ticks());
            for (index, text) in texts.iter().enumerate() {
                let index = u64::try_from(index).unwrap_or(u64::MAX);
                words.push(Word {
                    voice: spoken.voice,
                    text: (*text).to_owned(),
                    start: MediaTime::from_ticks(start + length * index / count),
                    end: MediaTime::from_ticks(start + length * (index + 1) / count),
                    last: index + 1 == count,
                });
            }
        }
        let first = words.first().map_or(0, |word| word.voice);
        let one_voice = words.iter().all(|word| word.voice == first);
        Self {
            script,
            events,
            words,
            heard: 0,
            finalized: 0,
            end: MediaTime::ZERO,
            window: None,
            given: 0,
            since: Instant::now(),
            began: None,
            voice: one_voice.then_some(first),
        }
    }

    /// Keeps the words of the stream's own voice, and of those the ones
    /// that had not ended when its first audio began: what was said
    /// before that was not said to this stream.
    fn own(&mut self, voice: u8, began: MediaTime) {
        self.words.retain(|word| word.voice == voice);
        let before = self
            .words
            .iter()
            .take_while(|word| word.end <= began)
            .count();
        self.heard = before;
        self.finalized = before;
    }

    async fn audio(&mut self, at: MediaTime, length: MediaDur, peak: u16) -> Result<(), PortError> {
        if self.began.is_none() {
            self.began = Some(at);
            if let Some(voice) = self.voice {
                self.own(voice, at);
            }
        }
        let began = self.began.unwrap_or(at);
        self.window.get_or_insert(at);
        self.end = at.after(length);
        if self.voice.is_none() {
            // Whose words these are is not known before someone speaks.
            let Some(voice) = voice_of(peak) else {
                return Ok(());
            };
            self.voice = Some(voice);
            self.own(voice, began);
        }
        // Every word the audio has now passed is heard.
        while let Some(word) = self.words.get(self.heard) {
            if word.end > self.end {
                break;
            }
            self.heard += 1;
            if self.script.caps.interim {
                self.give(false).await?;
            }
        }
        // Silence after the end of what was said makes it final, for a
        // recogniser that decides that itself.
        let by_itself = self.script.caps.finalize != SttFinalize::OnRequest;
        let paused = self.heard > self.finalized
            && self
                .words
                .get(self.heard - 1)
                .is_some_and(|word| word.last && self.end >= word.end.after(self.script.endpoint));
        if by_itself && paused {
            self.give(true).await?;
        }
        Ok(())
    }

    async fn finalize(&mut self) -> Result<(), PortError> {
        if self.script.caps.finalize.on_request() {
            self.give(true).await?;
        }
        Ok(())
    }

    /// Gives a transcript of the words heard and not yet final.
    async fn give(&mut self, is_final: bool) -> Result<(), PortError> {
        if let Some((after, error)) = &self.script.fail_after
            && self.given >= *after
        {
            return Err(error.clone());
        }
        tokio::time::sleep_until(self.since + self.script.latency).await;
        let words = self.words.get(self.finalized..self.heard).unwrap_or(&[]);
        let text = words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let span = if let (SttTiming::Words, Some(first), Some(last)) =
            (self.script.caps.timing, words.first(), words.last())
        {
            MediaSpan::new(first.start, last.end)
        } else {
            let start = self.window.unwrap_or(self.end);
            MediaSpan::new(start, self.end.max(start))
        };
        if is_final {
            self.finalized = self.heard;
            self.window = None;
        }
        self.given += 1;
        self.events
            .send(SttEvent::Transcript(Transcript {
                span,
                text,
                is_final,
            }))
            .await;
        Ok(())
    }
}
