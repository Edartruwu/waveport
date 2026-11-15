//! Instruction counts of the media path, one frame at a time.
//!
//! Run it with `cargo xtask bench-ir`, which needs Valgrind and
//! `gungraun-runner`. With `--base <rev>` the counts are compared with that
//! revision's and a growth of more than 2% fails.
//!
//! Each benchmark prepares a path in the middle of a stream, which is not
//! measured, and then measures the work of one frame: a packet taken in
//! and handed on, or a frame handed over and encoded. The cases are
//! described in `waveport_benches::media`. The counts for Opus include
//! libopus.
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
use waveport_benches::media::{Inbound, Outbound, Wire};

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::opus(args = (Wire::Opus), setup = Inbound::prepare)]
#[bench::g711(args = (Wire::G711), setup = Inbound::prepare)]
#[bench::pcm48(args = (Wire::Pcm48), setup = Inbound::prepare)]
#[bench::plain(args = (Wire::Plain), setup = Inbound::prepare)]
fn packet_in(mut inbound: Box<Inbound>) -> Box<Inbound> {
    black_box(black_box(&mut inbound).packet());
    // Returned, so that dropping the path is not measured.
    inbound
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::opus(args = (Wire::Opus), setup = Inbound::prepare)]
#[bench::g711(args = (Wire::G711), setup = Inbound::prepare)]
fn packet_in_after_a_loss(mut inbound: Box<Inbound>) -> Box<Inbound> {
    black_box(black_box(&mut inbound).packet_after_a_loss());
    inbound
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::opus(args = (Wire::Opus), setup = Outbound::prepare)]
#[bench::g711(args = (Wire::G711), setup = Outbound::prepare)]
#[bench::pcm48(args = (Wire::Pcm48), setup = Outbound::prepare)]
#[bench::plain(args = (Wire::Plain), setup = Outbound::prepare)]
fn frame_out(mut outbound: Box<Outbound>) -> Box<Outbound> {
    black_box(black_box(&mut outbound).frame());
    outbound
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::plain(args = (Wire::Plain), setup = Outbound::prepare)]
fn frame_out_lowered(mut outbound: Box<Outbound>) -> Box<Outbound> {
    black_box(black_box(&mut outbound).frame_lowered());
    outbound
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::plain(args = (Wire::Plain), setup = Outbound::prepare)]
fn position(mut outbound: Box<Outbound>) -> Box<Outbound> {
    black_box(black_box(&mut outbound).position());
    outbound
}

#[cfg(target_os = "linux")]
library_benchmark_group!(
    name = media_group,
    benchmarks = [
        packet_in,
        packet_in_after_a_loss,
        frame_out,
        frame_out_lowered,
        position
    ]
);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = media_group);

#[cfg(not(target_os = "linux"))]
fn main() {}
