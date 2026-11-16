//! Scenario 11: the answer may be heard before the question is known.
//!
//! When words follow their turn, nothing says how late they are. The
//! caller can be listening to the answer while the session still does not
//! know what they asked. Words that arrive then are about the past: they
//! are not speech over the agent, and they do not start a turn.
//!
//! Here the words are held back until the caller has heard three words of
//! the reply. The reply goes on to its end, the words take their place
//! before it in the conversation, and nobody is answered twice.

use waveport_kernel_sim::prelude::*;

pub(crate) fn sound_before_words(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().words_follow());
    let caller = sim.caller();
    sim.words_wait();

    sim.agent_will(say("It shipped on Monday and arrives on Thursday."));
    sim.says(caller, "where is my order");
    let turn = sim.expect_turn_without_words(caller)?;
    let reply = sim.expect_reply(turn)?;
    sim.hears_words(reply, 3)?;

    sim.words_arrive(caller);
    sim.expect_words(turn, "where is my order")?;
    sim.expect_not_interrupted(reply)?;
    sim.expect_spoken(
        reply,
        "It shipped on Monday and arrives on Thursday.",
        Complete,
    )?;
    sim.expect_context(&[
        user(caller, "where is my order"),
        spoke("It shipped on Monday and arrives on Thursday."),
    ])?;
    sim.finish()
}
