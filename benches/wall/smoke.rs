//! Wall-clock smoke benchmark.
//!
//! Run it with `cargo bench -p waveport-benches --bench wall_smoke`. CI runs
//! it with `-- --test`, which executes each benchmark once to prove it still
//! works without measuring anything.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

fn smoke(criterion: &mut Criterion) {
    criterion.bench_function("smoke/sim_rng_1024", |bencher| {
        bencher.iter(|| waveport_benches::smoke(black_box(42)));
    });
}

criterion_group!(benches, smoke);
criterion_main!(benches);
