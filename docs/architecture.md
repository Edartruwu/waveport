# Waveport architecture and technical spec

**Status (15 November 2026):** design spec. The workspace, the gates in section 2 and the test and benchmark harnesses exist. Of the product, the session kernel (section 4) exists: it takes turns, speaks, keeps the conversation, runs tool calls, lets a second agent answer a call or take the floor, closes, and is rebuilt from its journal after a crash. The audio path (`waveport-media`) exists too: Opus and G.711, the change of rate, the inbound path and the outbound path of sections 4.6 and 5, with no allocation per frame. The ports of section 8 exist (`waveport-ports`), with the planner of section 6 that decides how a session is run from what its parts can do, and so does what adapters share (`waveport-adapter-kit`). The runtime exists (`waveport-runtime`): shards, the session actor that drives the kernel and the audio path through the ports, and the compute plane that shares the listening models. So do the adapters of section 8.5, and the `waveport` command that puts them together into an agent to talk to. A session keeps its journal in a file and goes on from it after its process was killed (section 9). What a session does is told to a collector as traces and metrics (`waveport-telemetry-otel`, section 8.5), and the figures of section 12 that phase 1 can measure are measured and held by gates. All six of the kernel's scenarios (section 14) pass, and its ten invariants (section 13) are checked on random sessions and on every path through small ones in every test run. Section 12 says of every latency and memory figure whether it is measured and held, or still a design target, and which one is not met.

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
| Control | One single-writer actor per session driving `Kernel::step(now, event, &mut out)` | `step` never awaits, does no I/O and reads no clock. The actor waits in one place only: when no part has anything for it |
| Media | `Ingress` (reorder, decode, denoise, resample, fan-out) and `Playout` (buffer, pace, fade, encode) | Audio samples never enter the kernel. Plain structs, static dispatch, pooled frames, no allocation |
| Compute | Voice-activity detection, end-of-turn, interruption classifier, neural noise suppression | OS threads of their own. One model instance per compute thread, shared by every session, batched across sessions |

The whole control plane of a session is one task, and this is all it does:

```rust
loop {
    let events = parts.gather(now);            // every part looked at once, in a fixed order
    for event in events {
        kernel.step(now, &event, &mut out);    // pure; target under 10 µs
        journal.enqueue(&out.facts);           // serialised here, stored off the session's path
        for cmd in out.cmds.drain(..) {
            parts.carry_out(cmd);              // an ordinary call; never waits
        }
    }
    transport.send(frames_due(now));
    if nothing_moved {
        sleep_until_woken_or(next_deadline).await;   // the only await
    }
}
```

The parts are the transport, a recogniser for each participant, the answers of the listening models, the responses that are being written, the voice (synthesis and playout), the tool calls, the journal, and what the application asks. None of them is a task of its own, and there is no queue between two of them. A command is a call on the part it is for, so when the kernel says stop, playback has stopped before the next event is looked at. The order in which the parts are looked at is fixed and nothing in a session is random, so a session on a clock that a test controls does the same thing every time. A session that has more to do than fits in one run gives way to the other sessions of its shard and goes on at its next turn.

Two alternatives were rejected. A chain of per-processor tasks joined by queues (Pipecat's model) makes an interruption a message racing data through N queues, which is the structural cause of its race bugs. A compiled static dataflow graph is right for the per-frame audio chain, and waveport uses one there, but turn-taking is reactive control with cancellation, not dataflow.

### 4.2 Executor and shards

- The process runs N shards. Each shard is a thread with a single-threaded Tokio `LocalRuntime`, and a session stays on one shard for life. A new session goes to the shard that has the fewest. The threads are not pinned to cores yet. The density measurement says what pinning is worth (section 12): on a machine whose cores are not alike, a twentieth of what a session takes of its thread when the machine runs at full speed and a fifth when it runs slowly, and most of the long runs either way.
- The session path therefore has no cross-thread wake-ups and no locks, the kernel-to-playout call is an ordinary function call, and production has the same topology as the simulator.
- The cost is head-of-line blocking. Inline work is capped at 250 µs per call. Every run of a session is timed, and each shard counts the runs that took longer and keeps the longest; the CI gate on those counts comes with the other performance gates. Opus and SIMD resampling fit that budget and run inline. The cap was 100 µs until the phase 1 spike measured an Opus encode at 24 kHz: 110 to 140 µs on average and up to 215 µs, depending on encoder complexity.
- A session that panics ends alone. Its shard and the other sessions on it go on, whoever waits for the session is told that it panicked, and what it had recorded is in its journal.
- Adapters are still required to be `Send`, so moving to a work-stealing runtime stays possible. If pacing jitter misses its target, the media loop moves to a sibling thread per shard without touching the kernel.
- Model batching: each stream of audio has rings of its own to the thread that judges frames, one for where each frame begins and one for the samples, and a ring back for the answers. Neither side waits for the other: a frame that finds no room is dropped whole and counted. The models that judge a whole turn are slower and are asked far less often, and have a thread of their own. What a session and those threads hand each other is in one module of the runtime, and every way its threads can take turns over it is tried with `loom` (section 14). Every model fires immediately with whatever is queued, up to 64 frames for voice activity, so batches grow by themselves as load grows. The spike measured the alternative, waiting up to 2 ms to fill a batch: it saves 14% of inference time and misses the latency target in section 12. Results return as kernel events stamped with media time, so batching cannot reorder them.

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
- A timer only wakes the kernel. When one fires, the kernel reads from its own state and from `now` whether the thing it was set for is due, so a timer that fires late, twice, or after it was cancelled does no harm.
- Speech edges are ordered by where they are in the audio, not by when they arrive: a start from before speech that has already ended, or an end from before the stretch under way, is a repeat and is ignored.

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

- Text-to-speech and speech-to-speech adapters attach alignment marks `(text_offset, sample_offset)` to the audio they return. The marks stay with the audio, in the media plane's playout ledger; the kernel stores none. A provider without word timing yields one mark per segment, and the ledger interpolates inside the segment by share of characters. That mark arrives when the segment's audio ends; a response interrupted before then is cut where the segment begins.
- A provider with word timing marks where each word ends, and nothing else: the space and the full stop that follow a word have no mark of their own. Text is therefore taken to be synthesised to the end of what the synthesiser was given as soon as its last word is marked. A word that is never marked, because the provider reads it otherwise than it was written, holds what follows back only until the provider has given nothing for 400 ms.
- `Playout` reports how far playback has got: audio handed to the transport, audio acknowledged, and how much of the text has been heard in full, which is the last mark at or before `played_upto − estimated_client_buffer`. It uses transport playout acknowledgements where they exist.
- The runtime delivers the latest report to the kernel ahead of any other event, so an interruption is decided on what had been heard by then. A final position sent after the stop would carry the tag of a response that is no longer live, and be dropped.
- On completion or interruption the kernel commits exactly one assistant item: the planned text cut at the heard offset, flagged `truncated` when cut. A response of which nothing was heard commits nothing.
- LLM deltas never touch the context. They pass through the kernel, which releases text to synthesis by the chunking policy and the credit rule of section 5, so committed text is a prefix of released text by construction.
- `Playout::push(tag, now, chunk)` refuses audio whose tag is not that of the newest response there has been, so an adapter that keeps sending after a cancel is harmless. Audio for the newest response is taken before the kernel allows its playback to start as well as after, because the two can arrive in either order.
- What playback reports never goes back: not the audio played, and not the text heard. A transport cannot acknowledge more than it was handed.
- When a speech-to-speech provider supports truncation it receives the same cut point. When it does not, the divergence between its server-side context and the kernel's is recorded explicitly.

### 4.7 Barge-in

1. User speech that overlaps agent speech only **ducks** the agent (target: within 150 ms of speech onset, measured where audio leaves the server). Frames already handed to the transport cannot be ducked, which is why the send-ahead in section 5 is one frame.
2. The interruption classifier answers `Backchannel`, `BargeIn` or `Noise`. A policy timeout answers for it if it is slow.
3. `BargeIn` commits: the epoch is bumped, playout stops with a 5 to 10 ms fade, and the utterance is committed up to what was heard. Any other verdict un-ducks and the agent carries on.

The classifier is a port. Phase 1 ships a voice-activity-only policy; phase 5 ships an open-weights model.

Around these three steps the kernel keeps to a few rules:

- Where the far end can discard audio it holds, it is told to at a `BargeIn`, after the fade has gone out: no frame of a response follows the request to discard it. What the far end had played by then is what was heard.
- A turn does not end while it is undecided what its speech over the agent was. Speech taken for a backchannel or for noise is dismissed with its words, for that stated reason. A `BargeIn` verdict that arrives after the timeout still counts for as long as the turn is open.
- A response is not played while a user turn is open: the agent waits for a gap. If a newer turn meant for the agent is committed before anyone has heard the response, the response is replaced; once it is being heard, the next one waits for it.
- What the application asks the agent to say takes the floor at once, is never replaced by a response, and is not an answer: a turn that was owed a response is still owed one afterwards.

### 4.8 Tools

A tool call moves through `Proposed → Authorized → Dispatched` and ends once, as `Completed`, `Failed`, `Cancelled` or `InDoubt`. Its request and its result are items of the conversation, so the model that is told the result is also told how the call ended.

- Each `ToolSpec` declares an **effect class**: `Pure` (changes nothing), `Idempotent` (repeating it changes nothing more) or `SideEffecting` (must happen at most once).
- Each `ToolSpec` can declare a **gate**, for example "after the agent's own utterance has finished playing" for an end-call tool. A call whose gate never opens fails.
- A call is accepted only from a response that holds the floor and whose model is still writing, for a tool its agent has. A response that is cut off takes the calls it had not sent with it: they end `Cancelled`. Calls that were sent run on, and their results still enter the conversation.
- Before a call that is not `Pure` is dispatched, the kernel waits until the journal record that authorises the dispatch is durable (see section 9). Waiting for the record that proposes the call is not enough: after a crash, recovery would find only the proposal, conclude that the call never ran, and report a failure for an effect that happened. If the record is not stored within the tool's timeout, the call fails without having been sent.
- A tool that does not answer within its timeout is told to cancel. A `Pure` call then fails. Any other call may have taken effect: the tool has a short time to report that it undid the call, which ends it `Cancelled`, and otherwise the call ends `InDoubt`. A result that arrives in that time still counts.
- After a crash, a call that was never authorised fails: it was not sent. An authorised call may have been sent, whether or not the record of that survived. A `Pure` or `Idempotent` one is sent again, as a further attempt, up to three times. A `SideEffecting` one ends `InDoubt`, and the model is told that the outcome is unknown.
- The calls one response asks for form a **batch**. When all of them have ended and the response has finished writing, the batch is resolved and one response, the continuation, is owed for the results. The news of a tool never takes the floor: the continuation waits for a response that is being written or heard, and only a new user turn replaces a response nobody has heard.
- A tool can be run by another agent of the session instead of by the application (section 10).
- A session that is closing sends nothing new. It withdraws the calls it had not sent, cancels the `Pure` ones, and waits up to the grace time for the others, which then end `InDoubt`.
- Tool results enter the context exactly once, whatever happened to the turn that requested them.

### 4.9 Shutdown

The lifecycle is `Running → Draining → Closing → Closed`. A session that is draining lets the agent finish what it is saying and starts nothing new; it moves on as soon as nothing is under way, or at the first step after the drain time has passed, whichever event that step is for. Two timers, for the drain and for the grace time, make sure such a step happens even if one of them is lost. No command waits for an adapter, and an adapter that does not answer is dropped with the session. `Closed` is reached within drain time plus grace time whatever any adapter does.

A session that has closed says so. Its transport tells the far end that the session is over, and each recogniser is told that no more audio follows and is read until its stream ends, which is when an adapter tells its provider that it is done. Both are waited for together, for half a second at most: a far end that does not answer a goodbye is not worth more, and neither is a provider. A recogniser of somebody who leaves while the session goes on is let go of the same way, without anything waiting for it.

A session whose transport ends is closed as abandoned, and what the agent was saying is cut off where it was heard to. It is closed first and cut off after, so that the response that was owed next is not started for nobody. A session that cannot hear, because its recogniser failed six times in a row without a transcript in between, is closed as failed. A session that is told its journal belongs to another owner stops where it is: it falls silent at once and asks nothing more of anyone, the tools included. It says no goodbye either: the session is not over, the other owner has it.

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
| Transport → reorder buffer | A wait of 0–60 ms that adapts; 10 packets (200 ms) held at most | A late packet is dropped. A gap is made up by the codec for 60 ms, then filled with silence up to 1 s. A longer one breaks the stream off, and it begins anew when the next packet arrives |
| Ingress → voice-activity batcher | About 100 ms ring | Drop the frame, hold the last state, count it |
| Ingress → speech-to-text | 2 s ring | Drop oldest, mark the session degraded, fail over |
| Adapter → kernel | No queue. The session reads a stream when it runs | What is not read stays in the adapter, so pressure reaches the provider socket |
| Kernel → adapter commands | No queue. Calls that never wait | Text an adapter has no room for is kept and given when it has. A request for a transcript that is refused is counted |
| Application → session | 256 requests | The request is refused, and the application is told that the session is behind |
| Session → a follower | 256 things told | The follower is let go, and follows again from the last fact it saw |
| LLM → text-to-speech | Credit: the next piece of text is released once the one before has come back as audio, and while synthesized-minus-played is 3 s or less | Hold text; stop reading the LLM stream above 8 KB pending. A model that writes on to twice that is cancelled, and what it wrote is spoken |
| Text-to-speech → playout buffer | 3 s of audio | The driver stops reading the provider socket |
| Playout → transport | 20 ms frames; 20 ms send-ahead by default, 40 ms at most | Never queue beyond the lead |

- High-rate signals (playback position, audio levels, interim transcripts) are latest-value slots, not queues.
- The 3-second credit caps wasted text-to-speech spend at about 3 seconds per interruption.
- The reorder buffer holds nothing back to smooth the pace, because the listeners are machines. A packet is waited for only when a later one has overtaken it. The wait starts at nothing, grows by 20 ms each time a packet arrives after it was given up, and shrinks by 20 ms after 10 s in which none did.
- Filling a gap keeps the stream's length, so the media clock follows the sender's timestamps for as long as the stream is unbroken. It is set by the session clock again each time the stream begins anew. A sender whose clock drifts is not corrected for in phase 1.
- Pacing uses absolute deadlines from one timer tick per shard; the first frame of an utterance goes out immediately. When the audio runs out in the middle of a response, or a tick comes more than 100 ms late, the pace starts again from that moment: what was due in between is not sent in a burst.
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

Every port says what it can do (section 8.2), and a pure function decides from that how the session is run: `plan(&AgentNeeds, &BrainProfile, &TransportCaps) -> Result<SessionPlan, PlanError>`. It runs before the session starts. For a cascaded session it settles:

| Question | Answer |
|---|---|
| The rate of the audio handed to the recogniser | 16 kHz, or the nearest rate the recogniser takes |
| Whether the recogniser is asked for a final transcript when a turn ends | Yes if it can be asked; otherwise the turn waits until the recogniser notices |
| The rate speech is synthesised at | 24 kHz, or less if the wire carries less, or the nearest rate the synthesiser gives |
| How the synthesiser's marks are read | As exact times when it gives word or character times; otherwise spread over the audio by share of characters |
| Whether what was heard is what the far end says it played | Yes if it says; otherwise it is worked out from the pace at which audio was sent |
| Whether the far end is told to discard audio at an interruption | Yes if it can |

A session that cannot be run at all is refused: no rate in common, or an agent with tools and a model without tool calls. A session that can be run, but less well than with better parts, is run, and the plan lists each `Shortfall` so that it can be shown and logged: transcript times no finer than the audio sent, transcripts that cannot be asked for, spoken text cut by share of characters, playback that is estimated, audio that cannot be taken back. Nothing degrades silently. A speech-to-speech profile is refused for now, by name.

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

The planner of section 6 will decide this too, at session start and at every handoff. It picks the strongest strategy available and fails loudly instead of degrading silently:

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

Adapters hold no interruption logic. The runtime's drivers own tagging, cancellation by drop, retries and metrics. A helper in the adapter kit builds both halves of a stream port from one `async` block that reads commands and sends events over bounded queues. The receive half owns that block and runs it whenever it is polled, so an adapter spawns no task, and dropping the receive half ends the stream and everything it holds. A send half that is behind says `Full` and takes nothing; it never waits.

### 8.2 Driven ports

As built in `waveport-ports`, in outline:

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Speech-to-text. `Tts` has the same shape, with `try_speak` and `try_finish`.
pub trait Stt: Send + Sync + 'static {
    fn caps(&self) -> SttCaps; // rates, interim results, timing, who decides a transcript is final
    fn open(&self, config: SttConfig) -> BoxFuture<'_, Result<SttStream, PortError>>;
}
pub struct SttStream { pub tx: Box<dyn SttTx>, pub rx: Pin<Box<dyn SttRx>> }
pub trait SttTx: Send {
    fn try_send(&mut self, audio: AudioRef<'_>) -> Result<(), Full>;
    fn try_finalize(&mut self) -> Result<(), Full>;
}
pub trait SttRx: Send {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>)
        -> Poll<Option<Result<SttEvent, PortError>>>;
}

/// A language model has no send half: a request is whole when it starts.
pub trait Llm: Send + Sync + 'static {
    fn caps(&self) -> LlmCaps; // tool calls, several at once
    fn start<'a>(&'a self, request: LlmRequest<'a>)
        -> BoxFuture<'a, Result<Pin<Box<dyn LlmRx>>, PortError>>;
}

pub trait Transport: Send + 'static {
    fn caps(&self) -> TransportCaps; // codec each way, remote buffer (clearable, says what it played), text
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>>;
    fn try_send(&mut self, frame: FrameOut<'_>) -> Result<(), Full>;
    fn try_control(&mut self, control: TransportControl) -> Result<(), Full>; // Clear
    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<()>; // tells the far end that the session is over
}

pub trait Journal: Send + Sync + 'static {
    fn acquire(&self, session: SessionId) -> BoxFuture<'_, Result<Recovered, JournalError>>;
    fn append(&self, session: SessionId, lease: Lease, batch: Batch)
        -> BoxFuture<'_, Result<Seq, JournalError>>;
    fn snapshot(&self, session: SessionId, lease: Lease, snapshot: Snapshot)
        -> BoxFuture<'_, Result<(), JournalError>>;
}

pub trait ToolExecutor: Send + Sync + 'static {
    fn call<'a>(&'a self, request: ToolRequest<'a>) -> BoxFuture<'a, ToolReply>;
    fn undo(&self, session: SessionId, call: ToolCallId) -> BoxFuture<'_, bool>;
}

/// Compute plane: batched across sessions, one instance per compute thread.
pub trait VadModel: Send + 'static {
    type State: Default + Send;
    fn infer(&mut self, frames: &[&[i16]], states: &mut [Self::State], out: &mut [f32])
        -> Result<(), PortError>;
}
```

`TurnModel` and `InterruptionClassifier` have the shape of `VadModel` without the state. `EventSink` is given the facts of each step and the marks of each stage of a turn; it is never called inside the kernel. A list of sinks is a sink, so a session tells as many as it is given. Section 8.5 says what the marks are.

What the outline does not show:

- **Audio.** Rates and codecs are named in the domain (`waveport_domain::audio`), so a port or an adapter names a codec without linking one. Audio for a recogniser is lent (`AudioRef`: where it begins, its rate, its samples). Audio from a synthesiser is `PcmBytes`: the bytes as they came off the wire, read as samples where they are used, so an adapter copies nothing.
- **Marks.** A synthesiser's mark says that the text up to a byte offset ends at a point in the audio. The plan says how they are read (section 6).
- **The journal.** `acquire` takes a session over: it answers with a lease, the newest snapshot, the records since, and the number of the next record. Whoever held the session before is fenced from then on. An `append` is taken only with the current lease and the expected number, and answers with the number of the last record that is safe; the records are bytes the journal does not read. `snapshot` stores the session's state as of one of its records, in place of the records up to that one. `hurry` says that the session is waiting for what it handed over, which a journal that gathers requests before it stores them takes as a reason not to gather.
- **Tools.** `undo` takes back a call whose effect the kernel has given up on (section 4.8), and says whether it could.
- **Errors** carry a retry class: transient, rate-limited (with how long to wait, where the provider says), not authorised, permanent or timed out.

Not built yet:

```rust
/// Send half of a speech-to-speech session. Every command is answered by Ack or Nack.
pub trait RtTx: Send {
    fn try_audio(&mut self, audio: AudioRef<'_>) -> Result<(), Full>;
    fn try_cmd(&mut self, cmd: RtCmd) -> Result<(), Full>;
    // RtCmd: UpdateSession, InjectItem, CreateResponse, Cancel, Truncate, ToolResult
}
```

| Port | Style |
|---|---|
| `Denoise` | Synchronous per-stream `process(&mut [f32])` |
| `Bus`, `Retriever` | Boxed-future request ports |
| Clock | Lives only in the runtime; the kernel receives `now` and emits `SetTimer` |

### 8.3 What adapters share

`waveport-adapter-kit` holds what every provider adapter would otherwise write for itself:

| Part | What it does |
|---|---|
| Stream helper | Both halves of a stream port from one `async` block (section 8.1) |
| WebSocket client | Connects with a time limit, sends and receives text and binary messages, and turns a close code into a retry class |
| WebSocket server side | Takes a connection that was made to this machine: one path, one protocol that the client has to offer, messages of a megabyte at most, a time limit for the request |
| Kept connections | A connection whose work is done is kept for the work that comes next, since making one takes three trips to a provider and back. One that turns out to be gone before the provider was heard on it is replaced, and the new one is told what the old one was |
| HTTP client | A POST whose answer is read a piece at a time; a status that is not a success becomes an error with its retry class and the provider's `Retry-After` |
| Server-sent events | A parser that is given bytes as they arrive, with a limit on the length of an event |
| Secrets | A key type that prints as `Secret(…)` and is struck out of any text that may be shown |
| Retry | The class of an HTTP status or a close code, and a backoff with jitter |
| Timeline | Turns a recogniser's times, counted from the audio it was sent, into media time |

Both clients use one TLS configuration: `rustls` with `ring`, and certificates checked by the platform's own verifier.

### 8.4 Driving ports (sketches)

```rust
pub trait SessionControl: Send + Sync + 'static {
    fn send(&self, session: SessionId, input: SessionInput) -> Result<(), ControlError>;
    // SessionInput: Text, Say, Interrupt, Handoff, Close
    fn subscribe(&self, session: SessionId, from: Seq)
        -> Result<Pin<Box<dyn EventRx>>, ControlError>;       // resumable by Seq
}

pub trait Dispatch {
    fn offer(&self, job: JobOffer) -> Offer;
    fn load(&self) -> LoadReport;
    fn drain(&self, by: MonoTime);
}
```

`SessionControl` is built, and the runtime implements it. `Dispatch` is a sketch.

- Whoever follows a session is told two kinds of thing: the facts of the session, each with its number and in the form the journal holds it in, and the news that is not kept, such as who is speaking. A follower that comes back after a break names the first fact it has not seen. A session holds its last 128 facts for that; a follower that asks for older ones is told at which fact what follows begins, and has the ones before it from the journal.
- A request is taken or refused at once. A session that has 256 requests waiting refuses the next.
- Starting a session is a method of the runtime and not of the port. What a session is made of (its transport, its providers, its tools) is decided where the parts are put together, and a port cannot name that yet. It joins the port with the daemon, which starts sessions from a description. `migrate` comes with the fleet.

### 8.5 The adapters there are

| Crate | Port | What it is |
|---|---|---|
| `waveport-models-onnx` | `VadModel`, `TurnModel` | Silero VAD v6.2.3 and Smart Turn v3.2, run by ONNX Runtime. The library and the models are files, loaded when the program starts from the directory that `cargo xtask fetch-assets` fills; nothing is downloaded by a build |
| `waveport-stt-openai` | `Stt` | OpenAI's realtime transcription over one WebSocket, at 24 kHz. The provider is told not to judge where a turn ends, and writes a final transcript when it is asked for one. A session lasts an hour, and its end is a failure worth another try |
| `waveport-llm-openai-compat` | `Llm` | Chat completions read as server-sent events, in two dialects: OpenAI's own, and one for every other server that speaks the protocol, a model on this machine among them |
| `waveport-stt-deepgram` | `Stt` | Deepgram's live transcription over one WebSocket, at the rate of the session's audio, with the time of every word. The provider settles what it has heard whenever the speaker pauses, and also when it is asked to; it does not promise an answer to that, so a request that gets none within two seconds is answered with nothing more. A connection that carries no audio is kept alive |
| `waveport-stt-assemblyai` | `Stt` | AssemblyAI's streaming transcription over one WebSocket, with the time of every word. A stream opens only when the provider has begun a session with the model that was asked for. Audio is put together into messages of the size the provider takes. A turn is given once, when the provider says it is over, which it also does when asked |
| `waveport-tts-cartesia` | `Tts` | One WebSocket, one context per response, audio at 24 kHz with the time each word ends at. A connection is kept from one response to the next |
| `waveport-tts-elevenlabs` | `Tts` | One WebSocket, one context per response, audio at the one rate the adapter was set up with (24 kHz unless told), and with every piece of audio the characters spoken in it and when. A connection is kept from one response to the next |
| `waveport-transport-local-audio` | `Transport` | The microphone and the loudspeaker of this machine, through `cpal`, or a caller who follows a script |
| `waveport-transport-websocket` | `Transport` | A far end that connects to this machine over a WebSocket, and the client for it |
| `waveport-store-memory` | `Journal` | Keeps the rules of the port and nothing past the end of its process |
| `waveport-store-journal` | `Journal` | One file on this machine, from which a session goes on after its process died. Section 9 says how it is kept |
| `waveport-telemetry-otel` | `EventSink` | Tells a collector what sessions do, as OpenTelemetry traces and metrics over HTTP. One trace for each turn that is answered: the turn, inside it each response of the model, inside that each tool call. Counts of turns, utterances and tool calls, and how the lengths of the parts of a turn are spread. No word of a call is sent: not what was said, and not what a tool was asked or what it answered. The exporter tells nobody when a request fails, so what could not be sent is counted and can be asked for |

The crate documentation of each provider adapter lists what was learnt from the provider, or read in its documentation. The adapters for OpenAI were held against the provider, and their tests replay sessions recorded from it. **The adapters for Cartesia, Deepgram, AssemblyAI and ElevenLabs were written from the providers' documentation and have not been held against the providers yet.** Their tests replay messages that were made up in the documented shapes, and each has tests named `live_*` that talk to the provider and wait for a key (section 14).

**What a session tells.** After each step the session gives its sinks the facts of the step, and a mark for each stage a turn went through: the final transcript, the end of the turn, the model asked, its first word, the first text given to speech synthesis, the first audio back, the first frame handed to the transport, a response lowered, a response stopped, a tool call sent and back, and the journal's word that records are stored. A mark carries the session's own time. What lies between two marks is a part of a turn, and `waveport-adapter-kit` adds the marks up into those parts (`turns`) for whoever wants them: the telemetry adapter, the report that `waveport dev --report` and `waveport run --report` write when a call is over, and the performance gates of section 12. The order is the runtime's and a sink has to live with it: the utterance of a response that was spoken over is recorded before the mark that says it was stopped. Telemetry is switched on by `--otlp <address>` and by nothing in the environment alone; the headers a collector asks for are read from `OTEL_EXPORTER_OTLP_HEADERS` by the exporter, so no key is an argument. **The adapter has been held against a stand-in that reads what it is sent as the protocol's own messages, and not yet against a collector that somebody runs.**

**The local-audio transport** carries plain samples at 48 kHz both ways, in frames of 20 ms. A sound card asks for audio on a thread of its own that must never wait, so the two sides share nothing that locks: one ring of samples each way, and a few numbers that are read and written whole.

- Each response is a run with a number. Every sample in the ring carries the number of its run, so the device side counts what it plays of each, and the session is told how much of a response was played: the far end says what it played (section 4.6), to the sample.
- "Played" is counted where a sample is handed to the sound system. The listener hears it a few milliseconds to a few tens of milliseconds later, by a delay the sound system does not tell.
- At an interruption the session names the run to discard. The device side fades it out over 5 ms and throws the rest away, and audio of that run that is still on its way is dropped when it arrives.
- A run starts playing only when 25 ms more than the device asked for is in hand, or the run has ended. A device that asks for more than there is plays silence for what is missing; nothing is played twice. A moment in which the sound system itself did not keep time is counted as a stumble, and the count is shown when the call ends.
- Only devices that work at 48 kHz are taken. Several channels of a microphone are averaged into one, and the agent's voice goes to every channel of a loudspeaker.
- Nothing takes the agent's voice out of what the microphone hears. With headphones this does not matter. Without them `half_duplex` silences the microphone while the agent speaks and for 300 ms after, which also means the agent cannot be interrupted.
- In place of the sound card there can be a person who follows a script (`Step`: say this recording, stay silent this long, listen until the agent has finished), moved on by the session's own clock in ticks of 10 ms. What that person is played is kept as a recording. A scripted session takes the same path as a spoken one from the ring onwards, and is the same every time it is run.

**The WebSocket transport** is for a far end on this machine or on a network that is trusted: another terminal, a test, a program that wants to talk to an agent without a sound card of its own. It listens on the loopback interface, and on another only when it is told in so many words that it may be exposed. Nothing in it asks a caller who they are. Its protocol is named `waveport.ws.v0`, and it is not the client protocol of phase 2: it has no way to come back to a session after the connection is lost, and a connection that drops is a caller who left. Nor does it carry a resume token: a call that was under way when `waveport serve` died is in the journal, and no caller can go on with it yet.

| Message | From | What it says |
|---|---|---|
| `hello {rate}` | client | The first message: the rate of the audio, both ways |
| `ready {session, rate}` | server | The session has begun |
| binary | both | Audio as plain samples, 16 bits, little-endian. From the client it is the microphone; from the server it is the response that was last marked |
| `mark {run}` | server | The audio that follows belongs to response `run`. With `end: true`, that response has no more audio |
| `clear {run}` | server | Throw away what is left of response `run`: the agent was interrupted |
| `played {run, samples}` | client | How much of response `run` has been played, in samples. This is what lets an interrupted response be cut where it was heard to |
| `text {text}` | client | Text typed to the agent |
| `event {text}` | server | A line about the call for a person to read |
| `bye` | both | The call is over. The server says why, in `reason` |

Audio from the client is put into packets of 20 ms on the session's clock. A client that sends nothing for more than a second is taken to have paused, and its audio goes on at the time it comes back.

A client sends audio all the time, also in the moment it is told that the call is over. So the server does not let go of the connection when it has said goodbye: it reads what the client still sends until the client has closed too, for 300 ms at most. A connection that is let go of with something unread is reset, and a reset can cost the client the goodbye it had not read yet, so that a call that ended would look like one that broke off. The client, for its part, looks at what the server says before it sends, and when it can send no more it still reads what the server had said.

**Tools** are registered with the runtime's `Registry`, which is the `ToolExecutor` of a session: each tool is a name, a description and a JSON schema for the model, an effect class and a gate for the kernel (section 4.8), and a function. Arguments are checked by reading them into the function's own type, and a tool that is given what it cannot read answers the model with the reason.

**The `waveport` command** (`apps/waveport-cli`) is where the parts are put together: `waveport dev` runs an agent on the sound devices, `waveport run --script` runs it against a scripted caller, `waveport serve` takes calls over the WebSocket transport, as many at a time as come, `waveport client` makes one from the sound devices or from a script, and `waveport devices` lists the devices. `--stt` and `--tts` say who recognises and who synthesises speech; each provider brings its own address, model, voice and the name of the variable that holds its key. A key is never an argument: the command is given the name of the environment variable that holds it. The command has two tools: `get_time`, which changes nothing, and `end_call`, which has an effect and is gated on the agent's own words having been heard, so that the agent says goodbye before the line closes. With `--notes <FILE>` it has a third, `take_note`, which adds a line to the file: an effect that can be counted afterwards.

The command keeps its calls in a journal in a file, `.waveport/journal.redb` in the folder it is run in unless `--journal` names another; `--no-journal` keeps them in memory. A call that is begun by `dev` or `run` gets a resume token, which is written to a file next to the journal that only its owner can read, and `--resume last` goes on with that call in a later process: the session is rebuilt from the journal and says from what. `--resume <FILE>` goes on with the call a file has the token of. A token is never an argument and is never printed, for the reason a key is not. A call that ends is taken out of the journal, and one that broke off stays until it has been gone on with to its end: the command says when it starts how many there are, and there is no command yet that lists them or lets go of one. A journal that cannot be read stops the command, which names the file and leaves it as it is. Tools are journaled by their place in the list, so a call has to be gone on with by a command with the same tools.

## 9. Durability

Waveport stores a snapshot plus a journal of **domain events**, not inputs. Recovery is `snapshot ⊕ apply(tail)`. Replaying inputs through `decide` is rejected for recovery, because during a rolling deploy the target node runs newer logic. Inputs are still sampled into a flight recorder for replay tests.

| | |
|---|---|
| Journaled | Turn commits, context items, utterance commits, tool-call transitions, agent and flow transitions, participant and configuration changes, and a playout checkpoint each second while speaking. Expect 5 to 30 records per turn |
| Not journaled | Audio, token deltas, voice-activity edges, interim transcripts |
| Hot-path cost | The fact is written as a record on the session's thread and handed to the journal; a thread of the journal's own stores what it is handed, a group at a time, at most every 10 ms |
| One synchronous barrier | Before dispatching a call to a tool that is not `Pure`, the kernel emits `AwaitDurable(seq)` for the record that authorises the dispatch, and dispatches on `Durable` |
| Format | Facts and snapshots are versioned serde records defined in `waveport-protocol`. The domain crate stays free of serialisation: the runtime maps each fact to its record with an exhaustive match, so a new fact does not compile until it is mapped. Zero-copy archives (`rkyv`) were rejected for snapshots, because an archived layout cannot evolve and a newer build must read the state an older one wrote |
| Fencing | Appends are conditional on `(lease epoch, expected seq)`, so a superseded owner can neither write nor speak |
| A journal that does not answer | The append is tried again, after 50 ms and then after twice as long each time, up to 2 s. Nothing is lost meanwhile: the facts wait in order, and a tool call that has an effect waits with them. An append that is refused, because another owner has the session or the numbering does not fit, ends the session where it is |
| Snapshots | A session has its journal keep its state: what the kernel's snapshot holds, as a versioned record, and where the last audio ends that a turn was spoken in. A state is due after 256 records or a minute, and is taken when a turn or an utterance next enters the conversation, which is when a session holds least that is only under way; a session in which neither happens takes one once twice as many records wait. It is handed over when every fact it includes is stored, and then replaces the records up to that one. A state the journal fails to store is let go of, because the records are still there. A state that cannot be read, or is as of another record than the journal says, stops the session from starting: nothing is guessed |

Three recovery levels:

| Level | What happens | Target |
|---|---|---|
| Client reconnect | The kernel waits in a detached state, the transport rebinds, and the client resyncs from its last sequence number | No loss |
| Process crash | A new node loads snapshot and tail and reopens provider streams; the client reconnects with a resume token | Within 2 s of lease expiry |
| Live migration | The target pre-opens provider connections; the source waits for a safe point (floor idle, no live generation; forced at a deadline), freezes, flushes and hands over the lease | Drain time becomes "time to the next turn boundary" |

Of the three, phase 1 has the second on one machine: a process that died is followed by another that is started with the call's resume token, and the caller is whoever sits at that machine. Waiting detached for a client to come back, a lease that expires, and migration come with the daemon and the fleet.

**The journal in a file** (`waveport-store-journal`) is one [redb](https://www.redb.org) database, which one process has open at a time. Its safety is redb's: a commit that was cut short anywhere, by a crash or by the power, leaves the file as it was after the commit before.

- A row for each session holds who owns it (the lease), the number its next record gets, the record its stored state is as of, and the hash of its resume token. The records are stored in the groups they were appended in, and the newest state of a session stands in place of the records it covers. A fourth table says which version of the layout the file is in. A file of a later version is not opened.
- One thread does everything that changes the file. It begins a commit every 10 ms at most, and stores in it everything that was asked of it since the one before: one sync of the file for all of it. It does not wait for the 10 ms to pass while someone is held up: when a session is taken over, when a token is issued or checked, or when a session has said that it waits (`Journal::hurry`, which the session calls while the kernel waits at the barrier of section 4.8). Requests that arrive during a commit are the next group, so the more sessions write, the more one sync stores.
- A commit of redb is in one phase by default: what tells a whole commit from a torn one is a checksum that is not made to withstand someone who chooses what is recorded, reads the file and stops the machine at an instant of their choosing. Two phases rule that out for a second sync per commit, and are a setting.
- If the write of a commit fails, nothing of its group is in the file, everyone in the group is told, and the same records may be handed over again. If the sync fails, the file may hold the group all the same. The journal counts its commits in the file, reads the count back, and if the commit is there says so as a failure that is not to be tried again: a session that then tried again would be told that its records are out of order, and would end for the wrong reason.
- A **resume token** is the session's name and 16 bytes from the operating system, written as `wpr1_` and 64 hexadecimal digits. Whoever holds it may take the session over. The file holds its SHA-256 hash only, a token is compared in constant time, and one is not shown when it is debugged. A new token for a session takes the place of the one before.
- The file holds what was said in every session. On Unix a file the journal makes is its owner's alone.

Measured on the machine this was written on, with the file on an NVMe disk under ext4: one session that waits for its records waits 1.5 ms, and 64 sessions that hand theirs over together wait 3.6 ms between them. In two phases one session waits about 4 ms, between 3 and 7 from one run to the next. Without a disk the same commits take 0.07 ms and 1.1 ms: the sync is most of a commit. These are times on one machine, read by `cargo bench --bench wall_journal`, and gate nothing.

What the journal in a file does not do yet. It uses redb 4.5.0, whose repair can open a file that is damaged in both of its commit slots as an empty one; 4.6.0 mends that, was published on 13 November 2026, and is taken once it is a week old, as every dependency is. The whole file is read and checked each time it is opened. Nothing but ending a call takes it out of the file. And the plain segment file that was planned as a fallback behind the same port is not written: redb has held in every test of section 14.

How the media moves during a migration depends on the transport: through an SFU or relay it re-joins (gap of 300 ms or less); direct WebRTC uses an ICE restart (0.3 to 1 s); a carrier WebSocket is bound to its TCP connection and cannot move unless a thin media edge holds that socket.

A snapshot holds what the facts add up to and nothing that was only under way: `Kernel::restore(snapshot, tail)` gives a session whose snapshot equals the live one's, from any earlier snapshot. What was under way is dealt with by the first step the rebuilt kernel takes, whatever event that step is for:

- The epoch advances, so that whatever still arrives for the session's earlier life is stale.
- Speech that was being heard is committed as far as its last checkpoint, flagged truncated.
- A response that was owed and not heard to its end is owed again. Whether a response was heard to its end is itself journaled, with the utterance, so an answer that was given is not given twice.
- Tool calls are dealt with as section 4.8 says, and a handoff that was under way is completed.
- If anything was lost, a notice says so in the conversation, so that the model knows why it is asked again.
- A session that was closing is closed.

What cannot be restored, and what happens instead:

| Lost state | Behaviour |
|---|---|
| In-flight LLM or text-to-speech work | Discarded and regenerated from committed context |
| A partially played utterance after a crash | Committed to the last checkpoint and flagged truncated, with up to 1 s of uncertainty |
| Speech-to-text hypothesis state | On migration the open turn's pre-roll (up to 8 s) travels with the snapshot and is re-fed. On a crash it is lost and the agent asks again. A recogniser that fails while its session lives is opened again, after 200 ms and then after twice as long each time, up to 5 s, and is given what was heard since its last final transcript, from the same 8 s |
| A speech-to-speech provider session | Server-side audio context is not portable. Use the provider's resumption handle if it has one; otherwise seed a new session from the kernel's text mirror (prosody is lost); otherwise from a summary |

## 10. Multi-agent

Agents are participants of kind `Agent` on the stage. At most one holds the floor per output track; the others are background agents.

| Mode | Mechanism |
|---|---|
| In-process handoff | A handoff request arrives, usually from a tool result, carrying full context, a summary, a filtered context or none. The kernel bumps the epoch, which stops the agent that is speaking, leaves a note for the new agent when it is to see a summary or nothing in place of the conversation, and starts no response until the parts that serve the new agent are reported ready. Then it activates the new agent. If they are not ready in time, the agent that had the floor keeps it. Recomputing the session plan and swapping drivers make-before-break is the runtime's part |
| Cross-process: remote brain | Session, media and floor stay put. The think role is filled by a bus-backed LLM adapter whose results carry the `OpTag`, so the same liveness rule discards stale remote output. Costs one bus round trip per turn |
| Cross-node: session transfer | The migration from section 9 to a node hosting the target agent, followed by activation |
| Background and parallel agents | A generation without playout, optionally parented by a tool call. One parented by a call ends with that call, and an interruption of the floor leaves it running. Its output lands as a tool result or a notice. To speak, it requests the floor, and the policy that arbitrates humans arbitrates agents. In phase 1 the only background agent is one that answers a tool call: what it writes is the call's result, and it cannot call tools itself |

Bus envelopes are `{ session, lease, seq, tag, payload }`, delivered at least once and deduplicated on `(session, seq)`.

## 11. Fleet

The fleet layer (`waveport-control`) is designed in detail in phase 3. Its fixed points:

- Nodes register with the control plane and report load: session count, CPU, and shard poll latency.
- The dispatcher sends a job offer; a node accepts or declines; placement grants a lease with a fencing epoch.
- Draining a node stops new offers and migrates its sessions at turn boundaries. Drain time has no cap.
- Autoscaling signals are session and capacity gauges exported for the cluster autoscaler, not CPU.
- A process hosts at most 2,000 sessions, to bound the blast radius of a crash.

## 12. Performance targets

All are design targets. The numbers that gates hold are in `perf/targets.toml`: for each, the target of this table and what the code does today. A change that does worse than today fails its gate, so a cost that is meant is a line of that file in the same pull request. Below the table: what holds each figure, and what was measured.

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

**What holds each figure, at the end of phase 1.**

| Figures | Held by | When |
|---|---|---|
| Kernel `step`; packet in; journal cost per record | Counts of instructions, which are the same from run to run (`cargo xtask bench-ir`). A change that makes one grow by more than 2% fails | Every pull request |
| Heap allocations per frame | An allocator that counts, around the kernel's steps and the media path | Every test run |
| First token → first request; first byte → first packet; end of turn → first packet; barge-in → no more audio; speech onset → duck | Whole calls on the session actor with a stand-in in every port, on a clock the test controls (`benches/tests/latency.rs`). Such a clock counts waiting and not work | Every test run |
| Memory of an idle and of an active voice session | The same calls under an allocator that counts (`benches/tests/memory.rs`) | Every test run |
| The first three of those latencies in real time; session start; the share of a thread that a session takes | Fifty calls on one shard of the runtime, in real time (`benches/tests/load.rs`), held to a record of the same machine in `perf/baselines` by `cargo xtask perf check` | By hand, on a machine that is left alone. Every night the share is held on the runner, as a ratio to reference work timed in the same run |
| Voice-activity result after the frame; memory of a parked or text-only session | Nothing yet. The model that listens is a stand-in in every gate, and phase 1 has no session that is parked or without a voice | |

Each of the last three kinds of gate was seen to fail on a change made to break it: an answer started 60 ms late (one started 20 ms late is not seen and is not late, because a frame is handed over 20 ms before it is due), 9 s to look back on in place of 8, and every frame encoded twice.

**Measured: latency.** With providers that answer at once, a session adds no wait of its own. On a clock the test controls, the end of a turn, the request to the model, its first word, the first text given to speech synthesis, the first audio and the first frame handed to the transport fall at the same instant, on lines that carry Opus and plain samples at 48 and at 8 kHz. A caller who speaks over the agent has it lowered 60 ms after beginning to, which is the time speech has to last before it counts as speech (two frames of the voice-activity model), and once the agent is stopped no further frame of it is handed over. With providers that take their time, what a caller waits is the sum of what they take and nothing besides. In real time, with fifty sessions on one shard, the end of a turn to the first frame is 0.01 ms for the usual turn and 0.03 ms for 99 in a hundred, and a session starts in 0.09 ms (0.19 ms). These are the session's share with stand-ins for the providers in the same process: what an adapter adds with a real connection is not in them.

**Measured: memory.** A session that listens holds 528 KB of the Rust heap on a line that carries Opus and 586 KB on one that carries plain samples at 48 kHz, and about 11 KB more while it speaks. What libopus keeps for itself is not counted. An active session is far inside its target. An idle one is within the 600 KB asked of 99 in a hundred and above the 300 KB asked of the usual one. **That target is not met, and reserving the buffers later would not meet it:** the 8 s a session keeps to look back on, which the end-of-turn model reads, and the 2 s it keeps for speech recognition are 312 KB at 16 kHz, and both are written whenever a session listens. Only the speech to play could wait for the first answer, and a session would hold it from then on. So the buffers stay reserved when a session is set up, which is also what keeps the media path from allocating, and the figure for an idle session is open: either it is restated at what the two windows cost, or the session looks back on less than the 8 s the end-of-turn model was trained on. Nothing is left of a session that is over: a thousand calls of a minute each, run every night, give back every byte.

**Measured: the shard.** Fifty calls at once on one shard, on lines that carry Opus, with an agent that speaks a little under half the time. On one performance core of a laptop (Core Ultra 7 155H) a session takes 0.23% of the shard's thread, 435 sessions to a thread; on a runner of GitHub's, 0.21%. The spike, with its own model of a session, had put the shard's part at 0.45%. This is the shard's thread and nothing else: the models that listen are stand-ins in this measurement, so the 0.71% the spike measured for them is not in it, and the planning figure below stands, since inference is what limits the number of cores.

**The same machine is not always the same machine.** Earlier on the day of its record the laptop took 1.8 times as long over the reference work, and over a session: 0.48% of a thread. The reason was not found; its charger is the likeliest. A record therefore says how long the reference work took, and `perf check` refuses a machine that takes a tenth longer or shorter over it. Every night the share is held on the runner as a ratio to the reference work of the same run, which was 0.73 there and 0.92 on the laptop's core: the ratio takes the speed of the processor out of the number and not everything else about a machine, so the number that is held is the runner's own, from one run so far, with a quarter to spare.

**The runtime does not pin the threads of its shards yet.** What pinning is worth depends on the state of the machine too. At full speed it is a twentieth of what a session takes of its thread (0.24% where the system moves the thread among cores that are not alike), and the runs of a session that go over the budget of 250 µs fall from 2 in a thousand to 3 in ten thousand. When the machine was slow it was a fifth (0.57 to 0.63%, against 0.48%), and from 3 in a hundred to 6 in a thousand. How long a session runs at a time is measured and recorded and not held by a gate: the longest run moved between 0.7 and 1.9 ms on unchanged code.

**Density.** The phase 1 spike measured one active cascaded session at 1.17% of a core ([decision 0001](decisions/0001-density-spike.md)): 0.45% on the shard thread, most of it the Opus encoder, 0.14% for voice-activity inference and 0.57% for end-of-turn inference at one inference every ten seconds. That is 85 sessions per core with nothing to spare. The planning figure is 60 active voice sessions per core, which keeps a core at 75% and allows a tenth for the work the spike did not model, and about 900 per 16-core node. One shard thread holds its timing up to about 150 sessions; inference is what limits the core count. The earlier estimate of 0.7 to 1% and target of 100 per core left out end-of-turn inference and assumed batching would make inference 2 to 4 times cheaper; the measured gain is 1.9 times. The measurement has limits, listed in the decision record: one laptop core at turbo frequency, with the rest of the machine nearly idle. Not measured: local neural noise suppression, which the earlier figures put at about 1.5% of a core per session, and text-only sessions, estimated at more than 5,000 per core. For comparison, LiveKit's own guidance is 10 to 25 jobs per 4 cores.

**Journal.** What a record costs the session that makes it is counted in instructions (`benches/ir/journal.rs`): 3,193 to write the turn of a caller who said a sentence as a record, and 338 to hand the four records of a step to the journal, whatever the journal is doing. That is the whole of the hot-path cost in the table, as a count and not a time. How long the records then take to be stored is in section 9.

**Media path.** The work of one 20 ms frame is counted in instructions (`benches/ir/media.rs`), because a count is the same from run to run and a time is not. A packet in costs about 234,000 instructions as Opus, 49,000 as G.711 and 107,000 as plain samples at 48 kHz. A frame out costs about 565,000 as Opus, 56,000 as G.711 and 143,000 at 48 kHz. The Opus figures are libopus at 32 kbit/s and its highest complexity; the other two are the change of rate. The path itself (reordering, the window, the queue, pacing, the ledger) is under 1,500 instructions either way. A pull request that makes any of them grow by more than 2% fails. These are counts on x86-64 and not times. The buffers of a voice session are reserved when it is set up, with the settings of section 5 (8 s to look back on, 2 s for speech recognition, 3 s of speech); what they come to, and what that means for the idle session's target, is above.

## 13. Invariants

These ten are stated formally and checked in simulation.

1. The epoch is monotonic. No playout start is emitted for a generation that is not live, and no tool dispatch for a response that was cut off.
2. Every accepted perception event ends in exactly one outcome: a committed user turn, or a turn dismissed for a stated reason (a backchannel, noise, no words, or the session closing while the turn was open).
3. Each utterance commits exactly once; committed text is a prefix of planned text and never extends past the audio handed to the transport.
4. A call ends exactly once. A dispatched call that may have changed something ends `Completed`, `Failed` or `InDoubt`, or `Cancelled` if its tool confirms that it undid the call. A call that is not pure is dispatched only once the record that authorises it is durable. A call is dispatched at most once per key within one lease, and after a crash it is dispatched again only if its tool is declared pure or idempotent. Its result enters the context exactly once.
5. Each tool batch starts at most one continuation in one life of the session. A continuation that a crash interrupted is started again.
6. At most one generation holds playout per output track.
7. From `Close`, `Closed` is reached within drain plus grace under any adapter behaviour.
8. All queues are bounded; `step` is total and deterministic.
9. `load(snapshot) ⊕ apply(tail)` equals live state: a session rebuilt from any snapshot and the facts journaled since holds what the live session's snapshot holds.
10. The facts a kernel emits are numbered without gaps, and once told that its lease is lost it emits nothing more. That at most one lease appends per session is the journal's guarantee (section 9) and is checked with the journal, since a lease is not kernel state.

Invariants 1 to 5 close the incumbents' race classes by construction. General robustness to adapter interleavings is established by exploration, not by construction.

## 14. Verification strategy

| Technique | Scope |
|---|---|
| Kernel scenarios | Six stories written before the kernel, each closing a race from section 4.5 or a session shape from section 6. A simulated world stands in for every adapter, and each story is run along every order in which its events could arrive. The scenario files are not edited to make them pass |
| Deterministic simulation, level 1 | A seed makes up a session: who is in it, when they speak, what the model writes and which tools it calls, which agent has the floor, and how each adapter answers. Six profiles decide what goes wrong: calm, chaos, stalls, duplicates, shutdown, and crashes, in which the process dies and the session goes on from what of its journal had reached the disk. `Kernel::step` is driven with a virtual clock, no executor needed, and a monitor that keeps its own account of the session checks the invariants after every step, that the journal alone adds up to the kernel's state, and that nothing is left unfinished at the end. Each session runs twice to show the kernel is deterministic. Every test run has 600 sessions per profile; a nightly run has 200,000, and other ones each night |
| Deterministic simulation, level 2 | The real session actor on an executor whose clock the test controls, with a fake in every port and the monitor of level 1 on every step. A seed makes up the session: who calls and what they say, how late every part answers, what the line does to the packets, which parts fail and when, what the application asks, and at which record of its journal the process dies, up to three times. The six stories of the kernel scenarios are told again under many seeds, and sessions nobody wrote are made up under four profiles: calm, chaos, crashes and shutdown. Beyond the invariants, every run is checked on what the far end was sent (frames in order and on time, none after a clear or a close), that nothing is in the conversation that the far end did not play, that no tool with an effect was called before the record that allows it was stored, or twice, that the journal has no gaps, and that a session rebuilt from the journal is the session that wrote it. Every test run has 250 sessions; a nightly run has 10,000, and other ones each night. Every failure is a seed |
| Property tests | The ten invariants over arbitrary interleavings |
| Small-scope model check | Every order in which the events of a small session can arrive: speech, transcripts and estimates, model text and tool requests, synthesis and playback, tool results, timers, an interruption, a close and the death of the process, with the same monitor checking each step. One session with all of that would be too large, so there are several, each without what another has: up to 2 participants, 3 turns, 2 tools and 2 calls. Every test run tries about 200,000 states, and the nightly run about 6 million |
| Replay | A session is written down as the events its kernel was given, a line each. Five such traces are kept with what the kernel emitted for every line, and each test run gives a new kernel the same events and compares: a change in what the kernel does shows as the first line that differs. A path on which the model check or the simulation fails can be written down and run again in the same way. Flight-recorder traces from the runtime will use the format |
| Random networks | A seed makes up the network between a sender and the inbound path: jitter, loss, repeats, outages, bursts, and packets that arrive together. Rules are checked on what comes out: one unbroken timeline, each packet once and in order, never more than 60 ms made up in a row, every deadline in the future, and counters that add up. 500 networks in every test run. The outbound path, the codecs and the change of rate have exact tests of their own: a tone must come through the filter at its level, and one above the band must be 55 dB down |
| Fuzzing | `step` with structured events; every wire parser (SIP, SDP, RTP, client protocol); journal decode with snapshot round trip |
| Concurrency checking | What a session and the compute plane hand each other: the frames of a stream, the answers, the wake-up of the session and of the thread that judges frames. `loom` tries every way the threads of small cases can take turns, in every test run, and fails on a frame that arrives in pieces or a wake-up that is missed. The rings themselves are `rtrb`'s and are stood in for there: what is tried is how they are used |
| Port conformance kit | Functions in `waveport-testkit` that hold any implementation of a port to the port's rules: a model's response ends once and says how, a recogniser's final transcripts are in order and inside the audio it was sent, a synthesiser's audio and marks add up, a journal fences and keeps its numbering, and a stream that is dropped stops. They run in every test run on the fakes, in every combination of what a fake can say it does, and on implementations made to break one rule at a time, to show that each rule is checked. Adapters run them against servers that replay what a provider said, or what its documentation says it says. Against the provider itself they run in a workflow of their own, every night and when somebody asks, for the providers whose keys the repository holds; that workflow never runs for a pull request, and a test fails if it is given a trigger for one |
| Whole sessions on fakes | A fake of every port, on virtual time: a transport whose far end speaks, plays what it is sent and says what it played; a recogniser, a model, a synthesiser and tools that follow a script; a journal in memory. The kernel, the audio path and the session actor are the real ones. Conversations written by hand are run through the ports and nothing else, one for each thing a session has to do or to live through: a question and its answer heard to the end, an interruption answered on the words that were heard, a tool call and its result, parts that can do less, what an application asks, a line that closes, a recogniser that fails, a journal that fails or is taken over, a process that dies, and a follower that comes back. Sessions on real threads are started, driven, followed and ended, one that panics among them |
| A scripted call | The `waveport` command itself is started, with a caller who follows a script in place of the sound devices and scripted servers in place of the three providers. The models that listen, the adapters, the runtime and the audio path are the real ones. The caller asks a question in recorded synthetic speech; the agent has to call a tool, answer aloud, say goodbye and end the call, and the recording of what the caller was played has to hold the answer and nothing after it. The call is run with each recogniser and each synthesiser there is an adapter for, and once more with the agent in one process and the caller in another, over the WebSocket transport. Runs in every test run, on all four platforms |
| A power cut in the middle of a commit | The journal in a file is put on a disk in memory that the test can break. A seed makes up a few sessions that write records, store states and are taken over, picks a commit and a write in it at which the power goes, and decides for every write that was not yet synced whether it is kept, lost or torn. The file is opened again, and nothing the journal had acknowledged may be gone, the commit that was cut has to be there whole for every session or for none, and the sessions have to go on. The run counts how often that commit was found whole and how often not at all, and fails if either never happened. A write that fails and a sync that fails have tests of their own. 60 cuts in every test run, and 20,000 others every night |
| A call that is killed | The `waveport` command is run as for a scripted call, with its journal in a file, and its process is killed as `kill -9` does it, at six places: while the model answers, while the agent speaks, once a tool has acted and before it has answered, while the record that allows a tool waits to be stored, and before and after the commit that stores the call's state. The places are too brief to hit from outside, so the command is told where to stop, says when it is there, and is killed then. It is started again with `--resume last` and a caller who asks once more. Each run checks that the second process goes on with the call, that the model is shown the conversation as the journal had it, that a tool that adds a line to a file has acted at most once in both processes together, that the caller is answered aloud, and that the call, once over, is not there to go on with. Runs in every test run, on all four platforms |
| CI gates | Instruction-count benchmarks for `step`, the per-frame media path and what a journal record costs its session; a counting allocator that fails on any allocation in the media path; whole calls on the session actor held to the latency and memory figures of `perf/targets.toml` in every test run, the interruption targets among them; every night a soak of a thousand calls that have to give back every byte, and the share of a thread that a session takes, as a ratio to reference work; a `kill -9` test that must resume. Section 12 says which figure is held by which, and that each of the three kinds was seen to fail on a change made to break it |

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

`xtask/`, `benches/`, `crates/core/`, `crates/testing/` (without `-evals`), `assets/`, `perf/` and `docs/` exist today (`spikes/` held the density spike, which was deleted once the product could measure itself). So do the first adapters (`transport/local-audio`, `transport/websocket`, `stt/openai`, `stt/deepgram`, `stt/assemblyai`, `llm/openai-compat`, `tts/cartesia`, `tts/elevenlabs`, `models/onnx`, which holds both listening models, `store/memory`, `store/journal` and `telemetry/otel`) and `apps/waveport-cli`. The rest is added in the phase that needs it.

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
| Codecs and DSP | `opus` 0.4.0 (libopus), `ezk-g722` 0.1.2, `nnnoiseless` 0.5.2, `rtrb` 0.4.1, `bytes` 1.12.1 | libopus is built from its source by `cmake` and a C compiler, and linked in. G.711 and the change of rate are written in-repo. Audio is at 8, 16, 24 or 48 kHz, so every change is by a small whole ratio, and a fixed filter with whole-number coefficients does it: the same samples on every platform, which `rubato` 5.1.0, in floating point, does not promise. Opus decodes and encodes at the rate wanted, so the main path changes no rate at all. DeepFilterNet is a later candidate (git dependency only) |
| Local inference | `ort` 2.0.0-rc.15 (ONNX Runtime 1.31) | Chosen over `tract` 0.23.10 by the phase 1 spike ([decision 0001](decisions/0001-density-spike.md)): `tract` cannot load the standard Silero export, and on Smart Turn it is 2.6 times slower and its outputs differ. `ort` inference needs exclusive access to a session, so each compute thread owns one |
| Bundled models | Silero VAD v6.2.3 (MIT, 2.3 MB); Smart Turn v3.2 (BSD-2, 8.7 MB) | Pinned by address and SHA-256 in `assets/assets.lock.toml` and fetched by `cargo xtask fetch-assets`; never committed. LiveKit's turn detector cannot be used: its licence restricts it to LiveKit Agents |
| Sound devices | `cpal` 0.18.2 | ALSA on Linux (the headers are needed to build: `libasound2-dev`), CoreAudio on macOS, WASAPI on Windows. Its streams can be held by any thread |
| Command line | `clap` 4.6.7, `jiff` 0.2.41 | `jiff` gives the `get_time` tool the local time and its zone |
| Local speech (optional) | `sherpa-onnx` 2.0.1 | Local LLMs run out of process behind the OpenAI-compatible adapter |
| QUIC and MoQ | `quinn` 0.11.12, `web-transport-quinn` 0.13.6, `moq-net` 0.3.23 | Experimental transport; the drafts are still moving |
| WebSocket, HTTP, gRPC | `tokio-tungstenite` 0.30.0, `axum` 0.8.9, `hyper` 1.12.0, `reqwest` 0.13.5, `tonic` 0.14.9, `prost` 0.14.6 | A breaking `tonic` release is pending. The WebSocket client does not go through a proxy; the HTTP client uses the proxy the environment names |
| TLS | `rustls` 0.23 with `ring`, `tokio-rustls` 0.26.6, `rustls-platform-verifier` 0.7.1 | No OpenSSL, and no assembler to install on Windows. Certificates are checked by the platform's own verifier, so no list of roots is compiled in |
| MCP | `rmcp` 3.10.0 | Official SDK |
| Bus | `async-nats` 0.52.0, `redis` 1.8.2 | `fred` is dormant |
| Stores | `sqlx` 0.9.0 (Postgres), `redb` 4.5.0 (local journal; 4.6.0 once it is a week old), `object_store` 0.14.2, `moka` 0.12.16 | Snapshots are versioned serde records, not `rkyv` archives; see section 9 |
| Knowledge | `qdrant-client` 1.20.0, `lancedb` 0.41.0, `pgvector` 0.4.2, `fastembed` 7.7.0 | |
| Observability | `opentelemetry`, `opentelemetry_sdk` and `opentelemetry-otlp` 0.33.1, over HTTP with protobuf and no gRPC; later `tracing` 0.1.44, `tracing-opentelemetry` 0.34.0, `metrics` 0.24.6, `dial9-tokio-telemetry` 0.5.8 | In use since phase 1: the three `opentelemetry` crates, in `waveport-telemetry-otel` only. GenAI attribute names are defined in-repo; the upstream constants are deprecated |
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
| How a carrier media-stream session moves when its node drains or dies: a thin media edge that holds the socket, or a carrier-side reconnect | Phase 3 |
| Shared journal store at scale: Postgres alone, or Postgres plus object storage for snapshots | Phase 3 |
| How much of RTVI the client gateway implements | Phase 2 |
