# Raw outputs of the WebRTC spike

Made on 16 November 2026 on the laptop of decision 0001 (Core Ultra 7 155H, Linux 7.0), with Rust 1.100.0, str0m 0.25.0, Google Chrome 157.0.8103.38 and Mozilla Firefox 158.0 (snap), both headless. Other builds ran on the machine during the afternoon. A run in which the endpoint itself fell behind (more than 8 frames sent over 45 ms after the one before) or the browser skipped more than 1% of its own frames was run again; what is here are the clean runs. Times from single runs are indications.

The reports of the single runs hold the SDP of every run, and with it the private network addresses of the machine they were made on. They are left out of the repository for that (`../.gitignore`): the three kinds of file in the last rows of the table below are made again by `../batch-loss.sh` and `../batch-misc.sh`.

| File | What it holds |
|---|---|
| `tree-stock-rust-crypto.txt` | `cargo tree -i` for aws-lc-sys, openssl-sys, ring, rcgen, cc and cmake with str0m's stock `rust-crypto` feature |
| `tree-own-provider.txt` | The same for the provider of this spike |
| `provider-crates.txt` | Every crate under `spike-provider`, with versions |
| `deny-own-provider-spike.txt` | `cargo deny --config ../../deny.toml check` in the spike |
| `deny-product-workspace.txt` | `cargo deny check` in a scratch copy of the product workspace with the transport's dependencies added |
| `added-crates.md` | The 65 crates that copy's lockfile gained: licence, build script, C or assembly, date published |
| `provider-under-workspace-lints.txt` | What the workspace's lints say about the copied provider code |
| `bench-in-memory.json` | str0m against itself in memory: handshakes and packets |
| `loss-tables.md` | The loss runs (`<browser>-loss<percent>-<setting>`), tabulated by `table-loss.py` |
| `setup-table.md` | When each side could first do what, tabulated by `table-setup.py` |
| `<browser>-<experiment>.json.gz` | One run, packed by `pack-results.py`: what the page saw (`marks`, `states`, `ssrc`, `stats`, `heard`, the SDP) and, under `server`, what the endpoint saw (`rx`, `tx`, `cost`, `events`, `trace`). `../analyze.py results/<browser>-<experiment>.json` reads it |
| `sdp/` | Offer and answer of each run |
| `pipecat-messages.md` | What the stock Pipecat client and the stub said on the data channel, and the client's callbacks |
| `pipecat-http-*.json` | Every request to the stub of Pipecat's signalling and its reply, for three of the runs |

The experiments by name: `basic-1..3` (nothing special), `dtls13`, `dtls-auto`, `dc-inband`, `dc-negotiated`, `rtt100-*` (50 ms added each way), `tcp-*`, `dtmf*`, `ice-restart`, `stop-1..3` and `stop-red`, `frame-mode-*`, `whip`, `red-offer`, `nack-answer-only`, `rtt100-snap-dc-negotiated`, `pipecat*`.
