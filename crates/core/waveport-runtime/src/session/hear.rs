//! How a session hears one participant.
//!
//! The packets of a participant go through the inbound path, which puts
//! them in order and decodes them. What comes out goes two ways: to the
//! listening models, which say when the participant speaks, and to the
//! speech recogniser, which says what.
//!
//! The two are fed differently, because they fail differently. The models
//! are local and are given every frame as it comes; a frame they have no
//! room for is lost. The recogniser is far away and may be slow, or gone:
//! its audio waits in a queue of two seconds, of which the oldest is lost
//! when nobody takes it out. A recogniser that fails is opened again, a
//! little later each time, and the new one is given what was heard since
//! the last transcript the old one gave as final, as far back as the
//! inbound path remembers: words the old one took with it are not lost.
//! When opening it again has failed too many times in a row the session
//! is told to close, because a session that cannot hear is of no use.

use core::task::{Context, Poll};
use core::time::Duration;
use std::sync::Arc;

use waveport_domain::event::{CloseReason, ControlInput, Event};
use waveport_domain::ids::{ParticipantId, TurnId};
use waveport_domain::time::{MediaTime, MonoTime};
use waveport_media::codec::CodecError;
use waveport_media::ingress::{Ingress, IngressConfig, Packet};
use waveport_media::resample::Resampler;
use waveport_ports::audio::{AudioRef, Rate};
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::plan::ListenPlan;
use waveport_ports::stt::{Stt, SttConfig, SttEvent, SttStream};

use crate::compute::{Listener, Sensed};
use crate::edges::{EdgeConfig, Edges};

/// How much audio the recogniser is handed at a time, at most.
const CHUNK_MS: u32 = 100;

/// How many times in a row a recogniser is opened again before the
/// participant is given up as unheard.
const MAX_FAILURES: u32 = 6;

/// How long after its first failure a recogniser is opened again. The
/// wait doubles with each failure that follows, up to [`LONGEST_WAIT`].
const FIRST_WAIT: Duration = Duration::from_millis(200);
const LONGEST_WAIT: Duration = Duration::from_secs(5);

type Opening = BoxFuture<'static, Result<SttStream, PortError>>;

enum Recogniser {
    Opening(Opening),
    Open(SttStream),
    /// Failed, and to be opened again at this time.
    Waiting(MonoTime),
    /// Given up, or closed.
    Off,
}

/// Counts of what went wrong while hearing a participant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HearStats {
    /// Times the recogniser failed or ended by itself.
    pub recogniser_failures: u32,
    /// Requests for a final transcript that the recogniser had no room
    /// for.
    pub finalize_refused: u32,
}

/// One participant, as the session hears them.
pub(crate) struct Ear {
    pub who: ParticipantId,
    ingress: Ingress,
    listener: Listener,
    edges: Edges,
    rate: Rate,
    recogniser: Recogniser,
    stt: Arc<dyn Stt>,
    config: SttConfig,
    ask_to_finalize: bool,
    /// Brings audio to the rate the recogniser takes.
    resampler: Resampler,
    /// Audio out of the queue, at the rate it has there.
    queued: Vec<i16>,
    /// Audio for the recogniser that it has not taken yet.
    carry: Vec<i16>,
    carry_len: usize,
    carry_at: MediaTime,
    /// Where the last transcript that a recogniser gave as final ends.
    transcribed: MediaTime,
    /// Audio from before a recogniser was opened, for it to be given
    /// first: how much of it has been handed on, and where it ends.
    backlog: Vec<i16>,
    backlog_given: usize,
    backlog_end: MediaTime,
    /// When the inbound path is to be looked at again if no packet comes.
    due: Option<MonoTime>,
    failures: u32,
    stats: HearStats,
}

impl Ear {
    /// A participant who has just joined. Their recogniser is being
    /// opened.
    pub(crate) fn new(
        who: ParticipantId,
        plan: ListenPlan,
        origin: MonoTime,
        listener: Listener,
        edges: EdgeConfig,
        stt: Arc<dyn Stt>,
        language: Option<String>,
    ) -> Result<Self, CodecError> {
        let ingress = Ingress::new(IngressConfig {
            rate: plan.rate,
            origin,
            ..IngressConfig::new(plan.wire)
        })?;
        let resampler = Resampler::new(plan.rate, plan.recogniser_rate);
        let chunk = plan.rate.samples_in_millis(CHUNK_MS);
        let config = SttConfig {
            language,
            ..SttConfig::new(plan.recogniser_rate)
        };
        let mut ear = Self {
            who,
            ingress,
            listener,
            edges: Edges::new(edges),
            rate: plan.rate,
            recogniser: Recogniser::Off,
            stt,
            config,
            ask_to_finalize: plan.ask_to_finalize,
            carry: vec![0; resampler.yields(chunk) + 8],
            resampler,
            queued: vec![0; chunk],
            carry_len: 0,
            carry_at: MediaTime::from_ticks(0),
            transcribed: MediaTime::from_ticks(0),
            backlog: Vec::new(),
            backlog_given: 0,
            backlog_end: MediaTime::from_ticks(0),
            due: None,
            failures: 0,
            stats: HearStats::default(),
        };
        ear.open();
        Ok(ear)
    }

    fn open(&mut self) {
        let (stt, config) = (Arc::clone(&self.stt), self.config.clone());
        self.recogniser = Recogniser::Opening(Box::pin(async move { stt.open(config).await }));
        // Whatever was on its way to the stream before is not for this
        // one.
        self.resampler.reset();
        self.carry_len = 0;
        self.backlog = Vec::new();
        self.backlog_given = 0;
    }

    /// Sets aside, for a recogniser that has just been opened, what was
    /// heard since the last final transcript: all of it that the inbound
    /// path still has. It takes the place of what waits in the queue,
    /// which is the end of the same audio.
    fn catch_up(&mut self) {
        while self.ingress.take_queued(&mut self.queued).1 > 0 {}
        let end = self.ingress.end();
        let window = self.ingress.window();
        let missed = self.rate.samples_in(end.since(self.transcribed));
        self.backlog.resize(missed.min(window.len()), 0);
        let copied = window.latest(&mut self.backlog);
        self.backlog.truncate(copied);
        self.backlog_given = 0;
        self.backlog_end = end;
    }

    /// What went wrong so far.
    pub(crate) const fn stats(&self) -> HearStats {
        self.stats
    }

    /// Takes a packet of the participant.
    pub(crate) fn packet(&mut self, now: MonoTime, packet: Packet<'_>) {
        self.ingress.push(now, packet);
    }

    /// Asks the recogniser for a final transcript of what it has heard,
    /// if it is one that can be asked.
    pub(crate) fn finalize(&mut self) {
        if self.ask_to_finalize
            && let Recogniser::Open(stream) = &mut self.recogniser
            && stream.tx.try_finalize().is_err()
        {
            self.stats.finalize_refused += 1;
        }
    }

    /// Asks the models about a turn of the participant that began at
    /// `began`: whether it is over, or, with `overlap`, what its speech
    /// over the agent's is. They are given the audio since then, as much
    /// of it as `window` samples hold.
    pub(crate) fn judge(&mut self, turn: TurnId, began: MediaTime, window: usize, overlap: bool) {
        let since = self.rate.samples_in(self.ingress.end().since(began));
        let mut audio = vec![0; since.min(window).min(self.ingress.window().len())];
        let copied = self.ingress.window().latest(&mut audio);
        audio.truncate(copied);
        if overlap {
            self.listener.classify_overlap(turn, audio);
        } else {
            self.listener.estimate_eou(turn, audio);
        }
    }

    /// When to look at this participant again if nothing wakes the
    /// session before.
    pub(crate) fn deadline(&self) -> Option<MonoTime> {
        match (&self.recogniser, self.due) {
            (Recogniser::Waiting(at), Some(due)) => Some(due.min(*at)),
            (Recogniser::Waiting(at), None) => Some(*at),
            (_, due) => due,
        }
    }

    /// Moves everything on that can move, and adds to `events` what the
    /// kernel is to be told. Returns whether anything changed that may
    /// have made more work.
    pub(crate) fn poll(
        &mut self,
        cx: &mut Context<'_>,
        now: MonoTime,
        events: &mut Vec<Event>,
    ) -> bool {
        let who = self.who;
        let mut changed = false;

        // Audio in order goes to the models as it comes.
        let listener = &mut self.listener;
        self.due = self.ingress.poll(now, |heard| {
            listener.push(heard.at, heard.pcm, heard.begins);
        });
        while let Poll::Ready(sensed) = self.listener.poll(cx) {
            match sensed {
                Sensed::Speech { at, length, p } => {
                    self.edges.push(at, length, p, |edge, at| {
                        events.push(Event::Speech { who, edge, at });
                    });
                }
                Sensed::Eou { turn, p } => events.push(Event::Eou { turn, p }),
                Sensed::Overlap { turn, verdict } => events.push(Event::Overlap { turn, verdict }),
            }
        }

        // The recogniser is opened, read, and opened again when it fails.
        match &mut self.recogniser {
            Recogniser::Opening(opening) => {
                if let Poll::Ready(opened) = opening.as_mut().poll(cx) {
                    changed = true;
                    match opened {
                        Ok(stream) => {
                            self.recogniser = Recogniser::Open(stream);
                            self.catch_up();
                        }
                        Err(error) => self.failed(now, error.is_retryable(), events),
                    }
                }
            }
            Recogniser::Open(stream) => loop {
                match stream.rx.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(SttEvent::Transcript(transcript)))) => {
                        self.failures = 0;
                        if transcript.is_final && transcript.span.end > self.transcribed {
                            self.transcribed = transcript.span.end;
                        }
                        events.push(Event::Transcript {
                            who,
                            span: transcript.span,
                            text: transcript.text.into(),
                            is_final: transcript.is_final,
                        });
                    }
                    Poll::Ready(Some(Err(error))) => {
                        changed = true;
                        self.failed(now, error.is_retryable(), events);
                        break;
                    }
                    // A stream that ends while the participant is there
                    // has failed, whatever it says.
                    Poll::Ready(None) => {
                        changed = true;
                        self.failed(now, true, events);
                        break;
                    }
                    Poll::Pending => break,
                }
            },
            Recogniser::Waiting(at) => {
                if *at <= now {
                    changed = true;
                    self.open();
                }
            }
            Recogniser::Off => {}
        }

        self.feed();
        changed
    }

    /// Hands the recogniser the audio that waits for it, for as long as
    /// it takes any.
    fn feed(&mut self) {
        let Recogniser::Open(stream) = &mut self.recogniser else {
            return;
        };
        loop {
            if self.carry_len == 0 {
                // What was heard before the stream was opened goes first.
                let waiting = self.backlog.len() - self.backlog_given;
                let (at, audio) = if waiting > 0 {
                    let chunk = waiting.min(self.queued.len());
                    let behind = self.rate.length_of(waiting);
                    let at = self
                        .backlog_end
                        .ticks()
                        .saturating_sub(u64::from(behind.ticks()));
                    let from = self.backlog_given;
                    self.backlog_given += chunk;
                    let audio = self.backlog.get(from..from + chunk);
                    (MediaTime::from_ticks(at), audio)
                } else {
                    // None of it is needed again.
                    self.backlog = Vec::new();
                    self.backlog_given = 0;
                    let (at, taken) = self.ingress.take_queued(&mut self.queued);
                    (at, self.queued.get(..taken))
                };
                let Some(audio) = audio.filter(|audio| !audio.is_empty()) else {
                    return;
                };
                // The room is for a whole chunk, so this does not fail.
                self.carry_len = self.resampler.process(audio, &mut self.carry).unwrap_or(0);
                self.carry_at = at;
                if self.carry_len == 0 {
                    continue;
                }
            }
            let audio = AudioRef {
                at: self.carry_at,
                rate: self.config.rate,
                samples: self.carry.get(..self.carry_len).unwrap_or_default(),
            };
            if stream.tx.try_send(audio).is_err() {
                // It is behind. What it did not take is kept, and what
                // comes meanwhile waits in the queue.
                return;
            }
            self.carry_len = 0;
        }
    }

    fn failed(&mut self, now: MonoTime, worth_another_try: bool, events: &mut Vec<Event>) {
        self.stats.recogniser_failures += 1;
        self.failures += 1;
        self.recogniser = if worth_another_try && self.failures <= MAX_FAILURES {
            let doublings = self.failures.saturating_sub(1).min(16);
            let wait = FIRST_WAIT.saturating_mul(1 << doublings).min(LONGEST_WAIT);
            Recogniser::Waiting(now.after(wait))
        } else {
            // The participant cannot be heard, and will not be.
            events.push(Event::Control(ControlInput::Close {
                reason: CloseReason::Failure,
            }));
            Recogniser::Off
        };
    }
}

impl core::fmt::Debug for Ear {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let recogniser = match self.recogniser {
            Recogniser::Opening(_) => "opening",
            Recogniser::Open(_) => "open",
            Recogniser::Waiting(_) => "waiting",
            Recogniser::Off => "off",
        };
        f.debug_struct("Ear")
            .field("who", &self.who)
            .field("recogniser", &recogniser)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}
