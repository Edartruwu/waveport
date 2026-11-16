# Spike: speech-to-speech providers

A throwaway that answers what the speech-to-speech port of phase 2 has to be, before the port is written (decision 0007). It has its own Cargo workspace and lockfile, and none of the product's gates build it. What it found is in [FINDINGS.md](FINDINGS.md).

- `src/bin/openai.rs`: OpenAI Realtime over a WebSocket. **Run** on 16 November 2026; its recordings are in `recordings/openai/`.
- `src/bin/gemini.rs`: Gemini Live over a WebSocket. **Not run**: there was no key. Written from the documentation as read on 16 November 2026. Only the refusal of a wrong key was recorded (`recordings/gemini/manual_turn.wrong_key_*.jsonl`).
- `src/lib.rs`: the connection that writes down every message, on the product's own WebSocket client.
- `tools/digest.py`: reads a recording short.
- `audio/make.sh`: makes the speech the runners send. A machine speaks it (Flite's voice `slt`, through ffmpeg), as the speech of `waveport-testkit` was made. Nobody was recorded. The audio files are not kept in git.

## Running it

```sh
cd spikes/realtime
sh audio/make.sh                                   # needs ffmpeg built with flite
cargo build --release

# OpenAI: the key is read from OPENAI_API_KEY and is never printed or written.
./target/release/openai <scenario> [--model gpt-realtime-2.1] [--tag full] \
    [--transcriber gpt-live-transcribe] [--effort minimal] [--idle 150] [--quiet]

# Gemini, once GEMINI_API_KEY is set: every scenario but the long one, then the long one.
./target/release/gemini all --quiet
./target/release/gemini go_away --quiet            # about twelve minutes
```

The default OpenAI model is `gpt-realtime-2.1-mini`. A run writes `recordings/<provider>/<scenario>[.<tag>].jsonl` and replaces the file that was there.

| OpenAI scenario | What it asks |
|---|---|
| `detector_on` | A spoken turn with the provider's detector on (`--tag semantic` for the semantic one): order of events, time to first audio, text against audio |
| `client_turn` | Turn detection off: commit and response by the client, when the words of the turn come, a response asked for without a commit |
| `signal_only` | The detector on with both automatic flags off: what still comes, what the provider commits by itself, a clear and a commit under it |
| `drop_input` | `input_audio_buffer.clear` in the middle of an utterance |
| `interrupt` | Cancel and truncate, in both orders; truncating again, earlier, later, beyond the end |
| `unasked` | Whether the provider ever speaks by itself, and what a response with nothing to say is |
| `tool` | A function call: order, arguments, the result, a late result, a duplicate result |
| `inject` | Items of every role, with the client's ids and in the middle; responses with their own settings |
| `aside` | Whether an out-of-band response leaves anything in the conversation |
| `update` | What `session.update` changes while connected |
| `formats` | Which audio is taken and given, and a turn in G.711 |
| `limits` | Expiry, wrong commands and their errors, rate limits, usage, an idle connection |
| `seed` | A new session given 20, 100 and 400 items of history |
| `reconnect` | Whether anything of a session outlives its connection |

| Gemini scenario | What it asks |
|---|---|
| `manual_turn` | `activityStart`, audio, `activityEnd`: what is acknowledged, when the response starts, when the words come, text against audio |
| `held_input` | What holding a turn until it is over costs against streaming it |
| `drop_input` | What becomes of audio in an activity that is never ended, and of audio outside any activity |
| `interrupt` | A response stopped by `activityStart`, by `clientContent` and by empty `clientContent`: what the history keeps |
| `silence` | A turn the model chooses not to answer |
| `tool` | Blocking and non-blocking calls, the three schedulings, late and duplicate results, a call the provider cancels |
| `seed` | Initial history of 20, 100 and 400 turns; a model turn injected mid-session |
| `update` | Input formats, a second setup message, wrong messages |
| `resume` | Handles, the windows without one, a second connection with the same and with another setup |
| `detector` | The provider's own detector, `voiceActivity`, interim words, `audioStreamEnd` as a forced end of turn |
| `go_away` | The notice before a connection ends, and the resume after it (long) |

If the Gemini runner's first run fails at setup, two flags cover the two places the documentation disagrees with itself: `--modalities-at-top` (the tutorial puts `responseModalities` beside `model`; the reference puts it in `generationConfig`, which is the default here) and `--key-in-url` (the documented `?key=`; the default here is the `x-goog-api-key` header, which the service was seen to read).

## Reading a recording

```sh
python3 -I tools/digest.py recordings/openai/interrupt.jsonl            # one line a message, runs folded
python3 -I tools/digest.py recordings/openai/interrupt.jsonl --notes    # only what the runner concluded
python3 -I tools/digest.py recordings/openai/detector_on.jsonl --align  # text against audio received
python3 -I tools/digest.py recordings/openai/*.jsonl --usage            # tokens
```

Each line of a recording is `{"t", "dir", "sent", "got", "event"}`: milliseconds since the connection was asked for, `out`, `in` or `note`, samples of audio sent and received so far, and the message with its audio replaced by a length. No sound is in a recording, no key, and no handle that resumes a session.
