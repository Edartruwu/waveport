//! Scenario 14: a line that stays gone ends the session, and not before
//! its time.
//!
//! Waiting for a caller who will not come back holds a session, its
//! provider connections and whatever they cost. Not waiting at all ends a
//! call for a gap of two seconds. The session waits for a time it was
//! given, and then closes by itself as abandoned.
//!
//! The line drops in the middle of a sentence and does not return. Just
//! before the wait is over the session is still running; once it is over
//! the session closes, and the conversation it leaves holds what the
//! caller heard.

use waveport_kernel_sim::prelude::*;

pub(crate) fn line_stays_gone(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().detach_wait_ms(5_000));
    let caller = sim.caller();

    sim.agent_will(say(
        "Your order shipped on Monday and it should reach you by Thursday afternoon.",
    ));
    sim.says(caller, "where is my order");
    let turn = sim.expect_turn(caller, "where is my order")?;
    let reply = sim.expect_reply(turn)?;
    sim.hears_words(reply, 4)?;

    sim.line_drops();
    sim.expect_spoken(reply, "Your order shipped on", Truncated)?;
    sim.wait_ms(4_900);
    sim.expect_open()?;

    sim.expect_abandoned()?;
    sim.expect_lifecycle(Lifecycle::Closed)?;
    sim.expect_context(&[
        user(caller, "where is my order"),
        cut_off("Your order shipped on"),
    ])?;
    sim.finish()
}
