# Waveport roadmap

Waveport is an open-source platform for real-time voice and multimodal AI agents, written in Rust. One runtime is both the agent framework and the system that operates it: it runs conversations, terminates media, dispatches work across a fleet, journals every session, and ships the models, console and connectors needed to run all of it on your own servers.

**Status (15 November 2026):** phase 0 (foundations) is complete: its gates pass in CI on Linux (x86-64 and arm64), macOS and Windows. Phase 1 has started. Its first item, the density spike, is done, and what it decided is in [decision 0001](docs/decisions/0001-density-spike.md). The session kernel is written: it takes turns, speaks, keeps the conversation, runs tool calls, lets a second agent answer a call or take the floor, closes, and is rebuilt from its journal. All six of its scenarios pass, and its ten invariants hold on 3,600 random sessions and about 200,000 states of small sessions in every test run, and on 1.2 million sessions and 6 million states every night. The audio path is written too: Opus and G.711, the change of rate, the inbound path and the outbound path, with no allocation per frame and its cost per frame held by a gate. The ports are written, with the session planner and what adapters share. The runtime is written: shards, the session actor that drives the kernel and the audio path through the ports, and the compute plane that shares the listening models. With a fake in every port it holds the six scenarios and the ten invariants on 100 sessions made up from seeds in every test run and on 10,000 every night, with parts that fail, lines that lose packets and processes that die and go on from their journal. The local models, the providers and the first voice loop come next.

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
6. `waveport-runtime`: shards, session actor, cascaded driver, compute plane with cross-session batching, supervision and shutdown (done, with the journal's barrier before a tool that has an effect, and a session that goes on from its journal after its process died. Not yet: snapshots, which come with the local journal; threads pinned to cores and the gate on how long a session may run at a time, which come with the performance gates).
7. Local models: voice-activity detection and Smart Turn v3.2 end-of-turn detection, one instance shared by all sessions.
8. Provider adapters: speech-to-text for OpenAI, Deepgram and AssemblyAI; one LLM adapter for OpenAI-compatible APIs; text-to-speech for Cartesia and ElevenLabs.
9. Local-audio and WebSocket transports.
10. Tools with effect classes and gates (the kernel decides and the runtime runs them, both done; a registry of tools and the first two tools come with the first voice loop).
11. A local journal with snapshots and crash resume.
12. Traces and metrics.

Exit criteria:

- A person can talk to an agent from a laptop.
- All ten kernel invariants hold across thousands of simulated interleavings with fault injection.
- A `kill -9` in the middle of a call is followed by a resumed session.
- Latency, memory and density baselines are recorded and enforced as CI gates.

Gaps closed: ordering races and shutdown hangs; no backpressure; one process per session and its memory growth; no shared model sessions.

### Phase 2 — Real channels (L)

**Goal:** the same agent reachable from a browser, a phone and a text client.

Build:

- WebRTC direct endpoint on `str0m` with HTTP signalling and an adaptive jitter buffer.
- Carrier media-stream transports: Twilio, Telnyx, Plivo, Vonage, Exotel, jambonz.
- Client protocol v1 with an RTVI-compatible gateway, a JavaScript SDK and a playground.
- Text-only sessions over HTTP with server-sent events and over WebSocket.
- Two speech-to-speech adapters with opposite capability profiles (OpenAI Realtime and Gemini Live) and the realtime driver.
- The first tier of provider adapters (listed in the ecosystem document).
- `waveportd`, the server daemon, with HTTP and gRPC APIs.
- DTMF, provider failover, and a nightly conformance kit that tests each adapter's declared capabilities against the live API.

Exit criteria:

- One agent definition answers a browser call, a phone call through a carrier and a text-only chat.
- The conformance kit runs nightly against every shipped provider.
- Spoken-prefix accuracy is measured against client-side recordings on WebRTC and on one carrier.

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
- Seamless re-session for speech-to-speech providers with a session time limit.

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
