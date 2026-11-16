//! Scenario 7: a conversation in text is the same conversation.
//!
//! A session need not have a voice. Somebody types, the agent writes back,
//! and nothing is recognised, synthesised or played. Frameworks built
//! around a room and its audio tracks need a second code path for this, and
//! the two drift apart.
//!
//! Here the caller types twice and is answered twice. Each message is a
//! turn the moment it arrives, each answer is delivered as it is written,
//! and the conversation records what was delivered, as it would record
//! what was heard.

use waveport_kernel_sim::prelude::*;

pub(crate) fn typed_chat(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().text());
    let caller = sim.caller();

    sim.agent_will(say(
        "Your order shipped on Monday. It should reach you by Thursday.",
    ));
    sim.types(caller, "where is my order");
    let first = sim.expect_turn(caller, "where is my order")?;
    let reply = sim.expect_reply(first)?;
    sim.expect_spoken(
        reply,
        "Your order shipped on Monday. It should reach you by Thursday.",
        Complete,
    )?;

    sim.agent_will(say("You are welcome."));
    sim.types(caller, "thanks");
    let second = sim.expect_turn(caller, "thanks")?;
    let answer = sim.expect_reply(second)?;
    sim.expect_spoken(answer, "You are welcome.", Complete)?;
    sim.expect_context(&[
        user(caller, "where is my order"),
        spoke("Your order shipped on Monday. It should reach you by Thursday."),
        user(caller, "thanks"),
        spoke("You are welcome."),
    ])?;
    sim.finish()
}
