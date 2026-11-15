//! Responses: the work under way on behalf of an agent.
//!
//! A response's text passes through the kernel on its way from the
//! language model to speech synthesis. The kernel releases it a sentence
//! at a time and never more than the listener is about to hear, and it
//! knows at every moment how much has been heard. That is what lets it
//! record exactly the heard part when the response is cut off.

use alloc::string::String;

use super::view::GenerationRole;
use crate::ids::{AgentId, ContextVersion, OpTag};
use crate::time::{MediaDur, MonoTime};

/// Where a response's text comes from, and whether more is coming.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Source {
    /// A language model that is still writing.
    Writing,
    /// A language model that was asked to stop writing for now.
    Paused,
    /// A language model that has finished.
    Written,
    /// The application: the text was given in full.
    Given,
}

/// How far speech synthesis has got with a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Synthesis {
    /// It may be given more text.
    Open,
    /// It has been told that no more text follows.
    Closing,
    /// It has delivered its last audio.
    Done,
}

/// Whether a response's speech is being played.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Playback {
    /// Not yet: nothing has been released, or someone else is speaking.
    Waiting,
    /// Yes.
    Playing,
    /// Yes, at a lowered volume, because someone is speaking over it.
    Ducked,
}

/// A response under way.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Generation {
    pub(super) tag: OpTag,
    pub(super) agent: AgentId,
    pub(super) role: GenerationRole,
    /// The conversation the response was computed from.
    pub(super) context: ContextVersion,
    pub(super) source: Source,
    /// Everything the response is going to say, as far as it is known.
    pub(super) planned: String,
    /// Bytes of `planned` handed to synthesis.
    pub(super) released: usize,
    pub(super) synthesis: Synthesis,
    /// Bytes of `planned` synthesis has delivered audio for.
    pub(super) synthesized: usize,
    /// How much audio synthesis has delivered.
    pub(super) audio: MediaDur,
    pub(super) playback: Playback,
    /// How much audio has been handed to the transport.
    pub(super) handed: MediaDur,
    /// Bytes of `planned` the listener has heard in full.
    pub(super) heard: usize,
    /// Bytes of `planned` already journaled as heard.
    pub(super) recorded: usize,
    /// When the response counts as stalled, unless it makes progress
    /// before then.
    pub(super) stall_at: MonoTime,
}

impl Generation {
    /// A response that has produced nothing yet, with room reserved for
    /// `reserve` bytes of text.
    pub(super) fn begin(
        tag: OpTag,
        agent: AgentId,
        context: ContextVersion,
        source: Source,
        reserve: usize,
    ) -> Self {
        Self {
            tag,
            agent,
            role: GenerationRole::Floor,
            context,
            source,
            planned: String::with_capacity(reserve),
            released: 0,
            synthesis: Synthesis::Open,
            synthesized: 0,
            audio: MediaDur::ZERO,
            playback: Playback::Waiting,
            handed: MediaDur::ZERO,
            heard: 0,
            recorded: 0,
            stall_at: MonoTime::ZERO,
        }
    }

    /// Whether no more text will be added to `planned`.
    pub(super) fn is_written(&self) -> bool {
        matches!(self.source, Source::Written | Source::Given)
    }

    /// Whether the listener can hear the response now.
    pub(super) fn is_audible(&self) -> bool {
        self.playback != Playback::Waiting
    }

    /// The text not yet handed to synthesis.
    pub(super) fn pending(&self) -> &str {
        self.planned.get(self.released..).unwrap_or("")
    }

    /// Whether more text may be handed to synthesis: the audio of what it
    /// already has has arrived, and no more than `credit` of audio is
    /// waiting to be played. Without this, an interruption would waste
    /// everything synthesised ahead of the listener.
    pub(super) fn may_release(&self, credit: MediaDur) -> bool {
        self.synthesis == Synthesis::Open
            && self.synthesized >= self.released
            && self.audio.saturating_sub(self.handed) <= credit
    }

    /// Records how much of the released text has been heard. The report
    /// comes from outside, so it is held to what was released, to a
    /// character boundary, and to never going backwards.
    pub(super) fn hear(&mut self, heard: usize) {
        let mut heard = heard.min(self.released);
        while !self.planned.is_char_boundary(heard) {
            heard = heard.saturating_sub(1);
        }
        self.heard = self.heard.max(heard);
    }

    /// What the listener heard: all of it when the response played to its
    /// end, and otherwise the part reported as heard.
    pub(super) fn heard_text(&self, played_out: bool) -> &str {
        let heard = if played_out {
            &self.planned
        } else {
            self.planned.get(..self.heard).unwrap_or("")
        };
        heard.trim_end()
    }

    /// Whether `heard`, a prefix of the response, is less than all it was
    /// going to say.
    pub(super) fn is_cut_at(&self, heard: &str) -> bool {
        !self.is_written() || heard.len() < self.planned.trim_end().len()
    }
}

#[cfg(test)]
mod tests {
    use super::{Generation, Playback, Source, Synthesis};
    use crate::ids::{AgentId, ContextVersion, Epoch, GenId, OpTag};
    use crate::time::MediaDur;

    fn response(text: &str) -> Generation {
        let tag = OpTag {
            generation: GenId(1),
            epoch: Epoch(0),
        };
        let mut generation =
            Generation::begin(tag, AgentId(0), ContextVersion(1), Source::Writing, 64);
        generation.planned.push_str(text);
        generation
    }

    #[test]
    fn text_is_released_only_as_fast_as_it_is_heard() {
        let credit = MediaDur::from_millis(3_000);
        let mut generation = response("One. Two. Three.");
        assert_eq!(generation.pending(), "One. Two. Three.");
        assert!(generation.may_release(credit));

        // The first sentence is with synthesis: wait for its audio.
        generation.released = 5;
        assert_eq!(generation.pending(), "Two. Three.");
        assert!(!generation.may_release(credit));
        generation.synthesized = 5;
        generation.audio = MediaDur::from_millis(2_000);
        assert!(generation.may_release(credit));

        // Four seconds of audio are waiting: hold the rest.
        generation.audio = MediaDur::from_millis(4_000);
        assert!(!generation.may_release(credit));
        generation.handed = MediaDur::from_millis(1_000);
        assert!(generation.may_release(credit));

        generation.synthesis = Synthesis::Closing;
        assert!(!generation.may_release(credit));
    }

    #[test]
    fn what_was_heard_is_held_to_what_was_released() {
        let mut generation = response("Tschüß, bis morgen.");
        generation.released = 9;
        // Byte 5 is inside the "ü".
        generation.hear(5);
        assert_eq!(generation.heard, 4, "back to a character boundary");
        generation.hear(3);
        assert_eq!(generation.heard, 4, "never backwards");
        generation.hear(400);
        assert_eq!(generation.heard, 9, "never past what was released");
        assert_eq!(generation.heard_text(false), "Tschüß,");
    }

    #[test]
    fn a_response_is_cut_unless_all_of_it_was_heard() {
        let mut generation = response("Gate twelve. ");
        generation.released = 13;
        generation.hear(4);
        assert!(generation.is_cut_at(generation.heard_text(false)));
        generation.hear(13);
        assert_eq!(generation.heard_text(false), "Gate twelve.");
        assert!(
            generation.is_cut_at("Gate twelve."),
            "the model may still add to it"
        );
        generation.source = Source::Written;
        assert!(!generation.is_cut_at("Gate twelve."));
        assert_eq!(generation.heard_text(true), "Gate twelve.");

        assert!(!generation.is_audible());
        generation.playback = Playback::Ducked;
        assert!(generation.is_audible());
    }
}
