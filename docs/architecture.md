# Waveport architecture and technical spec

**Status (15 November 2026):** design spec. The workspace, the gates in section 2 and the test and benchmark harnesses exist. Of the product, the kernel's public types (section 4) and its six scenarios (section 14) are written; the kernel itself does nothing yet. Every latency and memory figure is a design target to be validated by benchmark, not a measurement. The density figures in section 12 are the exception: the phase 1 spike measured them.

See also the [roadmap](../ROADMAP.md) and the [ecosystem inventory](ecosystem.md).

## 1. Shape of the system

Waveport is a hexagonal (ports and adapters) system with a domain-driven core. Dependencies point inward only.

```
        driving adapters                              driven adapters
 HTTP and gRPC API · CLI · bindings         STT · LLM · TTS · speech-to-speech
 client gateway · SIP · signalling          transports · local models · journal
                                            bus · telemetry · retrieval · tools
              │                                            ▲
              ▼                                            │
   ┌──────────────────────── application ────────────────────────┐
   │   waveport-runtime (sessions)     waveport-control (fleet)   │
   │   ┌───────────────────────── ports ─────────────────────┐   │
   │   │                    waveport-ports                    │   │
   │   │   ┌──────────────────── domain ─────────────────┐   │   │
   │   │   │      waveport-domain      waveport-media     │   │   │
   │   │   └──────────────────────────────────────────────┘   │   │
   │   └──────────────────────────────────────────────────────┘   │
   └──────────────────────────────────────────────────────────────┘
```

`waveport-protocol` stands beside this picture. It defines what crosses a process boundary (the client protocol, the control API, the out-of-process adapter protocol and the journal record format), depends on no workspace crate, and is used by the application layer, adapters and every client SDK.

## 2. Layers and the dependency rule

| Layer | Crates | Rules |
|---|---|---|
| Domain | `waveport-domain` (includes the session kernel), `waveport-media` | Pure Rust: no async runtime, no sockets, no wall clock. `unsafe` is forbidden, except named DSP hot loops that run under Miri. `waveport-domain` is `no_std` with `alloc`, and CI builds it for a target with no operating system, so the compiler enforces the rule. `waveport-media` uses `std`, because it wraps DSP libraries, and is held to the rule by Clippy bans |
| Ports | `waveport-ports` | Traits and capability types only; depends on the domain |
| Protocol | `waveport-protocol` | Wire formats only. Depends on no workspace crate, so that a client SDK never links the kernel |
| Kit | `waveport-adapter-kit` | What adapters share: the plumbing for provider connections (WebSocket, HTTP, server-sent events, retry classes). Depends on domain, ports and protocol. The application layer may not depend on it |
| Application | `waveport-runtime`, `waveport-control` | Use cases: run a session, dispatch, drain, migrate, hand off. Depends on domain, ports and Tokio |
| Adapters | `crates/adapters/**` | One crate per provider or technology. Depend on ports and the kit, never on each other |
| Composition roots | `waveportd`, the `waveport` CLI, language bindings | The only places where adapters are chosen and wired |
| Testing | `crates/testing/**` | Simulation harness, allocation counter, test kit. Never a dependency of code that ships |
| Tooling | `xtask`, `benches` | Repository gates and benchmarks. `xtask` depends on no workspace crate, so the gates build even when product crates do not |

`cargo xtask arch-check` enforces the rule from `cargo metadata`. It fails when a domain, ports or protocol crate reaches an async runtime or an entropy source through any chain of dependencies, when a dependency crosses layers the wrong way, when one adapter reaches another, and when an inner layer gains a third-party dependency that is not allow-listed. [CONTRIBUTING.md](../CONTRIBUTING.md#layers-and-the-dependency-rule) lists every rule.

## 3. Bounded contexts

| Context | Type | Aggregates and key concepts |
|---|---|---|
| Conversation | Core | `Session` (root), `Conversation`, `Item` (message, tool call, tool result, notice, summary), `SpeakerRef`, `ContextVersion` |
| Turn-taking | Core | `Floor`, `UserTurn`, `AgentTurn`, `EouEstimate`, `OverlapVerdict`, `Epoch` |
| Response | Core | `Generation`, `Utterance` with its playout ledger, `ToolBatch`, `OpTag`, `AlignmentMark` |
| Agents and flows | Core | `Stage`, `AgentDef`, `Flow`, `FlowState`, `BrainProfile`, `HandoffPolicy` |
| Orchestration | Core | `Placement`, `Node`, `Lease`, `JobOffer`, `Deployment`, drain and migration |
| Media | Supporting | `MediaSession`, `Participant`, `Track`, `AudioFormat`, `MediaTime`, `MediaSpan` |
| Tools and knowledge | Supporting | `ToolCall`, `ToolSpec` (schema, effect class, gate, timeout), MCP, retrieval, memory |
| Connectivity | Supporting | Transports, signalling, client protocol, trunks, numbers, transfers |
| Durability | Supporting | Journal, snapshot, resume token |
| Observability and evaluation | Supporting | Spans, metrics, recordings, transcripts, scenarios, judges |
| Identity and access | Generic | Project, API key, access token and grants, revocation, roles, audit log |

`Session` is the consistency boundary: one event is one transaction. Each sub-machine (floor, generation, tool call, conversation, lifecycle) exposes `decide` and `apply`, and a coordinator composes them, so the kernel is not one large `match`.

## 4. Session kernel

### 4.1 Three planes

| Plane | What runs there | Rule |
|---|---|---|
| Control | One single-writer actor per session driving `Kernel::step(now, event, &mut out)` | `step` never awaits, does no I/O and reads no clock. The actor's mailbox receive is the only await |
| Media | `Ingress` (reorder, decode, denoise, resample, fan-out) and `Playout` (buffer, pace, fade, encode) | Audio samples never enter the kernel. Plain structs, static dispatch, pooled frames, no allocation |
| Compute | Voice-activity detection, end-of-turn, interruption classifier, neural noise suppression | Pinned OS threads. One model instance per compute thread, shared by every session, batched across sessions |

The whole control plane of a session is this loop:

```rust
loop {
    let ev = mailbox.recv_or_timer().await;    // the only await
    kernel.step(clock.now(), ev, &mut out);    // pure; target under 10 µs
    journal.enqueue(&out.facts);               // copy into the shard's journal buffer
    for cmd in out.cmds.drain(..) {
        drivers.dispatch(cmd);                 // try_send; never blocks
    }
}
```

Two alternatives were rejected. A chain of per-processor tasks joined by queues (Pipecat's model) makes an interruption a message racing data through N queues, which is the structural cause of its race bugs. A compiled static dataflow graph is right for the per-frame audio chain, and waveport uses one there, but turn-taking is reactive control with cancellation, not dataflow.

### 4.2 Executor and shards

- The process runs N shards. Each shard is a pinned, single-threaded Tokio `LocalRuntime`, and a session stays on one shard for life.
- The session path therefore has no cross-thread wake-ups and no locks, the kernel-to-playout call is an ordinary function call, and production has the same topology as the simulator.
- The cost is head-of-line blocking. Inline work is capped at 250 µs per call, enforced by a poll-time watchdog and a CI gate. Opus and SIMD resampling fit that budget and run inline. The cap was 100 µs until the phase 1 spike measured an Opus encode at 24 kHz: 110 to 140 µs on average and up to 215 µs, depending on encoder complexity.
- Adapters are still required to be `Send`, so moving to a work-stealing runtime stays possible. If pacing jitter misses its target, the media loop moves to a sibling thread per shard without touching the kernel.
- Model batching: shards push `(slot, frame)` into a lock-free ring. Every model fires immediately with whatever is queued, up to 64 frames for voice activity, so batches grow by themselves as load grows. The spike measured the alternative, waiting up to 2 ms to fill a batch: it saves 14% of inference time and misses the latency target in section 12. Results return as kernel events stamped with media time, so batching cannot reorder them.

### 4.3 Identities

| Identity | Meaning |
|---|---|
| `Seq(u64)` | Position of a fact in the session's journal, from 1 and without gaps. Only journaled facts are numbered; inputs and client events are separate orders |
| `TurnId` | One user turn |
| `Epoch(u32)` | Bumped whenever the agent output in flight for the floor becomes invalid: committed barge-in, superseded speculation, handoff, cancel, recovery |
| `GenId` | One response, recording the epoch and context version it was computed from |
| `OpTag { generation, epoch }` | Carried on every command that starts or steers agent output, and echoed on every result |
| `ToolCallId` | One tool call; also its idempotency key |
| `MediaTime` | Sample position on a track, in ticks of 1/48,000 s |
| `Prob(u16)` | A probability in hundredths of a percent. Kernel state holds no floating point, so two states can be hashed and compared exactly |

**Liveness.** The kernel keeps a small table of the responses under way, and a result is acted on only while its generation is in that table. An epoch bump removes the generations that hold the floor. A generation that works on a tool call for another agent (section 10) is removed when its call ends, not when the epoch moves. Comparing a result's epoch with the kernel's would be simpler and wrong: a barge-in on the front agent would discard the work of the back agent it had delegated to.

### 4.4 Events and commands

```rust
pub enum Event {
    // Perception: facts about the world. Never dropped as stale, never flushed.
    Speech { who: ParticipantId, edge: VadEdge, at: MediaTime },
    Transcript { who: ParticipantId, span: MediaSpan, text: Text, is_final: bool },
    Eou { turn: TurnId, p: Prob },
    Overlap { turn: TurnId, verdict: OverlapVerdict }, // Backchannel | BargeIn | Noise
    TextIn { who: ParticipantId, text: Text },

    // Results of agent output work. Dropped in one place if the tag's generation is not live.
    Op(OpTag, OpEvent), // LlmDelta, ToolProposed, LlmDone, TtsSegment, Played, Failed

    // Tool results. Keyed by call id, and deliberately independent of the epoch.
    Tool(ToolCallId, ToolEvent),

    Timer(TimerId),
    Control(ControlInput),
    Durable(Seq),
}

pub enum Command {
    Llm(OpTag, LlmCmd),         // Start | Pause | Resume | Cancel
    Tts(OpTag, TtsCmd),         // Speak { text } | Finish | Cancel
    Realtime(OpTag, RtCmd),     // added with speech-to-speech models in phase 2
    Stt(SttCmd),                // Finalize | Close
    Sense(SenseCmd),            // EstimateEou { turn, who } | ClassifyOverlap { turn, who }
    Playout(OpTag, PlayoutCmd), // Start | Duck | Unduck | Stop { fade_ms }
    Tool(ToolCallId, ToolCmd),  // Dispatch { tool, args, attempt } | Cancel; the call id is the idempotency key
    SetTimer(TimerId, Option<MonoTime>),
    AwaitDurable(Seq),
    Emit(ClientEvent),
}
```

- `Sense` is how the kernel asks the compute plane for a judgement: an end-of-turn estimate when a speaker goes quiet, a classification when speech overlaps the agent. The answers, `Eou` and `Overlap`, name the turn they were asked about, so a late answer is matched to the right turn or ignored.
- Text that arrives is owned by its event. Text that leaves is written once into the step's output buffer, and commands and facts refer to it by offset. A step clears that buffer first, so its output never mixes with the previous step's.
- A step also emits **facts**: what became true, numbered by `Seq`. Facts are what the journal stores (section 9).

### 4.5 Why the incumbents' races cannot occur

| Race reported against the incumbents | Why it cannot occur here |
|---|---|
| User input flushed by its own turn's interruption | An interruption is `epoch += 1`, which ends the responses that hold the floor, and only `Event::Op` is checked against the responses that are live. Perception has no flush path, and transcripts join turns by media span, not arrival order |
| A backchannel cancels a tool whose side effect already happened | Barge-in is two-phase, and tool calls live outside the epoch (see 4.7 and 4.8) |
| A tool call from an interrupted reply still executes | Dispatch is emitted in the same `step` that checks the generation is live |
| The LLM runs twice after parallel tools | A tool batch moves `Open → Resolved` in one guarded transition, which starts the single continuation |
| Spoken text is lost, or never-spoken text is appended | Assistant speech reaches the context through one function, `commit_utterance` (see 4.6) |
| Shutdown hangs | The kernel cannot block, and termination is timer-driven (see 4.9) |

No priority lanes are needed. Pipecat has them because slow work sits in line; here nothing slow is in line.

### 4.6 Spoken-text reconciliation

- Text-to-speech and speech-to-speech adapters attach alignment marks `(text_offset, sample_offset)` to the audio they return. The marks stay with the audio, in the media plane's playout ledger; the kernel stores none. A provider without word timing yields one mark per segment, and the ledger interpolates inside the segment by share of characters.
- `Playout` reports how far playback has got: audio handed to the transport, audio acknowledged, and how much of the text has been heard in full, which is the last mark at or before `played_upto − estimated_client_buffer`. It uses transport playout acknowledgements where they exist.
- The runtime delivers the latest report to the kernel ahead of any other event, so an interruption is decided on what had been heard by then. A final position sent after the stop would carry the tag of a response that is no longer live, and be dropped.
- On completion or interruption the kernel commits exactly one assistant item: the planned text cut at the heard offset, flagged `truncated` when cut.
- LLM deltas never touch the context. They pass through the kernel, which releases text to synthesis by the chunking policy and the credit rule of section 5, so committed text is a prefix of released text by construction.
- `Playout::push(tag, chunk)` drops chunks whose tag is not current, so an adapter that keeps sending after a cancel is harmless.
- When a speech-to-speech provider supports truncation it receives the same cut point. When it does not, the divergence between its server-side context and the kernel's is recorded explicitly.

### 4.7 Barge-in

1. User speech that overlaps agent speech only **ducks** the agent (target: within 150 ms of speech onset, measured where audio leaves the server). Frames already handed to the transport cannot be ducked, which is why the send-ahead in section 5 is one frame.
2. The interruption classifier answers `Backchannel`, `BargeIn` or `Noise`. A policy timeout answers for it if it is slow.
3. `BargeIn` commits: the epoch is bumped, playout stops with a 5 to 10 ms fade, and the utterance is committed up to what was heard. Any other verdict un-ducks and the agent carries on.

The classifier is a port. Phase 1 ships a voice-activity-only policy; phase 5 ships an open-weights model.

### 4.8 Tools

A tool call moves through `Proposed → Authorized → Dispatched → Completed | Failed | InDoubt`.

- Each `ToolSpec` declares an **effect class**. After dispatch, a call can be cancelled only if it is declared `Pure`, or when the tool acknowledges its own rollback.
- Each `ToolSpec` can declare a **gate**, for example "after the agent's own utterance has finished playing" for an end-call tool.
- Before a side-effecting tool is dispatched, the kernel waits until the journal record that authorises the dispatch is durable (see section 9). Waiting for the record that proposes the call is not enough: after a crash, recovery would find only the proposal, conclude that the call never ran, and report a failure for an effect that happened.
- A side-effecting call that was authorised and has no recorded result after a crash is `InDoubt`. It is retried only if the tool declares idempotency; otherwise the model is told the outcome is unknown.
- Tool results enter the context exactly once, whatever happened to the turn that requested them.

### 4.9 Shutdown

The lifecycle is `Running → Draining → Closing → Closed`, advanced by timer events. Commands are sent with `try_send`. Adapter tasks sit in a join set and are aborted when the grace timer fires. `Closed` is reached within drain time plus grace time whatever any adapter does.

### 4.10 Extension points

Waveport has no "drop in a processor" mechanism. Behaviour is extended through pure policies and taps:

- `TurnPolicy`, `InterruptionPolicy`, `AddresseePolicy`
- `ToolGate`
- Text chunking between LLM and text-to-speech
- Media taps on the inbound and outbound audio paths

## 5. Flow control

The principle is: **audio drops, text blocks, control coalesces.**

| Link | Bound | On overflow |
|---|---|---|
| Transport → reorder buffer | Adaptive 0–60 ms, maximum 200 ms | Drop late packets; codec concealment for gaps up to 60 ms |
| Ingress → voice-activity batcher | About 100 ms ring | Drop the frame, hold the last state, count it |
| Ingress → speech-to-text | 2 s ring | Drop oldest, mark the session degraded, fail over |
| Adapter → kernel mailbox | 256 events | The sender awaits, so pressure reaches the provider socket |
| Kernel → adapter commands | 32 | `try_send`; full means unhealthy, so cancel and fail over |
| LLM → text-to-speech | Credit: release text while synthesized-minus-played is 3 s or less | Hold text; stop reading the LLM stream above 8 KB pending |
| Text-to-speech → playout buffer | 3 s of audio | The driver stops reading the provider socket |
| Playout → transport | 20 ms frames; 20 ms send-ahead by default, 40 ms at most | Never queue beyond the lead |

- High-rate signals (playback position, audio levels, interim transcripts) are latest-value slots, not queues.
- The 3-second credit caps wasted text-to-speech spend at about 3 seconds per interruption.
- Pacing uses absolute deadlines from one timer tick per shard; the first frame of an utterance goes out immediately.
- On transports that cannot clear a remote buffer, the send-ahead (20 ms by default) is already on the wire when the server stops or ducks.

## 6. One model for every session shape

A `BrainProfile` states which port fills each of three roles (hear, think, speak) and which duties a provider handles internally (turn detection, context, synthesis). Application-layer drivers translate: `CascadeDriver` for speech-to-text, LLM and text-to-speech, `RealtimeDriver` for speech-to-speech. The kernel issues intent-level commands and never branches on "is this realtime".

| Shape | How it fits |
|---|---|
| Cascaded | Three ports fill the three roles |
| Speech-to-speech | One port fills all three; the kernel keeps a text mirror of the conversation |
| Hybrid front/back | Two agents share one stage. The front agent holds the floor; the back agent is reached through a `delegate` tool whose executor is another agent's generation |
| Multi-party | Every perception event carries a `ParticipantId`; speaker is mandatory on turns and messages. `Floor` tracks N holders, and an `AddresseePolicy` decides whether the agent is addressed. One-to-one is N = 1 with the policy `Always` |
| Text-only | A session with zero tracks and no transport. A text input opens and commits a turn in one step, and "played" means delivered |

## 7. Flows on speech-to-speech models

Each realtime adapter declares what it can do:

```rust
pub struct RealtimeCaps {
    pub instructions: Mutability,   // Live | PerResponse | ReconnectOnly
    pub tools: Mutability,
    pub inject: InjectCaps,
    pub truncate: bool,
    pub manual_response: bool,
    pub turn_owner: TurnOwner,      // Server | Client | Either
    pub resume: ResumeCaps,         // Handle { ttl } | ReplayText { max_tokens } | None
    pub max_session: Option<Duration>,
}
```

A pure `plan(&AgentDef, &Caps) -> Result<SessionPlan, PlanError>` runs at session start and at every handoff. It picks the strongest strategy available and fails loudly instead of degrading silently:

1. **Live:** update the provider session at the transition, behind a barrier. The next response is not created until the update is acknowledged.
2. **Per response:** each response request carries the node's instructions and tools.
3. **Gatekeeper (works with every provider):** register the union of all tools once. The kernel rejects out-of-node calls with a synthetic tool result and injects the node's task as a context item.
4. **Reconnect:** open a second provider session with the node's configuration and replayed context, and swap at a turn boundary. Provider session time limits use the same path.
5. **Front/back:** a text back-agent owns the flow and the speech-to-speech model is its voice.

The flow is a kernel-side state machine, so its guarantees never depend on what the model can see.

## 8. Ports

### 8.1 Three styles

| Path | Style | Dispatch |
|---|---|---|
| Per audio frame (codec, resample, denoise, playout) | Synchronous functions on caller-owned buffers | Generics or closed enums; no `dyn`, no `async`, no allocation |
| Provider streams (speech-to-text, LLM, text-to-speech, speech-to-speech, transport) | Split halves: `try_send` in, `poll_next` out | One boxed object per stream; one synchronous virtual call per event |
| Request/response (journal, bus, retrieval, tools, dispatch) | Boxed futures | `Arc<dyn …>`; a few calls per turn |

Stable Rust 1.100 has no `async fn` through `dyn`, no return-type notation, no async drop and no generators. The design depends on none of them. Poll-style receive halves are object-safe without boxing each call, and boxed futures are written by hand where a call happens a few times per turn.

Adapters hold no interruption logic. The runtime's drivers own tagging, cancellation by drop, retries and metrics. A helper builds both halves of a stream port from an ordinary select loop over bounded channels.

### 8.2 Driven ports (sketches)

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Speech-to-text. `Llm` and `Tts` have the same three-trait shape.
pub trait Stt: Send + Sync + 'static {
    fn caps(&self) -> SttCaps;
    fn open(&self, cfg: SttConfig)
        -> BoxFuture<'_, Result<(Box<dyn SttTx>, Box<dyn SttRx>), PortError>>;
}
pub trait SttTx: Send {
    fn try_send(&mut self, audio: AudioRef<'_>) -> Result<(), Full>;
    fn finalize(&mut self);
}
pub trait SttRx: Send {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>)
        -> Poll<Option<Result<SttEvent, PortError>>>;
}

/// Send half of a speech-to-speech session. Every command is answered by Ack or Nack.
pub trait RtTx: Send {
    fn try_audio(&mut self, audio: AudioRef<'_>) -> Result<(), Full>;
    fn try_cmd(&mut self, cmd: RtCmd) -> Result<(), Full>;
    // RtCmd: UpdateSession, InjectItem, CreateResponse, Cancel, Truncate, ToolResult
}

/// Compute plane: batched across sessions, one instance per compute thread.
pub trait VadModel: Send + 'static {
    type State: Default + Send;
    fn infer(&mut self, frames: &[&[f32]], states: &mut [Self::State], out: &mut [f32]);
}
pub trait InterruptionClassifier: Send + 'static {
    fn infer(&mut self, windows: &[OverlapWindow<'_>], out: &mut [OverlapVerdict]);
}

pub trait Transport: Send + 'static {
    fn caps(&self) -> TransportCaps; // codecs, remote buffer (clearable, marks), data, DTMF
    fn poll_event(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>>;
    fn try_send_media(&mut self, track: TrackId, frame: &EncodedFrame) -> Result<(), Full>;
    fn try_control(&mut self, ctl: TransportControl) -> Result<(), Full>; // Clear, Mark, Data
}

pub trait Journal: Send + Sync {
    fn append(&self, s: SessionId, lease: LeaseEpoch, from: Seq, batch: Bytes)
        -> BoxFuture<'_, Result<Seq, JournalError>>;
    fn load(&self, s: SessionId) -> BoxFuture<'_, Result<Recovered, JournalError>>;
}
```

The remaining driven ports:

| Port | Style |
|---|---|
| `TurnModel` | Compute plane, same shape as `InterruptionClassifier` |
| `Denoise` | Synchronous per-stream `process(&mut [f32])` |
| `Bus`, `ToolExecutor`, `Retriever` | Boxed-future request ports |
| Clock | Lives only in the runtime; the kernel receives `now` and emits `SetTimer` |
| Telemetry | A subscriber to the domain-event stream plus static per-shard histograms; never a call inside the kernel |

Errors carry a retry class: transient, rate-limited, auth, permanent or timeout.

### 8.3 Driving ports (sketches)

```rust
pub trait SessionControl {
    fn start(&self, spec: SessionSpec) -> BoxFuture<'_, Result<SessionId, StartError>>;
    fn send(&self, s: SessionId, input: ControlInput) -> Result<(), ControlError>;
    // ControlInput: Text, Say, Interrupt, Handoff, Close
    fn subscribe(&self, s: SessionId, from: Seq) -> Box<dyn EventRx>; // resumable by Seq
    fn migrate(&self, s: SessionId, to: NodeId) -> BoxFuture<'_, Result<(), MigrateError>>;
}

pub trait Dispatch {
    fn offer(&self, job: JobOffer) -> Offer;
    fn load(&self) -> LoadReport;
    fn drain(&self, by: MonoTime);
}
```

## 9. Durability

Waveport stores a snapshot plus a journal of **domain events**, not inputs. Recovery is `snapshot ⊕ apply(tail)`. Replaying inputs through `decide` is rejected for recovery, because during a rolling deploy the target node runs newer logic. Inputs are still sampled into a flight recorder for replay tests.

| | |
|---|---|
| Journaled | Turn commits, context items, utterance commits, tool-call transitions, agent and flow transitions, participant and configuration changes, and a playout checkpoint each second while speaking. Expect 5 to 30 records per turn |
| Not journaled | Audio, token deltas, voice-activity edges, interim transcripts |
| Hot-path cost | Serialisation into a shard buffer; group commit off-thread every 10 ms or less |
| One synchronous barrier | Before dispatching a side-effecting tool, the kernel emits `AwaitDurable(seq)` for the record that authorises the dispatch, and dispatches on `Durable` |
| Format | Facts and snapshots are versioned serde records defined in `waveport-protocol`. The domain crate stays free of serialisation: the runtime maps each fact to its record with an exhaustive match, so a new fact does not compile until it is mapped. Zero-copy archives (`rkyv`) were rejected for snapshots, because an archived layout cannot evolve and a newer build must read the state an older one wrote |
| Fencing | Appends are conditional on `(lease epoch, expected seq)`, so a superseded owner can neither write nor speak |

Three recovery levels:

| Level | What happens | Target |
|---|---|---|
| Client reconnect | The kernel waits in a detached state, the transport rebinds, and the client resyncs from its last sequence number | No loss |
| Process crash | A new node loads snapshot and tail and reopens provider streams; the client reconnects with a resume token | Within 2 s of lease expiry |
| Live migration | The target pre-opens provider connections; the source waits for a safe point (floor idle, no live generation; forced at a deadline), freezes, flushes and hands over the lease | Drain time becomes "time to the next turn boundary" |

How the media moves during a migration depends on the transport: through an SFU or relay it re-joins (gap of 300 ms or less); direct WebRTC uses an ICE restart (0.3 to 1 s); a carrier WebSocket is bound to its TCP connection and cannot move unless a thin media edge holds that socket.

What cannot be restored, and what happens instead:

| Lost state | Behaviour |
|---|---|
| In-flight LLM or text-to-speech work | Discarded and regenerated from committed context |
| A partially played utterance after a crash | Committed to the last checkpoint and flagged truncated, with up to 1 s of uncertainty |
| Speech-to-text hypothesis state | On migration the open turn's pre-roll (up to 8 s) travels with the snapshot and is re-fed. On a crash it is lost and the agent asks again |
| A speech-to-speech provider session | Server-side audio context is not portable. Use the provider's resumption handle if it has one; otherwise seed a new session from the kernel's text mirror (prosody is lost); otherwise from a summary |

## 10. Multi-agent

Agents are participants of kind `Agent` on the stage. At most one holds the floor per output track; the others are background agents.

| Mode | Mechanism |
|---|---|
| In-process handoff | A handoff request arrives, usually from a tool result, carrying full context, a summary, a filtered context or none. At a safe point the kernel bumps the epoch, activates the new agent, recomputes the session plan and swaps drivers make-before-break |
| Cross-process: remote brain | Session, media and floor stay put. The think role is filled by a bus-backed LLM adapter whose results carry the `OpTag`, so the same liveness rule discards stale remote output. Costs one bus round trip per turn |
| Cross-node: session transfer | The migration from section 9 to a node hosting the target agent, followed by activation |
| Background and parallel agents | A generation without playout, optionally parented by a tool call. One parented by a call ends with that call, and an interruption of the floor leaves it running. Its output lands as a tool result or a notice. To speak, it requests the floor, and the policy that arbitrates humans arbitrates agents |

Bus envelopes are `{ session, lease, seq, tag, payload }`, delivered at least once and deduplicated on `(session, seq)`.

## 11. Fleet

The fleet layer (`waveport-control`) is designed in detail in phase 3. Its fixed points:

- Nodes register with the control plane and report load: session count, CPU, and shard poll latency.
- The dispatcher sends a job offer; a node accepts or declines; placement grants a lease with a fencing epoch.
- Draining a node stops new offers and migrates its sessions at turn boundaries. Drain time has no cap.
- Autoscaling signals are session and capacity gauges exported for the cluster autoscaler, not CPU.
- A process hosts at most 2,000 sessions, to bound the blast radius of a crash.

## 12. Performance targets

All are design targets, each enforced by a benchmark gate from phase 1.

| Metric | p50 | p99 |
|---|---|---|
| Kernel `step` | 5 µs | 50 µs |
| Packet in → decoded, resampled, fanned out | 60 µs | 200 µs |
| Voice-activity result after the frame is complete | 1.5 ms | 3 ms |
| LLM first token → first text-to-speech request | 0.2 ms | 1 ms |
| Text-to-speech first byte → first packet on the wire | 2 ms | 10 ms |
| End-of-turn decision → first audio packet (framework share) | ≤ 5 ms | ≤ 15 ms |
| Barge-in decision → no more audio leaves the server | ≤ 10 ms | ≤ 30 ms |
| Speech onset → duck, where audio leaves the server | 100 ms | 150 ms |
| Memory, parked or text-only session | 64 KB | 150 KB |
| Memory, idle connected voice session | 300 KB | 600 KB |
| Memory, active voice session (8 s pre-roll, 3 s speech lead) | 1.5 MB | 4 MB |
| Session start in a warm process, excluding transport handshake | 2 ms | 10 ms |
| Journal cost per record on the hot path | 2 µs | 10 µs |
| Heap allocations per audio frame in steady state | 0 | 0 |

**Density.** The phase 1 spike measured one active cascaded session at 1.17% of a core ([decision 0001](decisions/0001-density-spike.md)): 0.45% on the shard thread, most of it the Opus encoder, 0.14% for voice-activity inference and 0.57% for end-of-turn inference at one inference every ten seconds. That is 85 sessions per core with nothing to spare. The planning figure is 60 active voice sessions per core, which keeps a core at 75% and allows a tenth for the work the spike did not model, and about 900 per 16-core node. One shard thread holds its timing up to about 150 sessions; inference is what limits the core count. The earlier estimate of 0.7 to 1% and target of 100 per core left out end-of-turn inference and assumed batching would make inference 2 to 4 times cheaper; the measured gain is 1.9 times. The measurement has limits, listed in the decision record: one laptop core at turbo frequency, with the rest of the machine nearly idle. Not measured: local neural noise suppression, which the earlier figures put at about 1.5% of a core per session, and text-only sessions, estimated at more than 5,000 per core. For comparison, LiveKit's own guidance is 10 to 25 jobs per 4 cores.

## 13. Invariants

These ten are stated formally and checked in simulation.

1. The epoch is monotonic, and no playout start or tool dispatch is emitted for a generation that is not live.
2. Every accepted perception event ends in exactly one outcome: a committed user turn, or a turn dismissed for a stated reason (a backchannel, noise, no words, or the session closing while the turn was open).
3. Each utterance commits exactly once; committed text is a prefix of planned text and never extends past the audio handed to the transport.
4. A dispatched side-effecting call ends `Completed`, `Failed` or `InDoubt`; it is dispatched at most once per key within one lease, and after a crash a call is dispatched again only if its tool is declared pure or idempotent; its result enters the context exactly once.
5. Each tool batch starts at most one continuation.
6. At most one generation holds playout per output track.
7. From `Close`, `Closed` is reached within drain plus grace under any adapter behaviour.
8. All queues are bounded; `step` is total and deterministic.
9. `load(snapshot) ⊕ apply(tail)` equals live state.
10. The facts a kernel emits are numbered without gaps, and once told that its lease is lost it emits nothing more. That at most one lease appends per session is the journal's guarantee (section 9) and is checked with the journal, since a lease is not kernel state.

Invariants 1 to 5 close the incumbents' race classes by construction. General robustness to adapter interleavings is established by exploration, not by construction.

## 14. Verification strategy

| Technique | Scope |
|---|---|
| Kernel scenarios | Six stories written before the kernel, each closing a race from section 4.5 or a session shape from section 6. A simulated world stands in for every adapter, and each story is run along every order in which its events could arrive. The scenario files are not edited to make them pass |
| Deterministic simulation, level 1 | Seeded generators feed `Kernel::step` with a virtual clock; no executor needed |
| Deterministic simulation, level 2 | The real actor loop and drivers on a simulated single-threaded executor. Fake adapters delay, reorder, stall, disconnect and crash at any journal offset. Every failure is a seed |
| Property tests | The ten invariants over arbitrary interleavings |
| Small-scope model check | Floor × generation × tool call × lifecycle with 2 participants, 3 turns, 2 tools |
| Replay | Flight-recorder traces re-run in CI against golden command traces, and differentially between versions |
| Fuzzing | `step` with structured events; every wire parser (SIP, SDP, RTP, client protocol); journal decode with snapshot round trip |
| Concurrency checking | The few lock-free structures (rings, batcher) |
| Port conformance kit | Each adapter's declared capabilities tested nightly against the live provider |
| CI gates | Instruction-count benchmarks for `step` and the per-frame media path; a counting allocator that fails on any allocation in the media path; a 1,000-session soak with flat memory; the density and interruption targets; a `kill -9` test that must resume |

## 15. Workspace and Cargo spec

```
waveport/
├─ Cargo.toml, rust-toolchain.toml, deny.toml, clippy.toml, release-plz.toml
├─ xtask/          repository gates, run as `cargo xtask <command>`
├─ benches/        waveport-benches: wall-clock and instruction-count benchmarks
├─ crates/
│  ├─ core/        waveport-domain · -media · -ports · -adapter-kit · -runtime · -control · -protocol
│  ├─ adapters/
│  │  ├─ transport/   webrtc · websocket · telephony-ws · sip · moq · livekit · local-audio
│  │  ├─ stt/ llm/ tts/ realtime/   one crate per provider
│  │  ├─ models/      vad · turn · interruption · denoise
│  │  ├─ store/       journal · objects · cache
│  │  ├─ bus/         in-process · NATS · Redis · Postgres
│  │  ├─ telemetry/   OpenTelemetry · Prometheus
│  │  ├─ knowledge/   vector stores · embeddings
│  │  └─ tools/       MCP · HTTP · WASM
│  ├─ bindings/    python · node · wasm-host · c-abi
│  ├─ client/      client-core (shared by every client SDK)
│  └─ testing/     waveport-sim · -kernel-sim · -alloc-counter · -testkit · -evals
├─ apps/           waveportd · waveport-cli · console (web UI) · operator
├─ sdks/           js · react · swift · kotlin · react-native · flutter · embedded
├─ models/         training and evaluation code for the open models
├─ assets/         assets.lock.toml: model files pinned by address and SHA-256, fetched on demand
├─ perf/           recorded measurements and baselines
├─ spikes/         throwaway experiments, outside the workspace and its gates
├─ deploy/         Dockerfiles · Helm chart · examples
└─ docs/ · examples/
```

`xtask/`, `benches/`, `crates/core/` (without the adapter kit), `crates/testing/` (without `-evals`), `assets/`, `perf/`, `spikes/` and `docs/` exist today. The rest is added in the phase that needs it.

The root [`Cargo.toml`](../Cargo.toml) is the source of truth for workspace members, lint levels, profiles and dependency versions. What it does not say by itself:

- **Only dependencies in use are declared.** Cargo and `cargo deny` reject unused ones. The pins already decided for later phases are: `tokio` `~1.53` (the long-term-support line); `str0m` `=0.25.0`, exact, because it breaks its API on most minor releases; `rsipstack` `=0.8.5`, exact, because it had 88 releases in twelve months; `ort` `=2.0.0-rc.15`, exact, because it is a release candidate.
- **Each crate declares its layer and stability tier** under `[package.metadata.waveport]`. The lists `arch-check` uses (async runtimes, entropy crates, third-party crates each inner layer may use) are under `[workspace.metadata.waveport]`.
- **`unsafe` is forbidden workspace-wide.** Crates that need it (DSP hot loops, FFI, bindings) do not inherit the workspace lint table. They carry a copy with `unsafe_code = "deny"`, mark each site with `#[expect]` and a safety comment, and run under Miri. `cargo xtask lint-sync` fails if the copy drifts. Today the only such crate is `waveport-alloc-counter`.
- **The Clippy denials** for `unwrap`, `expect`, `panic` and indexing are relaxed inside tests through `clippy.toml`.
- **The release profile keeps unwinding** (`panic = "unwind"`): a panic ends one session, not the process.
- **New dependency versions wait seven days** before they can be selected (`.cargo/config.toml`).
- **CI treats warnings as errors.**

## 16. Tech stack

Versions and licences were checked against crates.io on 14 November 2026.

**Toolchain:** Rust 1.100.0, edition 2024.

| Concern | Choice | Notes |
|---|---|---|
| Async runtime | `tokio` 1.53 long-term-support line (supported to September 2027), one `LocalRuntime` per shard | io_uring is still not available for sockets. Thread-per-core runtimes (monoio, glommio) are stale |
| WebRTC | `str0m` 0.25.0 | Sans-IO; includes DTMF and G.722. No TURN client and no jitter buffer |
| LiveKit interop | `livekit` 0.11.0 | Optional adapter only; pulls a prebuilt C++ libwebrtc |
| SIP | `rsipstack` 0.8.5 | `rvoip` 0.4.0 is the fallback |
| RTP, SRTP, SDP | `rtp` and `rtcp` 0.17.3, `webrtc-srtp` 0.17.3, `sdp-types` 0.3.2 | Packet types only; sessions are written in-repo |
| Codecs and DSP | `opus` 0.4.0 (libopus), `ezk-g722` 0.1.2, `rubato` 5.1.0, `nnnoiseless` 0.5.2, `rtrb` 0.4.1, `bytes` 1.12.1 | G.711 written in-repo. DeepFilterNet is a later candidate (git dependency only) |
| Local inference | `ort` 2.0.0-rc.15 (ONNX Runtime 1.31) | Chosen over `tract` 0.23.10 by the phase 1 spike ([decision 0001](decisions/0001-density-spike.md)): `tract` cannot load the standard Silero export, and on Smart Turn it is 2.6 times slower and its outputs differ. `ort` inference needs exclusive access to a session, so each compute thread owns one |
| Bundled models | Silero VAD v6.2.3 (MIT, 2.3 MB); Smart Turn v3.2 (BSD-2, 8.7 MB) | Pinned by address and SHA-256 in `assets/assets.lock.toml` and fetched by `cargo xtask fetch-assets`; never committed. LiveKit's turn detector cannot be used: its licence restricts it to LiveKit Agents |
| Local speech (optional) | `sherpa-onnx` 2.0.1 | Local LLMs run out of process behind the OpenAI-compatible adapter |
| QUIC and MoQ | `quinn` 0.11.12, `web-transport-quinn` 0.13.6, `moq-net` 0.3.23 | Experimental transport; the drafts are still moving |
| WebSocket, HTTP, gRPC | `tokio-tungstenite` 0.30.0, `axum` 0.8.9, `hyper` 1.12.0, `reqwest` 0.13.5, `tonic` 0.14.9, `prost` 0.14.6 | A breaking `tonic` release is pending |
| MCP | `rmcp` 3.10.0 | Official SDK |
| Bus | `async-nats` 0.52.0, `redis` 1.8.2 | `fred` is dormant |
| Stores | `sqlx` 0.9.0 (Postgres), `redb` 4.6.0 (local journal), `object_store` 0.14.2, `moka` 0.12.16 | Snapshots are versioned serde records, not `rkyv` archives; see section 9 |
| Knowledge | `qdrant-client` 1.20.0, `lancedb` 0.41.0, `pgvector` 0.4.2, `fastembed` 7.7.0 | |
| Observability | `tracing` 0.1.44, `opentelemetry` 0.33.1, `tracing-opentelemetry` 0.34.0, `metrics` 0.24.6, `dial9-tokio-telemetry` 0.5.8 | GenAI attribute names are defined in-repo; the upstream constants are deprecated |
| Bindings and plugins | `pyo3` 0.30.1, `napi` 3.16.2, `uniffi` 0.32.2, `wasm-bindgen` 0.2.130, `wasmtime` 50.0.1 with `wit-bindgen` 0.63.0 | WASI 0.3 (June 2026) gives plugins native async streams |
| Hot-path structures | `crossbeam-channel` 0.5.17, `arc-swap` 1.9.5, `papaya` 0.2.5, `zerocopy` 0.8.63; `mimalloc` 0.1.52 | The allocator was chosen by the phase 1 spike ([decision 0001](decisions/0001-density-spike.md)) |
| Kubernetes | `kube` 5.1.0, `k8s-openapi` 0.29.0 | Three major versions in 2026 |
| Testing | `turmoil` 0.7.2, `shuttle` 0.9.10, `loom` 0.7.3, `proptest` 1.12.0, `cargo-fuzz`, `insta` 1.49.0, `cargo-nextest`, `criterion` 0.8.2, `gungraun` 0.20.0 | |
| Release hygiene | `cargo-deny`, `cargo-semver-checks`, `cargo-hack`, `cargo-dist`, `release-plz` | |

**Written in-repo because no usable crate exists:**

- Streaming clients for OpenAI (including Realtime), Anthropic, Gemini (including Live), ElevenLabs, Cartesia, AssemblyAI and Azure Speech. AWS has official crates; Deepgram has a community one.
- Batched Silero VAD and Smart Turn inference.
- The jitter buffer and HTTP signalling for WebRTC.
- SIP call control, RTP sessions and RFC 4733 DTMF.
- The dispatcher, a server-sent-events parser, and OpenTelemetry GenAI attributes.

Multi-provider LLM crates (`genai`, `rig`) are rejected as foundations: they have no realtime support and change very fast.

**Dependency risks, each contained behind a port:**

| Bet | Risk | Containment |
|---|---|---|
| `ort` | Perpetual release candidate; native binaries | `tract` as a pure-Rust fallback for the small models |
| `str0m` | One core maintainer; frequent breaking changes | Wrapped by the transport port; `rtc` 0.21 is the alternative |
| `rsipstack` | One company; extreme release rate | Exact pin; `rvoip` as fallback |
| Local text-to-speech engines | Phonemisation depends on GPL-licensed espeak-ng | Optional and out of process |

**Prior art to study, not depend on:**

| Project | Licence | Why |
|---|---|---|
| flowcat, rustvani | Apache-2.0, BSD-2 | Rust projects that follow Pipecat's frame-pipeline design |
| active-call, RustPBX | MIT | SIP-to-agent glue and call-control design on `rsipstack` |
| adk-rust | Apache-2.0 | Realtime provider clients |
| atm0s-media-server | MIT | A Rust SFU |
| wavekat-turn | Apache-2.0 | Smart Turn feature extraction |

## 17. Engineering standards

| Area | Standard |
|---|---|
| Toolchain | Rust 1.100 pinned in `rust-toolchain.toml`; minimum supported version is latest stable minus two. Linux x86-64 and arm64 are tier 1; macOS and Windows are tested in CI |
| Errors | Typed errors per port with a retry class. Catch-all error types only in binaries |
| Features | Additive only. Each provider crate is opt-in. The feature powerset of core crates is checked in CI |
| Supply chain | Licence allow-list, advisory and duplicate checks on every pull request. Lockfile committed. Cargo 1.100 dependency cooldowns on, because crates.io filled with high-churn voice and SIP crates in 2026 |
| API stability | Three tiers: stable, preview, experimental; see the [stability policy](stability.md). Semver check and public-API diff on every pull request. Deprecations live at least two minor releases and six months |
| Release build | Fat LTO, one codegen unit, unwinding kept on, profile-guided optimisation for `waveportd`, `mimalloc` as the global allocator |
| Testing | See section 14. Provider contract tests run against recorded fixtures on every pull request and against live providers nightly |

## 18. Top risks

| Risk | Early test |
|---|---|
| The kernel becomes a monolith users cannot extend | Six executable scenarios before any real adapter: the four Pipecat races, hybrid front/back, three-party attribution |
| Shared-process blast radius; density measured on one laptop core only | The phase 1 density spike ([decision 0001](decisions/0001-density-spike.md)), to be repeated on server hardware with every core loaded; `kill -9` resume in CI from the first milestone; per-session panic isolation; an option to host native inference in a sidecar |
| Providers behave differently from their declared capabilities | Two speech-to-speech adapters with opposite capability profiles; the nightly conformance kit; measured spoken-prefix error on WebRTC and on a carrier WebSocket |

## 19. Open questions

These are not decided and each has an owner phase.

| Question | Decided in |
|---|---|
| Whether per-shard `LocalRuntime` holds its pacing target, or the media loop needs its own thread | Phase 1 |
| Which speech-to-text and text-to-speech providers ship first | Phase 1 |
| How a carrier media-stream session moves when its node drains or dies: a thin media edge that holds the socket, or a carrier-side reconnect | Phase 3 |
| Shared journal store at scale: Postgres alone, or Postgres plus object storage for snapshots | Phase 3 |
| How much of RTVI the client gateway implements | Phase 2 |
