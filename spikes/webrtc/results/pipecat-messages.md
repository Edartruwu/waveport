# What the stock Pipecat client and the stub said to each other

Made by `PAGE=pipecat/index.html ./run.sh <browser> 'exp=pipecat&dur=8'` on 16 November 2026 with `@pipecat-ai/client-js` 1.16.0 and `@pipecat-ai/small-webrtc-transport` 1.11.2 (RTVI protocol 2.3.0), bundled by esbuild 0.28.2. HTTP bodies are in `pipecat-http-*.json`; the SDP of each run is in the report (`offer`, `answer`).

## Chrome 157, `connect({webrtcUrl})`

State after `connect`: `ready`; at the end: `ready`; DTLS: DTLS1_2.

On the data channel, from the client (ms after the answer was made):

```
    67.5  (channel open) {"label": "chat"}
    69.0  {"type":"signalling","message":{"type":"trackStatus","receiver_index":0,"enabled":true}}
    69.0  {"type":"signalling","message":{"type":"trackStatus","receiver_index":1,"enabled":false}}
    69.0  {"type":"signalling","message":{"type":"trackStatus","receiver_index":2,"enabled":false}}
    69.1  {"label":"rtvi-ai","type":"client-ready","data":{"version":"2.3.0","about":{"library":"@pipecat-ai/client-js","library_version":"1.16.0","platform_details":{"browser":"Chrome","browser_version":"157.0.0.0","platform_type":"desktop","engine":"Blink"},"platform":"Linux"}},"id":"0dca44e2"}
  1069.1  ping: 1794862249583
  2069.4  ping: 1794862250583
```

Callbacks of the client (ms after the page began):

```
    42  onMediaStateChanged [{"cam": {"state": "initializing"}, "mic": {"state": "initializing"}}]
    42  onTransportStateChanged ["initializing"]
   543  onTrackStarted ["audio", {"id": "a817db7c-2f93-4389-aed3-e45ff309e1c3", "local": true, "name": ""}]
   545  onMediaStateChanged [{"cam": {"state": "initializing"}, "mic": {"state": "granted"}}]
   553  onTransportStateChanged ["initialized"]
   553  onMediaStateChanged [{"cam": {"state": "uninitialized"}, "mic": {"state": "granted"}}]
   554  onTransportStateChanged ["connecting"]
   637  onTrackStarted ["audio", null]
   638  onTransportStateChanged ["connected"]
   638  onConnected []
   638  onBotConnected [{"id": "SmallWebRTCConnection#33", "local": false, "name": "bot"}]
   638  onTransportStateChanged ["ready"]
   639  onBotReady [{"about": {"library": "webrtc-spike", "library_version": "0.0.0"}, "version": "2.3.0"}]
   639  onBotStartedSpeaking []
   639  onBotOutput [{"aggregated_by": "sentence", "output_type": "sentence", "spoken": true, "text": "Hello from the stub.", "text_type": "sentence"}]
   639  onUserStartedSpeaking []
   639  onUserTranscript [{"final": true, "text": "what time is it", "timestamp": "2026-11-16T00:00:00Z", "user_id": "u"}]
   639  onServerMessage [{"hello": "from the stub"}]
```

## Chrome 157, `startBotAndConnect({endpoint: "/start"})`

State after `connect`: `ready`; at the end: `ready`; DTLS: DTLS1_2.

On the data channel, from the client (ms after the answer was made):

```
    60.3  (channel open) {"label": "chat"}
    62.5  {"type":"signalling","message":{"type":"trackStatus","receiver_index":0,"enabled":true}}
    62.5  {"type":"signalling","message":{"type":"trackStatus","receiver_index":1,"enabled":false}}
    62.5  {"type":"signalling","message":{"type":"trackStatus","receiver_index":2,"enabled":false}}
    62.9  {"label":"rtvi-ai","type":"client-ready","data":{"version":"2.3.0","about":{"library":"@pipecat-ai/client-js","library_version":"1.16.0","platform_details":{"browser":"Chrome","browser_version":"157.0.0.0","platform_type":"desktop","engine":"Blink"},"platform":"Linux"}},"id":"435d18aa"}
  1062.8  ping: 1794862284084
  2062.7  ping: 1794862285084
```

Callbacks of the client (ms after the page began):

```
    58  onMediaStateChanged [{"cam": {"state": "initializing"}, "mic": {"state": "initializing"}}]
    58  onTransportStateChanged ["initializing"]
   653  onTrackStarted ["audio", {"id": "8f4a60d8-c613-4957-ba97-ba8650e1a39d", "local": true, "name": ""}]
   654  onMediaStateChanged [{"cam": {"state": "initializing"}, "mic": {"state": "granted"}}]
   662  onTransportStateChanged ["initialized"]
   662  onMediaStateChanged [{"cam": {"state": "uninitialized"}, "mic": {"state": "granted"}}]
   663  onTransportStateChanged ["authenticating"]
   664  onTransportStateChanged ["authenticated"]
   664  onBotStarted [{"iceConfig": {"iceServers": []}, "sessionId": "spike-session-1"}]
   665  onTransportStateChanged ["connecting"]
   744  onTrackStarted ["audio", null]
   747  onTransportStateChanged ["connected"]
   747  onConnected []
   747  onBotConnected [{"id": "SmallWebRTCConnection#34", "local": false, "name": "bot"}]
   747  onTransportStateChanged ["ready"]
   748  onBotReady [{"about": {"library": "webrtc-spike", "library_version": "0.0.0"}, "version": "2.3.0"}]
   748  onBotStartedSpeaking []
   748  onBotOutput [{"aggregated_by": "sentence", "output_type": "sentence", "spoken": true, "text": "Hello from the stub.", "text_type": "sentence"}]
   748  onUserStartedSpeaking []
   748  onUserTranscript [{"final": true, "text": "what time is it", "timestamp": "2026-11-16T00:00:00Z", "user_id": "u"}]
   748  onServerMessage [{"hello": "from the stub"}]
```

## Firefox 158, `connect({webrtcUrl})`

State after `connect`: `ready`; at the end: `ready`; DTLS: DTLS1_2.

On the data channel, from the client (ms after the answer was made):

```
    37.7  (channel open) {"label": "chat"}
    38.1  {"type":"signalling","message":{"type":"trackStatus","receiver_index":0,"enabled":true}}
    38.6  {"type":"signalling","message":{"type":"trackStatus","receiver_index":1,"enabled":false}}
    38.6  {"type":"signalling","message":{"type":"trackStatus","receiver_index":2,"enabled":false}}
    38.7  {"label":"rtvi-ai","type":"client-ready","data":{"version":"2.3.0","about":{"library":"@pipecat-ai/client-js","library_version":"1.16.0","platform_details":{"browser":"Firefox","browser_version":"158.0","platform_type":"desktop","engine":"Gecko"},"platform":"Linux"}},"id":"40cf0104"}
  1039.5  ping: 1794862295727
  2039.8  ping: 1794862296727
```

Callbacks of the client (ms after the page began):

```
   240  onMediaStateChanged [{"cam": {"state": "initializing"}, "mic": {"state": "initializing"}}]
   240  onTransportStateChanged ["initializing"]
  1273  onTrackStarted ["audio", {"id": "0b525816-c08a-45e9-b364-d68029bfea34", "local": true, "name": ""}]
  1276  onMediaStateChanged [{"cam": {"state": "initializing"}, "mic": {"state": "granted"}}]
  1279  onTransportStateChanged ["initialized"]
  1279  onMediaStateChanged [{"cam": {"state": "uninitialized"}, "mic": {"state": "granted"}}]
  1280  onTransportStateChanged ["connecting"]
  1347  onTrackStarted ["audio", null]
  1348  onTransportStateChanged ["connected"]
  1349  onConnected []
  1349  onBotConnected [{"id": "SmallWebRTCConnection#35", "local": false, "name": "bot"}]
  1349  onTransportStateChanged ["ready"]
  1350  onBotReady [{"about": {"library": "webrtc-spike", "library_version": "0.0.0"}, "version": "2.3.0"}]
  1350  onBotStartedSpeaking []
  1350  onBotOutput [{"aggregated_by": "sentence", "output_type": "sentence", "spoken": true, "text": "Hello from the stub.", "text_type": "sentence"}]
  1350  onUserStartedSpeaking []
  1350  onUserTranscript [{"final": true, "text": "what time is it", "timestamp": "2026-11-16T00:00:00Z", "user_id": "u"}]
  1350  onServerMessage [{"hello": "from the stub"}]
```

## Chrome 157, DTLS 1.3

State after `connect`: `ready`; at the end: `ready`; DTLS: DTLS1_3.

On the data channel, from the client (ms after the answer was made):

```
    65.5  (channel open) {"label": "chat"}
    67.8  {"type":"signalling","message":{"type":"trackStatus","receiver_index":0,"enabled":true}}
    67.8  {"type":"signalling","message":{"type":"trackStatus","receiver_index":1,"enabled":false}}
    67.8  {"type":"signalling","message":{"type":"trackStatus","receiver_index":2,"enabled":false}}
    68.2  {"label":"rtvi-ai","type":"client-ready","data":{"version":"2.3.0","about":{"library":"@pipecat-ai/client-js","library_version":"1.16.0","platform_details":{"browser":"Chrome","browser_version":"157.0.0.0","platform_type":"desktop","engine":"Blink"},"platform":"Linux"}},"id":"c3db3cfd"}
  1068.0  ping: 1794862305819
  2068.4  ping: 1794862306819
```

Callbacks of the client (ms after the page began):

```
    61  onMediaStateChanged [{"cam": {"state": "initializing"}, "mic": {"state": "initializing"}}]
    62  onTransportStateChanged ["initializing"]
   704  onTrackStarted ["audio", {"id": "4e8b5631-744b-4502-9622-81b63e3c1f6d", "local": true, "name": ""}]
   712  onMediaStateChanged [{"cam": {"state": "initializing"}, "mic": {"state": "granted"}}]
   729  onTransportStateChanged ["initialized"]
   729  onMediaStateChanged [{"cam": {"state": "uninitialized"}, "mic": {"state": "granted"}}]
   730  onTransportStateChanged ["connecting"]
   825  onTrackStarted ["audio", null]
   827  onTransportStateChanged ["connected"]
   827  onConnected []
   827  onBotConnected [{"id": "SmallWebRTCConnection#36", "local": false, "name": "bot"}]
   827  onTransportStateChanged ["ready"]
   832  onBotReady [{"about": {"library": "webrtc-spike", "library_version": "0.0.0"}, "version": "2.3.0"}]
   832  onBotStartedSpeaking []
   832  onBotOutput [{"aggregated_by": "sentence", "output_type": "sentence", "spoken": true, "text": "Hello from the stub.", "text_type": "sentence"}]
   833  onUserStartedSpeaking []
   833  onUserTranscript [{"final": true, "text": "what time is it", "timestamp": "2026-11-16T00:00:00Z", "user_id": "u"}]
   833  onServerMessage [{"hello": "from the stub"}]
```

## What the stub sends

On `client-ready` (see `endpoint/src/pipecat.rs`), each as one text message:

```
{"label":"rtvi-ai","type":"bot-ready","id":"<the id of client-ready>","data":{"version":"2.3.0","about":{"library":"webrtc-spike","library_version":"0.0.0"}}}
{"label":"rtvi-ai","type":"bot-started-speaking","id":null,"data":null}
{"label":"rtvi-ai","type":"bot-output","id":null,"data":{"text":"Hello from the stub.","spoken":true,"aggregated_by":"sentence"}}
{"label":"rtvi-ai","type":"user-started-speaking","id":null,"data":null}
{"label":"rtvi-ai","type":"user-transcription","id":null,"data":{"text":"what time is it","user_id":"u","timestamp":"2026-11-16T00:00:00Z","final":true}}
{"label":"rtvi-ai","type":"server-message","id":null,"data":{"hello":"from the stub"}}
```

It does not answer `ping: <ms>` or the `signalling` messages.
