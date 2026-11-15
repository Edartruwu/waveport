//! The six kernel scenarios, and where each one stands.
//!
//! Each scenario closes a race that conversational frameworks are known to
//! lose, and is explored along every path its world allows. The files in
//! `scenarios/` were written before the kernel and are not edited to make
//! them pass: needing to change one is the sign that the kernel's design is
//! wrong.
//!
//! This file says how far the kernel has got: all six pass. A scenario
//! added later is listed with `unstarted` while it fails at its first
//! expectation, with `pending` while it gets further and still fails, and
//! with `passes` once it holds on every path. The listing is checked both
//! ways, so a scenario that starts to pass must be promoted here.

mod backchannel_tool;
mod hybrid_front_back;
mod lost_input;
mod shutdown;
mod spoken_text;
mod three_party;

use waveport_kernel_sim::check::{Report, passes};

#[test]
fn lost_input() -> Result<(), Report> {
    passes(lost_input::lost_input)
}

#[test]
fn backchannel_tool() -> Result<(), Report> {
    passes(backchannel_tool::backchannel_tool)
}

#[test]
fn spoken_text() -> Result<(), Report> {
    passes(spoken_text::spoken_text)
}

#[test]
fn shutdown() -> Result<(), Report> {
    passes(shutdown::shutdown)
}

#[test]
fn hybrid_front_back() -> Result<(), Report> {
    passes(hybrid_front_back::hybrid_front_back)
}

#[test]
fn three_party() -> Result<(), Report> {
    passes(three_party::three_party)
}
