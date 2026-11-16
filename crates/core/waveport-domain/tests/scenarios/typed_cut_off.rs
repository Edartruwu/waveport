//! Scenario 9: an answer that is cut off in text is recorded as far as it
//! was delivered.
//!
//! The rule for speech is that the conversation holds what the listener
//! heard. In text the same rule reads: what the reader was sent. An agent
//! that records the whole answer believes it wrote things nobody received,
//! and one that records none of it writes them again.
//!
//! The caller types while an answer is still arriving. The answer stops
//! where it is, its part that was delivered enters the conversation marked
//! as cut off, and the new message is answered.

use waveport_kernel_sim::prelude::*;

pub(crate) fn typed_cut_off(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().text());
    let caller = sim.caller();

    sim.agent_will(say(
        "Your order shipped on Monday. It should reach you by Thursday. \
         Is there anything else I can do for you today?",
    ));
    sim.types(caller, "where is my order");
    let first = sim.expect_turn(caller, "where is my order")?;
    let reply = sim.expect_reply(first)?;
    sim.hears_words(reply, 5)?;

    sim.agent_will(say("Done, the order is cancelled."));
    sim.types(caller, "cancel it");
    sim.expect_interrupted(reply)?;
    sim.expect_spoken(reply, "Your order shipped on Monday.", Truncated)?;
    let second = sim.expect_turn(caller, "cancel it")?;
    let answer = sim.expect_reply(second)?;
    sim.expect_spoken(answer, "Done, the order is cancelled.", Complete)?;
    sim.expect_context(&[
        user(caller, "where is my order"),
        cut_off("Your order shipped on Monday."),
        user(caller, "cancel it"),
        spoke("Done, the order is cancelled."),
    ])?;
    sim.finish()
}
