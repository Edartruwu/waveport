# What Twilio says on a media stream, and what it is told

These are the examples of Twilio's documentation, as read on 17 November 2026, one message a line. Nothing here was recorded from a call: the transport has not been held against the provider yet.

- `from_carrier.jsonl`: what Twilio sends, from "Media Streams WebSocket messages". The second `dtmf` line names its track `inbound_track`, as a copy of the same page from November 2025 has it; the page says `inbound` now.
- `to_carrier.jsonl`: what a server may send on a stream it was connected to with `<Connect><Stream>`, from the same page. Twilio warns of a message that holds a field it does not know, so what the transport writes has to be these and nothing more.

The names in them are the documentation's placeholders, and the audio is a few bytes of it.
