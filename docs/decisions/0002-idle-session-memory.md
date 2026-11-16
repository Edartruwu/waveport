# Decision 0002: what a session that listens holds

**Date:** 16 November 2026. **Status:** accepted. **Phase:** 1, after milestone 11.

The [architecture](../architecture.md) asked that an idle connected voice session hold 300 KB, and 99 in a hundred 600 KB. The performance gates of milestone 11 measured 528 KB on a line that carries Opus and 587 KB on one that carries plain samples at 48 kHz. This record says where that memory is, what was decided about the target, and what is left for later.

A kilobyte here is 1,024 bytes, as in `benches/tests/memory.rs`, which measures. What libopus keeps for itself is not counted.

## Decisions

| Question | Decision | Why |
|---|---|---|
| The target for the usual session that listens | 480 KB, restated from 300 KB. The 600 KB for 99 in a hundred stand | The two buffers a voice session reserves when it is set up, 8 s to look back on and 3 s of speech to play, are 391 KB by themselves. The 300 KB were written as though a session that listens held neither |
| What waits for speech recognition | It is the end of the 8 s, and no longer a buffer of its own | The two held the same audio. A session holds 62.5 KB less, and a frame is copied once less |
| A shorter look-back | Not taken | It does not meet the target and it costs the end-of-turn model its input. See below |
| What the gate holds | The line that carries Opus is held to the target for the usual session, the line at 48 kHz to the one for 99 in a hundred | Until now no test compared anything with the target for the usual session |
| Holding 90 to 150 KB | Later, when sessions can be parked | Nothing needs it yet, and it changes what a session does at the start of a turn. See below |

## Where the memory is

| What | Where | Opus line | 48 kHz line |
|---|---|---|---|
| 8 s to look back on, at 16 kHz | `waveport-media`, `ingress` | 250.0 KB | 250.0 KB |
| 3 s of speech to play, at 24 kHz | `waveport-media`, `playout` | 140.6 KB | 140.6 KB |
| 2 s that wait for speech recognition, at 16 kHz | `waveport-media`, `ingress` | 62.5 KB | 62.5 KB |
| Ten packets held while one before them is missing | `waveport-media`, `ingress` | 12.5 KB | 56.3 KB |
| The rest: room for a frame or a few of them at each step of the path, the rings to the thread that judges frames, the kernel's text and tables, the session's own task | | about 63 KB | about 78 KB |
| **Measured before** | | **528 KB** | **587 KB** |
| **Measured now, without the 2 s** | | **466 KB** | **524 KB** |

A session that speaks holds about 11 KB more than one that listens, on either line. So the two rows of the architecture's table were never far apart in the code: every buffer is reserved when the session is set up, which is what keeps the audio path from asking for memory while a call runs.

The 2 s were written in the same place as the 8 s, with the same samples, and emptied with them when the stream broke off. Speech recognition took its audio out of the 2 s, and that was the only difference: what waited there was always the newest part of what the 8 s held. The inbound path now keeps the 8 s and a count of how much of their end has not been taken. The bound is as it was: no more than 2 s wait, the oldest is lost first, and what is lost is counted.

## Why not a shorter look-back

The other way to the old target was to keep less than 8 s. It was not taken, for three reasons.

- **It does not get there.** With 4 s a session would have held 403 KB on a line that carries Opus and 461 KB at 48 kHz. On the 48 kHz line no look-back at all would still have left it over 300 KB.
- **The end-of-turn model reads 8 s.** Smart Turn v3.2 takes a fixed 8 s, and a turn that is shorter has silence put before it. With a look-back of 4 s, every turn longer than that would be judged from its last 4 s alone, and those are the turns in which it is hardest to tell a pause from an end.
- **A recogniser that is opened again is caught up from the same 8 s** (section 9 of the architecture): what was heard since its last final transcript.

Nor is memory what limits a node. At the planning figure of 60 active sessions to a core, about 900 on a node of 16 cores, the sessions hold under half a gigabyte between them.

## What is left for later: a session that is parked

The architecture asks 64 KB of a session that is parked or has no voice. The kernel can be parked already (`Kernel::park`); the runtime has no parked session yet. When it has, a session that listens can give back most of what it holds between turns, in this way:

- **The 8 s are borrowed for a turn.** Between turns a session keeps about half a second to look back on. When speech begins it borrows 8 s from a pool that belongs to its shard, puts the half second at their start, and gives them back when the turn is over. The end-of-turn model still gets up to 8 s of every turn.
- **The 3 s of speech are borrowed for an answer**, when its first audio comes, and given back when the last of it has been played or stopped.

A session that listens then holds about 90 KB on a line that carries Opus and 150 KB at 48 kHz.

What it costs, and has to be settled when it is built:

- **A turn can begin with a call to the allocator**, when more turns are open on a shard than its pool has buffers for. The pool counts what it had to make (`waveport-media`, `pool`, which the runtime does not use yet). The gate on allocations is on every frame and stays as it is.
- **A recogniser that is opened again between turns is given the half second**, and not everything that was heard since its last final transcript. Between turns that is silence, or speech too short to count; section 9 of the architecture says "from the same 8 s" and changes with it.
- **The half second has to be longer than the start of a turn is dated back**: the 64 ms speech has to last before it counts, and the frames that wait for the voice-activity model, a little over 100 ms.
- **How many buffers a pool holds** for the turns and the answers of a shard at once, which wants a measurement with many sessions.

## What is not answered

- **What libopus holds.** Its decoder and encoder keep their state outside the Rust heap, where the gate does not count.
- **The packets held on a line at 48 kHz.** Room is kept for ten packets of the longest kind a line may send, 60 ms, which is 56 KB there. Ten packets of 20 ms would be 19 KB. It is left as it is.
