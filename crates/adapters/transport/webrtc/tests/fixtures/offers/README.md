# What two browsers offer

The offers of Chrome 157 and Firefox 158 for a call of audio with a data channel, as the spike behind [decision 0005](../../../../../../../docs/decisions/0005-webrtc-endpoint.md) recorded them on 16 November 2026.

What names a machine or a call is made up: the addresses, the candidates (there are none), the credentials of ICE, the fingerprint, the stream and source identifiers. Everything an answer depends on is as the browsers wrote it: the payload types and their order, the formats, the header extensions and the kinds of feedback.

A test answers them, so the fingerprint and the credentials only have to look like some.

`pipecat-chrome.sdp` and `pipecat-firefox.sdp` are what the stock Pipecat client (`@pipecat-ai/client-js` 1.16.0 with `@pipecat-ai/small-webrtc-transport` 1.11.2) had the same two browsers offer on the same day, recorded by the same spike for [decision 0006](../../../../../../../docs/decisions/0006-rtvi-gateway.md): audio both ways, video both ways, a second line of video for a shared screen, and a data channel the client opens itself. They are made up in the same places.
