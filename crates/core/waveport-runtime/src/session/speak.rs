//! How a session speaks.
//!
//! Text the kernel releases goes to a speech synthesiser, and the audio
//! that comes back goes into the outbound path, which hands it to the
//! transport a frame at a time at the pace it is heard. The synthesiser
//! is faster than speech, so the outbound path fills: when it has no room
//! the audio in hand is kept and the synthesiser is not read, which is
//! all the pressure it needs.
//!
//! One response is spoken at a time. Text for another response ends the
//! stream of the one before, and so does a cancellation: a stream that is
//! dropped is cancelled.

use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::sync::Arc;

use waveport_domain::event::{Event, OpEvent, Part};
use waveport_domain::ids::OpTag;
use waveport_domain::time::{MediaDur, MonoTime};
use waveport_media::codec::opus::OpusSettings;
use waveport_media::codec::{CodecError, Encoder};
use waveport_media::ledger::{Mark, Spread};
use waveport_media::playout::{Playout, PlayoutConfig, Position};
use waveport_ports::audio::PcmBytes;
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::plan::{Marks, PlaybackPlan, SpeakPlan};
use waveport_ports::transport::{FrameOut, Transport, TransportControl};
use waveport_ports::tts::{Tts, TtsConfig, TtsEvent, TtsStream};

type Opening = BoxFuture<'static, Result<TtsStream, PortError>>;

enum Stream {
    Opening(Opening),
    Open(TtsStream),
    Over,
}

/// The synthesis of one response.
struct Speaking {
    tag: OpTag,
    stream: Stream,
    /// Text the kernel released that the stream has not been given.
    unsent: VecDeque<String>,
    /// Whether the kernel has said that no more text follows, and whether
    /// the stream has been told.
    finish: bool,
    finish_sent: bool,
    /// Bytes of text released so far.
    text: u32,
    /// Bytes of text the stream has been given.
    sent: u32,
    /// Bytes of text the audio so far is known to reach.
    marked: u32,
    /// Bytes of text the stream is taken to have begun on: what it had
    /// been given when audio last arrived. For a synthesiser that says
    /// nothing of where its words are, this is all that is known of how
    /// far it has got before it has finished.
    begun: u32,
    /// Samples handed to the outbound path so far.
    audio: usize,
    /// Audio the outbound path had no room for, and how much of it has
    /// been handed over.
    held: Option<(PcmBytes, usize)>,
    /// Whether no audio has arrived yet.
    silent: bool,
}

impl Speaking {
    /// Sees the stream opened and gives it the text it is owed, as much
    /// as it takes. Returns whether the stream was opened or failed.
    fn send_text(&mut self, cx: &mut Context<'_>, events: &mut Vec<Event>) -> bool {
        let mut changed = false;
        if let Stream::Opening(opening) = &mut self.stream
            && let Poll::Ready(opened) = opening.as_mut().poll(cx)
        {
            changed = true;
            self.stream = match opened {
                Ok(stream) => Stream::Open(stream),
                Err(error) => {
                    events.push(failed(self.tag, &error));
                    Stream::Over
                }
            };
        }
        let Stream::Open(stream) = &mut self.stream else {
            return changed;
        };
        while let Some(text) = self.unsent.front() {
            if stream.tx.try_speak(text).is_err() {
                break;
            }
            let length = u32::try_from(text.len()).unwrap_or(u32::MAX);
            self.sent = self.sent.saturating_add(length);
            self.unsent.pop_front();
        }
        if self.finish && !self.finish_sent && self.unsent.is_empty() {
            self.finish_sent = stream.tx.try_finish().is_ok();
        }
        changed
    }
}

/// What the kernel is told when the synthesis of a response fails.
fn failed(tag: OpTag, error: &PortError) -> Event {
    let failed = OpEvent::Failed {
        part: Part::Speak,
        class: error.class,
    };
    Event::Op(tag, failed)
}

/// What happened while speaking that someone may want to time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Milestone {
    /// The first audio of a response arrived from the synthesiser.
    FirstAudio(OpTag),
    /// The first frame of a response was handed to the transport.
    FirstFrame(OpTag),
}

/// Counts of what went wrong while speaking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpeakStats {
    /// Frames the transport had no room for.
    pub frames_refused: u64,
    /// Frames that could not be encoded.
    pub frames_unencodable: u64,
}

/// The voice of a session: synthesis, the outbound path and the encoder.
pub(crate) struct Voice {
    plan: SpeakPlan,
    playback: PlaybackPlan,
    tts: Arc<dyn Tts>,
    config: TtsConfig,
    playout: Playout,
    encoder: Encoder,
    speaking: Option<Speaking>,
    /// The position last told to the kernel.
    reported: Option<Position>,
    /// The response whose first frame has gone out.
    started: Option<OpTag>,
    frame: Vec<i16>,
    packet: Vec<u8>,
    scratch: Vec<i16>,
    stats: SpeakStats,
}

impl Voice {
    pub(crate) fn new(
        plan: SpeakPlan,
        playback: PlaybackPlan,
        tts: Arc<dyn Tts>,
        language: Option<String>,
    ) -> Result<Self, CodecError> {
        let encoder = Encoder::new(plan.wire, plan.rate, OpusSettings::default())?;
        let config = TtsConfig {
            language,
            ..TtsConfig::new(plan.rate)
        };
        Ok(Self {
            plan,
            playback,
            tts,
            config,
            playout: Playout::new(PlayoutConfig::new(plan.rate)),
            encoder,
            speaking: None,
            reported: None,
            started: None,
            frame: vec![0; plan.rate.frame()],
            packet: vec![0; plan.wire.max_packet()],
            scratch: vec![0; plan.rate.frame() * 10],
            stats: SpeakStats::default(),
        })
    }

    /// What went wrong so far.
    pub(crate) const fn stats(&self) -> SpeakStats {
        self.stats
    }

    /// When the outbound path is to be looked at again if nothing wakes
    /// the session before.
    pub(crate) fn deadline(&self) -> Option<MonoTime> {
        self.playout.wake()
    }

    /// How far playback has got, if that has changed since the kernel was
    /// last told.
    pub(crate) fn position(&mut self, now: MonoTime) -> Option<Event> {
        let position = self.playout.position(now)?;
        if self.reported == Some(position) {
            return None;
        }
        self.reported = Some(position);
        let played = OpEvent::Played {
            handed: position.handed,
            acked: position.played,
            heard_text: position.heard_text,
            drained: position.drained,
        };
        Some(Event::Op(position.tag, played))
    }

    /// The far end says how much of a response it has played.
    pub(crate) fn acked(&mut self, tag: OpTag, played: MediaDur) {
        if self.playback.far_end_reports {
            self.playout.ack(tag, played);
        }
    }

    /// The kernel releases a piece of the text of the response `tag`.
    pub(crate) fn speak(&mut self, tag: OpTag, piece: &str) {
        if self
            .speaking
            .as_ref()
            .is_none_or(|speaking| speaking.tag != tag)
        {
            let (tts, config) = (Arc::clone(&self.tts), self.config.clone());
            let opening = async move { tts.open(config).await };
            self.speaking = Some(Speaking {
                tag,
                stream: Stream::Opening(Box::pin(opening)),
                unsent: VecDeque::new(),
                finish: false,
                finish_sent: false,
                text: 0,
                sent: 0,
                marked: 0,
                begun: 0,
                audio: 0,
                held: None,
                silent: true,
            });
        }
        if let Some(speaking) = &mut self.speaking {
            speaking.text = speaking
                .text
                .saturating_add(u32::try_from(piece.len()).unwrap_or(u32::MAX));
            speaking.unsent.push_back(piece.to_owned());
        }
    }

    /// No more text of the response `tag` follows.
    pub(crate) fn finish(&mut self, tag: OpTag) {
        if let Some(speaking) = &mut self.speaking
            && speaking.tag == tag
        {
            speaking.finish = true;
        }
    }

    /// The synthesis of the response `tag` is not wanted any more.
    pub(crate) fn cancel(&mut self, tag: OpTag) {
        self.speaking.take_if(|speaking| speaking.tag == tag);
    }

    pub(crate) fn start(&mut self, tag: OpTag, now: MonoTime) {
        // A response that is not the newest any more is not played, and
        // the kernel knows that it is not.
        let _ = self.playout.start(tag, now);
    }

    pub(crate) fn duck(&mut self, tag: OpTag) {
        self.playout.duck(tag);
    }

    pub(crate) fn unduck(&mut self, tag: OpTag) {
        self.playout.unduck(tag);
    }

    /// Stops playing the response `tag`. The fade goes out at once, as
    /// the last of the response, and only then is the far end told to let
    /// go of what it holds.
    pub(crate) fn stop(
        &mut self,
        tag: OpTag,
        fade_ms: u16,
        now: MonoTime,
        transport: &mut dyn Transport,
    ) {
        self.playout.stop(tag, fade_ms);
        self.send_frames(now, transport, &mut |_| {});
        if self.playback.clear_on_stop {
            // A transport that has no room for this has no room for audio
            // either, and holds nothing to let go of.
            let _ = transport.try_control(TransportControl::Clear { tag });
        }
    }

    /// Stops whatever is being synthesised and played, unasked: for a
    /// session that may say nothing more.
    pub(crate) fn halt(&mut self, fade_ms: u16, now: MonoTime, transport: &mut dyn Transport) {
        self.speaking = None;
        if let Some(position) = self.playout.position(now)
            && !position.drained
        {
            self.stop(position.tag, fade_ms, now, transport);
        }
    }

    /// Moves synthesis on: gives the stream the text it is owed and takes
    /// the audio the outbound path has room for. Returns whether anything
    /// changed that may have made more work.
    pub(crate) fn poll(
        &mut self,
        cx: &mut Context<'_>,
        now: MonoTime,
        events: &mut Vec<Event>,
        milestone: &mut impl FnMut(Milestone),
    ) -> bool {
        let Some(speaking) = &mut self.speaking else {
            return false;
        };
        let tag = speaking.tag;
        let rate = self.plan.rate;
        let mut changed = speaking.send_text(cx, events);
        let Stream::Open(stream) = &mut speaking.stream else {
            return changed;
        };

        loop {
            let taken = hand_on(
                &mut speaking.held,
                &mut self.playout,
                tag,
                now,
                &mut self.scratch,
            );
            if taken > 0 {
                changed = true;
                speaking.audio += taken;
                // The kernel holds text back until the audio of what it
                // released has arrived. A synthesiser that gives no times
                // does not say when that is before it has finished, so
                // its text counts from when audio follows it: otherwise
                // the kernel would wait for an end that it has to send
                // the rest of the text to bring about.
                let text_end = match self.plan.marks {
                    Marks::Timed => speaking.marked,
                    Marks::Spread => speaking.marked.max(speaking.begun),
                };
                let segment = OpEvent::TtsSegment {
                    text_end,
                    audio_end: rate.length_of(speaking.audio),
                    last: false,
                };
                events.push(Event::Op(tag, segment));
            }
            if speaking.held.is_some() {
                return changed;
            }
            match stream.rx.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(TtsEvent::Audio(pcm)))) => {
                    if core::mem::take(&mut speaking.silent) {
                        milestone(Milestone::FirstAudio(tag));
                    }
                    speaking.begun = speaking.sent;
                    speaking.held = Some((pcm, 0));
                }
                Poll::Ready(Some(Ok(TtsEvent::Mark(mark)))) => {
                    let spread = match self.plan.marks {
                        Marks::Timed => Spread::AtOnce,
                        Marks::Spread => Spread::Evenly,
                    };
                    let mark = Mark {
                        text: mark.text_end,
                        audio: mark.audio_end,
                        spread,
                    };
                    let _ = self.playout.mark(tag, mark);
                    speaking.marked = mark.text;
                }
                Poll::Ready(Some(Ok(TtsEvent::Done))) => {
                    let audio = rate.length_of(speaking.audio);
                    if speaking.marked < speaking.text {
                        // The synthesiser said nothing about where the
                        // rest of the text is: it is in what is left of
                        // the audio, somewhere.
                        let rest = Mark {
                            text: speaking.text,
                            audio,
                            spread: Spread::Evenly,
                        };
                        let _ = self.playout.mark(tag, rest);
                        speaking.marked = speaking.text;
                    }
                    let _ = self.playout.finish(tag);
                    let segment = OpEvent::TtsSegment {
                        text_end: speaking.text,
                        audio_end: audio,
                        last: true,
                    };
                    events.push(Event::Op(tag, segment));
                    changed = true;
                }
                Poll::Ready(Some(Err(error))) => {
                    events.push(failed(tag, &error));
                    speaking.stream = Stream::Over;
                    return true;
                }
                Poll::Ready(None) => {
                    speaking.stream = Stream::Over;
                    return true;
                }
                Poll::Pending => return changed,
            }
        }
    }

    /// Hands the frames that are due to the transport. Returns whether
    /// there were any.
    pub(crate) fn send_frames(
        &mut self,
        now: MonoTime,
        transport: &mut dyn Transport,
        milestone: &mut impl FnMut(Milestone),
    ) -> bool {
        let rate = self.plan.rate;
        let mut sent = false;
        while let Some(frame) = self.playout.poll(now, &mut self.frame) {
            sent = true;
            let Ok(len) = self.encoder.encode(&self.frame, &mut self.packet) else {
                self.stats.frames_unencodable += 1;
                continue;
            };
            let out = FrameOut {
                tag: frame.tag,
                at: frame.at,
                length: rate.length_of(frame.samples),
                payload: self.packet.get(..len).unwrap_or_default(),
                last: frame.last,
            };
            // Audio drops: a transport that is behind does not get this
            // frame later.
            if transport.try_send(out).is_err() {
                self.stats.frames_refused += 1;
            } else if self.started != Some(frame.tag) {
                self.started = Some(frame.tag);
                milestone(Milestone::FirstFrame(frame.tag));
            }
        }
        sent
    }
}

/// Hands held audio to the outbound path, as much of it as there is room
/// for, and returns how many samples that was. What is left stays held.
fn hand_on(
    held: &mut Option<(PcmBytes, usize)>,
    playout: &mut Playout,
    tag: OpTag,
    now: MonoTime,
    scratch: &mut [i16],
) -> usize {
    let Some((pcm, mut done)) = held.take() else {
        return 0;
    };
    let mut handed = 0;
    while done < pcm.len() {
        let n = pcm.copy_to(done, scratch);
        if n == 0 {
            break;
        }
        // Audio of a response that was stopped is not wanted.
        let taken = playout
            .push(tag, now, scratch.get(..n).unwrap_or_default())
            .unwrap_or(n);
        done += taken;
        handed += taken;
        if taken < n {
            *held = Some((pcm, done));
            break;
        }
    }
    handed
}

impl core::fmt::Debug for Voice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Voice")
            .field(
                "speaking",
                &self.speaking.as_ref().map(|speaking| speaking.tag),
            )
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}
