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

## What milestone 19 found

The gateway is built: the messages as types in `waveport-protocol` (`rtvi`), the mapping in the daemon (`apps/waveportd/src/rtvi.rs`), Pipecat's routes beside the daemon's own (`apps/waveportd/src/pipecat.rs`), and in the WebRTC transport a far end that speaks another protocol than the client protocol on a data channel it opens. The stock client, unchanged, is on whole sessions with the real daemon in a headless Chrome 157 in every test run ([`sdks/pipecat-stock`](../../sdks/pipecat-stock/README.md)): `/api/offer` and `/start`, a spoken call, `send-text`, `dtmf`, `disconnect-bot`, and a second call in the place of the first. The table says where what was built is not what was decided above, and what was found on the way.

| Subject | What was found | What was done |
|---|---|---|
| The end of a session | **A client that is not told that its bot left takes the end of its call for a network that failed.** It makes another call in the place of the one that ended, up to three times, and on a daemon where anyone may ask, each of those would be answered with a new session. The record above has the daemon send none of the transport's `signalling` messages | The daemon sends one: `peerLeft`, when the session says goodbye, whatever the reason, and in answer to `disconnect-bot`, since a session tells a far end that left nothing more. The client then ends its call and makes no other. A call that ends because the client itself made another in its place is ended without it |
| A call in the place of another (`restart_pc`) | The client makes a whole new call and names the one before. The transport for a session takes one far end in the place of another already | The new call is answered for the session of the call it names, and takes its place when its far end has come. A far end that comes back is told what happens from then on and nothing twice: RTVI has no way to say where a client was, so what happened while it was away is not told. A call that is over still stands for its session for as long as the session waits for its line, so a page that was closed and opened again within that time is on its session |
| An offer that names a call nobody knows | Pipecat makes a new connection for it | So does the daemon: it is an offer like a first one, for whoever may make one |
| Who may ask | The record did not say. The stock client sends the same headers with every request, and a status of the 400s ends its call for good, where one of the 500s is tried again | A client token as `Authorization: Bearer` is for its session, and is used up when the far end has come on its call, as on a line of the client protocol. A control key, or anyone where the daemon has none, has a session made of the agent the address names (`?agent=`), or of the one agent there is. The name of a call is good only with what its offer was made with. A daemon that has too many calls on their way, or is stopping, answers 503. The stock client says a token as the `headers` of what it asks, which one step of the browser test holds |
| The `sessionId` of `/start` | It becomes part of the address of the offer, and an address is written down by whatever stands between | It is neither the session nor a token: 128 random bits that stand for the session for a minute, and only together with what `/start` was asked with |
| `iceConfig` | A client that is given an empty list of servers has none, and one that is given nothing keeps the ones it was set up with | It is left out where the daemon was told of no servers |
| Tool calls | The record gives arguments and results where the agent's definition allows them to leave the server. No definition says so yet | They are never given. The name of the tool and the number of the call are |
| `bot-llm-stopped` | A session does not tell its clients when the model has finished | It is sent when the last audio of the response has been made, which is later |
| `bot-llm-text` | The agent's greeting is text no model wrote | It is sent for text of a response the model was asked for, and not for a greeting. `bot-output` is sent for both |
| `bot-tts-text` and `spoken_progress` | What was heard is told for a response and not for a stretch of it, and the end of a short response is told only by the fact that it ended | What was heard is laid over the stretches by counting characters, and what was heard since the client was last told is sent when the response ends. A response that was heard to its end has every stretch `completed`; one that was cut off has them as far as it was heard |
| `bot-output` in a text session | The record says `will_be_spoken` is false there | No text session comes this way: an offer for an agent that only writes is refused (422) |
| The client's ping | The record takes it as a sign of life | It is passed over. Whether a far end is there is known from its packets, as for every call (decision 0005) |
| A client of version 1 | As decided: warned with `error-response`, then served | Held by a test of the mapping and by no client: none of version 1 was run |
| How the stock client opens devices | **Told nothing, its transport fetches a program from its maker's servers when it starts** (`c.daily.co`, and two other names when that fails), and reports to an error service when it cannot. With no name that leads anywhere it never makes an offer. The spike ran it with the network open and did not see this | The test gives the transport the other of the two ways it comes with (`WavMediaManager`), which asks nobody. From the offer on the two are the same. An application that keeps the default needs that server whatever it calls: it is no part of what the daemon answers |
| How often the newest client is tried | Decided: once a week | Every night, with the other scheduled runs, which have one clock |
| An interruption | The stock client interrupts by speaking or by writing, and the stand-in daemon ends its call with its one answer | Held in the daemon's tests, where a client of the transport's writes while it is answered and is told `bot-interrupted`. Not tried in a browser |

Still not known: the stock client in Firefox and Safari against the daemon (the spike had it ready in Firefox against a stub; Firefox makes no call to `127.0.0.1`, see decision 0005); its camera and screen, which are refused in the answer and were not switched on; a network between the two ends; and how a real application built on the client's React components behaves, which nobody ran.

## What is not claimed

- **Pipecat's SDKs for phones** (Swift, Kotlin, React Native) use the same signalling. They are not tested here, and nothing is said about them until they are.
- **Other Pipecat transports** (Daily, LiveKit, its WebSocket transport, Media over QUIC) are other protocols. The gateway speaks SmallWebRTC's.
- **A Pipecat server's Python API.** The gateway is for clients. An agent written for Pipecat's pipeline is not run by waveport.
