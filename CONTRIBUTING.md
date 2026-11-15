# Contributing to Waveport

This guide covers how the repository is checked and the rules those checks enforce. For what is being built and why, see the [roadmap](ROADMAP.md) and the [architecture spec](docs/architecture.md).

## Setup

Install [rustup](https://rustup.rs/). The toolchain (Rust 1.100.0 with rustfmt and Clippy) is pinned in `rust-toolchain.toml` and is installed the first time you run `cargo` in the repository.

Building also needs [CMake](https://cmake.org/) and a C compiler: `waveport-media` uses libopus, which is built from its source and linked in. Nothing has to be installed for it beyond those two.

The slower gates use extra tools. `cargo xtask ci` skips a gate whose tool is missing and prints the command that installs it:

```sh
cargo install --locked cargo-nextest --version 0.9.152
cargo install --locked cargo-hack --version 0.6.46
cargo install --locked cargo-deny --version 0.20.2
cargo install --locked cargo-semver-checks --version 0.51.0
rustup toolchain install 1.98 --profile minimal
rustup toolchain install nightly-2026-11-14 --profile minimal --component miri --component rust-src
```

The instruction-count benchmarks also need Valgrind and `cargo install --locked gungraun-runner --version 0.20.0`. They run on Linux only.

### Model files

Model weights are not committed. `assets/assets.lock.toml` pins each file to an address that never changes what it serves, a SHA-256 and a licence:

```sh
cargo xtask fetch-assets           # download into target/assets/ and verify
cargo xtask fetch-assets --check   # verify only; fails if a file is missing
```

A file whose digest does not match is deleted, never left in place. Adding a model is a reviewed change to the lock file.

## Running the gates

```sh
cargo xtask ci           # every gate, cheapest first
cargo xtask ci --fast    # format, policy, Clippy, tests and doctests
cargo xtask ci --only clippy
cargo xtask ci --keep-going   # do not stop at the first failure
cargo xtask ci --strict       # a missing tool is a failure, not a skip
```

CI runs the same commands; a test in `xtask/src/ci.rs` fails if the workflows and the local runner drift apart. Warnings are errors in both.

| Gate | Command | What it enforces |
|---|---|---|
| `fmt` | `cargo fmt --all --check` | Formatting |
| `policy` | `cargo xtask arch-check`, `tier-check`, `lint-sync` | The layer rules, the [stability tiers](docs/stability.md) and lint configuration that has not drifted |
| `clippy` | `cargo clippy --workspace --all-targets --all-features --locked` | The workspace lint table |
| `test` | `cargo nextest run --workspace --all-features --locked --profile ci` | Unit and integration tests |
| `doctest` | `cargo test --workspace --doc --all-features --locked` | Examples in documentation |
| `docs` | `cargo doc --workspace --no-deps --all-features --locked` | Documentation builds without warnings |
| `no-std` | `cargo check -p waveport-domain --target x86_64-unknown-none --locked` | The domain builds without an operating system |
| `features` | `cargo hack check --feature-powerset --workspace --locked` | Every combination of features builds |
| `msrv` | `cargo hack check --rust-version --workspace --all-targets --locked` | The workspace builds on Rust 1.98 |
| `deny` | `cargo deny check` | Advisories, licences, duplicate versions and sources |
| `semver` | `cargo xtask semver` | No undeclared breaking change in a stable or preview crate |
| `bench-smoke` | `cargo bench -p waveport-benches --bench wall_smoke --locked -- --test` | The wall-clock benchmarks still run |
| `miri` | `cargo miri test -p waveport-alloc-counter` on the pinned nightly | The one crate with `unsafe` code has no undefined behaviour in its tests |
| `bench-ir` | `cargo xtask bench-ir --base <rev>` | No benchmark executes more than 2% more instructions than at the base revision |

## Layers and the dependency rule

Every crate declares its layer in its manifest, and the layer must match where the crate lives:

```toml
[package.metadata.waveport]
layer = "domain"
stability = "experimental"
```

| Layer | Location | May depend on, in the code it ships |
|---|---|---|
| `domain` | `crates/core/` | Other domain crates |
| `ports` | `crates/core/` | Domain |
| `protocol` | `crates/core/` | No workspace crate |
| `kit` | `crates/core/` | Domain, ports, protocol, other kit crates |
| `application` | `crates/core/` | Domain, ports, protocol, application. Not the kit |
| `adapter` | `crates/adapters/` | Domain, ports, protocol, kit. Never another adapter |
| `composition` | `apps/`, `crates/bindings/` | Everything above |
| `testing` | `crates/testing/` | Everything above, and other testing crates |
| `tooling` | `xtask/`, `benches/` | Anything; `xtask` itself depends on no workspace crate |

Tests follow the same directions, with one addition: any crate may dev-depend on a `testing` crate.

The `kit` layer holds what adapters share: connection plumbing that would otherwise be copied into each provider crate, since one adapter may not depend on another. The application layer cannot use it, so it keeps reaching providers only through ports.

`cargo xtask arch-check` reads the graph from `cargo metadata`, with every feature on, and enforces:

| Rule | Fails when |
|---|---|
| ARCH001 | A dependency crosses layers in a direction the table does not allow |
| ARCH002 | A domain, ports or protocol crate reaches an async runtime, an entropy source or a socket crate, directly or through any chain of dependencies |
| ARCH003 | An adapter reaches another adapter, directly or indirectly |
| ARCH004 | A domain, ports or protocol crate has a third-party dependency that is not allow-listed |
| ARCH005 | A crate that ships depends on a testing or tooling crate |
| ARCH006 | A domain, ports or protocol crate dev-depends on an async runtime |
| ARCH007 | A domain or ports crate has a build script |
| ARCH008 | The layer is missing, unknown, or does not match the crate's location |

The runtime and entropy crate lists, and the third-party crates each inner layer may use, are in the root `Cargo.toml` under `[workspace.metadata.waveport.arch]`. Adding a dependency to an inner layer therefore takes a reviewed change to that file.

Two layers have a second line of defence:

- `waveport-domain` is `#![no_std]` with `alloc`. It cannot name a socket, a clock, a thread or a file, and the `no-std` gate proves it.
- `waveport-media` uses `std`, because it wraps libopus. Its own `clippy.toml` bans clocks, sockets, threads, files and hash maps with random iteration order, so the time is always an argument.

### Adding a crate

1. Create it in the directory its layer requires. A directory the table does not list fails ARCH008; when a new area is needed, extend `layers_for_dir` in `xtask/src/arch.rs` in the same change.
2. Inherit the package fields and lints from the workspace (`version.workspace = true` and so on, `[lints] workspace = true`), and declare `layer` and `stability`.
3. Start the crate documentation with a summary and the line `//! Stability: <tier>`.
4. Declare any new third-party dependency in `[workspace.dependencies]` and refer to it with `workspace = true`.

## Lints

The lint levels live in `[workspace.lints]` in the root `Cargo.toml`. Beyond Clippy's pedantic group, library code may not use `unwrap`, `expect`, `panic!`, indexing that can panic, `dbg!`, `todo!` or `unimplemented!`. Tests may: `clippy.toml` relaxes the first four inside tests.

When a lint is wrong for a specific line, use `#[expect(lint, reason = "…")]` rather than `#[allow]`: the compiler then reports the attribute once it is no longer needed.

A `clippy.toml` inside a crate replaces the root file instead of adding to it, so it has to repeat every root setting. `cargo xtask lint-sync` checks that.

## `unsafe` code

`unsafe` is forbidden across the workspace. `forbid` cannot be lowered by an attribute, so a crate that needs `unsafe` is an explicit, reviewed exception:

1. Its name is added to `[workspace.metadata.waveport.unsafe] allowed` in the root `Cargo.toml`.
2. It does not inherit the workspace lints. It carries a copy of the table with `unsafe_code = "deny"`; `cargo xtask lint-sync` fails if the copy differs in any other way.
3. Each use is marked `#[expect(unsafe_code, reason = "…")]`, and each `unsafe` block and `unsafe impl` has a `// SAFETY:` comment. Clippy rejects one without it.
4. Its tests run under Miri in CI.

Today the only such crate is `waveport-alloc-counter`, which implements a global allocator.

## Dependencies

- Versions are declared once, in `[workspace.dependencies]`, and only for dependencies that are in use. Cargo rejects a dependency a crate declares but does not use, and `cargo deny` rejects a workspace entry no crate uses.
- New versions are not picked up until they are seven days old (`global-min-publish-age` in `.cargo/config.toml`).
- Licences must be on the allow-list in `deny.toml`. Adding a licence is a reviewed change to that file.
- One version of each crate. A duplicate that cannot be avoided is listed in `deny.toml` with the reason.
- `Cargo.lock` is committed, and every gate runs with `--locked`.

## Tests

Tests run with [nextest](https://nexte.st/); documentation tests run with `cargo test --doc`.

**Simulation.** `waveport-sim` runs a scenario over many seeds with a virtual clock. A failure prints the seed. A test that builds its plan with `SeedPlan::from_env` reproduces it, or runs a larger sweep, without being edited:

```sh
WAVEPORT_SIM_SEED=0x000000000000002a cargo nextest run -p <crate> <test>
WAVEPORT_SIM_SEEDS=100000 cargo nextest run -p <crate> <test>   # a larger sweep
```

A scenario stays repeatable only if all its randomness comes from the context's generator and all its time from the context's clock.

**Kernel scenarios.** `waveport-kernel-sim` runs a story about one session against the real kernel, with a simulated world in place of every adapter, along every path the story allows. The six scenarios in `crates/core/waveport-domain/tests/scenarios/` were written before the kernel. Do not edit one to make it pass: needing to is the sign that the kernel's design is wrong, and is settled in review. `tests/scenarios/main.rs` lists each scenario as `unstarted`, `pending` or `passes`, and the listing is checked both ways, so a scenario that starts to pass has to be promoted there in the same pull request. A failure prints the path that led to it, the check that did not hold and the last steps of the session.

**Kernel simulation.** `crates/core/waveport-domain/tests/simulation.rs` runs random sessions, 600 for each of six profiles of what goes wrong (calm, chaos, stalls, duplicates, shutdown, and crashes, in which the process dies and the session goes on from its journal). A monitor in `waveport-kernel-sim` keeps its own account of each session from the events and the kernel's emissions alone, and fails the step at which the kernel breaks an invariant of section 13 of the architecture document, or the session if it ends with something unfinished. The same monitor watches every path of the scenarios. Each test also checks that its sweep did what it is there for, such as interrupting the agent often enough. A kernel rule that the monitor cannot see, because breaking it only makes the session worse and breaks no invariant, needs a test in `tests/kernel.rs`, `tests/tools.rs`, `tests/recovery.rs` or `tests/stage.rs`.

When you change the kernel, run a larger sweep once: the rare failures are the interesting ones, and every one found so far needed more than 600 sessions. A release build runs a million sessions in under a minute:

```sh
WAVEPORT_SIM_SEEDS=200000 cargo test --release -p waveport-domain --test simulation
```

**Small sessions, every path.** `tests/model.rs` takes sessions small enough to try every order in which their events can arrive, and tries them all under the same monitor. Each test is one session, described by a `Scope`: what may happen in it and how often. A failure prints the path as a trace. The larger sessions are ignored in an ordinary run: `cargo test --release -p waveport-domain --test model -- --include-ignored` has them.

**Recorded sessions.** A trace is a session written down as the events its kernel was given (`waveport_kernel_sim::trace`). `tests/traces/` keeps five, each with what the kernel emitted for every event, and `tests/replay.rs` gives a new kernel the same events and compares. A change in what the kernel does therefore shows as a failure there, with the first line that differs. If the change is meant, write the files anew and review their diff, which is the change in behaviour:

```sh
WAVEPORT_BLESS=1 cargo nextest run -p waveport-domain --test replay
```

**Every night.** The `scheduled` workflow runs 200,000 random sessions per profile, other ones each night, and the larger sessions of `tests/model.rs`.

**Kernel cost.** `tests/no_alloc.rs` lists the steps a session takes most often and fails if one of them calls the allocator. `benches/ir/kernel.rs` counts the instructions of single steps and of rebuilding a session from its journal, and `cargo xtask bench-ir` fails a change that makes one more than 2% more expensive.

**Audio path.** `waveport-media` is tested on signals, not on sample values copied from a run: a tone has to come through a codec or a change of rate at its level, and one above the band has to be gone. `tests/network.rs` makes up 500 networks between a sender and the inbound path from seeds (jitter, loss, repeats, outages, bursts) and checks rules on what comes out: one unbroken timeline, each packet once and in order, no deadline in the past. It also checks that the sweep met each kind of trouble often enough. When you change the inbound path, run a larger sweep once; a release build does 200,000 networks in under half a minute:

```sh
WAVEPORT_SIM_SEEDS=200000 cargo test --release -p waveport-media --test network
```

`tests/no_alloc.rs` runs a packet in and a frame out for every codec, with losses, late packets, an interruption and a change of volume, and fails if any of it calls the allocator. `benches/ir/media.rs` counts the instructions of one frame each way for each codec, under the same 2% rule as the kernel's.

**Allocations.** `waveport-alloc-counter` proves a code path does not touch the heap. Install `CountingAllocator` as the global allocator of the test binary and wrap the code in `assert_no_alloc`. Without the allocator installed the assertion fails instead of passing silently.

## Benchmarks

Benchmarks live in the `benches` package: wall-clock ones under `benches/wall/` (criterion) and instruction-count ones under `benches/ir/` (gungraun, under Valgrind). The workloads are defined once under `benches/src/`, so both kinds measure the same code.

```sh
cargo bench -p waveport-benches --bench wall_smoke
cargo xtask bench-ir --base main
```

Only instruction counts gate a pull request: they repeat to within a fraction of a percent, where wall-clock time on a shared runner does not.

## Spikes

`spikes/` holds throwaway experiments that answer a question before product code depends on the answer. Each is its own Cargo workspace with its own lockfile, so it can use dependencies the product workspace would reject, and none of the gates build it. A spike's conclusions are written up under `docs/decisions/` and its raw numbers under `perf/`; the code is deleted once the product has its own benchmark for the same question.

## Pull requests

Pull requests are squash-merged, and the title becomes the commit subject. The changelog is generated from those subjects, so a title follows [Conventional Commits](https://www.conventionalcommits.org/):

```
type(scope)!: subject
```

- **type**: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`, `ci`, `chore` or `revert`.
- **scope** (optional): a crate name without `waveport-` (`domain`, `runtime`, `sim`, `xtask`, …), or one of `ci`, `deps`, `docs`, `release`.
- **`!`** (optional): the change breaks a public API. It is what allows a breaking change to a preview crate.
- **subject**: lower-case imperative, no full stop at the end.

Check a title locally with `cargo xtask check-title 'feat(domain): add epoch counters'`.

## Licence

Waveport is licensed under Apache-2.0. By contributing you agree that your contribution is licensed under the same terms.
