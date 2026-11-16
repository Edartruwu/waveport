# Waveport

Waveport is an open-source platform for real-time voice and multimodal AI agents, written in Rust. One runtime is meant to be both the agent framework and the system that operates it: it runs conversations, terminates media, dispatches work across a fleet and journals every session.

## Status

**Foundations, the first measurements, the session kernel, the audio path, the ports and the runtime.** The repository holds the workspace, the quality gates, the test and benchmark harnesses, and the first product code. The session kernel decides who has the floor, what the agent says and how much of it was heard, which tool calls run and what became of each, and what a session is after its process died. The audio path turns the packets of a transport into audio in order and the audio of speech synthesis into frames handed over at the pace they are heard, and knows how much of a response has been heard. Both are libraries with no I/O. The ports are the traits that a provider, a transport, a store or a tool implements, each with a statement of what it can do, and a planner that decides from those statements how a session is run and names what it will do less well. The runtime runs sessions: each is one task on a thread it never leaves, which drives the kernel and the audio path through the ports, keeps a journal and goes on from it after its process has died, while the models that listen are shared by all sessions. No provider, microphone or disk is behind the ports yet: in the tests, fakes of every port hold whole conversations with the real kernel, audio path and runtime.

Phase 1 of the [roadmap](ROADMAP.md) is under way. Its first step, a throwaway [density spike](docs/decisions/0001-density-spike.md), chose the inference backend and the allocator and measured what one voice session costs. The kernel is the second: it passes the six scenarios written before it, and its invariants are checked on random sessions and on every path through small ones. The audio path is the third: Opus and G.711, the change of rate, a reorder buffer, pacing and the playout ledger, with no allocation per frame. The ports are the fourth, with the plumbing adapters share: a stream helper, WebSocket and HTTP clients and a parser for server-sent events. The runtime is the fifth: shards, the session actor and the compute plane, tried every night on ten thousand sessions made up from seeds, with parts that fail, lines that lose packets and processes that die. The local models, the providers and the first voice loop come next.

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
| `crates/core/waveport-ports` | The traits adapters implement, what each can say it does, and the session planner |
| `crates/core/waveport-adapter-kit` | What adapters share: a stream helper, WebSocket and HTTP clients, server-sent events, retry classes |
| `crates/core/waveport-runtime` | Application layer: runs sessions. Shards, the session actor, and the compute plane that shares the listening models among sessions |
| `crates/core/waveport-control` | Application layer: runs the fleet |
| `crates/core/waveport-protocol` | Wire formats: client protocol, control API, journal records. Depends on no other crate |
| `crates/testing/waveport-sim` | Deterministic simulation: seeded runs with a virtual clock |
| `crates/testing/waveport-kernel-sim` | The session kernel under test: scenarios explored along every path, random sessions, every path through small sessions, and recorded sessions replayed, with the invariants checked at every step |
| `crates/testing/waveport-alloc-counter` | Counting allocator, to prove a code path does not allocate |
| `crates/testing/waveport-testkit` | Fakes of every port, and the checks every implementation of a port has to pass |
| `benches/` | Wall-clock and instruction-count benchmarks |
| `xtask/` | The repository's gates, run as `cargo xtask <command>` |
| `assets/` | The lock file that pins model files; `cargo xtask fetch-assets` downloads them |
| `perf/` | Recorded measurements |
| `spikes/` | Throwaway experiments, outside the workspace and its gates |

`waveport-control` is a documented skeleton today. Adapters (`crates/adapters/`), applications (`apps/`) and SDKs are added in the phases that need them.

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
