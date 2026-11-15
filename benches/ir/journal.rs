//! Instruction counts of handing records to the journal.
//!
//! Run it with `cargo xtask bench-ir`, which needs Valgrind and
//! `gungraun-runner`. With `--base <rev>` the counts are compared with that
//! revision's and a growth of more than 2% fails.
//!
//! A session hands its journal the records of a step on its own thread,
//! between two frames of audio, and what that costs it is measured here:
//! the request made and put where the thread that writes the journal
//! finds it. That thread is held in a commit while it is measured, as it
//! often is when a session writes, so that nothing it does is counted.
//! Storing the records is not this benchmark's: it is mostly the disk's
//! doing, and `wall_journal` measures it by the clock.
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
use waveport_benches::journal::Enqueue;

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::four_records(setup = Enqueue::prepare)]
fn enqueue(mut enqueue: Box<Enqueue>) -> Box<Enqueue> {
    black_box(&mut enqueue).hand_over();
    // Returned, so that letting the commit go on and ending the journal
    // are not measured.
    enqueue
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = journal; benchmarks = enqueue);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = journal);

#[cfg(not(target_os = "linux"))]
fn main() {}
