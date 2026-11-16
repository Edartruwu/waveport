//! Scenario 16: words and keys keep their order, and the agent does not
//! talk over keys.
//!
//! People speak and press in one breath: "it is", and then the digits. The
//! words are still on their way from the recogniser when the first key
//! arrives. A session that lets the keys end the spoken turn loses the
//! words or puts them after the keys, and one that answers the words at
//! once speaks while the caller is still pressing.
//!
//! The caller says two words and starts to press a moment after. The words
//! are a turn and the keys are a turn, in that order. A reply to the words
//! may be begun, and nothing of it is heard while keys are being pressed.
//! The reply that is heard is to the keys.

use waveport_kernel_sim::prelude::*;

pub(crate) fn keys_after_speech(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new());
    let caller = sim.caller();

    sim.agent_will(say("Sure. What is your account number?"));
    sim.says(caller, "what is my balance");
    let asked = sim.expect_turn(caller, "what is my balance")?;
    let reply = sim.expect_reply(asked)?;
    sim.expect_spoken(reply, "Sure. What is your account number?", Complete)?;

    sim.agent_will(say("Go ahead."));
    sim.agent_will(say("Thank you. Your balance is forty dollars."));
    sim.says(caller, "it is");
    sim.wait_ms(700);
    sim.presses(caller, "4071#");

    let said = sim.expect_turn(caller, "it is")?;
    let unheard = sim.expect_reply(said)?;
    let keyed = sim.expect_keys(caller, "4071")?;
    let answer = sim.expect_reply(keyed)?;
    sim.expect_never_spoke(unheard)?;
    sim.expect_spoken(
        answer,
        "Thank you. Your balance is forty dollars.",
        Complete,
    )?;
    sim.expect_context(&[
        user(caller, "what is my balance"),
        spoke("Sure. What is your account number?"),
        user(caller, "it is"),
        pressed(caller, "4071"),
        spoke("Thank you. Your balance is forty dollars."),
    ])?;
    sim.finish()
}
