//! The decisions an application may replace.
//!
//! A policy is a pure function of what the kernel shows it. It holds no
//! session state, performs no I/O and reads no clock, so the kernel stays
//! deterministic whatever policy is plugged in. The kernel decides when to
//! ask; the policy only answers.

mod addressee;
mod chunker;
mod gate;
mod interruption;
mod turn;

use alloc::boxed::Box;

pub use addressee::{AddressedByName, AddresseeInput, AddresseePolicy, AlwaysAddressed};
pub use chunker::{SentenceChunker, TextChunker};
pub use gate::{GateInput, SpecGate, ToolGate};
pub use interruption::{
    InterruptionPolicy, OverlapDecision, OverlapInput, VoiceActivityInterruption,
};
pub use turn::{EouThreshold, TurnDecision, TurnInput, TurnPolicy};

/// The set of policies a kernel runs with.
///
/// With concrete types every call is resolved at compile time. Use
/// [`BoxedPolicies`] where the types are only known at run time.
pub trait Policies {
    /// Decides when a user turn has ended.
    type Turn: TurnPolicy + ?Sized;
    /// Decides what speech over the agent's own speech does.
    type Interruption: InterruptionPolicy + ?Sized;
    /// Decides whether a user turn is meant for the agent.
    type Addressee: AddresseePolicy + ?Sized;
    /// Decides whether a tool call may be dispatched yet.
    type Gate: ToolGate + ?Sized;
    /// Decides how much language-model text is ready to be spoken.
    type Chunker: TextChunker + ?Sized;

    /// The turn policy.
    fn turn(&self) -> &Self::Turn;
    /// The interruption policy.
    fn interruption(&self) -> &Self::Interruption;
    /// The addressee policy.
    fn addressee(&self) -> &Self::Addressee;
    /// The tool gate.
    fn gate(&self) -> &Self::Gate;
    /// The text chunker.
    fn chunker(&self) -> &Self::Chunker;
}

/// The policies waveport ships with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DefaultPolicies {
    /// Ends a turn on the end-of-turn estimate, or on silence without one.
    pub turn: EouThreshold,
    /// Follows the overlap verdict, or voice activity without one.
    pub interruption: VoiceActivityInterruption,
    /// Treats every turn as meant for the agent.
    pub addressee: AlwaysAddressed,
    /// Applies the gate each tool declares.
    pub gate: SpecGate,
    /// Releases text a sentence at a time.
    pub chunker: SentenceChunker,
}

impl Policies for DefaultPolicies {
    type Turn = EouThreshold;
    type Interruption = VoiceActivityInterruption;
    type Addressee = AlwaysAddressed;
    type Gate = SpecGate;
    type Chunker = SentenceChunker;

    fn turn(&self) -> &Self::Turn {
        &self.turn
    }

    fn interruption(&self) -> &Self::Interruption {
        &self.interruption
    }

    fn addressee(&self) -> &Self::Addressee {
        &self.addressee
    }

    fn gate(&self) -> &Self::Gate {
        &self.gate
    }

    fn chunker(&self) -> &Self::Chunker {
        &self.chunker
    }
}

/// Policies chosen at run time, for language bindings and for tests that
/// swap one policy.
pub struct BoxedPolicies {
    /// Decides when a user turn has ended.
    pub turn: Box<dyn TurnPolicy + Send>,
    /// Decides what speech over the agent's own speech does.
    pub interruption: Box<dyn InterruptionPolicy + Send>,
    /// Decides whether a user turn is meant for the agent.
    pub addressee: Box<dyn AddresseePolicy + Send>,
    /// Decides whether a tool call may be dispatched yet.
    pub gate: Box<dyn ToolGate + Send>,
    /// Decides how much language-model text is ready to be spoken.
    pub chunker: Box<dyn TextChunker + Send>,
}

impl Default for BoxedPolicies {
    fn default() -> Self {
        DefaultPolicies::default().into()
    }
}

impl From<DefaultPolicies> for BoxedPolicies {
    fn from(policies: DefaultPolicies) -> Self {
        Self {
            turn: Box::new(policies.turn),
            interruption: Box::new(policies.interruption),
            addressee: Box::new(policies.addressee),
            gate: Box::new(policies.gate),
            chunker: Box::new(policies.chunker),
        }
    }
}

impl Policies for BoxedPolicies {
    type Turn = dyn TurnPolicy + Send;
    type Interruption = dyn InterruptionPolicy + Send;
    type Addressee = dyn AddresseePolicy + Send;
    type Gate = dyn ToolGate + Send;
    type Chunker = dyn TextChunker + Send;

    fn turn(&self) -> &Self::Turn {
        &*self.turn
    }

    fn interruption(&self) -> &Self::Interruption {
        &*self.interruption
    }

    fn addressee(&self) -> &Self::Addressee {
        &*self.addressee
    }

    fn gate(&self) -> &Self::Gate {
        &*self.gate
    }

    fn chunker(&self) -> &Self::Chunker {
        &*self.chunker
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::{
        AddressedByName, AddresseeInput, AddresseePolicy, BoxedPolicies, DefaultPolicies, Policies,
        TextChunker,
    };
    use crate::ids::ParticipantId;

    fn addressed<P: Policies>(policies: &P, text: &str) -> bool {
        policies.addressee().addressed(&AddresseeInput {
            who: ParticipantId(1),
            text,
            humans: 2,
        })
    }

    #[test]
    fn boxed_policies_answer_like_the_ones_they_hold() {
        let fixed = DefaultPolicies::default();
        let mut boxed = BoxedPolicies::default();
        assert!(addressed(&fixed, "anyone there"));
        assert!(addressed(&boxed, "anyone there"));
        assert_eq!(
            boxed.chunker().ready("One. Two", false),
            fixed.chunker().ready("One. Two", false)
        );

        boxed.addressee = Box::new(AddressedByName::new(&["Ava"]));
        assert!(!addressed(&boxed, "anyone there"));
        assert!(addressed(&boxed, "ava, are you there"));
    }
}
