//! Scenario 12: the words that interrupt the agent are never lost, when
//! they come later either.
//!
//! This is scenario 1 in a session whose words follow their turns. The
//! caller cuts in, and what they said is not known when the agent is
//! stopped, nor when their turn is committed, nor perhaps when the next
//! reply begins. A session that ties a turn to its transcript has nothing
//! to commit, and one that takes late words for a new turn answers the
//! interruption twice.
//!
//! The end of the caller's speech, the verdict that it is a barge-in and
//! the end-of-turn estimate arrive in every order. Whatever the order, the
//! agent is cut off once, the interruption is one turn, one reply follows
//! it, and both turns end up holding their words.

use waveport_kernel_sim::prelude::*;

pub(crate) fn words_after_barge_in(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().words_follow());
    let caller = sim.caller();

    sim.agent_will(say(
        "Your order shipped on Monday and it should reach you by Thursday afternoon.",
    ));
    sim.says(caller, "where is my order");
    let first = sim.expect_turn_without_words(caller)?;
    let reply = sim.expect_reply(first)?;
    sim.expect_words(first, "where is my order")?;
    sim.hears_words(reply, 3)?;

    sim.agent_will(say("Done, the order is cancelled."));
    sim.says(caller, "cancel it please");
    sim.race()?;

    sim.expect_interrupted(reply)?;
    sim.expect_spoken(reply, "Your order shipped", Truncated)?;
    let second = sim.expect_turn_without_words(caller)?;
    let answer = sim.expect_reply(second)?;
    sim.expect_words(second, "cancel it please")?;
    sim.expect_spoken(answer, "Done, the order is cancelled.", Complete)?;
    sim.expect_context(&[
        user(caller, "where is my order"),
        cut_off("Your order shipped"),
        user(caller, "cancel it please"),
        spoke("Done, the order is cancelled."),
    ])?;
    sim.finish()
}
