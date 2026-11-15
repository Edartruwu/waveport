# Waveport

Waveport is an open-source platform for real-time voice and multimodal AI agents, written in Rust. One runtime is meant to be both the agent framework and the system that operates it: it runs conversations, terminates media, dispatches work across a fleet and journals every session.

## Status

**Foundations, the first measurements, the session kernel and the audio path.** The repository holds the workspace, the quality gates, the test and benchmark harnesses, and the first product code. The session kernel decides who has the floor, what the agent says and how much of it was heard, which tool calls run and what became of each, and what a session is after its process died. The audio path turns the packets of a transport into audio in order and the audio of speech synthesis into frames handed over at the pace they are heard, and knows how much of a response has been heard. Both are libraries with no I/O. Nothing here runs a voice session yet.

Phase 1 of the [roadmap](ROADMAP.md) is under way. Its first step, a throwaway [density spike](docs/decisions/0001-density-spike.md), chose the inference backend and the allocator and measured what one voice session costs. The kernel is the second: it passes the six scenarios written before it, and its invariants are checked on random sessions and on every path through small ones. The audio path is the third: Opus and G.711, the change of rate, a reorder buffer, pacing and the playout ledger, with no allocation per frame. The ports, the runtime, providers and the first voice loop come next.

## Documents

| Document | Contents |
|---|---|
| [ROADMAP.md](ROADMAP.md) | What will be built, in which phases, and why |
| [docs/architecture.md](docs/architecture.md) | The design: layers, session kernel, ports, performance targets, tech stack |
| [docs/ecosystem.md](docs/ecosystem.md) | Everything the repository will contain, traced to the gap it closes |
| [docs/stability.md](docs/stability.md) | What each crate promises about its API |
| [docs/decisions/](docs/decisions/) | Decisions that rest on a measurement, with the numbers |
| [CONTRIBUTING.md](CONTRIBUTING.md) | The gates and the rules they enforce |

## Layout

Waveport follows a ports-and-adapters design with a domain-driven core. Dependencies point inward only, and a build gate enforces it.

| Path | Contents |
|---|---|
| `crates/core/waveport-domain` | Domain model and the session kernel. `no_std`: no sockets, clocks or threads |
| `crates/core/waveport-media` | The per-frame audio path: reordering, decoding, resampling, pacing |
| `crates/core/waveport-ports` | The traits adapters implement |
| `crates/core/waveport-runtime` | Application layer: runs sessions |
| `crates/core/waveport-control` | Application layer: runs the fleet |
| `crates/core/waveport-protocol` | Wire formats: client protocol, control API, journal records. Depends on no other crate |
| `crates/testing/waveport-sim` | Deterministic simulation: seeded runs with a virtual clock |
| `crates/testing/waveport-kernel-sim` | The session kernel under test: scenarios explored along every path, random sessions, every path through small sessions, and recorded sessions replayed, with the invariants checked at every step |
| `crates/testing/waveport-alloc-counter` | Counting allocator, to prove a code path does not allocate |
| `crates/testing/waveport-testkit` | Shared test helpers |
| `benches/` | Wall-clock and instruction-count benchmarks |
| `xtask/` | The repository's gates, run as `cargo xtask <command>` |
| `assets/` | The lock file that pins model files; `cargo xtask fetch-assets` downloads them |
| `perf/` | Recorded measurements |
| `spikes/` | Throwaway experiments, outside the workspace and its gates |

The core crates are documented skeletons today. Adapters (`crates/adapters/`), applications (`apps/`) and SDKs are added in the phases that need them.

## Building and checking

You need [rustup](https://rustup.rs/); the toolchain is pinned and installs itself.

```sh
cargo build --workspace
cargo xtask ci --fast    # format, policy, Clippy and tests
cargo xtask ci           # every gate
```

[CONTRIBUTING.md](CONTRIBUTING.md) lists the gates, the extra tools the slower ones need, and the rules for layers, lints, `unsafe` code, dependencies and pull-request titles.

## Licence

Licensed under the [Apache License, Version 2.0](LICENSE). Copyright 2026 The Waveport Authors.
