//! Scenario 4: a session always closes on time.
//!
//! Shutdown hangs when it waits for something that never answers: a
//! provider socket that went quiet, a playback that never reports its end.
//!
//! The session is closed at a moment when it is waiting on its adapters:
//! either the agent is in the middle of a sentence, or its reply is still
//! being generated while the caller has started to speak again. From then
//! on no adapter says anything. The session still reaches `Closed` within
//! the drain time plus the grace time, even when the timer that ends the
//! drain is lost as well, and it leaves a truthful record: the agent's
//! sentence as far as it was heard, the caller's unfinished turn dismissed.

use waveport_kernel_sim::prelude::*;

pub(crate) fn shutdown(lab: &mut Lab) -> Verdict {
    let mut sim = lab.sim(Setup::new().drain_ms(2_000).grace_ms(1_000));
    let caller = sim.caller();

    let mid_sentence = lab.either();
    let story = say("Once upon a time there was a lighthouse keeper who counted every ship.");
    sim.agent_will(if mid_sentence {
        story
    } else {
        story.after_ms(60_000)
    });
    sim.says(caller, "tell me a story");
    let turn = sim.expect_turn(caller, "tell me a story")?;
    let reply = sim.expect_reply(turn)?;
    if mid_sentence {
        sim.hears_words(reply, 4)?;
    } else {
        sim.says(caller, "wait i wanted to ask");
    }

    if lab.either() {
        sim.lose_timer(Timer::Drain);
    }
    sim.adapters_go_silent();
    sim.close();
    sim.expect_lifecycle(Lifecycle::Draining)?;
    sim.expect_closed_within(3_000)?;

    if mid_sentence {
        sim.expect_spoken(reply, "Once upon a time", Truncated)?;
    } else {
        sim.expect_never_spoke(reply)?;
        sim.expect_dismissed(caller, Dismissal::Closed)?;
    }
    sim.finish()
}
