//! Which text has been heard.
//!
//! Speech synthesis says how its audio lines up with its text: by this
//! much audio, this much text has been said. Those statements are
//! [`Mark`]s. The [`Ledger`] keeps the marks of the audio that has not
//! been played yet, and answers one question: with this much of the audio
//! played, how much of the text has the listener heard?
//!
//! The kernel asks it whenever a response ends or is interrupted, so that
//! the conversation records what was heard and not what was planned.

use std::collections::VecDeque;

use waveport_domain::time::MediaDur;

/// How the text before a mark lines up with the audio before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Spread {
    /// The text since the mark before is heard when the audio reaches
    /// this mark, and not before: the mark is the end of a word.
    AtOnce,
    /// The text since the mark before is spread evenly over the audio
    /// since then: the mark is the end of a stretch whose words have no
    /// times of their own.
    Evenly,
}

/// A place where a response's text and its audio meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mark {
    /// Bytes of the response's text said by then.
    pub text: u32,
    /// How much of the response's audio has been played by then.
    pub audio: MediaDur,
    /// How the text since the mark before lines up with the audio.
    pub spread: Spread,
}

/// The marks of one response, and how much of its text has been heard.
#[derive(Clone, Debug)]
pub struct Ledger {
    /// Marks the audio has not reached, earliest first.
    ahead: VecDeque<Mark>,
    capacity: usize,
    /// The last mark the audio reached.
    passed: Mark,
    /// The last mark given.
    last: Mark,
    heard: u32,
}

const START: Mark = Mark {
    text: 0,
    audio: MediaDur::ZERO,
    spread: Spread::AtOnce,
};

impl Ledger {
    /// A ledger that holds `capacity` marks ahead of the audio.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            ahead: VecDeque::with_capacity(capacity),
            capacity,
            passed: START,
            last: START,
            heard: 0,
        }
    }

    /// Forgets the response: the next mark belongs to another.
    pub fn clear(&mut self) {
        self.ahead.clear();
        self.passed = START;
        self.last = START;
        self.heard = 0;
    }

    /// How many marks are ahead of the audio.
    #[must_use]
    pub fn ahead(&self) -> usize {
        self.ahead.len()
    }

    /// Adds a mark. Marks come in order: one that is behind the last in
    /// text or in audio is moved up to it.
    ///
    /// When there is no room the last mark makes way for the new one. The
    /// text between them is then taken to be heard later than it is,
    /// which errs on the side of saying less was heard.
    pub fn mark(&mut self, mark: Mark) {
        let mark = Mark {
            text: mark.text.max(self.last.text),
            audio: mark.audio.max(self.last.audio),
            spread: mark.spread,
        };
        if self.ahead.len() >= self.capacity {
            self.ahead.pop_back();
        }
        self.ahead.push_back(mark);
        self.last = mark;
    }

    /// How many bytes of the text have been heard in full once `played`
    /// of the audio has been played. It never goes back.
    pub fn heard(&mut self, played: MediaDur) -> u32 {
        while let Some(mark) = self.ahead.front().copied() {
            if mark.audio > played {
                break;
            }
            self.passed = mark;
            self.ahead.pop_front();
        }
        let mut heard = self.passed.text;
        if let Some(next) = self.ahead.front()
            && next.spread == Spread::Evenly
        {
            // Part of the way from the last mark to the next.
            let audio = u64::from(next.audio.saturating_sub(self.passed.audio).ticks());
            let done = u64::from(played.saturating_sub(self.passed.audio).ticks());
            let text = u64::from(next.text.saturating_sub(self.passed.text));
            let part = (text * done).checked_div(audio).unwrap_or(0);
            heard = heard.saturating_add(u32::try_from(part).unwrap_or(0));
        }
        self.heard = self.heard.max(heard);
        self.heard
    }
}

#[cfg(test)]
mod tests {
    use waveport_domain::time::MediaDur;

    use super::{Ledger, Mark, Spread};

    fn word(text: u32, audio_ms: u32) -> Mark {
        Mark {
            text,
            audio: MediaDur::from_millis(audio_ms),
            spread: Spread::AtOnce,
        }
    }

    fn stretch(text: u32, audio_ms: u32) -> Mark {
        Mark {
            spread: Spread::Evenly,
            ..word(text, audio_ms)
        }
    }

    fn heard(ledger: &mut Ledger, played_ms: u32) -> u32 {
        ledger.heard(MediaDur::from_millis(played_ms))
    }

    #[test]
    fn a_word_is_heard_when_its_audio_has_been_played() {
        // "Gate twelve. " in three words, the last with its full stop.
        let mut ledger = Ledger::new(8);
        for mark in [word(5, 300), word(12, 800), word(13, 900)] {
            ledger.mark(mark);
        }
        assert_eq!(heard(&mut ledger, 0), 0);
        assert_eq!(heard(&mut ledger, 299), 0, "not in the middle of a word");
        assert_eq!(heard(&mut ledger, 300), 5);
        assert_eq!(heard(&mut ledger, 799), 5);
        assert_eq!(heard(&mut ledger, 5_000), 13);
        assert_eq!(ledger.ahead(), 0);
    }

    #[test]
    fn text_without_times_is_spread_over_its_audio() {
        // 40 bytes in the first second, 20 in the two that follow.
        let mut ledger = Ledger::new(8);
        ledger.mark(stretch(40, 1_000));
        ledger.mark(stretch(60, 3_000));
        assert_eq!(heard(&mut ledger, 0), 0);
        assert_eq!(heard(&mut ledger, 250), 10);
        assert_eq!(heard(&mut ledger, 999), 39);
        assert_eq!(heard(&mut ledger, 1_000), 40);
        assert_eq!(heard(&mut ledger, 2_000), 50);
        assert_eq!(heard(&mut ledger, 3_000), 60);
        assert_eq!(heard(&mut ledger, 9_000), 60);
    }

    #[test]
    fn what_was_heard_is_not_taken_back() {
        let mut ledger = Ledger::new(8);
        ledger.mark(stretch(40, 1_000));
        assert_eq!(heard(&mut ledger, 500), 20);
        assert_eq!(heard(&mut ledger, 100), 20);
        // A mark that is behind the last is moved up to it.
        ledger.mark(word(30, 900));
        ledger.mark(word(50, 1_500));
        assert_eq!(heard(&mut ledger, 1_000), 40);
        assert_eq!(heard(&mut ledger, 1_499), 40);
        assert_eq!(heard(&mut ledger, 1_500), 50);
    }

    #[test]
    fn with_no_room_the_newest_marks_are_merged() {
        let mut ledger = Ledger::new(3);
        for n in 1..=10 {
            ledger.mark(word(n * 10, n * 100));
        }
        assert_eq!(ledger.ahead(), 3);
        // The first two are kept. The others became one, at the end.
        assert_eq!(heard(&mut ledger, 100), 10);
        assert_eq!(heard(&mut ledger, 200), 20);
        assert_eq!(heard(&mut ledger, 999), 20, "less, not more");
        assert_eq!(heard(&mut ledger, 1_000), 100);
    }

    #[test]
    fn a_new_response_starts_from_nothing() {
        let mut ledger = Ledger::new(4);
        ledger.mark(word(10, 100));
        assert_eq!(heard(&mut ledger, 100), 10);
        ledger.clear();
        assert_eq!(heard(&mut ledger, 100), 0);
        ledger.mark(word(3, 50));
        assert_eq!(heard(&mut ledger, 100), 3);

        // A ledger made for no marks still holds one.
        let mut ledger = Ledger::new(0);
        ledger.mark(word(1, 10));
        ledger.mark(word(2, 20));
        assert_eq!(heard(&mut ledger, 20), 2);
    }
}
