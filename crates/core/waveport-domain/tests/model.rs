//! Every path through a small session.
//!
//! The simulation in `simulation.rs` tries sessions picked at random. The
//! tests here take sessions so small that every order in which their
//! events can arrive can be tried, and try them all, with every invariant
//! checked after every step. Each test is one such session: what may
//! happen in it is in its [`Scope`], and nothing in it is left to chance.
//!
//! A session with everything in it would be far too large, so each test
//! leaves out what another one has: one has two people and no tools,
//! another one tool call and a process that dies. Each also checks that
//! its paths get to the states it is there for, so that a scope which
//! stops reaching them cannot pass for want of trying.
//!
//! The tests that take longer are ignored in an ordinary run. The nightly
//! run has them: `cargo test --release -p waveport-domain --test model --
//! --include-ignored`.

use std::cell::Cell;

use waveport_domain::config::Effect;
use waveport_domain::fact::{Lifecycle, NoticeKind, ToolOutcome};
use waveport_domain::kernel::{ItemView, View};
use waveport_kernel_sim::check::Report;
use waveport_kernel_sim::model::{Explored, Scope, explore_where};

/// One person, one turn, and nothing else that can happen.
fn one_turn() -> Scope {
    Scope {
        people: 1,
        tools: Vec::new(),
        delegate: false,
        turns: 1,
        calls: 0,
        failures: 0,
        timers: 0,
        interrupts: 0,
        says: 0,
        closes: 0,
        drops: 0,
        crashes: 0,
        late: 0,
        most_states: 4_000_000,
    }
}

/// How many states had each of the things a test may be there for.
#[derive(Debug, Default)]
struct Seen {
    /// Speech that was heard to its end.
    whole: Cell<u64>,
    /// Speech that was cut off.
    cut: Cell<u64>,
    /// Two turns in the conversation with no speech between them.
    merged: Cell<u64>,
    completed: Cell<u64>,
    failed: Cell<u64>,
    withdrawn: Cell<u64>,
    in_doubt: Cell<u64>,
    /// A note that the session was restored.
    restored: Cell<u64>,
    closed: Cell<u64>,
    /// Two responses under way at once.
    together: Cell<u64>,
    /// A session that waits for its line with a reply owed.
    waiting: Cell<u64>,
    /// A session that closed with its line gone.
    abandoned: Cell<u64>,
}

impl Seen {
    fn look(&self, view: View<'_>) {
        let count = |cell: &Cell<u64>, holds: bool| cell.set(cell.get() + u64::from(holds));
        let spoke = |cut: bool| {
            view.items().any(
                |item| matches!(item, ItemView::Utterance { truncated, .. } if truncated == cut),
            )
        };
        let result = |wanted: ToolOutcome| {
            view.items().any(
                |item| matches!(item, ItemView::ToolResult { outcome, .. } if outcome == wanted),
            )
        };
        count(&self.whole, spoke(false));
        count(&self.cut, spoke(true));
        let turns = view
            .items()
            .map(|item| matches!(item, ItemView::UserTurn { .. }))
            .collect::<Vec<_>>();
        count(
            &self.merged,
            turns.windows(2).any(|pair| pair == [true, true]),
        );
        count(&self.completed, result(ToolOutcome::Completed));
        count(&self.failed, result(ToolOutcome::Failed));
        count(&self.withdrawn, result(ToolOutcome::Cancelled));
        count(&self.in_doubt, result(ToolOutcome::InDoubt));
        let restored = view.items().any(|item| {
            let kind = NoticeKind::Recovered;
            matches!(item, ItemView::Notice { kind: of, .. } if of == kind)
        });
        count(&self.restored, restored);
        count(&self.closed, view.lifecycle() == Lifecycle::Closed);
        count(&self.together, view.generations().count() >= 2);
        let gone = !view.is_attached();
        let running = view.lifecycle() == Lifecycle::Running;
        count(&self.waiting, gone && running && view.reply_owed());
        count(
            &self.abandoned,
            gone && view.lifecycle() == Lifecycle::Closed,
        );
    }
}

/// Tries every path through `scope`, and tells what the paths got to.
fn tried(scope: &Scope) -> Result<(Explored, Seen), Report> {
    let seen = Seen::default();
    let explored = explore_where(scope, |view| {
        seen.look(view);
        Ok(())
    })?;
    // Every path came to an end, and most of them to rest.
    assert!(explored.ends > 0 && explored.rests > 0, "{explored:?}");
    Ok((explored, seen))
}

#[test]
fn a_turn_a_tool_call_an_interruption_and_a_close() -> Result<(), Report> {
    let (explored, seen) = tried(&Scope::small())?;
    assert!(explored.states >= 40_000, "{explored:?}");
    // The call ends in each of the four ways: it fails when the record
    // that cleared it is never stored.
    for reached in [
        &seen.whole,
        &seen.cut,
        &seen.completed,
        &seen.failed,
        &seen.withdrawn,
        &seen.in_doubt,
        &seen.closed,
    ] {
        assert!(reached.get() > 0, "{seen:?}");
    }
    assert_eq!(seen.restored.get(), 0, "{seen:?}");
    Ok(())
}

#[test]
fn two_turns_of_one_person() -> Result<(), Report> {
    let scope = Scope {
        turns: 2,
        timers: 2,
        interrupts: 1,
        ..one_turn()
    };
    let (explored, seen) = tried(&scope)?;
    assert!(explored.states >= 20_000, "{explored:?}");
    // The second turn comes before the answer to the first is heard, in
    // the middle of it, or after it.
    assert!(seen.merged.get() > 0, "{seen:?}");
    assert!(seen.cut.get() > 0 && seen.whole.get() > 0, "{seen:?}");
    Ok(())
}

#[test]
fn two_people_speaking() -> Result<(), Report> {
    let scope = Scope {
        people: 2,
        turns: 2,
        timers: 1,
        ..one_turn()
    };
    let (explored, seen) = tried(&scope)?;
    assert!(explored.states >= 10_000, "{explored:?}");
    assert!(seen.merged.get() > 0 && seen.cut.get() > 0, "{seen:?}");
    Ok(())
}

#[test]
fn two_calls_in_one_batch() -> Result<(), Report> {
    let scope = Scope {
        tools: vec![Effect::Pure, Effect::SideEffecting],
        calls: 2,
        timers: 1,
        ..one_turn()
    };
    let (explored, seen) = tried(&scope)?;
    assert!(explored.states >= 20_000, "{explored:?}");
    // The answer to the calls is written while nothing else is: there is
    // one continuation, and it waits for the response that asked.
    assert!(seen.completed.get() > 0 && seen.whole.get() > 0, "{seen:?}");
    assert_eq!(seen.together.get(), 0, "{seen:?}");
    Ok(())
}

#[test]
fn a_call_and_a_process_that_dies() -> Result<(), Report> {
    for effect in [Effect::Pure, Effect::Idempotent, Effect::SideEffecting] {
        let scope = Scope {
            tools: vec![effect],
            calls: 1,
            timers: 1,
            crashes: 1,
            ..one_turn()
        };
        let (explored, seen) = tried(&scope)?;
        assert!(explored.states >= 3_000, "{effect:?}: {explored:?}");
        assert!(seen.restored.get() > 0, "{effect:?}: {seen:?}");
        assert!(seen.completed.get() > 0, "{effect:?}: {seen:?}");
        // Only a call that must happen at most once is left in doubt by a
        // restart: the others are sent again.
        let doubted = seen.in_doubt.get() > 0;
        assert_eq!(
            doubted,
            effect == Effect::SideEffecting,
            "{effect:?}: {seen:?}"
        );
    }
    Ok(())
}

#[test]
fn a_call_another_agent_answers() -> Result<(), Report> {
    let scope = Scope {
        delegate: true,
        calls: 1,
        timers: 1,
        interrupts: 1,
        ..one_turn()
    };
    let (explored, seen) = tried(&scope)?;
    assert!(explored.states >= 3_000, "{explored:?}");
    // The agent that works on the call does so beside the one that
    // speaks, and an interruption of that one leaves the call alone.
    assert!(seen.together.get() > 0, "{seen:?}");
    assert!(seen.completed.get() > 0 && seen.cut.get() > 0, "{seen:?}");
    Ok(())
}

#[test]
fn parts_that_fail() -> Result<(), Report> {
    let scope = Scope {
        tools: vec![Effect::SideEffecting],
        calls: 1,
        failures: 1,
        timers: 2,
        ..one_turn()
    };
    let (explored, seen) = tried(&scope)?;
    assert!(explored.states >= 15_000, "{explored:?}");
    assert!(
        seen.failed.get() > 0 && seen.completed.get() > 0,
        "{seen:?}"
    );
    Ok(())
}

#[test]
fn words_to_say_and_a_model_that_does_not_stop() -> Result<(), Report> {
    let scope = Scope {
        tools: vec![Effect::Pure],
        calls: 1,
        says: 1,
        late: 1,
        timers: 1,
        ..one_turn()
    };
    let (explored, seen) = tried(&scope)?;
    assert!(explored.states >= 10_000, "{explored:?}");
    assert!(seen.whole.get() > 0 && seen.cut.get() > 0, "{seen:?}");
    Ok(())
}

#[test]
fn a_line_that_drops() -> Result<(), Report> {
    let scope = Scope {
        drops: 1,
        timers: 2,
        ..one_turn()
    };
    let (explored, seen) = tried(&scope)?;
    assert!(explored.states >= 1_500, "{explored:?}");
    // The line drops before the turn, in the middle of it, while its
    // reply is written and while it is heard. It comes back, or the
    // session gives it up.
    assert!(seen.waiting.get() > 0, "{seen:?}");
    assert!(seen.abandoned.get() > 0, "{seen:?}");
    assert!(seen.cut.get() > 0 && seen.whole.get() > 0, "{seen:?}");
    Ok(())
}

#[test]
fn a_call_and_a_line_that_drops() -> Result<(), Report> {
    for effect in [Effect::Pure, Effect::SideEffecting] {
        let scope = Scope {
            tools: vec![effect],
            calls: 1,
            drops: 1,
            timers: 1,
            ..one_turn()
        };
        let (explored, seen) = tried(&scope)?;
        assert!(explored.states >= 5_000, "{effect:?}: {explored:?}");
        // The call goes on while the line is gone, and what it found is
        // said when the line is back.
        assert!(seen.completed.get() > 0, "{effect:?}: {seen:?}");
        assert!(seen.waiting.get() > 0, "{effect:?}: {seen:?}");
        assert!(seen.whole.get() > 0, "{effect:?}: {seen:?}");
    }
    Ok(())
}

/// The sessions of the nightly run: what the tests above have, with more
/// of it at once.
#[expect(clippy::too_many_lines, reason = "a list of sessions")]
fn larger() -> Vec<(&'static str, Scope, u64)> {
    let pure = vec![Effect::Pure];
    let once = vec![Effect::SideEffecting];
    let both = vec![Effect::Pure, Effect::SideEffecting];
    vec![
        (
            "a line that drops twice, an interruption and a close",
            Scope {
                drops: 2,
                timers: 2,
                interrupts: 1,
                closes: 1,
                ..one_turn()
            },
            40_000,
        ),
        (
            "two turns, a call and a line that drops",
            Scope {
                turns: 2,
                tools: pure.clone(),
                calls: 1,
                drops: 1,
                timers: 2,
                ..one_turn()
            },
            2_500_000,
        ),
        (
            "a call, a line that drops and a process that dies",
            Scope {
                tools: once.clone(),
                calls: 1,
                drops: 1,
                crashes: 1,
                timers: 1,
                ..one_turn()
            },
            40_000,
        ),
        (
            "a call, an interruption, a close and a process that dies",
            Scope {
                tools: once.clone(),
                calls: 1,
                timers: 2,
                interrupts: 1,
                closes: 1,
                crashes: 1,
                ..one_turn()
            },
            250_000,
        ),
        (
            "two people and an interruption",
            Scope {
                people: 2,
                turns: 2,
                timers: 2,
                interrupts: 1,
                ..one_turn()
            },
            150_000,
        ),
        (
            "three turns of two people",
            Scope {
                people: 2,
                turns: 3,
                timers: 1,
                ..one_turn()
            },
            800_000,
        ),
        (
            "two turns, a call and an interruption",
            Scope {
                turns: 2,
                tools: pure,
                calls: 1,
                timers: 1,
                interrupts: 1,
                ..one_turn()
            },
            120_000,
        ),
        (
            "two turns, a call and a close",
            Scope {
                turns: 2,
                tools: once.clone(),
                calls: 1,
                timers: 1,
                closes: 1,
                ..one_turn()
            },
            120_000,
        ),
        (
            "two turns, words to say and an interruption",
            Scope {
                turns: 2,
                says: 1,
                interrupts: 1,
                timers: 2,
                ..one_turn()
            },
            600_000,
        ),
        (
            "two tools, two calls, an interruption and a close",
            Scope {
                tools: both,
                calls: 2,
                timers: 2,
                interrupts: 1,
                closes: 1,
                ..one_turn()
            },
            2_000_000,
        ),
        (
            "a call another agent answers, one a tool does, and a close",
            Scope {
                delegate: true,
                tools: once.clone(),
                calls: 2,
                timers: 1,
                interrupts: 1,
                closes: 1,
                ..one_turn()
            },
            500_000,
        ),
        (
            "two parts that fail, and a close",
            Scope {
                tools: once.clone(),
                calls: 1,
                failures: 2,
                timers: 2,
                closes: 1,
                ..one_turn()
            },
            80_000,
        ),
        (
            "a call, a close and four timers",
            Scope {
                tools: once,
                calls: 1,
                closes: 1,
                timers: 4,
                ..one_turn()
            },
            120_000,
        ),
    ]
}

#[test]
#[ignore = "millions of states: the nightly run has it, in a release build"]
fn larger_sessions() -> Result<(), Report> {
    let mut states = 0;
    for (name, scope, least) in larger() {
        let (explored, _) = tried(&scope)?;
        println!("{name}: {explored:?}");
        assert!(explored.states >= least, "{name}: {explored:?}");
        states += explored.states;
    }
    assert!(states >= 8_000_000, "{states}");
    Ok(())
}
