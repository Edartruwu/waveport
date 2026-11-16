# Decision 0001: inference backend, allocator and density

**Date:** 15 November 2026. **Status:** accepted. **Phase:** 1, milestone 1.

The roadmap puts a density spike first in phase 1, before any product code, because three choices in the [architecture](../architecture.md) rested on estimates: which library runs the local models, which allocator the binaries use, and how many voice sessions one core carries. This record states what was decided and shows the measurements.

The code is in [`spikes/density/`](../../spikes/density/). It is a throwaway, outside the workspace and its gates. The raw numbers are in [`perf/spike/2026-11-15-core-ultra-7-155h.json`](../../perf/spike/2026-11-15-core-ultra-7-155h.json).

## Decisions

| Question | Decision | Why |
|---|---|---|
| Inference backend | `ort` 2.0.0-rc.15 with ONNX Runtime 1.31. `tract` is not used | `tract` cannot load the standard Silero export. On Smart Turn it is 2.6 times slower and its outputs differ from ONNX Runtime's by up to 0.78 |
| Voice-activity batching | Run as soon as a frame is waiting, with everything queued, up to 64 frames | Waiting 2 ms to fill a batch saves 14% of inference time and delivers the result 2 ms later, which misses the latency target |
| Allocator | `mimalloc` | The three allocators tested cost the same CPU. jemalloc uses 8% less memory and starts a session 65 µs sooner at p99, which does not pay for having no Windows target |
| Density | Plan for 60 active voice sessions per core. The target of 100 is withdrawn | One session costs 1.17% of a core, not the 0.7 to 1% estimated. End-of-turn inference, which the estimate left out, is half of it |
| Inline work budget on a shard | 250 µs per call, up from 100 µs | One Opus encode takes 110 to 140 µs on average and up to 215 µs |

## How it was measured

**Machine.** A laptop with an Intel Core Ultra 7 155H (6 P-cores with two threads each, 8 E-cores, 2 low-power E-cores), 30 GiB of memory, Linux 7.0, on mains power with the performance profile. Rust 1.100.0, release build with fat LTO and one codegen unit.

**Layout.** One shard: a thread pinned to a P-core, running a single-threaded Tokio `LocalRuntime` with one task per session. One batching thread for voice-activity inference, pinned to a second P-core. Two threads for end-of-turn inference, each on its own P-core. The load generator runs on an E-core. Every pinned thread has a physical core to itself; its sibling thread is left idle.

**One session**, modelled on a ten-second conversation cycle: the user speaks for 4 s, then the agent for 4.5 s.

| Work | Model in the spike |
|---|---|
| Inbound audio | A real Opus packet (fullband, 32 kbit/s, as a browser sends) every 20 ms, all the time. Decoded at 16 kHz and copied into an 8 s pre-roll ring and a 2 s speech-to-text ring |
| Voice activity | A 512-sample frame with 64 samples of context, every 32 ms, pushed through a lock-free ring to the batching thread. Recurrent state is kept per session on that thread |
| End of turn | One Smart Turn inference per cycle, 0.2 s after the user stops: the last 8 s of audio, Whisper log-mel features, then the model |
| Outbound audio | An Opus frame encoded from 24 kHz speech at 32 kbit/s every 20 ms while the agent speaks (45% of the time), and sent with a real system call on a UDP socket |
| Provider traffic | 50 messages a second of 100 to 4,000 bytes: allocated, filled, scanned twice (standing in for framing and parsing), and one in ten kept as conversation context, up to 64 |
| Session state | The buffers the design sizes: 8 s pre-roll, 2 s speech-to-text ring, 2 s playout, 32 KB of kernel state, a 256-slot mailbox |
| Scheduling | A 5 ms shard tick that paces outbound frames and collects inference results |
| Churn | Sessions are closed and replaced at a set rate: 5% a minute in the density runs |

The audio is synthetic speech from a text-to-speech engine. No human recording was used.

**What the model leaves out.** Kernel steps (the design target is 5 µs each, perhaps 100 to 150 a second for a session), the journal, TLS on provider connections, SRTP, the reorder buffer, and loss concealment. This record estimates, without having measured it, that together they add about a tenth to the per-session cost. Each gets its own benchmark when it is built.

**Run hygiene.** CPU cost is the on-CPU time of each pinned thread, read from the scheduler, divided by wall time. The driver starts a run only when the package is cool and the machine idle, measures how much CPU every other process used during the run, and checks a calibration loop on the shard's core before and after. A run that fails any check is repeated. All 21 runs reported here passed all three checks. Runs are 40 s after a 10 s warm-up, except the churn runs, which are 120 s.

**Three limits of this machine** bound what the numbers mean:

- With all six P-cores busy the package reaches 100 °C and the cores slow to between a third and a sixth of their single-core speed. A layout with one shard per core therefore measured the cooling, not the code, and was abandoned. The roadmap's 1,000-session run was not possible on this hardware.
- The cores slow down as more of them work. One Smart Turn inference takes 39 ms when the machine carries 25 sessions and 58 ms when it carries 150, on the same core. Per-session cost therefore rises with load, and a figure taken on a lightly loaded machine flatters a busy one.
- The loaded cores run near single-core turbo frequency. A server with every core busy runs each core slower than that. "A core" below means one P-core of this laptop with its sibling idle. A cloud vCPU is one of two threads on a slower core and will carry fewer sessions; by how much was not measured.

## Results

### 1. Inference backend

Each model was loaded in both libraries, run on the same input, and timed on one pinned core. The voice-activity model ran over 17.5 s of speech frame by frame; the end-of-turn model ran on 33 windows ending every half second of the same clip.

Silero VAD v6.2.3 comes as a standard export, which contains `If` operators, and an export without them.

| Backend | Model file | Largest difference from `ort` on the standard export | µs per frame at batch 1 | at batch 8 | at batch 64 |
|---|---|---|---|---|---|
| `ort` | standard | reference | 70.7 | 42.2 | 38.0 |
| `tract` | standard | does not load | | | |
| `ort` | without `If` | 6.0e-7 | 63.2 | 41.0 | 38.1 |
| `tract` | without `If` | 1.4e-6 | 58.0 | 44.3 | 41.2 |

`tract` fails to analyse a nested `If` in the standard export. On the export without `If` it agrees with `ort` and is 8% slower at batch 64. In both libraries a stream's output is bit-for-bit the same whether it runs alone or in a batch of 64, so batching across sessions is safe.

Smart Turn v3.2 (the quantised CPU model):

| Backend | Inference p50 | p99 | Largest difference from `ort` | Mean difference |
|---|---|---|---|---|
| `ort` | 39.8 ms | 53.7 ms | reference | |
| `tract` | 104.1 ms | 120.8 ms | 0.78 | 0.31 |

The model's output is a probability, so a difference of 0.78 is a different answer. `ort` gives 0.98 at the end of the clip, where the last sentence ends, 0.96 and 0.51 at two earlier points, and under 0.25 at the other 30. `tract` gives values between 0.18 and 0.69 with no such pattern, and 0.20 at the end of the clip. The result is the same whether `tract` is given a fixed input shape or the model's symbolic batch size. The unquantised model was not tried: at 2.6 times the cost `tract` is ruled out either way.

The rule set before measuring was: `tract` if it loads both models, matches `ort` (voice activity within 1e-3, end of turn within 2e-2) and costs at most 1.5 times as much. It fails the first two on one model each. **`ort` is used.**

Consequences:

- ONNX Runtime is a C++ library. The spike let the `ort` crate download a prebuilt copy at build time. The product will not: it will load the library dynamically from a file pinned in `assets/assets.lock.toml`, like the models.
- An `ort` session needs exclusive access to run, so each compute thread owns its sessions.
- The standard Silero export is the one to ship. The export without `If` stays in the asset lock only while this spike exists.

### 2. Batching

Batching across sessions makes voice-activity inference 1.9 times cheaper per frame at best (70.7 µs alone, 38.0 µs at batch 64). The architecture assumed 2 to 4 times.

Two policies were run at 100 sessions:

| Policy | Mean batch | Voice-activity cost per session | Frame pushed → result computed, p50 | p99 |
|---|---|---|---|---|
| Wait for 64 frames or 2 ms | 11.9 | 0.145% of a core | 2.36 ms | 3.15 ms |
| Run as soon as a frame waits | 4.0 | 0.167% of a core | 0.39 ms | 0.72 ms |

Waiting saves 0.02% of a core per session, 2% of what a session costs, and by construction puts the median result 2 ms later. The target in the architecture is 1.5 ms at p50 and 3 ms at p99. Without the wait batches still form, because frames queue while an inference runs, and they grow with load. **The batcher runs as soon as a frame is waiting.**

In both cases the session sees the result later than the compute thread produces it, 3.1 to 3.4 ms after the frame at p50 and 5.2 to 7.9 ms at p99, because the shard collects results on its 5 ms tick. To meet the target at the session, the compute thread has to wake the shard. That is a requirement on the runtime in milestone 7.

### 3. What one session costs

Seven clean runs at 100 sessions with default settings (encoder complexity 10, waiting batcher):

| Part | Share of a core per session | Range over the runs | What it is |
|---|---|---|---|
| Shard thread | 0.45% | 0.446 to 0.460 | Opus encode 0.32%, Opus decode 0.10%, provider messages 0.02%, everything else 0.01% |
| Voice-activity inference | 0.14% | 0.142 to 0.145 | 31 frames a second at 46 µs |
| End-of-turn inference | 0.57% | 0.567 to 0.585 | One inference per 10 s: 46 ms for the model, 11 ms for the features |
| **Total** | **1.17%** | 1.157 to 1.188 | |

The same load at other session counts:

| Sessions | Shard busy | Shard cost | Voice activity | End of turn | Total per session | Tick late p99 | Ingress delay p99 | Frames dropped |
|---|---|---|---|---|---|---|---|---|
| 25 | 14% | 0.571% | 0.229% | 0.497% | 1.30% | 1.2 ms | 0.9 ms | 0 |
| 50 | 25% | 0.491% | 0.170% | 0.510% | 1.17% | 1.2 ms | 1.2 ms | 0 |
| 75 | 35% | 0.468% | 0.155% | 0.534% | 1.16% | 1.3 ms | 1.8 ms | 0 |
| 100 | 46% | 0.460% | 0.145% | 0.583% | 1.19% | 1.3 ms | 2.4 ms | 0 |
| 125 | 57% | 0.455% | 0.137% | 0.633% | 1.23% | 1.3 ms | 3.1 ms | 0 |
| 150 | 70% | 0.469% | 0.140% | 0.716% | 1.33% | 1.3 ms | 3.7 ms | 0 |
| 175 | 89% | 0.506% | 0.156% | 0.784% | 1.45% | 3.7 ms | 11.5 ms | 0 |
| 200 | 100% | 0.500% | 0.375% | 0.992% | 1.87% | 134 ms | 470 ms | 10,997 |

Tick lateness includes up to 1 ms that Tokio's timer adds by itself: its resolution is 1 ms.

What the table says:

- **One shard thread holds its timing up to about 150 sessions**, at 70% busy. At 175 it is 89% busy and the tick is 3.7 ms late at p99. At 200 it is saturated and drops frames.
- **A session's whole cost is 1.17% of a core** in the range where the machine is not yet slowing down (50 to 100 sessions). That is 85 sessions per core with nothing to spare, and 64 at 75% load.
- **The cost rises as the machine fills**, for the reason given under the machine's limits: 1.33% at 150 sessions, when about two cores are busy.
- **Half the cost is end-of-turn inference.** The architecture's estimate did not include it.

**The planning figure is 60 active voice sessions per core.** The arithmetic gives 58: 1.17% measured, about a tenth added for what the model leaves out, and a core kept at 75%. The earlier target of 100 is withdrawn. The plan's rule for this outcome was to restate the figure if the spike measured between 50 and 100, and to stop if it measured under 50.

Settings that lower the cost, each measured at 100 sessions:

| Setting | Shard | Voice activity | End of turn | Total | Change |
|---|---|---|---|---|---|
| Defaults | 0.460% | 0.145% | 0.583% | 1.19% | |
| Opus encoder complexity 5 instead of 10 | 0.384% | 0.144% | 0.601% | 1.13% | Encode takes 110 µs instead of 141 µs |
| Skip voice-activity inference on frames below -48 dBFS | 0.444% | 0.069% | 0.567% | 1.08% | Two frames in three are skipped |
| Both | 0.386% | 0.069% | 0.555% | 1.01% | |

Neither is adopted here. Encoder complexity is a quality trade that milestone 5 decides by listening. Skipping silent frames leaves the model's recurrent state without the silence it was trained to see, so it needs a detection-quality test first. A third lever was not measured: the spike's feature extraction for Smart Turn takes 11 ms with a plain FFT, and a tuned one should take a fraction of that.

### 4. Allocator

At 100 sessions and 5% churn a minute, two runs each:

| Allocator | Shard cost per session | Memory before any session | Memory per session | First sessions: start p50 | p99 |
|---|---|---|---|---|---|
| mimalloc 0.1.52 | 0.451%, 0.454% | 149 MB | 907 KB | 25 µs | 631 µs, 655 µs |
| jemalloc (`tikv-jemallocator` 0.7.0) | 0.453%, 0.455% | 129 MB | 889 KB | 27 µs | 106 µs, 98 µs |
| System (glibc) | 0.446%, 0.454% | 125 MB | 846 KB | 37 µs, 41 µs | 180 µs, 314 µs |

Under churn, 150 sessions with each replaced about every 40 s, for two minutes (450 sessions started):

| Allocator | Memory after warm-up | At the end | Session start p50 | p99 | Slowest |
|---|---|---|---|---|---|
| mimalloc | 274 MB | 278 MB | 57 µs | 180 µs | 293 µs |
| jemalloc | 252 MB | 255 MB | 57 µs | 115 µs | 124 µs |
| System (glibc) | 245 MB | 248 MB | 66 µs | 115 µs | 128 µs |

"Memory per session" divides everything the process gained by the session count, so it includes a fixed part. Fitted over the ramp, mimalloc holds about 30 MB per shard plus 0.7 MB per session. The memory before any session is mostly ONNX Runtime and the two models.

The three cost the same CPU. Memory stays flat under churn with all three. jemalloc uses 8% less memory than mimalloc and the system allocator 11% less. Session start is far inside its target (2 ms at p50, 10 ms at p99) with all three, and mimalloc is the slowest at p99.

The rule set before measuring was: mimalloc, unless jemalloc is at least 10% better on memory after churn or at least 20% better on p99 session start. jemalloc is 8% better on memory and 36% better on p99 session start, so read literally the rule selects jemalloc. **It is not followed, and mimalloc is used.** The rule compared ratios and should have had a floor: the difference is 65 µs on an operation that happens once per call and has a 10 ms budget. Against that, jemalloc does not build for Windows, which is one of the four platforms the gates run on, so choosing it means a second allocator there and behaviour that differs by platform.

On this evidence alone the system allocator would do as well as either. mimalloc is kept over it for two things a two-minute run on one operating system cannot show: the same allocator behaviour on all four platforms, and memory that stays flat over days in a process with many threads. Milestone 11's soak test checks the second. If mimalloc shows no advantage over the system allocator there, the dependency is dropped. The choice is one line in a composition root, and nothing depends on it before milestone 8.

### 5. Other findings

| Finding | Number | Consequence |
|---|---|---|
| An Opus encode exceeds the 100 µs inline budget | 141 µs mean, 213 µs p99 at complexity 10; 110 µs and 180 µs at complexity 5. A decode takes 21 µs | The budget becomes 250 µs. Encode stays inline: at 100 sessions a tick encodes about eleven frames, 1.6 ms of work |
| End-of-turn inference sits on the response path and queues | Request to result with two threads: 59 to 63 ms p50 and 91 to 101 ms p99 at 100 sessions (threads 23% and 36% busy); 168 ms p99 at 150; 252 ms p99 at 175 | End-of-turn threads are provisioned for latency, not throughput: under about a third busy. Their count is a setting |
| Smart Turn needs exact Whisper features | The spike's own implementation gives 0.98 at the end of the clip and low values at most other points. That is plausible, not a test | Milestone 8 tests its feature extraction against vectors from the reference implementation |
| Tokio's timer resolution is 1 ms | Median tick lateness is either 0.2 to 0.3 ms or 1.0 to 1.3 ms, depending on where the run started | The pacer computes deadlines from the clock, never by counting ticks |
| Memory per session is under its target | About 0.7 MB with the design's buffers; the target is 1.5 MB | None |
| Session start is under its target | 25 to 66 µs at p50, under 1 ms at p99, without connections | None |
| Cores slow down as more of them work | Smart Turn inference 39 ms at 25 sessions, 47 ms at 100, 58 ms at 150 | Density gates compare against a calibration taken in the same run, as milestone 11 already plans |

## What is not answered

- **A fully loaded machine.** Everything here is one shard with three helper threads. The 1,000-session run needs hardware that can keep every core busy without throttling.
- **Other platforms.** All numbers are x86-64 Linux. Nothing was measured on arm64, macOS or Windows.
- **Long runs.** The longest run is two minutes. Memory growth over hours is milestone 11's soak test.
- **What the model leaves out**, listed above.
- **Local noise suppression**, whose cost the architecture estimated and nobody has measured.

## Reproducing

```sh
cargo xtask fetch-assets
cd spikes/density
mkdir -p bin
cargo run --release --bin probe -- --cpu 6 --out probe.json
for allocator in mimalloc jemalloc; do
    cargo build --release --bin density --features alloc-$allocator
    cp target/release/density bin/density-$allocator
done
cargo build --release --bin density && cp target/release/density bin/density-system
./run_matrix.py bin matrix.jsonl ramp knobs alloc soak
./summarise.py probe.json matrix.jsonl
```

The test audio is generated with `ffmpeg`; the commands are in [`spikes/density/README.md`](../../spikes/density/README.md). The CPU numbers in `run_matrix.py` are specific to the machine above.
