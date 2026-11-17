# Waveport architecture and technical spec

**Status (16 November 2026):** design spec. The workspace, the gates in section 2 and the test and benchmark harnesses exist. Of the product, the session kernel (section 4) exists: it takes turns, speaks, keeps the conversation, runs tool calls, lets a second agent answer a call or take the floor, closes, and is rebuilt from its journal after a crash. The audio path (`waveport-media`) exists too: Opus and G.711, the change of rate, the inbound path and the outbound path of sections 4.6 and 5, with no allocation per frame. The ports of section 8 exist (`waveport-ports`), with the planner of section 6 that decides how a session is run from what its parts can do, and so does what adapters share (`waveport-adapter-kit`). The runtime exists (`waveport-runtime`): shards, the session actor that drives the kernel and the audio path through the ports, and the compute plane that shares the listening models. So do the adapters of section 8.5, and the `waveport` command that puts them together into an agent to talk to. A session keeps its journal in a file and goes on from it after its process was killed (section 9). What a session does is told to a collector as traces and metrics (`waveport-telemetry-otel`, section 8.5), and the figures of section 12 that phase 1 can measure are measured and held by gates. The six scenarios of the kernel of phase 1 (section 14) all pass, and its eleven invariants (section 13) are checked on random sessions and on every path through small ones in every test run. Section 12 says of every latency and memory figure whether it is measured and held, or still a design target, and which one is not met. Of phase 2, the session is run through the driver of its shape, and the second shape of section 6 exists: a session of text, which `waveport chat` is at a terminal. The kernel waits for a line that dropped, and gives whoever comes back the reply they did not get (section 4.10). It takes the keys of a telephone as turns of their own, and a session presses keys as its line takes them: as a message to the transport, or as tones the audio path makes (section 4.11). What an agent is made of is one definition, put together in one library for the command and for the daemon to come (`waveport-assembly`, section 8.5). What a client and a session say to each other is written down as client protocol v1 (section 8.6). The daemon exists (`waveportd`, section 8.7): it runs the agents of a file, is asked for sessions over HTTP, and serves a session of text as a stream of server-sent events that a client comes back to, also after the daemon was stopped or killed. Nothing carries audio to it yet.

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
| Composition roots | `waveportd`, the `waveport` CLI, language bindings | The only places where adapters are chosen and wired. They do it through one library, `waveport-assembly`, in which an agent is one definition |
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

- The process runs N shards. Each shard is a thread with a single-threaded Tokio `LocalRuntime`, and a session stays on one shard for life. A new session goes to the shard that has the fewest. The thread of a shard can be kept on a processor of its own, and is not unless whoever runs the process names one for each shard (`RuntimeConfig::pinning`; `processors` in the daemon's file, section 8.7). That is asked of Linux, which has a call for it, and is an error anywhere else. The thread is bound before its executor is built. The threads the executor starts for work that blocks are sent to the processors that are no shard's as they start, and every other thread of the process is kept off the shards' processors by the thread that starts them. The density measurement says what pinning is worth (section 12): on a machine whose cores are not alike, a twentieth of what a session takes of its thread when the machine runs at full speed and a fifth when it runs slowly, and most of the long runs either way. On runners whose cores are alike nothing was pinned, and on two of their three processors the long runs were as rare. So nothing is pinned unless the processors are named, and the runtime chooses none ([decision 0003](decisions/0003-shard-pinning.md)).
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
    Key { who: ParticipantId, key: Key }, // one of the sixteen keys of a telephone

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
- When a speech-to-speech provider supports truncation it receives the same cut point, once the response it cancelled has ended: a cut that arrives sooner shortens the audio and leaves the whole text in the model's context. When it does not, the divergence between its server-side context and the kernel's is a shortfall of the session's plan, and the next connection is seeded from the kernel's ([decision 0007](decisions/0007-speech-to-speech-port.md)).

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

A session whose transport ends has lost its line for good. It is told that the line dropped (section 4.10), which cuts off what the agent was saying where it was heard to and starts nothing more, and then to close as abandoned: there is nothing to wait for. A session that cannot hear, because its recogniser failed six times in a row without a transcript in between, is closed as failed. A session that is told its journal belongs to another owner stops where it is: it falls silent at once and asks nothing more of anyone, the tools included. It says no goodbye either: the session is not over, the other owner has it.

### 4.10 A line that drops

The line to a browser or a telephone can be gone for seconds and come back: a train in a tunnel, a network that changes. A transport that notices tells the session, and tells it again when the line is back (`Detached` and `Attached`). The kernel does four things about it.

- **It cuts off what is being said where it was heard**, as an interruption does: what was heard enters the conversation, the rest is discarded, and the epoch advances. A turn somebody was in the middle of ends with the words it has, because nobody is going to say where its speech ends.
- **It says nothing while the line is gone.** No reply is started, and words the application gives it to say are not said. Tool calls go on, and so does a second agent that answers one: what they come to waits in the conversation.
- **It owes the reply again.** When the line is back, the reply that was cut off is asked for once more, and the model is shown what the listener heard of the first. Three things are not asked for again: a reply that was heard to its end, words the application gave it to say, and a reply whose tool calls are still out, because what is owed there is the answer that is made of their results.
- **It waits only so long.** `Timing.detach_wait` after the line dropped, 30 s unless it is set, the session closes as abandoned. Being told again that the line is gone does not begin the wait anew. A wait of nothing closes at once, which is what a transport that has ended gets (section 4.9).

Being told that a line is back which was never gone does nothing, because every restart of a process says it. Whether a line is there is not in the journal: it is a matter of the moment, and a session rebuilt from its journal is told by its transport.

An interruption by the application and a line that dropped leave the same record, an epoch that advanced because the agent was interrupted, so the format of the journal did not change. What goes into a line that is gone is one frame: the fade of what was cut off.

### 4.11 The keys of a telephone

A caller presses keys: an account number, a choice from a menu. A transport that is told of a key tells the session (`TransportEvent::Dtmf`), and the kernel takes it as an event of its own, `Key`, with the sixteen keys of RFC 4733: the digits, the star, the hash, and A to D.

- **Keys that follow one another are one turn.** A run of keys is a turn in the table of turns, as speech is, so what holds while somebody is in the middle of a turn holds while they press: nothing is said to them. A run ends on the key that ends runs (`KeyPolicy.terminator`, the hash unless it is set, and not one of the run's keys), on a number of keys if one is set (`KeyPolicy.count`), when it has as many keys as a run may have (`Limits.keys`, 32), or when no key has followed for `KeyPolicy.wait`, three seconds unless it is set. The key that ends runs, pressed when no run is under way, is a run by itself: a caller who is told "press the hash to go on" is answered.
- **A run enters the conversation as keys.** `Fact::KeysCommitted { turn, who, keys }` is a fact of its own kind, and owes a reply as a committed turn does. The runtime shows it to the model as text of the user, `(pressed on the telephone's keys: 4071)`, so the port of a model knows nothing of keys. Being a kind of its own, digits can later be kept out of a journal or away from a model without a change of format.
- **A key stops the agent at once.** A key is never a noise of agreement, so there is nothing to judge: what is being heard is cut off where it was heard, and whoever pressed has the floor.
- **Words and keys keep their order.** A key ends what whoever pressed it was saying, with the words it has and those still on their way. A person's turns enter the conversation in the order they began, so words that were said before keys are before them, even when their transcript comes after the last key. Speech does not end a run of keys: the tone of a key can be taken for a voice by whatever listens for one, and a run that ended at every tone would be a run of one key. Keys end together with what was said after them only when that later turn has to end, because it went on for as long as a turn may.
- **A key there is no room for changes nothing.** When as many turns are open as a session holds, a key that would begin a run is not accepted: it stops nothing and ends nothing.

Waiting for a first key is not the kernel's. Only a task that asked for keys can wait for one, and tasks come with the agent layer.

Keys the other way are not the kernel's either. An application asks a session to press keys (`SessionInput::Press`), into the menu of a telephone system the agent has called, and the session presses them as its transport says keys leave it (`TransportCaps.keys`):

| How keys leave | What the session does |
|---|---|
| Out of band | Each key is one message to the transport (`TransportControl::Dtmf`), in order. A transport that cannot take it now is asked again a frame later |
| As tones | The audio path makes them (`waveport_media::dtmf`): the two frequencies of ITU-T Q.23 for each key, the lower at −8 dBm0 and the higher at −6, 100 ms of tone and 100 ms of pause. While the agent speaks they are added to what it says, as on a telephone. Otherwise they go out in frames of their own (`Transport::try_send_tone`), at the pace they are heard at |
| Not at all | The keys are not pressed, and the session says how many (`SpeakStats.keys_unpressed`) |

Keys are not kept for later: those asked for while the line is gone, of a session of text, or of a session that is closing are counted as not pressed, and so are those still to press when a line drops. What a session pressed is not part of the conversation, and is not in the journal.

Keys are the first change of the journal's format (section 9). A record or a state says the first version that has everything it holds, so a session in which nobody pressed a key writes what a build from before keys reads, and one kept journal of such a build is read, written again byte for byte and restored in every test run.

### 4.12 Extension points

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
| Ingress → speech-to-text | The newest 2 s of the 8 s a session keeps to look back on, held once | Drop oldest, mark the session degraded, fail over |
| Adapter → kernel | No queue. The session reads a stream when it runs | What is not read stays in the adapter, so pressure reaches the provider socket |
| Kernel → adapter commands | No queue. Calls that never wait | Text an adapter has no room for is kept and given when it has. A request for a transcript that is refused is counted |
| Application → session | 256 requests | The request is refused, and the application is told that the session is behind |
| Session → a follower | 256 things told | The follower is let go, and follows again from the last fact it saw |
| LLM → text-to-speech | Credit: the next piece of text is released once the one before has come back as audio, and while synthesized-minus-played is 3 s or less | Hold text; stop reading the LLM stream above 8 KB pending. A model that writes on to twice that is cancelled, and what it wrote is spoken |
| Text-to-speech → playout buffer | 3 s of audio | The driver stops reading the provider socket |
| Playout → transport | 20 ms frames; 20 ms send-ahead by default, 40 ms at most | Never queue beyond the lead |

- High-rate signals (playback position, audio levels, interim transcripts) are latest-value slots, not queues.
- The 3-second credit caps wasted text-to-speech spend at about 3 seconds per interruption.
- The reorder buffer holds nothing back to smooth the pace, because the listeners are machines. A packet is waited for only when a later one has overtaken it. The wait starts at nothing, grows by 20 ms each time a packet arrives after it was given up, and shrinks by 20 ms after 10 s in which none did. It is the adaptive jitter buffer the WebRTC endpoint of phase 2 needs, with three things added and no second buffer: the frame that comes again inside a later packet (redundant audio, and Opus's own low-rate copy) fills the place of the one that was lost, the sequence numbers a browser spends on padding and on keys are not counted as loss, and where retransmission was agreed the longest wait is what a retransmission takes ([decision 0005](decisions/0005-webrtc-endpoint.md)).
- Filling a gap keeps the stream's length, so the media clock follows the sender's timestamps for as long as the stream is unbroken. It is set by the session clock again each time the stream begins anew. A sender whose clock drifts is not corrected for in phase 1.
- Pacing uses absolute deadlines from one timer tick per shard; the first frame of an utterance goes out immediately. When the audio runs out in the middle of a response, or a tick comes more than 100 ms late, the pace starts again from that moment: what was due in between is not sent in a burst.
- On transports that cannot clear a remote buffer, the send-ahead (20 ms by default) is already on the wire when the server stops or ducks.

## 6. One model for every session shape

A `BrainProfile` states which port fills each of three roles (hear, think, speak) and which duties a provider handles internally (turn detection, context, synthesis). Application-layer drivers translate: `CascadeDriver` for speech-to-text, LLM and text-to-speech, `RealtimeDriver` for speech-to-speech. The kernel issues intent-level commands and never branches on "is this realtime". It has no command for a speech-to-speech model either: the driver of such a session translates the commands there are ([decision 0004](decisions/0004-session-shapes.md)).

| Shape | How it fits |
|---|---|
| Cascaded | Three ports fill the three roles |
| Speech-to-speech | One port fills all three; the kernel keeps a text mirror of the conversation |
| Hybrid front/back | Two agents share one stage. The front agent holds the floor; the back agent is reached through a `delegate` tool whose executor is another agent's generation |
| Multi-party | Every perception event carries a `ParticipantId`; speaker is mandatory on turns and messages. `Floor` tracks N holders, and an `AddresseePolicy` decides whether the agent is addressed. One-to-one is N = 1 with the policy `Always` |
| Text-only | A session with a language model, its tools and nothing that hears or speaks. It needs no transport: what is typed is asked of it, and what it writes is told to whoever follows it. A text input opens and commits a turn in one step, and takes the floor as speech does. Text is released as it is written, and "played" means delivered, which is at once: what is on record of an answer that was stopped is what the reader was given |

Every port says what it can do (section 8.2), and a pure function decides from that how the session is run: `plan(&AgentNeeds, &BrainProfile, Option<&TransportCaps>) -> Result<SessionPlan, PlanError>`. It runs before the session starts. The transport is optional because a session may have none, and a transport may carry no audio; parts that hear and speak are refused on either. For a cascaded session it settles:

| Question | Answer |
|---|---|
| The rate of the audio handed to the recogniser | 16 kHz, or the nearest rate the recogniser takes |
| Whether the recogniser is asked for a final transcript when a turn ends | Yes if it can be asked; otherwise the turn waits until the recogniser notices |
| The rate speech is synthesised at | 24 kHz, or less if the wire carries less, or the nearest rate the synthesiser gives |
| How the synthesiser's marks are read | As exact times when it gives word or character times; otherwise spread over the audio by share of characters |
| Whether what was heard is what the far end says it played | Yes if it says; otherwise it is worked out from the pace at which audio was sent |
| Whether the far end is told to discard audio at an interruption | Yes if it can |
| How keys are pressed that the application asks for | As the transport takes them: by a message to it, or as tones in the audio. Where it takes neither they are not pressed, and counted (section 4.11) |

For a session of text it settles nothing, and names one shortfall: a transport that carries audio, which such a session does not hear.

**The driver of a shape** is the part of a session that differs between shapes, behind one closed set with no dynamic dispatch (`session/drive.rs` in `waveport-runtime`). The session keeps what every shape shares: the kernel, timers, tools, the journal, its inbox, its followers and the agents that are reached as tools. The cascade's driver holds the ear and the voice. The driver of text holds neither: when the kernel hands it text to say, it has said it, and it reports so in the same instant, so the kernel's account of what was heard is exact and nothing of the 3 s that a voice may run ahead holds text back. The kernel is the same kernel, with nothing in it that asks whether a session speaks. The policies a session of text is given are wrapped so that text is released as the model writes it, and its kernel gives back what it is not using between two messages.

A session that cannot be run at all is refused: no rate in common, or an agent with tools and a model without tool calls. A session that can be run, but less well than with better parts, is run, and the plan lists each `Shortfall` so that it can be shown and logged: transcript times no finer than the audio sent, transcripts that cannot be asked for, spoken text cut by share of characters, playback that is estimated, audio that cannot be taken back. Nothing degrades silently. A speech-to-speech profile is refused for now, by name.

## 7. Flows on speech-to-speech models

Each realtime adapter declares what it can do. The list is in [decision 0007](decisions/0007-speech-to-speech-port.md), as two providers that share almost no commands were found to differ, one of them measured and one read:

| What a provider says | Why a session's plan needs it |
|---|---|
| The audio it takes and gives | What is changed in rate on the way, and whether a telephone line's audio can be handed over as it came |
| Whether the end of a turn and the request to respond are apart | A session that may leave a turn unanswered is refused on a provider that always answers |
| Whether a turn's beginning stops the model, and whether input can be dropped | A turn's audio is streamed where it can be dropped, and held until the turn is committed where it cannot |
| Whether a stop cuts its record to what was heard | Where it does not, the kernel's conversation is the truth and the provider's is a shortfall |
| What can be injected, where, and whether that interrupts | How the conversation is kept in step, and how a new connection is seeded |
| Who decides when a tool's result is spoken, and whether it takes calls back | The driver's decision or the provider's |
| Which settings change on a live connection | Anything else is a new connection at a turn boundary |
| How long a connection lives, and whether a handle resumes it | Reconnecting is routine: the next connection is opened and seeded while the old one serves |
| When the words of a turn come | The agent of such a session is set to words that follow the turn |
| Whether it can answer in text alone, or with silence | A response that says nothing ends without an error, with its reason |

A provider that cannot give up turns to the client is refused when the session is planned, by name. The planner of section 6 will decide the strategy of a flow too, at session start and at every handoff. It picks the strongest strategy available and fails loudly instead of degrading silently:

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
    fn caps(&self) -> TransportCaps; // audio, if it carries any: codec each way, remote buffer (clearable, says what it played); text; how keys leave
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>>;
    fn try_send(&mut self, frame: FrameOut<'_>) -> Result<(), Full>;
    fn try_send_tone(&mut self, tone: ToneOut<'_>) -> Result<(), Full>; // audio of no response: the tone of a key
    fn try_control(&mut self, control: TransportControl) -> Result<(), Full>; // Clear | Dtmf
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
/// Send half of a speech-to-speech session. A command says what is wanted,
/// and the adapter owns the provider's messages for it and their order. No
/// command has an answer of its own: what became of it is in the events, which
/// name the response, the item or the tool call they are about (decision 0007).
pub trait RtTx: Send {
    fn try_audio(&mut self, audio: AudioRef<'_>) -> Result<(), Full>;
    fn try_cmd(&mut self, cmd: RtCmd<'_>) -> Result<(), Full>;
    // RtCmd: TurnBegins, TurnEnds { respond }, DropInput, Respond { with },
    //        Stop { response, heard }, Inject { item }, ToolResult { .. }, Configure { to }
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
| WebSocket server side | Takes a connection that was made to this machine: one path, one protocol that the client has to offer if the protocol has a name (a carrier's media stream names none), messages of a megabyte at most, a time limit for the request. It also takes a connection whose request an HTTP server read and answered, as the daemon does on its one port: the kit gives the answer to the client's key, and reads the connection from there under the same limits |
| Audio over messages | What the transports share that carry audio over such a connection: the responses a far end was sent, under the numbers it knows them by and with how much of each was sent, and its audio cut into packets of 20 ms whatever the pieces it arrives in, for samples of 16 bits and for G.711. Opus arrives a packet a message, and is numbered and placed by the length its first bytes give it. A far end that comes back on another connection is told again whose audio follows |
| Kept connections | A connection whose work is done is kept for the work that comes next, since making one takes three trips to a provider and back. One that turns out to be gone before the provider was heard on it is replaced, and the new one is told what the old one was |
| HTTP client | A POST or a GET whose answer is read a piece at a time, or whole when it is short; a status that is not a success becomes an error with its retry class and the provider's `Retry-After`. Who asks is said as a bearer token, as a name and its key (`Basic`), or in a header of the provider's own, and in each shape the key is in a header that is not shown and is struck out of what the server says back. A body is JSON, or a form |
| Server-sent events | A parser that is given bytes as they arrive, with a limit on the length of an event |
| Secrets | A key type that prints as `Secret(…)` and is struck out of any text that may be shown |
| Retry | The class of an HTTP status or a close code, and a backoff with jitter |
| Timeline | Turns a recogniser's times, counted from the audio it was sent, into media time |

Both clients use one TLS configuration: `rustls` with `ring`, and certificates checked by the platform's own verifier.

### 8.4 Driving ports (sketches)

```rust
pub trait SessionControl: Send + Sync + 'static {
    fn send(&self, session: SessionId, input: SessionInput) -> Result<(), ControlError>;
    // SessionInput: Text, Say, Press, Interrupt, Handoff, Close
    fn subscribe(&self, session: SessionId, from: Seq)
        -> Result<Pin<Box<dyn EventRx>>, ControlError>;       // resumable by Seq
    fn subscribe_from_state(&self, session: SessionId)
        -> Result<Pin<Box<dyn EventRx>>, ControlError>;       // for whoever cannot resume
}

pub trait Dispatch {
    fn offer(&self, job: JobOffer) -> Offer;
    fn load(&self) -> LoadReport;
    fn drain(&self, by: MonoTime);
}
```

`SessionControl` is built, and the runtime implements it. `Dispatch` is a sketch.

- Whoever follows a session is told two kinds of thing: the facts of the session, each with its number and in the form the journal holds it in, and the news that is not kept, such as who is speaking. A follower that comes back after a break names the first fact it has not seen. A session holds its last 128 facts for that; a follower that asks for older ones is told at which fact what follows begins, and has the ones before it from the journal.
- A follower that cannot go on from what it holds asks to follow from the session's state. It is told the state first, as the journal would store it and as of the last fact the session has made, and then what happens from there. Nothing can fall between the two: the session makes both in one step.
- A request is taken or refused at once. A session that has 256 requests waiting refuses the next.
- Starting a session is a method of the runtime and not of the port. What a session is made of (its transport, its providers, its tools) is decided where the parts are put together, and a port cannot name that yet. `migrate` comes with the fleet.
- A runtime can be drained: from then on it starts no session, and whoever drains it waits for the ones it has. Each session says where it stands when it has run, and the runtime says whether any has something under way, which is what a daemon that stops waits for (section 8.7).

### 8.5 The adapters there are

| Crate | Port | What it is |
|---|---|---|
| `waveport-models-onnx` | `VadModel`, `TurnModel` | Silero VAD v6.2.3 and Smart Turn v3.2, run by ONNX Runtime. The library and the models are files, loaded when the program starts from the directory that `cargo xtask fetch-assets` fills; nothing is downloaded by a build |
| `waveport-stt-openai` | `Stt` | OpenAI's realtime transcription over one WebSocket, at 24 kHz. The provider is told not to judge where a turn ends, and writes a final transcript when it is asked for one. A session lasts an hour, and its end is a failure worth another try |
| `waveport-llm-openai-compat` | `Llm` | Chat completions read as server-sent events, in two dialects: OpenAI's own, and one for every other server that speaks the protocol, a model on this machine among them |
| `waveport-stt-deepgram` | `Stt` | Deepgram's live transcription over one WebSocket, at the rate of the session's audio, with the time of every word. The provider settles what it has heard whenever the speaker pauses, and also when it is asked to; it does not promise an answer to that, so a request that gets none within two seconds is answered with nothing more. A connection that carries no audio is kept alive |
| `waveport-stt-assemblyai` | `Stt` | AssemblyAI's streaming transcription over one WebSocket. The provider's times for its words are not where the words were spoken, so a transcript is placed in the audio sent since the one before. A stream opens only when the provider has begun a session with the model that was asked for. Audio is put together into messages of the size the provider takes. A turn is given once, when the provider says it is over, which it also does when asked |
| `waveport-tts-cartesia` | `Tts` | One WebSocket, one context per response, audio at 24 kHz with the time each word ends at. A connection is kept from one response to the next |
| `waveport-tts-elevenlabs` | `Tts` | One WebSocket, one context per response, audio at the one rate the adapter was set up with (24 kHz unless told), and with every piece of audio the characters spoken in it and when. A connection is kept from one response to the next |
| `waveport-transport-local-audio` | `Transport` | The microphone and the loudspeaker of this machine, through `cpal`, or a caller who follows a script |
| `waveport-transport-websocket` | `Transport` | A far end that connects over a WebSocket and speaks client protocol v1, with audio as plain samples or as Opus, or with none; a far end whose connection dropped is attached again to the transport it left. And the client for it |
| `waveport-transport-telephony-ws` | `Transport` | A telephone call that a carrier streams over a WebSocket: the caller's audio and keys in, the agent's audio out, what the carrier says the caller was played, and a discard. Twilio is its first carrier. With it come the document a call is answered with, the check that a request is the carrier's, and a stand-in for a carrier, for tests. Written from the carrier's documentation and **not yet held against a carrier** |
| `waveport-store-memory` | `Journal` | Keeps the rules of the port and nothing past the end of its process |
| `waveport-store-journal` | `Journal` | One file on this machine, from which a session goes on after its process died. Section 9 says how it is kept |
| `waveport-telemetry-otel` | `EventSink` | Tells a collector what sessions do, as OpenTelemetry traces and metrics over HTTP. One trace for each turn that is answered: the turn, inside it each response of the model, inside that each tool call. Counts of turns, utterances and tool calls, and how the lengths of the parts of a turn are spread. No word of a call is sent: not what was said, and not what a tool was asked or what it answered. The exporter tells nobody when a request fails, so what could not be sent is counted and can be asked for |

The crate documentation of each provider adapter lists what was learnt from the provider, or read in its documentation. Every adapter has been held against its provider, and its tests replay what the provider said then: the ones for OpenAI on 15 November 2026, and the ones for Cartesia, Deepgram, AssemblyAI and ElevenLabs, which were written from the providers' documentation, on 16 November 2026. That first run changed two of them. ElevenLabs times the characters it speaks on a clock that runs on through the speech, not from the beginning of each piece of audio, so the marks of every piece but the first were too late. AssemblyAI gives a time for every word that is not where the word was spoken, so its transcripts are now placed in the audio sent since the transcript before, as those of OpenAI are, and a session's plan says that their spans are wide. What a provider does when it fails in the middle of a session has not been seen for any of the four, and is as its documentation has it. Each adapter has tests named `live_*` that talk to the provider (section 14).

**What a follower is told.** Whoever follows a session through the control port is told its facts, each with its number, and news that is not kept: who is speaking, what an agent is doing, the words of a response as they go out (to speech synthesis in a session that speaks, which makes them captions that run ahead of the voice; to the reader in a session of text, where they are the answer), and the words of a speaker as far as the recogniser has them. A fact is told when the session makes it, which is before its journal has stored it. If the process dies in between, the journal never had that fact, and the session that goes on from the journal gives its number to another: an answer may have been read that the conversation does not hold, and is then given again. A client that comes back after its session's process died cannot go on from the number of the last fact it saw without being told where the journal stood. The client protocol says it (section 8.6): a session says which of its lives it is in, and a client that was told of an earlier one is sent the session's state in place of what it holds.

**What a session tells.** After each step the session gives its sinks the facts of the step, and a mark for each stage a turn went through: the final transcript, the end of the turn, the model asked, its first word, the first text given to speech synthesis, the first audio back, the first frame handed to the transport, a response lowered, a response stopped, a tool call sent and back, and the journal's word that records are stored. A mark carries the session's own time. What lies between two marks is a part of a turn, and `waveport-adapter-kit` adds the marks up into those parts (`turns`) for whoever wants them: the telemetry adapter, the report that `waveport dev --report` and `waveport run --report` write when a call is over, and the performance gates of section 12. The order is the runtime's and a sink has to live with it: the utterance of a response that was spoken over is recorded before the mark that says it was stopped. Telemetry is switched on by `--otlp <address>` and by nothing in the environment alone; the headers a collector asks for are read from `OTEL_EXPORTER_OTLP_HEADERS` by the exporter, so no key is an argument. **The adapter has been held against a stand-in that reads what it is sent as the protocol's own messages, and not yet against a collector that somebody runs.**

**The local-audio transport** carries plain samples at 48 kHz both ways, in frames of 20 ms. A sound card asks for audio on a thread of its own that must never wait, so the two sides share nothing that locks: one ring of samples each way, and a few numbers that are read and written whole.

- Each response is a run with a number. Every sample in the ring carries the number of its run, so the device side counts what it plays of each, and the session is told how much of a response was played: the far end says what it played (section 4.6), to the sample.
- "Played" is counted where a sample is handed to the sound system. The listener hears it a few milliseconds to a few tens of milliseconds later, by a delay the sound system does not tell.
- At an interruption the session names the run to discard. The device side fades it out over 5 ms and throws the rest away, and audio of that run that is still on its way is dropped when it arrives.
- A run starts playing only when 25 ms more than the device asked for is in hand, or the run has ended. A device that asks for more than there is plays silence for what is missing; nothing is played twice. A moment in which the sound system itself did not keep time is counted as a stumble, and the count is shown when the call ends.
- Only devices that work at 48 kHz are taken. Several channels of a microphone are averaged into one, and the agent's voice goes to every channel of a loudspeaker.
- Nothing takes the agent's voice out of what the microphone hears. With headphones this does not matter. Without them `half_duplex` silences the microphone while the agent speaks and for 300 ms after, which also means the agent cannot be interrupted.
- In place of the sound card there can be a person who follows a script (`Step`: say this recording, stay silent this long, listen until the agent has finished), moved on by the session's own clock in ticks of 10 ms. What that person is played is kept as a recording. A scripted session takes the same path as a spoken one from the ring onwards, and is the same every time it is run.

**The WebSocket transport** carries client protocol v1 (section 8.6) over one connection: a text message is one message of the protocol, and a binary message is audio. A client asks for `/v1/client` and names the subprotocol `waveport.client.v1`. Its `hello` says how the audio is written, the same both ways, or names none, which makes the connection a line of text.

| `audio.codec` | A binary message holds |
|---|---|
| `pcm16` | Plain samples, 16 bits, little-endian, at `audio.rate`. From the client any whole number of them, up to a second, which the transport cuts into packets of 20 ms on the session's clock. To the client, 20 ms |
| `opus` | One Opus packet. It is numbered as it arrives and placed by the length its first bytes give it, on Opus's clock of 48 kHz. `audio.rate` is the rate the client plays at, and what it says it played is counted at that rate |

A client that sends nothing for more than a second is taken to have paused, and its audio goes on at the time it comes back. The audio of a response is a run: a `mark` says which response the audio that follows belongs to, and another that all of it was sent; `played` says how much of a run the client has played, which is what lets an interrupted response be cut where it was heard to, and is never taken for more than was sent; `clear` has the client throw away what it has not played. Keys a client presses (`dtmf`) reach the session one by one. A message that cannot be taken is answered with an `error`, and the connection goes on: the client can say it again.

**A connection that drops is not a caller who left.** One that ends without a `bye` from either side tells the session that the line is gone, and the session waits (section 4.10). The client comes back on a new connection with `hello.resume`: the token its `welcome` gave it, the life of the session it was told of, and the number of the last fact it was sent. Whoever serves it finds the session by the token and attaches the connection to the transport that session already has, which tells the session that the line is back. Run numbers go on, and the next audio says again whose it is, since the new connection holds none of what the old one was sent. A client that is attached while another is still there takes its place, and the one before is told `bye` with `replaced`. A client whose audio is not the session's is refused: a session cannot change how its line is written.

The transport is made for a client before anybody is on it, so that the session can be started first and the client welcomed with what the session was started as. What a connection carries besides audio and what the transport does itself is a link, for whoever serves the client: through it the client is told of the session (`fact`, `told`, `gap`, `state`), in order with the audio, and through it comes what a client asks that no transport can do (`interrupt`). When the session ends, the client is told `bye` once the link is let go of, or after 150 ms, so that how the session ended is told before the goodbye.

A client sends audio all the time, also in the moment it is told that the call is over. So the server does not let go of the connection when it has said goodbye: it reads what the client still sends until the client has closed too, for 300 ms at most. A connection that is let go of with something unread is reset, and a reset can cost the client the goodbye it had not read yet, so that a call that ended would look like one that broke off. The client, for its part, looks at what the server says before it sends, and when it can send no more it still reads what the server had said.

Nothing in the transport asks a caller who they are: it checks that a token in a hello looks like one, and whether it is good is for whoever serves the connection. Its own listener takes an address of the loopback interface, and another only when it is told in so many words that it may be exposed. A server with an HTTP server of its own hands the transport the connection it upgraded.

**The carrier transport** (`waveport-transport-telephony-ws`) is a telephone call as a carrier hands it to a server. The carrier takes the call and asks an address of the application what to do with it. The answer connects the call to a WebSocket, and the carrier sends the caller's audio over it and plays what it is sent. The crate is that end of the WebSocket, and the two things around it that are a carrier's own: the document a call is answered with (`Carrier::answer`) and the check that a request is the carrier's (`Carrier::verify`). Both are functions of what they are given, so a server only mounts them. The carriers are a closed list, each a module that reads and writes its messages, under one loop. Twilio is the first, written from its documentation as read on 17 November 2026. **Nothing of it has been held against a carrier**: there is no account with one, and the tests have a stand-in that does what the documentation says.

- The audio is what the carrier's stream carries, both ways: with Twilio G.711 µ-law at 8 kHz, in text messages that hold it as base64. What the caller says is cut into packets of 20 ms, wherever the carrier cut it.
- A carrier knows nothing of responses. It takes audio and marks: a mark is a name put after some audio, and the carrier says it back when the caller has been played everything before it. The transport puts a mark where a response begins and one after every frame of it, named by the response and by how much of it was sent (`r3.160`, and `r3.480.end` for the last). A mark that comes back is what the caller has heard (section 4.6), to within a frame. The mark where a response begins comes back when the caller is about to hear it, which is later than it was sent if the carrier still holds something else.
- A discard empties everything the carrier holds, and the carrier says back every mark it throws away with it. Those are of audio nobody heard, so from a discard on no mark of the response that was discarded, or of one before it, is believed. A mark that was on its way back when the discard was sent cannot be told from one that was thrown away: what the caller heard may be a few frames more than the session knows, and is never less.
- The keys a caller presses arrive as messages, and are keys (section 4.11). Twilio takes no key from a server, so a key the session presses is a tone in its audio, which the session makes.
- With Twilio the address of a stream carries nothing after a `?`. What a stream has to say to be let in goes into the answer as parameters, which the carrier says back as the call begins (`Started::parameters`).
- A call that drops is over. A carrier does not bring a call back to a stream it left, so the transport says that its caller has left and never that the line dropped. A session that ends closes the stream, and the carrier goes on with whatever its instructions say comes next, which after a stream alone is the end of the call.
- A request of Twilio's is signed with the account's token: HMAC-SHA1 over the address as Twilio wrote it and the fields of the form in the order of their names, compared in a time that does not tell how much of it fits. Nothing in the signature says when it was made, so a request that was signed once is good again: whoever answers calls remembers the calls it answered, and gives each stream something that is good once. How the request for a stream is signed is one sentence of the documentation, so the address is tried in each way it may have been written: with and without its port, with and without a stroke at its end, as a `wss` and as an `https` address. A request that was signed in the newer way, with a key of its own, is refused and said to be that.

The stand-in (`sim`) is part of the crate, for the tests of whatever serves calls. It brings a call to a stream, speaks, presses keys and hangs up. It holds what it is sent and plays as much of it as a test says, and says back each mark it reaches, and every mark it throws away at a discard. It takes only what the documentation shows a carrier taking, and keeps anything else as a complaint, as a carrier warns of a field it does not know.

**Tools** are registered with the runtime's `Registry`, which is the `ToolExecutor` of a session: each tool is a name, a description and a JSON schema for the model, an effect class and a gate for the kernel (section 4.8), and a function. Arguments are checked by reading them into the function's own type, and a tool that is given what it cannot read answers the model with the reason.

**Assembly** (`apps/waveport-assembly`) is where the parts are put together, for whatever runs agents. An agent is one value, an `AgentSpec`: what it is told, the model it thinks with, and what it hears and speaks with, if it does either. It can be read from TOML, and holds no key: a definition names the environment variable that holds one, and a definition with anything else in it is refused without what it held being repeated. A `Stack` is what the agents of one process share: the runtime, the models that listen, which are loaded when the first agent that hears needs them, the tools, the journal and what sends traces and metrics. A session is an agent on a stack, begun over a line or over none, and how it is run is picked by what its line carries: over a transport with audio an agent that hears and speaks does, and over one without, or over none, the same agent reads and writes. The library is the one place that knows which adapter a provider's name stands for, and what is usual for each provider. It prints nothing: what whoever runs the program should know comes back as notes, and a failure about a setting names it as whoever gave the settings calls it. It also holds what following a session for a client comes to, whatever the connection (`shown`): which facts a client is shown and which news it asked for, where a client that comes back is gone on from, and the state that takes the place of facts a session no longer holds. The daemon's event streams and the command's server say the same things because they say them through it.

**The `waveport` command** (`apps/waveport-cli`) defines one agent on its command line and runs it: `waveport dev` runs an agent on the sound devices, `waveport run --script` runs it against a scripted caller, `waveport serve` takes calls over the WebSocket transport, as many at a time as come, `waveport client` makes one from the sound devices or from a script and tells the call from the facts it is sent, line by line as the agent's side tells it, `waveport chat` is the same agent in writing, with its language model and its tools and none of the parts that hear or speak, and `waveport devices` lists the devices. `--stt` and `--tts` say who recognises and who synthesises speech; each provider brings its own address, model, voice and the name of the variable that holds its key. A key is never an argument: the command is given the name of the environment variable that holds it. The command has two tools: `get_time`, which changes nothing, and `end_call`, which has an effect and is gated on the agent's own words having been heard, so that the agent says goodbye before the line closes. With `--notes <FILE>` it has a third, `take_note`, which adds a line to the file: an effect that can be counted afterwards.

The command keeps its calls in a journal in a file, `.waveport/journal.redb` in the folder it is run in unless `--journal` names another; `--no-journal` keeps them in memory. A call that is begun by `dev` or `run` gets a resume token, which is written to a file next to the journal that only its owner can read, and `--resume last` goes on with that call in a later process: the session is rebuilt from the journal and says from what. `--resume <FILE>` goes on with the call a file has the token of. A token is never an argument and is never printed, for the reason a key is not. A call that ends is taken out of the journal, and one that broke off stays until it has been gone on with to its end: the command says when it starts how many there are, `waveport calls` lists them, and there is no command yet that lets go of one. A journal that cannot be read stops the command, which names the file and leaves it as it is. Tools are journaled by their place in the list, so a call has to be gone on with by a command with the same tools.

A call that `serve` takes is gone on with by its caller. The caller is given a resume token as the call begins, and `waveport client` keeps it in memory. When its connection drops it comes back by itself, for half a minute: to a call that is under way it is attached, and to one that broke off with the agent's process it is given the call back by `waveport serve --resume`, which rebuilds the session from the journal on the caller's new connection. The caller is then told of another life of the call, and so is sent the call as it stands before what follows. Without `--resume` a caller of a call that is not under way is refused. A caller of an agent that takes calls on this machine alone is shown what the tools were given and what they answered; with `--exposed`, where a caller can be anybody, it is told only that they were called.

### 8.6 The client protocol

`waveport_protocol::client::v1` is what a client and a session say to each other: a browser, a telephone bridge or a program on one side, and the server that runs the session on the other. The messages are the same on every connection that can carry them: a WebSocket, the data channel of a WebRTC call and, for what the server says, a stream of server-sent events. Everything that is not audio is one JSON object whose `type` says what it is. On a WebSocket, audio is a binary message that holds audio and nothing else, as plain samples or as one Opus packet. On a WebRTC call the audio is the call's.

| From | Messages |
|---|---|
| the client | `hello {v, token?, audio? {codec, rate}, features, resume? {token, life, seen}}`, `played {run, samples}`, `played_rtp {ts?, emitted?, concealed?, delay_ms}`, `text`, `dtmf`, `interrupt`, `bye` |
| the server | `welcome {v, session, life, resumed, resume_token?, audio?, features, next_seq, grace_ms?}`, `mark {run, generation, end?}`, `clear {run}`, `fact {seq, kind, …}`, `told {kind, …}`, `gap {resumes}`, `state {upto, lifecycle, agent, participants, items, speech?}`, `error {code, message}`, `bye {reason}` |

- **The client speaks first.** A hello that names no audio asks for a session of text. The audio of a connection is the same both ways.
- **Stopping, dropping and leaving are three things.** `interrupt` stops the agent and the session goes on. A connection that drops leaves the session waiting (section 4.10), for as long as the welcome's `grace_ms` says; a session that is on no line, and is only followed and written to, names no time and does not end for it. `bye` ends the session.
- **Audio from the server is in runs.** A `mark` says that the audio which follows is that of one response, and another that the response has been sent whole. `played` says how much of a run the client has played, which is how the session knows what its listener heard, and `clear` tells the client to discard what it holds and has not played. A browser on a WebRTC call cannot count samples of a run: it says where in the stream the audio is that it played last, or how much has come out (`played_rtp`, as [decision 0005](decisions/0005-webrtc-endpoint.md) found each browser can).
- **A client is told two kinds of thing**, as a follower is (section 8.4). A `fact` is something that became true of the session, with its number. `told` is news that is not kept, and is not told again to a client that missed it.
- **A fact here is a client's view of a record of the journal**, and what a client sees of each kind of record is decided in one function with a case for every kind, so a new one does not compile until it is decided. A turn, keys and what was heard of a response are shown as they are. An epoch is the server's, and a client is shown that something was cut off, and why. What is said to the language model and to nobody else is not shown, nor the server's own bookkeeping of tool calls. So the numbers a client sees rise and need not follow one another.
- **Tool calls are told to a client that asks**, by the tool's name. What a tool was given and what it answered do not leave the server unless the server is told that they may: an argument may be a card number.
- **Coming back.** A client that comes back says the number of the last fact it was sent, and is sent the facts after it. Two things can be in the way. The server holds those facts no longer. Or the session was rebuilt after its process died, from what was stored of it, and never did the last things the client was told (section 8.4). Numbers do not show the second: the rebuilt session gives the same numbers to other facts, and may be past them by the time the client is back. So a session says which of its lives it is in (`life`, which goes up each time it is rebuilt), and a client that comes back says which one it was told of. In either case the client is sent `gap`, then the session's `state`, which takes the place of what it holds, then the facts after it. `View` is what the facts add up to, as a client holds it, and is the same whether a client was sent every fact or a state and the facts after it.
- **Change.** A reader skips a field it does not know, and takes a message, a fact, a piece of news or a value of a kind it does not know as `unknown`. So a field or a kind can be added without a new version. A client names the version it speaks in its hello.

`waveport_protocol::control::v1` holds the bodies of the control API: the request for a session of an agent and its answer, which carries a client token; what a server says of its sessions; what is asked of a session that runs, which is what `SessionInput` is (section 8.4); and the body of an answer that is not a success, with the members of RFC 9457 and a code for a program. The codes are the client protocol's.

Two tokens are named here, and neither is a key to anything but one session. A client token (`wpc1_`) lets its holder connect to a session, once. A resume token (`wpr1_`, section 9) lets its holder go on with one.

The fixtures of the protocol (`crates/core/waveport-protocol/tests/fixtures/`) are the protocol as it is promised: every message as it is written, what a reader has to tolerate, what is refused, and what a client holds after what it was sent. The clients in other languages are held to the same files.

**The daemon speaks the server's half** over server-sent events, and takes what a client says there as requests (section 8.7). The WebSocket transport carries all of it but `played_rtp`, with the audio as binary messages (section 8.5).

### 8.7 The daemon

`waveportd` runs the agents a file defines and is asked over HTTP. It is a composition root (section 2): it reads a configuration, has `waveport-assembly` make each agent once, and starts a session of one when somebody comes for it. What it adds is who may ask, what a session is called from outside, and what becomes of the sessions when it stops. A session of the daemon is on a line, with its far end on a WebSocket, or on none, and is then written to and followed.

| Request | Who may ask | Answer |
|---|---|---|
| `POST /v1/sessions` `{agent}` | a control key | 201: the session's name, a client token, and how long the token can be connected with |
| `GET /v1/sessions`, `?state=broken` | a control key | the sessions, or the ones that stand so |
| `GET /v1/sessions/{id}` | a control key | the session: its agent, where it stands, how it is run, its life, the number of its next fact |
| `DELETE /v1/sessions/{id}` | a control key | 204 when it is gone, 202 when it runs and was asked to end |
| `POST /v1/sessions/{id}/tokens` | a control key | 201: another client token |
| `POST /v1/sessions/{id}/input` | a control key, or the session's resume token | 202: taken |
| `GET /v1/sessions/{id}/events` | a control key, a client token, or the session's resume token | a stream of server-sent events |
| `GET /v1/client` | a client token or a resume token, in the hello | 101: a WebSocket that speaks client protocol v1 |
| `GET /playground/`, and what is beside it | anyone, where the daemon serves it | the page to try the agents on, and the client for JavaScript it is made with |
| `GET /healthz` | anyone | 200, or 503 while the daemon stops |

The bodies are those of `control::v1` (section 8.6), and what is refused is answered with a problem (RFC 9457) whose `code` is one of the client protocol's.

- **A session starts when somebody comes for it.** Making a session names an agent and returns a token. Nothing runs until the first stream of events, the first input or the first client on a WebSocket: how a session is run is picked by the line its far end comes on, and that is known then. A session nobody comes for is let go of when its client token is no longer good.
- **A session is on a line, or on none, for all of its lives.** One that a client on a WebSocket starts is on that line (section 8.5): it hears and speaks if the client's hello names audio and the agent does either, and reads and writes over the line otherwise. One that a stream of events or an input starts is on no line. Either can be followed as events and asked by the application. An agent that greets does so on a line when its far end joins, and the session sees to that by itself, before it takes anything the far end says: a client that writes the moment it is welcomed is greeted and then answered. Asked for from outside, the greeting could come after the question, and stop the answer to it. A session on no line has nobody join it, and is asked to greet by whoever starts it. A client that comes on a WebSocket to a session on no line is told that it is on none, and a hello that names audio for an agent that only writes is told that, before anything is started. Which of the two a session is, is kept with it, beside the name of its agent. So a daemon that goes on with a session after another one knows that the session waits for its far end to come back on a line, and does not start it for a stream of events that asks first.
- **A client's line.** The request for it says nothing of who asks: a browser cannot send a header with it, and an address is written down by what it passes through, so a request with anything after a `?` is refused. Who asks is in the hello: a client token, which names its session by itself and is used up, or the resume token of a client that comes back, with the life it was told of and the number of the last fact it was sent. The daemon takes the connection from its HTTP server once it has answered, hears the hello, and puts the client on the line of its session, which it starts or goes on with if it does not run. What the session does is told to the client by the same follower that writes the streams of events (section 8.5), through the transport, so that a fact and the audio around it leave in order. A client's `interrupt` stops the agent. A client that comes back while its session runs is on the line again. One that comes back after the daemon was killed is on the line of the session's next life, and is sent `gap` and `state`. When the daemon stops it says `bye` with `going_away` on each line and closes it, and holds the line until that is done, for half a second at most. The session is kept, and its client comes back to the daemon that is started next.
- **The stream of events is the server's half of the client protocol.** Each event is one message a server says: the event is named as the message's `type`, and its data is the message. The first is the `welcome`, with a `retry:` for a client that reconnects by itself. The last is a `bye`: `ended` when the session is over, `going_away` when the daemon stops. A stream that ends without one was cut. Nothing is buffered on the way (`X-Accel-Buffering: no`), and a comment is sent every 15 s on a stream that has nothing to say.
- **What a far end says goes to the input route**, as the messages a client says: what it typed, that the agent is to stop, that it leaves. An application asks more there, in the kinds of the control API: that the agent say something, that another agent take the floor, that the session end. A far end that sends one of those is told that it is no message it could send.
- **News is told to whoever asks for it.** A stream names the features it wants in its address (`?features=said,hearing,tools`), which is what a hello does on a connection. Tool calls are told by name to whoever asks. What a tool was given and what it answered is shown to nobody but the holder of a control key, and only where the configuration says so.
- **Coming back.** A fact and a state carry an `id` that is the life of the session and the number of the fact, as `2.17`. A client that comes back says the last one it was sent (`Last-Event-ID`, which a browser sends by itself, or `?after=`), and is sent the facts after it. If the session holds them no longer, if the client was told of another life, or if it names a fact this life has not made, it is sent `gap` and the session's `state` instead, and the facts from there (sections 8.4 and 8.6).

**Who may ask.** Three things let a request in, each said as `Authorization: Bearer`.

| | Held by | Lets its holder | Good for |
|---|---|---|---|
| A control key | the application, on its server | ask everything | as long as the configuration names the environment variable it is in. Two can be named, so that one is changed while the other is good. The daemon keeps their hashes and compares in constant time |
| A client token (`wpc1_`) | the far end, from the application | connect to one session, once | a minute. Made by the daemon, which keeps its hash in memory |
| A resume token (`wpr1_`) | the far end, from its welcome | follow that session and write to it | the session's life, across daemons: its hash is kept with the session (section 9). One that is issued takes the place of the one before |

A token is never part of an address: an address is written into logs, and a header is not. So a page in a browser reads the stream with `fetch`, and not with `EventSource`, which cannot send a header. On a WebSocket, where a page can send no header either, the token is in the first message.

A daemon that listens on this machine alone may be given no control key, and then lets in whoever is on the machine. It still looks at two things, as every daemon does. Where a request says it is going (`Host`) has to be a name the daemon is reached by: the address it listens on, `localhost` on this machine, or a name its configuration gives. A name that somebody pointed at this machine is none, which is what keeps a page from reaching a daemon on its reader's own machine through a name of the page's choosing. And where a request comes from (`Origin`, and `Sec-Fetch-Site` where a browser sends no origin) has to be the daemon itself or a page the configuration names; a page that is named is answered with what its browser needs to let it read the answer. A daemon that is to listen anywhere else is not started without a key. A body has a size it may not pass, the head of a request a time it has to arrive in, and a request that is no stream a time it has to be answered in.

**Stopping.** A daemon that is told to stop (`SIGTERM`, or Ctrl-C) takes nothing new: making a session and asking something of one are answered 503, and so is `/healthz`, which is how whatever stands in front of it learns to send it nothing more. It waits until no session has anything under way, for 10 s at most: a session says where it stands each time it has run (the number of its next fact, and whether an answer, a tool call or a record that is not stored yet is under way), and the runtime adds that up. Then every stream is told `bye` with `going_away` and ends, the connections close, and the sessions stop where they are. Told twice, the daemon stops where it is.

What its sessions had come to is in the journal. With a journal in a file the next daemon lists them as `broken`, each of the agent whose name was kept with it (section 9), and goes on with one when its far end comes back with its resume token: the session is rebuilt, in its next life, and the client is told what it has come to. A daemon that was killed is followed in the same way, and a session that was answering when it died answers again. With a journal in memory, which is what a daemon has that is told of no file, they are gone.

**Where its threads run.** Sessions run on the shards, whose number is a setting (`shards`). A daemon can be told a processor for each of them (`processors`, on Linux; section 4.2). The thread of a shard is then kept on its processor, and every other thread of the daemon off all of them. A thread begins where the thread that started it is. So the first thread of the daemon is kept off the shards' processors before it starts any other, and the threads that serve connections, the journal's, those of the models that listen and those of telemetry begin there. The processors are checked against the ones the process may run on, which is how what a container was given is respected, and one at least has to be left for the other threads. A daemon that cannot place its shards as it was told does not start, and says why. Nothing is placed unless the file says where.

**The page.** A daemon serves a page to try its agents on, at `/playground/`: it makes a session of an agent and talks or writes to it, with the client for JavaScript (section 8.8) served beside it. Both are part of the program. Their files are read when it is built, so nothing is read from a disk or fetched from anywhere when it runs, and a daemon that is one file stays one.

The page asks as anyone asks, over the control API and a client's line, so it can do what whoever opens it could do without it, and no more. Where the daemon has a control key the page asks its user for it, says it to the daemon it came from, and keeps it nowhere. It is behind the same look at `Host` and `Origin` as every other request. A browser is told that a page from the daemon runs and shows what is from the daemon, talks to the daemon, and does nothing else (`Content-Security-Policy`, with `default-src 'none'` and `'self'` for scripts, styles and connections), and that it is shown inside no other page. A daemon that listens on this machine alone serves the page. One that listens anywhere else does not, unless its file says so (`playground`): a page that makes sessions is for whoever runs the daemon, and not for its callers. A browser gives a microphone only to a page that reached it safely, which one from this machine is; a daemon that is reached by a name stands behind something that ends TLS, as it does anyway.

**Not there yet:** a WebRTC call and a carrier's stream, the gRPC API, and quotas. The daemon serves HTTP/1.1 without TLS, and is to stand behind something that ends TLS when it listens beyond one machine.

### 8.8 The JavaScript client

`sdks/js` is the client of a session for a page in a browser: the client's half of the protocol (section 8.6) on a WebSocket, and a voice. It is ES modules with their types in comments, which a browser runs as they are written. Nothing is bundled and it depends on nothing. That is what lets a daemon serve it as part of itself, and it keeps a second supply chain out of what runs: what is installed is the TypeScript compiler, which reads the types, and nothing a page is given.

| Module | What it is |
|---|---|
| `protocol.js` | The messages, read with the tolerance the protocol asks for and written with its refusals, and the view a client keeps: the reducer of section 8.6 a second time, held to the same fixtures as the one in Rust |
| `client.js` | The connection: the hello, what is told to whoever listens, the view, and coming back |
| `worklet.js` | What runs on the audio thread: playing in runs, counting what was played, cutting what the microphone hears into frames |
| `audio.js` | The voice: how audio is written, the microphone, and the two nodes of the audio thread |
| `index.js` | `call`, which is the client and the voice in the order a call needs |

- **What was played is counted where it is played.** The audio of a response is given to the output on the audio thread, a block at a time. A sample counts as played once the output has had the time to sound it, which is the longest of what the browser says that time is (`getOutputTimestamp`, `outputLatency`, `baseLatency`): what was not heard is not to be said to have been. The count of a run is said a few times a second and once more when the run has been played to its end, and the client sends each as `played`. On `clear` nothing more of the run is played, and what had been given to the output by then is said at once, because that much will sound whatever happens. This is the number an utterance is cut at when its speaker is interrupted (section 4.6). From a browser it is counted, and not worked out from a clock. A session goes by its clock for as long as a far end says nothing of a response, so a page that played nothing and said nothing would leave a record that reads as if it had: the client's tests look at the counts themselves.
- **The voice is there before the line.** An agent that greets speaks the moment its far end joins (section 8.7). A page that connected first and asked for the microphone afterwards would be sent the greeting while its user was still being asked, would play none of it, and would say nothing of it. So a call opens the voice first: the microphone, the output, the audio thread. Then it asks for its token, which is good for a minute, where a person can take longer than that to allow a microphone. Then it connects, and puts the voice on the line in the same turn of the browser in which it is welcomed.
- **Opus where the browser codes it.** A browser that codes and decodes Opus itself (WebCodecs) sends and takes packets of 20 ms at 48 kHz, the rate a browser's decoder gives whatever it is asked. Any other sends and takes plain samples, which is twenty-four times the bytes and otherwise the same call. The coder is told that it codes a voice, and to send every packet, silence too: the session hears at the pace it is sent.
- **Echo.** The microphone is asked for with the echo of what is played taken out of it, and how much is taken out is left to the browser. By its documentation Chrome on a desktop takes out everything the page plays, what an audio thread plays too. Asking for exactly that by name (`echoCancellation: "all"`) is new, is known to one browser, is not there on Linux and adds delay, so it is not asked by default; a page can ask it, and can read what it was given. How well a browser's echo cancellation serves a session has not been measured here: the test in a browser turns it off, so that the recording it hears arrives as it is.
- **Coming back is the client's work.** It tries at once, then after a quarter of a second, a half, one and two, each a fifth more or less so that the clients of a server that went do not return together, for as long as its welcome said the session waits. It says the number of the last fact it was sent, and is brought to where the session is (section 8.7). A server that goes away or is busy is tried again, and anything else that is refused is final. What the page held of a response when the line dropped is discarded: the session cut it where it was heard.
- **Nothing of who connects is in an address.** The client refuses an address with anything after a `?`, or a name and a password in it, before it connects.

**Not there yet:** a call over WebRTC, which comes with that transport; a package in a registry; and Firefox and Safari, which have been read about and not tried.

## 9. Durability

Waveport stores a snapshot plus a journal of **domain events**, not inputs. Recovery is `snapshot ⊕ apply(tail)`. Replaying inputs through `decide` is rejected for recovery, because during a rolling deploy the target node runs newer logic. Inputs are still sampled into a flight recorder for replay tests.

| | |
|---|---|
| Journaled | Turn commits, context items, utterance commits, tool-call transitions, agent and flow transitions, participant and configuration changes, and a playout checkpoint each second while speaking. Expect 5 to 30 records per turn |
| Not journaled | Audio, token deltas, voice-activity edges, interim transcripts |
| Hot-path cost | The fact is written as a record on the session's thread and handed to the journal; a thread of the journal's own stores what it is handed, a group at a time, at most every 10 ms |
| One synchronous barrier | Before dispatching a call to a tool that is not `Pure`, the kernel emits `AwaitDurable(seq)` for the record that authorises the dispatch, and dispatches on `Durable` |
| Format | Facts and snapshots are versioned serde records defined in `waveport-protocol`. The domain crate stays free of serialisation: the runtime maps each fact to its record with an exhaustive match, so a new fact does not compile until it is mapped. Zero-copy archives (`rkyv`) were rejected for snapshots, because an archived layout cannot evolve and a newer build must read the state an older one wrote. A record and a state say the first version of the format that has everything they hold, and a reader refuses a version above its own, so a build that is rolled back reads what it could have written. The format is at its second version: keys were the first new kind of fact (section 4.11) |
| Fencing | Appends are conditional on `(lease epoch, expected seq)`, so a superseded owner can neither write nor speak |
| A journal that does not answer | The append is tried again, after 50 ms and then after twice as long each time, up to 2 s. Nothing is lost meanwhile: the facts wait in order, and a tool call that has an effect waits with them. An append that is refused, because another owner has the session or the numbering does not fit, ends the session where it is |
| Snapshots | A session has its journal keep its state: what the kernel's snapshot holds, as a versioned record, and where the last audio ends that a turn was spoken in. A state is due after 256 records or a minute, and is taken when a turn or an utterance next enters the conversation, which is when a session holds least that is only under way; a session in which neither happens takes one once twice as many records wait. It is handed over when every fact it includes is stored, and then replaces the records up to that one. A state the journal fails to store is let go of, because the records are still there. A state that cannot be read, or is as of another record than the journal says, stops the session from starting: nothing is guessed |

Three recovery levels:

| Level | What happens | Target |
|---|---|---|
| Client reconnect | The kernel waits in a detached state, the transport rebinds, and the client resyncs from its last sequence number | No loss |
| Process crash | A new node loads snapshot and tail and reopens provider streams; the client reconnects with a resume token | Within 2 s of lease expiry |
| Live migration | The target pre-opens provider connections; the source waits for a safe point (floor idle, no live generation; forced at a deadline), freezes, flushes and hands over the lease | Drain time becomes "time to the next turn boundary" |

Of the three, phase 1 has the second on one machine: a process that died is followed by another that is started with the call's resume token, and the caller is whoever sits at that machine. Of the first, the kernel's part is there: it waits for a line that dropped (section 4.10). Of the first and the second, the daemon has what a session of text needs (section 8.7): a client that comes back to a stream of events is sent what it missed, and a daemon that is started after one that stopped or was killed goes on with a session when its far end comes back. The transports that carry audio and bring a client back are still to come, and a lease that expires and migration with the fleet.

**The journal in a file** (`waveport-store-journal`) is one [redb](https://www.redb.org) database, which one process has open at a time. Its safety is redb's: a commit that was cut short anywhere, by a crash or by the power, leaves the file as it was after the commit before.

- A row for each session holds who owns it (the lease), the number its next record gets, the record its stored state is as of, and the hash of its resume token. The records are stored in the groups they were appended in, and the newest state of a session stands in place of the records it covers. A fourth table says which version of the layout the file is in. A file of a later version is not opened. A fifth holds a label for a session whose keeper gave it one: the daemon keeps the name of a session's agent there, which is how the daemon that is started next knows whose each session is. The journal does not read a label, and lets go of it with the session.
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
| A speech-to-speech provider session | Server-side audio context is not portable. Use the provider's resumption handle if it has one; otherwise seed a new session from the kernel's text mirror (prosody is lost); otherwise from a summary. A connection that ends is routine and not a failure: one provider ends it after about ten minutes, another after sixty. The next one is opened and seeded while the old one still serves, and is taken at a turn boundary ([decision 0007](decisions/0007-speech-to-speech-port.md)) |

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
| Memory, idle connected voice session (8 s to look back on and 3 s of speech, reserved) | 480 KB | 600 KB |
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
| Memory of a text-only session | Chats on the session actor under the same allocator, between two messages: one of ten messages against the figure for the usual session, and one of a hundred against the figure for 99 in a hundred | Every test run |
| The first three of those latencies in real time; session start; the share of a thread that a session takes | Fifty calls on one shard of the runtime, in real time (`benches/tests/load.rs`). `cargo xtask perf check` holds the share to a record of the same machine in `perf/baselines`, and the latencies and the start to their targets | By hand, on a machine that is left alone. Every night the share is held on the runner, as a ratio to reference work timed in the same run |
| Voice-activity result after the frame; memory of a parked voice session | Nothing yet. The model that listens is a stand-in in every gate, and no session that hears is parked yet | |

Each of the last three kinds of gate was seen to fail on a change made to break it: an answer started 60 ms late (one started 20 ms late is not seen and is not late, because a frame is handed over 20 ms before it is due), 9 s to look back on in place of 8, and every frame encoded twice.

**Measured: latency.** With providers that answer at once, a session adds no wait of its own. On a clock the test controls, the end of a turn, the request to the model, its first word, the first text given to speech synthesis, the first audio and the first frame handed to the transport fall at the same instant, on lines that carry Opus and plain samples at 48 and at 8 kHz. A caller who speaks over the agent has it lowered 60 ms after beginning to, which is the time speech has to last before it counts as speech (two frames of the voice-activity model), and once the agent is stopped no further frame of it is handed over. With providers that take their time, what a caller waits is the sum of what they take and nothing besides. In real time, with fifty sessions on one shard, the end of a turn to the first frame is 0.01 ms for the usual turn and 0.02 ms for 99 in a hundred, and a session starts in 0.07 ms (0.10 ms). These are the session's share with stand-ins for the providers in the same process: what an adapter adds with a real connection is not in them.

**Measured: memory.** A session that listens holds 466 KB of the Rust heap on a line that carries Opus and 524 KB on one that carries plain samples at 48 kHz, and about 11 KB more while it speaks. What libopus keeps for itself is not counted. An active session is far inside its target. An idle one holds nearly as much as an active one, because the buffers of a voice session are reserved when it is set up, which is also what keeps the media path from allocating: 8 s to look back on, 250 KB at 16 kHz, which the end-of-turn model reads and a recogniser that is opened again is caught up from, and 3 s of speech to play, 141 KB at 24 kHz. **The target for the usual idle session was 300 KB and is restated at 480 KB, which is what those buffers cost** ([decision 0002](decisions/0002-idle-session-memory.md)). It was written as though a session that listens held neither, and a shorter look-back would not have met it: with 4 s a session still held over 400 KB, and the end-of-turn model, which reads 8 s, would have judged every longer turn from its end alone. The line that carries Opus is held to the restated target, and the widest line to the 600 KB asked of 99 in a hundred. What waits for speech recognition, 2 s at most, was a second copy of the end of the 8 s. It is read out of them now, and that took 62 KB off every session: one that listens held 528 and 587 KB before. **Left for when sessions can be parked:** the 8 s are borrowed for a turn and the 3 s of speech for an answer, from a pool of the shard's, with about half a second kept to look back on between turns. An idle session then holds about 90 KB on a line that carries Opus and 150 KB at 48 kHz, and the model still gets its 8 s. Nothing is left of a session that is over: a thousand calls of a minute each, run every night, give back every byte. A session of text holds 10 KB with nothing typed, 22 KB after ten messages and their answers of thirty words, and 72 KB after a hundred, by when it holds the 128 records it keeps for whoever follows it. Its kernel is parked between two messages. Both are inside the figure for a text-only session, which no longer waits for a session to measure.

**Measured: the shard.** Fifty calls at once on one shard, on lines that carry Opus, with an agent that speaks a little under half the time. On one performance core of a laptop (Core Ultra 7 155H), on mains and with its power profile for performance, a session takes 0.19% of the shard's thread, 536 sessions to a thread; on GitHub's runners, 0.20 to 0.27% in twelve runs, by the processor a runner has. The spike, with its own model of a session, had put the shard's part at 0.45%. This is the shard's thread and nothing else: the models that listen are stand-ins in this measurement, so the 0.71% the spike measured for them is not in it, and the planning figure below stands, since inference is what limits the number of cores.

**The same machine is not always the same machine.** Three things about the laptop change what it measures, and each was found by a measurement that would not repeat. *Its power.* On its battery the reference work takes it 4.48 ms, and 2.49 ms on mains, and a session takes over twice as much of its thread: 0.48%. *How the speed of its processor is governed.* On mains, with the power profile that balances speed against energy, the reference work takes as long as with the profile for performance, but a session takes 0.24% of its thread where it takes 0.19%: a session works in bursts of a fraction of a millisecond, and is done before the processor is at speed. *Its heat.* Straight after a build, the reference work took a quarter longer at one end of a run than at the other. So a record says how long the reference work took, how the machine was powered and how the speed of its processor was governed, and is refused if the reference work moved by more than 3% while it was made: the first record of this laptop was made with the balanced profile, while it moved by 7.6%. `perf check` refuses a machine whose processor is governed otherwise than for its record, before it measures, and one that takes a tenth longer or shorter over the reference work, after. Every night the share is held on the runner as a ratio to the reference work of the same run. The ratio takes the speed of the processor out of the number: it is 0.75 on the laptop's core with the profile for performance, and was between 0.68 and 0.77 on the runners, in twelve runs on three processors (Xeon 6975P-C, EPYC 9V74 and EPYC 7763), while a session took 0.20% of its thread on the fastest of those and 0.27% on the slowest. It does not take out how the speed of a processor is governed: with the balanced profile the laptop's ratio is 0.92 to 0.94. So the number that is held is the runner's own, with a quarter to spare. It is measured three times every night, on three runners, each of which says what processor it has, and what is held is set again from ten nights of that (`CONTRIBUTING.md` says how).

**The runtime pins the threads of its shards only where it is told their processors.** What pinning is worth depends on the state of the machine too. At full speed it is a twentieth of what a session takes of its thread (0.24% where the system moves the thread among cores that are not alike), and the runs of a session that go over the budget of 250 µs fall from 2 in a thousand to 3 in ten thousand. When the machine was slow it was a fifth (0.57 to 0.63%, against 0.48%), and from 3 in a hundred to 6 in a thousand. That was measured with the laptop's balanced power profile, by keeping the whole measuring process on one processor with `taskset`, on a laptop whose cores are not alike: the runtime pinned no thread. With the profile for performance the one core gives 0.19% and 3 in ten thousand, and what a thread left to the system gives has not been measured that way. On GitHub's runners, whose cores are alike, nothing was kept anywhere, and in twelve runs a session took 0.20 to 0.27% of its thread, by the processor, and 3 runs in ten thousand went over the budget on two of the three processors, as on the laptop's one core, and 6 on the slowest. So the gain that was measured is the laptop's. Pinning is there since the daemon of phase 2, where a process first has more than one shard: on Linux, and off unless the processors are named ([decision 0003](decisions/0003-shard-pinning.md)). Nothing was measured with it: what it is worth on a server with every core at work is still to be measured (section 18). How long a session runs at a time is measured and recorded and not held by a gate: the longest run moved between 0.7 and 1.9 ms on unchanged code, and it is the longest of a whole measurement, the start of its sessions included.

**Density.** The phase 1 spike measured one active cascaded session at 1.17% of a core ([decision 0001](decisions/0001-density-spike.md)): 0.45% on the shard thread, most of it the Opus encoder, 0.14% for voice-activity inference and 0.57% for end-of-turn inference at one inference every ten seconds. That is 85 sessions per core with nothing to spare. The planning figure is 60 active voice sessions per core, which keeps a core at 75% and allows a tenth for the work the spike did not model, and about 900 per 16-core node. One shard thread holds its timing up to about 150 sessions; inference is what limits the core count. The earlier estimate of 0.7 to 1% and target of 100 per core left out end-of-turn inference and assumed batching would make inference 2 to 4 times cheaper; the measured gain is 1.9 times. The measurement has limits, listed in the decision record: one laptop core at turbo frequency, with the rest of the machine nearly idle. Not measured: local neural noise suppression, which the earlier figures put at about 1.5% of a core per session, and text-only sessions, estimated at more than 5,000 per core. For comparison, LiveKit's own guidance is 10 to 25 jobs per 4 cores.

**Journal.** What a record costs the session that makes it is counted in instructions (`benches/ir/journal.rs`): 3,193 to write the turn of a caller who said a sentence as a record, and 338 to hand the four records of a step to the journal, whatever the journal is doing. That is the whole of the hot-path cost in the table, as a count and not a time. How long the records then take to be stored is in section 9.

**Media path.** The work of one 20 ms frame is counted in instructions (`benches/ir/media.rs`), because a count is the same from run to run and a time is not. A packet in costs about 234,000 instructions as Opus, 49,000 as G.711 and 107,000 as plain samples at 48 kHz. A frame out costs about 565,000 as Opus, 56,000 as G.711 and 143,000 at 48 kHz. The Opus figures are libopus at 32 kbit/s and its highest complexity; the other two are the change of rate. The path itself (reordering, the window, the queue, pacing, the ledger) is under 1,500 instructions either way. A pull request that makes any of them grow by more than 2% fails. These are counts on x86-64 and not times. The buffers of a voice session are reserved when it is set up, with the settings of section 5 (8 s to look back on, the newest 2 s of which wait for speech recognition, and 3 s of speech); what they come to, and what that means for the idle session's target, is above.

## 13. Invariants

These eleven are stated formally and checked in simulation.

1. The epoch is monotonic. No playout start is emitted for a generation that is not live, and no tool dispatch for a response that was cut off.
2. Every accepted perception event ends in exactly one outcome: a committed user turn, or a turn dismissed for a stated reason (a backchannel, noise, no words, or the session closing while the turn was open). A key is such an event: every key that is accepted is a key of exactly one run, a run enters the conversation as the keys that were pressed, when it is over and not while another could follow, and a key stops what is being heard in the step that takes it. A person's turns, of words and of keys, enter the conversation in the order they began.
3. Each utterance commits exactly once; committed text is a prefix of planned text and never extends past the audio handed to the transport.
4. A call ends exactly once. A dispatched call that may have changed something ends `Completed`, `Failed` or `InDoubt`, or `Cancelled` if its tool confirms that it undid the call. A call that is not pure is dispatched only once the record that authorises it is durable. A call is dispatched at most once per key within one lease, and after a crash it is dispatched again only if its tool is declared pure or idempotent. Its result enters the context exactly once.
5. Each tool batch starts at most one continuation in one life of the session. A continuation that a crash interrupted, or that a line which dropped cut off, is started again.
6. At most one generation holds playout per output track.
7. From `Close`, `Closed` is reached within drain plus grace under any adapter behaviour.
8. All queues are bounded; `step` is total and deterministic.
9. `load(snapshot) ⊕ apply(tail)` equals live state: a session rebuilt from any snapshot and the facts journaled since holds what the live session's snapshot holds.
10. The facts a kernel emits are numbered without gaps, and once told that its lease is lost it emits nothing more. That at most one lease appends per session is the journal's guarantee (section 9) and is checked with the journal, since a lease is not kernel state.
11. While its line is gone a session starts no response and holds the floor for none; whoever works on a tool call goes on. A reply that the dropped line cut off is owed again when the line is back. A session begins to close only when it is asked to, for the reason it is given, or by itself as abandoned when its line has been gone for as long as it waits for one, and then it does.

Invariants 1 to 5 close the incumbents' race classes by construction. General robustness to adapter interleavings is established by exploration, not by construction.

## 14. Verification strategy

| Technique | Scope |
|---|---|
| Kernel scenarios | Stories written before the kernel does what they ask, each closing a race from section 4.5 or a session shape from section 6: six before the kernel of phase 1, which all pass, and ten before phase 2, of which seven pass, the three about a session of text, the two about a line that drops and the two about keys, and three wait for the kernel to learn of words that come after their turn. A simulated world stands in for every adapter, and each story is run along every order in which its events could arrive. The scenario files are not edited to make them pass |
| Deterministic simulation, level 1 | A seed makes up a session: who is in it, when they speak, what the model writes and which tools it calls, which agent has the floor, and how each adapter answers. Eight profiles decide what goes wrong: calm, chaos, stalls, duplicates, shutdown, crashes, in which the process dies and the session goes on from what of its journal had reached the disk, lines, in which the line drops and is back in time, too late or never, and keys, in which the callers press keys among what they say and over the agent, under rules for a run that differ from session to session. `Kernel::step` is driven with a virtual clock, no executor needed, and a monitor that keeps its own account of the session checks the invariants after every step, that the journal alone adds up to the kernel's state, and that nothing is left unfinished at the end. Each session runs twice to show the kernel is deterministic. Every test run has 600 sessions per profile; a nightly run has 200,000, and other ones each night |
| Deterministic simulation, level 2 | The real session actor on an executor whose clock the test controls, with a fake in every port and the monitor of level 1 on every step. A seed makes up the session: who calls and what they say, how late every part answers, what the line does to the packets, which parts fail and when, what the application asks, and at which record of its journal the process dies, up to three times. Ten stories of the kernel scenarios are told again under many seeds, the six of phase 1, the two of a line that drops and the two of keys, and sessions nobody wrote are made up under six profiles: calm, chaos, crashes, shutdown, lines, and keys, in which the callers press keys and the application has the session press some, on a line that takes them as messages, on one that takes tones and on one that has none. Beyond the invariants, every run is checked on what the far end was sent (frames in order and on time, none after a clear or a close, and nothing into a line that is gone but the fade of what was cut off), that nothing is in the conversation that the far end did not play, that no tool with an effect was called before the record that allows it was stored, or twice, that the journal has no gaps, and that a session rebuilt from the journal is the session that wrote it. The sessions with keys are checked on two things more: that the keys a session was asked to press left as its line takes them, and that the keys in the conversation are the keys the callers pressed, in order. Sessions of text are run the same way, with nothing in the ports that hear and speak: four of the six stories of phase 1 are stories in writing too, and chats nobody wrote are made up under the first four profiles. Their far end is whoever follows them, and every run is checked on that: what is on record of an answer is what the follower was told of it, and nothing of an answer is told once it is over. Every state a session stores is held against what a client would be shown: the state, as the client protocol shows it, has to be what the facts before it add up to as the protocol shows them (section 8.6). Every test run has 600 sessions; a nightly run has 24,000, and other ones each night. Every failure is a seed |
| Property tests | The eleven invariants over arbitrary interleavings |
| Small-scope model check | Every order in which the events of a small session can arrive: speech, transcripts and estimates, model text and tool requests, synthesis and playback, tool results, timers, an interruption, a close, a line that drops and comes back, keys, and the death of the process, with the same monitor checking each step. One session with all of that would be too large, so there are several, each without what another has: up to 2 participants, 3 turns, 2 tools and 2 calls. Every test run tries about 300,000 states, and the nightly run about 13 million |
| Replay | A session is written down as the events its kernel was given, a line each. Six such traces are kept with what the kernel emitted for every line, and each test run gives a new kernel the same events and compares: a change in what the kernel does shows as the first line that differs. A path on which the model check or the simulation fails can be written down and run again in the same way. Flight-recorder traces from the runtime will use the format |
| Random networks | A seed makes up the network between a sender and the inbound path: jitter, loss, repeats, outages, bursts, and packets that arrive together. Rules are checked on what comes out: one unbroken timeline, each packet once and in order, never more than 60 ms made up in a row, every deadline in the future, and counters that add up. 500 networks in every test run. The outbound path, the codecs and the change of rate have exact tests of their own: a tone must come through the filter at its level, and one above the band must be 55 dB down |
| Fuzzing | `step` with structured events; every wire parser (SIP, SDP, RTP, client protocol); journal decode with snapshot round trip. Of these there is the client protocol's, as a test and not yet as a fuzzer that learns: a seed takes a message of the fixtures, bends it into JSON that says something else, damages its bytes, or both, and every parser of `waveport-protocol` is given the result. None may panic, and what one takes has to be written and read back as it was. 20,000 texts in every test run and a million every night |
| Concurrency checking | What a session and the compute plane hand each other: the frames of a stream, the answers, the wake-up of the session and of the thread that judges frames. `loom` tries every way the threads of small cases can take turns, in every test run, and fails on a frame that arrives in pieces or a wake-up that is missed. The rings themselves are `rtrb`'s and are stood in for there: what is tried is how they are used |
| Port conformance kit | Functions in `waveport-testkit` that hold any implementation of a port to the port's rules: a model's response ends once and says how, a recogniser's final transcripts are in order and inside the audio it was sent, a synthesiser's audio and marks add up, a journal fences and keeps its numbering, a stream that is dropped stops, and a transport says that its far end joined before anything else, numbers and places what the far end says, tells the far end whose audio follows and never takes more as played than was sent, says goodbye after what was sent before it, and stays over once it is. A transport's checks are driven through its own test client, as the far end. They run in every test run on the fakes, in every combination of what a fake can say it does, and on implementations made to break one rule at a time, to show that each rule is checked. Adapters run them against servers that replay what a provider said, or what its documentation says it says. Against the provider itself they run in a workflow of their own, every night and when somebody asks, for the providers whose keys the repository holds; that workflow never runs for a pull request, and a test fails if it is given a trigger for one |
| Whole sessions on fakes | A fake of every port, on virtual time: a transport whose far end speaks, plays what it is sent and says what it played; a recogniser, a model, a synthesiser and tools that follow a script; a journal in memory. The kernel, the audio path and the session actor are the real ones. Conversations written by hand are run through the ports and nothing else, one for each thing a session has to do or to live through: a question and its answer heard to the end, an interruption answered on the words that were heard, a tool call and its result, parts that can do less, what an application asks, keys a caller presses and keys the session is asked to press, a line that closes, a recogniser that fails, a journal that fails or is taken over, a process that dies, and a follower that comes back. Sessions of text have conversations of their own: an answer read as it is written, one that is typed over, two messages before the first is answered, a tool call, a chat that goes on from its journal. Sessions on real threads are started, driven, followed and ended, one that panics among them |
| A scripted call | The `waveport` command itself is started, with a caller who follows a script in place of the sound devices and scripted servers in place of the three providers. The models that listen, the adapters, the runtime and the audio path are the real ones. The caller asks a question in recorded synthetic speech; the agent has to call a tool, answer aloud, say goodbye and end the call, and the recording of what the caller was played has to hold the answer and nothing after it. The call is run with each recogniser and each synthesiser there is an adapter for, and once more with the agent in one process and the caller in another, over the WebSocket transport. A caller that is a program drops its connection there, is refused with a token that is not its call's, and is given its line back with the one that is. Runs in every test run, on all four platforms |
| A power cut in the middle of a commit | The journal in a file is put on a disk in memory that the test can break. A seed makes up a few sessions that write records, store states and are taken over, picks a commit and a write in it at which the power goes, and decides for every write that was not yet synced whether it is kept, lost or torn. The file is opened again, and nothing the journal had acknowledged may be gone, the commit that was cut has to be there whole for every session or for none, and the sessions have to go on. The run counts how often that commit was found whole and how often not at all, and fails if either never happened. A write that fails and a sync that fails have tests of their own. 60 cuts in every test run, and 20,000 others every night |
| A call that is killed | The `waveport` command is run as for a scripted call, with its journal in a file, and its process is killed as `kill -9` does it, at six places: while the model answers, while the agent speaks, once a tool has acted and before it has answered, while the record that allows a tool waits to be stored, and before and after the commit that stores the call's state. The places are too brief to hit from outside, so the command is told where to stop, says when it is there, and is killed then. It is started again with `--resume last` and a caller who asks once more. Each run checks that the second process goes on with the call, that the model is shown the conversation as the journal had it, that a tool that adds a line to a file has acted at most once in both processes together, that the caller is answered aloud, and that the call, once over, is not there to go on with. Once more the agent takes its calls over a WebSocket and only the agent is killed: it is started again with `--resume`, the caller comes back to it by itself, and is answered on the connection it came back on. Runs in every test run, on all four platforms |
| The daemon | `waveportd` itself is started with a configuration, and asked over HTTP as an application and a far end would ask it, with a scripted server in place of the language model. A chat is had from its first word to its last. A client comes back to a stream and is sent what it missed, or what the session has come to when it was away for more than a session holds, was told of another life, or names a fact that was not made; in each case what it then holds is compared with what a client holds that saw everything, by the reducer of the protocol. Who may ask what is tried with every credential at every door, and a page from elsewhere is refused on a daemon that asks for no key. The daemon is told to stop while an answer is written, and killed once its journal has a turn and before it has the answer that the client was already shown; the daemon that is started next has to go on with the session, which then gives another answer under numbers the client has seen, and the client has to be told what the session has come to in place of what it holds. Each rule of who may ask and of coming back was broken on purpose, twenty-three changes in all, and a test fails for each. A far end is on a line as well: a client of the WebSocket transport writes and is answered, stops the agent, drops its connection and comes back, and comes back to the daemon that is started after one was killed or told to stop. One speaks: it codes a question as Opus, a packet every 20 ms, and decodes the answer, with scripted servers for the providers that recognise and synthesise speech. What asks for a line as no client does is refused at each step of asking, a token in the address among them. The rules of lines were broken on purpose as well, thirty changes with the greeting of a far end that joins, and a test fails for each. A daemon that is told the processors of its shards is looked at from outside: the system is asked which processors each thread of the process may run on, and the shards have to be on theirs and every other thread off them. The twelve rules of that were broken on purpose too. Runs in every test run, on all four platforms; telling the daemon to stop is tried where there is a signal for it, and placing threads on Linux, while anywhere else the same file has to keep the daemon from starting |
| A browser on the daemon's page | The daemon is started with the models that listen and with stand-ins for its three providers, and a headless Chrome is put on the page it serves, driven over the pipe Chrome offers for that, with a recording of the question for a microphone and an output that goes nowhere. Four sessions are had from the page: a spoken call as Opus and one as plain samples, in which the agent greets the moment the page is on the line, hears the question, answers aloud and ends the call; a session in writing; and one whose line drops and is come back to. The page has to show the greeting, the question and the answer, each heard to its end, and has to have told the session that it played both responses to their last sample, in counts that never go back. The client's other tests run in Node: its reading and writing against the fixtures of the Rust protocol, its connection against a server that is the test, the counting of the audio thread sample by sample, and the voice against a stand-in for a browser's sound, where it has to be on the line in the turn in which its client is welcomed. Sixty-seven rules of the client, of the page and of how the daemon serves it were broken on purpose, and a test fails for each. Runs in every test run, on Linux; Firefox and Safari are not run |
| A stand-in for a carrier | The carrier transport is run against a stand-in that does what the carrier's documentation says: it holds the audio it is sent, says a mark back when it has played up to it, and at a discard says back every mark it throws away. The session's side has to be told what the caller was played as the stand-in plays it, nothing of what was thrown away, and of a response behind something else only when its turn has come. The stand-in complains of any message the documentation does not show, and a test that sends what a carrier takes has no complaint. The rules of every transport are held through it. What it cannot show is what a carrier does that its documentation does not say |
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
├─ apps/           waveportd · waveport-assembly · waveport-cli · console (web UI) · operator
├─ sdks/           js · react · swift · kotlin · react-native · flutter · embedded
├─ models/         training and evaluation code for the open models
├─ assets/         assets.lock.toml: model files pinned by address and SHA-256, fetched on demand
├─ perf/           recorded measurements and baselines
├─ spikes/         throwaway experiments, outside the workspace and its gates
├─ deploy/         Dockerfiles · Helm chart · examples
└─ docs/ · examples/
```

`xtask/`, `benches/`, `crates/core/`, `crates/testing/` (without `-evals`), `assets/`, `perf/` and `docs/` exist today (`spikes/` holds the two spikes of phase 2, on WebRTC and on speech-to-speech models; the density spike of phase 1 was deleted once the product could measure itself). So do the first adapters (`transport/local-audio`, `transport/websocket`, `transport/telephony-ws`, `stt/openai`, `stt/deepgram`, `stt/assemblyai`, `llm/openai-compat`, `tts/cartesia`, `tts/elevenlabs`, `models/onnx`, which holds both listening models, `store/memory`, `store/journal` and `telemetry/otel`), `apps/waveport-assembly`, `apps/waveport-cli` and `apps/waveportd`, and `sdks/js`. The rest is added in the phase that needs it.

The root [`Cargo.toml`](../Cargo.toml) is the source of truth for workspace members, lint levels, profiles and dependency versions. What it does not say by itself:

- **Only dependencies in use are declared.** Cargo and `cargo deny` reject unused ones. The pins already decided for later phases are: `tokio` `~1.53` (the long-term-support line); `str0m` `=0.25.0`, exact, because it breaks its API on most minor releases, and with none of its default features, because its stock crypto brings C ([decision 0005](decisions/0005-webrtc-endpoint.md)); `rsipstack` `=0.8.5`, exact, because it had 88 releases in twelve months; `ort` `=2.0.0-rc.15`, exact, because it is a release candidate.
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
| WebRTC | `str0m` 0.25.0 | Sans-IO; includes DTMF and G.722. No TURN client and no jitter buffer. Its pure-Rust crypto asks for `rcgen`, which brings aws-lc, so the transport has a crypto provider of its own: with it there is no C in the tree, and `deny.toml` stands as it is ([decision 0005](decisions/0005-webrtc-endpoint.md)) |
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
| WebSocket, HTTP, gRPC | `tokio-tungstenite` 0.30.0, `axum` 0.8.9, `hyper` 1.12.0, `reqwest` 0.13.5, `tonic` 0.14.9, `prost` 0.14.6 | `tonic` has been in maintenance since May 2026, and the `grpc` crate that is to follow it is a preview on another protobuf runtime. So generated code stays behind one module of the daemon, where the change will be made. The WebSocket client does not go through a proxy; the HTTP client uses the proxy the environment names |
| TLS | `rustls` 0.23 with `ring`, `tokio-rustls` 0.26.6, `rustls-platform-verifier` 0.7.1 | No OpenSSL, and no assembler to install on Windows. Certificates are checked by the platform's own verifier, so no list of roots is compiled in |
| A carrier's signature | `ring` 0.17.14 | The cryptography that is under `rustls` already. A signature is compared by it, in a time that does not tell how much of it fits. Twilio signs with HMAC-SHA1, which is kept for that and used for nothing else |
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
| JavaScript client | Nothing at run time. Checked with `typescript` 7.0.2 and `@types/node` 24.19.2 on Node.js 24, tested with Node's own runner and a headless Chrome | Checked on 17 November 2026. Exact versions in a committed lock file; npm takes nothing younger than seven days and runs no script of a package |

**Written in-repo because no usable crate exists:**

- Streaming clients for OpenAI (including Realtime), Anthropic, Gemini (including Live), ElevenLabs, Cartesia, AssemblyAI and Azure Speech. AWS has official crates; Deepgram has a community one.
- Batched Silero VAD and Smart Turn inference.
- For WebRTC: the reorder buffer, the unpacking of redundant audio, the framing of ICE over TCP, HTTP signalling, and the crypto provider.
- SIP call control, RTP sessions and RFC 4733 DTMF.
- The dispatcher, a server-sent-events parser, and OpenTelemetry GenAI attributes.

Multi-provider LLM crates (`genai`, `rig`) are rejected as foundations: they have no realtime support and change very fast.

**Dependency risks, each contained behind a port:**

| Bet | Risk | Containment |
|---|---|---|
| `ort` | Perpetual release candidate; native binaries | `tract` as a pure-Rust fallback for the small models |
| `str0m` | One core maintainer; frequent breaking changes | Wrapped by the transport port; `rtc` 0.21 is the alternative, and was not needed when the spike of phase 2 asked whether str0m can be had without aws-lc |
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
