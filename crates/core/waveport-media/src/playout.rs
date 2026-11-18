//! The outbound path: from the audio of speech synthesis to frames handed
//! to a transport at the pace they are heard.
//!
//! Speech synthesis delivers audio faster than it is spoken. [`Playout`]
//! holds what has not been played, hands it on one frame at a time, each
//! at its time, and knows at every moment how much of the response has
//! been heard. That is what lets an interruption cut a response where the
//! listener was and not where the synthesiser was.
//!
//! # One response at a time
//!
//! Every call names the response it is for by its tag. Audio is taken for
//! the newest response there has been, before or after playback of it is
//! allowed to [`start`](Playout::start). Audio for any other is refused,
//! so a synthesiser that keeps sending after it was told to stop does no
//! harm.
//!
//! # Pace
//!
//! Frame `k` of a response is heard at `t0 + k × 20 ms`, and is handed
//! over [`lead`](PlayoutConfig::lead) before that, so the transport is
//! never given more than the lead to hold. Times are worked out from the
//! clock reading passed in, never counted in calls: a call that comes late
//! hands over what was due and the pace is kept.
//!
//! When the audio runs out before the response is finished, the listener
//! hears a gap. The pace starts again from the moment more audio arrives.
//! The same happens when a call comes more than 100 ms late: what is due
//! by then would only pile up at the far end.
//!
//! # What was played
//!
//! A transport whose far end says what it played is believed
//! ([`ack`](Playout::ack)), and one that says nothing is taken to play
//! as the clock says. A far end that says some and then stops short of a
//! response it was handed whole is waited for, since what it says is
//! better than the clock, but not for ever: once the last of the
//! response has been due for [`ack_wait`](PlayoutConfig::ack_wait), the
//! response is taken to have been played to its end. A report can be
//! lost, and a far end can be a little short in what it counts; a
//! response is not held open for either.

use core::time::Duration;

use waveport_domain::ids::OpTag;
use waveport_domain::time::{MediaDur, MonoTime};

use crate::gain::{self, Gain};
use crate::ledger::{Ledger, Mark};
use crate::rate::Rate;
use crate::ring::Ring;

/// How late a frame may be handed over and the pace still be kept.
const MAX_BEHIND: Duration = Duration::from_millis(100);

/// How an outbound path is set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayoutConfig {
    /// The rate of the audio it is given, and of the frames it hands over.
    pub rate: Rate,
    /// How long before a frame is heard it is handed to the transport.
    pub lead: Duration,
    /// How much audio it holds at most.
    pub buffer: MediaDur,
    /// The volume while someone speaks over the agent.
    pub ducked: Gain,
    /// How much audio the far end is taken to hold that it has not played
    /// yet. It is used when the transport does not say what was played.
    pub far_buffer: MediaDur,
    /// How long a far end that says what it played may stay short of a
    /// response it was handed whole, from the time the last of it is
    /// heard by the clock, before it is waited for no longer: the
    /// response is then taken to have been played to its end.
    pub ack_wait: Duration,
    /// How many marks it holds for audio not yet played.
    pub marks: usize,
}

impl PlayoutConfig {
    /// The settings of the design for audio at `rate`: a lead of one
    /// frame, 3 s of audio held, a quarter of the volume while someone
    /// speaks over the agent, and 2 s for a far end to say that it played
    /// the end of a response.
    #[must_use]
    pub const fn new(rate: Rate) -> Self {
        Self {
            rate,
            lead: Duration::from_millis(20),
            buffer: MediaDur::from_millis(3_000),
            ducked: Gain::ratio(1, 4),
            far_buffer: MediaDur::ZERO,
            ack_wait: Duration::from_secs(2),
            marks: 256,
        }
    }
}

/// A call named a response that is not the one being played or waited
/// for: an older one, or one that was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale;

impl core::fmt::Display for Stale {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the response is not the one being played")
    }
}

impl core::error::Error for Stale {}

/// A frame handed to the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The response it is part of.
    pub tag: OpTag,
    /// Where it begins in the response's audio.
    pub at: MediaDur,
    /// How many of its samples are the response's audio. The rest of the
    /// frame, if any, is silence.
    pub samples: usize,
    /// Whether it is the last frame of the response.
    pub last: bool,
}

/// How far playback of a response has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    /// The response.
    pub tag: OpTag,
    /// Audio handed to the transport.
    pub handed: MediaDur,
    /// Audio played to the listener: as the transport says, or else as
    /// the clock and the far end's buffer suggest.
    pub played: MediaDur,
    /// Bytes of the response's text heard in full.
    pub heard_text: u32,
    /// Whether all of the response has been played.
    pub drained: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Audio is taken and kept. None is played yet.
    Waiting,
    /// Audio is handed over at its pace.
    Playing,
    /// The response was stopped. One frame is left: its first `fade`
    /// samples, fading out.
    Fading { fade: usize },
    /// The response was stopped. Nothing of it is taken or handed over.
    Over,
}

/// The outbound path of one track of audio.
#[derive(Debug)]
pub struct Playout {
    config: PlayoutConfig,
    buffer: Ring,
    ledger: Ledger,
    /// The newest response there has been.
    tag: Option<OpTag>,
    stage: Stage,
    /// Samples of the response handed over.
    handed: u64,
    /// Whether all of the response's audio has been given.
    finished: bool,
    /// The moment at which the sample `anchor_at` of the response is
    /// heard. The pace is counted from here.
    anchor: MonoTime,
    anchor_at: u64,
    /// The volume the last frame ended at, and the one to move to.
    gain: Gain,
    target: Gain,
    /// Samples the transport says were played.
    acked: Option<u64>,
    /// The most samples reported as played.
    played: u64,
    /// Whether the end of the response was reported.
    drained: bool,
    /// Ticks of the media clock in one sample of the response: worked
    /// out once, since every report of a position asks for it.
    tick: u32,
}

impl Playout {
    /// An outbound path.
    #[must_use]
    pub fn new(config: PlayoutConfig) -> Self {
        Self {
            buffer: Ring::new(config.rate.samples_in(config.buffer)),
            ledger: Ledger::new(config.marks),
            tag: None,
            stage: Stage::Over,
            handed: 0,
            finished: false,
            anchor: MonoTime::ZERO,
            anchor_at: 0,
            gain: Gain::FULL,
            target: Gain::FULL,
            acked: None,
            played: 0,
            drained: false,
            tick: config.rate.ticks_per_sample(),
            config,
        }
    }

    /// How it was set up.
    #[must_use]
    pub const fn config(&self) -> &PlayoutConfig {
        &self.config
    }

    /// How many samples it holds that have not been handed over.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// How many more samples it can take.
    #[must_use]
    pub const fn room(&self) -> usize {
        self.buffer.room()
    }

    /// Makes `tag` the response in hand if it is newer than the one that
    /// is, and says whether calls for it are in order.
    fn admit(&mut self, tag: OpTag) -> Result<(), Stale> {
        match self.tag {
            Some(current) if current == tag => match self.stage {
                Stage::Waiting | Stage::Playing => Ok(()),
                Stage::Fading { .. } | Stage::Over => Err(Stale),
            },
            Some(current) if tag.generation <= current.generation => Err(Stale),
            _ => {
                self.buffer.clear();
                self.ledger.clear();
                self.tag = Some(tag);
                self.stage = Stage::Waiting;
                self.handed = 0;
                self.finished = false;
                self.anchor_at = 0;
                self.gain = Gain::FULL;
                self.target = Gain::FULL;
                self.acked = None;
                self.played = 0;
                self.drained = false;
                Ok(())
            }
        }
    }

    /// Whether `tag` is the response in hand and has not been stopped.
    fn live(&self, tag: OpTag) -> bool {
        self.tag == Some(tag) && matches!(self.stage, Stage::Waiting | Stage::Playing)
    }

    /// Audio of the response `tag` may be played from `now` on.
    ///
    /// # Errors
    ///
    /// The response is older than the one in hand, or was stopped.
    pub fn start(&mut self, tag: OpTag, now: MonoTime) -> Result<(), Stale> {
        self.admit(tag)?;
        if self.stage == Stage::Waiting {
            self.stage = Stage::Playing;
            self.anchor = now;
            self.anchor_at = self.handed;
        }
        Ok(())
    }

    /// Takes more audio of the response `tag`, which arrived at `now`.
    /// Returns how many samples were taken, from the first: fewer than
    /// given when there is no room for all of them, and none once the
    /// response was [finished](Self::finish). What was not taken has to be
    /// given again later.
    ///
    /// # Errors
    ///
    /// The response is older than the one in hand, or was stopped.
    pub fn push(&mut self, tag: OpTag, now: MonoTime, pcm: &[i16]) -> Result<usize, Stale> {
        self.admit(tag)?;
        if self.finished {
            return Ok(0);
        }
        let ran_out = !self.frame_ready();
        let taken = self.buffer.push_some(pcm);
        if self.stage == Stage::Playing && ran_out && now > self.due() {
            // The listener has been hearing nothing since the next frame
            // was due. The pace starts again here.
            self.anchor = now;
            self.anchor_at = self.handed;
        }
        Ok(taken)
    }

    /// Adds a mark to the response `tag`: where its text and its audio
    /// meet.
    ///
    /// # Errors
    ///
    /// The response is older than the one in hand, or was stopped.
    pub fn mark(&mut self, tag: OpTag, mark: Mark) -> Result<(), Stale> {
        self.admit(tag)?;
        self.ledger.mark(mark);
        Ok(())
    }

    /// All the audio of the response `tag` has been given.
    ///
    /// # Errors
    ///
    /// The response is older than the one in hand, or was stopped.
    pub fn finish(&mut self, tag: OpTag) -> Result<(), Stale> {
        self.admit(tag)?;
        self.finished = true;
        Ok(())
    }

    /// Lowers the volume of the response `tag`: someone is speaking over
    /// it. The next frame handed over moves to the lower volume. Frames
    /// already handed over cannot be changed.
    pub fn duck(&mut self, tag: OpTag) {
        if self.live(tag) {
            self.target = self.config.ducked;
        }
    }

    /// Restores the volume of the response `tag` over the next frame.
    pub fn unduck(&mut self, tag: OpTag) {
        if self.live(tag) {
            self.target = Gain::FULL;
        }
    }

    /// Stops the response `tag`. What is held of it is discarded, but for
    /// `fade_ms` milliseconds, one frame at most, that fade out in one
    /// last frame so that the sound does not end in a click.
    ///
    /// Audio that still arrives for the response is refused from here on.
    pub fn stop(&mut self, tag: OpTag, fade_ms: u16) {
        match self.tag {
            Some(current) if current == tag => {}
            Some(current) if tag.generation <= current.generation => return,
            // It was stopped before any of it got here.
            _ => self.tag = Some(tag),
        }
        let fade = self
            .config
            .rate
            .samples_in_millis(u32::from(fade_ms))
            .min(self.config.rate.frame())
            .min(self.buffer.len());
        match self.stage {
            Stage::Playing if self.tag == Some(tag) && fade > 0 && self.handed > 0 => {
                self.stage = Stage::Fading { fade };
            }
            Stage::Fading { .. } => {}
            Stage::Waiting | Stage::Playing | Stage::Over => {
                self.stage = Stage::Over;
                self.buffer.clear();
            }
        }
    }

    /// The transport says that `played` of the audio of the response `tag`
    /// has been played to the listener. From then on what it says is
    /// taken in place of the clock.
    pub fn ack(&mut self, tag: OpTag, played: MediaDur) {
        if self.live(tag) {
            let played = self.config.rate.samples_in(played) as u64;
            self.acked = Some(self.acked.unwrap_or(0).max(played).min(self.handed));
        }
    }

    /// Whether a frame can be made of what is held.
    fn frame_ready(&self) -> bool {
        self.buffer.len() >= self.config.rate.frame() || (self.finished && !self.buffer.is_empty())
    }

    /// How long `samples` samples take.
    fn span(&self, samples: u64) -> Duration {
        let nanos = u128::from(samples) * 1_000_000_000 / u128::from(self.config.rate.hz());
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// When the next sample to hand over is heard.
    fn due(&self) -> MonoTime {
        self.anchor
            .after(self.span(self.handed.saturating_sub(self.anchor_at)))
    }

    /// When the last of what was handed over is heard, by the clock and
    /// what the far end is taken to hold.
    fn heard_out(&self) -> MonoTime {
        let far = u64::from(self.config.far_buffer.ticks()) * 1_000_000_000 / 48_000;
        self.due().after(Duration::from_nanos(far))
    }

    /// Whether a far end that says what it played has stayed short of the
    /// end of what it was handed for too long at `now`, and is waited for
    /// no longer. It is asked only between the last frame of a response
    /// and its end, and is kept out of the way of what is asked at every
    /// step.
    #[cold]
    #[inline(never)]
    fn waited_out(&self, now: MonoTime) -> bool {
        self.acked.is_some() && now >= self.heard_out().after(self.config.ack_wait)
    }

    /// A length of the response's audio. One the media clock cannot
    /// hold, about a day, is given as the longest it can.
    fn length(&self, samples: u64) -> MediaDur {
        let ticks = u32::try_from(samples)
            .unwrap_or(u32::MAX)
            .saturating_mul(self.tick);
        MediaDur::from_ticks(ticks)
    }

    /// When to call [`poll`](Self::poll) next: the time the next frame is
    /// to be handed over, or the time the response will have been played
    /// to its end and [`position`](Self::position) will say so. `None`
    /// when there is nothing to do until more audio or another call
    /// arrives. A time that has passed means at once.
    #[must_use]
    pub fn wake(&self) -> Option<MonoTime> {
        match self.stage {
            Stage::Fading { .. } => Some(MonoTime::ZERO),
            Stage::Playing if self.frame_ready() => {
                let lead = u64::try_from(self.config.lead.as_nanos()).unwrap_or(u64::MAX);
                Some(MonoTime::from_nanos(
                    self.due().as_nanos().saturating_sub(lead),
                ))
            }
            Stage::Playing if self.finished && !self.drained => Some(match self.acked {
                // The far end says what it played, and is given a while
                // to say that it played the end.
                Some(_) => self.heard_out().after(self.config.ack_wait),
                None => self.heard_out(),
            }),
            Stage::Waiting | Stage::Playing | Stage::Over => None,
        }
    }

    /// Hands over the next frame, into the start of `out`, if one is due
    /// at `now`. The frame is 20 ms long. The caller calls again until
    /// there is none, and then at [`wake`](Self::wake).
    ///
    /// `out` needs room for a frame; with less, nothing is handed over.
    pub fn poll(&mut self, now: MonoTime, out: &mut [i16]) -> Option<Frame> {
        let tag = self.tag?;
        let out = out.get_mut(..self.config.rate.frame())?;
        let at = self.length(self.handed);
        match self.stage {
            Stage::Waiting | Stage::Over => return None,
            Stage::Fading { fade } => {
                let samples = self.buffer.pop(out).min(fade);
                let (sound, silence) = out.split_at_mut(samples);
                gain::ramp(sound, self.gain, Gain::SILENT);
                silence.fill(0);
                self.stage = Stage::Over;
                self.buffer.clear();
                return Some(Frame {
                    tag,
                    at,
                    samples,
                    last: true,
                });
            }
            Stage::Playing => {}
        }
        if !self.frame_ready() {
            return None;
        }
        if now.since(self.due()) > MAX_BEHIND {
            self.anchor = now;
            self.anchor_at = self.handed;
        }
        if now.after(self.config.lead) < self.due() {
            return None;
        }
        let samples = self.buffer.pop(out);
        let (sound, silence) = out.split_at_mut(samples);
        gain::ramp(sound, self.gain, self.target);
        silence.fill(0);
        self.gain = self.target;
        self.handed += samples as u64;
        Some(Frame {
            tag,
            at,
            samples,
            last: self.finished && self.buffer.is_empty(),
        })
    }

    /// How far playback of the response in hand has got at `now`. `None`
    /// when there is no response, or it was stopped.
    pub fn position(&mut self, now: MonoTime) -> Option<Position> {
        let tag = self.tag?;
        let playing = match self.stage {
            Stage::Waiting => false,
            Stage::Playing => true,
            Stage::Fading { .. } | Stage::Over => return None,
        };
        let mut played = match self.acked {
            Some(acked) => acked,
            None if playing => {
                // What was handed over is heard at its time, less what
                // the far end still holds.
                let since = now.since(self.anchor).as_nanos();
                let since = since * u128::from(self.config.rate.hz()) / 1_000_000_000;
                let far = u64::from(self.config.far_buffer.ticks() / self.tick);
                self.anchor_at
                    .saturating_add(u64::try_from(since).unwrap_or(u64::MAX))
                    .saturating_sub(far)
            }
            None => 0,
        };
        let all_handed = self.finished && self.buffer.is_empty();
        self.drained = false;
        if playing && all_handed {
            if played < self.handed && self.waited_out(now) {
                played = self.handed;
            }
            self.drained = played >= self.handed;
        }
        self.played = self.played.max(played.min(self.handed));
        let played = self.length(self.played);
        Some(Position {
            tag,
            handed: self.length(self.handed),
            played,
            heard_text: self.ledger.heard(played),
            drained: self.drained,
        })
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use waveport_domain::ids::{Epoch, GenId, OpTag};
    use waveport_domain::time::{MediaDur, MonoTime};

    use super::{Frame, Playout, PlayoutConfig, Position, Stale};
    use crate::gain::Gain;
    use crate::ledger::{Mark, Spread};
    use crate::rate::Rate;

    const RATE: Rate = Rate::Hz24000;

    fn tag(generation: u32) -> OpTag {
        OpTag {
            generation: GenId(generation),
            epoch: Epoch(3),
        }
    }

    const TAG: OpTag = OpTag {
        generation: GenId(7),
        epoch: Epoch(3),
    };

    fn at(ms: u64) -> MonoTime {
        MonoTime::from_millis(ms)
    }

    fn ms(ms: u32) -> MediaDur {
        MediaDur::from_millis(ms)
    }

    /// `millis` of audio in which every sample says where it is, counted
    /// from `from`.
    fn counting(from: usize, millis: u32) -> Vec<i16> {
        (from..from + RATE.samples_in_millis(millis))
            .map(|n| i16::try_from(n % 30_000).unwrap())
            .collect()
    }

    /// `millis` of audio at one level.
    fn level(millis: u32) -> Vec<i16> {
        vec![10_000; RATE.samples_in_millis(millis)]
    }

    /// A frame that was handed over, and when.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Handed {
        ms: u64,
        frame: Frame,
        pcm: Vec<i16>,
    }

    impl Handed {
        /// When it was handed over and where in the response it begins,
        /// both in milliseconds.
        fn times(&self) -> (u64, u32) {
            (self.ms, self.frame.at.as_millis())
        }
    }

    struct Deck {
        playout: Playout,
        handed: Vec<Handed>,
    }

    impl Deck {
        fn new() -> Self {
            Self::with(PlayoutConfig::new(RATE))
        }

        fn with(config: PlayoutConfig) -> Self {
            Self {
                playout: Playout::new(config),
                handed: Vec::new(),
            }
        }

        /// Calls at `ms` until nothing more is handed over, and returns
        /// how many frames were.
        fn poll(&mut self, ms: u64) -> usize {
            let before = self.handed.len();
            // What a frame does not write would show.
            let mut out = [7_777; 480];
            while let Some(frame) = self.playout.poll(at(ms), &mut out) {
                self.handed.push(Handed {
                    ms,
                    frame,
                    pcm: out.to_vec(),
                });
                out = [7_777; 480];
            }
            self.handed.len() - before
        }

        /// Lets time pass from `from` to `until`, calling whenever the
        /// playout asks to be called, as a runtime does.
        fn run(&mut self, from: u64, until: u64) {
            let mut now = from;
            loop {
                self.poll(now);
                // The end of a response is reported when it comes.
                self.playout.position(at(now));
                let next = self
                    .playout
                    .wake()
                    .map(|wake| wake.as_nanos().div_ceil(1_000_000));
                match next {
                    Some(next) if next > now && next <= until => now = next,
                    Some(next) => {
                        assert!(next > now, "asked to be called at once, with nothing to do");
                        return;
                    }
                    None => return,
                }
            }
        }

        fn times(&self) -> Vec<(u64, u32)> {
            self.handed.iter().map(Handed::times).collect()
        }

        fn position(&mut self, ms: u64) -> Position {
            self.playout.position(at(ms)).unwrap()
        }
    }

    #[test]
    fn the_first_frames_go_out_at_once_and_the_rest_keep_pace() {
        let mut deck = Deck::new();
        assert_eq!(deck.playout.start(TAG, at(1_000)), Ok(()));
        assert_eq!(deck.playout.wake(), None, "there is nothing to play yet");
        let audio = counting(0, 100);
        assert_eq!(deck.playout.push(TAG, at(1_000), &audio), Ok(2_400));
        assert_eq!(deck.playout.finish(TAG), Ok(()));

        // The first frame is heard now, and the second is handed over one
        // frame ahead of its time.
        assert_eq!(deck.poll(1_000), 2);
        assert_eq!(deck.playout.wake(), Some(at(1_020)));
        assert_eq!(deck.poll(1_019), 0);
        deck.run(1_019, 1_099);
        assert_eq!(
            deck.times(),
            [
                (1_000, 0),
                (1_000, 20),
                (1_020, 40),
                (1_040, 60),
                (1_060, 80)
            ]
        );
        // What was handed over is what was given, in order, and only the
        // last frame says it is the last.
        let pcm: Vec<i16> = deck.handed.iter().flat_map(|h| h.pcm.clone()).collect();
        assert_eq!(pcm, audio);
        let last: Vec<bool> = deck.handed.iter().map(|h| h.frame.last).collect();
        assert_eq!(last, [false, false, false, false, true]);
        assert!(deck.handed.iter().all(|h| h.frame.tag == TAG));
        assert!(deck.handed.iter().all(|h| h.frame.samples == 480));

        // It has all been heard 100 ms after it began.
        assert_eq!(deck.playout.wake(), Some(at(1_100)));
        let position = deck.position(1_099);
        assert_eq!((position.handed, position.played), (ms(100), ms(99)));
        assert!(!position.drained);
        assert!(deck.position(1_100).drained);
        assert_eq!(deck.playout.wake(), None);
    }

    #[test]
    fn audio_that_comes_before_the_start_waits() {
        let mut deck = Deck::new();
        assert_eq!(deck.playout.push(TAG, at(900), &counting(0, 60)), Ok(1_440));
        assert_eq!(deck.poll(950), 0);
        assert_eq!(deck.playout.wake(), None);
        let waiting = deck.position(950);
        assert_eq!((waiting.handed, waiting.played), (ms(0), ms(0)));
        assert!(!waiting.drained);

        assert_eq!(deck.playout.start(TAG, at(1_000)), Ok(()));
        deck.run(1_000, 1_030);
        assert_eq!(deck.times(), [(1_000, 0), (1_000, 20), (1_020, 40)]);
        // Starting twice changes nothing.
        assert_eq!(deck.playout.start(TAG, at(1_030)), Ok(()));
        assert_eq!(deck.position(1_030).played, ms(30));
    }

    #[test]
    fn a_response_started_before_its_audio_begins_when_the_audio_comes() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        assert_eq!(deck.poll(1_100), 0);
        deck.playout.push(TAG, at(1_130), &counting(0, 60)).unwrap();
        deck.run(1_130, 1_140);
        assert_eq!(deck.times(), [(1_130, 0), (1_130, 20)]);
        assert_eq!(deck.position(1_140).played, ms(10));
        deck.run(1_140, 1_500);
        assert_eq!(deck.times()[2..], [(1_150, 40)]);
    }

    #[test]
    fn audio_that_runs_out_leaves_a_gap_and_the_pace_starts_again() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &counting(0, 60)).unwrap();
        deck.run(1_000, 1_190);
        assert_eq!(deck.times(), [(1_000, 0), (1_000, 20), (1_020, 40)]);
        // All that was given has been heard, and the response is not
        // over.
        let stalled = deck.position(1_100);
        assert_eq!((stalled.handed, stalled.played), (ms(60), ms(60)));
        assert!(!stalled.drained);

        // More arrives 140 ms after the last of it was heard.
        deck.playout
            .push(TAG, at(1_200), &counting(1_440, 40))
            .unwrap();
        assert_eq!(deck.position(1_200).played, ms(60));
        deck.playout.finish(TAG).unwrap();
        deck.run(1_200, 1_210);
        assert_eq!(deck.times()[3..], [(1_200, 60), (1_200, 80)]);
        assert_eq!(deck.position(1_210).played, ms(70));
        assert!(!deck.position(1_239).drained);
        assert!(deck.position(1_240).drained);
    }

    #[test]
    fn what_was_played_is_not_taken_back_when_the_pace_starts_again() {
        // The far end is taken to hold 100 ms.
        let mut deck = Deck::with(PlayoutConfig {
            far_buffer: ms(100),
            ..PlayoutConfig::new(RATE)
        });
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(60)).unwrap();
        deck.run(1_000, 1_190);
        assert_eq!(deck.position(1_190).played, ms(60));
        // The pace starts again, and the 100 ms count from there.
        deck.playout.push(TAG, at(1_200), &level(40)).unwrap();
        deck.run(1_200, 1_250);
        assert_eq!(deck.position(1_250).played, ms(60));
        assert_eq!(deck.position(1_320).played, ms(80));
    }

    #[test]
    fn audio_that_arrives_just_in_time_does_not_move_the_pace() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        // A synthesiser that only just keeps up: each frame arrives 15 ms
        // before it is heard, which is after it was to be handed over.
        for frame in 0..10 {
            let now = 1_000 + 20 * frame;
            let from = 480 * usize::try_from(frame).unwrap();
            deck.playout
                .push(TAG, at(now + 5), &counting(from, 20))
                .unwrap();
            deck.poll(now + 5);
        }
        let expected: Vec<(u64, u32)> = (0..10)
            .map(|k| (1_005 + 20 * u64::from(k), 20 * k))
            .collect();
        // The first frame came 5 ms after the start, which is where the
        // pace begins. No frame after it moved it.
        assert_eq!(deck.times(), expected);
        assert_eq!(deck.position(1_105).played, ms(100));
    }

    #[test]
    fn a_call_that_comes_late_does_not_slow_the_pace() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout
            .push(TAG, at(1_000), &counting(0, 1_000))
            .unwrap();
        // Every call is 3 ms late.
        for call in 0..=25 {
            deck.poll(1_003 + 20 * call);
        }
        assert_eq!(deck.handed.len(), 27);
        assert_eq!(deck.times()[26], (1_503, 520));

        // A call 60 ms late hands over what was due.
        assert_eq!(deck.poll(1_583), 4);
        assert_eq!(deck.times()[30], (1_583, 600));

        // Nor does audio that arrives while a call is late: there was no
        // gap, because there was audio to play.
        deck.playout
            .push(TAG, at(1_650), &counting(24_000, 100))
            .unwrap();
        assert_eq!(deck.poll(1_650), 3);
        assert_eq!(deck.times()[33], (1_650, 660));
        assert_eq!(deck.position(1_650).played, ms(650));
    }

    #[test]
    fn a_call_that_comes_far_too_late_starts_the_pace_again() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout
            .push(TAG, at(1_000), &counting(0, 1_000))
            .unwrap();
        assert_eq!(deck.poll(1_000), 2);
        // Nothing runs for half a second. 23 frames are due by then.
        assert_eq!(deck.poll(1_500), 2);
        assert_eq!(deck.times()[2..], [(1_500, 40), (1_500, 60)]);
        assert_eq!(deck.poll(1_520), 1);
        assert_eq!(deck.position(1_510).played, ms(50));
    }

    #[test]
    fn speech_over_the_agent_lowers_it_within_a_frame_and_restores_it() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(200)).unwrap();
        deck.poll(1_000);
        deck.playout.duck(TAG);
        deck.poll(1_020);
        deck.poll(1_040);
        deck.playout.unduck(TAG);
        deck.poll(1_060);
        deck.poll(1_080);

        let ends: Vec<(i16, i16)> = deck.handed.iter().map(|h| (h.pcm[0], h.pcm[479])).collect();
        assert_eq!(
            ends,
            [
                (10_000, 10_000),
                (10_000, 10_000),
                // Down over one frame, to a quarter.
                (10_000, 2_516),
                (2_500, 2_500),
                // And up again over one.
                (2_500, 9_984),
                (10_000, 10_000),
            ]
        );
        // The way down has no steps in it.
        let down = &deck.handed[2].pcm;
        assert!(
            down.windows(2)
                .all(|pair| (0..=16).contains(&(pair[0] - pair[1])))
        );
    }

    #[test]
    fn a_stop_fades_out_and_discards_the_rest() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(1_000)).unwrap();
        assert_eq!(deck.poll(1_000), 2);

        deck.playout.stop(TAG, 10);
        assert_eq!(deck.playout.position(at(1_005)), None, "it is over");
        assert_eq!(deck.playout.wake(), Some(MonoTime::ZERO), "at once");
        assert_eq!(deck.poll(1_005), 1);
        let fade = &deck.handed[2];
        assert_eq!((fade.frame.samples, fade.frame.last), (240, true));
        assert_eq!(fade.frame.at, ms(40));
        assert_eq!(fade.pcm[0], 10_000);
        assert!(fade.pcm[..240].windows(2).all(|pair| pair[0] > pair[1]));
        assert!(fade.pcm[239] < 50);
        assert!(fade.pcm[240..].iter().all(|&sample| sample == 0));

        // Nothing is left, and nothing more is taken.
        assert_eq!(deck.poll(1_100), 0);
        assert_eq!(deck.playout.wake(), None);
        assert_eq!(deck.playout.buffered(), 0);
        assert_eq!(deck.playout.push(TAG, at(1_100), &level(20)), Err(Stale));
        assert_eq!(deck.playout.mark(TAG, word(1, 10)), Err(Stale));
        assert_eq!(deck.playout.finish(TAG), Err(Stale));
        assert_eq!(deck.playout.start(TAG, at(1_100)), Err(Stale));
        // Stopping again changes nothing.
        deck.playout.stop(TAG, 10);
        assert_eq!(deck.poll(1_200), 0);
    }

    #[test]
    fn a_stop_fades_from_the_volume_the_response_had() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(1_000)).unwrap();
        deck.playout.duck(TAG);
        deck.poll(1_000);
        // Someone speaks over the agent, and then it is cut off. A fade
        // is never longer than a frame.
        deck.playout.stop(TAG, 500);
        deck.playout.unduck(TAG);
        assert_eq!(deck.poll(1_001), 1);
        let fade = &deck.handed[2];
        assert_eq!(fade.frame.samples, 480);
        assert_eq!(fade.pcm[0], 2_500);
        assert!(fade.pcm[479] < 10);
    }

    #[test]
    fn a_stop_with_nothing_playing_only_discards() {
        // Before the start.
        let mut deck = Deck::new();
        deck.playout.push(TAG, at(900), &level(100)).unwrap();
        deck.playout.stop(TAG, 10);
        assert_eq!((deck.poll(950), deck.playout.buffered()), (0, 0));
        assert_eq!(deck.playout.wake(), None);
        assert_eq!(deck.playout.push(TAG, at(960), &level(20)), Err(Stale));

        // After the audio ran out.
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(20)).unwrap();
        assert_eq!(deck.poll(1_000), 1);
        deck.playout.stop(TAG, 10);
        assert_eq!(deck.poll(1_010), 0);

        // With no fade asked for.
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(200)).unwrap();
        deck.poll(1_000);
        deck.playout.stop(TAG, 0);
        assert_eq!((deck.poll(1_010), deck.playout.buffered()), (0, 0));
    }

    #[test]
    fn only_the_newest_response_is_taken() {
        let mut deck = Deck::new();
        assert_eq!(deck.playout.position(at(0)), None, "there is none yet");
        deck.playout.push(tag(7), at(1_000), &level(100)).unwrap();
        // An older one is refused.
        assert_eq!(deck.playout.push(tag(6), at(1_000), &level(20)), Err(Stale));
        assert_eq!(deck.playout.start(tag(6), at(1_000)), Err(Stale));
        assert_eq!(deck.playout.buffered(), 2_400);

        // A newer one takes its place.
        assert_eq!(deck.playout.push(tag(8), at(1_010), &level(20)), Ok(480));
        assert_eq!(deck.playout.buffered(), 480);
        assert_eq!(deck.playout.push(tag(7), at(1_020), &level(20)), Err(Stale));
        deck.playout.duck(tag(7));
        deck.playout.stop(tag(7), 10);
        deck.playout.ack(tag(7), ms(500));
        deck.playout.start(tag(8), at(1_020)).unwrap();
        assert_eq!(deck.poll(1_020), 1);
        assert_eq!(deck.handed[0].pcm[479], 10_000, "at full volume");
        assert_eq!(deck.position(1_030).played, ms(10));

        // One that was stopped before any of it arrived is known.
        deck.playout.stop(tag(9), 10);
        assert_eq!(deck.playout.push(tag(9), at(1_040), &level(20)), Err(Stale));
        assert_eq!(deck.playout.push(tag(8), at(1_040), &level(20)), Err(Stale));
        assert_eq!(deck.playout.position(at(1_040)), None);
    }

    #[test]
    fn no_more_is_held_than_three_seconds() {
        let mut deck = Deck::new();
        let long = level(4_000);
        assert_eq!(deck.playout.push(TAG, at(1_000), &long), Ok(72_000));
        assert_eq!(deck.playout.room(), 0);
        assert_eq!(deck.playout.push(TAG, at(1_000), &long), Ok(0));
        // Room is made as frames are handed over.
        deck.playout.start(TAG, at(1_000)).unwrap();
        assert_eq!(deck.poll(1_000), 2);
        assert_eq!(deck.playout.room(), 960);
        assert_eq!(deck.playout.push(TAG, at(1_000), &long), Ok(960));
        // What comes after the end is not taken.
        deck.playout.finish(TAG).unwrap();
        deck.poll(1_020);
        assert_eq!(deck.playout.push(TAG, at(1_020), &long), Ok(0));
    }

    fn word(text: u32, audio_ms: u32) -> Mark {
        Mark {
            text,
            audio: ms(audio_ms),
            spread: Spread::AtOnce,
        }
    }

    #[test]
    fn the_text_heard_follows_the_audio_played() {
        for far_buffer in [0, 100] {
            let mut deck = Deck::with(PlayoutConfig {
                far_buffer: ms(far_buffer),
                ..PlayoutConfig::new(RATE)
            });
            let far = u64::from(far_buffer);
            deck.playout.start(TAG, at(1_000)).unwrap();
            deck.playout.push(TAG, at(1_000), &level(1_000)).unwrap();
            for mark in [word(5, 300), word(12, 800), word(13, 1_000)] {
                deck.playout.mark(TAG, mark).unwrap();
            }
            deck.playout.finish(TAG).unwrap();
            deck.run(1_000, 1_299 + far);
            assert_eq!(deck.position(1_299 + far).heard_text, 0);
            assert_eq!(deck.position(1_300 + far).heard_text, 5);
            deck.run(1_300 + far, 1_850 + far);
            let position = deck.position(1_850 + far);
            assert_eq!((position.heard_text, position.played), (12, ms(850)));
            // The end is reported once the far end has played what it
            // held.
            deck.run(1_850 + far, 1_999 + far);
            assert!(!deck.position(1_999 + far).drained);
            assert_eq!(deck.playout.wake(), Some(at(2_000 + far)));
            let end = deck.position(2_000 + far);
            assert_eq!((end.heard_text, end.drained), (13, true));
            assert_eq!(end.tag, TAG);
        }
    }

    #[test]
    fn a_transport_that_says_what_was_played_is_believed() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.ack(TAG, ms(0));
        deck.playout.push(TAG, at(1_000), &level(200)).unwrap();
        deck.playout.mark(TAG, word(9, 150)).unwrap();
        deck.playout.finish(TAG).unwrap();
        deck.run(1_000, 1_300);
        assert_eq!(deck.handed.len(), 10);
        assert_eq!(deck.position(1_300).played, ms(0));

        // The far end is slower than the clock suggests.
        deck.playout.ack(TAG, ms(120));
        let position = deck.position(1_300);
        assert_eq!((position.played, position.heard_text), (ms(120), 0));
        assert!(!position.drained);
        // The last of it was due at 1,200. The transport is given two
        // seconds from then to say that it was played.
        assert_eq!(deck.playout.wake(), Some(at(3_200)));
        // What it says does not go back, nor past what it was given.
        deck.playout.ack(TAG, ms(100));
        assert_eq!(deck.position(1_300).played, ms(120));
        deck.playout.ack(TAG, ms(900));
        let end = deck.position(1_300);
        assert_eq!((end.played, end.heard_text), (ms(200), 9));
        assert!(end.drained);
        // A report that comes out of order does not bring the response
        // back.
        deck.playout.ack(TAG, ms(50));
        assert!(deck.position(1_300).drained);
        assert_eq!(deck.playout.wake(), None, "nothing is left to wait for");
    }

    /// A response of 200 ms with one word, handed over whole from 1,000
    /// on, to a far end that says what it plays: of this, 120 ms.
    fn short_of_the_end(config: PlayoutConfig) -> Deck {
        let mut deck = Deck::with(config);
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.ack(TAG, ms(0));
        deck.playout.push(TAG, at(1_000), &level(200)).unwrap();
        deck.playout.mark(TAG, word(9, 150)).unwrap();
        deck.playout.finish(TAG).unwrap();
        deck.run(1_000, 1_300);
        deck.playout.ack(TAG, ms(120));
        deck
    }

    #[test]
    fn a_far_end_that_stops_short_of_the_end_is_not_waited_for_for_ever() {
        let mut deck = short_of_the_end(PlayoutConfig::new(RATE));
        // It says no more. Until its time is up, it is believed.
        assert_eq!(deck.playout.wake(), Some(at(3_200)));
        let before = deck.position(3_199);
        assert_eq!(
            (before.played, before.heard_text, before.drained),
            (ms(120), 0, false)
        );
        assert_eq!(deck.playout.wake(), Some(at(3_200)));
        // Then the response is taken to have been played to its end.
        let end = deck.position(3_200);
        assert_eq!(
            (end.played, end.heard_text, end.drained),
            (ms(200), 9, true)
        );
        assert_eq!(deck.playout.wake(), None);
        // What it says afterwards changes nothing.
        deck.playout.ack(TAG, ms(150));
        let after = deck.position(3_300);
        assert_eq!((after.played, after.drained), (ms(200), true));
    }

    #[test]
    fn a_runtime_that_calls_when_it_is_asked_to_sees_such_a_response_end() {
        let mut deck = short_of_the_end(PlayoutConfig::new(RATE));
        deck.run(1_300, 60_000);
        assert_eq!(deck.playout.wake(), None);
        assert!(deck.position(3_200).drained);
    }

    #[test]
    fn how_long_a_far_end_is_waited_for_counts_from_when_it_hears_the_end() {
        let mut deck = short_of_the_end(PlayoutConfig {
            far_buffer: ms(100),
            ack_wait: Duration::from_millis(500),
            ..PlayoutConfig::new(RATE)
        });
        // The end is heard at 1,300, with what the far end holds.
        assert_eq!(deck.playout.wake(), Some(at(1_800)));
        assert!(!deck.position(1_799).drained);
        assert!(deck.position(1_800).drained);
    }

    #[test]
    fn a_far_end_that_says_it_played_the_end_in_time_is_believed_at_once() {
        let mut deck = short_of_the_end(PlayoutConfig::new(RATE));
        deck.playout.ack(TAG, ms(200));
        let end = deck.position(1_400);
        assert_eq!((end.played, end.drained), (ms(200), true));
        assert_eq!(deck.playout.wake(), None);
    }

    #[test]
    fn a_far_end_is_waited_for_while_more_of_the_response_may_come() {
        // The response is not finished: what the far end did not play
        // of it is not taken as played, however long it takes.
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.ack(TAG, ms(0));
        deck.playout.push(TAG, at(1_000), &level(200)).unwrap();
        deck.run(1_000, 1_300);
        deck.playout.ack(TAG, ms(120));
        assert_eq!(deck.playout.wake(), None);
        let late = deck.position(90_000);
        assert_eq!((late.played, late.drained), (ms(120), false));
        // Its time runs from when the last of it is heard: the rest
        // comes late, and is handed over from then on.
        deck.playout.push(TAG, at(90_000), &level(100)).unwrap();
        deck.playout.finish(TAG).unwrap();
        deck.run(90_000, 90_200);
        assert_eq!(deck.playout.wake(), Some(at(92_100)));
        assert!(!deck.position(92_099).drained);
        let end = deck.position(92_100);
        assert_eq!((end.played, end.drained), (ms(300), true));
    }

    #[test]
    fn a_response_that_was_stopped_is_not_played_to_its_end_by_waiting() {
        let mut deck = short_of_the_end(PlayoutConfig::new(RATE));
        deck.playout.stop(TAG, 5);
        assert_eq!(deck.playout.wake(), None);
        assert_eq!(deck.playout.position(at(10_000)), None);
    }

    #[test]
    fn a_transport_cannot_have_played_what_it_was_not_given() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(100)).unwrap();
        deck.run(1_000, 1_300);
        // It claims more than it has.
        deck.playout.ack(TAG, ms(900));
        assert_eq!(deck.position(1_300).played, ms(100));
        // What it is given afterwards is not played by that claim.
        deck.playout.push(TAG, at(1_300), &level(100)).unwrap();
        deck.run(1_300, 1_500);
        let position = deck.position(1_500);
        assert_eq!((position.handed, position.played), (ms(200), ms(100)));
    }

    #[test]
    fn a_new_response_starts_afresh() {
        let mut deck = Deck::new();
        deck.playout.start(tag(7), at(1_000)).unwrap();
        deck.playout.push(tag(7), at(1_000), &level(100)).unwrap();
        deck.playout.mark(tag(7), word(50, 20)).unwrap();
        deck.playout.mark(tag(7), word(90, 5_000)).unwrap();
        deck.playout.duck(tag(7));
        deck.run(1_000, 1_100);
        assert_eq!(deck.handed[4].pcm[0], 2_500, "the old one ended lowered");
        assert_eq!(deck.position(1_100).heard_text, 50);

        // The new one is at full volume, and none of its text is heard
        // for the marks of the old.
        deck.playout.start(tag(8), at(2_000)).unwrap();
        deck.playout.push(tag(8), at(2_000), &level(40)).unwrap();
        deck.run(2_000, 2_100);
        assert_eq!(deck.handed[5].frame.tag, tag(8));
        assert_eq!(deck.handed[5].pcm[0], 10_000);
        assert_eq!(deck.handed[6].pcm[479], 10_000);
        let position = deck.position(2_100);
        assert_eq!((position.played, position.heard_text), (ms(40), 0));
    }

    #[test]
    fn the_last_frame_is_filled_up_with_silence() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(30)).unwrap();
        assert_eq!(deck.poll(1_000), 1, "the rest is no frame yet");
        deck.playout.finish(TAG).unwrap();
        assert_eq!(deck.poll(1_000), 1);
        let last = &deck.handed[1];
        assert_eq!((last.frame.samples, last.frame.last), (240, true));
        assert_eq!(last.pcm[239], 10_000);
        assert!(last.pcm[240..].iter().all(|&sample| sample == 0));
        assert_eq!(deck.position(1_030).handed, ms(30));
        assert!(deck.position(1_030).drained);
    }

    #[test]
    fn a_frame_needs_room() {
        let mut deck = Deck::new();
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(100)).unwrap();
        let mut short = [0; 479];
        assert_eq!(deck.playout.poll(at(1_000), &mut short), None);
        assert_eq!(deck.playout.buffered(), 2_400);
        // More room than a frame is no harm.
        let mut long = [7; 600];
        assert!(deck.playout.poll(at(1_000), &mut long).is_some());
        assert_eq!(long[480..], [7; 120]);
        assert_eq!(deck.playout.config().lead, Duration::from_millis(20));
    }

    #[test]
    fn with_no_lead_each_frame_goes_out_at_its_time() {
        let mut deck = Deck::with(PlayoutConfig {
            lead: Duration::ZERO,
            ducked: Gain::ratio(1, 2),
            ..PlayoutConfig::new(RATE)
        });
        deck.playout.start(TAG, at(1_000)).unwrap();
        deck.playout.push(TAG, at(1_000), &level(60)).unwrap();
        deck.run(1_000, 1_100);
        assert_eq!(deck.times(), [(1_000, 0), (1_020, 20), (1_040, 40)]);
    }
}
