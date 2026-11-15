# Waveport

Waveport is an open-source platform for real-time voice and multimodal AI agents, written in Rust. One runtime is meant to be both the agent framework and the system that operates it: it runs conversations, terminates media, dispatches work across a fleet and journals every session.

## Status

**Foundations only.** The repository holds the workspace, the quality gates and the test and benchmark harnesses. There is no product code yet: nothing here runs a voice session. The session kernel and the first voice loop are the next phase of the [roadmap](ROADMAP.md).

## Documents

| Document | Contents |
|---|---|
| [ROADMAP.md](ROADMAP.md) | What will be built, in which phases, and why |
| [docs/architecture.md](docs/architecture.md) | The design: layers, session kernel, ports, performance targets, tech stack |
| [docs/ecosystem.md](docs/ecosystem.md) | Everything the repository will contain, traced to the gap it closes |
| [docs/stability.md](docs/stability.md) | What each crate promises about its API |
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
| `crates/testing/waveport-alloc-counter` | Counting allocator, to prove a code path does not allocate |
| `crates/testing/waveport-testkit` | Shared test helpers |
| `benches/` | Wall-clock and instruction-count benchmarks |
| `xtask/` | The repository's gates, run as `cargo xtask <command>` |

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
