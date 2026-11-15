//! From how likely each frame is to hold speech, to where speech begins
//! and ends.
//!
//! A model gives a likelihood for every frame. The kernel wants two
//! things: when someone began to speak and when they stopped. In between
//! lies a little judgement, which is all this module holds. Speech begins
//! when frames have been likely for long enough that a click does not
//! count, and it ends when they have been unlikely for long enough that a
//! pause for breath does not count. The two thresholds differ, so that a
//! likelihood that hovers around one of them does not flicker.
//!
//! The times reported are the times of the audio, not of the decision:
//! speech began where its first likely frame begins, and ended where the
//! quiet that followed it begins.

use waveport_domain::event::VadEdge;
use waveport_domain::time::{MediaDur, MediaTime};

/// How frames are judged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeConfig {
    /// The likelihood from which a frame counts as speech when nobody is
    /// speaking.
    pub on: f32,
    /// The likelihood below which a frame counts as quiet while someone
    /// is speaking.
    pub off: f32,
    /// How long frames have to be speech before speech has begun.
    pub onset: MediaDur,
    /// How long frames have to be quiet before speech has ended.
    pub hangover: MediaDur,
}

impl Default for EdgeConfig {
    /// Speech from a likelihood of one half, quiet below 0.35, an onset of
    /// two frames of 32 ms and a hangover of 200 ms.
    fn default() -> Self {
        Self {
            on: 0.5,
            off: 0.35,
            onset: MediaDur::from_millis(64),
            hangover: MediaDur::from_millis(200),
        }
    }
}

/// Finds where the speech of one stream begins and ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edges {
    config: EdgeConfig,
    speaking: bool,
    /// Where the frames that may be the beginning of speech begin, while
    /// nobody is speaking; where the quiet that may be its end begins,
    /// while someone is.
    since: Option<MediaTime>,
    /// Where the last frame that was judged ends.
    end: Option<MediaTime>,
}

impl Edges {
    /// A stream in which nobody is speaking yet.
    #[must_use]
    pub const fn new(config: EdgeConfig) -> Self {
        Self {
            config,
            speaking: false,
            since: None,
            end: None,
        }
    }

    /// Whether someone is speaking, as far as the frames so far say.
    #[must_use]
    pub const fn speaking(&self) -> bool {
        self.speaking
    }

    /// Takes the likelihood `p` that the frame which begins at `at` and is
    /// `length` long holds speech, and tells `edge` what follows from it:
    /// nothing, the beginning of speech, or its end. A frame that does not
    /// follow the one before it can end speech and begin it again.
    pub fn push(
        &mut self,
        at: MediaTime,
        length: MediaDur,
        p: f32,
        mut edge: impl FnMut(VadEdge, MediaTime),
    ) {
        // Audio that is missing is not speech. If enough of it is missing,
        // whoever was speaking has stopped as far as anyone can tell.
        if let Some(end) = self.end
            && at > end
        {
            let missing = at.since(end);
            if self.speaking {
                let quiet = *self.since.get_or_insert(end);
                if at.since(quiet) >= self.config.hangover {
                    self.speaking = false;
                    self.since = None;
                    edge(VadEdge::End, quiet);
                }
            } else if missing > MediaDur::ZERO {
                self.since = None;
            }
        }
        let frame_end = at.after(length);
        self.end = Some(frame_end);

        if self.speaking {
            if p >= self.config.off {
                self.since = None;
                return;
            }
            let quiet = *self.since.get_or_insert(at);
            if frame_end.since(quiet) >= self.config.hangover {
                self.speaking = false;
                self.since = None;
                edge(VadEdge::End, quiet);
            }
        } else {
            if p < self.config.on {
                self.since = None;
                return;
            }
            let began = *self.since.get_or_insert(at);
            if frame_end.since(began) >= self.config.onset {
                self.speaking = true;
                self.since = None;
                edge(VadEdge::Start, began);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use waveport_domain::event::VadEdge;
    use waveport_domain::time::{MediaDur, MediaTime};

    use super::{EdgeConfig, Edges};

    const FRAME: u32 = 32;

    /// Feeds frames of 32 ms, one likelihood each, from `from_ms`, and
    /// returns the edges with their times in milliseconds.
    fn feed(edges: &mut Edges, from_ms: u64, likelihoods: &[f32]) -> Vec<(VadEdge, u64)> {
        let mut found = Vec::new();
        for (n, p) in likelihoods.iter().enumerate() {
            let at = MediaTime::from_millis(from_ms + n as u64 * u64::from(FRAME));
            edges.push(at, MediaDur::from_millis(FRAME), *p, |edge, at| {
                found.push((edge, at.ticks() / 48));
            });
        }
        found
    }

    fn fresh() -> Edges {
        Edges::new(EdgeConfig::default())
    }

    #[test]
    fn speech_begins_where_its_first_frame_does() {
        let mut edges = fresh();
        // Quiet, then speech from the third frame on. Two frames of
        // speech make an onset, and it is dated to the first of them.
        let found = feed(&mut edges, 0, &[0.1, 0.2, 0.9, 0.8, 0.9]);
        assert_eq!(found, [(VadEdge::Start, 64)]);
        assert!(edges.speaking());
    }

    #[test]
    fn a_click_is_not_speech() {
        let mut edges = fresh();
        let found = feed(&mut edges, 0, &[0.1, 0.9, 0.1, 0.9, 0.1, 0.1]);
        assert_eq!(found, []);
        assert!(!edges.speaking());
    }

    #[test]
    fn speech_ends_where_the_quiet_after_it_begins() {
        let mut edges = fresh();
        let _ = feed(&mut edges, 0, &[0.9, 0.9]);
        // Seven quiet frames are 224 ms: the hangover of 200 ms is over
        // in the seventh, and the end is dated to the first.
        let found = feed(&mut edges, 64, &[0.1; 7]);
        assert_eq!(found, [(VadEdge::End, 64)]);
        assert!(!edges.speaking());
    }

    #[test]
    fn a_pause_for_breath_is_not_the_end() {
        let mut edges = fresh();
        let _ = feed(&mut edges, 0, &[0.9, 0.9]);
        // Six quiet frames, 192 ms, and then speech again.
        let found = feed(&mut edges, 64, &[0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.9, 0.1]);
        assert_eq!(found, []);
        assert!(edges.speaking());
    }

    #[test]
    fn a_likelihood_between_the_thresholds_changes_nothing() {
        let mut edges = fresh();
        // 0.4 does not begin speech.
        assert_eq!(feed(&mut edges, 0, &[0.4; 10]), []);
        // And it does not end it.
        let found = feed(
            &mut edges,
            320,
            &[0.9, 0.9, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4],
        );
        assert_eq!(found, [(VadEdge::Start, 320)]);
        assert!(edges.speaking());
    }

    #[test]
    fn audio_that_is_missing_ends_speech() {
        let mut edges = fresh();
        let _ = feed(&mut edges, 0, &[0.9, 0.9]);
        // The stream breaks off and begins again a second later, with
        // speech. What was being said ended where the audio did, and
        // something new begins.
        let found = feed(&mut edges, 1_064, &[0.9, 0.9]);
        assert_eq!(found, [(VadEdge::End, 64), (VadEdge::Start, 1_064)]);
    }

    #[test]
    fn a_frame_that_is_lost_is_not_the_end() {
        let mut edges = fresh();
        let _ = feed(&mut edges, 0, &[0.9, 0.9]);
        // One frame of 32 ms never came.
        let found = feed(&mut edges, 96, &[0.9, 0.9]);
        assert_eq!(found, []);
        assert!(edges.speaking());
        // Nor does a lost frame join two clicks into speech.
        let mut edges = fresh();
        let _ = feed(&mut edges, 0, &[0.9]);
        let found = feed(&mut edges, 64, &[0.9]);
        assert_eq!(found, []);
    }
}
