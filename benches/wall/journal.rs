//! How long the journal takes to store what sessions hand it.
//!
//! Run it with `cargo bench -p waveport-benches --bench wall_journal`. The
//! file is under the target directory, so the numbers are those of the
//! disk that directory is on: a commit is one sync of the file, or two in
//! two phases, and the sync is most of it. The same commits on a journal
//! in memory say what is left when the disk is taken away.
//!
//! Each measurement is a round in which every session hands over the
//! four records of a step and waits until they are stored, as a tool
//! with an effect makes its session wait. With one session that is the
//! wait itself. With 64 it is the same wait, shared: the journal stores
//! what arrives together in one commit.

use core::time::Duration;
use std::hint::black_box;
use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};
use waveport_benches::journal::Committing;

fn journal(criterion: &mut Criterion) {
    let folder = Path::new(env!("CARGO_TARGET_TMPDIR")).join("wall-journal");
    let mut group = criterion.benchmark_group("journal");
    group
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    let cases = [
        ("commit/1_session", 1, false),
        ("commit/64_sessions", 64, false),
        ("commit_in_two_phases/1_session", 1, true),
    ];
    for (name, sessions, two_phase) in cases {
        let path = folder.join(format!("{sessions}-{two_phase}.redb"));
        let mut committing = Committing::in_file(&path, sessions, two_phase);
        group.bench_function(name, |bencher| {
            bencher.iter(|| black_box(committing.round()));
        });
        drop(committing);
        let _ = std::fs::remove_file(&path);
    }
    for sessions in [1, 64] {
        let mut committing = Committing::in_memory(sessions);
        let name = if sessions == 1 {
            "commit_in_memory/1_session".to_owned()
        } else {
            format!("commit_in_memory/{sessions}_sessions")
        };
        group.bench_function(&name, |bencher| {
            bencher.iter(|| black_box(committing.round()));
        });
    }
    group.finish();
}

criterion_group!(benches, journal);
criterion_main!(benches);
