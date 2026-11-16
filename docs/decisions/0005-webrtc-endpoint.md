# Decision 0005: the WebRTC endpoint

**Date:** 16 November 2026. **Status:** accepted for Chrome and Firefox, which were measured. No Safari was at hand, and nothing here says anything about it: it is measured before milestone 18, and this record is corrected by what that finds. **Phase:** 2, before milestone 16.

Phase 2 of the [roadmap](../../ROADMAP.md) has "a WebRTC direct endpoint on `str0m` with HTTP signalling and an adaptive jitter buffer". The [architecture](../architecture.md) names `rtc` as the library to fall back on. This record fixes what the transport of milestone 16 is made of. It rests on a spike, [`spikes/webrtc`](../../spikes/webrtc/FINDINGS.md), which built a small endpoint on str0m 0.25.0 and held it against Chrome 157 and Firefox 158, without their heads, and against the stock Pipecat client. Browser and endpoint ran on one machine, so there was no network between them: loss was made by a seeded generator, a packet at a time, and delay was added where a figure needed it. Real networks lose in bursts, which is harder on redundancy than what was measured.

## Decisions

| Question | Decision | Why |
|---|---|---|
| The library | str0m, with `default-features = false` and a crypto provider of our own | The stock pure-Rust provider asks its DTLS library for `rcgen`, which switches on `aws-lc-rs`, and with it C and cmake. With our own provider there is no aws-lc, OpenSSL, ring or C at all. 65 crates join the lockfile, none changes, and `cargo deny check` passes with `deny.toml` as it is. `rtc` was not looked at, because the reason to did not arise |
| The crypto provider | A module of the transport crate: str0m's pure-Rust provider written again in this repository's style, with the certificate made by `p256` and `x509-cert` | The provider as copied is 514 lines and fails the workspace's lints in 19 places, none of them `unsafe`. Both browsers accept the certificate |
| The certificate | One for a daemon, given to every connection | str0m makes one for every connection otherwise, which costs 175 to 290 µs each time |
| How str0m hands audio over | RTP mode: every packet with its sequence number and its RTP time | In frame mode str0m reorders by itself, with a window of 15 packets. At 10% loss that held the usual frame back 181 ms. `waveport-media::ingress` stays the only reorder buffer |
| ICE | ICE-lite, and one UDP socket for a daemon that sends from the address it advertises | Bound to every address of a machine that has several, the socket answered from another one than it was asked at, and Firefox's ICE failed. Chrome did not mind |
| Networks that block UDP | A passive TCP candidate on the same port, with the framing of RFC 4571, which is ours to write | Both browsers connect through it and audio flows both ways. On loopback it costs 60 to 260 ms more to connect |
| DTLS | 1.2, which is str0m's default. 1.3 is a setting that is off | 1.3 worked in all nine handshakes tried and connects a browser one round trip sooner. But its server takes the first SRTP profile the client lists, which with Chrome is AES-CM with HMAC-SHA1: about twice the cost for each packet, and weaker. Nine handshakes say nothing about the two open bugs |
| Where the handshake runs | Off the shard | Its costliest datagram takes 0.19 to 1.2 ms, and a shard gives a session 250 µs at a time |
| Redundant audio (RED, RFC 2198) | Listed first in the answer when the browser offers it, and sent at a distance of one frame. Unpacking it is ours in RTP mode | Chrome then sends every packet with the frame before it. **Firefox does not offer RED**, so with Firefox there is none in either direction |
| Opus in-band FEC | On in both directions. Our encoder is told the loss that the browser's receiver reports show. The browser switches its own on by itself when our reports show loss | To the browser it brings what is concealed from 5.0, 10.4 and 20.9% down to 1.3, 3.0 and 8.0% at 5, 10 and 20% loss. From the browser it recovers about 60% of single losses |
| Retransmission of the browser's audio (NACK) | Asked for in the answer when the browser offers no RED, which today is Firefox. Not together with RED | While NACK is on, our reports show no loss and the browser never switches its FEC on: the two exclude each other. With Chrome, RED and FEC leave 0.27% of frames missing at 10% loss within 40 ms, and NACK does better only with a wait of 100 ms or more. With Firefox there is no RED, and NACK leaves 0.6% where FEC alone leaves 4.3% |
| Retransmission of our audio | None | str0m resends only through RTX, which no browser offers for audio. Chrome asked 1,174 times in 30 s and got nothing |
| The data channel | Agreed in advance (`negotiated: true`) for waveport's own SDK. One the client opens in band is accepted too | Agreed in advance, the endpoint may send one round trip sooner, and with Firefox both sides may. The stock Pipecat client opens its channel in band |
| Signalling | In the shape of WHIP: an offer is posted and answered with 201, `Location` and `ETag`. A second offer for the same session restarts ICE | Both browsers connect through it. A restart needed no new DTLS handshake and lost no packet |
| DTMF | The answer has `telephone-event/48000` alone when it is offered, and the 8000 one only when there is no other. A key is the RTP time its packets share, and its end bit. Its duration is not read. Keys we send are timed on the audio's clock | Given both, Chrome sends on the 8000 one and counts its durations as if the clock were 48,000, so the event ends after two packets. Firefox offers only 8000 and does the same. Events timed on a clock of their own are played by neither browser |
| Packets that are not audio | The transport takes padding and telephone-events out before the reorder buffer, and tells it which sequence numbers they had | Both browsers send packets of padding alone on the audio stream, 13 to 27 in a quiet 30 s and up to 600 under loss. A key takes the place of audio while it lasts. All of them use the audio's sequence numbers, and a buffer that does not know counts them as lost |
| What the browser played | Where our stream is: `getSynchronizationSources()` and its `rtpTimestamp`. Where that is missing or stops short: `jitterBufferEmittedCount` with `concealedSamples`. Else the server works it out from the pace | The first follows what we sent to one packet in Chrome. In Firefox it stops 40 to 60 ms short when we stop. The counter is exact in both, but it says how much real audio came out and not which |
| Stopping | The SDK mutes what the browser still holds when it is told to clear | Nothing the endpoint sends empties the browser's buffer. After our last packet 35 to 75 ms of real audio still play, more once loss has made the buffer grow, and then about 120 ms of made-up audio that can be heard |

## The adaptive jitter buffer

The roadmap asks for one. It is the reorder buffer the inbound path has, with three things added, and not a second buffer.

The listener of a call's audio is a machine. So the buffer holds nothing back to smooth the pace, waits for a packet only when a later one has overtaken it, and lets that wait grow and shrink with what packets have lately needed (section 5 of the architecture). That is the adapting. What a browser on a real network adds:

1. **Redundancy.** A frame that arrives inside a later packet (RED), or as the low-rate copy inside the next frame (FEC), fills the place of the packet that was lost. The buffer is given both.
2. **Sequence numbers that are not audio.** See the table.
3. **A wait for retransmission.** For a call in which NACK was agreed, the longest wait is set to what a retransmission takes: half of them were back within 40 ms plus a round trip, nine in ten within 60 to 100 ms plus one.

Two things the roadmap's words might be read to ask for are not built:

- **Discontinuous transmission.** Neither browser used it unasked: both sent 50 packets a second through silence. The endpoint does not ask for it.
- **Clock drift.** A buffer that plays to a person drifts against the sender's clock. This one plays to nobody: audio is handed to speech recognition as it arrives. In the other direction it is the browser's buffer that deals with our clock. One thing does belong here, and the buffer has to know it: a hole in RTP time is not always loss. Chrome skipped frames by itself when the machine was overloaded.

## What it costs

| | Measured | How |
|---|---|---|
| A handshake, in CPU on the answering side | 0.34 to 1.5 ms | 27 handshakes with browsers, and 400 of str0m against itself |
| A packet in, with SRTP | 9 to 13 µs at the median, 22 to 36 µs for 99 in a hundred | 20 clean runs with browsers, a packet every 20 ms, which meets cold caches. In a tight loop it is 1 µs |
| A packet out | 4 to 8.5 µs at the median, 13 to 58 µs for 99 in a hundred | The same |
| Connecting, on loopback | 50 to 62 ms with Chrome, 27 to 32 ms with Firefox | Three runs each |
| Connecting, with a round trip of 100 ms | 322 ms with Chrome, 433 ms with Firefox, until the page says it is connected | One run each |
| A packet with RED | 158 to 163 bytes of payload, against 77 to 79 without | |

A packet in and one out, fifty times a second, are about a millisecond of a second at the live figures: a tenth of a percent of a core for a call, before the audio path. Milestone 40 measures it as a session's share of its thread.

About 50 ms of every Chrome call's set-up on a fast network are a wait: Chrome sends its first DTLS message right after its first check, and str0m as ICE-lite answers it only when the check that nominates has come.

## What this changes in the plan for phase 2

1. **"Redundant audio both ways" holds for Chrome.** With Firefox the repairs are FEC both ways and NACK from the browser.
2. **Audio NACK needs no change to the offer.** The line in the answer was enough for both browsers. An answer that adds a kind of feedback the offer did not list is outside the rules of offer and answer, so this leans on browsers being lenient, and the browser test of every release holds it.
3. **`deny.toml` needs no new skip.** Four of its reasons name other crates than the ones now also behind them: `rand@0.9`, `rand_core@0.9` and `getrandom@0.3` are also behind `sctp-proto`, and `syn@2` behind three more derive crates.
4. **The daemon's one socket** is bound to the address it advertises, or sends from the address a packet came to.
5. **The pre-agreed data channel** helps waveport's own SDK. The stock Pipecat client opens its own.
6. **Playback reports** need the counter beside the source for Firefox, and `concealedSamples` beside the counter under loss.
7. **DTLS 1.3 stays a setting that is off,** for a second reason beside the open bugs: the SRTP profile it leaves Chrome with.

## What was not answered

| What | Why | When |
|---|---|---|
| Anything about Safari | None on Linux | Before milestone 18: whether it offers RED and DTLS 1.3, what it reports of playback, its DTMF and its TCP candidates |
| A real network | Everything ran on one machine: no jitter, no bursts of loss, no NAT, no firewall, no TURN | The browser tests of milestone 18, and the measurement of milestone 39 |
| Loss over TCP | Not measured. One lost segment delays everything behind it | Milestone 18 |
| How fast the encoder learns the loss from receiver reports | The encoder was told the true loss from the first packet | Milestone 16, which builds it |
| Why Chrome with RED and NACK together sometimes resent a third less | Seen, not explained | Not followed up: the two are not used together |
| What a loudspeaker plays | The browsers had no heads and no sound. In Chrome a tap of the audio in the page is itself about 60 ms late | Milestone 39 |
| Mobile browsers | None here | Not in this phase |

## What it cost

An afternoon of one machine. The endpoint, the page that drives a call, the scripts and the tables are in `spikes/webrtc/`. The reports of the single runs hold the SDP of every call, and with it the addresses of the machine they were made on, so they are not in the repository: the tables made from them are.
