# Waveport ecosystem

This document lists every component waveport needs, where it lives in the repository, and which roadmap phase builds it. It then maps each gap found in our November 2026 research on Pipecat and LiveKit to the component that closes it.

**Status (16 November 2026):** what phases 0 and 1 build is built, and phase 2 has started. The [roadmap](../ROADMAP.md) says where each item stands; this document says what there will be, where, and which gap it closes. The gap lists of sections 3 and 4 were checked again on 16 November 2026, as the roadmap asks at the start of every phase.

See also the [roadmap](../ROADMAP.md) and the [architecture spec](architecture.md).

## 1. Components

Phase numbers refer to the roadmap. Locations are relative to the repository root.

### Core runtime

| Component | Location | Phase |
|---|---|---|
| Domain model and session kernel | `crates/core/waveport-domain` | 1 |
| Media path: reorder buffer, resampling, pacing, playout ledger, G.711, DTMF | `crates/core/waveport-media` | 1 |
| Port traits, capability types, session planner | `crates/core/waveport-ports` | 1 |
| Session runtime: shards, actor, drivers, compute plane with batching | `crates/core/waveport-runtime` | 1 |
| Control plane: dispatcher, placement, leases, drain, migration | `crates/core/waveport-control` | 3 |
| Protocol definitions: client protocol, control API, adapter protocol, journal records | `crates/core/waveport-protocol` | 2 |

### Transports

| Component | Location | Phase |
|---|---|---|
| Local audio | `crates/adapters/transport/local-audio` | 1 |
| WebSocket | `crates/adapters/transport/websocket` | 1 |
| WebRTC direct endpoint with HTTP signalling and jitter buffer | `crates/adapters/transport/webrtc`, `apps/waveportd` | 2 |
| Carrier media streams: Twilio, Telnyx, Plivo, Vonage, Exotel, jambonz | `crates/adapters/transport/telephony-ws` | 2 |
| Text-only over HTTP with server-sent events | `apps/waveportd` | 2 |
| Native SIP and RTP | `crates/adapters/transport/sip` | 6 |
| Asterisk AudioSocket | `crates/adapters/transport/sip` | 6 |
| WhatsApp calling | `crates/adapters/transport/telephony-ws` | 6 |
| LiveKit room participant | `crates/adapters/transport/livekit` | 9 |
| Media over QUIC | `crates/adapters/transport/moq` | 9 |

### Model providers

| Component | Location | Phase |
|---|---|---|
| First speech-to-text, LLM (OpenAI-compatible) and text-to-speech adapters | `crates/adapters/stt`, `llm`, `tts` | 1 |
| Speech-to-speech adapters: OpenAI Realtime, Gemini Live | `crates/adapters/realtime` | 2 |
| Tier 1 providers (see section 2) | `crates/adapters/*` | 2 |
| Provider failover and switching | `crates/core/waveport-runtime` | 2 |
| Nightly conformance kit against live providers | `crates/testing/waveport-testkit` | 2 |
| Tier 2 providers (see section 2) | `crates/adapters/*` | 4 onward |
| Local engines: Whisper-class speech-to-text, local text-to-speech | `crates/adapters/stt`, `tts` | 5 |
| Out-of-process adapter protocol and Python bridge for existing Pipecat and LiveKit plugins | `crates/core/waveport-protocol`, `crates/bindings/python` | 8 |

### Open models

| Component | Location | Phase |
|---|---|---|
| Voice-activity detection, batched across sessions | `crates/adapters/models/vad` | 1 |
| End-of-turn detection (Smart Turn v3.2) | `crates/adapters/models/turn` | 1 |
| Interruption policy based on voice activity only | `crates/core/waveport-domain` | 1 |
| Noise suppression baseline (RNNoise-class) | `crates/adapters/models/denoise` | 5 |
| Interruption and backchannel classifier with open weights | `crates/adapters/models/interruption`, `models/` | 5 |
| Neural noise suppression | `crates/adapters/models/denoise` | 5 |
| End-of-turn for 8 kHz telephony and more languages | `models/` | 5 |
| Addressee detection for multi-party sessions | `models/` | 5 |
| Answering-machine and voicemail classifier | `models/` | 6 |
| Training code, test sets, public turn-taking benchmark | `models/` | 5 |

### Agent layer

| Component | Location | Phase |
|---|---|---|
| Agents, tools with effect classes and gates, async tool results | `crates/core/waveport-domain` | 1 |
| Flow engine for cascaded and speech-to-speech models | `crates/core/waveport-domain` | 4 |
| Prebuilt tasks: email, address, DTMF, consent | `crates/core/waveport-runtime` | 4 |
| Handoff in-process and across nodes; multi-agent bus | `crates/core/waveport-runtime`, `crates/adapters/bus` | 4 |
| Hybrid fast-front and slow-back agents | `crates/core/waveport-runtime` | 4 |
| Structured output | `crates/core/waveport-ports` | 4 |
| Context summarisation with pinned and excluded messages | `crates/core/waveport-domain` | 4 |
| MCP client for tools and resources | `crates/adapters/tools/mcp` | 4 |
| Knowledge retrieval and long-term memory | `crates/adapters/knowledge` | 4 |
| Text-to-speech cache | `crates/adapters/store/cache` | 4 |
| Declarative agent files | `apps/waveport-assembly`, read by `apps/waveport-cli` and `apps/waveportd` | 4 |
| Seamless re-session for time-limited speech-to-speech providers | `crates/core/waveport-runtime` | 4 |

### Orchestration and durability

| Component | Location | Phase |
|---|---|---|
| Local journal, snapshots, crash resume | `crates/adapters/store/journal` | 1 |
| Server daemon with HTTP and gRPC API | `apps/waveportd` | 2 |
| Dispatcher, node registry, load reports, placement, dispatch rules | `crates/core/waveport-control` | 3 |
| Leases with fencing, shared journal, live migration, unlimited drain | `crates/core/waveport-control`, `crates/adapters/store/journal` | 3 |
| Access tokens with grants and revocation; quotas | `crates/core/waveport-control` | 3 |
| Webhooks; multi-region routing | `apps/waveportd` | 3 |
| Helm chart and autoscaling signals | `deploy/` | 3 |
| Kubernetes operator | `apps/operator` | 3 |
| Recordings to object storage | `crates/adapters/store/objects` | 7 |

### Telephony

| Component | Location | Phase |
|---|---|---|
| DTMF on WebRTC and carrier streams | `crates/core/waveport-media` | 2 |
| Bring-your-own carrier numbers, inbound and outbound, through carrier streams | `crates/adapters/transport/telephony-ws` | 2 |
| SIP server and client roles, call control, RTP sessions, RFC 4733 DTMF | `crates/adapters/transport/sip` | 6 |
| Trunks, dispatch rules | `crates/core/waveport-control` | 6 |
| SIP REGISTER, cold transfer (REFER), warm transfer, hold, early media | `crates/adapters/transport/sip` | 6 |
| SIPREC and call recording | `crates/adapters/transport/sip` | 6 |
| Answering-machine detection, IVR navigation | `crates/core/waveport-runtime` | 6 |
| Outbound dialler | `crates/core/waveport-control` | 6 |

### Observability and evaluation

| Component | Location | Phase |
|---|---|---|
| Traces, metrics, structured logs with user-supplied log configuration | `crates/adapters/telemetry` | 1 |
| Simulation harness and test kit | `crates/testing/waveport-sim`, `waveport-testkit` | 0–1 |
| Console: sessions, transcripts, traces, recordings, latency waterfall, fleet view | `apps/console` | 7 |
| Replay debugger driven by the journal | `apps/console` | 7 |
| Simulated callers, LLM judges, load generator | `crates/testing/waveport-evals` | 7 |
| PII redaction; analytics export | `crates/core/waveport-runtime` | 7 |

### Clients, bindings and plugins

| Component | Location | Phase |
|---|---|---|
| Client protocol v1 with an RTVI-compatible gateway | `crates/core/waveport-protocol`, `apps/waveportd` | 2 |
| JavaScript SDK and playground | `sdks/js` | 2 |
| React SDK, UI kit, embeddable widget | `sdks/react` | 7 |
| Python and Node.js bindings | `crates/bindings/python`, `node` | 8 |
| Sandboxed WASM plugins | `crates/bindings/wasm-host`, `crates/adapters/tools/wasm` | 8 |
| C ABI | `crates/bindings/c-abi` | 10 |
| Rust client core shared by every client SDK | `crates/client/client-core` | 10 |
| Swift, Kotlin, React Native, Flutter and embedded C SDKs | `sdks/` | 10 |
| API client libraries generated from the protocol definitions (.NET, Go, Java, Python, TypeScript) | `sdks/` | 10 |

### Media plane

| Component | Location | Phase |
|---|---|---|
| Rooms and multi-node SFU, audio first and then video | `crates/core/waveport-control`, `crates/adapters/transport/webrtc` | 9 |
| Recording and streaming out, including plain HTTP upload | `apps/waveportd` | 9 |
| Ingest from RTMP, WHIP and RTSP | `apps/waveportd` | 9 |
| Avatars and vision input | `crates/adapters/*` | 9 |
| Persistent rooms and per-room webhooks | `crates/core/waveport-control` | 9 |

### Developer experience and enterprise

| Component | Location | Phase |
|---|---|---|
| CLI: `init`, `dev`, `console`, `eval`, `deploy` | `apps/waveport-cli` | 1 onward |
| Templates, examples, docs site | `examples/`, `docs/` | 1 onward |
| Coding-agent skills and a docs MCP server | `docs/` | 4 |
| Visual flow builder | `apps/console` | 8 |
| Migration guides from Pipecat and LiveKit Agents | `docs/` | 8 |
| SSO, role-based access control, audit log | `crates/core/waveport-control` | 10 |
| Air-gapped installation | `deploy/` | 10 |

## 2. Provider tiers

Provider choices are proposals until the phase that builds them starts.

| Tier | Speech-to-text | LLM | Text-to-speech | Speech-to-speech |
|---|---|---|---|---|
| First loop (phase 1) | OpenAI, Deepgram, AssemblyAI | OpenAI-compatible (also covers Groq, Together, OpenRouter, Ollama, vLLM and llama.cpp servers) | Cartesia, ElevenLabs | — |
| Tier 1 (phase 2) | Those of phase 1 | Anthropic, Google Gemini | Those of phase 1 | OpenAI Realtime, Gemini Live |
| Tier 2 (phase 4 onward) | Azure Speech, AWS Transcribe, Google Cloud, Speechmatics | AWS Bedrock | Azure Speech, AWS Polly, Google Cloud | AWS Nova Sonic |
| Long tail | Through the adapter protocol and Python bridge (phase 8) | | | |

## 3. Gap traceability: Pipecat

Each row is a gap in Pipecat as of November 2026.

**Checked again on 16 November 2026,** against Pipecat 1.16.0 and its `main`. No row closed, and four moved:

- **Native SIP.** A SIP transport on baresip was merged on 13 November and is in no release yet. It registers, or dials through a trunk, without Daily. Transfer through it is "not supported yet", and it has no build for Windows.
- **Shared model sessions.** 1.16.0 has a helper that loads a local model once for a process, and one of the project's own models uses it. The voice-activity model and the end-of-turn model are still loaded for every pipeline.
- **Text-to-speech caching.** A cache is listed in the documentation as an integration the community maintains, which the project "does not test or officially support". The pull request that would have put one in the core was closed unmerged.
- **Windows.** There is still no statement of what is supported, and the project's CI runs on Linux alone. The WebRTC library it depends on has shipped a build for Windows since October 2026, and fixes for Windows are landing.

Three rows are worded more exactly than they were, and one figure is withdrawn: the 20 to 70 MB a session was said to grow by has no source of the project's own. That it runs one process for a session is its documented practice.

| Gap | Waveport answer | Phase |
|---|---|---|
| Non-Python server runtime | Rust core; Python and Node.js bindings over the same kernel | 1, 8 |
| Open-source dispatcher, worker pool, load balancing | `waveport-control` | 3 |
| Kubernetes guide or Helm chart | Helm chart and operator | 3 |
| Session persistence and resume | Journal and crash resume, then live migration | 1, 3 |
| Flows on speech-to-speech models | Capability-planned flow engine with five strategies | 4 |
| Built-in retrieval or knowledge base | Retrieval port, vector-store adapters, memory | 4 |
| Native SIP without Daily | SIP transport | 6 |
| Transport-agnostic call transfer | Transfer is a domain command; each transport implements it | 6 |
| Observability for more than one session at a time, self-hosted (there are two open tools for a developer to watch one) | Self-hosted console | 7 |
| Shared model sessions across pipelines in one process | Compute plane with one model instance shared by all sessions | 1 |
| Backpressure between processors | Every queue bounded; audio drops, text blocks, control coalesces | 1 |
| Multi-party turn-taking logic (who spoke is told since 1.16.0, where the recogniser tells it) | `Floor` with N holders and an addressee policy; models in phase 5 | 1, 5 |
| Text-to-speech response caching | Cache adapter | 4 |
| Visual builder beyond an editor of flow definitions | Flow builder in the console | 8 |
| Mobile SDK parity; dormant embedded SDK | One Rust client core generating every SDK | 10 |
| Windows support | Windows in CI from the first commit | 0 |
| Cloud assurances (SOC 2, SLA, SSO) | Non-goal as a hosted service. SSO, roles and audit log for self-hosters | 10 |
| One process per session | Sessions as tasks with bounded buffers; flat-memory soak test in CI | 1 |

## 4. Gap traceability: LiveKit

Each row is a gap in LiveKit as of November 2026.

**Checked again on 16 November 2026,** against LiveKit Agents 1.8.12, agents-js 1.10.0 and the server at 1.13.11. No row closed, and one moved:

- **Turn detector.** The default is now a model that listens to audio. The larger one is hosted and metered. The smaller one runs locally in 14 languages, at 140 to 250 MB for each copy that is loaded, under a licence that allows it only together with LiveKit Agents.

Two are on their way: agents of text alone without a room (the server's half is released, the SDK's half is a draft) and handoff across processes (a draft). One is worded more exactly than it was.

| Gap | Waveport answer | Phase |
|---|---|---|
| True multi-party conversation with speaker attribution | Speaker is mandatory on every perception event, turn and message; floor control and addressee detection | 1, 5 |
| Local adaptive interruption; pluggable interruption detector | Interruption classifier is a port; open-weights model | 1, 5 |
| Open-source noise cancellation in the framework (two proprietary models can be had without LiveKit Cloud, each with a licence of one's own) | Noise-suppression port with open models | 5 |
| Multi-node rooms in open source | Multi-node SFU | 9 |
| Cross-process multi-agent handoff | Remote brain over the bus, and session transfer across nodes | 4 |
| Session persistence and durable state | Journal, snapshots, resume, migration | 1, 3 |
| Room-less text-only agents | A session with zero tracks and no transport | 2 |
| Non-WebRTC media transport in open source | WebSocket and carrier media streams | 1, 2 |
| SIP REGISTER, SIPREC | SIP transport | 6 |
| SIP video | Non-goal | — |
| Phone numbers outside the US; outbound on vendor numbers | Non-goal to sell numbers. Any carrier in any country, inbound and outbound | 2, 6 |
| Self-hosted WhatsApp connector | WhatsApp calling connector | 6 |
| Self-hosted dashboard and analytics | Console | 7 |
| Agent insights with self-hosted media; long retention | Console reads the operator's own journal and recordings; retention is the operator's choice | 7 |
| Node.js parity | Bindings wrap one core, verified by a shared conformance suite | 8 |
| Agent hosting regions | Non-goal as a hosted service. Runs in any region; multi-region routing | 3 |
| Built-in vector store or memory | Knowledge and memory adapters | 4 |
| Agent builder for production flows | Visual flow builder producing the same flow definitions as code | 8 |
| A turn detector that may be used outside LiveKit Agents; one for telephone audio | Wider-language and telephony end-of-turn models with open weights | 5 |
| Persistent rooms; per-room webhooks | Media plane | 9 |
| Token revocation when self-hosted | Access tokens with revocation | 3 |
| .NET server SDK | API clients generated from the protocol definitions | 10 |

## 5. Community requests

Requests with real engagement in both projects' issue trackers and forums as of November 2026.

| Request | Source | Waveport answer | Phase |
|---|---|---|---|
| Correctness under concurrency: lost user input, tools cancelled by a backchannel, context that does not match what was spoken, shutdown hangs | Pipecat #5683, #6505, #4996, #5305, #3757 | Kernel invariants 1 to 7 | 1 |
| Share one voice-activity model across pipelines | Pipecat #2050 | Compute plane | 1 |
| Graceful fade on interruption | Pipecat #3985 | Playout stops with a 5 to 10 ms fade | 1 |
| Decouple async tool results from interruption settings | Pipecat #4657 | Tool results live outside the epoch | 1 |
| Speech-to-speech edge cases: tool results re-sent after reconnect, settings updates silently ignored | Pipecat Gemini Live issues; LiveKit #5742 | A tool's result is sent once, whatever reconnects. A setting that cannot change on a live connection is a capability the plan reads, and a refusal is reported with the provider's reason. Nightly conformance kit ([decision 0007](decisions/0007-speech-to-speech-port.md)) | 2 |
| Pin or exclude messages during context summarisation | Pipecat #3989 | Summarisation with pinned and excluded messages | 4 |
| Realtime sessions longer than 60 minutes | Pipecat #3170 | Seamless re-session. The part a call needs to outlive a provider's connection comes with the realtime driver | 2, 4 |
| MCP resources | Pipecat #4284 | MCP client covers tools and resources | 4 |
| Flows on realtime models | pipecat-flows #66 | Flow engine | 4 |
| Fewer false "incomplete" predictions; a narrowband telephony test split | smart-turn #41, #42 | Telephony end-of-turn model and public benchmark | 5 |
| Outbound WhatsApp calls | Pipecat #2811 | WhatsApp calling connector | 6 |
| A simple way to deploy | Hacker News, Thoughtworks Radar | Dispatcher, Helm chart, operator | 3 |
| A calmer upgrade path | Pipecat release notes | Stability tiers and semver gates | 0, 10 |
| Self-hostable adaptive interruption | LiveKit #6033 and forum threads | Open-weights classifier | 5 |
| Open-source noise cancellation for self-hosters | LiveKit forum 1711, #5507 | Noise-suppression models | 5 |
| Licence clarity on bundled model weights | LiveKit #6877 | Apache-2.0 weights; third-party weights listed with their licences | 5 |
| Self-hostable dashboard | LiveKit forum 1046, livekit #1378 | Console | 7 |
| Multi-participant sessions | LiveKit #391 | Multi-party floor control | 5 |
| SIP REGISTER | livekit/sip #524 | SIP transport | 6 |
| Carrier interop: hold, early media, transfers behind proxies; zero-filled inbound audio | livekit/sip #779, forum 1315 | Interop matrix as a phase exit criterion | 6 |
| Custom logger configuration | LiveKit #1611 | Logging accepts a user-supplied configuration | 1 |
| Documented interruption setting ignored; phone-number readings cut off | LiveKit #3515, forum 2519 | Policies are tested against the invariants; telephony end-of-turn model | 1, 5 |
| Backchannel prediction | LiveKit #2907 | Interruption and backchannel classifier | 5 |
| Heavy per-job processes | LiveKit forum 1530 | Sessions as tasks | 1 |
| Drain longer than one hour | LiveKit forum 2252 | Unlimited drain plus migration | 3 |
| Paying voice rates for text-only sessions | LiveKit forum 849 | A text session is a 64 KB task with no media | 2 |
| A hosted inference gateway that times out | LiveKit forum 1718 | Non-goal: adapters connect to providers directly, with failover | 2 |
| Room metadata on webhooks; persistent rooms | livekit #2120, #4849 | Media plane | 9 |
| HTTP upload for on-premise recording storage; RTSP ingest | egress #650, ingress #270 | Media plane | 9 |
| Flutter background blur | client-sdk-flutter #479 | Non-goal: client-side video effects | — |

## 6. Non-goals

| Non-goal | Reason |
|---|---|
| A hosted cloud, hosted agent regions, compliance certifications for a hosted service | Waveport is software you run; these are properties of an operator, not of a codebase |
| A hosted inference gateway | It adds a single point of failure; adapters connect to providers directly |
| Selling phone numbers | Waveport connects to any carrier instead |
| SIP video | No demand found, and both incumbents decline it |
| Client-side video effects | Outside a voice-agent platform |
| Native adapters for every provider | The adapter protocol and Python bridge cover the long tail |
