//! Random sessions.
//!
//! The scenarios in `scenarios/` tell six stories along every path. Here
//! a seed makes up the story: who is in the session, when they speak, what
//! the language model writes and which tools it calls, which agent has the
//! floor, how late, how often and whether at all each adapter answers, and
//! when the process dies. Every invariant is checked after every step, and
//! at the end of each session that nothing is left unfinished and that the
//! journal alone adds up to what the kernel holds. Each session runs
//! twice, to see that the kernel does the same both times.
//!
//! A failure prints its seed and the last steps of the session.
//! `WAVEPORT_SIM_SEED=<seed>` runs that session alone, and
//! `WAVEPORT_SIM_SEEDS=<count>` runs more sessions than the default.

use waveport_kernel_sim::check::Report;
use waveport_kernel_sim::seeded::{Profile, Tally, sweep};

/// Sessions per profile in an ordinary test run: 4,800 in all.
const SEEDS: u64 = 600;

/// Runs the profile's sessions. Returns what they did when at least the
/// default number ran, so that a test can see that its sweep is not empty:
/// one that never interrupts anybody proves nothing about interruptions.
fn swept(profile: Profile) -> Result<Option<Tally>, Report> {
    let tally = sweep(profile, SEEDS)?;
    Ok((tally.runs >= SEEDS).then_some(tally))
}

#[test]
fn calm() -> Result<(), Report> {
    let Some(tally) = swept(Profile::Calm)? else {
        return Ok(());
    };
    assert!(tally.turns >= 2_000, "{tally:?}");
    assert!(tally.replies >= 1_500, "{tally:?}");
    assert!(tally.spoken >= 800, "{tally:?}");
    assert!(tally.cut >= 300, "{tally:?}");
    assert!(tally.barge_ins >= 200, "{tally:?}");
    assert!(tally.asides >= 200, "{tally:?}");
    assert!(tally.superseded >= 300, "{tally:?}");
    assert!(tally.pauses >= 100, "{tally:?}");
    assert!(tally.closed >= 100, "{tally:?}");
    // Tool calls, in each of the ways they end, and what follows them.
    assert!(tally.calls >= 500, "{tally:?}");
    assert!(tally.sent >= 350, "{tally:?}");
    assert!(tally.completed >= 280, "{tally:?}");
    assert!(tally.withdrawn >= 60, "{tally:?}");
    assert!(tally.in_doubt >= 25, "{tally:?}");
    assert!(tally.continuations >= 250, "{tally:?}");
    assert!(tally.delegated >= 50, "{tally:?}");
    assert!(tally.handoffs >= 50, "{tally:?}");
    assert!(tally.checks >= 600, "{tally:?}");
    Ok(())
}

#[test]
fn chaos() -> Result<(), Report> {
    let Some(tally) = swept(Profile::Chaos)? else {
        return Ok(());
    };
    assert!(tally.turns >= 2_000, "{tally:?}");
    assert!(tally.spoken >= 500, "{tally:?}");
    assert!(tally.cut >= 300, "{tally:?}");
    assert!(tally.failures >= 100, "{tally:?}");
    assert!(tally.empty >= 100, "{tally:?}");
    assert!(tally.calls >= 400, "{tally:?}");
    assert!(tally.completed >= 200, "{tally:?}");
    assert!(tally.failed >= 120, "{tally:?}");
    assert!(tally.handoffs >= 80, "{tally:?}");
    Ok(())
}

#[test]
fn stalls() -> Result<(), Report> {
    let Some(tally) = swept(Profile::Stalls)? else {
        return Ok(());
    };
    assert!(tally.turns >= 1_500, "{tally:?}");
    assert!(tally.failures >= 250, "{tally:?}");
    assert!(tally.empty >= 300, "{tally:?}");
    assert!(tally.spoken >= 200, "{tally:?}");
    // Tools that never answer, and records that are never stored.
    assert!(tally.calls >= 300, "{tally:?}");
    assert!(tally.failed >= 110, "{tally:?}");
    assert!(tally.withdrawn >= 60, "{tally:?}");
    assert!(tally.in_doubt >= 20, "{tally:?}");
    Ok(())
}

#[test]
fn duplicates() -> Result<(), Report> {
    let Some(tally) = swept(Profile::Duplicates)? else {
        return Ok(());
    };
    assert!(tally.turns >= 2_500, "{tally:?}");
    assert!(tally.spoken >= 800, "{tally:?}");
    assert!(tally.cut >= 300, "{tally:?}");
    assert!(tally.barge_ins >= 200, "{tally:?}");
    // A result that arrives twice counts once.
    assert!(tally.calls >= 600, "{tally:?}");
    assert!(tally.completed >= 300, "{tally:?}");
    assert!(tally.continuations >= 240, "{tally:?}");
    Ok(())
}

#[test]
fn shutdown() -> Result<(), Report> {
    let Some(tally) = swept(Profile::Shutdown)? else {
        return Ok(());
    };
    assert_eq!(
        tally.closed, tally.runs,
        "every session was asked to close: {tally:?}"
    );
    assert!(tally.turns >= 500, "{tally:?}");
    assert!(tally.cut >= 100, "{tally:?}");
    // Sessions that are closed with calls under way.
    assert!(tally.calls >= 100, "{tally:?}");
    assert!(tally.withdrawn >= 25, "{tally:?}");
    assert!(tally.in_doubt >= 1, "{tally:?}");
    Ok(())
}

#[test]
fn crashes() -> Result<(), Report> {
    let Some(tally) = swept(Profile::Crashes)? else {
        return Ok(());
    };
    // Every session dies at least once, and goes on.
    assert!(tally.crashes >= tally.runs, "{tally:?}");
    assert!(tally.turns >= 1_500, "{tally:?}");
    assert!(tally.spoken >= 600, "{tally:?}");
    assert!(tally.calls >= 500, "{tally:?}");
    assert!(tally.completed >= 250, "{tally:?}");
    // Calls that were under way when the process died: sent again where
    // that does no harm, and otherwise left in doubt.
    assert!(tally.resent >= 15, "{tally:?}");
    assert!(tally.in_doubt >= 20, "{tally:?}");
    assert!(tally.continuations >= 280, "{tally:?}");
    // The journal is checked before each death, and now and then.
    assert!(tally.checks >= 2_000, "{tally:?}");
    Ok(())
}

#[test]
fn lines() -> Result<(), Report> {
    let Some(tally) = swept(Profile::Lines)? else {
        return Ok(());
    };
    // Every session loses its line at least once. Most get it back, and
    // then say what the drop cut off or what waited for the line.
    assert!(tally.drops >= tally.runs, "{tally:?}");
    assert!(tally.returns >= 300, "{tally:?}");
    assert!(tally.resumed >= 100, "{tally:?}");
    // And some wait in vain, or do not wait at all.
    assert!(tally.abandoned >= 100, "{tally:?}");
    assert!(tally.closed >= tally.abandoned, "{tally:?}");
    assert!(tally.turns >= 1_000, "{tally:?}");
    assert!(tally.cut >= 150, "{tally:?}");
    // Tool calls that go on while the line is gone, and what follows them.
    assert!(tally.calls >= 200, "{tally:?}");
    assert!(tally.completed >= 100, "{tally:?}");
    assert!(tally.continuations >= 80, "{tally:?}");
    Ok(())
}

#[test]
fn keys() -> Result<(), Report> {
    let Some(tally) = swept(Profile::Keys)? else {
        return Ok(());
    };
    // Several keys to a run, and many runs to a session: with and without
    // the key that ends one, of a number of keys, or ended by the time in
    // which no key followed.
    assert!(tally.keys >= 6_000, "{tally:?}");
    assert!(tally.keyed >= 2_500, "{tally:?}");
    assert!(tally.keys >= 2 * tally.keyed, "{tally:?}");
    // Keys among speech, and over the agent, which every one of them
    // stops.
    assert!(tally.turns >= 2_500, "{tally:?}");
    assert!(tally.barge_ins >= 800, "{tally:?}");
    assert!(tally.cut >= 800, "{tally:?}");
    // What the keys ask for is answered, in words and with tools.
    assert!(tally.replies >= 4_000, "{tally:?}");
    assert!(tally.spoken >= 800, "{tally:?}");
    assert!(tally.calls >= 1_000, "{tally:?}");
    assert!(tally.continuations >= 400, "{tally:?}");
    assert!(tally.closed >= 100, "{tally:?}");
    Ok(())
}

#[test]
#[ignore = "prints what the sweeps cover"]
fn coverage() -> Result<(), Report> {
    for profile in Profile::ALL {
        let tally = sweep(profile, SEEDS)?;
        println!("{}: {tally:#?}", profile.name());
    }
    Ok(())
}
