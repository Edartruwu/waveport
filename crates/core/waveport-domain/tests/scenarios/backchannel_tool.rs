//! Scenario 2: a murmur of agreement costs nothing, and tool results are
//! used once.
//!
//! Two races meet here. In the first, any sound from the caller counts as an
//! interruption, so an "mm-hm" cancels a refund that has already been issued,
//! or cuts the agent off for nothing. In the second, each result of two
//! parallel tool calls starts the language model again, and the agent
//! answers twice.
//!
//! The agent asks for two tools at once and keeps talking. The caller
//! murmurs: the agent's volume drops and comes back, and nothing is
//! cancelled. Then both results arrive, one of them twice, in every order.
//! One continuation follows, and each result is in the conversation once.

use waveport_kernel_sim::prelude::*;

pub(crate) fn backchannel_tool(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(
        Setup::new()
            .tool("lookup_order", Effect::Pure)
            .tool("refund", Effect::SideEffecting),
    );
    let caller = sim.caller();
    sim.tools_wait();

    sim.agent_will(
        say("One moment while I look up your last order and start the refund for you.")
            .call("lookup_order", r#"{"which":"last"}"#)
            .call("refund", r#"{"which":"last"}"#),
    );
    sim.says(caller, "please refund my last order");
    let turn = sim.expect_turn(caller, "please refund my last order")?;
    let reply = sim.expect_reply(turn)?;
    let lookup = sim.expect_dispatched("lookup_order")?;
    let refund = sim.expect_dispatched("refund")?;
    sim.hears_words(reply, 4)?;

    sim.murmurs(caller, "mm-hm");
    sim.expect_ducked(reply)?;
    sim.expect_unducked(reply)?;
    sim.expect_dismissed(caller, Dismissal::Backchannel)?;
    sim.expect_not_interrupted(reply)?;
    sim.expect_not_cancelled(lookup)?;
    sim.expect_not_cancelled(refund)?;

    // Both results are on their way, and the refund's is delivered twice.
    sim.agent_will(say("Your refund is on its way."));
    sim.tool_returns(lookup, r#"{"order":"A-1041","total":"39.90"}"#);
    sim.tool_returns(refund, r#"{"refunded":"39.90"}"#);
    sim.tool_returns(refund, r#"{"refunded":"39.90"}"#);
    sim.race()?;

    sim.expect_result_once(lookup, r#"{"order":"A-1041","total":"39.90"}"#)?;
    sim.expect_result_once(refund, r#"{"refunded":"39.90"}"#)?;
    sim.expect_spoken(
        reply,
        "One moment while I look up your last order and start the refund for you.",
        Complete,
    )?;
    let next = sim.expect_continuation(reply)?;
    sim.expect_spoken(next, "Your refund is on its way.", Complete)?;
    sim.finish()
}
