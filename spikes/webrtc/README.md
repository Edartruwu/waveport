# WebRTC spike

Throwaway code for decision 0005. Its own Cargo workspace, outside the product workspace and its gates. What it found is in [`FINDINGS.md`](FINDINGS.md); the tables and the outputs of the dependency checks are under [`results/`](results/). The reports of the single runs are not in the repository: they hold the SDP of every call, and with it the addresses of the machine they were made on. `./batch-loss.sh` and `./batch-misc.sh` make them again.

## What is here

| Path | What it is |
|---|---|
| `provider/` | The crypto provider: `str0m-rust-crypto` 0.7.0 with its certificate made by RustCrypto instead of `rcgen`. This crate's dependencies are what would enter the product workspace |
| `endpoint/` | The endpoint: one UDP socket and one TCP listener for every peer, ICE-lite, RTP mode, Opus. It serves the test pages, drops packets by a seed, counts what it gets, and has a stub of Pipecat's SmallWebRTC signalling |
| `web/` | The pages a browser runs. `test.html` runs one call by itself and posts what it saw to `/report` |
| `pipecat/` | The stock Pipecat JS client, bundled for `web/pipecat/` (question 4) |
| `checks/stock/` | Only resolved, never built: shows that the stock `rust-crypto` backend of str0m compiles aws-lc |
| `run.sh` | Starts a headless browser on one experiment and waits for its report |
| `batch-loss.sh`, `batch-misc.sh` | The loss runs; every other experiment, one browser at a time |
| `analyze.py`, `table-loss.py`, `table-setup.py` | Read the reports: one run in full, the loss tables, the set-up table |
| `pack-results.py`, `results_io.py` | Pack the reports (a run is 0.3 to 2 MB as written, a twentieth packed); read them either way |

## Run it

```sh
cd spikes/webrtc
cargo build --release

# The dependency question
cargo tree -p spike-provider -i aws-lc-sys        # no such package
cargo deny --config ../../deny.toml check          # advisories ok, bans ok, licenses ok, sources ok
(cd checks/stock && cargo tree -i aws-lc-sys)      # the stock backend: aws-lc-sys, cmake, cc

# str0m against itself in memory: what a handshake and a packet cost
./target/release/spike-endpoint bench > results/bench-in-memory.json

# The endpoint. --ip is the address browsers on this machine reach it at;
# without it, the address of the interface with the default route.
./target/release/spike-endpoint serve --ip <address> &

# One experiment in a headless browser (Chrome or Firefox must be installed)
./run.sh chrome  'exp=basic-1&dur=6'
./run.sh firefox 'exp=basic-1&dur=6'
./analyze.py results/chrome-basic-1.json results/firefox-basic-1.json

# Loss at 5, 10 and 20 % both ways, with and without RED, FEC and NACK (about 4 minutes)
./batch-loss.sh && ./table-loss.py > results/loss-tables.md
# A run the table marks "disturbed" says little: run those again, fewer browsers at once
SERIAL=1 NO_ZERO=1 ONLY='chrome-loss(05|10|20)-fec$' ./batch-loss.sh

# Everything else: DTLS versions, data channel, DTMF, ICE restart, stopping, frame mode, WHIP, TCP (about 9 minutes)
./batch-misc.sh && ./table-setup.py > results/setup-table.md

# Shrink results/ before keeping it
./pack-results.py

# The stock Pipecat client against the stub
(cd pipecat && npm install && npm run build)
PAGE=pipecat/index.html ./run.sh chrome 'exp=pipecat&dur=8'            # connect({webrtcUrl})
PAGE=pipecat/index.html ./run.sh chrome 'exp=pipecat-start&start=1&dur=8'  # startBotAndConnect({endpoint: '/start'})
```

The endpoint is one thread and the measurements are of time, so run them on a machine that is doing little else. A run in which the endpoint itself fell behind is marked by `analyze.py` and `table-loss.py`.

The dependency question against the product lockfile was answered in a scratch copy of the product workspace, outside this directory: copy the workspace, add a crate `crates/adapters/transport/webrtc` with the dependencies of `provider/Cargo.toml` (through `[workspace.dependencies]`), then `cargo metadata`, `cargo deny check` and `cargo xtask arch-check` there, and compare the two lockfiles. What that gave is in `results/added-crates.md` and `results/deny-product-workspace.txt`.

Neither browser plays anything aloud: Chrome runs with `--mute-audio`, Firefox with `media.volume_scale` at 0. No microphone is opened; a page makes what it sends with Web Audio.

## The query of `test.html`

Read by the endpoint (`POST /offer`):

| Key | Meaning |
|---|---|
| `red=1`, `red_dist=1,2` | Answer with RED first and send RED, with redundant copies that many packets back |
| `fec=<percent>` | In-band FEC in the endpoint's encoder, with this expected loss |
| `nack=1` | Answer with `nack` for Opus and ask for retransmissions (the page needs `munge_nack=1`) |
| `loss_in`, `loss_out`, `seed` | Share of RTP packets dropped each way, by a seeded generator |
| `tcp=1`, `udp=0`, `drop_udp=1` | Advertise a passive TCP candidate; do not advertise UDP; ignore UDP |
| `dtls=12 \| 13 \| 0` | DTLS 1.2, 1.3, or whichever the first ClientHello says |
| `dcid=<n>` | A data channel agreed beforehand, with this id |
| `tele=1 \| 48 \| 8 \| 48first` | Telephone-event: both clock rates, only one, or both with 48000 listed first |
| `snap=1` | str0m's SNAP (the SCTP handshake carried in the SDP) switched on |
| `delay=<ms>` | Hold every packet this long in each direction |
| `frame_mode=1` | str0m's frame mode instead of RTP mode |
| `src=tones \| speech \| silence` | What the endpoint sends (and the page too) |

Read by the page: `exp`, `dur`, `munge_nack`, `dc`, `dtmf`, `server_dtmf` (with `ts48=1`: events on an 8000 payload type are stamped on the audio's clock), `stop_at`, `restart_at`, `whip` (see the top of `web/test.html`).

A session ends when its report is posted.

## The known signal

`src=tones`: every 100 ms a new pair of tones that is the number of that 100 ms (0 to 255). An audio worklet in the page (`web/detector.js`) reads the number back from what the browser plays, every 10 ms, so that what was heard can be set against what was sent and when.
