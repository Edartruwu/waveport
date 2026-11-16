//! Scenario 13: a line that drops and comes back costs the caller nothing
//! they had not heard.
//!
//! A browser on a train loses its connection for a few seconds. In the
//! frameworks waveport learned from, that ends the session, or the agent
//! talks on into a line nobody is on and the conversation records all of
//! it as said.
//!
//! The line drops either while the agent is in the middle of a sentence or
//! while its reply is still being written. The agent says nothing into the
//! gap. What the caller heard before the drop is what the conversation
//! holds. When the line is back, the caller is owed the reply they did not
//! get, and gets it.

use waveport_kernel_sim::prelude::*;

pub(crate) fn line_drops(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().detach_wait_ms(5_000));
    let caller = sim.caller();
    let mid_sentence = lab.either();

    sim.agent_will(
        say("Your order shipped on Monday and it should reach you by Thursday afternoon.")
            .after_ms(600),
    );
    sim.says(caller, "where is my order");
    let turn = sim.expect_turn(caller, "where is my order")?;
    let reply = sim.expect_reply(turn)?;
    if mid_sentence {
        sim.hears_words(reply, 4)?;
    }

    sim.line_drops();
    sim.expect_interrupted(reply)?;
    if mid_sentence {
        sim.expect_spoken(reply, "Your order shipped on", Truncated)?;
    } else {
        sim.expect_never_spoke(reply)?;
    }
    sim.wait_ms(2_000);
    sim.expect_open()?;

    sim.agent_will(say("It shipped on Monday and arrives on Thursday."));
    sim.line_returns();
    let again = sim.expect_reply(turn)?;
    sim.expect_spoken(
        again,
        "It shipped on Monday and arrives on Thursday.",
        Complete,
    )?;
    if mid_sentence {
        sim.expect_context(&[
            user(caller, "where is my order"),
            cut_off("Your order shipped on"),
            spoke("It shipped on Monday and arrives on Thursday."),
        ])?;
    } else {
        sim.expect_context(&[
            user(caller, "where is my order"),
            spoke("It shipped on Monday and arrives on Thursday."),
        ])?;
    }
    sim.finish()
}
