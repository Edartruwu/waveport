//! What speech over the agent's own speech does.

use core::time::Duration;

use crate::event::OverlapVerdict;

/// What the kernel knows about speech that overlaps the agent's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlapInput {
    /// The classifier's verdict, if it has arrived.
    pub verdict: Option<OverlapVerdict>,
    /// Whether the user is still speaking.
    pub speaking: bool,
    /// How long ago the overlap began.
    pub elapsed: Duration,
}

/// What to do about speech that overlaps the agent's. Until a decision, the
/// agent keeps speaking at a lowered volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlapDecision {
    /// The user takes the floor: stop the agent.
    Interrupt,
    /// The agent keeps the floor: restore its volume.
    Resume,
    /// Ask again after this long, or sooner if something changes.
    Wait(Duration),
}

/// Decides what speech over the agent's own speech does.
pub trait InterruptionPolicy {
    /// Asked when the overlap begins, when the verdict arrives, when the
    /// user stops speaking, and when a wait it asked for is over.
    fn decide(&self, input: &OverlapInput) -> OverlapDecision;
}

/// Follows the classifier's verdict when there is one, and otherwise judges
/// by voice activity alone: speech that lasts is a barge-in, speech that
/// stops early was a backchannel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceActivityInterruption {
    /// How long overlapping speech must last, without a verdict, to count
    /// as a barge-in.
    pub min_speech: Duration,
}

impl Default for VoiceActivityInterruption {
    fn default() -> Self {
        Self {
            min_speech: Duration::from_millis(500),
        }
    }
}

impl InterruptionPolicy for VoiceActivityInterruption {
    fn decide(&self, input: &OverlapInput) -> OverlapDecision {
        match input.verdict {
            Some(OverlapVerdict::BargeIn) => OverlapDecision::Interrupt,
            Some(OverlapVerdict::Backchannel | OverlapVerdict::Noise) => OverlapDecision::Resume,
            None if !input.speaking => OverlapDecision::Resume,
            None => match self.min_speech.checked_sub(input.elapsed) {
                Some(left) if !left.is_zero() => OverlapDecision::Wait(left),
                _ => OverlapDecision::Interrupt,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{InterruptionPolicy, OverlapDecision, OverlapInput, VoiceActivityInterruption};
    use crate::event::OverlapVerdict;

    fn decide(verdict: Option<OverlapVerdict>, speaking: bool, elapsed_ms: u64) -> OverlapDecision {
        VoiceActivityInterruption::default().decide(&OverlapInput {
            verdict,
            speaking,
            elapsed: Duration::from_millis(elapsed_ms),
        })
    }

    #[test]
    fn a_verdict_decides_whatever_the_voice_activity() {
        let barge_in = Some(OverlapVerdict::BargeIn);
        assert_eq!(decide(barge_in, false, 0), OverlapDecision::Interrupt);
        for verdict in [OverlapVerdict::Backchannel, OverlapVerdict::Noise] {
            assert_eq!(decide(Some(verdict), true, 900), OverlapDecision::Resume);
        }
    }

    #[test]
    fn without_a_verdict_lasting_speech_is_a_barge_in() {
        assert_eq!(
            decide(None, true, 200),
            OverlapDecision::Wait(Duration::from_millis(300))
        );
        assert_eq!(decide(None, true, 500), OverlapDecision::Interrupt);
        assert_eq!(decide(None, false, 200), OverlapDecision::Resume);
    }
}
