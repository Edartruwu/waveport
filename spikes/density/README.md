# Density spike

Throwaway code for the first item of roadmap phase 1. It answers three questions before product code depends on the answers: which library runs the local models, which allocator the binaries use, and what one voice session costs. The answers, and the numbers behind them, are in [decision 0001](../../docs/decisions/0001-density-spike.md).

This directory is its own Cargo workspace. The repository's gates do not build it, and it will be deleted once the product has benchmarks for the same questions.

## Contents

| Path | What it is |
|---|---|
| `src/bin/probe.rs` | Loads both models in `ort` and in `tract`, compares their outputs and times one inference |
| `src/bin/density.rs` | The harness: synthetic sessions on a pinned shard, a batching inference thread and end-of-turn threads |
| `src/backend/` | The two inference backends behind one interface |
| `src/mel.rs` | The Whisper log-mel front end that Smart Turn needs |
| `run_matrix.py` | Runs the harness over the measurement matrix, repeating any run the machine disturbed |
| `summarise.py` | Turns the raw output into the tables of the decision record |

## Running it

The harness reads the CPU time of its own threads from `/proc`, so it needs Linux. Building needs `cmake` (for libopus) and network access: the `ort` crate downloads a prebuilt ONNX Runtime at build time. The product will not do that; see the decision record.

Model files:

```sh
cargo xtask fetch-assets        # from the repository root
```

Test audio. It is synthetic speech; no human recording is involved. `ffmpeg` must be built with `flite`:

```sh
mkdir -p ../../target/spike && cd ../../target/spike
cat > speech.txt <<'EOF'
Hello, I would like to change the delivery address for my order. The parcel was supposed to arrive on Thursday, but I will be away until the following week. Could you hold it at the depot, or send it to my office instead? The office is open from nine until half past five. Thank you, that would be very helpful.
EOF
# What the agent says, at the 24 kHz a text-to-speech provider returns.
ffmpeg -f lavfi -i "flite=textfile=speech.txt:voice=slt" -ar 24000 -ac 1 -c:a pcm_s16le speech24k.wav
# The same clip at 16 kHz, for the probe.
ffmpeg -f lavfi -i "flite=textfile=speech.txt:voice=slt" -ar 16000 -ac 1 -c:a pcm_s16le speech16k.wav
# What the user sends: 4 s of speech, then 6 s of room noise, at 48 kHz.
ffmpeg -f lavfi -i "flite=textfile=speech.txt:voice=slt" \
    -f lavfi -i "anoisesrc=color=pink:amplitude=0.002:sample_rate=48000:duration=10" \
    -filter_complex "[0]atrim=0:4,aresample=48000,apad=whole_dur=10[s];[s][1]amix=inputs=2:duration=first:normalize=0" \
    -ac 1 -ar 48000 -c:a pcm_s16le user48k.wav
```

Then, from this directory:

```sh
cargo run --release --bin probe -- --cpu 6
cargo run --release --bin density -- --sessions 100 --shard-cpus 6 --batcher-cpus 8 \
    --turn-cpus 10,0 --generator-cpus 12
```

`--features alloc-mimalloc` or `--features alloc-jemalloc` selects the allocator; without either the system allocator is used. `run_matrix.py` expects the three builds side by side as `density-mimalloc`, `density-jemalloc` and `density-system`.

The CPU numbers must suit the machine: each pinned thread should have a physical core to itself. `lscpu --extended` shows which logical CPUs share a core.

## Options of the harness

| Option | Default | Meaning |
|---|---|---|
| `--sessions` | 100 | Sessions per shard |
| `--shard-cpus` | 6 | One shard per listed CPU |
| `--batcher-cpus` | 8 | Voice-activity batching threads; shards are spread over them |
| `--turn-cpus` | 10 | End-of-turn threads; `none` turns end-of-turn inference off |
| `--generator-cpus` | 12,13,14,15 | Where the load generators run |
| `--backend` | ort | `ort` or `tract` |
| `--policy` | window | `window`: run a batch at 64 frames or 2 ms. `greedy`: run as soon as a frame waits |
| `--complexity` | 10 | Opus encoder complexity |
| `--churn` | 0.05 | Fraction of the sessions replaced each minute |
| `--energy-gate` | off | Skip voice-activity inference on frames below -48 dBFS |
| `--warmup`, `--measure` | 15, 60 | Seconds |
| `--out` | none | Append the run as one JSON line |
