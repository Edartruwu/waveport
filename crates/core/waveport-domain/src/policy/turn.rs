//! When a user turn has ended.

use core::time::Duration;

use crate::ids::Prob;
use crate::time::MediaDur;

/// What the kernel knows about a turn whose speaker has gone quiet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnInput {
    /// The latest end-of-turn estimate for the turn, if one has arrived
    /// since the speaker last stopped.
    pub estimate: Option<Prob>,
    /// How long the speaker has been quiet.
    pub silence: Duration,
    /// How much speech the turn holds so far.
    pub speech: MediaDur,
}

/// What to do with a turn whose speaker has gone quiet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnDecision {
    /// The turn has ended.
    End,
    /// Ask again once the speaker has been quiet this much longer. Speech
    /// in the meantime continues the turn.
    Wait(Duration),
}

/// Decides when a user turn has ended.
pub trait TurnPolicy {
    /// Asked when the speaker stops, when an estimate arrives, and when a
    /// wait it asked for is over.
    fn decide(&self, input: &TurnInput) -> TurnDecision;
}

/// Ends a turn when the end-of-turn model says so.
///
/// When the model says the speaker is not finished, the turn ends anyway
/// after a longer silence. When no estimate arrives, the turn ends after a
/// plain silence, as an endpointer without a model would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EouThreshold {
    /// The estimate at or above which the turn ends.
    pub threshold: Prob,
    /// The silence that ends a turn the model calls unfinished.
    pub unfinished_silence: Duration,
    /// The silence that ends a turn for which no estimate has arrived.
    pub fallback_silence: Duration,
}

impl Default for EouThreshold {
    fn default() -> Self {
        Self {
            threshold: Prob::from_basis_points(5_000),
            unfinished_silence: Duration::from_secs(3),
            fallback_silence: Duration::from_millis(800),
        }
    }
}

impl TurnPolicy for EouThreshold {
    fn decide(&self, input: &TurnInput) -> TurnDecision {
        let limit = match input.estimate {
            Some(estimate) if estimate >= self.threshold => return TurnDecision::End,
            Some(_) => self.unfinished_silence,
            None => self.fallback_silence,
        };
        match limit.checked_sub(input.silence) {
            Some(left) if !left.is_zero() => TurnDecision::Wait(left),
            _ => TurnDecision::End,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{EouThreshold, TurnDecision, TurnInput, TurnPolicy};
    use crate::ids::Prob;
    use crate::time::MediaDur;

    fn decide(estimate: Option<f32>, silence_ms: u64) -> TurnDecision {
        EouThreshold::default().decide(&TurnInput {
            estimate: estimate.map(Prob::from_f32),
            silence: Duration::from_millis(silence_ms),
            speech: MediaDur::from_millis(900),
        })
    }

    #[test]
    fn a_confident_estimate_ends_the_turn_at_once() {
        assert_eq!(decide(Some(0.9), 0), TurnDecision::End);
        assert_eq!(decide(Some(0.5), 0), TurnDecision::End);
    }

    #[test]
    fn an_unfinished_turn_ends_after_a_long_silence() {
        assert_eq!(
            decide(Some(0.2), 1_000),
            TurnDecision::Wait(Duration::from_secs(2))
        );
        assert_eq!(decide(Some(0.2), 3_000), TurnDecision::End);
    }

    #[test]
    fn without_an_estimate_plain_silence_ends_the_turn() {
        assert_eq!(
            decide(None, 300),
            TurnDecision::Wait(Duration::from_millis(500))
        );
        assert_eq!(decide(None, 800), TurnDecision::End);
        assert_eq!(decide(None, 5_000), TurnDecision::End);
    }
}
