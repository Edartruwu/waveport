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

The gate of the JavaScript client needs [Node.js](https://nodejs.org/) 24 or later, whose npm keeps a version out until it is a week old, and a Chrome or Chromium for the test that runs a browser. The browser is looked for where it is usually installed, or where `CHROME_BIN` says it is. It is run without a window, makes no sound and opens no microphone.

### Model files

Model weights are not committed, and neither is ONNX Runtime, the library that runs them. `assets/assets.lock.toml` pins each file to an address that never changes what it serves, a SHA-256 and a licence. The library is pinned once for each platform, as the file inside the archive its project releases:

```sh
cargo xtask fetch-assets           # download into target/assets/ and verify
cargo xtask fetch-assets --check   # verify only; fails if a file is missing
```

A file whose digest does not match is deleted, never left in place. Adding a model is a reviewed change to the lock file.

The tests of `waveport-models-onnx`, of `waveport-assembly` and of `waveport-cli` run the real models, so they need the files: fetch them once before `cargo xtask ci`. They are looked for in `target/assets/`, or in the directory `WAVEPORT_ASSETS` names.

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
| `js` | In `sdks/js`: `npm ci`, `npm run check`, `npm test`, then `cargo build --locked -p waveportd --example stand_in` and `node test/browser/run.js --daemon ../../target/debug/examples/stand_in` | The JavaScript client has the types its comments give, passes its tests, and has whole sessions with the real daemon from a headless Chrome |

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
| `composition` | `apps/`, `crates/bindings/` | Everything above, and other composition crates |
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
- What is written in JavaScript (`sdks/`) runs on nothing but the browser: a `package.json` there has no dependencies. What it is checked with is the TypeScript compiler and the types of Node, each at one exact version, in a lock file that is committed and that `npm ci` installs from. Its `.npmrc` keeps a version out until it is seven days old and runs no script of a package as it is installed. A test in `xtask/src/js.rs` holds every package under `sdks/` to all of that, and adding a tool is a reviewed change to the list there.

## Tests

Tests run with [nextest](https://nexte.st/); documentation tests run with `cargo test --doc`.

**Simulation.** `waveport-sim` runs a scenario over many seeds with a virtual clock. A failure prints the seed. A test that builds its plan with `SeedPlan::from_env` reproduces it, or runs a larger sweep, without being edited:

```sh
WAVEPORT_SIM_SEED=0x000000000000002a cargo nextest run -p <crate> <test>
WAVEPORT_SIM_SEEDS=100000 cargo nextest run -p <crate> <test>   # a larger sweep
```

A scenario stays repeatable only if all its randomness comes from the context's generator and all its time from the context's clock.

**Kernel scenarios.** `waveport-kernel-sim` runs a story about one session against the real kernel, with a simulated world in place of every adapter, along every path the story allows. The scenarios in `crates/core/waveport-domain/tests/scenarios/` are written before the kernel does what they ask: six before the kernel of phase 1, and ten before phase 2, about a session of text, a line that drops, the keys of a telephone and turns whose words come after them. A scenario may ask the world for something the kernel cannot be told yet (a key, late words); the world then keeps it to itself, and the scenario fails where it expects the kernel to have acted. Do not edit one to make it pass: needing to is the sign that the kernel's design is wrong, and is settled in review. `tests/scenarios/main.rs` lists each scenario as `unstarted`, `pending` or `passes`, and the listing is checked both ways, so a scenario that starts to pass has to be promoted there in the same pull request. A failure prints the path that led to it, the check that did not hold and the last steps of the session.

**Kernel simulation.** `crates/core/waveport-domain/tests/simulation.rs` runs random sessions, 600 for each of eight profiles of what goes wrong (calm, chaos, stalls, duplicates, shutdown, crashes, in which the process dies and the session goes on from its journal, lines, in which the line drops and is back in time, too late or never, and keys, in which callers press keys among what they say and over the agent). A monitor in `waveport-kernel-sim` keeps its own account of each session from the events and the kernel's emissions alone, and fails the step at which the kernel breaks an invariant of section 13 of the architecture document, or the session if it ends with something unfinished. The same monitor watches every path of the scenarios. Each test also checks that its sweep did what it is there for, such as interrupting the agent often enough. A kernel rule that the monitor cannot see, because breaking it only makes the session worse and breaks no invariant, needs a test in `tests/kernel.rs`, `tests/tools.rs`, `tests/recovery.rs`, `tests/stage.rs`, `tests/line.rs` or `tests/keys.rs`. A rule the monitor does see needs a test beside the monitor in which a kernel breaks it, to show that the monitor notices.

When you change the kernel, run a larger sweep once: the rare failures are the interesting ones, and every one found so far needed more than 600 sessions. A release build runs a million sessions in under a minute:

```sh
WAVEPORT_SIM_SEEDS=200000 cargo test --release -p waveport-domain --test simulation
```

**Small sessions, every path.** `tests/model.rs` takes sessions small enough to try every order in which their events can arrive, and tries them all under the same monitor. Each test is one session, described by a `Scope`: what may happen in it and how often. A failure prints the path as a trace. The larger sessions are ignored in an ordinary run: `cargo test --release -p waveport-domain --test model -- --include-ignored` has them.

**Recorded sessions.** A trace is a session written down as the events its kernel was given (`waveport_kernel_sim::trace`). `tests/traces/` keeps six, each with what the kernel emitted for every event, and `tests/replay.rs` gives a new kernel the same events and compares. A change in what the kernel does therefore shows as a failure there, with the first line that differs. If the change is meant, write the files anew and review their diff, which is the change in behaviour:

```sh
WAVEPORT_BLESS=1 cargo nextest run -p waveport-domain --test replay
```

**Every night.** The `scheduled` workflow runs 200,000 random sessions of the kernel per profile, 1,000 of the runtime per story and profile, 20,000 power cuts in the middle of a commit of the journal (see below) and a million damaged texts through the parsers of the wire formats, other ones each night, and the larger sessions of `tests/model.rs`. It also runs a thousand calls of a minute each on the session actor, which have to give back every byte they took, and measures how much of a thread a session takes (see "Performance gates" below). The `live` workflow runs the tests that talk to the providers (see "Provider adapters" below).

**Kernel cost.** `tests/no_alloc.rs` lists the steps a session takes most often and fails if one of them calls the allocator. `benches/ir/kernel.rs` counts the instructions of single steps and of rebuilding a session from its journal, and `cargo xtask bench-ir` fails a change that makes one more than 2% more expensive.

**Audio path.** `waveport-media` is tested on signals, not on sample values copied from a run: a tone has to come through a codec or a change of rate at its level, and one above the band has to be gone. `tests/network.rs` makes up 500 networks between a sender and the inbound path from seeds (jitter, loss, repeats, outages, bursts) and checks rules on what comes out: one unbroken timeline, each packet once and in order, no deadline in the past. It also checks that the sweep met each kind of trouble often enough. When you change the inbound path, run a larger sweep once; a release build does 200,000 networks in under half a minute:

```sh
WAVEPORT_SIM_SEEDS=200000 cargo test --release -p waveport-media --test network
```

`tests/no_alloc.rs` runs a packet in and a frame out for every codec, with losses, late packets, an interruption and a change of volume, and the tones of keys at every rate, and fails if any of it calls the allocator. The tones (`dtmf`) are held to the sample: each is compared with the two sines it is made of, and its first samples and a sum of all of them are written down. `benches/ir/media.rs` counts the instructions of one frame each way for each codec, under the same 2% rule as the kernel's.

**Ports.** `waveport-testkit` has a fake of every port (`fakes`). They keep time by Tokio's clock, so a test under `#[tokio::test(start_paused = true)]` takes no real time and runs the same way every time. Each fake follows a script and says what it can do; change its `caps` and it behaves as it says. The line of the fake transport can be scripted to drop and to come back, and nothing gets through it either way while it is gone. Its far end can be scripted to press keys, and it says how keys leave it and keeps those it was asked to press and the tones it was sent. `conformance` holds the rules of each port as functions that take any implementation: an adapter's tests call them against a recorded server, and its `live_*` tests against the provider. A new rule goes there, together with a case in `tests/conformance.rs` that breaks it, to show that the check sees it.

**Sessions.** `waveport-runtime` runs whole sessions on those fakes. `tests/sessions.rs` holds conversations written by hand, one for each thing a session has to do or to live through: a question answered, an interruption, a tool call, parts that can do less, a line that closes, a line that drops and comes back or stays gone, keys that a caller presses and keys the session is asked to press, a recogniser that fails, a journal that fails or is taken over, a process that dies. `tests/text.rs` holds the same for sessions of text, which have a model and tools and nothing that hears or speaks: what they write is read from what they tell a follower. `tests/shards.rs` runs sessions on real threads. When you change a port or the session, these say whether a session can still be run. A fault the simulation finds gets a conversation here that fails without the fix: the simulation finds it again only under the seed that has it.

**Session simulation.** `tests/simulation/` makes sessions up from seeds and runs them on the real session actor, with the kernel's monitor on every step. A seed decides who calls and what they say, how late every part answers, what the line does to the packets, which parts fail, what the application asks, and at which records of its journal the process dies. `stories` tells ten kernel scenarios again, the six of phase 1, the two of a line that drops and the two of keys, and `sweeps` makes up sessions nobody wrote under six profiles (calm, chaos, crashes, shutdown, lines, keys). `written` does both for sessions of text: the four stories that are stories in writing too, and chats nobody wrote under the first four profiles, since a chat has no line that could drop and no telephone. Beyond the invariants, `world.rs` checks every run on what the far end was sent, that nothing is in the conversation that the far end did not play, that nothing was said into a line that was gone, that keys left as the line takes them, that no tool with an effect was called early or twice, and that a session rebuilt from the journal is the session that wrote it. A session of text has no far end but whoever follows it, and is checked on what they were told: what is on record of an answer is what was told of it, and nothing of an answer is told once it is over. A new rule about what a session may do to the world goes there.

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

**The journal's format.** What a record and a stored state look like is in `waveport-protocol` (`journal.rs`), whose first lines say when a change needs a new version: a new kind of fact does. A record is written in the first version that has what it holds, so the facts there were before are written as they were. `crates/core/waveport-runtime/tests/fixtures/` keeps a journal for each version before the newest, written by the build before the change and not by hand, and a test in `codec.rs` reads it, writes it again byte for byte and restores a session from it. A new version adds the journal of the one before it there, in the same pull request.

**The client protocol.** What a client and a session say to each other, and the bodies of the control API, are in `waveport-protocol` (`client/v1.rs`, `control/v1.rs`). `crates/core/waveport-protocol/tests/fixtures/` is the protocol as it is promised: every message as it is written, byte for byte (`from_client.jsonl`, `from_server.jsonl`, `bodies.jsonl`), what a reader has to tolerate (`tolerated.jsonl`), what is refused (`refused.jsonl`), and what a client holds after what it was sent (`views.json`). The clients in other languages read the same files, so a change to one is a change for all of them.

- A new kind of message, fact, news or item gets a line in the fixtures. The tests name every kind in a `match` that does not compile without the new one, next to the list the fixtures have to cover.
- A reader skips what it does not know, so a field or a kind that old readers can do without is no new version. Anything a reader would misread without noticing is.
- What a client is shown of a record of the journal is decided in `Fact::shown`, which has a case for every kind of record. Text that a tool was given or answered is shown only where `Show` says so, and the tests look for it where it must not be.
- A token in a fixture is made up and short (`wpc1_madeup`). `xtask`'s scan for keys knows how both kinds of token begin, and fails on one that is long enough to be real.
- `tests/garbage.rs` gives every parser text made up from a seed: 20,000 texts in every test run and a million every night, other ones each night. A failure names the seed that `WAVEPORT_SIM_SEED` runs alone. A new parser is added to it. Run more after a change to a parser or to what a client holds:

  ```sh
  WAVEPORT_SIM_SEEDS=1000000 cargo test --release -p waveport-protocol --test garbage --test client_v1
  ```
- That a stored state, shown to a client, is what the facts before it add up to is not something the protocol crate can know: it has no kernel. The simulation of the runtime checks it on every state every session stores (`shown_adds_up` in `tests/simulation/world.rs`). Check that it can fail: show a client an item it is not shown as a fact, and the first seed fails.

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

**A whole call.** `apps/waveport-cli/tests/scripted/` starts the `waveport` command itself with a scripted caller and with scripted servers in place of the three providers. The server that stands in for OpenAI's speech recognition replays a recording of it, and the others are made up. The models, the adapters, the runtime and the audio path are the real ones, and so is the clock: a call takes about four seconds. The same call is run with OpenAI and Cartesia, with Deepgram and ElevenLabs, and with AssemblyAI, and once with the agent in `waveport serve` and the caller in `waveport client`, two processes with a WebSocket between them. One more caller of `waveport serve` is the test itself, with the transport's client: it drops its connection, is refused with a token that is not its call's, comes back with the one it was given, and reads the call as facts. And one caller is `waveport client` with a stand-in for an agent's side that the test writes by hand: it closes the line with no goodbye, then says that it is going away, and the caller has to come back each time with what it was last given. Each run checks what the command printed, what each server was sent, and the recording of what the caller was played. When one fails it prints what each server was sent, which is where to start reading. An adapter for a new provider gets a stand-in and a run here.

**A chat.** `written.rs` in the same folder starts `waveport chat` with what is typed coming through a pipe, and a scripted server in place of the language model. No recogniser, synthesiser or model that listens is started, and the command is told of none. The runs check what the command printed and what the model was asked: a question answered with a tool and ended by the agent, lines that come at once and are said one at a time, a chat killed once its journal has an answer and gone on with, and one killed before its journal has the answer, which is then given again. The chat that is killed is also what `waveport calls` is tried on: it lists nothing before, the chat while it is broken off, and nothing when the chat is over.

**A call that is killed.** `resumed.rs` in the same folder runs such a call with its journal in a file, kills the process as `kill -9` does, starts the command again with `--resume last`, and has the caller ask once more. It does so at six places: while the model answers, while the agent speaks, once a tool has acted and before it has answered, while the record that allows a tool waits to be stored, and before and after the commit that stores all the call has come to. Each run checks that the second process goes on with the call, that the model is shown the conversation as the journal had it, that the tool that acts (`take_note`, which adds a line to a file) acted at most once in both processes together, that the caller's next question is answered aloud, and that a call that is over is not there to go on with. A seventh run has the agent in `waveport serve` and the caller in `waveport client`, and kills the agent alone while the model answers. The agent is started again at the same address with `--resume`. Nobody tells the caller: it notices that its line dropped, comes back until it is taken, is sent the call as it stands, hears the answer it was owed on the connection it came back on, and asks again.

Some of those places last less than a millisecond, so the command is told where to stop: `WAVEPORT_TEST_HALT` names a place, the command says `halted: <place>` when it is there, and the test kills it then. A place in the journal (`before:<kind>` or `after:<kind>` of a record, or of `state`) stops the thread that writes the journal at that commit, while the rest of the call runs on; `tool` stops the tool that takes notes once it has written. `apps/waveport-assembly/src/halt.rs` is all of it, and nothing else reads the variable. A new place a call can die in gets a case there. Check that a case can fail: make the kernel send a call with an effect again after a restart, and the case with the tool has to find two notes.

**The daemon.** `apps/waveportd/tests/served/` starts `waveportd` itself with a configuration in a folder of its own, on a port the machine picks, and asks it over HTTP with a client that says exactly what a test has it say (`far_end.rs`): no library stands between a test and the bytes. The language model is a scripted server. `asked.rs` holds what the daemon is asked and answers: a chat from its first word to its last, a client that comes back to its stream, who may ask what, where a request may come from, and what is answered to what is asked wrongly. `kept.rs` holds a daemon that stops and the one after it: one that is told to stop while an answer is written, and one that is killed at a place `WAVEPORT_TEST_HALT` names, as for a call. What a client holds after it came back is compared with what a client holds that saw everything, by the reducer of the protocol (`View`), so a test of coming back says what was sent and that it adds up. `lined.rs` holds a far end on a WebSocket, with the client of the transport: it writes and is answered, stops the agent, drops its line and comes back, to the same daemon and to the one after a kill or a stop, and asks for a line in each way that gets none. `aloud.rs` holds one that speaks. The test codes a question as Opus, sends a packet every 20 ms as a browser does, and decodes what it is sent, with scripted servers for the recogniser and the synthesiser; it needs the models that listen (`cargo xtask fetch-assets`). `placed.rs` holds where the threads of a daemon run: one is started with processors for its shards, and the system is asked which processors each thread of the process may run on (`/proc/<pid>/task`). That is on Linux. Anywhere else the same file has to keep a daemon from starting.

A rule about who may ask, or about coming back, is tried by breaking it: let a client token be used twice, leave out the look at `Origin`, or believe the number a client says without its life, and a test has to fail. Twenty-three such changes were made when the daemon was written. The first round found one that no test noticed, and three that a test noticed only by hanging until it was stopped; the tests were mended, and each change now fails a test that says what is wrong. Pinning was tried the same way, with twelve changes (a shard that is not kept on its processor, a thread it starts that stays with it, a first thread that is not kept off), and each fails a test. So were lines, with thirty (a token taken from the address, a client token good twice, a session that was on a line gone on with on none, a client shown what the tools said, a daemon that goes without saying so, a far end greeted after what it says), and each fails a test. One rule is not held that way: a daemon that goes holds a line until its goodbye is said. No test can make the thread of a session late, so a daemon without the hold fails only now and then, on a busy machine.

A test that kills a daemon to see what the next one holds names the place (`WAVEPORT_TEST_HALT`), and makes sure that nothing it does not want stored can be stored before it: the journal writes on a thread of its own and takes what is waiting together, so an answer that is written at once can be stored with the question it answers. `lined.rs` has the model stop in the middle of its answer for that, which holds on any machine. `kept.rs` is about an answer that was told and never stored, so there the model answers slowly, and the question is stored well before the answer ends. Such a test also waits for what the kill is meant to follow, and not for the place alone: a process that is killed while the model answers is killed once the model was asked, because it asks the model as it stores the question and the two are not in step. And what a process says is read by a thread of the test's own, which can be behind: a test waits for the line it looks for (`noted`), or for the process to be over, when its output is read to the end.

Tests that wait on sockets and a clock are run many times before they are trusted, and on a machine that is kept busy (a loop per processor and a few more). What was mended with the daemon's lines, three faults and four tests, each showed about once in fifteen to sixty such runs, and never in five quiet ones.

**The page a daemon serves.** `page.rs` in the same folder holds what the daemon answers when it is asked for its playground: the page and the client beside it, byte for byte as the repository has them, what a browser is told it may do with them (nothing that is not from the daemon, and not inside another page), that a page from elsewhere and a name that was pointed at the machine get nothing, and that a daemon which listens beyond the machine serves it only when told to. What a browser does with the page is tested from the client's side.

**The JavaScript client.** `sdks/js` is tested in Node, with `node --test` and nothing installed for it, and in a browser. `test/protocol.test.js` reads the fixtures of the Rust protocol (`crates/core/waveport-protocol/tests/fixtures/client-v1/`): every message has to be read and written as it stands, what is unknown has to be tolerated as the protocol says, and a view has to come to what the Rust one comes to, so the two cannot drift apart without a test failing on one side. `test/client.test.js` is the connection, with a server that is the test. `test/worklet.test.js` is what runs on the audio thread, as plain classes: what is played and what is said to have been played, by the sample, with the time the output takes. `test/voice.test.js` stands in for a browser's sound and holds the voice to the order a call needs: ready before the client connects, and on the line in the same turn in which the client is welcomed. A test that waits for something that never comes is ended after 20 s and fails (`--test-timeout`).

`test/browser/run.js` is a headless Chrome on the playground of `apps/waveportd/examples/stand_in.rs`, which is the real daemon with the models that listen and with stand-ins for its three providers (`waveport_testkit::stand_in`, which the tests of the command and of the daemon use too). The script drives Chrome over the pipe it offers for that (`--remote-debugging-pipe`), with nothing installed. Chrome is given the question the stand-in recogniser hears as a recording in place of a microphone (`--use-file-for-fake-audio-capture`) and plays to nothing (`--disable-audio-output`): no sound is made and no microphone is opened, so it runs on a machine without either. It has a spoken call as Opus and one as plain samples, a session in writing, and one whose line drops and comes back. In each the agent greets the moment the page is on the line. What the page shows of the conversation has to be the greeting, the question and the answer, each heard to its end. That alone would not show that the page played anything: a session takes a far end that says nothing of what it played to play as it is sent. So the test keeps what the page says on its line, and each run has to have been said played to its last sample, 19,200 for the greeting and 57,600 for the answer, in counts that never go back. `--notes` prints what the page wrote to its console, and `--can-run` says whether there is a Node and a Chrome to run it with, which is what the gate asks before it starts.

The rules of the client, of the page and of how the daemon serves it were tried by breaking them, sixty-seven changes: a token of the wrong kind that is said, a client that comes back as if it had seen nothing, a sample that counts as heard before the output could sound it, a voice that is on its line a moment after the welcome, a page that is served to a page from elsewhere, a package that takes on a dependency. The first round found four that no test noticed. Nothing held the number of things that wait for a line that is away. And three faults of playing, a voice that came late to its line, an audio thread that played nothing and one that was never told a run was whole, left a conversation that read as it should, for the reason above; that is what put the counts into the browser test. Each change now fails a test that says what is wrong, and none by hanging: a test in Node that waits for what never comes fails after 20 s.

The page and the client are read into the daemon when it is built (`include_str!`), so the daemon is built before a browser is run on it, which `cargo xtask ci --only js` does. A change to a file of `sdks/js/src` or `sdks/js/playground` whose name changes is a change to the list in `apps/waveportd/src/playground.rs`, and a test there fails if a file names another that is not served.

**The WebSocket transport.** `crates/adapters/transport/websocket/tests/loopback.rs` runs the server side and the client against each other over real sockets on the loopback interface: with plain samples and with Opus, on a line of text, with a client that drops and comes back, one that takes another's place, and a client that says what no client should, which is told what was wrong and is heard as before. Its rules, those of coming back in the command, and the checks of the next paragraph were tried by breaking them, 52 changes. The first round found three that no test noticed: a token that read as the call's own and was not, a line closed with no goodbye, and a check that another check stood in front of. Each now fails a test.

**The carrier transport.** `crates/adapters/transport/telephony-ws/tests/loopback.rs` runs the transport against a stand-in for a carrier (`sim`, part of the crate), over real sockets on the loopback interface. The stand-in is written from the same documentation as the transport. So what the tests show is that the transport does what the documentation asks of a server. They cannot show what a carrier does that its documentation does not say, and the crate says in bold that it has not been held against one. Three things keep the stand-in from agreeing with whatever the transport does. It takes only the messages the documentation shows, field for field, and keeps anything else as a complaint, and a test that sends what a carrier takes asserts that there is none. It holds what it is sent, and says a mark back only when a test has had it play up to there, or throw it away. And it has tests of its own, with the sentences of the documentation they stand for beside them. The messages the documentation shows are fixtures (`tests/fixtures/twilio`), dated, with the documentation's own placeholders in them, and the signature is held to the example the documentation works through. A recording of a real call, when there is one, goes beside them with its numbers and names made up, and without the header its signature is in.

A new carrier is a module that reads and writes its messages, an arm in each function of `Carrier`, its fixtures, and its part of the stand-in. The four checks of every transport and the tests of that file are then run for it. The rules of the transport, of the signature and of the stand-in were tried by breaking them. The first round found a transport that was over refusing what it says it takes without a word, and a test of the stand-in that passed for the wrong reason, and one change was noticed only by a test that hung. Two more were not rules a test can hold, and were taken out of the round. Remembering that a response had ended changed nothing anybody could see, and is gone from the code. And whether a transport that ended its own call says that its caller left is told to nobody, because a transport that has said goodbye says nothing more. Each of the 102 changes of the last round fails a test, and none by hanging.

**A carrier's call to the daemon.** `called.rs` in the daemon's tests is the carrier: it posts the form Twilio posts, signed as Twilio signs, reads the answer as a carrier does, and brings the call to the stream the answer names with the transport's stand-in, which holds what it is sent and plays it at the pace of a telephone. One test has one agent of one daemon answer in writing, on a client's line in Opus and on such a call in G.711, and holds the three to the same conversation. The stand-ins for the providers speak at the rate a session asks of them (`stand_in::spoken_at`), because a telephone line asks a synthesiser for 8 kHz: one that answers at 24 kHz whatever it is asked makes a call three times as long as what was said, which is how that was found. A test that turns a stream away uses a daemon whose agent has a voice, so that a stream which is let in by mistake is sent it and the test sees the difference between a stream that was refused and a session that failed to start.

Two faults of the first draft are tests now. A session that was made for a call could be started as one that is written to by whoever followed it before the stream came, and the call was then turned away: a test that followed the session a moment too early found it. And a stream that said the ticket of one call and the name of another used up what the other was let in by, which was seen when the rules were listed to be broken.

The rules of the two routes, of the memory of calls, of the configuration and of what the stand-ins gained were tried by breaking them, seventy-four changes. The first round missed one: no test posted a form that says more of its kind than its name (`; charset=UTF-8`), and the tests post that now. One more was taken out of the round. A call is not started on a session that has run, and nothing a test does from outside brings a stream to a session that has: the ticket is used up, and the calls a daemon answered are forgotten with it. The check stays. Each of the seventy-four was noticed, none by a test that hung.

**A transport, any transport.** What every transport owes a session is checked by four functions of `waveport-testkit` (`conformance::transport_hears`, `transport_speaks`, `transport_says_goodbye` and `transport_is_left`). They know nothing of how a transport reaches its far end: a transport's tests give them its own client as the far end, by implementing `FarEnd` for it, and a transport that nobody has looked at yet. `crates/testing/waveport-testkit/tests/conformance.rs` has a line that keeps the rules and breaks one at a time, twenty-one ways, and each has to be told which. A new transport implements `FarEnd` for its test client and runs the four.

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
- `tests/memory.rs`, in every test run: what a session holds on the heap when it listens and when it speaks, what a session of text holds after ten messages and after a hundred, and that nothing is left of sessions that are over. A session that holds much less than the file says fails too: say the good news in the file. The thousand calls and the thousand chats of every night are the tests in it that are ignored:

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

`--cpu` keeps the measurement on one processor, with `taskset`, and is worth using on a machine whose cores are not alike: the numbers then repeat to about 1%, when nothing else runs on that processor meanwhile. A check that fails measures again, three times in all at most, because whatever else a machine does can only make a measurement worse.

The runtime pins the threads of its shards only where it is told their processors, which the daemon's file can say (`processors`, on Linux; [decision 0003](docs/decisions/0003-shard-pinning.md)). The `waveport` command has one shard and no such setting. On a machine whose cores are not alike it is kept on the fast ones the same way, from outside: `taskset -c 0-11 waveport dev` on a processor whose performance cores are numbered 0 to 11. An Intel processor of that kind lists them in `/sys/devices/cpu_core/cpus`.

The same machine is not always the same machine, and three things about a laptop change what it measures. On its battery, the laptop these records began with takes 1.8 times as long over the reference work as on mains, and over twice as long over a session. With a power profile that balances speed against energy it takes as long over the reference work as with the profile for performance, and a quarter more of a thread for a session, which works in bursts and is done before the processor is at speed. And straight after a build, while it is warm, the reference work takes it a quarter longer at one end of a run than at the other. So a record says how long the reference work took, how the machine was powered and how the speed of its processor was governed. A check refuses a machine whose processor is governed otherwise, before it measures, and one that takes a tenth longer or shorter over the reference work: bring the machine back to that state, or make a new record. Record on mains, with the power profile you measure with, a few minutes after the last build and with the machine left alone: `perf record` refuses a run in which the reference work moved by more than 3%. The record is named after the machine's processor, and says nothing else of the machine than those three things and the processor it was measured on. Make a new record in the pull request that changes what a session costs, and say why it changed.

The share of every night is measured three times, on three runners, and each measurement says in its summary which processor it ran on and what it measured. Runners are not alike: in the first twelve measurements a session took 0.20% of its thread on one processor and 0.27% on another, and the ratio was between 0.68 and 0.77 on all three. So when a measurement fails, read the three together, and against what the same processor measured on other nights, before anything is changed:

- **The reference work took longer at one end of the run than at the other** ("the machine was not left alone"). Nothing was measured: run that measurement again.
- **One or two of the three fail.** Look at what the processor of the one that failed measured on other nights. If its ratio was as high then, that is the runner: set what is held again, as below, without waiting for the ten nights. If it was not, run that measurement again, and if it fails again take it for the next case.
- **All three fail.** That is the code. Find the change with `cargo xtask perf check` on a machine that has a record, where the numbers repeat to about 1%.

What `[density]` holds was set from the first runs. It is set again once ten nights have been measured, which is thirty measurements:

- Read `density.ratio`, `density.share_percent`, `poll.over_budget_share` and the processor out of the summaries, or out of the logs of the jobs.
- Of the ratio and the share, hold the one that moves less from runner to runner, at its median. A run may be over it by twice what the largest measurement was over the median, as a share of the median, rounded up to the next twentieth, and by a tenth at the least.
- If the measurements of one processor lie more than a tenth from those of another, hold each processor to its own number rather than allow all of them more.
- If no measurement of `poll.over_budget_share` is more than twice the median, hold it too, at three times the median. Otherwise it goes on being printed and is not held.

## Spikes

`spikes/` holds throwaway experiments that answer a question before product code depends on the answer. Each is its own Cargo workspace with its own lockfile, so it can use dependencies the product workspace would reject, and none of the gates build it. A spike's conclusions are written up under `docs/decisions/` and its raw numbers under `perf/`; the code is deleted once the product has its own benchmark for the same question. There are two at the moment, both of phase 2: `spikes/webrtc`, which held an endpoint on `str0m` against two browsers ([decision 0005](docs/decisions/0005-webrtc-endpoint.md)), and `spikes/realtime`, which drove a speech-to-speech provider through every command the port needs ([decision 0007](docs/decisions/0007-speech-to-speech-port.md)). What each found is in its `FINDINGS.md`, with the recordings and tables it rests on beside it, because nothing in `perf/` reads them. Each goes when the product holds the same against the provider or the browser in its own tests. A recording a spike keeps holds no key, no audio of a person and no address of the machine it was made on: look before you add one. The density spike of phase 1 was deleted when `cargo xtask perf` could measure the runtime itself, and its record is [decision 0001](docs/decisions/0001-density-spike.md).

## Pull requests

Pull requests are squash-merged, and the title becomes the commit subject. The changelog is generated from those subjects, so a title follows [Conventional Commits](https://www.conventionalcommits.org/):

```
type(scope)!: subject
```

- **type**: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`, `ci`, `chore` or `revert`.
- **scope** (optional): a crate name without `waveport-` (`domain`, `runtime`, `sim`, `xtask`, …), or one of `ci`, `deps`, `docs`, `release`, `sdk-js`.
- **`!`** (optional): the change breaks a public API. It is what allows a breaking change to a preview crate.
- **subject**: lower-case imperative, no full stop at the end.

Check a title locally with `cargo xtask check-title 'feat(domain): add epoch counters'`.

## Licence

Waveport is licensed under Apache-2.0. By contributing you agree that your contribution is licensed under the same terms.
