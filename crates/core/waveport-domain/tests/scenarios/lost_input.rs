//! Scenario 1: the words that interrupt the agent are never lost.
//!
//! In the frameworks waveport learned from, an interruption flushes whatever
//! is in flight, and the transcript of the very words that caused it can be
//! flushed with it: the caller says "cancel it", the agent stops, and then
//! says nothing, because the request is gone.
//!
//! Here the caller cuts in while the agent is speaking. The end of their
//! speech, its transcript, the verdict that it is a barge-in and the
//! end-of-turn estimate then arrive in every order the adapters could
//! deliver them in. Whatever the order, the words end in one committed
//! turn, the agent is cut off once, and one reply follows.

use waveport_kernel_sim::prelude::*;

pub(crate) fn lost_input(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new());
    let caller = sim.caller();

    sim.agent_will(say(
        "Your order shipped on Monday and it should reach you by Thursday afternoon.",
    ));
    sim.says(caller, "where is my order");
    let first = sim.expect_turn(caller, "where is my order")?;
    let reply = sim.expect_reply(first)?;
    sim.hears_words(reply, 3)?;

    sim.agent_will(say("Done, the order is cancelled."));
    sim.says(caller, "cancel it please");
    sim.race()?;

    let second = sim.expect_turn(caller, "cancel it please")?;
    sim.expect_interrupted(reply)?;
    sim.expect_spoken(reply, "Your order shipped", Truncated)?;
    let answer = sim.expect_reply(second)?;
    sim.expect_spoken(answer, "Done, the order is cancelled.", Complete)?;
    sim.expect_context(&[
        user(caller, "where is my order"),
        cut_off("Your order shipped"),
        user(caller, "cancel it please"),
        spoke("Done, the order is cancelled."),
    ])?;
    sim.finish()
}
