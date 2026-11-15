//! Instruction-count smoke benchmark.
//!
//! Run it with `cargo xtask bench-ir`, which needs Valgrind and
//! `gungraun-runner`. With `--base <rev>` the counts are compared with that
//! revision's and a growth of more than 2% fails.
//!
//! Valgrind is used on Linux only; elsewhere this benchmark does nothing.

#![cfg_attr(
    target_os = "linux",
    expect(
        missing_docs,
        reason = "the benchmark macros generate public items without documentation"
    )
)]

#[cfg(target_os = "linux")]
use std::hint::black_box;

#[cfg(target_os = "linux")]
use gungraun::prelude::*;

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::seed_42(42)]
fn smoke(seed: u64) -> u64 {
    black_box(waveport_benches::smoke(black_box(seed)))
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = smoke_group, benchmarks = [smoke]);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = smoke_group);

#[cfg(not(target_os = "linux"))]
fn main() {}
