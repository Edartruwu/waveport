//! Scenario 8: a second message before the first is answered gets one
//! answer.
//!
//! People who type send what they mean in pieces: a question, and a moment
//! later the detail it was missing. An agent that answers each piece on its
//! own answers the first one wrongly, and then says so.
//!
//! The caller types again while the model is still thinking about their
//! first message. Nothing of the first answer has been delivered, so it is
//! dropped unseen, and the one answer that is delivered was written with
//! both messages in front of it.

use waveport_kernel_sim::prelude::*;

pub(crate) fn typed_twice(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().text());
    let caller = sim.caller();

    sim.agent_will(say("Which order do you mean?").after_ms(800));
    sim.types(caller, "where is my order");
    let first = sim.expect_turn(caller, "where is my order")?;
    let dropped = sim.expect_reply(first)?;

    sim.wait_ms(300);
    sim.agent_will(say("Order 1234 shipped on Monday."));
    sim.types(caller, "it is order 1234");
    let second = sim.expect_turn(caller, "it is order 1234")?;
    let answer = sim.expect_reply(second)?;
    sim.expect_spoken(answer, "Order 1234 shipped on Monday.", Complete)?;
    sim.expect_never_spoke(dropped)?;
    sim.expect_context(&[
        user(caller, "where is my order"),
        user(caller, "it is order 1234"),
        spoke("Order 1234 shipped on Monday."),
    ])?;
    sim.finish()
}
