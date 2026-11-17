//! Instruction counts of a WebRTC call, one packet at a time.
//!
//! Run it with `cargo xtask bench-ir`, which needs Valgrind and
//! `gungraun-runner`. With `--base <rev>` the counts are compared with that
//! revision's and a growth of more than 2% fails.
//!
//! Each benchmark prepares a call in the middle of itself, which is not
//! measured, and then measures the work of one packet: a datagram taken
//! in, opened and handed on as audio, or a frame sealed and made a
//! datagram. The cases are described in `waveport_benches::webrtc`. The
//! counts include `str0m` and the ciphers, and not a codec: a transport
//! carries packets and does not look into them.
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
use waveport_benches::webrtc::{Inbound, Outbound};
#[cfg(target_os = "linux")]
use waveport_transport_webrtc::Like;

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::redundant(args = (Like::Chrome), setup = Inbound::prepare)]
#[bench::single(args = (Like::Firefox), setup = Inbound::prepare)]
fn packet_in(mut inbound: Box<Inbound>) -> Box<Inbound> {
    black_box(black_box(&mut inbound).packet());
    // Returned, so that dropping the call is not measured.
    inbound
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::redundant(args = (Like::Chrome), setup = Outbound::prepare)]
#[bench::single(args = (Like::Firefox), setup = Outbound::prepare)]
fn frame_out(mut outbound: Box<Outbound>) -> Box<Outbound> {
    black_box(black_box(&mut outbound).frame());
    outbound
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = webrtc_group, benchmarks = [packet_in, frame_out]);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = webrtc_group);

#[cfg(not(target_os = "linux"))]
fn main() {}
