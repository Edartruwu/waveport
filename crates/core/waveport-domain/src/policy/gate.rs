//! Whether a tool call may be dispatched yet.

use crate::config::Gate;
use crate::ids::ToolId;

/// What the kernel knows about a call that is waiting to be dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GateInput {
    /// The tool.
    pub tool: ToolId,
    /// The gate its specification declares.
    pub gate: Gate,
    /// Whether the speech of the response that asked for the call has ended:
    /// played to the end, or cut off, or there was none.
    pub utterance_done: bool,
}

/// Decides whether a tool call may be dispatched yet. The kernel asks again
/// whenever something the answer may depend on changes.
pub trait ToolGate {
    /// Whether the call may be dispatched now.
    fn open(&self, input: &GateInput) -> bool;
}

/// Applies the gate each tool declares in its specification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpecGate;

impl ToolGate for SpecGate {
    fn open(&self, input: &GateInput) -> bool {
        match input.gate {
            Gate::None => true,
            Gate::AfterOwnUtterance => input.utterance_done,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GateInput, SpecGate, ToolGate};
    use crate::config::Gate;
    use crate::ids::ToolId;

    #[test]
    fn a_gated_call_waits_for_its_own_speech() {
        let input = |gate, utterance_done| GateInput {
            tool: ToolId(0),
            gate,
            utterance_done,
        };
        assert!(SpecGate.open(&input(Gate::None, false)));
        assert!(!SpecGate.open(&input(Gate::AfterOwnUtterance, false)));
        assert!(SpecGate.open(&input(Gate::AfterOwnUtterance, true)));
    }
}
