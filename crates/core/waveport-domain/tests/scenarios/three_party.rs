//! Scenario 6: with several people in the room, the agent knows who said
//! what and when it is being spoken to.
//!
//! A session built for one caller breaks in a meeting: two people talking at
//! once become one garbled turn, and the agent answers every remark,
//! including those the humans address to each other.
//!
//! Two people talk over each other. Each one's words become their own turn,
//! and the agent stays quiet, because nobody said its name. Then one of them
//! asks the agent by name, and it replies once.

use waveport_kernel_sim::prelude::*;

pub(crate) fn three_party(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().answers_to("Ava"));
    let ana = sim.caller();
    let ben = sim.joins("Ben");

    // Ben starts to answer while Ana is still asking.
    sim.says(ana, "ben did you send the invoice");
    sim.wait_ms(900);
    sim.says(ben, "yes i sent it this morning");
    let asked = sim.expect_turn(ana, "ben did you send the invoice")?;
    let answered = sim.expect_turn(ben, "yes i sent it this morning")?;
    sim.expect_no_reply(asked)?;
    sim.expect_no_reply(answered)?;

    sim.agent_will(say("The invoice total is four hundred euros."));
    sim.says(ana, "ava what is the invoice total");
    let question = sim.expect_turn(ana, "ava what is the invoice total")?;
    let reply = sim.expect_reply(question)?;
    sim.expect_spoken(reply, "The invoice total is four hundred euros.", Complete)?;
    sim.expect_context(&[
        user(ana, "ben did you send the invoice"),
        user(ben, "yes i sent it this morning"),
        user(ana, "ava what is the invoice total"),
        spoke("The invoice total is four hundred euros."),
    ])?;
    sim.finish()
}
