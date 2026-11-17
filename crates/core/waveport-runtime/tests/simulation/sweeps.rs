//! Sessions nobody wrote.
//!
//! A seed makes up who calls and what they say, what the model writes
//! and which tools it calls, and what the application asks. A profile
//! says how the world behaves: calmly, or with parts that fail and a
//! line that loses packets, or with a process that dies, or with an end
//! that comes at any moment, or with a line that drops and comes back,
//! or with callers who press the keys of their telephones. Little is
//! asked of such a session beyond what is asked of every session: the
//! checks of `world`.

use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use waveport_domain::config::{AgentSpec, Effect, KernelConfig, ToolSpec};
use waveport_domain::event::{Key, RetryClass};
use waveport_domain::ids::{ParticipantId, ToolId};
use waveport_ports::error::PortError;
use waveport_ports::stt::{SttFinalize, SttTiming};
use waveport_ports::tool::ToolReply;
use waveport_ports::tts::{Alignment, TtsInput};
use waveport_runtime::context::ToolDef;
use waveport_runtime::session::{Ended, SessionInput, SessionSpec, StoreBroken};
use waveport_sim::{Seed, SimRng};
use waveport_testkit::fakes::{LlmStep, Said};

use crate::world::{Doing, Line, Outcome, Plan, SESSION, recogniser, run, synthesiser};
use crate::{SEEDS, told};

/// How the world of a made-up session behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Profile {
    /// Every part answers, and the line is clean.
    Calm,
    /// Parts fail, hang and answer late, the line loses and delays
    /// packets, and the journal has to be asked twice.
    Chaos,
    /// The process dies, up to three times, each time at a record of
    /// its journal that the seed picks.
    Crashes,
    /// The session ends at a moment the seed picks: it is asked to
    /// close, or its line closes, or another process takes it over.
    Shutdown,
    /// The line drops, up to three times, and is back in time, too late
    /// or never. The session waits for it for a second, for five, or not
    /// at all.
    Lines,
    /// The callers press keys of their telephones, in runs, among what
    /// they say and over the agent, and the application has the session
    /// press some. What makes a run differs from session to session.
    Keys,
}

pub(super) const WORDS: &[&str] = &[
    "yes", "no", "the", "order", "shipped", "on", "monday", "it", "arrives", "please", "thanks",
    "ok", "refund", "where", "is", "my", "parcel",
];

const SENTENCES: &[&str] = &[
    "Let me look into that for you.",
    "Your order shipped on Monday.",
    "It should arrive within three days.",
    "Is there anything else I can help you with?",
    "I am sorry about that.",
    "One moment please.",
];

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(super) fn index(rng: &mut SimRng, len: usize) -> usize {
    usize::try_from(rng.range(0..len as u64)).unwrap()
}

/// One to three sentences.
fn sentences(rng: &mut SimRng) -> LlmStep {
    let count = rng.range(1..4);
    let said: Vec<&str> = (0..count)
        .map(|_| SENTENCES[index(rng, SENTENCES.len())])
        .collect();
    LlmStep::Say(said.join(" "))
}

/// A session with one agent, a tool that looks something up and a tool
/// that changes something.
pub(super) fn spec() -> SessionSpec {
    let tools = [("look_up", Effect::Pure), ("refund", Effect::SideEffecting)];
    SessionSpec {
        kernel: KernelConfig {
            agents: vec![AgentSpec {
                tools: vec![ToolId(0), ToolId(1)],
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
        ..SessionSpec::new(SESSION, "You are a helpful agent on a call.")
    }
}

/// The parts and the line of a made-up session.
fn parts(plan: &mut Plan, rng: &mut SimRng) {
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
    plan.tools_ms = rng.range(30..900);
    plan.journal_ms = rng.range(1..30);
}

/// What the callers say. Returns when the last of them has finished.
fn callers(plan: &mut Plan, rng: &mut SimRng) -> u64 {
    // Whose voice is whose is told by how loud it is, which Opus does
    // not keep well enough for two.
    plan.callers = if plan.line != Line::Opus && rng.chance(1, 4) {
        2
    } else {
        1
    };
    let mut at = 800 + rng.range(0..1_500);
    let mut free = vec![0_u64; plan.callers];
    let mut last = at;
    for _ in 0..rng.range(2..7) {
        let who = index(rng, plan.callers);
        let from = at.max(free[who] + 400);
        let words: Vec<&str> = (0..rng.range(1..7))
            .map(|_| WORDS[index(rng, WORDS.len())])
            .collect();
        let to = plan.says(who, from, rng.range(260..420), &words.join(" "));
        free[who] = to;
        last = last.max(to);
        let pause = *rng.choose(&[200, 600, 1_200, 2_500, 5_000]).unwrap();
        at = to + pause + rng.range(0..900);
        // Now and then the next to speak does not wait for this one.
        if plan.callers == 2 && rng.chance(1, 3) {
            at = to.saturating_sub(rng.range(100..700));
        }
    }
    last
}

/// What the model writes, and what the tools answer.
pub(super) fn minds(plan: &mut Plan, rng: &SimRng, profile: Profile) {
    let dice = Mutex::new(rng.fork("model"));
    plan.llm = Arc::new(move |asked| {
        let mut rng = lock(&dice);
        let results = asked
            .messages
            .iter()
            .filter(|said| matches!(said, Said::ToolResult { .. }))
            .count();
        let roll = rng.range(0..100);
        let unruly = profile == Profile::Chaos;
        if unruly && roll < 5 {
            return vec![LlmStep::Fail(PortError::transient("overloaded"))];
        }
        if unruly && roll < 8 {
            return vec![LlmStep::Fail(PortError::permanent("refused"))];
        }
        if (unruly || profile == Profile::Shutdown) && roll < 12 {
            return vec![LlmStep::Hang];
        }
        if unruly && roll < 15 {
            return vec![LlmStep::call("no_such_tool", "{}")];
        }
        if unruly && roll < 18 {
            return vec![sentences(&mut rng), LlmStep::Hang];
        }
        if results < 3 && roll < 48 {
            let mut steps = Vec::new();
            if rng.chance(1, 2) {
                steps.push(sentences(&mut rng));
            }
            for _ in 0..=u64::from(rng.chance(1, 4)) {
                let tool = if rng.chance(1, 2) {
                    "look_up"
                } else {
                    "refund"
                };
                steps.push(LlmStep::call(tool, "{}"));
            }
            return steps;
        }
        vec![sentences(&mut rng)]
    });

    let dice = Mutex::new(rng.fork("tools"));
    plan.tools = Arc::new(move |called| {
        let mut rng = lock(&dice);
        if profile == Profile::Chaos && rng.chance(1, 6) {
            let class = if rng.chance(1, 2) {
                RetryClass::Transient
            } else {
                RetryClass::Permanent
            };
            return ToolReply::Failed {
                class,
                message: "it did not work".to_owned(),
            };
        }
        ToolReply::Completed(format!("done {}", called.call.get()))
    });
}

/// Parts that fail, and a line that loses what it carries.
pub(super) fn trouble(plan: &mut Plan, rng: &mut SimRng, last: u64) {
    if rng.chance(1, 3) {
        let after = usize::try_from(rng.range(2..25)).unwrap();
        plan.stt.fail_after = Some((after, PortError::transient("reset by peer")));
    }
    if rng.chance(1, 25) {
        plan.stt.fail_open = Some(PortError::transient("no route"));
    }
    if rng.chance(1, 3) {
        let after = usize::try_from(rng.range(1..40)).unwrap();
        let error = if rng.chance(1, 2) {
            PortError::transient("reset by peer")
        } else {
            PortError::permanent("voice not found")
        };
        plan.tts.fail_after = Some((after, error));
    }
    if rng.chance(1, 20) {
        plan.tts.fail_open = Some(PortError::transient("no route"));
    }
    if rng.chance(1, 6) {
        // Longer than a tool is given.
        plan.tools_ms = 15_000;
    }
    plan.jitter_ms = rng.range(0..90);
    plan.lost_per_mille = rng.range(0..60);
    for _ in 0..rng.range(0..4) {
        plan.doings
            .push((rng.range(500..last + 3_000), Doing::JournalFails));
    }
}

/// How a session that is to end does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum End {
    Asked,
    HungUp,
    Taken,
}

pub(super) fn ending(seed: u64) -> End {
    let mut rng = SimRng::new(Seed(seed)).fork("ending");
    *rng.choose(&[End::Asked, End::Asked, End::HungUp, End::Taken])
        .unwrap()
}

fn made_up(profile: Profile, seed: u64) -> Plan {
    let mut rng = SimRng::new(Seed(seed)).fork("made up");
    let mut plan = Plan::new(seed, spec());
    parts(&mut plan, &mut rng);
    let last = callers(&mut plan, &mut rng);
    minds(&mut plan, &rng, profile);

    // What the application asks, now and then.
    if rng.chance(1, 5) {
        let text = "Please hold the line.".to_owned();
        let say = Doing::Ask(SessionInput::Say { text });
        plan.doings.push((rng.range(300..last + 2_000), say));
    }
    if rng.chance(1, 5) {
        let interrupt = Doing::Ask(SessionInput::Interrupt);
        plan.doings.push((rng.range(300..last + 2_000), interrupt));
    }
    if rng.chance(1, 8) {
        let typed = Doing::Ask(SessionInput::Text {
            who: ParticipantId(1),
            text: "hello are you there".to_owned(),
        });
        plan.doings.push((rng.range(300..last + 2_000), typed));
    }

    match profile {
        Profile::Calm => {}
        Profile::Chaos => trouble(&mut plan, &mut rng, last),
        Profile::Crashes => {
            let mut dies: Vec<u64> = (0..rng.range(1..4)).map(|_| rng.range(1..45)).collect();
            dies.sort_unstable();
            dies.dedup();
            plan.dies_after = dies;
        }
        Profile::Shutdown => {
            let hour = 3_600_000;
            if rng.chance(1, 4) {
                plan.tts.first = Duration::from_millis(hour);
            }
            if rng.chance(1, 4) {
                plan.tools_ms = hour;
            }
            let at = rng.range(600..last + 4_000);
            match ending(seed) {
                End::Asked => plan.doings.push((at, Doing::Ask(SessionInput::Close))),
                End::HungUp => plan.hangs_up_ms = Some(at),
                End::Taken => plan.doings.push((at, Doing::Taken)),
            }
        }
        Profile::Lines => {
            let wait = *rng.choose(&[0, 1_000, 5_000, 5_000]).unwrap();
            plan.spec.kernel.timing.detach_wait = Duration::from_millis(wait);
            let mut from = 400;
            for _ in 0..rng.range(1..4) {
                let drops = rng.range(from..from + last / 2 + 2_000);
                if rng.chance(1, 6) {
                    // It never comes back, so it cannot drop again.
                    plan.drops.push((drops, None));
                    break;
                }
                let back = drops + rng.range(60..2_500);
                plan.drops.push((drops, Some(back)));
                from = back + 200;
            }
        }
        Profile::Keys => {
            let rules = &mut plan.spec.kernel.keys;
            let hash = Some(Key::Hash);
            rules.terminator = *rng.choose(&[hash, hash, Some(Key::Star), None]).unwrap();
            rules.count = *rng.choose(&[None, None, Some(1), Some(4)]).unwrap();
            rules.wait = Duration::from_millis(*rng.choose(&[300, 1_000, 3_000]).unwrap());
            for caller in 0..plan.callers {
                for _ in 0..rng.range(1..4) {
                    let run: String = (0..rng.range(1..7))
                        .map(|_| rng.choose(&Key::ALL).unwrap().as_char())
                        .collect();
                    plan.presses(caller, rng.range(400..last + 3_000), &run);
                }
            }
            for _ in 0..rng.range(1..3) {
                let keys = (0..rng.range(1..6))
                    .map(|_| *rng.choose(&Key::ALL).unwrap())
                    .collect();
                let press = Doing::Ask(SessionInput::Press { keys });
                plan.doings.push((rng.range(300..last + 2_000), press));
            }
        }
    }
    // Long enough for everything that was begun to be given up on.
    plan.until_ms = last + 200_000;
    plan
}

/// The check of a sweep that asks nothing beyond what every run is asked.
#[expect(
    clippy::unnecessary_wraps,
    reason = "it stands where the checks that can fail stand"
)]
pub(super) fn nothing_more(_: &Plan, _: &Outcome) -> Result<(), String> {
    Ok(())
}

#[test]
fn calm() {
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Calm, seed), nothing_more) else {
        return;
    };
    assert!(tally.turns >= 2 * tally.runs, "{tally:?}");
    assert!(tally.spoken >= tally.runs, "{tally:?}");
    assert!(tally.cut >= tally.runs / 8, "{tally:?}");
    assert!(tally.barge_ins >= tally.runs / 8, "{tally:?}");
    assert!(tally.completed >= tally.runs / 2, "{tally:?}");
    assert!(tally.continuations >= tally.runs / 3, "{tally:?}");
    assert_eq!(tally.lives, tally.runs, "{tally:?}");
    // Three sessions in four keep their state as they go, and none was
    // rebuilt from one, because none died. Every state that was stored
    // was held against what a client is shown of the facts before it.
    assert!(tally.states >= tally.runs, "{tally:?}");
    assert_eq!(tally.from_state, 0, "{tally:?}");
    assert_eq!(tally.shown, tally.states, "{tally:?}");
}

#[test]
fn chaos() {
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Chaos, seed), nothing_more) else {
        return;
    };
    assert!(tally.turns >= tally.runs, "{tally:?}");
    assert!(tally.spoken >= tally.runs / 2, "{tally:?}");
    assert!(tally.failed >= tally.runs / 10, "{tally:?}");
    // Recognisers failed, were opened again, and the callers were still
    // heard.
    assert!(tally.reopened > 0, "{tally:?}");
}

#[test]
fn crashes() {
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Crashes, seed), nothing_more) else {
        return;
    };
    // Most sessions died at least once, and went on.
    assert!(tally.lives >= tally.runs * 3 / 2, "{tally:?}");
    assert!(tally.turns >= tally.runs, "{tally:?}");
    assert!(tally.notices >= tally.runs / 4, "{tally:?}");
    assert!(tally.in_doubt >= tally.runs / 50, "{tally:?}");
    // And many of them from the state their journal held, not from all
    // their records.
    assert!(tally.from_state >= tally.runs / 4, "{tally:?}");
}

/// A session that is asked to close, or whose line closes, is closed. One
/// that another process took over has stopped, if it noticed: it notices
/// with the next thing it tries to store.
fn ended_as_it_should(plan: &Plan, outcome: &Outcome) -> Result<(), String> {
    match (ending(plan.seed), outcome.ended()) {
        (End::Asked | End::HungUp, Some(Ended::Closed))
        | (End::Taken, None | Some(Ended::Lost(StoreBroken::Fenced))) => Ok(()),
        // A line that closes after everyone has stopped talking closes
        // with the microphones, which is when the plan says or before.
        (how, ended) => Err(format!(
            "the session was to end as {how:?} and ended as {ended:?}"
        )),
    }
}

#[test]
fn shutdown() {
    let check = ended_as_it_should;
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Shutdown, seed), check) else {
        return;
    };
    assert!(tally.closed >= tally.runs / 2, "{tally:?}");
    assert!(tally.lost >= tally.runs / 20, "{tally:?}");
}

/// A session whose line dropped for good is closed, and no other is,
/// whatever became of its line on the way.
fn waited_as_it_should(plan: &Plan, outcome: &Outcome) -> Result<(), String> {
    let wait = plan.spec.kernel.timing.detach_wait;
    let wait = u64::try_from(wait.as_millis()).unwrap();
    // The line is given up when it stays gone for as long as the session
    // waits for one. A line that is back at the very moment may be
    // either.
    let given_up =
        |&(drops, back): &(u64, Option<u64>)| back.is_none_or(|back| back > drops + wait);
    let kept = |&(drops, back): &(u64, Option<u64>)| back.is_some_and(|back| back < drops + wait);
    let ended = outcome.ended();
    if plan.drops.iter().any(given_up) && ended != Some(&Ended::Closed) {
        return Err(format!(
            "a line stayed gone for more than {wait} ms, and the session ended as {ended:?}"
        ));
    }
    if plan.drops.iter().all(kept) && ended.is_some() {
        return Err(format!(
            "every line was back within {wait} ms, and the session ended as {ended:?}"
        ));
    }
    Ok(())
}

#[test]
fn lines() {
    let check = waited_as_it_should;
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Lines, seed), check) else {
        return;
    };
    assert!(tally.drops >= tally.runs, "{tally:?}");
    // Some sessions wait in vain or not at all, and most go on.
    assert!(tally.abandoned >= tally.runs / 8, "{tally:?}");
    assert!(tally.closed < tally.runs, "{tally:?}");
    assert!(tally.turns >= tally.runs, "{tally:?}");
    assert!(tally.spoken >= tally.runs, "{tally:?}");
    assert!(tally.cut >= tally.runs / 8, "{tally:?}");
}

/// The keys a session was asked to press left as its line takes them,
/// and the keys its callers pressed are in its conversation.
fn pressed_as_it_should(plan: &Plan, outcome: &Outcome) -> Result<(), String> {
    let mut asked: Vec<(u64, &[Key])> = plan
        .doings
        .iter()
        .filter_map(|(at, doing)| match doing {
            Doing::Ask(SessionInput::Press { keys }) => Some((*at, &keys[..])),
            _ => None,
        })
        .collect();
    asked.sort_by_key(|(at, _)| *at);
    let asked: Vec<Key> = asked
        .iter()
        .flat_map(|(_, keys)| keys.iter().copied())
        .collect();
    let [life] = &outcome.lives[..] else {
        return Err(format!("the session had {} lives", outcome.lives.len()));
    };
    let pressed: Vec<Key> = life.wire.pressed.iter().map(|(_, key)| *key).collect();
    let tones = life.wire.tones.len();
    let all = u64::try_from(asked.len()).unwrap();
    let as_it_should = match plan.line {
        // A sound card has no keys, and the session says so.
        Line::SoundCard => pressed.is_empty() && tones == 0 && life.unpressed == all,
        // Tones, of which those that sounded over the agent are in its
        // frames: ten frames a key at most are of their own.
        Line::Telephone => pressed.is_empty() && tones <= 10 * asked.len() && life.unpressed == 0,
        Line::Opus => pressed == asked && tones == 0 && life.unpressed == 0,
    };
    if !as_it_should {
        return Err(format!(
            "on {:?} the session was asked to press {asked:?}: it had {pressed:?} pressed, sent \
             {tones} frames of tones and left {} keys",
            plan.line, life.unpressed
        ));
    }

    // Where no key ends a run, every key a caller pressed is a key of a
    // run, so what is in the conversation is what they pressed.
    if plan.spec.kernel.keys.terminator.is_none() {
        for caller in 0..plan.callers {
            let mut keys: Vec<(u64, char)> = plan
                .keys
                .iter()
                .filter(|(_, who, _)| *who == caller)
                .flat_map(|(at, _, run)| (*at..).step_by(200).zip(run.chars()))
                .collect();
            keys.sort_by_key(|(at, _)| *at);
            let keys: String = keys.iter().map(|(_, key)| *key).collect();
            let who = ParticipantId(u16::try_from(caller + 1).unwrap());
            let kept: String = outcome
                .keys()
                .iter()
                .filter(|(by, _)| *by == who)
                .map(|(_, keys)| *keys)
                .collect();
            // Where a number of keys is a run, somebody who presses
            // faster than their words come in begins more runs than a
            // session holds open, and the keys beyond that are not
            // taken. What is taken is still what was pressed, in order.
            let mut pressed = keys.chars();
            let in_order = kept.chars().all(|key| pressed.any(|next| next == key));
            let all = plan.spec.kernel.keys.count.is_some() || kept == keys;
            if !in_order || !all {
                return Err(format!(
                    "{who} pressed {keys:?}, and the conversation has {kept:?}"
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn keys() {
    let check = pressed_as_it_should;
    let Some(tally) = told(SEEDS, |seed| made_up(Profile::Keys, seed), check) else {
        return;
    };
    // Several runs of keys to a session, among what is said and over the
    // agent, which every key stops.
    assert!(tally.keyed >= 2 * tally.runs, "{tally:?}");
    assert!(tally.turns >= 2 * tally.runs, "{tally:?}");
    assert!(tally.barge_ins >= tally.runs / 2, "{tally:?}");
    assert!(tally.spoken >= tally.runs, "{tally:?}");
    // And keys that leave in each of the three ways: pressed by the
    // line, as tones, and not at all.
    assert!(tally.pressed >= tally.runs / 4, "{tally:?}");
    assert!(tally.tones >= tally.runs, "{tally:?}");
    assert!(tally.unpressed >= tally.runs / 4, "{tally:?}");
    assert_eq!(tally.lives, tally.runs, "{tally:?}");
}

/// A seed is a session: the same steps at the same moments, every time
/// it is run. A failure is reported as the seed that made it, which is
/// worth something only if this holds.
#[test]
fn a_seed_tells_the_same_session_every_time() {
    for profile in [
        Profile::Chaos,
        Profile::Crashes,
        Profile::Shutdown,
        Profile::Lines,
        Profile::Keys,
    ] {
        for seed in [3, 17] {
            let first = run(&made_up(profile, seed), true).unwrap();
            let again = run(&made_up(profile, seed), true).unwrap();
            assert!(!first.log.is_empty(), "{profile:?} {seed}");
            let differs = first
                .log
                .iter()
                .zip(&again.log)
                .position(|(one, other)| one != other)
                .map(|at| (&first.log[at], &again.log[at]));
            assert_eq!(differs, None, "{profile:?} {seed}");
            assert_eq!(first.log.len(), again.log.len(), "{profile:?} {seed}");
            assert_eq!(first.tally, again.tally, "{profile:?} {seed}");
        }
    }
}
