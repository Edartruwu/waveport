//! Scenario 10: a turn whose words come later is still one turn.
//!
//! A model that listens and speaks by itself answers the caller's audio
//! and says only afterwards what it heard. A session that waits for words
//! before it accepts a turn dismisses that turn as empty, and then takes
//! the late words for a new turn: the caller is answered twice, the second
//! time for something they said once.
//!
//! Here the turn is committed when it is over, without words. The reply is
//! to that turn. The words fill the turn in when they arrive, the
//! conversation reads as if they had been there all along, and nothing is
//! answered a second time.

use waveport_kernel_sim::prelude::*;

pub(crate) fn late_words(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().words_follow());
    let caller = sim.caller();

    sim.agent_will(say("It shipped on Monday."));
    sim.says(caller, "where is my order");
    let turn = sim.expect_turn_without_words(caller)?;
    let reply = sim.expect_reply(turn)?;
    sim.expect_words(turn, "where is my order")?;
    sim.expect_spoken(reply, "It shipped on Monday.", Complete)?;
    sim.expect_context(&[
        user(caller, "where is my order"),
        spoke("It shipped on Monday."),
    ])?;
    sim.finish()
}
