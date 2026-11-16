# Waveport

Waveport is an open-source platform for real-time voice and multimodal AI agents, written in Rust. One runtime is meant to be both the agent framework and the system that operates it: it runs conversations, terminates media, dispatches work across a fleet and journals every session.

## Status

**Foundations, the first measurements, the session kernel, the audio path, the ports, the runtime, an agent to talk to, with a choice of providers, a journal that outlasts its process, and a session that says what it did and is held to what it may cost.** The repository holds the workspace, the quality gates, the test and benchmark harnesses, and the first product code. The session kernel decides who has the floor, what the agent says and how much of it was heard, which tool calls run and what became of each, and what a session is after its process died. The audio path turns the packets of a transport into audio in order and the audio of speech synthesis into frames handed over at the pace they are heard, and knows how much of a response has been heard. Both are libraries with no I/O. The ports are the traits that a provider, a transport, a store or a tool implements, each with a statement of what it can do, and a planner that decides from those statements how a session is run and names what it will do less well. The runtime runs sessions: each is one task on a thread it never leaves, which drives the kernel and the audio path through the ports, keeps a journal and goes on from it after its process has died, while the models that listen are shared by all sessions. Behind the ports there is every part a voice agent needs, and for some a choice: the two models that listen for speech and for the end of a turn, speech recognition by OpenAI, Deepgram or AssemblyAI, any language model behind an OpenAI-compatible API, speech synthesis by Cartesia or ElevenLabs, and for a transport the microphone and loudspeaker of the machine or a WebSocket. The `waveport` command puts them together into an agent you can talk to, at the machine it runs on or from another terminal. It keeps its calls in a journal in a file: a call whose process was killed is gone on with by the next one, with `--resume last`.

Phase 1 of the [roadmap](ROADMAP.md) is under way. Its first step, a throwaway [density spike](docs/decisions/0001-density-spike.md), chose the inference backend and the allocator and measured what one voice session costs. The kernel is the second: it passes the six scenarios written before it, and its invariants are checked on random sessions and on every path through small ones. The audio path is the third: Opus and G.711, the change of rate, a reorder buffer, pacing and the playout ledger, with no allocation per frame. The ports are the fourth, with the plumbing adapters share: a stream helper, WebSocket and HTTP clients and a parser for server-sent events. The runtime is the fifth: shards, the session actor and the compute plane, tried every night on ten thousand sessions made up from seeds, with parts that fail, lines that lose packets and processes that die. The first voice loop is the sixth: the listening models, the first three providers, the local-audio transport and the `waveport` command, with a whole call run from a script on every platform against servers that stand in for the providers. The seventh adds three providers (Deepgram and AssemblyAI to hear, ElevenLabs to speak), a transport whose far end connects over a WebSocket, with its client, and a workflow that runs the tests against the providers themselves every night. **The adapters for Cartesia, Deepgram, AssemblyAI and ElevenLabs are written from the providers' documentation and have not been tried against the providers yet**; the ones for OpenAI have. The eighth is the journal on disk: one file, one sync for everything that is stored together, a session's state kept beside its records, and a resume token that is the right to go on with a call. It is tried on a disk that loses its power in the middle of a commit, and the command itself is killed at six places in a call and has to go on with it. The ninth is what a session tells and what it is held to: traces and metrics for a collector, a report of each call, and gates on how long the parts of a turn take, what a session holds in memory and how much of a thread it takes. With that the code of phase 1 is written. **Phase 1 is not done until its exit criteria have been shown**, and two things still wait for a person: talking to the agent, and keys for four of the providers. One target of the design is not met: a session that listens holds 528 to 586 KB, above the 300 KB asked of the usual one (the [architecture](docs/architecture.md), section 12, says why and what the choice is).

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
| `crates/core/waveport-adapter-kit` | What adapters share: a stream helper, WebSocket and HTTP clients, connections kept for the next response, server-sent events, retry classes |
| `crates/core/waveport-runtime` | Application layer: runs sessions. Shards, the session actor, and the compute plane that shares the listening models among sessions |
| `crates/core/waveport-control` | Application layer: runs the fleet |
| `crates/core/waveport-protocol` | Wire formats: client protocol, control API, journal records. Depends on no other crate |
| `crates/adapters/models/onnx` | The models that listen, run by ONNX Runtime: Silero VAD and Smart Turn |
| `crates/adapters/stt/openai` | Speech recognition by OpenAI's realtime transcription |
| `crates/adapters/stt/deepgram` | Speech recognition by Deepgram's live transcription |
| `crates/adapters/stt/assemblyai` | Speech recognition by AssemblyAI's streaming transcription |
| `crates/adapters/llm/openai-compat` | Language models behind an OpenAI-compatible chat completions API |
| `crates/adapters/tts/cartesia` | Speech synthesis by Cartesia, with the time of every word |
| `crates/adapters/tts/elevenlabs` | Speech synthesis by ElevenLabs, with the time of every character |
| `crates/adapters/transport/local-audio` | The microphone and loudspeaker of this machine, or a caller who follows a script |
| `crates/adapters/transport/websocket` | A far end that connects over a WebSocket, and the client for it. For this machine and networks that are trusted |
| `crates/adapters/store/memory` | A journal that lasts as long as its process |
| `crates/adapters/store/journal` | A journal in a file on this machine, from which a session goes on after its process died |
| `apps/waveport-cli` | The `waveport` command, where the parts are put together |
| `crates/testing/waveport-sim` | Deterministic simulation: seeded runs with a virtual clock |
| `crates/testing/waveport-kernel-sim` | The session kernel under test: scenarios explored along every path, random sessions, every path through small sessions, and recorded sessions replayed, with the invariants checked at every step |
| `crates/testing/waveport-alloc-counter` | Counting allocator, to prove a code path does not allocate |
| `crates/testing/waveport-testkit` | Fakes of every port, and the checks every implementation of a port has to pass |
| `benches/` | Wall-clock and instruction-count benchmarks |
| `xtask/` | The repository's gates, run as `cargo xtask <command>` |
| `assets/` | The lock file that pins model files; `cargo xtask fetch-assets` downloads them |
| `perf/` | What the performance gates hold the code to (`targets.toml`), the record of a machine (`baselines/`), and what the density spike measured (`spike/`) |

`waveport-control` is a documented skeleton today. Further adapters and applications, and the SDKs, are added in the phases that need them.

## Building and checking

You need [rustup](https://rustup.rs/); the toolchain is pinned and installs itself. You also need CMake and a C compiler, and on Linux the ALSA headers (`sudo apt-get install libasound2-dev` on Debian and Ubuntu).

```sh
cargo build --workspace
cargo xtask ci --fast    # format, policy, Clippy and tests
cargo xtask ci           # every gate
```

[CONTRIBUTING.md](CONTRIBUTING.md) lists the gates, the extra tools the slower ones need, and the rules for layers, lints, `unsafe` code, dependencies and pull-request titles.

## Talking to an agent

Told nothing else, the agent hears through OpenAI, thinks with an OpenAI model and speaks through Cartesia, so it needs a key for each, in the environment. Wear headphones: nothing takes the agent's voice out of what the microphone hears, and an agent that hears itself stops speaking.

```sh
cargo xtask fetch-assets         # the listening models and the library that runs them, once
export OPENAI_API_KEY=…
export CARTESIA_API_KEY=…
cargo run --release -p waveport-cli -- dev --greeting "Hello! What can I do for you?"
```

The command prints what it heard, what the agent said and how long the first sound of each answer took. Ask it what time it is, or tell it goodbye: those are its two tools. Ctrl-C ends the call.

| Command | What it does |
|---|---|
| `waveport dev` | An agent on this machine's microphone and loudspeaker. `--half-duplex` is for loudspeakers without headphones: the microphone is silenced while the agent speaks, so the agent cannot be interrupted |
| `waveport run --script <file>` | The same agent with a caller who follows a script of recordings and silences. `--record <file>` keeps what the caller was played |
| `waveport serve` | The same agent for callers who are somewhere else: it takes calls over a WebSocket, at `127.0.0.1:9283` unless `--listen` says otherwise, as many at a time as come. Nothing asks a caller who they are, so it takes calls from other machines only with `--exposed` |
| `waveport client` | Calls an agent that `waveport serve` runs, from this machine's microphone and loudspeaker or from a script |
| `waveport devices` | Lists the sound devices, for `--microphone` and `--loudspeaker` |

`--stt openai|deepgram|assemblyai` and `--tts cartesia|elevenlabs` say who hears and who speaks. Each provider brings its own model, voice and the name of the variable that holds its key (`DEEPGRAM_API_KEY`, `ASSEMBLYAI_API_KEY`, `ELEVENLABS_API_KEY`):

```sh
export DEEPGRAM_API_KEY=…
export ELEVENLABS_API_KEY=…
cargo run --release -p waveport-cli -- dev --stt deepgram --tts elevenlabs
```

`--llm-url` points the agent at any server that speaks the chat completions protocol, a model on this machine among them. `waveport dev --help` lists the rest. A key is never an argument: `--stt-key-env`, `--llm-key-env` and `--tts-key-env` name the variables that hold them.

`--otlp <address>` tells an OpenTelemetry collector what the calls do, as traces and metrics over HTTP: one trace for each turn, with the model's responses and the tool calls inside it, and how long each part took. `--report <file>`, for `dev` and `run`, writes the same as JSON when the call is over. Neither holds a word of the call. What a collector asks for in headers is read from `OTEL_EXPORTER_OTLP_HEADERS`. This has been tried against a stand-in for a collector and not yet against one that somebody runs.

The command keeps its calls in `.waveport/journal.redb`, in the folder it is run in, with what was said in them; the file is its owner's alone. A call that broke off, because the command was killed or the machine went down, is gone on with from there:

```sh
cargo run --release -p waveport-cli -- dev --resume last
```

The agent then knows what was said before, is told that something was lost, and gives the answer it still owed. To see it, kill the command in the middle of an answer (`kill -9`, not Ctrl-C, which ends the call) and run the line above. A call that ended is taken out of the file; one that broke off stays there until it has been gone on with to its end, and deleting the file lets go of them all. `--journal <file>` keeps the calls somewhere else and `--no-journal` keeps none. A call over a WebSocket cannot be gone on with yet.

## Licence

Licensed under the [Apache License, Version 2.0](LICENSE). Copyright 2026 The Waveport Authors.
