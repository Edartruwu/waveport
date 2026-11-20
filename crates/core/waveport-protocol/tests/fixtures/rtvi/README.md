# RTVI as the gateway reads and writes it

`from_client.jsonl`: what a client wrote (`given`, which is the message, or the text where it is no JSON) and what it is read as (`read`). The two `client-ready` messages at the top are the stock Pipecat client's (`@pipecat-ai/client-js` 1.16.0) in Chrome 157 and Firefox 158, as the spike behind [decision 0006](../../../../../../docs/decisions/0006-rtvi-gateway.md) recorded them on 16 November 2026; the signalling message and the ping are that client's too. The rest is written from the source of Pipecat 1.16.0 and of that client as read on 20 November 2026, and from what a client may get wrong.

`to_client.jsonl`: every kind of message the gateway writes, as it is written. The test builds each in the same order and holds it against its line.
