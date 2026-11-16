//! Sessions of text: the stories that can be told in writing, and
//! sessions nobody wrote.
//!
//! Four of the six stories are stories in text too. A reader who types
//! over an answer is answered on what they had read (3). A session that
//! is asked to close closes (4). Work handed to a second agent goes on
//! when the reader writes to the first (5). With several people writing,
//! the agent knows who wrote and to whom (6). The other two are about
//! sound: words that arrive after the speech that carried them (1), and
//! a noise of agreement (2). Nothing that is typed is either.
//!
//! What `world` holds every session of text to is what a reader would
//! hold it to: what is on record of an answer is what they were given,
//! and nothing of an answer is given once it is over.

use std::sync::Arc;

use waveport_domain::config::Effect;
use waveport_domain::event::CloseReason;
use waveport_domain::fact::{Fact, Lifecycle, ToolOutcome};
use waveport_domain::ids::ParticipantId;
use waveport_runtime::session::{Ended, SessionInput, StoreBroken};
use waveport_sim::{Seed, SimRng};
use waveport_testkit::fakes::{LlmStep, Said};

use crate::stories::{
    ALSO, COVERS, ONE_MOMENT, STORY, TOTAL, asks_the_back, front_and_back, has_result, researched,
    spec,
};
use crate::sweeps::{End, Profile, WORDS, ending, index, minds, nothing_more, trouble};
use crate::world::{Doing, Outcome, Plan, Shape, run};
use crate::{SEEDS, told};

/// What a seed decides of the parts of a session of text: how late they
/// are.
fn weather(plan: &mut Plan, rng: &mut SimRng) {
    plan.shape = Shape::Text;
    plan.llm_first_ms = rng.range(50..900);
    plan.llm_each_ms = rng.range(5..60);
    plan.tools_ms = rng.range(20..1_200);
    plan.journal_ms = rng.range(1..30);
}

/// Scenario 3: a reader who types over an answer is answered on what
/// they had read.
fn typed_over(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("typed over");
    let mut plan = Plan::new(seed, spec(&[]));
    weather(&mut plan, &mut rng);
    plan.llm = Arc::new(|asked| match asked.last_user() {
        Some("tell me a story") => vec![LlmStep::say(STORY)],
        _ => vec![LlmStep::say("All right.")],
    });
    plan.types(0, 1_000, "tell me a story");
    // The story has thirty words and more. The reader types when they
    // have somewhere between three and twenty-five of them.
    let read = plan.llm_first_ms + rng.range(3..25) * plan.llm_each_ms;
    let cuts_in = 1_000 + read + rng.range(0..plan.llm_each_ms);
    plan.types(0, cuts_in, "no stop it please");
    plan.until_ms = cuts_in + 40_000;
    plan
}

fn typed_over_ends(_: &Plan, outcome: &Outcome) -> Result<(), String> {
    let turns: Vec<&str> = outcome.turns().iter().map(|(_, text, _)| *text).collect();
    if turns != ["tell me a story", "no stop it please"] {
        return Err(format!("the turns are {turns:?}"));
    }
    let utterances = outcome.utterances();
    let [(_, read, cut), (_, then, whole)] = utterances[..] else {
        return Err(format!("the agent wrote {utterances:?}"));
    };
    if !cut || read.is_empty() || !STORY.starts_with(read) || read.len() >= STORY.len() {
        return Err(format!(
            "of the story the conversation has {read:?}, cut off: {cut}"
        ));
    }
    // The model writes a word at a time, so the story stops between two.
    if !STORY[read.len()..].starts_with(' ') {
        return Err(format!("the story was cut inside a word: {read:?}"));
    }
    if (then, whole) != ("All right.", false) {
        return Err(format!("the second answer is {then:?}, cut off: {whole}"));
    }
    // The model was shown what the reader had read, not what it wrote.
    let shown: Vec<&str> = outcome.asked[1]
        .messages
        .iter()
        .filter_map(|said| match said {
            Said::Assistant { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if shown != [read] {
        return Err(format!("the model was shown {shown:?} of the story"));
    }
    Ok(())
}

#[test]
fn a_reader_who_types_over_an_answer_is_answered_on_what_they_read() {
    let Some(tally) = told(SEEDS, typed_over, typed_over_ends) else {
        return;
    };
    assert_eq!(tally.cut, tally.runs, "{tally:?}");
    assert_eq!(tally.turns, 2 * tally.runs, "{tally:?}");
    // Nobody spoke, and nothing was sent over a line.
    assert_eq!((tally.barge_ins, tally.frames), (0, 0), "{tally:?}");
}

/// When a plan asks its session to close.
fn closes_at(plan: &Plan) -> Option<u64> {
    plan.doings
        .iter()
        .find_map(|(at, doing)| matches!(doing, Doing::Ask(SessionInput::Close)).then_some(*at))
}

/// Scenario 4: a session that is asked to close closes, whatever its
/// parts do.
fn closing(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("closing in text");
    let mut plan = Plan::new(seed, spec(&[("book", Effect::SideEffecting)]));
    weather(&mut plan, &mut rng);
    // The tool may have fallen silent for good, and so may the model.
    if rng.chance(1, 2) {
        plan.tools_ms = 3_600_000;
    }
    let model = rng.range(0..4);
    plan.llm = Arc::new(move |asked| match model {
        0 => vec![LlmStep::Hang],
        1 => vec![LlmStep::say(STORY), LlmStep::Hang],
        2 if !has_result(asked) => vec![LlmStep::say("One moment."), LlmStep::call("book", "{}")],
        _ => vec![LlmStep::say(STORY)],
    });
    plan.types(0, 1_000, "book a table for two");
    let closes = 1_000 + rng.range(0..9_000);
    plan.doings.push((closes, Doing::Ask(SessionInput::Close)));
    plan.until_ms = closes + 60_000;
    plan
}

fn closing_ends(plan: &Plan, outcome: &Outcome) -> Result<(), String> {
    if outcome.ended() != Some(&Ended::Closed) {
        return Err(format!("the session ended as {:?}", outcome.ended()));
    }
    let asked_at = closes_at(plan).ok_or("the plan does not ask the session to close")?;
    let timing = plan.spec.kernel.timing;
    let allowed = u64::try_from((timing.drain + timing.grace).as_millis()).unwrap() + 100;
    let took = outcome.lives[0].over_ms - asked_at;
    if took > allowed {
        return Err(format!(
            "closing took {took} ms, and {allowed} ms are allowed"
        ));
    }
    let Some(Fact::LifecycleChanged { to, reason }) = outcome.facts.last().map(|last| last.fact)
    else {
        return Err("the last fact is not the session's end".to_owned());
    };
    if (to, reason) != (Lifecycle::Closed, Some(CloseReason::Requested)) {
        return Err(format!("the session is {to:?} for {reason:?}"));
    }
    Ok(())
}

#[test]
fn a_chat_asked_to_close_closes_whatever_its_parts_do() {
    let Some(tally) = told(SEEDS, closing, closing_ends) else {
        return;
    };
    assert_eq!(tally.closed, tally.runs, "{tally:?}");
}

/// Scenario 5: work handed to a second agent goes on when the reader
/// writes to the first.
fn front_back(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("front and back in text");
    let mut plan = Plan::new(seed, front_and_back());
    weather(&mut plan, &mut rng);
    // The agent in front has written its thirteen words, and asked the
    // one at the back, between 0.2 s and 0.8 s after the question.
    plan.llm_first_ms = rng.range(50..400);
    plan.llm_each_ms = rng.range(8..25);
    plan.journal_ms = rng.range(1..10);
    plan.llm = Arc::new(asks_the_back);
    plan.types(0, 1_000, "what does the warranty cover");
    // The reader writes again while the agent at the back is at work,
    // which it is for 1.6 s at the least.
    let handed_on = 1_000 + plan.llm_first_ms + 14 * plan.llm_each_ms + 2 * plan.journal_ms + 50;
    let writes_again = handed_on + rng.range(50..1_200);
    plan.types(0, writes_again, "and accidental damage too");
    plan.until_ms = writes_again + 90_000;
    plan
}

fn front_back_ends(_: &Plan, outcome: &Outcome) -> Result<(), String> {
    let turns: Vec<&str> = outcome.turns().iter().map(|(_, text, _)| *text).collect();
    if turns != ["what does the warranty cover", "and accidental damage too"] {
        return Err(format!("the turns are {turns:?}"));
    }
    // The answer of the agent at the back is the result of the call,
    // once, and no tool of the application was asked for anything.
    let results: Vec<(ToolOutcome, &str)> = outcome
        .facts
        .iter()
        .filter_map(|stamped| match stamped.fact {
            Fact::ToolFinished { outcome: how, .. } => Some((how, outcome.text_of(&stamped.fact))),
            _ => None,
        })
        .collect();
    let found = researched();
    if results != [(ToolOutcome::Completed, found.trim())] {
        return Err(format!("the call ended as {results:?}"));
    }
    if !outcome.called.is_empty() || outcome.tally.delegated != 1 {
        return Err(format!(
            "the work was not handed on once: {:?}",
            outcome.tally
        ));
    }
    // The reader had all of the first answer before they wrote again,
    // was told that more would be asked, and was told what was found,
    // once.
    let said: Vec<(&str, bool)> = outcome
        .utterances()
        .iter()
        .map(|(_, text, cut)| (*text, *cut))
        .collect();
    if said != [(ONE_MOMENT, false), (ALSO, false), (COVERS, false)] {
        return Err(format!("the agent wrote {said:?}"));
    }
    Ok(())
}

#[test]
fn work_handed_to_a_second_agent_goes_on_when_the_reader_writes_to_the_first() {
    let Some(tally) = told(SEEDS, front_back, front_back_ends) else {
        return;
    };
    assert_eq!(tally.completed, tally.runs, "{tally:?}");
    assert_eq!(tally.delegated, tally.runs, "{tally:?}");
}

/// Scenario 6: with several people writing, the agent knows who wrote
/// what, and when it is being written to.
fn three_party(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("three parties in text");
    let mut plan = Plan::new(seed, spec(&[]));
    weather(&mut plan, &mut rng);
    plan.names = vec!["Ava"];
    plan.llm = Arc::new(|_| vec![LlmStep::say(TOTAL)]);
    plan.types(0, 1_000, "ben did you send the invoice");
    let answered = 1_000 + rng.range(1..2_000);
    plan.types(1, answered, "yes i sent it this morning");
    let asked = answered + rng.range(1..4_000);
    plan.types(0, asked, "ava what is the invoice total");
    plan.until_ms = asked + 60_000;
    plan
}

fn three_party_ends(_: &Plan, outcome: &Outcome) -> Result<(), String> {
    let (ana, ben) = (ParticipantId(1), ParticipantId(2));
    let wanted = [
        (ana, "ben did you send the invoice", false),
        (ben, "yes i sent it this morning", false),
        (ana, "ava what is the invoice total", true),
    ];
    if outcome.turns() != wanted {
        return Err(format!("the turns are {:?}", outcome.turns()));
    }
    if outcome.asked.len() != 1 || outcome.asked[0].last_user() != Some(wanted[2].1) {
        return Err(format!("the model was asked {} times", outcome.asked.len()));
    }
    let said: Vec<(&str, bool)> = outcome
        .utterances()
        .iter()
        .map(|(_, text, cut)| (*text, *cut))
        .collect();
    if said != [(TOTAL, false)] {
        return Err(format!("the agent wrote {said:?}"));
    }
    Ok(())
}

#[test]
fn with_several_people_the_agent_knows_who_wrote_and_to_whom() {
    let Some(tally) = told(SEEDS, three_party, three_party_ends) else {
        return;
    };
    assert_eq!(tally.turns, 3 * tally.runs, "{tally:?}");
    assert_eq!(tally.spoken, tally.runs, "{tally:?}");
}

/// A chat nobody wrote: a seed makes up what is typed and when, what the
/// model writes, which tools it calls and what the application asks.
fn made_up(profile: Profile, seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("made up in text");
    let mut plan = Plan::new(seed, crate::sweeps::spec());
    weather(&mut plan, &mut rng);
    let writers = if rng.chance(1, 4) { 2 } else { 1 };
    let mut at = 500 + rng.range(0..1_500);
    let mut last = at;
    for _ in 0..rng.range(2..8) {
        let words: Vec<&str> = (0..rng.range(1..7))
            .map(|_| WORDS[index(&mut rng, WORDS.len())])
            .collect();
        plan.types(index(&mut rng, writers), at, &words.join(" "));
        last = at;
        // The next message comes while the answer to this one is being
        // thought of or written, or after a pause.
        let pause = *rng
            .choose(&[20, 150, 400, 1_200, 2_500, 5_000, 12_000])
            .unwrap();
        at += pause + rng.range(0..600);
    }
    minds(&mut plan, &rng, profile);

    // What the application asks, now and then.
    if rng.chance(1, 5) {
        let text = "Please bear with me.".to_owned();
        let say = Doing::Ask(SessionInput::Say { text });
        plan.doings.push((rng.range(300..last + 2_000), say));
    }
    if rng.chance(1, 5) {
        let interrupt = Doing::Ask(SessionInput::Interrupt);
        plan.doings.push((rng.range(300..last + 2_000), interrupt));
    }

    match profile {
        // A chat has no line that could drop.
        Profile::Calm | Profile::Lines => {}
        Profile::Chaos => trouble(&mut plan, &mut rng, last),
        Profile::Crashes => {
            let mut dies: Vec<u64> = (0..rng.range(1..4)).map(|_| rng.range(1..45)).collect();
            dies.sort_unstable();
            dies.dedup();
            plan.dies_after = dies;
        }
        Profile::Shutdown => {
            if rng.chance(1, 4) {
                plan.tools_ms = 3_600_000;
            }
            let at = rng.range(600..last + 4_000);
            match ending(seed) {
                // There is no line to hang up.
                End::Asked | End::HungUp => plan.doings.push((at, Doing::Ask(SessionInput::Close))),
                End::Taken => plan.doings.push((at, Doing::Taken)),
            }
        }
    }
    // Long enough for everything that was begun to be given up on.
    plan.until_ms = last + 200_000;
    plan
}

#[test]
fn calm_chats() {
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Calm, seed), nothing_more) else {
        return;
    };
    assert!(tally.turns >= 2 * tally.runs, "{tally:?}");
    assert!(tally.spoken >= tally.runs, "{tally:?}");
    assert!(tally.cut >= tally.runs / 8, "{tally:?}");
    assert!(tally.completed >= tally.runs / 2, "{tally:?}");
    assert!(tally.continuations >= tally.runs / 3, "{tally:?}");
    assert_eq!(tally.lives, tally.runs, "{tally:?}");
    assert_eq!((tally.barge_ins, tally.frames), (0, 0), "{tally:?}");
}

#[test]
fn chats_in_chaos() {
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Chaos, seed), nothing_more) else {
        return;
    };
    assert!(tally.turns >= tally.runs, "{tally:?}");
    assert!(tally.spoken >= tally.runs / 2, "{tally:?}");
    assert!(tally.failed >= tally.runs / 10, "{tally:?}");
}

#[test]
fn chats_whose_process_dies() {
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Crashes, seed), nothing_more) else {
        return;
    };
    // Most sessions died at least once, and went on.
    assert!(tally.lives >= tally.runs * 3 / 2, "{tally:?}");
    assert!(tally.turns >= tally.runs, "{tally:?}");
    assert!(tally.from_state >= tally.runs / 4, "{tally:?}");
}

/// A chat that is asked to close is closed. One that another process
/// took over has stopped, if it noticed: it notices with the next thing
/// it tries to store.
fn ended_as_it_should(plan: &Plan, outcome: &Outcome) -> Result<(), String> {
    match (ending(plan.seed), outcome.ended()) {
        (End::Asked | End::HungUp, Some(Ended::Closed))
        | (End::Taken, None | Some(Ended::Lost(StoreBroken::Fenced))) => Ok(()),
        (how, ended) => Err(format!(
            "the session was to end as {how:?} and ended as {ended:?}"
        )),
    }
}

#[test]
fn chats_that_end_at_any_moment() {
    let check = ended_as_it_should;
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Shutdown, seed), check) else {
        return;
    };
    assert!(tally.closed >= tally.runs / 2, "{tally:?}");
}

/// A seed is a chat: the same steps at the same moments, every time it
/// is run.
#[test]
fn a_seed_tells_the_same_chat_every_time() {
    for profile in [Profile::Chaos, Profile::Crashes, Profile::Shutdown] {
        for seed in [3, 17] {
            let first = run(&made_up(profile, seed), true).unwrap();
            let again = run(&made_up(profile, seed), true).unwrap();
            assert!(!first.log.is_empty(), "{profile:?} {seed}");
            assert_eq!(first.log, again.log, "{profile:?} {seed}");
            assert_eq!(first.tally, again.tally, "{profile:?} {seed}");
        }
    }
}
