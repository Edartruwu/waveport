//! Where the audio sent to a provider is on the media clock.
//!
//! A speech recogniser says when words were spoken in its own time: so
//! many seconds into the audio it was sent. A turn is joined to its words
//! by the session's media clock. The two differ whenever the audio sent
//! had a gap in it, because the provider's time does not count gaps. A
//! [`Timeline`] notes each piece of audio as it is sent and turns the
//! provider's times back into the session's.

use core::time::Duration;
use std::collections::VecDeque;

use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};

/// How many stretches of unbroken audio are remembered. A time before the
/// oldest is worked out from the oldest.
const STRETCHES: usize = 32;

/// The start of a stretch of audio that was sent without a gap.
#[derive(Clone, Copy, Debug)]
struct Stretch {
    /// How much audio had been sent before it, in ticks.
    offset: u64,
    /// Where it begins on the media clock.
    at: MediaTime,
}

/// The audio sent on one stream, against the media clock.
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    stretches: VecDeque<Stretch>,
    /// How much audio has been sent, in ticks.
    sent: u64,
    /// Where the audio sent so far ends.
    end: MediaTime,
    /// Where the audio sent since the window was last taken begins.
    window: Option<MediaTime>,
}

impl Timeline {
    /// A timeline for a stream nothing has been sent on.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes a piece of audio that is sent: where it begins on the media
    /// clock and how long it is.
    pub fn sent(&mut self, at: MediaTime, length: MediaDur) {
        if self.stretches.is_empty() || at != self.end {
            if self.stretches.len() == STRETCHES {
                self.stretches.pop_front();
            }
            self.stretches.push_back(Stretch {
                offset: self.sent,
                at,
            });
        }
        self.window.get_or_insert(at);
        self.sent += u64::from(length.ticks());
        self.end = at.after(length);
    }

    /// How much audio has been sent.
    #[must_use]
    pub const fn total(&self) -> Duration {
        Duration::from_nanos(self.sent * 62_500 / 3)
    }

    /// Where the audio sent so far ends on the media clock.
    #[must_use]
    pub const fn end(&self) -> MediaTime {
        self.end
    }

    /// The place on the media clock of the audio `offset` into the
    /// stream, as the provider counts: without the gaps.
    #[must_use]
    pub fn at(&self, offset: Duration) -> MediaTime {
        // 48,000 ticks in 1,000,000,000 ns.
        let ticks = u64::try_from(offset.as_nanos() * 3 / 62_500).unwrap_or(u64::MAX);
        let stretch = self
            .stretches
            .iter()
            .rev()
            .find(|stretch| stretch.offset <= ticks)
            .or_else(|| self.stretches.front());
        let Some(stretch) = stretch else {
            return MediaTime::ZERO;
        };
        let into = ticks.saturating_sub(stretch.offset);
        MediaTime::from_ticks(stretch.at.ticks().saturating_add(into))
    }

    /// The stretch of the media clock between two of the provider's
    /// times.
    #[must_use]
    pub fn span(&self, from: Duration, to: Duration) -> MediaSpan {
        let start = self.at(from);
        MediaSpan::new(start, self.at(to).max(start))
    }

    /// The stretch of the media clock that the audio sent since this was
    /// last called covers, for a provider that gives no times: its words
    /// were spoken somewhere in there. `None` when nothing was sent.
    pub fn take_window(&mut self) -> Option<MediaSpan> {
        let window = self.window();
        self.window = None;
        window
    }

    /// The same stretch, left where it is: what words that are not final
    /// yet were spoken in.
    #[must_use]
    pub fn window(&self) -> Option<MediaSpan> {
        let start = self.window?;
        Some(MediaSpan::new(start, self.end.max(start)))
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};

    use super::{STRETCHES, Timeline};

    const fn ms(millis: u64) -> MediaTime {
        MediaTime::from_millis(millis)
    }

    /// Sends `count` pieces of 20 ms each from `from_ms` on.
    fn send(timeline: &mut Timeline, from_ms: u64, count: u64) {
        for n in 0..count {
            timeline.sent(ms(from_ms + 20 * n), MediaDur::from_millis(20));
        }
    }

    #[test]
    fn without_gaps_the_two_clocks_differ_by_where_the_stream_began() {
        let mut timeline = Timeline::new();
        assert_eq!(timeline.at(Duration::from_secs(1)), MediaTime::ZERO);
        send(&mut timeline, 5_000, 100);
        assert_eq!(timeline.total(), Duration::from_secs(2));
        assert_eq!(timeline.end(), ms(7_000));
        assert_eq!(timeline.at(Duration::ZERO), ms(5_000));
        assert_eq!(timeline.at(Duration::from_millis(1_234)), ms(6_234));
        assert_eq!(
            timeline.span(Duration::from_millis(500), Duration::from_millis(900)),
            MediaSpan::new(ms(5_500), ms(5_900))
        );
    }

    #[test]
    fn a_gap_in_the_audio_is_not_in_the_providers_time() {
        let mut timeline = Timeline::new();
        send(&mut timeline, 1_000, 50); // 1 s of audio, to 2,000.
        send(&mut timeline, 10_000, 50); // Another, from 10,000.
        assert_eq!(timeline.total(), Duration::from_secs(2));
        // Half a second in is in the first stretch.
        assert_eq!(timeline.at(Duration::from_millis(500)), ms(1_500));
        // One and a half seconds in is half a second into the second.
        assert_eq!(timeline.at(Duration::from_millis(1_500)), ms(10_500));
        // The provider's second second starts where the second stretch does.
        assert_eq!(timeline.at(Duration::from_secs(1)), ms(10_000));
        // A time just past what was sent is taken from the last stretch.
        assert_eq!(timeline.at(Duration::from_millis(2_100)), ms(11_100));
        // A span never ends before it begins.
        let span = timeline.span(Duration::from_secs(1), Duration::from_millis(500));
        assert_eq!(span, MediaSpan::new(ms(10_000), ms(10_000)));
    }

    #[test]
    fn the_window_is_the_audio_sent_since_it_was_taken() {
        let mut timeline = Timeline::new();
        assert_eq!(timeline.take_window(), None);
        send(&mut timeline, 1_000, 10);
        // Looking at it leaves it there.
        assert_eq!(
            timeline.window(),
            Some(MediaSpan::new(ms(1_000), ms(1_200)))
        );
        assert_eq!(
            timeline.take_window(),
            Some(MediaSpan::new(ms(1_000), ms(1_200)))
        );
        assert_eq!(timeline.window(), None);
        assert_eq!(timeline.take_window(), None);
        send(&mut timeline, 1_200, 5);
        send(&mut timeline, 4_000, 5);
        assert_eq!(
            timeline.take_window(),
            Some(MediaSpan::new(ms(1_200), ms(4_100)))
        );
    }

    #[test]
    fn only_so_many_gaps_are_remembered() {
        let mut timeline = Timeline::new();
        let stretches = u64::try_from(STRETCHES).unwrap() + 8;
        for n in 0..stretches {
            // 20 ms of audio every second.
            send(&mut timeline, 1_000 * n, 1);
        }
        // The newest are exact.
        let last = stretches - 1;
        assert_eq!(
            timeline.at(Duration::from_millis(20 * last)),
            ms(1_000 * last)
        );
        // The forgotten ones are worked out from the oldest that is kept,
        // which is wrong by the gaps since, and not far in the future.
        let forgotten = timeline.at(Duration::ZERO);
        assert_eq!(forgotten, ms(1_000 * 8));
    }
}
