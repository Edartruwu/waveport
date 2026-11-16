//! Scenario 15: the keys of a telephone are one turn, and are keys.
//!
//! A caller who is asked for a number presses it. Each key is an event of
//! its own, and an agent that answers each one answers four times. Turned
//! into a transcript, the keys are no longer keys: nothing downstream can
//! tell a typed account number from a spoken one, or keep it out of a log.
//!
//! The caller presses four digits, and either ends them with the hash key
//! or just stops. Either way the four are one turn, committed as keys and
//! without the hash, and it is answered once.

use waveport_kernel_sim::prelude::*;

pub(crate) fn keys_are_one_turn(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new());
    let caller = sim.caller();
    let ends_with_hash = lab.either();

    sim.agent_will(say("Please enter your account number."));
    sim.says(caller, "what is my balance");
    let asked = sim.expect_turn(caller, "what is my balance")?;
    let reply = sim.expect_reply(asked)?;
    sim.expect_spoken(reply, "Please enter your account number.", Complete)?;

    sim.agent_will(say("Thank you. Your balance is forty dollars."));
    sim.presses(caller, if ends_with_hash { "4071#" } else { "4071" });
    let keyed = sim.expect_keys(caller, "4071")?;
    let answer = sim.expect_reply(keyed)?;
    sim.expect_spoken(
        answer,
        "Thank you. Your balance is forty dollars.",
        Complete,
    )?;
    sim.expect_context(&[
        user(caller, "what is my balance"),
        spoke("Please enter your account number."),
        pressed(caller, "4071"),
        spoke("Thank you. Your balance is forty dollars."),
    ])?;
    sim.finish()
}
