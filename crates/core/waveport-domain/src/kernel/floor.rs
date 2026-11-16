//! Turn-taking: the user turns that have begun and not ended.
//!
//! A turn opens when a participant starts to speak and ends in exactly one
//! of two ways: it is committed to the conversation, or it is dismissed
//! for a stated reason. Until then everything heard from that participant
//! belongs to it. Transcripts join a turn by the audio they cover, not by
//! when they arrive, so the order in which the listener, the transcriber
//! and the turn-taking models report makes no difference to the outcome.

use alloc::string::String;

use crate::config::Timing;
use crate::fact::Dismissal;
use crate::ids::{GenId, ParticipantId, Prob, TurnId};
use crate::time::{MediaSpan, MediaTime, MonoTime};

/// A decision that is still open about speech over the agent's own speech.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Overlap {
    /// The response whose speech was lowered.
    pub(super) generation: GenId,
    /// When the overlap began.
    pub(super) since: MonoTime,
}

/// A user turn that has begun and not ended.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct OpenTurn {
    pub(super) id: TurnId,
    pub(super) who: ParticipantId,
    /// The audio the turn covers so far.
    pub(super) span: MediaSpan,
    /// When the turn began.
    pub(super) begun: MonoTime,
    pub(super) speaking: bool,
    /// Where in the audio the latest stretch of speech began.
    pub(super) voiced_from: MediaTime,
    /// When the speaker last stopped.
    pub(super) quiet_since: MonoTime,
    /// The latest end-of-turn estimate since the speaker last stopped.
    pub(super) estimate: Option<Prob>,
    /// When the turn policy said the turn is over, if it has.
    pub(super) ended: Option<MonoTime>,
    /// The words of the final transcripts so far.
    pub(super) text: String,
    /// How far into the turn's audio final transcripts reach, once one has
    /// arrived.
    pub(super) heard_through: Option<MediaTime>,
    /// The open decision about speech over the agent's, if there is one.
    pub(super) overlap: Option<Overlap>,
    /// Why the turn will be dismissed instead of committed, once speech
    /// over the agent's has been judged not to ask for the floor.
    pub(super) aside: Option<Dismissal>,
}

impl OpenTurn {
    /// A turn that begins with speech at `at`.
    pub(super) fn begin(id: TurnId, who: ParticipantId, at: MediaTime, now: MonoTime) -> Self {
        Self {
            id,
            who,
            span: MediaSpan::new(at, at),
            begun: now,
            speaking: true,
            voiced_from: at,
            quiet_since: now,
            estimate: None,
            ended: None,
            text: String::new(),
            heard_through: None,
            overlap: None,
            aside: None,
        }
    }

    /// Whether a transcript of `span` is of this turn's speech: it is, if
    /// any of it lies after the turn began.
    pub(super) fn holds(&self, span: MediaSpan) -> bool {
        span.end > self.span.start
    }

    /// Adds the words of a final transcript.
    pub(super) fn hear(&mut self, span: MediaSpan, words: &str) {
        let words = words.trim();
        if !words.is_empty() {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(words);
        }
        self.heard_through = Some(self.heard_through.map_or(span.end, |end| end.max(span.end)));
    }

    /// Whether final transcripts reach the end of the turn's speech, give
    /// or take the slack between a recogniser's last word and the end of
    /// voice activity.
    fn is_transcribed(&self, timing: &Timing) -> bool {
        self.heard_through
            .is_some_and(|through| through.after(timing.transcript_slack) >= self.span.end)
    }

    /// Whether the turn is waiting for its final transcript and for
    /// nothing else.
    pub(super) fn lacks_transcript(&self, timing: &Timing) -> bool {
        self.ended.is_some() && !self.is_transcribed(timing)
    }

    /// Whether the turn can end now: its speaker is quiet, the turn policy
    /// has said it is over, nothing is undecided about it, and its words
    /// have arrived or have had their time to.
    pub(super) fn is_over(&self, now: MonoTime, timing: &Timing) -> bool {
        let Some(ended) = self.ended else {
            return false;
        };
        !self.speaking
            && self.overlap.is_none()
            && (self.is_transcribed(timing) || now.since(ended) >= timing.transcript_wait)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{OpenTurn, Overlap};
    use crate::config::Timing;
    use crate::ids::{GenId, ParticipantId, TurnId};
    use crate::time::{MediaSpan, MediaTime, MonoTime};

    fn span(from_ms: u64, to_ms: u64) -> MediaSpan {
        MediaSpan::new(
            MediaTime::from_millis(from_ms),
            MediaTime::from_millis(to_ms),
        )
    }

    /// A turn whose speaker spoke from 1.0 s to 2.2 s and has stopped.
    fn quiet_turn() -> OpenTurn {
        let mut turn = OpenTurn::begin(
            TurnId(1),
            ParticipantId(1),
            MediaTime::from_millis(1_000),
            MonoTime::from_millis(1_000),
        );
        turn.speaking = false;
        turn.span.end = MediaTime::from_millis(2_200);
        turn
    }

    #[test]
    fn transcripts_join_by_the_audio_they_cover() {
        let turn = quiet_turn();
        assert!(turn.holds(span(1_000, 2_200)));
        assert!(turn.holds(span(900, 1_100)));
        assert!(!turn.holds(span(200, 1_000)), "it ended as the turn began");
    }

    #[test]
    fn words_accumulate_in_order() {
        let mut turn = quiet_turn();
        turn.hear(span(1_000, 1_600), " where is ");
        turn.hear(span(1_600, 2_200), "my order");
        turn.hear(span(2_200, 2_200), "   ");
        assert_eq!(turn.text, "where is my order");
        assert_eq!(turn.heard_through, Some(MediaTime::from_millis(2_200)));
    }

    #[test]
    fn a_turn_ends_only_when_everything_about_it_is_settled() {
        let timing = Timing::default();
        let now = MonoTime::from_millis(2_300);
        let mut turn = quiet_turn();
        assert!(!turn.is_over(now, &timing), "the policy has not ended it");

        turn.ended = Some(now);
        assert!(turn.lacks_transcript(&timing));
        assert!(!turn.is_over(now, &timing), "its words have not arrived");

        // A transcript that stops within the slack of the end counts.
        turn.hear(span(1_000, 1_700), "where is my order");
        assert!(!turn.lacks_transcript(&timing));
        assert!(turn.is_over(now, &timing));

        turn.overlap = Some(Overlap {
            generation: GenId(1),
            since: now,
        });
        assert!(!turn.is_over(now, &timing), "the overlap is undecided");
        turn.overlap = None;
        turn.speaking = true;
        assert!(!turn.is_over(now, &timing), "the speaker went on");
    }

    #[test]
    fn a_turn_without_words_ends_when_they_have_had_their_time() {
        let timing = Timing::default();
        let mut turn = quiet_turn();
        turn.hear(span(1_000, 1_200), "where");
        turn.ended = Some(MonoTime::from_millis(2_300));
        assert!(
            turn.lacks_transcript(&timing),
            "a second of speech is missing"
        );
        let early = MonoTime::from_millis(2_300).after(Duration::from_millis(1_999));
        assert!(!turn.is_over(early, &timing));
        let late = MonoTime::from_millis(2_300).after(timing.transcript_wait);
        assert!(turn.is_over(late, &timing));
    }
}
