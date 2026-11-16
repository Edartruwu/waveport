# Contributing to Waveport

This guide covers how the repository is checked and the rules those checks enforce. For what is being built and why, see the [roadmap](ROADMAP.md) and the [architecture spec](docs/architecture.md).

## Setup

Install [rustup](https://rustup.rs/). The toolchain (Rust 1.100.0 with rustfmt and Clippy) is pinned in `rust-toolchain.toml` and is installed the first time you run `cargo` in the repository.

Building also needs [CMake](https://cmake.org/) and a C compiler: `waveport-media` uses libopus, which is built from its source and linked in. Nothing has to be installed for it beyond those two.

On Linux it needs the headers of ALSA as well, for the transport that runs on the sound devices of the machine (`sudo apt-get install libasound2-dev` on Debian and Ubuntu). macOS and Windows have what it needs. No sound device is needed to build or to run the tests.

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

Model weights are not committed, and neither is ONNX Runtime, the library that runs them. `assets/assets.lock.toml` pins each file to an address that never changes what it serves, a SHA-256 and a licence. The library is pinned once for each platform, as the file inside the archive its project releases:

```sh
cargo xtask fetch-assets           # download into target/assets/ and verify
cargo xtask fetch-assets --check   # verify only; fails if a file is missing
```

A file whose digest does not match is deleted, never left in place. Adding a model is a reviewed change to the lock file.

The tests of `waveport-models-onnx` and of `waveport-cli` run the real models, so they need the files: fetch them once before `cargo xtask ci`. They are looked for in `target/assets/`, or in the directory `WAVEPORT_ASSETS` names.

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
| `loom` | `cargo test --release --locked -p waveport-runtime --lib --target-dir target/loom` with `RUSTFLAGS="--cfg waveport_loom"` | What a session and the compute plane hand each other arrives whole and wakes whoever waits for it, however the threads take turns |
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

**Every night.** The `scheduled` workflow runs 200,000 random sessions of the kernel per profile, 1,000 of the runtime per story and profile and 20,000 power cuts in the middle of a commit of the journal (see below), other ones each night, and the larger sessions of `tests/model.rs`. It also runs a thousand calls of a minute each on the session actor, which have to give back every byte they took, and measures how much of a thread a session takes (see "Performance gates" below). The `live` workflow runs the tests that talk to the providers (see "Provider adapters" below).

**Kernel cost.** `tests/no_alloc.rs` lists the steps a session takes most often and fails if one of them calls the allocator. `benches/ir/kernel.rs` counts the instructions of single steps and of rebuilding a session from its journal, and `cargo xtask bench-ir` fails a change that makes one more than 2% more expensive.

**Audio path.** `waveport-media` is tested on signals, not on sample values copied from a run: a tone has to come through a codec or a change of rate at its level, and one above the band has to be gone. `tests/network.rs` makes up 500 networks between a sender and the inbound path from seeds (jitter, loss, repeats, outages, bursts) and checks rules on what comes out: one unbroken timeline, each packet once and in order, no deadline in the past. It also checks that the sweep met each kind of trouble often enough. When you change the inbound path, run a larger sweep once; a release build does 200,000 networks in under half a minute:

```sh
WAVEPORT_SIM_SEEDS=200000 cargo test --release -p waveport-media --test network
```

`tests/no_alloc.rs` runs a packet in and a frame out for every codec, with losses, late packets, an interruption and a change of volume, and fails if any of it calls the allocator. `benches/ir/media.rs` counts the instructions of one frame each way for each codec, under the same 2% rule as the kernel's.

**Ports.** `waveport-testkit` has a fake of every port (`fakes`). They keep time by Tokio's clock, so a test under `#[tokio::test(start_paused = true)]` takes no real time and runs the same way every time. Each fake follows a script and says what it can do; change its `caps` and it behaves as it says. `conformance` holds the rules of each port as functions that take any implementation: an adapter's tests call them against a recorded server, and its `live_*` tests against the provider. A new rule goes there, together with a case in `tests/conformance.rs` that breaks it, to show that the check sees it.

**Sessions.** `waveport-runtime` runs whole sessions on those fakes. `tests/sessions.rs` holds conversations written by hand, one for each thing a session has to do or to live through: a question answered, an interruption, a tool call, parts that can do less, a line that closes, a recogniser that fails, a journal that fails or is taken over, a process that dies. `tests/shards.rs` runs sessions on real threads. When you change a port or the session, these say whether a session can still be run. A fault the simulation finds gets a conversation here that fails without the fix: the simulation finds it again only under the seed that has it.

**Session simulation.** `tests/simulation/` makes sessions up from seeds and runs them on the real session actor, with the kernel's monitor on every step. A seed decides who calls and what they say, how late every part answers, what the line does to the packets, which parts fail, what the application asks, and at which records of its journal the process dies. `stories` tells the six kernel scenarios again, and `sweeps` makes up sessions nobody wrote under four profiles (calm, chaos, crashes, shutdown). Beyond the invariants, `world.rs` checks every run on what the far end was sent, that nothing is in the conversation that the far end did not play, that no tool with an effect was called early or twice, and that a session rebuilt from the journal is the session that wrote it. A new rule about what a session may do to the world goes there.

An ordinary run has 25 sessions per test. When you change the session, run more once; a release build does 10,000 in about two minutes. A failure prints its seed, and that seed alone prints every step of its session:

```sh
WAVEPORT_SIM_SEEDS=1000 cargo test --release -p waveport-runtime --test simulation
WAVEPORT_SIM_SEED=12 cargo test --release -p waveport-runtime --test simulation sweeps::shutdown
```

With `-- --nocapture` each test prints what its sessions did, counted: turns, interruptions, tool calls and how they ended, processes that died. A sweep in which something stopped happening passes while it tests nothing, so look at the counts when you change what a profile makes up.

**Journal.** `waveport-store-journal` is held to what every journal is held to (`waveport_testkit::conformance`), and `tests/journal.rs` adds what only a journal in a file can be held to. `tests/power.rs` puts the file on a disk in memory that the test can break: a write or a sync that fails, and the power going at a write that a seed picks, in the middle of a commit, with every write that was not yet synced kept, lost or torn. After each cut the file is opened again, and nothing the journal acknowledged may be gone, a commit has to be there whole or not at all, and the sessions have to go on. An ordinary run makes 60 cuts; when you change how the journal writes, make more once. A release build makes 2,000 in under two minutes:

```sh
WAVEPORT_SIM_SEEDS=2000 cargo test --release -p waveport-store-journal --test power -- --nocapture
```

The test prints how often the commit that was cut was found whole and how often not at all. Both have to happen, or it tests nothing: a cut that always lands before the commit proves only that an empty file opens.

**Threads.** A session never leaves its thread, and the listening models run on others. Everything the two sides share is in `src/handoff.rs` of the runtime, and its tests try every way the threads of a small case can take turns, with [loom](https://docs.rs/loom). They are in a build of their own, which the gate `loom` makes:

```sh
cargo xtask ci --only loom
```

If you change what crosses between a session and the compute plane, change it there and add a schedule for it. Check that the schedule fails when the code is wrong: put the wake-up before the answer, or the head of a frame before its samples, and loom has to report a thread that never wakes. Under `--cfg waveport_loom` the rest of the crate is not built, and the rings are stand-ins of which every operation is one step: the real ones are `rtrb`'s.

**Provider adapters.** An adapter is tested against a server that the test starts on the loopback interface and that follows a script (`waveport_testkit::servers`): each line of a script is something the server says, something it waits to be told, a pause, or the way it ends the connection. Where a provider has been listened to, the script is a recording of it, kept under `tests/fixtures/` with the day it was made, and the crate documentation lists what the recording taught. A recording of speech synthesis keeps every message but leaves the audio out and says how long it was; the test puts silence of that length in its place. Where it has not, the crate documentation says so in bold, and the messages are made up in the documented shapes. Never edit a recording to make a test pass: what the provider said is the fact, and the adapter is what has to change.

The tests named `live_*` talk to the provider itself. They are ignored in an ordinary run, need the provider's key in the environment (`OPENAI_API_KEY`, `CARTESIA_API_KEY`, `DEEPGRAM_API_KEY`, `ASSEMBLYAI_API_KEY`, `ELEVENLABS_API_KEY`), cost a few cents, and send speech made by a machine, never a recording of a person:

```sh
cargo nextest run -p waveport-stt-openai --run-ignored only live_
```

The `live` workflow runs them every night, and when somebody starts it by hand, with keys from the repository's secrets, which are named as the variables are. A provider whose key is not there is passed over, so a repository with no secrets runs nothing. **That workflow must never run for a pull request**: code that nobody has reviewed would have the keys in reach. A test in `xtask` fails if it is given such a trigger, if an adapter has `live_*` tests that it does not run, or if another workflow reads a provider's key.

An adapter that has not been held against its provider says so in bold in its crate documentation. The first run of its `live_*` tests is expected to find things; when it has, the made-up messages give way to recordings.

A key is read from the environment and goes nowhere but to its provider: none is ever an argument, a file in the repository or a line of output. `xtask` has a test that reads every file under a `fixtures` directory and fails on anything with the shape of a key, because a provider repeats a key it does not know.

**Sound devices.** `waveport-transport-local-audio` is tested without a sound card. Everything between the session and the device is in `src/line.rs`, whose tests play both sides, two threads among them. `tests/scripted.rs` puts a person who follows a script where the card would be, on a clock the test controls. Three tests named `live_*` use the devices of the machine and are ignored in an ordinary run; `live_a_tone_is_played_while_the_microphone_listens` makes a sound and opens the microphone.

**A whole call.** `apps/waveport-cli/tests/scripted/` starts the `waveport` command itself with a scripted caller and with scripted servers in place of the three providers. The server that stands in for OpenAI's speech recognition replays a recording of it, and the others are made up. The models, the adapters, the runtime and the audio path are the real ones, and so is the clock: a call takes about four seconds. The same call is run with OpenAI and Cartesia, with Deepgram and ElevenLabs, and with AssemblyAI, and once with the agent in `waveport serve` and the caller in `waveport client`, two processes with a WebSocket between them. Each run checks what the command printed, what each server was sent, and the recording of what the caller was played. When one fails it prints what each server was sent, which is where to start reading. An adapter for a new provider gets a stand-in and a run here.

**A call that is killed.** `resumed.rs` in the same folder runs such a call with its journal in a file, kills the process as `kill -9` does, starts the command again with `--resume last`, and has the caller ask once more. It does so at six places: while the model answers, while the agent speaks, once a tool has acted and before it has answered, while the record that allows a tool waits to be stored, and before and after the commit that stores all the call has come to. Each run checks that the second process goes on with the call, that the model is shown the conversation as the journal had it, that the tool that acts (`take_note`, which adds a line to a file) acted at most once in both processes together, that the caller's next question is answered aloud, and that a call that is over is not there to go on with.

Some of those places last less than a millisecond, so the command is told where to stop: `WAVEPORT_TEST_HALT` names a place, the command says `halted: <place>` when it is there, and the test kills it then. A place in the journal (`before:<kind>` or `after:<kind>` of a record, or of `state`) stops the thread that writes the journal at that commit, while the rest of the call runs on; `tool` stops the tool that takes notes once it has written. `apps/waveport-cli/src/halt.rs` is all of it, and nothing else reads the variable. A new place a call can die in gets a case there. Check that a case can fail: make the kernel send a call with an effect again after a restart, and the case with the tool has to find two notes.

**The WebSocket transport.** `crates/adapters/transport/websocket/tests/loopback.rs` runs the server side and the client against each other over real sockets on the loopback interface, and a client that breaks the protocol against the server.

**Adapter kit.** The WebSocket and HTTP clients are tested against servers that the tests start on the loopback interface; nothing leaves the machine. A test of a connection that is refused has to accept a timeout as well: Windows takes about two seconds to refuse a closed port on the loopback interface.

**Allocations.** `waveport-alloc-counter` proves a code path does not touch the heap. Install `CountingAllocator` as the global allocator of the test binary and wrap the code in `assert_no_alloc`. Without the allocator installed the assertion fails instead of passing silently.

## Benchmarks

Benchmarks live in the `benches` package: wall-clock ones under `benches/wall/` (criterion) and instruction-count ones under `benches/ir/` (gungraun, under Valgrind). The workloads are defined once under `benches/src/`, so both kinds measure the same code.

```sh
cargo bench -p waveport-benches --bench wall_smoke
cargo xtask bench-ir --base main
```

Only instruction counts gate a pull request: they repeat to within a fraction of a percent, where wall-clock time on a shared runner does not.

The journal has one of each. `ir_journal` counts what a record costs the session that makes it: a fact written as a record, and the records of a step handed to the journal, with the thread that writes the journal held in a commit so that nothing it does is counted. `wall_journal` measures how long the records then take to be stored, which is what a tool with an effect waits for: for one session and for 64 at once, in one phase and in two, in a file under the target directory and in memory. That number is the disk's more than the journal's, so read it on the machine that matters:

```sh
cargo bench -p waveport-benches --bench wall_journal
```

## Performance gates

`perf/targets.toml` holds what the code is held to. A number there is either a target of the design (section 12 of the architecture) or what the code does today, and a number that is held is never looser than its target. A change that does worse than today fails. If the cost is meant, change the number in the same pull request, where a reviewer sees it; if a change does better, lower the number, so that the next change is held to the better one.

Three kinds of test read the file, all in the `benches` package, and all run whole calls on the real session actor with a stand-in in every port (`benches/src/calls.rs`).

- `tests/latency.rs`, in every test run: how long the parts of a turn take on a clock the test controls. Such a clock counts waiting and not work, so these numbers are the same on every machine.
- `tests/memory.rs`, in every test run: what a session holds on the heap when it listens and when it speaks, and that nothing is left of sessions that are over. A session that holds much less than the file says fails too: say the good news in the file. The thousand calls of every night are the test in it that is ignored:

  ```sh
  cargo test --release -p waveport-benches --test memory -- --ignored
  ```

- `tests/load.rs`, every night and by hand: fifty calls on one shard of the runtime in real time. It prints how much of its thread a session takes, how long a session takes to start and how long the parts of a turn take.

  ```sh
  cargo test --release -p waveport-benches --test load -- --ignored --nocapture --test-threads 1
  ```

A time is a number about a machine, so two things are done with what `tests/load.rs` prints. Every night, on a shared runner, the share of a thread is divided by what a piece of reference work takes in the same run, and that ratio is held to `[density]` in the file. And on a machine that is left alone, the numbers themselves are recorded and checked:

```sh
cargo xtask perf record --cpu 6    # writes perf/baselines/<processor>.toml
cargo xtask perf check             # measures the same way, and holds the machine to its record
```

`--cpu` keeps the measurement on one processor, with `taskset`, and is worth using on a machine whose cores are not alike: the numbers then repeat to about 1%. A check that fails measures again, up to three times, because whatever else a machine does can only make a measurement worse.

The same machine is not always the same machine. A laptop takes nearly twice as long over the same work when it is on its battery, warm, or busy, and then over a session too. So a record says how long the reference work took, and a check refuses a machine that takes a tenth longer or shorter over it now: bring the machine back to that state, or make a new record. Record with the machine as it is when it is left alone. The record is named after the machine's processor and says nothing else of the machine. Make a new record in the pull request that changes what a session costs, and say why it changed.

## Spikes

`spikes/` holds throwaway experiments that answer a question before product code depends on the answer. Each is its own Cargo workspace with its own lockfile, so it can use dependencies the product workspace would reject, and none of the gates build it. A spike's conclusions are written up under `docs/decisions/` and its raw numbers under `perf/`; the code is deleted once the product has its own benchmark for the same question. There is none at the moment: the density spike was deleted when `cargo xtask perf` could measure the runtime itself, and its record is [decision 0001](docs/decisions/0001-density-spike.md).

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
