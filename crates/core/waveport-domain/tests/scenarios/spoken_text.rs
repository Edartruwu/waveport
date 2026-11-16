//! Scenario 3: the conversation records what the caller heard, and nothing
//! else.
//!
//! When an agent is cut off, the language model must later be shown what was
//! actually said. Recording the whole reply makes the agent believe it said
//! things the caller never heard; recording nothing makes it repeat itself.
//!
//! The caller cuts in after six words. The agent's turn is committed with
//! exactly those six words, marked as cut off, and the next reply is
//! committed whole.

use waveport_kernel_sim::prelude::*;

pub(crate) fn spoken_text(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new());
    let caller = sim.caller();

    sim.agent_will(say(
        "Your flight leaves at nine tomorrow morning from gate twelve. \
         Boarding starts forty minutes before that.",
    ));
    sim.says(caller, "when does my flight leave");
    let turn = sim.expect_turn(caller, "when does my flight leave")?;
    let reply = sim.expect_reply(turn)?;
    sim.hears_words(reply, 6)?;

    sim.agent_will(say("Gate twelve."));
    sim.says(caller, "which gate");
    sim.expect_ducked(reply)?;
    sim.expect_interrupted(reply)?;
    sim.expect_spoken(reply, "Your flight leaves at nine tomorrow", Truncated)?;

    let second = sim.expect_turn(caller, "which gate")?;
    let answer = sim.expect_reply(second)?;
    sim.expect_spoken(answer, "Gate twelve.", Complete)?;
    sim.expect_context(&[
        user(caller, "when does my flight leave"),
        cut_off("Your flight leaves at nine tomorrow"),
        user(caller, "which gate"),
        spoke("Gate twelve."),
    ])?;
    sim.finish()
}
