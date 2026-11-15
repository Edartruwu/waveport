//! Instruction counts of single kernel steps.
//!
//! Run it with `cargo xtask bench-ir`, which needs Valgrind and
//! `gungraun-runner`. With `--base <rev>` the counts are compared with that
//! revision's and a growth of more than 2% fails.
//!
//! Each benchmark prepares a kernel in the middle of a conversation, which
//! is not measured, and then measures one step. The cases are described in
//! `waveport_benches::kernel`.
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
use waveport_benches::kernel::{Case, Prepared};

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::playout_progress(args = (Case::PlayoutProgress), setup = Case::prepare)]
#[bench::speech_edge(args = (Case::SpeechEdge), setup = Case::prepare)]
#[bench::duck(args = (Case::Duck), setup = Case::prepare)]
#[bench::interim_transcript(args = (Case::InterimTranscript), setup = Case::prepare)]
#[bench::stale_result(args = (Case::StaleResult), setup = Case::prepare)]
#[bench::delta_held(args = (Case::DeltaHeld), setup = Case::prepare)]
#[bench::delta_released(args = (Case::DeltaReleased), setup = Case::prepare)]
#[bench::barge_in(args = (Case::BargeIn), setup = Case::prepare)]
#[bench::turn_commit(args = (Case::TurnCommit), setup = Case::prepare)]
#[bench::stale_timer(args = (Case::StaleTimer), setup = Case::prepare)]
fn step(mut prepared: Prepared) -> Prepared {
    black_box(black_box(&mut prepared).step());
    // Returned, so that dropping the kernel is not measured.
    prepared
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = kernel_group, benchmarks = [step]);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = kernel_group);

#[cfg(not(target_os = "linux"))]
fn main() {}
