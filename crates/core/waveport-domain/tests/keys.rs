//! The keys of a telephone, one rule at a time: what makes a run of them
//! a turn, what a key does to speech, and in which order what somebody
//! said and what they pressed enter the conversation.
//!
//! The stories are `keys_are_one_turn` and `keys_after_speech` in
//! `scenarios/`.

#[allow(
    dead_code,
    reason = "shared with the other test files, each of which uses part of it"
)]
mod support;

use core::time::Duration;

use waveport_domain::command::{ClientEvent, Command, LlmCause, LlmCmd, PlayoutCmd, SttCmd};
use waveport_domain::config::{KernelConfig, KeyPolicy, Limits};
use waveport_domain::event::{CloseReason, ControlInput, Event, Finish, Key, TimerId, VadEdge};
use waveport_domain::fact::{Dismissal, EpochCause, Fact, Lifecycle};
use waveport_domain::ids::{Epoch, OpTag, ParticipantId, Prob, TurnId};
use waveport_domain::time::{MediaSpan, MediaTime, MonoTime};

use self::support::{ANA, Emitted, Session, desk, played};

const BEN: ParticipantId = ParticipantId(2);
/// How long after a key a run is over, unless a session is told otherwise.
const WAIT_MS: u64 = 3_000;

#[expect(
    clippy::unwrap_used,
    reason = "a helper of tests: a failed unwrap fails the test"
)]
fn key(who: ParticipantId, written: char) -> Event {
    Event::Key {
        who,
        key: Key::from_char(written).unwrap(),
    }
}

/// Ana presses `keys`, each a quarter of a second after the one before.
/// Returns what the last of them led to.
#[expect(
    clippy::unwrap_used,
    reason = "a helper of tests: a failed unwrap fails the test"
)]
fn presses(session: &mut Session, keys: &str) -> Emitted {
    let mut last = None;
    for written in keys.chars() {
        last = Some(session.after(250, key(ANA, written)));
    }
    last.unwrap()
}

/// The keys that entered the conversation in a step, with their turns.
fn keyed(emitted: &Emitted) -> Vec<(TurnId, ParticipantId, &str)> {
    emitted
        .told()
        .into_iter()
        .filter_map(|(fact, keys)| match fact {
            Fact::KeysCommitted { turn, who, .. } => Some((turn, who, keys)),
            _ => None,
        })
        .collect()
}

fn started_for(emitted: &Emitted) -> Option<LlmCause> {
    emitted.started().and_then(|(_, start)| match start {
        LlmCmd::Start { cause, .. } => Some(cause),
        _ => None,
    })
}

fn waits(emitted: &Emitted, turn: TurnId, until_ms: u64) -> bool {
    emitted.has(Command::SetTimer(
        TimerId::Keys(turn),
        Some(MonoTime::from_millis(until_ms)),
    ))
}

fn with_keys(keys: KeyPolicy) -> Session {
    Session::with(KernelConfig { keys, ..desk() })
}

/// A session in which the agent is in the middle of its answer.
fn speaking() -> (Session, OpTag) {
    let answer = "It shipped on Monday. It arrives on Thursday.";
    let mut session = Session::new();
    let tag = session.asked();
    session.writes(tag, answer);
    session.done(tag, Finish::Stop);
    session.voiced(tag, answer.len());
    session.op(tag, played(900, 21, false));
    (session, tag)
}

fn edge(who: ParticipantId, edge: VadEdge, at_ms: u64) -> Event {
    Event::Speech {
        who,
        edge,
        at: MediaTime::from_millis(at_ms),
    }
}

fn words(who: ParticipantId, from_ms: u64, to_ms: u64, text: &str) -> Event {
    Event::Transcript {
        who,
        span: MediaSpan::new(
            MediaTime::from_millis(from_ms),
            MediaTime::from_millis(to_ms),
        ),
        text: text.into(),
        is_final: true,
    }
}

// ---- What makes a run of keys a turn -----------------------------------------

#[test]
fn keys_that_follow_one_another_are_one_turn() {
    let mut session = Session::new();
    let first = session.at(1_000, key(ANA, '4'));
    // A key by itself is nothing yet: another may follow.
    assert_eq!(first.facts(), Vec::new());
    assert!(waits(&first, TurnId(1), 1_000 + WAIT_MS));
    let view = session.kernel.view();
    let open = view.turns().collect::<Vec<_>>();
    assert_eq!(open.len(), 1);
    assert!(open[0].keys && !open[0].speaking);
    assert_eq!((open[0].turn, open[0].who), (TurnId(1), ANA));

    // Each key gives whoever presses the same time again.
    let more = presses(&mut session, "071");
    assert_eq!(more.facts(), Vec::new());
    assert!(waits(&more, TurnId(1), 1_750 + WAIT_MS));

    // The hash ends the run, and is not one of its keys.
    let over = session.after(250, key(ANA, '#'));
    assert_eq!(keyed(&over), [(TurnId(1), ANA, "4071")]);
    assert!(over.has(Command::SetTimer(TimerId::Keys(TurnId(1)), None)));
    // They are owed a reply, as a turn meant for the agent is.
    assert_eq!(
        started_for(&over),
        Some(LlmCause::Reply { turn: TurnId(1) })
    );
    assert_eq!(session.items(), ["1 pressed: 4071"]);
    assert_eq!(session.kernel.view().turns().count(), 0);
}

#[test]
fn keys_that_nothing_ends_are_a_turn_when_no_key_follows() {
    let mut session = Session::new();
    session.at(1_000, key(ANA, '4'));
    session.at(2_500, key(ANA, '0'));
    // The timer of the first key fires. The second came since.
    let early = session.at(1_000 + WAIT_MS, Event::Timer(TimerId::Keys(TurnId(1))));
    assert!(early.is_silent());
    let almost = session.at(2_499 + WAIT_MS, Event::Timer(TimerId::Keys(TurnId(1))));
    assert!(almost.is_silent());

    let over = session.at(2_500 + WAIT_MS, Event::Timer(TimerId::Keys(TurnId(1))));
    assert_eq!(keyed(&over), [(TurnId(1), ANA, "40")]);
    assert_eq!(
        started_for(&over),
        Some(LlmCause::Reply { turn: TurnId(1) })
    );
    // A timer for keys that are a turn already wakes nobody.
    assert!(
        session
            .after(10, Event::Timer(TimerId::Keys(TurnId(1))))
            .is_silent()
    );
    assert!(
        session
            .after(10, Event::Timer(TimerId::Keys(TurnId(9))))
            .is_silent()
    );
}

#[test]
fn a_number_of_keys_is_a_run() {
    let policy = KeyPolicy {
        count: Some(4),
        ..KeyPolicy::default()
    };
    let mut session = with_keys(policy);
    assert_eq!(presses(&mut session, "407").facts(), Vec::new());
    let fourth = session.after(250, key(ANA, '1'));
    assert_eq!(keyed(&fourth), [(TurnId(1), ANA, "4071")]);
    // The fifth begins the next, and the hash still ends one early.
    let fifth = session.after(250, key(ANA, '9'));
    assert_eq!(fifth.facts(), Vec::new());
    assert_eq!(session.kernel.view().turns().count(), 1);
    let over = session.after(250, key(ANA, '#'));
    assert_eq!(keyed(&over), [(TurnId(2), ANA, "9")]);
}

#[test]
fn the_key_that_ends_runs_is_a_run_by_itself_when_none_is_under_way() {
    let mut session = Session::new();
    let alone = session.at(1_000, key(ANA, '#'));
    assert_eq!(keyed(&alone), [(TurnId(1), ANA, "#")]);
    assert_eq!(
        started_for(&alone),
        Some(LlmCause::Reply { turn: TurnId(1) })
    );
    assert_eq!(session.kernel.view().turns().count(), 0);
}

#[test]
fn with_no_key_that_ends_a_run_the_hash_is_a_key_like_another() {
    let policy = KeyPolicy {
        terminator: None,
        ..KeyPolicy::default()
    };
    let mut session = with_keys(policy);
    assert_eq!(presses(&mut session, "40#*").facts(), Vec::new());
    let over = session.after(WAIT_MS, Event::Timer(TimerId::Keys(TurnId(1))));
    assert_eq!(keyed(&over), [(TurnId(1), ANA, "40#*")]);

    // Another key may end runs: the star here.
    let policy = KeyPolicy {
        terminator: Some(Key::Star),
        wait: Duration::from_secs(1),
        ..KeyPolicy::default()
    };
    let mut session = with_keys(policy);
    assert_eq!(presses(&mut session, "4#").facts(), Vec::new());
    let over = session.after(250, key(ANA, '*'));
    assert_eq!(keyed(&over), [(TurnId(1), ANA, "4#")]);
    // And a session waits as long as it is told to.
    session.at(10_000, key(ANA, '7'));
    assert!(
        session
            .at(10_999, Event::Timer(TimerId::Keys(TurnId(2))))
            .is_silent()
    );
    let over = session.at(11_000, Event::Timer(TimerId::Keys(TurnId(2))));
    assert_eq!(keyed(&over), [(TurnId(2), ANA, "7")]);
}

#[test]
fn a_run_is_no_longer_than_a_session_has_room_for() {
    let limits = Limits {
        keys: 3,
        ..Limits::default()
    };
    let mut session = Session::with(KernelConfig { limits, ..desk() });
    assert_eq!(presses(&mut session, "40").facts(), Vec::new());
    let third = session.after(250, key(ANA, '7'));
    assert_eq!(keyed(&third), [(TurnId(1), ANA, "407")]);
    // The key after it begins the next run.
    assert_eq!(session.after(250, key(ANA, '1')).facts(), Vec::new());
    let over = session.after(250, key(ANA, '#'));
    assert_eq!(keyed(&over), [(TurnId(2), ANA, "1")]);
}

#[test]
fn every_key_there_is_can_be_pressed() {
    let policy = KeyPolicy {
        terminator: None,
        ..KeyPolicy::default()
    };
    let mut session = with_keys(policy);
    for pressed in Key::ALL {
        session.after(
            100,
            Event::Key {
                who: ANA,
                key: pressed,
            },
        );
    }
    let over = session.after(WAIT_MS, Event::Timer(TimerId::Keys(TurnId(1))));
    assert_eq!(keyed(&over), [(TurnId(1), ANA, "0123456789*#ABCD")]);
    for (code, pressed) in (0..).zip(Key::ALL) {
        assert_eq!(pressed.code(), code);
        assert_eq!(Key::from_code(code), Some(pressed));
        assert_eq!(Key::from_char(pressed.as_char()), Some(pressed));
    }
    assert_eq!(Key::from_code(16), None);
    assert_eq!(Key::from_char('E'), None);
    assert_eq!(Key::from_char('d'), Some(Key::D));
}

// ---- Keys and the agent --------------------------------------------------------

#[test]
fn a_key_while_the_agent_speaks_stops_it_at_once() {
    let (mut session, tag) = speaking();
    let pressed = session.step(key(ANA, '4'));
    // Nothing is judged: a key is never a noise of agreement.
    assert!(pressed.has(Command::Playout(tag, PlayoutCmd::Stop { fade_ms: 8 })));
    assert!(matches!(
        pressed.told()[..],
        [
            (
                Fact::UtteranceCommitted {
                    truncated: true,
                    ..
                },
                "It shipped on Monday."
            ),
            (
                Fact::EpochAdvanced {
                    to: Epoch(1),
                    cause: EpochCause::BargeIn
                },
                _
            ),
        ]
    ));
    // The keys that follow stop nothing more, and are answered as one.
    assert_eq!(presses(&mut session, "071").facts(), Vec::new());
    let over = session.after(250, key(ANA, '#'));
    assert_eq!(keyed(&over), [(TurnId(2), ANA, "4071")]);
    assert_eq!(
        started_for(&over),
        Some(LlmCause::Reply { turn: TurnId(2) })
    );
    assert_eq!(
        session.items(),
        [
            "1: can you help me",
            "agent 0: It shipped on Monday. [cut]",
            "1 pressed: 4071"
        ]
    );
}

#[test]
fn nothing_is_heard_while_keys_are_pressed() {
    let mut session = Session::new();
    let tag = session.asked();
    // The answer is written and ready, and Ana presses before any of it
    // is played.
    session.step(key(ANA, '4'));
    session.writes(tag, "It shipped on Monday. ");
    let ready = session.done(tag, Finish::Stop);
    assert!(!ready.speaks(), "{ready:?}");
    // What the keys ask for is what is answered: the answer nobody heard
    // gives way to it.
    let over = session.after(250, key(ANA, '#'));
    assert_eq!(keyed(&over), [(TurnId(2), ANA, "4")]);
    assert!(over.told().iter().any(|(fact, _)| matches!(
        fact,
        Fact::EpochAdvanced {
            cause: EpochCause::Superseded,
            ..
        }
    )));
    assert_eq!(
        started_for(&over),
        Some(LlmCause::Reply { turn: TurnId(2) })
    );
    assert_eq!(
        session.items(),
        ["1: can you help me", "1 pressed: 4"],
        "nothing of the first answer was heard"
    );
}

// ---- Keys and speech -----------------------------------------------------------

#[test]
fn what_was_said_before_keys_comes_before_them() {
    let mut session = Session::new();
    // Ana says two words and presses before the recogniser has them.
    session.at(1_000, edge(ANA, VadEdge::Start, 1_000));
    session.at(1_600, edge(ANA, VadEdge::End, 1_600));
    let first = session.at(1_700, key(ANA, '4'));
    // What she was saying is over: the recogniser is asked for the rest.
    assert!(first.has(Command::Stt(SttCmd::Finalize { who: ANA })));
    assert_eq!(first.facts(), Vec::new());
    presses(&mut session, "071");
    // The keys are over, and wait for the words that were said before.
    let over = session.after(250, key(ANA, '#'));
    assert_eq!(over.facts(), Vec::new());
    assert_eq!(session.kernel.view().turns().count(), 2);

    let said = session.after(100, words(ANA, 1_000, 1_600, "it is"));
    assert!(matches!(
        said.told()[..],
        [
            (
                Fact::TurnCommitted {
                    turn: TurnId(1),
                    ..
                },
                "it is"
            ),
            (
                Fact::KeysCommitted {
                    turn: TurnId(2),
                    ..
                },
                "4071"
            ),
        ]
    ));
    // One reply, to all of it.
    assert_eq!(
        started_for(&said),
        Some(LlmCause::Reply { turn: TurnId(2) })
    );
    assert_eq!(session.items(), ["1: it is", "1 pressed: 4071"]);
}

#[test]
fn a_key_ends_what_its_presser_was_saying() {
    let mut session = Session::new();
    session.at(1_000, edge(ANA, VadEdge::Start, 1_000));
    // She presses in the middle of a word. Nobody is going to say where
    // her speech ends.
    let pressed = session.at(1_400, key(ANA, '4'));
    assert!(pressed.has(Command::Emit(ClientEvent::UserSpeaking {
        who: ANA,
        turn: TurnId(1),
        speaking: false,
    })));
    assert!(pressed.has(Command::Stt(SttCmd::Finalize { who: ANA })));
    let over = session.after(250, key(ANA, '#'));
    assert_eq!(over.facts(), Vec::new());

    // No words come. The turn of speech is given up when they have had
    // their time, and the keys are a turn then.
    let wait = desk().timing.transcript_wait;
    let wait = u64::try_from(wait.as_millis()).unwrap();
    let given_up = session.at(1_400 + wait, Event::Timer(TimerId::Eou(TurnId(1))));
    assert!(matches!(
        given_up.told()[..],
        [
            (
                Fact::TurnDismissed {
                    turn: TurnId(1),
                    reason: Dismissal::Empty,
                    ..
                },
                _
            ),
            (
                Fact::KeysCommitted {
                    turn: TurnId(2),
                    ..
                },
                "4"
            ),
        ]
    ));
}

#[test]
fn what_is_said_after_keys_comes_after_them() {
    let mut session = Session::new();
    session.at(1_000, key(ANA, '4'));
    // Ana says something while her keys could still go on, and the words
    // are in at once.
    session.at(1_200, edge(ANA, VadEdge::Start, 1_200));
    session.at(1_800, edge(ANA, VadEdge::End, 1_800));
    let turn = TurnId(2);
    session.at(1_860, Event::Eou { turn, p: Prob::ONE });
    let said = session.at(2_000, words(ANA, 1_200, 1_800, "sorry"));
    assert_eq!(said.facts(), Vec::new(), "the words wait for the keys");

    // Speech does not end a run of keys: the tone of a key can sound like
    // speech to whoever listens for it. The next key is of the same run.
    let more = session.at(2_200, key(ANA, '0'));
    assert_eq!(more.facts(), Vec::new());
    let over = session.at(2_400, key(ANA, '#'));
    assert!(matches!(
        over.told()[..],
        [
            (
                Fact::KeysCommitted {
                    turn: TurnId(1),
                    ..
                },
                "40"
            ),
            (
                Fact::TurnCommitted {
                    turn: TurnId(2),
                    ..
                },
                "sorry"
            ),
        ]
    ));
    assert_eq!(session.items(), ["1 pressed: 40", "1: sorry"]);
}

#[test]
fn speech_that_goes_on_after_keys_is_a_new_turn() {
    let mut session = Session::new();
    session.at(1_000, edge(ANA, VadEdge::Start, 1_000));
    session.at(1_600, edge(ANA, VadEdge::End, 1_600));
    session.at(1_700, key(ANA, '4'));
    // She speaks again before the words of before are in. It is not the
    // same turn going on: keys were pressed in between.
    let again = session.at(2_000, edge(ANA, VadEdge::Start, 2_000));
    assert!(matches!(
        again.told()[..],
        [(
            Fact::TurnDismissed {
                turn: TurnId(1),
                reason: Dismissal::Empty,
                ..
            },
            _
        )]
    ));
    assert!(again.has(Command::Emit(ClientEvent::UserSpeaking {
        who: ANA,
        turn: TurnId(3),
        speaking: true,
    })));
    let open = session
        .kernel
        .view()
        .turns()
        .map(|open| (open.turn, open.keys))
        .collect::<Vec<_>>();
    assert_eq!(open, [(TurnId(2), true), (TurnId(3), false)]);
}

#[test]
fn keys_end_with_what_was_said_after_them_when_that_has_to_end() {
    let mut config = desk();
    config.timing.turn_max = Duration::from_secs(6);
    let mut session = Session::with(config);
    // Ana presses a key, and then speaks for as long as a turn may last,
    // pressing as she speaks: the run of keys is under way all the while.
    session.at(1_000, key(ANA, '4'));
    session.at(1_200, edge(ANA, VadEdge::Start, 1_200));
    assert_eq!(session.at(3_500, key(ANA, '0')).facts(), Vec::new());
    assert_eq!(session.at(6_000, key(ANA, '7')).facts(), Vec::new());
    session.at(7_000, edge(ANA, VadEdge::End, 7_000));
    let ran_out = session.at(7_200, Event::Timer(TimerId::TurnMax(TurnId(2))));
    assert_eq!(ran_out.facts(), Vec::new());
    // Her words are in, and wait for the keys she began before them.
    let said = session.at(7_300, words(ANA, 1_200, 7_000, "the number is"));
    assert_eq!(said.facts(), Vec::new());

    // She speaks anew. What she said has to enter the conversation now,
    // and her keys before it, though another could have followed them.
    let anew = session.at(7_500, edge(ANA, VadEdge::Start, 7_500));
    assert!(matches!(
        anew.told()[..],
        [
            (
                Fact::KeysCommitted {
                    turn: TurnId(1),
                    ..
                },
                "407"
            ),
            (
                Fact::TurnCommitted {
                    turn: TurnId(2),
                    ..
                },
                "the number is"
            ),
        ]
    ));
    assert!(anew.has(Command::SetTimer(TimerId::Keys(TurnId(1)), None)));
    assert!(anew.has(Command::Emit(ClientEvent::UserSpeaking {
        who: ANA,
        turn: TurnId(3),
        speaking: true,
    })));
    // The key after it begins a run.
    let next = session.at(7_600, key(ANA, '1'));
    assert!(waits(&next, TurnId(4), 7_600 + WAIT_MS));
}

#[test]
fn two_people_press_at_once() {
    let mut session = Session::new();
    session.control(ControlInput::Join { who: BEN });
    session.at(1_000, key(ANA, '4'));
    session.at(1_100, key(BEN, '9'));
    session.at(1_200, key(ANA, '0'));
    let ben = session.at(1_300, key(BEN, '#'));
    assert_eq!(keyed(&ben), [(TurnId(2), BEN, "9")]);
    let ana = session.at(1_400, key(ANA, '#'));
    assert_eq!(keyed(&ana), [(TurnId(1), ANA, "40")]);
}

// ---- Keys nobody takes, and keys that are left -----------------------------------

#[test]
fn keys_of_somebody_who_is_not_there_are_not_taken() {
    let mut session = Session::new();
    assert!(session.at(1_000, key(BEN, '4')).is_silent());
    assert_eq!(session.kernel.view().turns().count(), 0);
}

#[test]
fn keys_of_somebody_who_leaves_are_a_turn_as_they_are() {
    let mut session = Session::new();
    presses(&mut session, "40");
    let left = session.control(ControlInput::Leave { who: ANA });
    assert!(matches!(
        left.told()[..],
        [
            (
                Fact::KeysCommitted {
                    turn: TurnId(1),
                    ..
                },
                "40"
            ),
            (Fact::ParticipantLeft { who: ANA }, _),
        ]
    ));
}

#[test]
fn a_session_that_closes_takes_keys_until_it_stops_listening() {
    let mut session = Session::new();
    presses(&mut session, "40");
    let reason = CloseReason::Requested;
    let closing = session.control(ControlInput::Close { reason });
    // Keys that were not over are dismissed with the session, as speech
    // is.
    assert!(closing.told().iter().any(|(fact, _)| matches!(
        fact,
        Fact::TurnDismissed {
            turn: TurnId(1),
            reason: Dismissal::Closed,
            ..
        }
    )));
    assert_eq!(session.kernel.view().lifecycle(), Lifecycle::Closed);
    assert!(session.after(10, key(ANA, '7')).is_silent());
}

#[test]
fn what_is_not_about_keys_leaves_them_alone() {
    let mut session = Session::new();
    session.at(1_000, key(ANA, '4'));
    let turn = TurnId(1);
    // An estimate, a verdict and the timers of a turn of speech name the
    // turn of keys. None of them is about it.
    for event in [
        Event::Eou { turn, p: Prob::ONE },
        Event::Timer(TimerId::Eou(turn)),
        Event::Timer(TimerId::Overlap(turn)),
        Event::Timer(TimerId::TurnMax(turn)),
    ] {
        assert!(session.after(10, event.clone()).is_silent(), "{event:?}");
    }
    // Nor are words of hers that arrive now, which are a turn of their
    // own as words without a turn are.
    let late = session.after(10, words(ANA, 100, 600, "hello"));
    assert!(matches!(
        late.told()[..],
        [(
            Fact::TurnCommitted {
                turn: TurnId(2),
                ..
            },
            "hello"
        )]
    ));
    let over = session.after(250, key(ANA, '#'));
    assert_eq!(keyed(&over), [(TurnId(1), ANA, "4")]);
}

#[test]
fn a_key_for_which_no_turn_has_room_is_not_taken() {
    let limits = Limits {
        open_turns: 1,
        ..Limits::default()
    };
    let mut session = Session::with(KernelConfig { limits, ..desk() });
    session.control(ControlInput::Join { who: BEN });
    session.at(1_000, key(ANA, '4'));
    assert!(session.at(1_100, key(BEN, '9')).is_silent());
    let over = session.at(1_200, key(ANA, '#'));
    assert_eq!(keyed(&over), [(TurnId(1), ANA, "4")]);
}

#[test]
fn keys_are_in_a_session_that_is_rebuilt_from_its_journal() {
    let mut session = Session::new();
    presses(&mut session, "4071#");
    let rebuilt = session.rebuilt(&session.origin);
    assert_eq!(rebuilt, session.kernel.snapshot());
    let again = session.restart();
    assert_eq!(again.items(), ["1 pressed: 4071"]);
}
