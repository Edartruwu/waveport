//! Identities: what the kernel uses to name turns, responses, tool calls
//! and the people and agents in a session.

use core::fmt;

macro_rules! id {
    ($(#[$doc:meta])* $name:ident($inner:ty)) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub $inner);

        impl $name {
            /// The raw value.
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }

            /// The identity after this one. It stops at the largest value
            /// instead of wrapping, so identities never repeat.
            #[must_use]
            pub const fn next(self) -> Self {
                Self(self.0.saturating_add(1))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id! {
    /// Position of a fact in the session's journal. The first fact is 1, and
    /// the sequence has no gaps.
    Seq(u64)
}
id! {
    /// One user turn.
    TurnId(u32)
}
id! {
    /// One response: a single run of an agent, from the request to its
    /// language model to the end of what it says.
    GenId(u32)
}
id! {
    /// Bumped whenever the agent output in flight for the floor becomes
    /// invalid: a committed barge-in, a handoff, a cancel, a recovery.
    Epoch(u32)
}
id! {
    /// The tool calls one response proposed together.
    BatchId(u32)
}
id! {
    /// One tool call. It is also the call's idempotency key.
    ToolCallId(u64)
}
id! {
    /// A tool, as an index into the tools the session was configured with.
    ToolId(u16)
}
id! {
    /// A human or an agent taking part in the session.
    ParticipantId(u16)
}
id! {
    /// An agent, as an index into the agents the session was configured with.
    AgentId(u8)
}
id! {
    /// Bumped each time an item is appended to the conversation.
    ContextVersion(u32)
}

/// Carried on every command that starts or steers agent output, and echoed
/// on every result of that work.
///
/// A result is acted on only while its generation is live. A result whose
/// generation has ended is dropped in one place, before any sub-machine sees
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpTag {
    /// The response the work belongs to.
    pub generation: GenId,
    /// The epoch the response was started in.
    pub epoch: Epoch,
}

impl fmt::Display for OpTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "g{}/e{}", self.generation, self.epoch)
    }
}

/// A probability, held as an integer so that kernel state can be hashed and
/// compared exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Prob(u16);

impl Prob {
    /// Certainly not.
    pub const ZERO: Self = Self(0);
    /// Certainly.
    pub const ONE: Self = Self(Self::SCALE);

    const SCALE: u16 = 10_000;

    /// A probability in hundredths of a percent. Values above 10,000 mean 1.
    #[must_use]
    pub const fn from_basis_points(points: u16) -> Self {
        if points > Self::SCALE {
            Self::ONE
        } else {
            Self(points)
        }
    }

    /// The nearest representable probability. Anything outside `0.0..=1.0`,
    /// including NaN, is clamped into it; NaN becomes 0.
    #[must_use]
    pub fn from_f32(value: f32) -> Self {
        let scaled = value.clamp(0.0, 1.0) * f32::from(Self::SCALE) + 0.5;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the value is within 0.5..=10000.5, and a NaN casts to 0"
        )]
        let points = scaled as u16;
        Self::from_basis_points(points)
    }

    /// The probability in hundredths of a percent, `0..=10_000`.
    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Prob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}%", self.0 / 100, self.0 % 100)
    }
}

#[cfg(test)]
mod tests {
    use super::{GenId, Prob, Seq};

    #[test]
    fn identities_count_up_and_never_wrap() {
        assert_eq!(Seq(1).next(), Seq(2));
        assert_eq!(GenId(u32::MAX).next(), GenId(u32::MAX));
        assert_eq!(Seq::default().get(), 0);
    }

    #[test]
    fn probabilities_are_clamped() {
        assert_eq!(Prob::from_f32(0.5).basis_points(), 5_000);
        assert_eq!(Prob::from_f32(0.999_96), Prob::ONE);
        assert_eq!(Prob::from_f32(7.0), Prob::ONE);
        assert_eq!(Prob::from_f32(-1.0), Prob::ZERO);
        assert_eq!(Prob::from_f32(f32::NAN), Prob::ZERO);
        assert_eq!(Prob::from_basis_points(20_000), Prob::ONE);
        assert!(Prob::from_f32(0.2) < Prob::from_f32(0.8));
    }
}
