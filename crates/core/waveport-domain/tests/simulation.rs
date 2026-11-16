//! Random sessions.
//!
//! The scenarios in `scenarios/` tell six stories along every path. Here
//! a seed makes up the story: who is in the session, when they speak, what
//! the language model writes, and how late, how often and whether at all
//! each adapter answers. Every invariant is checked after every step, and
//! at the end of each session that nothing is left unfinished. Each
//! session runs twice, to see that the kernel does the same both times.
//!
//! A failure prints its seed and the last steps of the session.
//! `WAVEPORT_SIM_SEED=<seed>` runs that session alone, and
//! `WAVEPORT_SIM_SEEDS=<count>` runs more sessions than the default.

use waveport_kernel_sim::check::Report;
use waveport_kernel_sim::seeded::{Profile, Tally, sweep};

/// Sessions per profile in an ordinary test run: 3,000 in all.
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
    Ok(())
}
