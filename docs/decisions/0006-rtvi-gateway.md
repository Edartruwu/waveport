# Decision 0006: how much of RTVI the gateway speaks

**Date:** 16 November 2026. **Status:** accepted. **Phase:** 2, before milestone 1.

The [architecture](../architecture.md) left one question to phase 2: how much of RTVI the client gateway implements. RTVI is the message protocol of Pipecat's client SDKs. This record answers the question, message by message, and says which of Pipecat's HTTP routes the daemon imitates so that an application written for Pipecat's JavaScript client talks to waveport without a change to its code.

What is here was read on 16 November 2026 from the RTVI standard as Pipecat publishes it (protocol 2.3.0), from the source of Pipecat 1.16.0 and of `@pipecat-ai/small-webrtc-transport` 1.11.2, and held against the stock client in the WebRTC spike ([decision 0005](0005-webrtc-endpoint.md)).

## Decisions

| Question | Decision | Why |
|---|---|---|
| What the gateway is for | An application built on `@pipecat-ai/client-js` with its SmallWebRTC transport connects to `waveportd` by changing an address | It is the client most voice applications outside LiveKit are written against, and its protocol says it works "with any RTVI-compatible server" |
| Which protocol version | 2.3.0, the newest there is. A client of major version 2 is served in full | The standard's own rule: the same major version is compatible |
| A client of version 1 | Served in the format of version 2, after the warning the standard asks for (`error-response`, and the connection goes on) | Version 1 has a second format for `bot-output`. One client version is pinned and tested; a second format nobody tests would be a claim without evidence |
| What waveport's own protocol is | Client protocol v1 stays the protocol of record. RTVI is a mapping from it, a pure function in the daemon | RTVI cannot resume a connection, has no sequence numbers and no playback reports. Those are what v1 is for |
| A message the gateway does not act on | Answered with `error-response` carrying the message's `id`, as the standard says for a type that is not known | Nothing is dropped silently |
| How it is kept true | The stock client runs against the daemon in a headless browser in every test run, at a pinned version. Once a week the same test runs against the newest published client and says when it drifts | Pipecat releases a minor version about every eleven days and the protocol had four versions in five months of 2026 |
| A second way in | `sdks/pipecat-transport`, a transport for the Pipecat client over waveport's own signalling | It gives a Pipecat application what SmallWebRTC cannot: a connection that resumes, a fallback when UDP is blocked, and reports of what was played |

## Messages from the client

| Message | What the gateway does |
|---|---|
| `client-ready` | Answers `bot-ready` with version 2.3.0, `about` naming the library and its version, and `capabilities` of audio in and audio out, with video, screen and metrics false |
| `disconnect-bot` | Ends the session, as `bye` does in v1 |
| `send-text` | The text is a turn, as typed text is in v1. With `run_immediately` false or `audio_response` false it is refused: the kernel has no turn that is not answered, and a voice session has no reply that is not spoken |
| `dtmf` | Each of its `buttons` is a key, in order |
| `send-file` | Refused. Sessions take no files in this phase |
| `llm-function-call-result` | Refused. Tools run on the server; tools on the client come with the agent layer of phase 4 |
| `client-message` | Refused. There is nothing in the daemon yet for an application to answer with |
| `ui-snapshot`, `ui-event`, `ui-cancel-job-group` | Refused. The UI Agent Protocol is not built |

## Messages to the client

| Message | Sent when |
|---|---|
| `bot-ready` | In answer to `client-ready` |
| `error` | The session has failed; `fatal` is true when it is closing |
| `user-started-speaking`, `user-stopped-speaking` | The kernel says a participant's speech began or stopped |
| `bot-started-speaking`, `bot-stopped-speaking` | The far end starts to hear a response, and when it has heard the last of it or it was cut |
| `bot-interrupted` | A response was cut off by the user |
| `user-input` | An interim transcript (`final` false), a committed turn (`final` true), typed text (`input_type` `chat`), and speech over the agent that was judged a noise of agreement (`input_type` `backchannel`). `perception` is never set |
| `bot-output` | For each stretch of text released to be spoken: once as `new`, then `in-progress` and `completed` with `spoken_progress`, which is taken from what the far end played and not from a clock. In a text session `will_be_spoken` is false. `delivery` and the expression output types are never set |
| `bot-llm-started`, `bot-llm-stopped` | A response is asked of the model, and when the model has finished |
| `bot-tts-started`, `bot-tts-stopped` | The first text of a response goes to synthesis, and when its last audio has arrived |
| `llm-function-call-started`, `-in-progress`, `-stopped` | A tool call is proposed, sent and ended. The tool's name and the call's id are given. Its arguments and its result are not, unless the agent's definition allows them to leave the server |
| `error-response` | A message from the client was refused, or its version is not served in full |
| `user-transcription`, `bot-llm-text`, `bot-tts-text` | These three are deprecated and Pipecat still sends them. They are sent while the pinned Pipecat sends them by default, so that the callbacks an existing application listens to still fire, and are dropped when Pipecat drops them |

Never sent: `system-log`, `vad-user-started-speaking` and `vad-user-stopped-speaking`, `user-audio-level` and `bot-audio-level`, `user-mute-started` and `user-mute-stopped`, `metrics`, `server-message` and `server-response`, `bot-llm-search-response`, `bot-llm-marker`, `stt-raw-text`, `llm-raw-text`, `tts-raw-text`, `user-llm-text`, `ui-command`, `ui-job-group`, and the two that were deprecated before version 2 (`bot-transcription`, `llm-function-call`). Each is either off by default in Pipecat, or about a part waveport does not have, or raw text of a service that a client has no need of.

## Pipecat's routes

SmallWebRTC is signalled over HTTP. The daemon serves the routes below beside its own (`POST /v1/webrtc`, decision 0005). They are Pipecat's as read from its source, and are held by the test with the stock client.

| Route | Request | Answer |
|---|---|---|
| `POST /start` | `{transport: "webrtc", enableDefaultIceServers?, body?}` | `{sessionId, iceConfig?}`. A session is made for the agent the address names |
| `POST /api/offer`, `POST /sessions/{sessionId}/api/offer` | `{sdp, type, pc_id, restart_pc, requestData?}` | `{sdp, type, pc_id}`. A `pc_id` that is known renegotiates the same connection, which is also how this client restarts ICE. `restart_pc` true closes the connection and makes a new one |
| `PATCH` on the same path | `{pc_id, candidates: [{candidate, sdp_mid, sdp_mline_index}]}` | `{"status": "success"}`, or 404 for a `pc_id` that is not known |

- **The client's offer has three media lines**, in this order: audio both ways, video both ways, and a second video for a shared screen. They are matched by their place in the offer. The answer takes the audio and refuses both video lines.
- **The data channel is the client's**, with the label `chat`, in order. RTVI envelopes travel on it. So do two things that are not RTVI: a text `ping: <milliseconds>` once a second, which the daemon takes as a sign of life, and messages of type `signalling` (the client's `trackStatus`; the server's `renegotiate` and `peerLeft`), of which the daemon reads the first and sends none.
- **Playback.** This client does not say what it played. A session it calls has `PlaybackEstimated` in its plan, as any far end that does not report has (section 6 of the architecture). A Pipecat application that wants what was heard to be exact uses `sdks/pipecat-transport`.
- **There is no route that ends a call.** The session ends with `disconnect-bot`, or when the connection is gone for longer than a line that dropped is waited for ([decision 0004](0004-session-shapes.md)).

## What the spike held

The stock client (`@pipecat-ai/client-js` 1.16.0 with `@pipecat-ai/small-webrtc-transport` 1.11.2), unchanged, against a stub of these routes on the spike's endpoint ([`spikes/webrtc`](../../spikes/webrtc/FINDINGS.md)):

- **It reaches `ready` and plays our audio** in Chrome 157 and Firefox 158, through `/api/offer` and through `/start`, against an answer that is ICE-lite, takes the audio, refuses both video lines with port 0 and accepts the data channel. From `connecting` to `onBotReady` took 70 to 102 ms on loopback.
- **It opens its data channel in band**, so a channel agreed in advance (decision 0005) is of no use to it.
- **Its trickled candidates are not needed** by an endpoint that is ICE-lite. The `PATCH` is answered and nothing is done with it.
- **Its pings were not answered** and it stayed `ready` for the 8 s that were watched. Longer was not tried.
- **Not tried:** a `bot-ready` of another protocol version, renegotiation and `restart_pc`, `send-text`, DTMF, an interruption, `disconnect-bot`, and the client's camera. Milestone 19 tries each.

## What is not claimed

- **Pipecat's SDKs for phones** (Swift, Kotlin, React Native) use the same signalling. They are not tested here, and nothing is said about them until they are.
- **Other Pipecat transports** (Daily, LiveKit, its WebSocket transport, Media over QUIC) are other protocols. The gateway speaks SmallWebRTC's.
- **A Pipecat server's Python API.** The gateway is for clients. An agent written for Pipecat's pipeline is not run by waveport.
