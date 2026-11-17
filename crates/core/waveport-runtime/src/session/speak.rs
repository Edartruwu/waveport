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
//!
//! Keys that the session is to press leave here as well. A transport
//! that presses keys is asked to. For one that has no way but the audio,
//! the tone of each key is made and goes out at the pace it is heard:
//! over what is being said while something is, in frames of its own
//! while nothing is.

use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::Arc;

use waveport_domain::audio::FRAME_MS;
use waveport_domain::event::{Event, Key, OpEvent, Part};
use waveport_domain::ids::OpTag;
use waveport_domain::time::{MediaDur, MonoTime};
use waveport_media::codec::opus::{LossWatch, OpusSettings};
use waveport_media::codec::{CodecError, Encoder};
use waveport_media::dtmf::{Dialer, MOST};
use waveport_media::ledger::{Mark, Spread};
use waveport_media::playout::{Playout, PlayoutConfig, Position};
use waveport_ports::audio::PcmBytes;
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::plan::{Marks, PlaybackPlan, SpeakPlan};
use waveport_ports::transport::{FrameOut, KeysOut, ToneOut, Transport, TransportControl};
use waveport_ports::tts::{Tts, TtsConfig, TtsEvent, TtsStream};

type Opening = BoxFuture<'static, Result<TtsStream, PortError>>;

/// How long a synthesiser that marks its words may give nothing, after it
/// has given audio for the text it has, before that text is taken to have
/// been spoken although its last word was never marked. A synthesiser
/// that names a word otherwise than it was written leaves it unmarked,
/// and the rest of the response would wait for that mark for ever.
const QUIET: Duration = Duration::from_millis(400);

/// How long a frame of audio lasts.
const FRAME: Duration = Duration::from_millis(FRAME_MS as u64);

enum Stream {
    Opening(Opening),
    Open(TtsStream),
    Over,
}

/// The synthesis of one response.
#[expect(
    clippy::struct_excessive_bools,
    reason = "four things that have each happened or not, in no fixed order"
)]
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
    /// Bytes of text the stream has been given, up to the end of the last
    /// word among them: the last letter or digit. A mark for a word can
    /// reach this far and no further, because the space or the full stop
    /// after the last word has no mark of its own.
    worded: u32,
    /// Bytes of text the audio so far is known to reach.
    marked: u32,
    /// Bytes of text the kernel was last told have audio.
    counted: u32,
    /// When the stream last gave anything.
    heard_at: MonoTime,
    /// Whether the stream has said that it has given everything.
    ended: bool,
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
            let last_word = text
                .char_indices()
                .rev()
                .find(|(_, character)| character.is_alphanumeric());
            if let Some((at, character)) = last_word {
                let end = u32::try_from(at + character.len_utf8()).unwrap_or(u32::MAX);
                self.worded = self.sent.saturating_add(end);
            }
            self.sent = self.sent.saturating_add(length);
            self.unsent.pop_front();
        }
        if self.finish && !self.finish_sent && self.unsent.is_empty() {
            self.finish_sent = stream.tx.try_finish().is_ok();
        }
        changed
    }

    /// How much of the text has audio that has arrived, as far as can be
    /// told at `now`. The kernel holds the rest of a response back until
    /// this has reached what it released, so this must not stay short of
    /// the truth for good.
    fn reached(&self, marks: Marks, now: MonoTime) -> u32 {
        match marks {
            // A synthesiser that gives no times does not say where it is
            // before it has finished, so its text counts from when audio
            // follows it: otherwise the kernel would wait for an end that
            // it has to send the rest of the text to bring about.
            Marks::Spread => self.marked.max(self.begun),
            // The last word that was given has been marked: so has what
            // follows it, which is nothing that is spoken.
            Marks::Timed if self.marked >= self.worded => self.marked.max(self.sent),
            // Audio followed the text, and then nothing did for a while:
            // the synthesiser has no more to give for it.
            Marks::Timed if self.stalled_at().is_some_and(|at| at <= now) => self.sent,
            Marks::Timed => self.marked,
        }
    }

    /// When a stream that marks its words is taken to have spoken what
    /// it was given although the last word is not marked, if that is what
    /// is being waited for.
    fn stalled_at(&self) -> Option<MonoTime> {
        let waiting = !self.ended
            && self.marked < self.worded
            && self.begun >= self.sent
            && self.counted < self.sent;
        waiting.then(|| self.heard_at.after(QUIET))
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
    /// Keys the session was asked to press and did not: its transport has
    /// no way to press one, more were waiting than there is room for, or
    /// they were still to press when the line dropped or the session
    /// stopped. A frame of a tone that the transport had no room for is
    /// counted with the frames that were refused.
    pub keys_unpressed: u64,
}

/// The voice of a session: synthesis, the outbound path and the encoder.
pub(crate) struct Voice {
    plan: SpeakPlan,
    playback: PlaybackPlan,
    tts: Arc<dyn Tts>,
    config: TtsConfig,
    playout: Playout,
    encoder: Encoder,
    /// What the far end says it lost, and what the encoder expects of
    /// the line for it.
    loss: LossWatch,
    speaking: Option<Speaking>,
    /// The position last told to the kernel.
    reported: Option<Position>,
    /// The response whose first frame has gone out.
    started: Option<OpTag>,
    /// The keys to press whose tones the session makes.
    dialer: Dialer,
    /// The keys the transport is still to be asked to press.
    to_press: VecDeque<Key>,
    /// When pressing goes on if nothing is said before: the next frame
    /// of a tone is due, or the transport is asked again.
    press_at: Option<MonoTime>,
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
            loss: LossWatch::default(),
            speaking: None,
            reported: None,
            started: None,
            dialer: Dialer::new(plan.rate),
            to_press: VecDeque::with_capacity(MOST),
            press_at: None,
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
        let stalled = match (&self.speaking, self.plan.marks) {
            (Some(speaking), Marks::Timed) => speaking.stalled_at(),
            _ => None,
        };
        let speech = match (self.playout.wake(), stalled) {
            (Some(wake), Some(stalled)) => Some(wake.min(stalled)),
            (wake, stalled) => wake.or(stalled),
        };
        speech.into_iter().chain(self.press_at).min()
    }

    /// The session is to press `keys`, after those it was asked to press
    /// before.
    pub(crate) fn press(&mut self, keys: &[Key], now: MonoTime) {
        for &key in keys {
            let taken = match self.plan.keys {
                Some(KeysOut::Tones) => self.dialer.press(key),
                Some(KeysOut::OutOfBand) if self.to_press.len() < MOST => {
                    self.to_press.push_back(key);
                    true
                }
                Some(KeysOut::OutOfBand) | None => false,
            };
            if taken {
                self.press_at.get_or_insert(now);
            } else {
                self.stats.keys_unpressed += 1;
            }
        }
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

    /// The far end says how much of what it was sent lately did not
    /// arrive. The encoder is told what to expect of the line when that
    /// changes.
    pub(crate) fn lost(&mut self, percent: u8) {
        if let Some(expects) = self.loss.told(percent) {
            // What a watch expects is in range.
            let _ = self.encoder.expect_loss(expects);
        }
    }

    /// The keys that are still to be pressed are not: nobody is there to
    /// hear them, or the session may say nothing more.
    pub(crate) fn forget_keys(&mut self) {
        let waiting = self.dialer.unsounded() + self.to_press.len();
        let waiting = u64::try_from(waiting).unwrap_or(u64::MAX);
        self.stats.keys_unpressed = self.stats.keys_unpressed.saturating_add(waiting);
        self.dialer.clear();
        self.to_press.clear();
        self.press_at = None;
    }

    /// The kernel releases a piece of the text of the response `tag`.
    /// Returns whether it is the first of that response.
    pub(crate) fn speak(&mut self, tag: OpTag, piece: &str) -> bool {
        let first = self
            .speaking
            .as_ref()
            .is_none_or(|speaking| speaking.tag != tag);
        if first {
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
                worded: 0,
                marked: 0,
                counted: 0,
                heard_at: MonoTime::ZERO,
                ended: false,
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
        first
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
        self.forget_keys();
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
        if !matches!(speaking.stream, Stream::Open(_)) {
            return changed;
        }

        loop {
            let taken = hand_on(
                &mut speaking.held,
                &mut self.playout,
                tag,
                now,
                &mut self.scratch,
            );
            if taken > 0 {
                speaking.audio += taken;
            }
            // The kernel is told of audio that was taken, and of text
            // that is now known to have its audio.
            let text_end = speaking.reached(self.plan.marks, now);
            if !speaking.ended && (taken > 0 || text_end > speaking.counted) {
                changed = true;
                speaking.counted = speaking.counted.max(text_end);
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
            let Stream::Open(stream) = &mut speaking.stream else {
                return changed;
            };
            match stream.rx.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(TtsEvent::Audio(pcm)))) => {
                    if core::mem::take(&mut speaking.silent) {
                        milestone(Milestone::FirstAudio(tag));
                    }
                    speaking.heard_at = now;
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
                    speaking.heard_at = now;
                    speaking.marked = mark.text;
                }
                Poll::Ready(Some(Ok(TtsEvent::Done))) => {
                    speaking.ended = true;
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
        let mut over = false;
        while let Some(frame) = self.playout.poll(now, &mut self.frame) {
            sent = true;
            // A key that is being pressed sounds over what is being said.
            over |= self.dialer.mix(&mut self.frame) > 0;
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
        sent | self.press_on(now, transport, over)
    }

    /// Goes on pressing the keys that are to be pressed. `over` says that
    /// the tone of one went out just now, over what is being said.
    /// Returns whether anything went out.
    fn press_on(&mut self, now: MonoTime, transport: &mut dyn Transport, over: bool) -> bool {
        let mut sent = false;
        // The transport presses keys itself, and is asked for each. One
        // that has no room is asked again a frame later.
        while let Some(&key) = self.to_press.front() {
            if transport
                .try_control(TransportControl::Dtmf { key })
                .is_err()
            {
                self.press_at = Some(now.after(FRAME));
                return sent;
            }
            self.to_press.pop_front();
            sent = true;
        }
        if self.dialer.is_idle() {
            self.press_at = None;
            return sent;
        }
        if over {
            // The tone goes on in a frame of its own when the frame it
            // went out in has been heard, unless more is said by then.
            self.press_at = Some(now.after(FRAME));
            return sent;
        }
        if self.playout.buffered() > 0 && self.playout.wake().is_some() {
            // More of what is being said is on its way out, and the tone
            // goes with it: the outbound path says when.
            self.press_at = None;
            return sent;
        }
        // The tones of the keys, in frames of their own while nothing is
        // being said, at the pace they are heard. A session that was not
        // run for a while does not send what it missed in a burst.
        let mut due = self.press_at.unwrap_or(now);
        if now.since(due) > 2 * FRAME {
            due = now;
        }
        let rate = self.plan.rate;
        while due <= now && !self.dialer.is_idle() {
            self.frame.fill(0);
            self.dialer.mix(&mut self.frame);
            due = due.after(FRAME);
            let Ok(len) = self.encoder.encode(&self.frame, &mut self.packet) else {
                self.stats.frames_unencodable += 1;
                continue;
            };
            let tone = ToneOut {
                length: rate.length_of(self.frame.len()),
                payload: self.packet.get(..len).unwrap_or_default(),
            };
            if transport.try_send_tone(tone).is_err() {
                self.stats.frames_refused += 1;
            }
            sent = true;
        }
        self.press_at = (!self.dialer.is_idle()).then_some(due);
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
