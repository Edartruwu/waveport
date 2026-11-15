//! How much language-model text is ready to be spoken.

/// Decides how much of the text a language model has written so far is
/// handed to speech synthesis. Synthesis sounds best on whole sentences, and
/// text that has been handed over cannot be taken back.
pub trait TextChunker {
    /// How many bytes at the start of `pending` are ready. `done` says that
    /// the model has finished, so nothing will follow `pending`.
    ///
    /// The answer must fall on a character boundary and be at most
    /// `pending.len()`; the kernel treats anything else as 0.
    fn ready(&self, pending: &str, done: bool) -> usize;
}

/// Releases text a sentence at a time.
///
/// A sentence ends at `.`, `!`, `?` or `…` followed by white space, at a
/// line break, or at an ideographic full stop. Text that runs past
/// `max_bytes` without a sentence end is released up to its last word
/// boundary, so a long clause cannot stall the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SentenceChunker {
    /// The most text held back while waiting for a sentence to end.
    pub max_bytes: usize,
}

impl Default for SentenceChunker {
    fn default() -> Self {
        Self { max_bytes: 240 }
    }
}

impl SentenceChunker {
    /// The end of the last complete sentence in `pending`, including the
    /// white space after it.
    fn last_sentence_end(pending: &str) -> usize {
        let mut end = 0;
        let mut closes_sentence = false;
        for (at, c) in pending.char_indices() {
            let after = at + c.len_utf8();
            if matches!(c, '\n' | '。' | '！' | '？') || (closes_sentence && c.is_whitespace()) {
                end = after;
            } else {
                closes_sentence = matches!(c, '.' | '!' | '?' | '…')
                    // Closing quotes and brackets belong to the sentence.
                    || (closes_sentence && matches!(c, '"' | '\'' | ')' | ']' | '”' | '’'));
            }
        }
        end
    }

    /// The end of the last word that fits in `max_bytes`, or the last
    /// character boundary there if the text has no white space.
    fn last_word_end(self, pending: &str) -> usize {
        let mut boundary = 0;
        let mut word_end = 0;
        for (at, c) in pending.char_indices() {
            let after = at + c.len_utf8();
            if after > self.max_bytes {
                break;
            }
            boundary = after;
            if c.is_whitespace() {
                word_end = after;
            }
        }
        if word_end == 0 { boundary } else { word_end }
    }
}

impl TextChunker for SentenceChunker {
    fn ready(&self, pending: &str, done: bool) -> usize {
        if done {
            return pending.len();
        }
        let sentence = Self::last_sentence_end(pending);
        if sentence == 0 && pending.len() > self.max_bytes {
            self.last_word_end(pending)
        } else {
            sentence
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SentenceChunker, TextChunker};

    fn ready(pending: &str) -> &str {
        let at = SentenceChunker::default().ready(pending, false);
        &pending[..at]
    }

    #[test]
    fn whole_sentences_are_released() {
        assert_eq!(ready("Hello there. How are"), "Hello there. ");
        assert_eq!(ready("One. Two! Three? Four"), "One. Two! Three? ");
        assert_eq!(ready("He said \"stop.\" Then"), "He said \"stop.\" ");
        assert_eq!(ready("First line\nsecond"), "First line\n");
        assert_eq!(ready("そうです。次は"), "そうです。");
    }

    #[test]
    fn an_unfinished_sentence_is_held() {
        assert_eq!(ready("Hello there"), "");
        // The model may still be writing "3.5".
        assert_eq!(ready("It costs 3."), "");
        assert_eq!(ready("It costs 3.5 dollars"), "");
        assert_eq!(ready(""), "");
    }

    #[test]
    fn everything_is_released_when_the_model_is_done() {
        let chunker = SentenceChunker::default();
        assert_eq!(chunker.ready("Hello there", true), 11);
        assert_eq!(chunker.ready("", true), 0);
    }

    #[test]
    fn a_long_clause_is_released_at_a_word_boundary() {
        let chunker = SentenceChunker { max_bytes: 12 };
        let text = "one two three four five";
        assert_eq!(&text[..chunker.ready(text, false)], "one two ");
        let unbroken = "ééééééééééé";
        let at = chunker.ready(unbroken, false);
        assert_eq!(at, 12);
        assert!(unbroken.is_char_boundary(at));
    }
}
