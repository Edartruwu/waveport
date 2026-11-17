//! A transport with a far end that follows a script.
//!
//! The far end is a person with a microphone and a loudspeaker. What the
//! person does is scripted: when they are there, when they talk. The
//! loudspeaker plays what it is sent at the pace it is spoken, and, if
//! the transport is set up to, says how far it has got and discards what
//! it is told to.
//!
//! The line between the two can be scripted to drop and to come back.
//! While it is gone nothing gets through either way: what the far end
//! says is lost, and so is what it is sent.
//!
//! The person can be scripted to press keys of a telephone, and the
//! transport keeps the keys it is asked to press at the far end, or the
//! frames of their tones, whichever it is set up to say it takes.

use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use tokio::time::{Instant, Sleep};
use waveport_domain::event::Key;
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_domain::time::MediaDur;
use waveport_media::codec::Encoder;
use waveport_media::codec::opus::OpusSettings;
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::Full;
use waveport_ports::transport::{
    AudioCaps, FrameOut, KeysOut, RemoteBuffer, ToneOut, Transport, TransportCaps,
    TransportControl, TransportEvent,
};

use super::tts::tone;

/// How often the far end says how much it has played.
const REPORT_EVERY: Duration = Duration::from_millis(100);

/// A frame a fake transport was given to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentFrame {
    /// When, since the transport was made.
    pub when: Duration,
    /// The response it belongs to.
    pub tag: OpTag,
    /// Where in the response's audio it begins.
    pub at: MediaDur,
    /// How long it lasts.
    pub length: MediaDur,
    /// The packet.
    pub payload: Bytes,
    /// Whether it is the last frame of the response.
    pub last: bool,
}

/// A frame of no response that a fake transport was given to send: the
/// tone of a key, or the pause after one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentTone {
    /// When, since the transport was made.
    pub when: Duration,
    /// How long it lasts.
    pub length: MediaDur,
    /// The packet.
    pub payload: Bytes,
}

/// What a fake transport was asked to do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransportLog {
    /// The frames it was given to send.
    pub frames: Vec<SentFrame>,
    /// The responses it was told to discard the audio of, and when.
    pub cleared: Vec<(Duration, OpTag)>,
    /// What the far end had played of each response when it was last
    /// heard of.
    pub played: Vec<(OpTag, MediaDur)>,
    /// How many frames of a response were sent after the far end was told
    /// to discard it. The port does not allow any.
    pub late: usize,
    /// How many frames and requests were sent after the transport had
    /// said that it was closed.
    pub after_close: usize,
    /// How many times the transport was asked to tell the far end that
    /// the session is over.
    pub goodbyes: usize,
    /// How many times the line dropped.
    pub drops: usize,
    /// How many frames were sent while the line was gone. Nobody heard
    /// them. A session that is told its line is gone fades out what it
    /// was saying, which is one frame for each time the line drops.
    pub unheard: usize,
    /// The keys it was asked to press at the far end, and when.
    pub pressed: Vec<(Duration, Key)>,
    /// The frames of no response it was given to send: the tones of keys.
    pub tones: Vec<SentTone>,
}

impl TransportLog {
    /// How much of the audio of `tag` was sent.
    #[must_use]
    pub fn sent(&self, tag: OpTag) -> MediaDur {
        self.frames
            .iter()
            .filter(|frame| frame.tag == tag)
            .map(|frame| frame.at.saturating_add(frame.length))
            .max()
            .unwrap_or(MediaDur::ZERO)
    }

    /// How much of the audio of `tag` the far end played.
    #[must_use]
    pub fn heard(&self, tag: OpTag) -> MediaDur {
        self.played
            .iter()
            .rev()
            .find(|(played, _)| *played == tag)
            .map_or(MediaDur::ZERO, |(_, length)| *length)
    }
}

/// A look at what a fake transport was asked to do, for the test that
/// gave the transport away.
#[derive(Clone, Debug, Default)]
pub struct TransportHandle {
    log: Arc<Mutex<TransportLog>>,
}

impl TransportHandle {
    /// What the transport was asked to do so far.
    #[must_use]
    pub fn log(&self) -> TransportLog {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The loudspeaker at the far end, playing one response.
#[derive(Clone, Copy, Debug)]
struct Playing {
    tag: OpTag,
    /// When it began, or will begin, to be played.
    since: Instant,
    /// How much of its audio has arrived and was not discarded.
    received: MediaDur,
    /// How much the far end last said it had played.
    reported: Option<MediaDur>,
    /// Whether it was told to discard the response, or lost the line
    /// while it played it. It plays no more of it then, whatever arrives.
    cleared: bool,
}

impl Playing {
    /// How much has been played by `now`: what there was time for, of
    /// what arrived.
    fn played(&self, now: Instant) -> MediaDur {
        let elapsed = now.saturating_duration_since(self.since).as_micros() * 48 / 1_000;
        let elapsed = MediaDur::from_ticks(u32::try_from(elapsed).unwrap_or(u32::MAX));
        if elapsed < self.received {
            elapsed
        } else {
            self.received
        }
    }
}

/// Writes the script of a fake transport's far end.
#[derive(Clone, Debug)]
pub struct FakeTransportBuilder {
    caps: TransportCaps,
    events: Vec<(Duration, TransportEvent)>,
    closes: bool,
    clings: bool,
    holds: Duration,
}

impl FakeTransportBuilder {
    /// At `at_ms`, `who` is there.
    #[must_use]
    pub fn join(mut self, at_ms: u64, who: ParticipantId) -> Self {
        self.events
            .push((Duration::from_millis(at_ms), TransportEvent::Joined { who }));
        self
    }

    /// At `at_ms`, `who` is gone.
    #[must_use]
    pub fn leave(mut self, at_ms: u64, who: ParticipantId) -> Self {
        self.events
            .push((Duration::from_millis(at_ms), TransportEvent::Left { who }));
        self
    }

    /// At `at_ms` the line drops. The far end hears no more of what it
    /// was playing, and nothing it says or is sent gets through until the
    /// line [is back](Self::line_returns).
    #[must_use]
    pub fn line_drops(mut self, at_ms: u64) -> Self {
        self.events
            .push((Duration::from_millis(at_ms), TransportEvent::Detached));
        self
    }

    /// At `at_ms` the line is back.
    #[must_use]
    pub fn line_returns(mut self, at_ms: u64) -> Self {
        self.events
            .push((Duration::from_millis(at_ms), TransportEvent::Attached));
        self
    }

    /// From `at_ms` on, `who` presses `keys` of their telephone, one
    /// every 200 ms. A character that is no key is not pressed.
    #[must_use]
    pub fn keys(mut self, at_ms: u64, who: ParticipantId, keys: &str) -> Self {
        let pressed = keys.chars().filter_map(Key::from_char);
        for (at_ms, key) in (at_ms..).step_by(200).zip(pressed) {
            self.events.push((
                Duration::from_millis(at_ms),
                TransportEvent::Dtmf { who, key },
            ));
        }
        self
    }

    /// The transport says that keys the session presses reach the far
    /// end in the way `how`, and keeps what it is given for them.
    #[must_use]
    pub const fn pressing(mut self, how: KeysOut) -> Self {
        self.caps.keys = Some(how);
        self
    }

    /// At `at_ms`, `who` sends `text`.
    #[must_use]
    pub fn text(mut self, at_ms: u64, who: ParticipantId, text: &str) -> Self {
        let text = text.to_owned();
        self.events.push((
            Duration::from_millis(at_ms),
            TransportEvent::Text { who, text },
        ));
        self
    }

    /// From `from_ms` to `to_ms` the microphone of `who` is open: a
    /// packet arrives every 20 ms. It holds a tone inside the stretches
    /// of `talks`, each a start and an end in milliseconds, and silence
    /// outside them.
    ///
    /// # Panics
    ///
    /// The codec of the transport cannot be set up.
    #[must_use]
    pub fn microphone(
        self,
        who: ParticipantId,
        from_ms: u64,
        to_ms: u64,
        talks: &[(u64, u64)],
    ) -> Self {
        self.microphone_of(who, 0, from_ms, to_ms, talks)
    }

    /// A [`microphone`](Self::microphone) whose tone is in the voice
    /// numbered `voice`, from 0 to 2: the higher the number the quieter
    /// the tone, which is how a scripted recogniser tells who is
    /// speaking.
    ///
    /// # Panics
    ///
    /// The codec of the transport cannot be set up.
    #[must_use]
    pub fn microphone_of(
        mut self,
        who: ParticipantId,
        voice: u8,
        from_ms: u64,
        to_ms: u64,
        talks: &[(u64, u64)],
    ) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "a fake that cannot be set up fails the test that asked for it"
        )]
        let codec = self
            .caps
            .audio
            .expect("a far end with a microphone is on a transport that carries audio")
            .inbound;
        let rate = codec.clock();
        #[expect(
            clippy::expect_used,
            reason = "a fake that cannot be set up fails the test that asked for it"
        )]
        let mut encoder =
            Encoder::new(codec, rate, OpusSettings::default()).expect("the far end's encoder");
        let step = encoder.timestamp_step(rate.frame());
        let mut packet = vec![0; codec.max_packet()];
        let mut frame = vec![0_i16; rate.frame()];
        for (n, at_ms) in (from_ms..to_ms).step_by(20).enumerate() {
            let talking = talks.iter().any(|(from, to)| (*from..*to).contains(&at_ms));
            for (offset, sample) in frame.iter_mut().enumerate() {
                *sample = if talking {
                    tone(n * rate.frame() + offset) / (i16::from(voice.min(2)) + 1)
                } else {
                    0
                };
            }
            let len = encoder.encode(&frame, &mut packet).unwrap_or(0);
            let payload = Bytes::copy_from_slice(packet.get(..len).unwrap_or(&[]));
            let n = u32::try_from(n).unwrap_or(0);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "sequence numbers wrap round"
            )]
            let sequence = n as u16;
            self.events.push((
                Duration::from_millis(at_ms),
                TransportEvent::Audio {
                    who,
                    sequence,
                    timestamp: n.wrapping_mul(step),
                    payload,
                },
            ));
        }
        self
    }

    /// Changes when the packets written so far arrive: each at the time
    /// `arrival` makes of the time it was sent, or never, for `None`.
    /// Packets that arrive in another order than they were sent in are
    /// what a network does to them.
    #[must_use]
    pub fn disturbed(mut self, mut arrival: impl FnMut(Duration) -> Option<Duration>) -> Self {
        let mut kept = Vec::with_capacity(self.events.len());
        for (at, event) in self.events {
            if matches!(event, TransportEvent::Audio { .. }) {
                if let Some(at) = arrival(at) {
                    kept.push((at, event));
                }
            } else {
                kept.push((at, event));
            }
        }
        self.events = kept;
        self
    }

    /// The far end holds `ms` of each response before it plays any of it,
    /// as a buffer against jitter does. What it has played is then that
    /// far behind what it was sent.
    #[must_use]
    pub const fn holding(mut self, ms: u64) -> Self {
        self.holds = Duration::from_millis(ms);
        self
    }

    /// The transport closes once its script has run out, whatever the
    /// far end is playing then. Without this it stays open, with nothing
    /// more happening at the far end.
    #[must_use]
    pub const fn then_closes(mut self) -> Self {
        self.closes = true;
        self
    }

    /// The far end never answers a goodbye: the transport is never done
    /// telling it that the session is over.
    #[must_use]
    pub const fn never_lets_go(mut self) -> Self {
        self.clings = true;
        self
    }

    /// The transport, and a look at what it will be asked to do. The
    /// script's clock starts now.
    #[must_use]
    pub fn build(mut self) -> (FakeTransport, TransportHandle) {
        // In order of time; things at the same time stay in the order
        // they were written.
        self.events.sort_by_key(|(at, _)| *at);
        let handle = TransportHandle::default();
        let start = Instant::now();
        let transport = FakeTransport {
            caps: self.caps,
            start,
            script: self.events.into(),
            closes: self.closes,
            clings: self.clings,
            line: Line::There,
            holds: self.holds,
            playing: None,
            timer: Box::pin(tokio::time::sleep_until(start)),
            waker: None,
            log: Arc::clone(&handle.log),
        };
        (transport, handle)
    }
}

/// The line to the far end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Line {
    There,
    /// It has dropped and is not back.
    Gone,
    /// The transport has said that it is closed.
    Closed,
}

/// A transport with a far end that follows a script.
#[derive(Debug)]
pub struct FakeTransport {
    caps: TransportCaps,
    start: Instant,
    script: VecDeque<(Duration, TransportEvent)>,
    closes: bool,
    /// Whether it never finishes saying goodbye.
    clings: bool,
    line: Line,
    holds: Duration,
    playing: Option<Playing>,
    timer: Pin<Box<Sleep>>,
    /// Whoever waits for the next event, to be told when a frame gives
    /// the loudspeaker something to report.
    waker: Option<Waker>,
    log: Arc<Mutex<TransportLog>>,
}

impl FakeTransport {
    /// A sound card: plain samples at 48 kHz both ways, and a far end
    /// that says what it played and discards what it is told to.
    #[must_use]
    pub const fn sound_card() -> FakeTransportBuilder {
        Self::with(TransportCaps {
            audio: Some(AudioCaps {
                inbound: Codec::Pcm(Rate::Hz48000),
                outbound: Codec::Pcm(Rate::Hz48000),
                remote_buffer: RemoteBuffer {
                    clearable: true,
                    reports_played: true,
                },
            }),
            text: true,
            keys: None,
        })
    }

    /// A transport that says it can do `caps`, which it then does.
    #[must_use]
    pub const fn with(caps: TransportCaps) -> FakeTransportBuilder {
        FakeTransportBuilder {
            caps,
            events: Vec::new(),
            closes: false,
            clings: false,
            holds: Duration::ZERO,
        }
    }

    fn log(&self) -> std::sync::MutexGuard<'_, TransportLog> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the far end can do with audio it holds. One that carries no
    /// audio can do nothing with any.
    fn remote_buffer(&self) -> RemoteBuffer {
        self.caps
            .audio
            .map_or_else(RemoteBuffer::default, |audio| audio.remote_buffer)
    }

    /// Takes the next thing of the script that is due at `now` and gets
    /// through. What a microphone sends into a line that is gone, and what
    /// is typed into it, is lost.
    fn scripted(&mut self, now: Instant) -> Option<TransportEvent> {
        while let Some((at, _)) = self.script.front() {
            if self.start + *at > now {
                break;
            }
            let (_, event) = self.script.pop_front()?;
            match event {
                TransportEvent::Detached => {
                    self.line = Line::Gone;
                    self.log().drops += 1;
                    // The loudspeaker hears no more of what it was
                    // playing.
                    if let Some(playing) = &mut self.playing {
                        playing.received = playing.played(now);
                        playing.cleared = true;
                    }
                }
                TransportEvent::Attached => self.line = Line::There,
                TransportEvent::Audio { .. }
                | TransportEvent::Text { .. }
                | TransportEvent::Dtmf { .. }
                    if self.line == Line::Gone =>
                {
                    continue;
                }
                _ => {}
            }
            return Some(event);
        }
        None
    }

    /// What the loudspeaker has to report at `now`, if anything.
    fn report(&mut self, now: Instant) -> Option<TransportEvent> {
        if !self.remote_buffer().reports_played || self.line == Line::Gone {
            return None;
        }
        let playing = self.playing.as_mut()?;
        let played = playing.played(now);
        let due = match playing.reported {
            None => true,
            // The end is said at once; on the way there, every so often.
            Some(reported) => {
                played > reported
                    && (played == playing.received
                        || played.saturating_sub(reported).as_millis()
                            >= u32::try_from(REPORT_EVERY.as_millis()).unwrap_or(u32::MAX))
            }
        };
        if !due {
            return None;
        }
        playing.reported = Some(played);
        let tag = playing.tag;
        self.log().played.push((tag, played));
        Some(TransportEvent::Played { tag, played })
    }

    /// When the loudspeaker next has something to report.
    fn next_report(&self, now: Instant) -> Option<Instant> {
        if !self.remote_buffer().reports_played || self.line == Line::Gone {
            return None;
        }
        let playing = self.playing.as_ref()?;
        let reported = playing.reported.unwrap_or(MediaDur::ZERO);
        if reported >= playing.received {
            return None;
        }
        // The next report is a step on from the last, or the end if that
        // is nearer.
        let step = MediaDur::from_millis(u32::try_from(REPORT_EVERY.as_millis()).unwrap_or(100));
        let target = reported.saturating_add(step);
        let target = if target < playing.received {
            target
        } else {
            playing.received
        };
        let at = playing.since + Duration::from_micros(u64::from(target.ticks()) * 1_000 / 48);
        Some(at.max(now))
    }
}

impl Transport for FakeTransport {
    fn caps(&self) -> TransportCaps {
        self.caps
    }

    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>> {
        let this = self;
        let now = Instant::now();
        if let Some(event) = this.scripted(now) {
            return Poll::Ready(Some(event));
        }
        if this.line == Line::Closed || (this.closes && this.script.is_empty()) {
            // The far end is gone, whatever it was playing.
            this.line = Line::Closed;
            return Poll::Ready(None);
        }
        if let Some(event) = this.report(now) {
            return Poll::Ready(Some(event));
        }
        let scripted = this.script.front().map(|(at, _)| this.start + *at);
        let report = this.next_report(now);
        let next = match (scripted, report) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let Some(next) = next else {
            // Nothing is due. A frame that is sent may change that.
            this.waker = Some(cx.waker().clone());
            return Poll::Pending;
        };
        this.timer.as_mut().reset(next);
        if this.timer.as_mut().poll(cx).is_ready() {
            // The time has come while this was looked at: look again.
            cx.waker().wake_by_ref();
        }
        this.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn try_send(&mut self, frame: FrameOut<'_>) -> Result<(), Full> {
        if self.line == Line::Closed {
            self.log().after_close += 1;
            return Ok(());
        }
        let now = Instant::now();
        if self.line == Line::Gone {
            // It is sent, and nobody hears it.
            let sent = SentFrame {
                when: now.saturating_duration_since(self.start),
                tag: frame.tag,
                at: frame.at,
                length: frame.length,
                payload: Bytes::copy_from_slice(frame.payload),
                last: frame.last,
            };
            let mut log = self.log();
            log.unheard += 1;
            log.frames.push(sent);
            return Ok(());
        }
        let end = frame.at.saturating_add(frame.length);
        match &mut self.playing {
            Some(playing) if playing.tag == frame.tag => {
                if end > playing.received && !playing.cleared {
                    playing.received = end;
                }
            }
            // A new response: the loudspeaker starts on it.
            _ => {
                self.playing = Some(Playing {
                    tag: frame.tag,
                    since: now + self.holds,
                    received: end,
                    reported: None,
                    cleared: false,
                });
            }
        }
        if self
            .playing
            .is_some_and(|playing| playing.tag == frame.tag && playing.cleared)
        {
            self.log().late += 1;
        }
        let sent = SentFrame {
            when: now.saturating_duration_since(self.start),
            tag: frame.tag,
            at: frame.at,
            length: frame.length,
            payload: Bytes::copy_from_slice(frame.payload),
            last: frame.last,
        };
        self.log().frames.push(sent);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        Ok(())
    }

    fn try_control(&mut self, control: TransportControl) -> Result<(), Full> {
        if self.line == Line::Closed {
            self.log().after_close += 1;
            return Ok(());
        }
        let now = Instant::now();
        let when = now.saturating_duration_since(self.start);
        let tag = match control {
            TransportControl::Clear { tag } => tag,
            TransportControl::Dtmf { key } => {
                self.log().pressed.push((when, key));
                return Ok(());
            }
        };
        self.log().cleared.push((when, tag));
        if self.remote_buffer().clearable
            && let Some(playing) = &mut self.playing
            && playing.tag == tag
        {
            // What was not played yet never will be.
            playing.received = playing.played(now);
            playing.cleared = true;
        }
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        Ok(())
    }

    fn try_send_tone(&mut self, tone: ToneOut<'_>) -> Result<(), Full> {
        if self.line == Line::Closed {
            self.log().after_close += 1;
            return Ok(());
        }
        let sent = SentTone {
            when: Instant::now().saturating_duration_since(self.start),
            length: tone.length,
            payload: Bytes::copy_from_slice(tone.payload),
        };
        let gone = self.line == Line::Gone;
        let mut log = self.log();
        // Sent into a line that is gone, nobody hears it.
        log.unheard += usize::from(gone);
        log.tones.push(sent);
        Ok(())
    }

    fn poll_close(&mut self, _: &mut Context<'_>) -> Poll<()> {
        self.log().goodbyes += 1;
        if self.clings {
            // A far end that never answers. Nothing will wake whoever
            // waits for it.
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}
