# WebRTC spike: what it found

For decision 0005. Made on 16 November 2026 on the laptop of decision 0001 (Core Ultra 7 155H, Linux 7.0), Rust 1.100.0.

| | Version |
|---|---|
| str0m | 0.25.0 (`is` 0.12.0, `str0m-proto` 0.8.0, `dimpl` 0.9.0, `sctp-proto` 0.10.8) |
| Google Chrome | 157.0.8103.38, `--headless=new` |
| Mozilla Firefox | 158.0 (snap), `--headless` |
| Safari | **not available on Linux: nothing below says anything about Safari** |
| Pipecat JS client | `@pipecat-ai/client-js` 1.16.0, `@pipecat-ai/small-webrtc-transport` 1.11.2, RTVI protocol 2.3.0 |

**How to read this.** Everything is measured here unless a sentence begins with **Read:**, which means it was read in source code or documentation and not measured. Browser and endpoint ran on the same machine over its LAN address, so the network adds nothing: no delay (except where 50 ms each way was added on purpose), no jitter, and loss only where the endpoint dropped packets by a seeded generator, each packet independently. Real networks lose in bursts, which is harder on redundancy than what was measured. Other builds ran on the machine during the afternoon; a run in which the endpoint itself fell behind was thrown away and repeated (how that is detected is at the end). Times from single runs are indications.

## In brief

| # | Question | Answer |
|---|---|---|
| 1 | str0m without aws-lc, OpenSSL or ring | **Yes.** With `default-features = false` and a crypto provider of ours (the stock one with its certificate made by RustCrypto instead of `rcgen`): no aws-lc-sys, no openssl-sys, no ring, no C at all. 65 crates join the product lockfile, none changes, `cargo deny check` passes with `deny.toml` unchanged. `rtc` 0.21 was not looked at |
| 2 | A minimal endpoint | Built: one UDP socket and one TCP listener for all peers, ICE-lite, HTTP offer and answer, RTP mode, Opus. Both browsers connect to it and accept its certificate |
| 3a | RED | Chrome offers it, sends it once the answer lists it first (one redundant copy, the frame before), and decodes ours at distance 1 and at 1 and 2. **Firefox does not offer RED**, so with Firefox there is none in either direction |
| 3b | Opus in-band FEC | Works both ways. Ours to the browser: concealment falls from 5.0 / 10.4 / 20.9% to 1.3 / 3.0 / 8.0% at 5 / 10 / 20% loss. The browser's to us: it switches LBRR on by itself when our receiver reports show loss, and recovers about 60% of single losses |
| 3c | ICE over TCP | Works in both browsers with a passive candidate on the same port. It costs 60 to 260 ms more to connect on loopback |
| 3d | Audio NACK | Works for the uplink only, and needs no munged offer: `nack` in the answer is enough. It gets nearly every packet back, half of them within 40 ms plus a round trip. Against RED it wins only if we wait 100 ms or more; with Firefox it is the only repair there is beyond LBRR. It switches the browser's FEC off |
| 3e | Data channel, pre-agreed or in band | Pre-agreed saves one round trip: for our first message with Chrome, for both sides with Firefox. Usable 5 round trips after the answer on DTLS 1.2, 4 on DTLS 1.3 (Firefox: 6 and 5) |
| 3f | Cost | A handshake: 0.34 to 1.5 ms of CPU, its costliest datagram 0.19 to 1.2 ms, so it does not fit a 250 µs slice. A packet in: 9 to 13 µs at the median with real browsers (1 µs in a hot loop), out: 5 to 8 µs. DTLS 1.3 works with both browsers and saves the browser a round trip |
| 3g | ICE restart by a second offer | Works in both; no new DTLS handshake, no packet lost |
| 3h | DTMF | Both directions work, with conditions: answer `telephone-event/48000` alone to Chrome, send on the audio's clock, and do not trust the duration field |
| 3i | Playback reports | `getSynchronizationSources().rtpTimestamp` follows what we sent to one packet in Chrome; in Firefox it stops 40 to 60 ms short when we stop. `jitterBufferEmittedCount` is exact in both but counts an amount, not a position. After we stop, 35 to 75 ms of real audio still plays, then about 120 ms of audible made-up audio |
| 3j | Frame mode or RTP mode | RTP mode. At 10% loss frame mode holds the median frame back 181 ms |
| 4 | Stock Pipecat client | Reaches `ready` and plays our audio in Chrome and Firefox against an ICE-lite, audio-only answer that refuses its two video lines |

## What touches the plan

1. **Firefox has no audio RED.** "Redundant audio both ways" holds for Chrome only. With Firefox the repairs are in-band FEC both ways and NACK for the uplink.
2. **Audio NACK needs no munged offer, is uplink only, and fights with FEC.** `a=rtcp-fb:<opus> nack` in the answer alone made both browsers retransmit. str0m does not answer the browser's NACKs for our audio (Chrome sent 1,174 of them in 30 s and got nothing back). And while NACK is on, our receiver reports show no loss, so the browser never switches LBRR on. The plan keeps NACK "only if it recovers more than redundancy alone": with Chrome it does so only when we are willing to wait about 100 ms; with Firefox it recovers far more than FEC alone (0.6% left against 4.3% at 10% loss, waiting 100 ms).
3. **DTLS 1.3 worked with both browsers** in the nine handshakes tried, and str0m's default is DTLS 1.2, so "a setting that is off" is what one gets by not asking. Two things come with switching it on: the browser is connected one round trip sooner, and the SRTP profile becomes whatever the browser lists first, which with Chrome is `SRTP_AES128_CM_HMAC_SHA1_80` (about twice the per-packet cost of the AES-GCM profile, and weaker).
4. **str0m as ICE-lite does not answer the ClientHello until the nominating check has come.** Chrome sends its ClientHello right after the first check and nominates on its next one, 48 to 61 ms later on loopback. Nothing to change in the plan, but about 50 ms of every Chrome call set-up on a fast network is this wait.
5. **The socket must send from the address it advertises.** Bound to `0.0.0.0` on this machine with several interfaces, replies left from another address and Firefox's ICE failed (Chrome did not mind). The daemon's one socket needs to be bound to the advertised address or to use `IP_PKTINFO`.
6. **Not every sequence number is audio, and not every hole in RTP time is loss.** Chrome and Firefox send padding-only packets on the audio stream (13 to 27 in a quiet 30 s, up to 600 under loss), and telephone-event packets take the place of audio while a key lasts. Both use the audio's sequence numbers. The reorder buffer has to know them, or it will count them as lost or as audio.
7. **Playback reports.** The plan's order (the synchronisation source first, the counter where a browser lacks it) stands for Chrome. For Firefox the source is 2 to 3 packets short at the end of a burst. The counter says how much real audio came out, not where in our stream it was, so under loss it needs `concealedSamples` beside it.
8. **DTMF.** Answer only `telephone-event/48000` when it is offered: Chrome, given both rates, sends on the 8000 one and gets it wrong. Firefox offers only 8000 and its events are as short and as oddly timed.
9. **The stock Pipecat client opens its data channel in band**, so the pre-agreed channel helps waveport's own SDK only. Refusing its video lines is fine.
10. **`deny.toml` needs no new skip**, but four of its reasons name other crates than the ones that would now also be behind them (below).

## 1. The dependency question

**Answer: str0m can be had cleanly. `rtc` 0.21 was not examined, because the condition for trying it did not arise.**

- The stock pure-Rust backend cannot be used. `str0m = { default-features = false, features = ["rust-crypto"] }` brings `str0m-rust-crypto` 0.7.0, which asks `dimpl` for `rust-crypto` and `rcgen`; dimpl's `rcgen` feature switches on `aws-lc-rs`. `cargo tree -i aws-lc-sys` there shows `aws-lc-sys 0.47.0` (cmake, C). Output: `results/tree-stock-rust-crypto.txt`; the crate that shows it: `checks/stock/`.
- Our own provider is `provider/`: the four source files of `str0m-rust-crypto` 0.7.0 (MIT OR Apache-2.0; 514 lines) with one function changed, and `cert.rs` (72 lines), which makes the self-signed P-256 certificate with `p256` and `x509-cert` instead of `rcgen`. It depends on `dimpl` with `rust-crypto` only. `cargo tree -i` finds no `aws-lc-sys`, `aws-lc-rs`, `openssl-sys`, `ring`, `rcgen`, `cc` or `cmake` (`results/tree-own-provider.txt`). Chrome 157 and Firefox 158 both accept the certificate.
- Against the product lockfile, in a scratch copy of the workspace with a crate `crates/adapters/transport/webrtc` that has the provider's dependencies (`results/added-crates.md`, one line per crate):
  - **65 crates are added, no existing entry changes.**
  - **No C or assembly file in any of them.** One build script (`curve25519-dalek`), which only sets `cfg` flags from the target.
  - Licences: MIT, Apache-2.0 or BSD-3-Clause, all in the allow-list.
  - **`cargo deny check` with `deny.toml` unchanged: advisories ok, bans ok, licenses ok, sources ok** on the four targets (`results/deny-product-workspace.txt`). `cargo xtask arch-check`, `tier-check` and `lint-sync` pass on the copy.
  - No new skip is needed, but existing ones gain users their `reason` does not name: `rand@0.9`, `rand_core@0.9` and `getrandom@0.3` are now also behind `sctp-proto` 0.10.8; `syn@2` is now also behind `curve25519-dalek-derive`, `der_derive` and `tracing-attributes`.
  - Highest `rust-version` among the added: 1.89 (str0m, dimpl, `is`, `aes`).
- **The cooldown** lives in `.cargo/config.toml` (`[registry] global-min-publish-age = "7 days"`, `[resolver] incompatible-publish-age = "deny"`), not in `deny.toml`. The tree resolved under it. Youngest on 16 November: `crypto-bigint` 0.7.11 (7 days, the edge), `inout` 0.2.6 (8), str0m, `is` and `str0m-proto` (12), `aes` 0.9.4 and `ctutils` 0.4.8 (13), `sctp-proto` 0.10.8 (14).
- The copied provider does not pass the workspace's lints as it is: 19 errors (12 slicing, 5 `expect` or `unwrap`, 2 indexing) and 20 warnings, no `unsafe` (`results/provider-under-workspace-lints.txt`). It is about 600 lines to rewrite in the house style.
- SCTP (`sctp-proto`) is compiled whether or not a data channel is used; str0m has no feature to leave it out.
- The spike's endpoint also links libopus through the `opus` crate, to make and read the test signal. That is the spike's tooling and not part of this answer.

## 2. The endpoint

`endpoint/` (about 2,100 lines). One UDP socket and one TCP listener on the same port for every peer; each datagram goes to the `Rtc` that says `accepts()`. ICE-lite, RTP mode, Opus with or without RED, `POST /offer` with SDP in and JSON out (and `POST /whip` in the shape of WHIP: 201, `Location`, `ETag`, `application/sdp`; both browsers connect through it the same way). It sends a signal that numbers every 100 ms with a pair of tones, or looped speech, and counts what it receives by RTP time. A page (`web/test.html`) drives one call by itself and posts what it saw; the endpoint adds its side to the same report.

## 3. Against browsers

### 3.1 Setting up a call

On loopback, ms after the page set the answer (`results/setup-table.md`, three runs each):

| | Chrome | Firefox |
|---|---|---|
| First check arrives | 0 | 0 |
| ClientHello arrives | 0 | 21 |
| Our first DTLS flight leaves | 48 to 61 | 21 |
| Endpoint connected | 50 to 62 | 22 to 24 |
| Page says `connected` | 50 to 62 | 27 to 32 |

- Chrome sends its ClientHello (1,463 bytes in two datagrams) right after the first check is answered. str0m as ICE-lite answers it only when the nominating check has arrived, which Chrome sends 48 to 61 ms later. **Read:** str0m sends nothing but STUN until `IceAgentEvent::NominatedSend` has set the address to send to (`str0m/src/lib.rs`).
- Firefox waits for its own nomination before the ClientHello (264 bytes), so nothing is lost there.

With 50 ms added each way (a round trip of 100 ms), ms after the page set the answer:

| | Chrome 1.2, in band | Chrome 1.2, pre-agreed | Chrome 1.3, pre-agreed | Firefox 1.2, in band | Firefox 1.2, pre-agreed | Firefox 1.3, pre-agreed |
|---|---|---|---|---|---|---|
| ClientHello arrives | 151 | 152 | 149 | 273 | 273 | 273 |
| Endpoint connected (may send audio) | 271 | 271 | 275 | 376 | 375 | 376 |
| Page `connected` | 321 | 322 | **226** | 431 | 433 | **330** |
| Endpoint: channel open | 580 | **476** | 379 | 684 | **585** | 482 |
| Page: channel open | 530 | 527 | 430 | 734 | **635** | 531 |
| Page has our first message | 630 | **527** | 431 | 734 | **635** | 531 |
| Endpoint has the page's first message | 580 | 577 | 482 | 785 | 687 | 583 |

- In round trips after the answer: ICE 1 (Firefox 2, it nominates in a round trip of its own), DTLS 1.2 two, SCTP two. DTLS 1.3 lets the browser finish one round trip sooner; the endpoint is connected at the same time as with 1.2.
- Asked for "whichever the ClientHello says" (`DtlsVersion::Auto`), str0m negotiated DTLS 1.3 with both browsers: both offer it today. **Read:** str0m's default is `DtlsVersion::Dtls12`.
- DTLS 1.3 was tried in nine handshakes (five Chrome, four Firefox, one of them the Pipecat client). None failed. That is too few to say anything about the two open handshake bugs.
- SRTP profile: under DTLS 1.2 both browsers got `SRTP_AEAD_AES_256_GCM`. Under DTLS 1.3 Chrome got `SRTP_AES128_CM_HMAC_SHA1_80` and Firefox `SRTP_AEAD_AES_128_GCM`. **Read:** dimpl's DTLS 1.2 server chooses by its own order (AES-256-GCM, AES-128-GCM, SHA1-80); its DTLS 1.3 server takes the first of the client's list that it knows (`dimpl/src/dtls12/server.rs`, `dtls13/server.rs`).
- Neither browser offers `a=sctp-init` (SNAP), with or without str0m's SNAP switched on.
- Firefox retransmits its DTLS flights after 50 ms, so at a round trip of 100 ms every flight comes twice.

**CPU of a handshake, answering side, inside str0m only:**

| | Until connected, µs | Costliest single datagram, µs | How |
|---|---|---|---|
| DTLS 1.2, in memory | p50 516, p99 764 | p50 373, p99 600 | 400 handshakes of str0m against itself, one thread, after a warm-up (`results/bench-in-memory.json`) |
| DTLS 1.3, in memory | p50 339 to 565, p99 569 to 713 | p50 187 to 312, p99 319 to 428 | the same, two series |
| Chrome, DTLS 1.2 | 404 to 1,540 | 247 to 1,184 | nine runs of the live endpoint |
| Chrome, DTLS 1.3 | 716 to 1,415 | 391 to 1,113 | four runs |
| Firefox, DTLS 1.2 | 356 to 834 | 215 to 572 | ten runs |
| Firefox, DTLS 1.3 | 495 to 877 | 260 to 400 | four runs |

- A handshake is 2 to 5 datagrams in and 2 to 3 out. Its costliest step is over 250 µs more often than not, so it belongs off the shard, as the plan has it.
- Making a certificate costs p50 175 to 290 µs. **Read:** str0m makes one per `Rtc` unless `RtcConfig::set_dtls_cert` gives it one.

### 3.2 A packet in and a packet out

Time inside str0m (`handle_input` and the `poll_output` calls after it), SRTP included:

| | p50 µs | p99 µs | Highest seen µs | How |
|---|---|---|---|---|
| RTP in, in memory | 1.0 to 1.1 | 1.7 to 2.0 | p99.9: 4.0 | 300,000 packets in a tight loop, 80 and 170 bytes |
| RTP out, in memory | 1.0 to 1.1 | 1.6 to 2.0 | p99.9: 4.4 | the same |
| RTP in, live, AES-GCM | 8.5 to 13.5 | 22 to 36 | 30 to 72, once 182 | 20 clean runs with browsers, one packet every 20 ms |
| RTP out, live, AES-GCM | 4.0 to 8.5 | 13 to 58 | 16 to 100, once 231 | the same |
| RTP in, live, Chrome on DTLS 1.3 (AES-CM with HMAC-SHA1) | 20 | 64 to 76 | 84 to 107 | two runs |
| RTCP in, live | 11 to 18 | | 25 to 94, once 184 | |
| STUN in, live | 11 to 21 | | | |

The live numbers are ten times the in-memory ones because a packet every 20 ms meets cold caches. Both are far inside 250 µs. When the machine was overloaded (the discarded runs) single packets took up to 0.6 ms.

### 3.3 RED

- Chrome offers `a=rtpmap:63 red/48000/2` with `a=fmtp:63 111/111`. str0m, with RED enabled, answers `m=audio … 63 111`.
- Chrome then sends every packet as RED: 399 of 400 packets were `[opus, 20 ms before][opus, now]` (one redundant block, RFC 2198 header with a timestamp offset of 960), the first packet primary only. Payload 158 to 163 bytes against 77 to 79 without. We could not change the distance from our side.
- **In RTP mode str0m hands the RED payload over as it came**; splitting it is ours (`endpoint/src/red.rs`, 95 lines with tests).
- Chrome decodes RED we send, at distance 1 (161 bytes a packet) and at distances 1 and 2 (243 bytes). Its `packetsDiscarded` then counts every redundant block it did not need (397 of 398 packets without loss): that counter is not a sign of trouble.
- Firefox's offer has no RED line (Opus, G.722, PCMU, PCMA, telephone-event). The answer then has none and both directions are plain Opus.

### 3.4 Loss: FEC, RED and NACK

Speech both ways, 30 s a run, 1,500 packets each way, loss by seed 11, independent per packet. All rows: `results/loss-tables.md`.

**Uplink (the browser sends, the endpoint drops, then counts what it could still have had).** Frames left with nothing, as a share of frames sent:

| | 5% | 10% | 20% | Bytes a packet | When it is there |
|---|---|---|---|---|---|
| Opus alone, Chrome (LBRR in the next packet) | 1.66% | 4.26% | 10.79% | 77 | 20 ms later |
| Opus alone, Firefox | 1.67% | 4.26% | 10.59% | 77 | 20 ms later |
| RED alone, Chrome, ignoring LBRR | 0.27% | 0.93% | 4.46% | 158 | 20 ms later |
| RED and LBRR, Chrome | 0.13% | 0.27% | 2.47% | 158 | 20 to 40 ms later |
| NACK alone, Chrome, waiting up to 60 / 100 / 200 ms | 0.53 / 0 / 0% | 1.33 / 0.27 / 0.07% | 4.60 / 1.40 / 0.20% | 79 | p50 37 to 41 ms, p90 61 to 85 ms |
| NACK alone, Firefox, the same waits | 0.47 / 0.20 / 0% | 1.67 / 0.60 / 0% | 5.67 / 2.20 / 0.27% | 79 | p50 38 to 42 ms, p90 58 to 103 ms |
| RED and NACK, Chrome, the same waits | 0.13 / 0 / 0% | 0.40 / 0.13 / 0.07% | 2.27 / 1.13 / 0.87% | 160 to 163 | |

- **The browser switches its FEC on by itself.** Without loss neither browser sent a single frame with LBRR in 1,500 (Opus in hybrid mode). Once our receiver reports showed loss, 52 to 62% of frames carried LBRR and the encoder moved to SILK. LBRR is not in every frame, which is why it recovers about 60% of single losses and not all.
- **NACK and the browser's FEC exclude each other.** With NACK on, the retransmitted packets count as received, our reports say `fractionLost` 0, and LBRR stays at 0%. With RED and NACK together it was 0 to 21%.
- NACK set-up: the answer carries `a=rtcp-fb:<opus> nack`; in str0m, `PayloadParams::set_fb_nack(true)` and, for the receive stream, `suppress_nack(false)`, because **Read:** str0m suppresses NACK for a stream without RTX, and browsers offer no RTX for audio. Adding the line to the offer in the page made no difference: with it only in the answer, Chrome resent 66 of 66 and Firefox 63 of 63 (p50 41 to 44 ms). An answer that adds a feedback type the offer did not list is outside the offer and answer rules; this leans on the browsers being lenient.
- NACK waits are on loopback. A real path adds its round trip. **Read:** str0m sends NACKs at most every 33 ms (`NACK_MIN_INTERVAL`) and asks at most 5 times.
- With RED and NACK together Chrome did not always resend: 68 of 68 and 148 of 152 came back at 5 and 10%, but 203 of 312 at 20% (two earlier runs of the same setting, since overwritten, gave 309 of 310 and 286 of 308). Not explained.
- **The other direction does not exist in str0m.** With `nack` in the answer Chrome asked for our dropped packets (`nackCount` 1,174 in 30 s at 10% loss) and got none; concealment was the same as without. **Read:** str0m resends only through RTX.

**Downlink (the endpoint sends and drops, the browser counts).** `concealedSamples` as a share of `totalSamplesReceived`:

| | 5% | 10% | 20% | Bytes a packet | Mean jitter-buffer delay |
|---|---|---|---|---|---|
| Chrome, Opus alone | 4.96% | 10.40% | 20.89% | 78 | 18 to 27 ms |
| Chrome, our encoder told the loss (FEC) | 1.27% | 2.97% | 8.04% | 78 | 41 to 62 ms |
| Chrome, RED at distance 1 | 0.37% | 1.10% | 4.85% | 161 | 35 to 57 ms |
| Chrome, RED at distance 1, and FEC | 0.23% | 0.35% | 2.22% | 161 | 78 to 87 ms |
| Chrome, RED at distances 1 and 2 | 0.06% | 0.15% | 1.14% | 243 | 51 to 53 ms |
| Firefox, Opus alone | 5.00% | 10.04% | 20.83% | 78 | 17 to 26 ms |
| Firefox, FEC | 1.21% | 2.78% | 8.10% | 78 | 39 to 58 ms |

- "Told the loss" means the encoder's expected loss was set to the real loss from the first packet. Nothing here measured how fast a controller driven by receiver reports gets there.
- Every repair costs the listener delay: the browser's jitter buffer holds 20 to 60 ms more once FEC or RED is in use. Without loss it held 33 ms (Chrome) and 49 ms (Firefox).
- Without any loss Chrome concealed 0.02% and Firefox 0.10%.

### 3.5 ICE over TCP

A passive TCP candidate on the same port (`a=candidate:… tcp … typ host tcptype passive`), frames as RFC 4571. str0m takes and gives the packets; the listener, the framing and finding the connection are ours.

| "UDP blocked" as | Chrome: page `connected` after the answer | Firefox |
|---|---|---|
| Nothing blocked (UDP) | 50 to 62 ms | 27 to 32 ms |
| Only the TCP candidate in the answer | 176 ms | 120 ms |
| Both candidates, the endpoint ignores every datagram | 313 ms | 86 ms |
| Chrome's policy `disable_non_proxied_udp` | 180 ms | |
| Only TCP, with a round trip of 100 ms | 440 ms (UDP: 322) | 650 ms (UDP: 433) |

Both browsers connect from an active TCP candidate and audio flows both ways (400 packets in 8 s, none lost). Chrome starts its TCP checks about 115 ms after the answer, and about 250 ms after when a UDP candidate is also there. No firewall was used, and loss over TCP (where one lost segment delays everything behind it) was not measured.

### 3.6 Data channel

In the table of 3.1. In short, with a round trip of 100 ms:

- **Pre-agreed (`negotiated: true`, the same id on both sides): the endpoint may send about 100 ms sooner** (one round trip), because it need not wait for the browser's open message.
- Chrome fires `open` at the same moment either way (527 against 530 ms) and its first message reaches us at the same time. Firefox waits for the acknowledgement of an in-band open, so there pre-agreed is one round trip sooner for both sides (635 against 734 ms).
- On loopback the channel is open 3 to 6 ms after DTLS.

### 3.7 ICE restart

The page calls `createOffer({iceRestart: true})` and posts the offer for the same session; the endpoint gives it to the same `Rtc` (`sdp_api().accept_offer`).

- The answer has a new ufrag and password, the same fingerprint and candidate. str0m goes to `Checking` and is `Completed` again 114 ms later with Chrome, 23 ms with Firefox.
- No DTLS handshake follows. Audio went on in both directions throughout: no frame missing on the uplink, `concealedSamples` 0 in the browser.

### 3.8 DTMF (RFC 4733)

**From the browser** (`RTCDTMFSender.insertDTMF("159#", 160, 80)`):

| Browser offers | We answer | What arrives |
|---|---|---|
| Chrome: `110 telephone-event/48000` and `126 telephone-event/8000` | 48000 only | Well formed, on 110: 10 packets a key over 139 ms, durations 960 to 7,680 (160 ms), three with the end bit |
| | both, in either order | On **126**: 4 packets within 21 ms, durations 960, 1,920, 1,920, 1,920 (end on the second) |
| | 8000 only | The same 4 packets |
| Firefox: `101 telephone-event/8000` only | 8000 | The same 4 packets within 21 ms |
| | 48000 only | No telephone-event in the answer; `canInsertDTMF` is false |

- On an 8000 payload type both browsers count the duration in steps of 960, as if the clock were 48,000, and so end the event after two packets. The key and its end still arrive. **Do not trust the duration, and do not expect the event to last as long as the tone.**
- An event's packets all carry the RTP time of its start, on the audio's clock (the 8000 payload type too). One key is one RTP time.
- While a key lasts the browser sends no audio packets: 9 frames of audio are missing per key with Chrome on 48000, 3 per key with Firefox's short form. Sequence numbers run on through the events.

**To the browser** (the endpoint sends events of 160 ms in place of audio, 80 ms apart):

| Sent on | RTP time of the events | Chrome | Firefox |
|---|---|---|---|
| The 48000 payload type | audio's clock | Plays three tones of about 120 ms | not offered |
| The 8000 payload type | audio's clock | Plays three tones (120, 80, 80 ms); 110 ms concealed | Plays three tones (80, 80, 160 ms); 111 ms concealed |
| The 8000 payload type | its own 8000 clock | Plays nothing; 283 ms concealed | Plays nothing, or 40 ms of one tone; 275 ms concealed |

The browser makes the tones itself and plays them to the person; a page cannot read them as events. Tone lengths are from a detector with 40 ms steps.

### 3.9 What the browser reports of playback

Tones, no loss, three runs each. The endpoint logs when it sent each RTP time; the page reads the source every 10 ms and the statistics every 100 ms, and an audio worklet reads the tones back from the received track.

| | Chrome | Firefox |
|---|---|---|
| `getSynchronizationSources()[0].rtpTimestamp`: page time minus when we sent that RTP time | p5 24 to 38, p50 40 to 48, p95 54 to 68 ms | p5 40, p50 50 to 51, p95 73 to 76 ms |
| It moves in steps of | one packet (960) | one packet |
| Against what a Web Audio tap of the track renders | the source is 60 ms ahead of the tap | within one packet of the tap (0 to 20 ms behind) |
| `jitterBufferEmittedCount` over 5.7 s | 5,700 to 5,720 ms of audio | 5,720 to 5,740 ms |
| Mean `jitterBufferDelay` per emitted sample | 35 to 40 ms | 34 to 41 ms |
| `media-playout` statistics | yes: `totalPlayoutDelay / totalSamplesCount` = 37.9 ms | none |
| `audioLevel` of the source | null (the header extension was not negotiated) | |

- **`rtpTimestamp` gives the position in our stream directly**, to one packet, whatever was lost or stretched.
- **`jitterBufferEmittedCount` counts real audio only.** Under loss it equalled `totalSamplesReceived − concealedSamples` to within a frame in both browsers, with and without FEC and RED (10% loss: 1,341 frames emitted of 1,342 received; with FEC 1,452; with RED 1,481). It says how much, not which: to place it in our stream one adds `concealedSamples`. How acceleration and deceleration (`removedSamplesForAcceleration`, `insertedSamplesForDeceleration`, 10 to 40 ms in these runs) enter the count was not separated.
- `totalSamplesReceived` runs on at 48,000 a second whether or not anything arrives: it is a clock.
- Chrome's Web Audio tap is itself about 60 ms late, so "what the tap heard" is not what the loudspeaker played in Chrome. No real loudspeaker was measured (headless, muted).

**When we stop sending** (the last packet is the first 20 ms of a new tone; three runs each, and one with RED in Chrome that looked the same):

| | Chrome | Firefox |
|---|---|---|
| `rtpTimestamp` reaches the last RTP time we sent | 27 to 31 ms after we sent it, and stays | **never: it stays 40 to 60 ms short** |
| `jitterBufferEmittedCount` | stops at exactly the samples we sent (151 packets, 144,960), within 40 ms | the same |
| `concealedSamples` starts to grow | 35 to 45 ms after the last packet | 45 to 75 ms after |
| `silentConcealedSamples` starts | about 500 ms after | about 500 ms after |
| The made-up audio, from where the real audio ended | 10 dB down after about 65 ms, 20 dB after about 125 ms, 40 dB after about 260 ms | the same shape |

So after the last packet leaves us the browser still plays what its jitter buffer holds (35 to 75 ms here, more once loss has made the buffer grow: up to 87 ms mean delay in 3.4), then conceals: about 120 ms that can be heard and silence by 260 ms. Nothing the endpoint sends can empty that buffer.

### 3.10 Frame mode or RTP mode

| 15 s of tones, 10% of the browser's packets dropped | Frames handed over | Time from the packet going in to its frame coming out |
|---|---|---|
| RTP mode (any run) | every packet, with its sequence number and RTP time | the same call: 9 to 13 µs |
| Frame mode, no loss | 403 of 403 | p50 0.02 ms |
| Frame mode, Chrome | 666 (78 dropped) | **p50 181 ms, p99 351 ms, highest 384 ms** |
| Frame mode, Firefox | 666 (78 dropped) | p50 182 ms, p99 352 ms |
| Frame mode with RED, Chrome | 713: str0m recovered 47 itself | p50 54 ms, p99 320 ms |

- **Read:** frame mode's defaults for audio are a reordering window of 15 packets and a timeout of 1 s (`reordering_size_audio`, `reordering_timeout_audio`). 15 packets are 300 ms, which is the delay behind every hole.
- In frame mode str0m unpacks RED itself; in RTP mode it does not.
- In one Firefox frame-mode run the browser sent 1,044 packets in 15 s (bursts of up to 120 a second) and str0m handed over 966 frames, 300 more than audio packets arrived. What the extra ones were was not checked; most likely the padding packets below.

### 3.11 Other things seen

- **Padding-only packets on the audio stream.** With transport-wide congestion control negotiated (str0m answers it) both browsers send RTP packets with the padding bit and no payload: 13 to 27 in 30 s without loss, 166 to 607 at 10% loss. They use the audio's sequence numbers, and in RTP mode str0m hands them over as packets.
- **Sequence numbers are one series** across audio, padding and telephone-events (438 + 3 + 16 = 457 by sequence in one run).
- **No DTX**: with nothing asked for, both browsers sent 50 packets a second throughout, 1,504 and 1,530 frames in 30 s with no hole in RTP time.
- When the machine was overloaded Chrome skipped frames by itself (up to 2% in the discarded runs): RTP time jumped although nothing was lost.
- Chrome's audio was Opus mostly in CELT mode for the tones and hybrid for speech; frames were 20 ms throughout.
- Clock drift between browser and endpoint was not measurable here (one machine, 30 s).

## 4. The stock Pipecat client

`pipecat/entry.js` uses `PipecatClient` and `SmallWebRTCTransport` unchanged. The stub is `endpoint/src/pipecat.rs`. Exact messages: `results/pipecat-messages.md`. The HTTP bodies and the SDP are in the reports of the runs, which are not in the repository (see the end).

**Result: `connect()` resolves, the state is `ready`, and the page hears our tones**, in Chrome and Firefox through `connect({webrtcUrl: '/api/offer'})`, and in Chrome also through `startBotAndConnect({endpoint: '/start'})` and on DTLS 1.3. From `connecting` to `onBotReady`: 70 to 102 ms on loopback.

What the client sends, and what was enough in reply:

| Step | Client | Stub |
|---|---|---|
| Optional | `POST /start` `{"transport": "webrtc"}` | `{"sessionId": "…", "iceConfig": {"iceServers": []}}`; the client then uses `/sessions/{sessionId}/api/offer` |
| Offer | `POST /api/offer` `{"sdp", "type": "offer", "pc_id": null, "restart_pc": false}` | `{"sdp", "type": "answer", "pc_id": "SmallWebRTCConnection#<n>"}` |
| The offer's SDP | four lines in one bundle: audio `sendrecv`, video `sendrecv`, video `sendonly`, application | audio accepted (Opus, `a=ice-lite`, one host candidate), **both video lines refused with port 0**, application accepted, bundle `0 3` |
| Trickle | `PATCH` the same path `{"pc_id", "candidates": [{"candidate", "sdp_mid", "sdp_mline_index"}]}`, once | `{"status": "success"}`; the candidates are not needed by an ICE-lite endpoint |
| Data channel | opens `chat` **in band** (id 0, ordered) | accepted by str0m |
| On open | three `{"type": "signalling", "message": {"type": "trackStatus", "receiver_index": 0\|1\|2, "enabled": true\|false\|false}}` | ignored |
| | `{"label": "rtvi-ai", "type": "client-ready", "id": "…", "data": {"version": "2.3.0", "about": {…}}}` | `{"label": "rtvi-ai", "type": "bot-ready", "id": <the same>, "data": {"version": "2.3.0", "about": {…}}}` |
| Every second | the text `ping: <ms since 1970>` | not answered; the client stayed `ready` for the 8 s watched |

Callbacks that fired from what the stub then sent: `onBotReady`, `onBotStartedSpeaking`, `onBotOutput`, `onUserStartedSpeaking`, `onUserTranscript`, `onServerMessage`; before them `onTrackStarted` for our audio, `onConnected`, `onBotConnected` (and `onBotStarted` on the `/start` path).

Not tried: a `bot-ready` with another protocol version, renegotiation and `restart_pc`, send-text, DTMF, interruption, disconnect, staying silent on pings for longer than 8 s, and the client's camera path.

## Not answered

| What | Why |
|---|---|
| Anything about Safari | No Safari on Linux. Open for it: whether it offers RED, DTLS 1.3 and `getSynchronizationSources`, its DTMF, its TCP candidates |
| `rtc` 0.21 | str0m passed, so the fallback was not built |
| A real network | Everything ran on one machine. No jitter, no bursts of loss, no NAT, no firewall, no TURN |
| Loss over TCP | Not measured |
| The two open DTLS 1.3 bugs of dimpl | Nine handshakes without a failure say nothing about them |
| Why Chrome with RED and NACK sometimes resends a third less | Seen, not explained |
| What a loudspeaker plays | Headless and muted; in Chrome the Web Audio tap is itself about 60 ms late |
| How time-stretching enters `jitterBufferEmittedCount` | Not separated |
| Clock drift, DTX | No drift measurable on one machine; neither browser used DTX and it was not asked for |
| A controller that sets the encoder's expected loss from receiver reports | The encoder was told the true loss from the start |
| Mobile browsers | None here |

## How the numbers were made

- `./run.sh <browser> '<query>'` starts a headless browser on `web/test.html`; the page runs the call and posts a report, the endpoint adds its side. One file per run in `results/` (packed; `./analyze.py` reads them), which is not in the repository. The tables: `./table-loss.py`, `./table-setup.py`. The batches: `./batch-loss.sh`, `./batch-misc.sh`. The in-memory numbers: `spike-endpoint bench`.
- Neither browser opened a microphone; the page makes what it sends with Web Audio.
- **Runs that were thrown away.** The endpoint is one thread that sends a frame every 20 ms. When the machine was busy with other work that thread fell behind, for seconds at a time and for all sessions at once, and every per-packet cost in the run rose about three-fold. A run counts as disturbed when the endpoint sent more than 8 frames over 45 ms after the one before, or the browser skipped more than 1% of its own frames. The loss batch was run several times and the disturbed runs repeated with fewer browsers at once until none was left; the tables hold only clean runs.
- The page's clock and the endpoint's agree to within 2 ms (checked through the data channel in both browsers).
- The reports of the single runs hold the SDP of every run, and with it the private network addresses of the machine they were made on. So they are left out of the repository, and the tables made from them are in it. The batches make them again.
