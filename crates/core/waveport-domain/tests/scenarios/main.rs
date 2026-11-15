//! The six kernel scenarios, and where each one stands.
//!
//! Each scenario closes a race that conversational frameworks are known to
//! lose, and is explored along every path its world allows. The files in
//! `scenarios/` were written before the kernel and are not edited to make
//! them pass: needing to change one is the sign that the kernel's design is
//! wrong.
//!
//! This file says how far the kernel has got. A scenario is listed as
//! `unstarted` while it fails at its first expectation, `pending` while it
//! gets further and still fails, and `passes` once it holds on every path.
//! The listing is checked both ways, so a scenario that starts to pass must
//! be promoted here.

mod backchannel_tool;
mod hybrid_front_back;
mod lost_input;
mod shutdown;
mod spoken_text;
mod three_party;

use waveport_kernel_sim::check::{Report, unstarted};

#[test]
fn lost_input() -> Result<(), Report> {
    unstarted(lost_input::lost_input)
}

#[test]
fn backchannel_tool() -> Result<(), Report> {
    unstarted(backchannel_tool::backchannel_tool)
}

#[test]
fn spoken_text() -> Result<(), Report> {
    unstarted(spoken_text::spoken_text)
}

#[test]
fn shutdown() -> Result<(), Report> {
    unstarted(shutdown::shutdown)
}

#[test]
fn hybrid_front_back() -> Result<(), Report> {
    unstarted(hybrid_front_back::hybrid_front_back)
}

#[test]
fn three_party() -> Result<(), Report> {
    unstarted(three_party::three_party)
}
