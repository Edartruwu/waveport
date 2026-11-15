//! Scenario 5: work handed to a second agent survives an interruption of
//! the first.
//!
//! A fast agent talks to the caller and hands slow questions to a second
//! agent through a tool. If an interruption discards everything the session
//! has in flight, the caller's "and one more thing" throws away the research
//! they are waiting for.
//!
//! The front agent delegates and keeps talking. The caller cuts in: the
//! front agent stops, and the back agent's work goes on. Its answer arrives
//! while the front agent is replying to the caller, enters the conversation
//! once, and is followed by one continuation.

use waveport_kernel_sim::prelude::*;

pub(crate) fn hybrid_front_back(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(
        Setup::new()
            .back_agent("researcher")
            .delegate("research", "researcher"),
    );
    let caller = sim.caller();

    sim.agent_will(
        say("Give me a moment while I check what the warranty says about that.")
            .call("research", r#"{"topic":"warranty cover"}"#),
    );
    sim.back_agent_will(
        "researcher",
        say("Parts and labour for two years.").after_ms(4_000),
    );
    sim.says(caller, "what does the warranty cover");
    let turn = sim.expect_turn(caller, "what does the warranty cover")?;
    let reply = sim.expect_reply(turn)?;
    let call = sim.expect_dispatched("research")?;
    let work = sim.expect_delegated(call)?;
    sim.hears_words(reply, 3)?;

    sim.agent_will(say(
        "Sure, I will also ask whether accidental damage is included in it.",
    ));
    sim.says(caller, "and accidental damage too");
    let second = sim.expect_turn(caller, "and accidental damage too")?;
    sim.expect_interrupted(reply)?;
    sim.expect_spoken(reply, "Give me a", Truncated)?;
    sim.expect_live(work)?;
    sim.expect_not_cancelled(call)?;
    let answer = sim.expect_reply(second)?;

    sim.agent_will(say("The warranty covers parts and labour for two years."));
    sim.expect_result_once(call, "Parts and labour for two years.")?;
    sim.expect_spoken(
        answer,
        "Sure, I will also ask whether accidental damage is included in it.",
        Complete,
    )?;
    let next = sim.expect_continuation(reply)?;
    sim.expect_spoken(
        next,
        "The warranty covers parts and labour for two years.",
        Complete,
    )?;
    sim.finish()
}
