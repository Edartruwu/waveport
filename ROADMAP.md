# Waveport roadmap

Waveport is an open-source platform for real-time voice and multimodal AI agents, written in Rust. One runtime is both the agent framework and the system that operates it: it runs conversations, terminates media, dispatches work across a fleet, journals every session, and ships the models, console and connectors needed to run all of it on your own servers.

**Status (16 November 2026):** phase 0 (foundations) is complete: its gates pass in CI on Linux (x86-64 and arm64), macOS and Windows. Phase 1 has started. Its first item, the density spike, is done, and what it decided is in [decision 0001](docs/decisions/0001-density-spike.md). The session kernel is written: it takes turns, speaks, keeps the conversation, runs tool calls, lets a second agent answer a call or take the floor, closes, and is rebuilt from its journal. All six of its scenarios pass, and its ten invariants hold on 3,600 random sessions and about 200,000 states of small sessions in every test run, and on 1.2 million sessions and 6 million states every night. The audio path is written too: Opus and G.711, the change of rate, the inbound path and the outbound path, with no allocation per frame and its cost per frame held by a gate. The ports are written, with the session planner and what adapters share. The runtime is written: shards, the session actor that drives the kernel and the audio path through the ports, and the compute plane that shares the listening models. With a fake in every port it holds the six scenarios and the ten invariants on 250 sessions made up from seeds in every test run and on 10,000 every night, with parts that fail, lines that lose packets and processes that die and go on from their journal. The first voice loop is closed: the two models that listen, the first provider of each kind, the sound devices of the machine and the `waveport` command make an agent to talk to, and a whole call is run from a script in every test run on all four platforms. There is now a choice of providers (OpenAI, Deepgram or AssemblyAI to hear, Cartesia or ElevenLabs to speak) and a second transport, over a WebSocket. Every provider adapter has been held against its provider: the ones for OpenAI as they were written, the other four on 16 November 2026, which found two things to change. The journal is on disk: a session keeps its records and its state in a file, and the `waveport` command, killed at six places in a call in every test run on all four platforms, goes on with the call in its next process, with a tool that acts having acted once. Traces and metrics are written: a session tells a collector what it does, one trace for each turn, and the `waveport` command writes a report of a call. The performance gates are written: every test run holds whole calls on the session actor to how long the parts of a turn may take and to what a session may hold, every night a thousand calls have to give back every byte, and `cargo xtask perf` records and checks, on one machine, how much of a thread a session takes. All of the code of phase 1 is written. The phase is done when its exit criteria have been shown, and the first of them waits for a person to talk to the agent. Open from the measurements: the threads of the shards are not pinned, and are pinned when the daemon of phase 2 gives a process more than one shard, since what was measured is a gain on a laptop whose cores are not alike, and runners whose cores are alike did as well with nothing pinned ([decision 0003](docs/decisions/0003-shard-pinning.md)); and a session that listens holds what its buffers cost, 466 to 524 KB, which is what its target is restated at: when sessions can be parked, the 8 s it looks back on are borrowed for a turn and its 3 s of speech for an answer from a pool of the shard's, and it holds about 90 to 150 KB ([decision 0002](docs/decisions/0002-idle-session-memory.md), and section 12 of the [architecture](docs/architecture.md)). Phase 2 has started, with what its channels need of the core. Its stories are told to the kernel before the kernel knows them: ten scenarios, of which the three in writing pass and seven wait for what they describe. A session is run through the driver of its shape, and the second shape is there: a session of text, which thinks with a language model, has its tools and neither hears nor speaks. The kernel is the one that takes calls, unchanged. What such a session writes is told as it is written to whoever follows it, what is on record of an answer that was typed over is what its reader was given, and `waveport chat` is one at a terminal, run in every test run on four platforms. Four of the six scenarios are told in writing, and with chats made up from seeds they are 200 sessions of text in every test run and 8,000 every night. It holds 22 KB after ten messages and 72 KB after a hundred, inside what is asked of a session without a voice, and every night a thousand chats have to give back every byte.

Companion documents:

- [Architecture and technical spec](docs/architecture.md)
- [Ecosystem inventory and gap traceability](docs/ecosystem.md)

## Why waveport exists

Research into both projects in November 2026 found that Pipecat and LiveKit Agents now offer nearly the same features and tie on the only benchmarks that ran one agent on both. What each still lacks is structural, and the two lists barely overlap.

- **Pipecat** is Python-only and runs one OS process per session. Its queues are unbounded, and its dominant bug class in 2026 is ordering races between interruptions, turn detection, tool calls and context writes. It has no open-source dispatcher and no session resume, and its Flows do not work on speech-to-speech models.
- **LiveKit** has dispatch and SIP built in, but runs one heavy subprocess per room, binds a session to one participant, and supports WebRTC as its only media transport. Its best turn-detection and interruption models, noise cancellation, observability, simulations and channel connectors need LiveKit Cloud.

Waveport closes both lists with one design. Two Rust voice-agent projects already exist (flowcat and rustvani), and both follow Pipecat's frame-pipeline design. Waveport's differentiation is therefore not "Pipecat in Rust": it is a different session kernel, plus the fleet, durability and open-model layers neither incumbent offers in open source.

## Principles

Each principle answers a documented gap.

1. **Correct by construction.** A deterministic, I/O-free session kernel decides everything about turns, interruptions, tools and context. The incumbents' races become impossible states instead of bugs to patch.
2. **Sessions are tasks, not processes.** Thousands of sessions share a process, memory per session is bounded, and each model is loaded once.
3. **Durable by default.** Every session is journaled, so it can resume after a reconnect or a crash and migrate to another node during a drain.
4. **Open all the way down.** Turn detection, interruption classification and noise suppression run locally with open weights. The dashboard and the connectors are self-hosted.
5. **Any transport, any shape.** Voice, text-only, multi-party, cascaded, speech-to-speech and hybrid sessions are one model with no special cases.
6. **One core, every language.** Bindings wrap the same kernel, so no language trails another.
7. **A stable API.** Stability tiers, semver checks in CI and long deprecation windows from the first release.

## Scope decisions

| Decision | Choice | Reason |
|---|---|---|
| Repository | One Cargo workspace holding core, adapters, apps, SDKs, models and deployment files | The whole ecosystem is built and versioned together |
| Authoring surface | Rust API first; Python and TypeScript bindings and sandboxed WASM plugins later, all over the same core | Removes "Python-only" and "Node.js trails Python" by construction |
| Media | The runtime terminates media itself (WebRTC, SIP, WebSocket, MoQ) and can join LiveKit rooms; its own multi-node SFU comes last | The SFU is the largest single item and nothing else depends on it |
| Licence | Apache-2.0 for everything, including the console, connectors and model weights | The incumbents' gaps are mostly things they gate commercially |
| Schedule | Phases with exit criteria and relative sizes, no calendar dates | Dates depend on team size, which this document does not assume |

## Non-goals

- A hosted cloud service, a hosted inference gateway, or selling phone numbers. Waveport connects to any carrier and any model provider directly.
- Video over SIP.
- Client-side video effects such as background blur.
- Re-implementing every one of the 246 integrations Pipecat lists. Waveport ships native adapters for the main providers and a bridge that reuses existing Pipecat and LiveKit plugins out of process.

## How the roadmap is organised

- **Sizes** are relative: S, M, L, XL.
- **A phase is done** only when every exit criterion has been demonstrated, not when its code is merged.
- **Order.** Phases 0 to 3 are strictly sequential. Phases 4 to 7 are independent tracks once phase 3 is done and can run in parallel or be reordered. Phase 8 needs the agent API from phase 4. Phase 9 needs the WebRTC work from phase 2 and the fleet from phase 3. Phase 10 closes the 1.0 release.

```
0 Foundations
└─ 1 Kernel and first voice loop
   └─ 2 Real channels
      └─ 3 Fleet and durability
         ├─ 4 Agent layer ──────── 8 Polyglot and extensibility
         ├─ 5 Open intelligence
         ├─ 6 Native telephony
         ├─ 7 Console and evals
         └─ 9 Media plane
                                   10 Breadth and 1.0 (after all of the above)
```

## Phases

### Phase 0 — Foundations (S)

**Goal:** a repository where every quality gate exists before the first line of product code.

Build:

- Cargo workspace on edition 2024, toolchain pinned to Rust 1.100.0, minimum supported Rust version 1.98.
- Workspace lint tables, formatter and Clippy configuration, licence and advisory checks.
- `xtask arch-check`, which fails the build if a domain crate depends on an async runtime or an adapter, or if one adapter depends on another.
- CI on Linux (x86-64 and arm64), macOS and Windows: format, lint, test, docs, licence check, semver check, feature powerset.
- Skeletons of the simulation harness, the benchmark harness and the allocation-counting test helper.
- Release tooling, changelog convention and the three stability tiers (stable, preview, experimental).

Exit criteria:

- The empty workspace builds and tests on all three operating systems with every gate active.

Gaps closed: Windows as a second-class platform; API churn (the stability policy is in place from day one).

### Phase 1 — Kernel and first voice loop (L)

**Goal:** prove the kernel design on a real conversation, and retire the three largest risks before building anything on top.

Build, in this order:

1. **Density spike** (done; see [decision 0001](docs/decisions/0001-density-spike.md)). Synthetic sessions with real Opus, batched voice-activity detection and end-of-turn inference. It chose `ort` over `tract` and `mimalloc` over jemalloc, and replaced the estimate of 100 sessions per core with a measured planning figure of 60. The 1,000-session run it was meant to include needs a machine that can keep every core busy; it moves to the density gate at the end of this phase.
2. **Six kernel scenarios** as executable tests (done, and all six pass; in `crates/core/waveport-domain/tests/scenarios/`), written before any real adapter: the four Pipecat race classes, a hybrid front/back agent, and a three-party conversation with speaker attribution. A scenario that needs a kernel edit instead of a policy is a redesign signal.
3. `waveport-domain`: identities, events, commands, the session kernel and its policies (done: turn-taking, responses, the conversation, the lifecycle, tool calls with effect classes and gates, a second agent that answers a call or takes the floor, snapshots and recovery).
4. `waveport-media`: inbound and outbound audio paths, reorder buffer, pacer, playout ledger, G.711, frame pool (done, with Opus and a resampler among 8, 16, 24 and 48 kHz. Not yet: noise suppression, G.722, and the framing the voice-activity model wants, which comes with the compute plane).
5. `waveport-ports`: every port trait, the capability types and the session planner (done, with `waveport-adapter-kit`, which holds what adapters share: a stream helper, WebSocket and HTTP clients, server-sent events. Also done: `SessionControl`, the port an application drives running sessions through. Not yet: the speech-to-speech port, the bus, retrieval and dispatch, which later phases need).
6. `waveport-runtime`: shards, session actor, cascaded driver, compute plane with cross-session batching, supervision and shutdown (done, with the journal's barrier before a tool that has an effect, and a session that goes on from its journal after its process died. Snapshots came with the local journal: a session has its journal keep its state every 256 records or every minute. Not here: threads pinned to cores, which moved to the daemon of phase 2 ([decision 0003](docs/decisions/0003-shard-pinning.md)). The performance gates measured what pinning is worth: a twentieth to a fifth of what a session takes of its thread on a laptop whose cores are not alike, and runners whose cores are alike did as well with nothing pinned. How long a session runs at a time is measured and recorded by `cargo xtask perf` and not held by a gate, because left to the system on that laptop it says more about the machine than about the code. On the runners 3 runs in ten thousand were over the budget in eleven measurements of twelve, and 6 in the one on the slowest processor, and whether it is held there is decided with the ten nights of measurements that set the density gate again).
7. Local models: voice-activity detection and Smart Turn v3.2 end-of-turn detection, one instance shared by all sessions (done: Silero VAD v6.2.3 and Smart Turn v3.2 on ONNX Runtime, in `waveport-models-onnx`).
8. Provider adapters: speech-to-text for OpenAI, Deepgram and AssemblyAI; one LLM adapter for OpenAI-compatible APIs; text-to-speech for Cartesia and ElevenLabs (done: all six, and each held against its provider. Cartesia, Deepgram, AssemblyAI and ElevenLabs were written from the providers' documentation and first tried against them on 16 November 2026, which changed how ElevenLabs' character times and AssemblyAI's word times are read; a workflow runs every adapter's tests against its provider each night, for the providers whose keys the repository holds).
9. Local-audio and WebSocket transports (done: local audio, on the sound devices of the machine or with a caller who follows a script, and a transport whose far end connects over a WebSocket, with its client. Its protocol, `waveport.ws.v0`, is for this machine and trusted networks, and is not the client protocol of phase 2).
10. Tools with effect classes and gates (done: the kernel decides, the runtime runs them, and a registry holds them. The `waveport` command has the first two: one that tells the time, and one that ends the call once the agent has been heard saying goodbye).
11. A local journal with snapshots and crash resume (done: `waveport-store-journal`, one `redb` file with one commit for everything that is stored together, fencing, a session's state beside its records, and a resume token that is stored as its hash. It is tried on a disk that loses its power at a write a seed picks, 60 times in every test run and 20,000 times every night. Not yet: a call over the WebSocket transport that is gone on with, which needs the client protocol of phase 2; a command that lists the calls that broke off; and the move to `redb` 4.6.0, which mends a repair that can open a damaged file as an empty one and is taken once it is a week old).
12. Traces and metrics (done: `waveport-telemetry-otel` sends OpenTelemetry traces and metrics over HTTP, one trace for each turn, with no word of a call in them; `waveport --otlp` switches it on, and `--report` writes what a call came to as JSON. Tried against a stand-in for a collector, not yet against one that somebody runs. With it came the performance gates: `perf/targets.toml` holds the latency and memory figures that every test run checks on whole calls, a soak of a thousand calls and the share of a thread a session takes are run every night, and `cargo xtask perf record` and `perf check` keep the record of one machine in `perf/baselines`).

The `waveport` command puts these together (done: `waveport dev` at the microphone, `waveport run --script` for a call that follows a script, which every test run makes on four platforms against servers that stand in for the providers).

Exit criteria:

- A person can talk to an agent from a laptop.
- All ten kernel invariants hold across thousands of simulated interleavings with fault injection.
- A `kill -9` in the middle of a call is followed by a resumed session.
- Latency, memory and density baselines are recorded and enforced as CI gates.

Where the criteria stand on 15 November 2026. The second and third are shown in every test run, and more widely every night. The fourth is shown, with what it found: every figure that phase 1 can measure is inside its target, the memory of a session that listens among them now that its target is restated at what its buffers cost ([decision 0002](docs/decisions/0002-idle-session-memory.md)); and the density gate of every night holds a ratio on a shared runner, while the numbers themselves are held on one machine, by hand. The first has been shown by a script in every test run and not yet by a person at a microphone.

Gaps closed: ordering races and shutdown hangs; no backpressure; one process per session and its memory growth; no shared model sessions.

### Phase 2 — Real channels (L)

**Goal:** the same agent reachable from a browser, a phone and a text client.

Build:

- WebRTC direct endpoint on `str0m` with HTTP signalling and an adaptive jitter buffer ([decision 0005](docs/decisions/0005-webrtc-endpoint.md): `str0m` with a crypto provider of our own, so that no C enters the tree; redundant audio where a browser offers it and Opus's own in both directions; ICE over TCP for networks that block UDP; and the jitter buffer is the reorder buffer of phase 1, given the redundancy).
- Carrier media-stream transports: Twilio, Telnyx, Plivo, Vonage, Exotel, jambonz.
- Client protocol v1 with an RTVI-compatible gateway, a JavaScript SDK and a playground ([decision 0006](docs/decisions/0006-rtvi-gateway.md): an application on Pipecat's JavaScript client connects by changing an address, and is held to that by the stock client in every test run).
- Text-only sessions over HTTP with server-sent events and over WebSocket (done: the session itself, which has a language model and its tools and nothing that hears or speaks, on the same kernel with nothing changed in it, and `waveport chat`, which is one at a terminal. Not yet: over HTTP and over WebSocket, which come with the daemon).
- Two speech-to-speech adapters with opposite capability profiles (OpenAI Realtime and Gemini Live) and the realtime driver ([decision 0007](docs/decisions/0007-speech-to-speech-port.md): a port of intents, measured against OpenAI and read against Gemini, for which there was no key. With it comes the part of phase 4's re-session that a call needs to outlive a provider's connection: Gemini ends one after about ten minutes).
- The first tier of provider adapters (listed in the ecosystem document).
- `waveportd`, the server daemon, with HTTP and gRPC APIs, and with the number of shards and the processors their threads are pinned to as settings ([decision 0003](docs/decisions/0003-shard-pinning.md)).
- DTMF, provider failover, and a nightly conformance kit that tests each adapter's declared capabilities against the live API.

Added on 16 November 2026, when the plan for the phase was held against what the leading frameworks ship. Each can be struck without the others:

- Text sessions in the two protocols chat front ends speak: OpenAI's chat completions as a stream, and AG-UI 1.0.
- Speech recognisers that report turns (Deepgram Flux, and AssemblyAI's model for agents), with a provider's word on a turn taken as an estimate by the kernel's own decision.
- Replies begun before the turn is certain, and held until it is ([decision 0004](docs/decisions/0004-session-shapes.md)). Off unless an agent asks for it, and not on a speech-to-speech model, whose responses cannot be held.

What the kernel learns in this phase, and how sessions of other shapes are run, is in [decision 0004](docs/decisions/0004-session-shapes.md).

Exit criteria:

- One agent definition answers a browser call, a phone call through a carrier and a text-only chat.
- The conformance kit runs nightly against every shipped provider.
- Spoken-prefix accuracy is measured against client-side recordings on WebRTC and on one carrier.

Where the criteria stand on 16 November 2026: none is shown yet. Three things they need are not in the repository's hands. Two criteria ask for a call through a real carrier, and there is no account with one: the carrier transports are written from the carriers' documentation and held against simulators, and say so. The nightly kit asks for keys for Anthropic and for Gemini among the repository's secrets, and the half of decision 0007 that is about Gemini waits for the same key.

Gaps closed: no non-WebRTC media transport; room-less text agents; self-hosted carrier connector.

### Phase 3 — Fleet and durability (L)

**Goal:** run many nodes without building your own dispatcher, and never lose a session to a deploy.

Build:

- Dispatcher: node registry, load reports, job offers, placement, dispatch rules.
- Leases with fencing, a shared journal, and live migration at the next turn boundary.
- Unlimited drain time.
- Access tokens with grants and revocation; quotas.
- Helm chart, Kubernetes operator and autoscaling signals based on session count.
- Webhooks and multi-region routing.

Exit criteria:

- Killing a node in the middle of a call is followed by the session continuing on another node.
- A rolling deploy drops no sessions.
- A session lasting several hours survives a drain.

Gaps closed: no open-source dispatcher; no Kubernetes path; no session resume; drain caps; token revocation.

### Phase 4 — Agent layer (L)

**Goal:** everything needed to write a production agent without leaving the framework.

Build:

- Flow engine that works on cascaded and speech-to-speech models.
- Prebuilt tasks: collect an email address, a postal address, DTMF digits, consent.
- Handoff in-process and across nodes; multi-agent bus; hybrid fast-front and slow-back agents.
- MCP client for tools and resources.
- Structured output; context summarisation with pinned and excluded messages.
- Knowledge retrieval and long-term memory.
- Text-to-speech cache; declarative agent files.
- Seamless re-session for speech-to-speech providers with a session time limit (the part a call needs to outlive a provider's connection is in phase 2, with the realtime driver; what is left here is a change of provider or of settings in the middle of a call).

Exit criteria:

- One flow definition runs unchanged on a cascaded pipeline and on two speech-to-speech models.
- An agent hands a live call to an agent on another node.

Gaps closed: Flows on speech-to-speech; cross-process handoff; built-in retrieval and memory; text-to-speech caching.

### Phase 5 — Open intelligence (XL)

**Goal:** the conversational models the incumbents keep hosted, released with open weights and running offline.

Build:

- An interruption classifier that separates a real barge-in from a backchannel such as "mm-hmm".
- Neural noise suppression.
- End-of-turn detection for 8 kHz telephony audio and for more languages.
- Multi-party floor control with speaker attribution and addressee detection.
- Training code, test sets and a public benchmark for turn-taking.

Exit criteria:

- The open-weights interruption model beats a voice-activity-only baseline on a published test set.
- A three-person conversation is attributed correctly end to end.
- Every model runs with no network access.

Gaps closed: Cloud-only adaptive interruption; no open noise cancellation; multi-party conversation; turn-model language coverage and licence.

This phase is research. It needs labelled audio and training compute, and its exit criteria can slip without blocking other phases, because the ports it fills exist from phase 1.

### Phase 6 — Native telephony (XL)

**Goal:** a SIP endpoint that carriers and PBXs accept without a bridge product in between.

Build:

- SIP server and client roles on `rsipstack`, with call control, RTP sessions and RFC 4733 DTMF written in-repo.
- REGISTER, cold transfer (REFER), warm transfer, hold, early media, SIPREC.
- Answering-machine and voicemail detection; IVR navigation.
- Outbound dialler.
- Asterisk AudioSocket transport; WhatsApp calling connector.

Exit criteria:

- An interop matrix passes against major carriers and open-source PBXs (Asterisk, FreeSWITCH, Kamailio).
- A SIP load test meets the density target.

Gaps closed: native SIP; SIP REGISTER and SIPREC; transport-agnostic call transfer; self-hosted WhatsApp connector.

### Phase 7 — Console and evals (L)

**Goal:** see and test everything on your own servers.

Build:

- Console web application: sessions, transcripts, traces, recordings, latency waterfall, fleet view.
- Replay debugger driven by the session journal.
- Simulated callers, LLM judges and a load generator.
- PII redaction; recordings to object storage; analytics export.

Exit criteria:

- Every session is inspectable self-hosted, with whatever retention the operator chooses.
- An evaluation suite runs simulated calls in CI.

Gaps closed: no self-hosted dashboard; Cloud-only insights and simulations.

### Phase 8 — Polyglot and extensibility (L)

**Goal:** write agents in Python or TypeScript with exactly the features of the Rust API.

Build:

- Python and Node.js bindings.
- Sandboxed WASM plugins on the component model.
- An out-of-process adapter protocol with a Python bridge that hosts existing Pipecat and LiveKit plugins.
- Visual flow builder.
- Migration guides from Pipecat and from LiveKit Agents.

Exit criteria:

- One agent written in Rust, Python and TypeScript passes the same conformance suite.

Gaps closed: non-Python runtime; Node.js parity; a flow builder fit for production.

The adapter protocol and Python bridge can be pulled forward into phase 2 if provider breadth becomes the bottleneck.

### Phase 9 — Media plane (XL)

**Goal:** rooms that span nodes, without a commercial mesh.

Build:

- Rooms and a multi-node SFU, audio first and then video forwarding.
- Recording and streaming out, including plain HTTP upload to on-premise storage.
- Ingest from RTMP, WHIP and RTSP.
- Avatars and vision input.
- Persistent rooms and per-room webhooks.

Exit criteria:

- One self-hosted room spans two or more nodes.
- Recording and ingest work end to end.

Gaps closed: single-node rooms; persistent rooms; outstanding recording and ingest requests.

### Phase 10 — Breadth and 1.0 (L)

**Goal:** a release people can pin for years.

Build:

- Swift, Kotlin, React Native, Flutter and embedded C client SDKs, generated from one Rust client core.
- API client libraries generated from the protocol definitions, including .NET, Go and Java.
- SSO, role-based access control, audit log.
- Air-gapped installation.
- API freeze and a long-term-support policy.

Exit criteria:

- Version 1.0 is released with a written stability guarantee.

Gaps closed: mobile SDK parity; enterprise controls; missing server SDK languages.

## Risk register

| Risk | Why it matters | Mitigation |
|---|---|---|
| The kernel becomes a monolith users cannot extend | There is no "drop in a processor" escape hatch | Six scenarios in phase 1; extension through pure policies |
| Density rests on estimates; a shared process has a wide blast radius | One native-code fault could end hundreds of calls | Phase 1 spike; crash resume from the first milestone; per-session panic isolation; 2,000-session cap per process |
| Providers behave differently from their declared capabilities | Realtime APIs change often and ignore updates silently | Two opposite speech-to-speech adapters in phase 2; nightly conformance kit |
| Dependency churn (`str0m`, `rsipstack`, `ort`) | Each has one maintainer or company and breaks API often | Exact version pins; each sits behind a port with a named fallback |
| The roadmap is large | Phases 5, 6 and 9 are each XL | Phases 4 to 7 are independent, so effort can go where demand is; phase 9 can be deferred indefinitely because waveport interoperates with LiveKit rooms |
| Gap lists age fast | Both incumbents closed most older gaps during 2026 | Re-check the gap tables in the ecosystem document at the start of every phase |

## Changing this roadmap

Phase order and scope change only together with the affected exit criteria. A change that drops a gap from the traceability tables in the [ecosystem document](docs/ecosystem.md) must move it to the non-goals list with a reason.
