//! The six stories of the kernel's scenarios, told to the whole session.
//!
//! Each is one conversation. A seed decides what the story leaves open:
//! how late each part answers, how the line treats the audio, how long
//! the far end holds what it is sent. Whatever the seed, the story ends
//! the same way.

use core::time::Duration;
use std::sync::Arc;

use waveport_domain::config::{AgentSpec, Effect, Executor, KernelConfig, ToolSpec};
use waveport_domain::event::CloseReason;
use waveport_domain::fact::{Fact, Lifecycle, ToolOutcome};
use waveport_domain::ids::{AgentId, ParticipantId, ToolId};
use waveport_ports::stt::{SttFinalize, SttTiming};
use waveport_ports::tool::ToolReply;
use waveport_ports::tts::{Alignment, TtsInput};
use waveport_runtime::context::{AgentDef, ToolDef};
use waveport_runtime::session::{Ended, SessionInput, SessionSpec};
use waveport_sim::{Seed, SimRng};
use waveport_testkit::fakes::{Asked, LlmStep, Said};

use crate::world::{Doing, Line, Outcome, Plan, SESSION, recogniser, synthesiser};
use crate::{SEEDS, told};

const INSTRUCTIONS: &str = "You are a helpful agent on a call.";

/// A session with one agent that has `tools`.
fn spec(tools: &[(&str, Effect)]) -> SessionSpec {
    let ids = (0..tools.len()).map(|index| ToolId(u16::try_from(index).unwrap()));
    SessionSpec {
        kernel: KernelConfig {
            agents: vec![AgentSpec {
                tools: ids.collect(),
            }],
            tools: tools
                .iter()
                .map(|(_, effect)| ToolSpec::new(*effect))
                .collect(),
            ..KernelConfig::default()
        },
        tools: tools
            .iter()
            .map(|(name, _)| ToolDef {
                name: (*name).to_owned(),
                description: format!("The tool {name}."),
                parameters: "{}".to_owned(),
            })
            .collect(),
        ..SessionSpec::new(SESSION, INSTRUCTIONS)
    }
}

/// What a seed decides of the parts in every story: how late they are,
/// what they can do, and what the line is like.
fn weather(plan: &mut Plan, rng: &mut SimRng) {
    plan.line = *rng
        .choose(&[
            Line::SoundCard,
            Line::SoundCard,
            Line::Telephone,
            Line::Opus,
        ])
        .unwrap();
    plan.holds_ms = rng.range(0..400);
    plan.jitter_ms = rng.range(0..35);
    let timing = *rng.choose(&[SttTiming::Words, SttTiming::Window]).unwrap();
    let finalize = *rng
        .choose(&[
            SttFinalize::Both,
            SttFinalize::OnRequest,
            SttFinalize::ByProvider,
        ])
        .unwrap();
    plan.stt = recogniser(rng.range(10..500), timing, finalize);
    plan.llm_first_ms = rng.range(50..900);
    plan.llm_each_ms = rng.range(5..60);
    let alignment = *rng
        .choose(&[Alignment::Word, Alignment::Char, Alignment::None])
        .unwrap();
    let input = *rng
        .choose(&[TtsInput::Incremental, TtsInput::Whole])
        .unwrap();
    plan.tts.caps = synthesiser(alignment, input);
    plan.tts.first = Duration::from_millis(rng.range(40..600));
    plan.tts.speed = u32::try_from(rng.range(1..8)).unwrap();
    plan.tools_ms = rng.range(20..1_200);
    plan.journal_ms = rng.range(1..30);
}

/// Parts that answer soon, for a story in which something has to happen
/// while the agent speaks: a first sentence of thirteen words goes out
/// between 0.4 s and 1.6 s after the caller has finished.
fn quick(plan: &mut Plan, rng: &mut SimRng) {
    plan.stt.latency = Duration::from_millis(rng.range(10..250));
    if plan.stt.caps.finalize == SttFinalize::ByProvider {
        plan.stt.caps.finalize = SttFinalize::Both;
    }
    plan.llm_first_ms = rng.range(50..400);
    plan.llm_each_ms = rng.range(8..25);
    plan.tts.first = Duration::from_millis(rng.range(40..300));
}

const STORY: &str = "Of course. Once upon a time there was a ship that sailed all the seas of \
                     the world and never came home. Its captain was a woman from the north.";

/// Scenario 3: a caller who interrupts is answered on what they heard.
fn spoken_text(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("spoken text");
    let mut plan = Plan::new(seed, spec(&[]));
    weather(&mut plan, &mut rng);
    plan.llm = Arc::new(|asked| match asked.last_user() {
        Some("tell me a story") => vec![LlmStep::say(STORY)],
        _ => vec![LlmStep::say("All right.")],
    });
    let asked = plan.says(0, 1_000, 300, "tell me a story");
    // The caller cuts in somewhere in the story.
    let cuts_in = asked + rng.range(2_500..7_000);
    plan.says(0, cuts_in, 350, "no stop it please");
    plan.until_ms = cuts_in + 40_000;
    plan
}

fn spoken_text_ends(plan: &Plan, outcome: &Outcome) -> Result<(), String> {
    let turns: Vec<&str> = outcome.turns().iter().map(|(_, text, _)| *text).collect();
    if turns != ["tell me a story", "no stop it please"] {
        return Err(format!("the turns are {turns:?}"));
    }
    let utterances = outcome.utterances();
    let (heard, cut, then, whole) = match utterances[..] {
        [(_, heard, cut), (_, then, whole)] => (heard, cut, then, whole),
        // A synthesiser that takes its text piece by piece and says
        // nothing of where its words are leaves nothing known of what
        // was heard before it has finished: the story is not recorded.
        [(_, then, whole)] if plan.tts.caps.alignment == Alignment::None => ("", true, then, whole),
        _ => return Err(format!("the agent said {utterances:?}")),
    };
    if !cut || !STORY.starts_with(heard) || heard.len() >= STORY.len() {
        return Err(format!(
            "of the story the conversation has {heard:?}, cut off: {cut}"
        ));
    }
    // A synthesiser that says where its words are is cut between two of
    // them. One that says where each character is is cut at the one that
    // was heard last, and one that says nothing by share of characters.
    let between_words = STORY[heard.len()..].starts_with(' ');
    if plan.tts.caps.alignment == Alignment::Word && !heard.is_empty() && !between_words {
        return Err(format!("the story was cut inside a word: {heard:?}"));
    }
    if (then, whole) != ("All right.", false) {
        return Err(format!("the second answer is {then:?}, cut off: {whole}"));
    }
    // The model was shown what the caller heard, not what it wrote.
    let shown: Vec<&str> = outcome.asked[1]
        .messages
        .iter()
        .filter_map(|said| match said {
            Said::Assistant { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if shown != [heard] && !(heard.is_empty() && shown.is_empty()) {
        return Err(format!("the model was shown {shown:?} of the story"));
    }
    Ok(())
}

#[test]
fn a_caller_who_interrupts_is_answered_on_what_they_heard() {
    let Some(tally) = told(SEEDS, spoken_text, spoken_text_ends) else {
        return;
    };
    assert!(tally.cut >= tally.runs / 2, "{tally:?}");
    assert_eq!(tally.barge_ins, tally.runs, "{tally:?}");
}

/// Whether a model was shown the result of a tool call, and not the
/// stand-in for one that is still running.
fn has_result(asked: &Asked) -> bool {
    asked.messages.iter().any(|said| {
        matches!(said, Said::ToolResult { text, .. } if text != waveport_runtime::context::STILL_RUNNING)
    })
}

/// Scenario 1: what a caller says over the agent is one turn, however
/// late its words arrive.
fn lost_input(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("lost input");
    let mut plan = Plan::new(seed, spec(&[]));
    weather(&mut plan, &mut rng);
    // The words may come long after the speech that carried them, and
    // long after it was decided what the speech was.
    plan.stt.latency = Duration::from_millis(rng.range(5..1_800));
    plan.llm = Arc::new(|asked| match asked.last_user() {
        Some("tell me a story") => vec![LlmStep::say(STORY)],
        _ => vec![LlmStep::say("All right.")],
    });
    let asked = plan.says(0, 1_000, 300, "tell me a story");
    // The story begins at most 3.6 s after it was asked for and takes 9 s.
    let cuts_in = asked + rng.range(3_700..8_300);
    plan.says(
        0,
        cuts_in,
        rng.range(300..420),
        "wait that is not what i meant",
    );
    plan.until_ms = cuts_in + 60_000;
    plan
}

fn lost_input_ends(_: &Plan, outcome: &Outcome) -> Result<(), String> {
    let turns: Vec<&str> = outcome.turns().iter().map(|(_, text, _)| *text).collect();
    if turns != ["tell me a story", "wait that is not what i meant"] {
        return Err(format!("the turns are {turns:?}"));
    }
    let answered = outcome.asked.last().and_then(Asked::last_user);
    if answered != Some("wait that is not what i meant") {
        return Err(format!("the last thing answered is {answered:?}"));
    }
    match outcome.utterances().last() {
        Some((_, "All right.", false)) => Ok(()),
        other => Err(format!("the last thing the agent said is {other:?}")),
    }
}

#[test]
fn what_a_caller_says_over_the_agent_is_one_turn() {
    let Some(tally) = told(SEEDS, lost_input, lost_input_ends) else {
        return;
    };
    assert_eq!(tally.turns, 2 * tally.runs, "{tally:?}");
    assert_eq!(tally.barge_ins, tally.runs, "{tally:?}");
}

const CHECKING: &str = "Let me check that for you right now, it will only take a moment.";
const IN_STOCK: &str = "Yes, we have two in stock at ten euros each.";

/// Scenario 2: a listener's noise of agreement stops nothing.
fn backchannel_tool(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("backchannel");
    let mut plan = Plan::new(
        seed,
        spec(&[("stock", Effect::Pure), ("price", Effect::Pure)]),
    );
    weather(&mut plan, &mut rng);
    // A model says what speech over the agent's is: under half a second
    // of it is no wish to speak.
    plan.classifier = Some(500);
    quick(&mut plan, &mut rng);
    // The tools are still at work when the caller makes their noise, and
    // answer in one order or the other.
    plan.tools_ms = rng.range(3_800..6_000);
    let quick = rng.chance(1, 2);
    plan.tools = Arc::new(move |called| {
        let first = (called.tool.get() == 0) == quick;
        ToolReply::Completed(if first { "two" } else { "ten euros" }.to_owned())
    });
    plan.llm = Arc::new(|asked| {
        if has_result(asked) {
            vec![LlmStep::say(IN_STOCK)]
        } else {
            vec![
                LlmStep::say(CHECKING),
                LlmStep::call("stock", "{}"),
                LlmStep::call("price", "{}"),
            ]
        }
    });
    let asked = plan.says(0, 1_000, 300, "do you have it in stock");
    // The agent speaks from at most 1.6 s after the question, for 3.8 s.
    plan.says(0, asked + rng.range(1_700..3_700), 280, "mm-hm");
    plan.until_ms = asked + 60_000;
    plan
}

fn backchannel_tool_ends(_: &Plan, outcome: &Outcome) -> Result<(), String> {
    let turns: Vec<&str> = outcome.turns().iter().map(|(_, text, _)| *text).collect();
    if turns != ["do you have it in stock"] {
        return Err(format!("the turns are {turns:?}"));
    }
    let said: Vec<(&str, bool)> = outcome
        .utterances()
        .iter()
        .map(|(_, text, cut)| (*text, *cut))
        .collect();
    if said != [(CHECKING, false), (IN_STOCK, false)] {
        return Err(format!("the agent said {said:?}"));
    }
    let tally = &outcome.tally;
    if (tally.ducks, tally.unducks, tally.barge_ins) != (1, 1, 0) {
        return Err(format!(
            "the agent was lowered and not raised again: {tally:?}"
        ));
    }
    if (tally.sent, tally.completed, tally.continuations) != (2, 2, 1) {
        return Err(format!("the calls did not end in one answer: {tally:?}"));
    }
    if !outcome.undone.is_empty() || outcome.called.len() != 2 || outcome.asked.len() != 2 {
        return Err(format!(
            "{} calls made, {} taken back, the model asked {} times",
            outcome.called.len(),
            outcome.undone.len(),
            outcome.asked.len()
        ));
    }
    Ok(())
}

#[test]
fn a_noise_of_agreement_stops_nothing() {
    let Some(tally) = told(SEEDS, backchannel_tool, backchannel_tool_ends) else {
        return;
    };
    assert_eq!(tally.dismissed, tally.runs, "{tally:?}");
}

/// Scenario 4: a session that is asked to close closes, whatever its
/// parts do.
fn shutdown(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("shutdown");
    let mut plan = Plan::new(seed, spec(&[("book", Effect::SideEffecting)]));
    weather(&mut plan, &mut rng);
    // Each part may have fallen silent for good.
    let hour = 3_600_000;
    if rng.chance(1, 3) {
        plan.stt.latency = Duration::from_millis(hour);
    }
    if rng.chance(1, 2) {
        plan.tts.first = Duration::from_millis(hour);
    }
    if rng.chance(1, 2) {
        plan.tools_ms = hour;
    }
    let model = rng.range(0..4);
    plan.llm = Arc::new(move |asked| match model {
        0 => vec![LlmStep::Hang],
        1 => vec![LlmStep::say(STORY), LlmStep::Hang],
        2 if !has_result(asked) => vec![LlmStep::say("One moment."), LlmStep::call("book", "{}")],
        _ => vec![LlmStep::say(STORY)],
    });
    let asked = plan.says(0, 1_000, 300, "book a table for two");
    let closes = asked + rng.range(0..9_000);
    plan.doings.push((closes, Doing::Ask(SessionInput::Close)));
    plan.until_ms = closes + 60_000;
    plan
}

fn shutdown_ends(plan: &Plan, outcome: &Outcome) -> Result<(), String> {
    if outcome.ended() != Some(&Ended::Closed) {
        return Err(format!("the session ended as {:?}", outcome.ended()));
    }
    let asked_at = plan.doings[0].0;
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
fn a_session_asked_to_close_closes_whatever_its_parts_do() {
    let Some(tally) = told(SEEDS, shutdown, shutdown_ends) else {
        return;
    };
    assert_eq!(tally.closed, tally.runs, "{tally:?}");
}

const FRONT: &str = "You talk to the caller.";
const BACK: &str = "You look things up.";
const ONE_MOMENT: &str = "Give me a moment while I check what the warranty says about that.";
const ALSO: &str = "Sure, I will also ask whether accidental damage is included in it.";
const COVERS: &str = "The warranty covers parts and labour for two years.";

/// What the agent at the back finds out: its answer at the end of some
/// thinking aloud, so that it takes its time, between two and five
/// seconds.
fn researched() -> String {
    let thinking = "hm ".repeat(200);
    format!("{thinking}Parts and labour for two years.")
}

/// Scenario 5: work handed to a second agent survives an interruption of
/// the first.
fn hybrid_front_back(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("front and back");
    let delegated = ToolSpec {
        executor: Executor::Agent(AgentId(1)),
        ..ToolSpec::new(Effect::Pure)
    };
    let spec = SessionSpec {
        kernel: KernelConfig {
            agents: vec![
                AgentSpec {
                    tools: vec![ToolId(0)],
                },
                AgentSpec { tools: Vec::new() },
            ],
            tools: vec![delegated],
            ..KernelConfig::default()
        },
        agents: vec![
            AgentDef {
                instructions: FRONT.to_owned(),
            },
            AgentDef {
                instructions: BACK.to_owned(),
            },
        ],
        tools: vec![ToolDef {
            name: "research".to_owned(),
            description: "Asks the researcher.".to_owned(),
            parameters: "{}".to_owned(),
        }],
        ..SessionSpec::new(SESSION, FRONT)
    };
    let mut plan = Plan::new(seed, spec);
    weather(&mut plan, &mut rng);
    quick(&mut plan, &mut rng);
    plan.llm = Arc::new(|asked| {
        if asked.instructions == BACK {
            return vec![LlmStep::Say(researched())];
        }
        let called = asked
            .messages
            .iter()
            .any(|said| matches!(said, Said::ToolCall { .. }));
        if has_result(asked) {
            // Whatever it is asked for now, it knows the answer.
            vec![LlmStep::say(COVERS)]
        } else if called {
            vec![LlmStep::say(ALSO)]
        } else {
            vec![
                LlmStep::say(ONE_MOMENT),
                LlmStep::call("research", "{\"topic\":\"warranty\"}"),
            ]
        }
    });
    let asked = plan.says(0, 1_000, 300, "what does the warranty cover");
    // The caller cuts in while the first agent is still speaking.
    let cuts_in = asked + rng.range(1_700..3_500);
    plan.says(0, cuts_in, 330, "and accidental damage too");
    plan.until_ms = cuts_in + 90_000;
    plan
}

fn hybrid_front_back_ends(plan: &Plan, outcome: &Outcome) -> Result<(), String> {
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
    // The first agent was cut off, and the caller was told what the
    // other had found, once: in the answer to what they said next if it
    // was known by then, and otherwise in an answer of its own.
    let said: Vec<(&str, bool)> = outcome
        .utterances()
        .iter()
        .map(|(_, text, cut)| (*text, *cut))
        .collect();
    let told = said.iter().filter(|said| **said == (COVERS, false)).count();
    // What was heard of the answer that was cut off is known when the
    // synthesiser says where its words are.
    let cut_off = said
        .first()
        .is_some_and(|(text, cut)| *cut && ONE_MOMENT.starts_with(text));
    if told != 1 || !(cut_off || plan.tts.caps.alignment == Alignment::None) {
        return Err(format!("the agent said {said:?}"));
    }
    if outcome.tally.barge_ins == 0 || outcome.tally.continuations > 1 {
        return Err(format!("{:?}", outcome.tally));
    }
    Ok(())
}

#[test]
fn work_handed_to_a_second_agent_survives_an_interruption_of_the_first() {
    let Some(tally) = told(SEEDS, hybrid_front_back, hybrid_front_back_ends) else {
        return;
    };
    assert_eq!(tally.completed, tally.runs, "{tally:?}");
}

const TOTAL: &str = "The invoice total is four hundred euros.";

/// Scenario 6: with several people on the line, the agent knows who said
/// what, and when it is being spoken to.
fn three_party(seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("three parties");
    let mut plan = Plan::new(seed, spec(&[]));
    weather(&mut plan, &mut rng);
    // Whose voice is whose is told by how loud it is, which Opus does
    // not keep well enough.
    if plan.line == Line::Opus {
        plan.line = Line::SoundCard;
    }
    plan.callers = 2;
    plan.names = vec!["Ava"];
    plan.llm = Arc::new(|_| vec![LlmStep::say(TOTAL)]);
    // Ben starts to answer while Ana is still asking.
    let asked = plan.says(0, 1_000, 300, "ben did you send the invoice");
    let answered = plan.says(
        1,
        asked - rng.range(200..900),
        300,
        "yes i sent it this morning",
    );
    plan.says(
        0,
        answered + rng.range(1_500..4_000),
        300,
        "ava what is the invoice total",
    );
    plan.until_ms = answered + 60_000;
    plan
}

fn three_party_ends(_: &Plan, outcome: &Outcome) -> Result<(), String> {
    let (ana, ben) = (ParticipantId(1), ParticipantId(2));
    let mut turns = outcome.turns();
    // The two who talked over each other finished in one order or the
    // other.
    turns[..2].sort_by_key(|(who, _, _)| *who);
    let wanted = [
        (ana, "ben did you send the invoice", false),
        (ben, "yes i sent it this morning", false),
        (ana, "ava what is the invoice total", true),
    ];
    if turns != wanted {
        return Err(format!("the turns are {turns:?}"));
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
        return Err(format!("the agent said {said:?}"));
    }
    Ok(())
}

#[test]
fn with_several_people_the_agent_knows_who_spoke_and_to_whom() {
    let Some(tally) = told(SEEDS, three_party, three_party_ends) else {
        return;
    };
    assert_eq!(tally.turns, 3 * tally.runs, "{tally:?}");
}
