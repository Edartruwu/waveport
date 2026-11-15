//! Instruction counts of what a record costs the session that makes it.
//!
//! Run it with `cargo xtask bench-ir`, which needs Valgrind and
//! `gungraun-runner`. With `--base <rev>` the counts are compared with that
//! revision's and a growth of more than 2% fails.
//!
//! A session writes its facts as records and hands them to its journal
//! on its own thread, between two frames of audio. Both are measured
//! here: a fact written as a record, and the records of a step made
//! into a request and put where the thread that writes the journal
//! finds it. That thread is held in a commit while the second is
//! measured, as it often is when a session writes, so that nothing it
//! does is counted. Storing the records is not this benchmark's: it is
//! mostly the disk's doing, and `wall_journal` measures it by the clock.
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
use waveport_benches::journal::{Enqueue, Written};

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::a_turn(setup = Written::prepare)]
fn write(written: Box<Written>) -> (Box<Written>, Option<bytes::Bytes>) {
    let record = black_box(&written).write();
    // Returned, so that letting go of them is not measured.
    (written, record)
}

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
library_benchmark_group!(name = journal; benchmarks = write, enqueue);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = journal);

#[cfg(not(target_os = "linux"))]
fn main() {}
