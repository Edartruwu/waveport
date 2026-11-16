# Findings: speech-to-speech providers

**Date:** 16 November 2026. Raw material for decision 0007 (the speech-to-speech port).

Two halves, and they are not the same kind of evidence:

- **OpenAI Realtime was measured.** Every statement about it below was seen on the wire that day, over a WebSocket to `wss://api.openai.com/v1/realtime`, and names the recording it is in (`recordings/openai/<name>.jsonl`). The model was `gpt-realtime-2.1-mini` unless the recording's name ends in `.full`, which is `gpt-realtime-2.1`. Both ids were confirmed with the models endpoint. Times are from one laptop on one line: an acknowledgement took 152 ms at the median (141 to 659 ms over 170 of them), so anything near 150 ms below is mostly the round trip.
- **Gemini Live was not run.** There was no key. Everything about it below is a reading of its documentation on the same day (`gemini-3.8-live`, the Live API guides and WebSocket reference), marked **read, not run**. The one thing measured is how a wrong key is refused. The runner for the rest is written and waits for `GEMINI_API_KEY` (see the end).

Speech sent was made by a machine (Flite). The runs are few: one to four of each. The timings say what order things come in and roughly how long they take; they are not percentiles.

## What contradicts the plan

1. **A truncate that reaches OpenAI before the cancelled response is over cuts the audio and leaves the text whole.** The provider's own guide says not to wait. Sent together with `response.cancel`, a truncate at 1.5 s left the item with 1.5 s of audio and the transcript of everything generated, and the model then said it had spoken twenty-one numbers of which the user could have heard two. The same truncate sent again after `response.done` cut the text. Four runs, both models. `Stop { heard }` is two commands with an order the adapter has to keep.
2. **OpenAI's detector as a signal still commits, and it commits again after a commit by the client.** With both automatic flags off it creates no response, as documented, but it writes its own user items, and when the client commits a turn under it, it later commits a second, empty item for the silence that followed. Using the signal means cleaning up after it. Turn detection off is the clean way to own turns.
3. **Acknowledgements do not name the command they answer.** No acknowledgement carries the client's `event_id`; only errors do, and nine of the twenty-two kinds of error seen came without it. Outcomes have to be matched by kind and order, with one command of a kind in flight.
4. **`Respond` never comes back empty on OpenAI; it comes back with speech nobody wanted.** A response asked of an empty conversation, asked again after the assistant's own turn, or told to stay silent, spoke every time. The outcome `NoResponse` is Gemini's. OpenAI's response-less endings are other things: `cancelled`, `incomplete` with no output, and `failed`.
5. **`TurnBegins` is nothing on OpenAI and is an interruption on Gemini** (read). And **`Inject` interrupts on Gemini** (read): any `clientContent` stops the model.
6. **Gemini does have a way to stop the model** (read): `clientContent` interrupts unconditionally, and so does `activityStart`. What it lacks is truncation: its history keeps what was sent.
7. **OpenAI takes audio at 24 kHz or as G.711, nothing else.** 16 kHz, which the product listens at, is refused. Gemini takes any rate and is native at 16 kHz (read).
8. **Seeding a new OpenAI session costs about 27 ms an item**, one after the other: 400 items took 10.9 s. A reconnect at the sixty-minute limit cannot replay a long call item by item inside a turn gap.
9. **Words do come later, and on OpenAI never earlier.** The words of a turn arrived 0.6 to 1.0 s after the commit, about when the response's first audio did, and not once before the commit, whichever transcriber. Gemini documents interim words while the user speaks (read).
10. **A tool call is spoken first.** Both OpenAI models said a sentence ("Let me check…") and then called, in one response. A result sent twice is accepted twice and the model reports both.

## OpenAI Realtime, measured

### The session

- Connecting took 525 to 1,595 ms (median 754, 26 connections). `session.created` comes at once. `expires_at` is 3,600 s after creation (`limits`).
- The session a connection gets has the detector on (`server_vad`, `create_response` and `interrupt_response` true), transcription off, output `audio`, `max_output_tokens` `inf`, `truncation` `auto`, and instructions of the provider's own (`update`, its `session.created`). An adapter has to set all of it.
- Nothing outlives the connection. A second connection could not retrieve an item of the first, `session.update` has no field for a conversation, and `response.conversation` takes only `auto` and `none` (`reconnect`, `reconnect.second_connection`). There is no resume.
- An idle connection stays: 150 s with nothing sent brought no message and no close, and the session still answered (`limits`).
- The client events it accepts, by its own error message: `session.update`, `session.close`, `input_audio_buffer.append`, `session.input_audio_buffer.append`, `input_audio_buffer.commit`, `input_audio_buffer.clear`, `conversation.item.create`, `conversation.item.truncate`, `conversation.item.delete`, `conversation.item.retrieve`, `response.create`, `response.cancel`. `session.close` is refused outside translation sessions (`aside`). `output_audio_buffer.clear` is not accepted on a WebSocket (`limits`).

### A spoken turn with the provider's detector (`detector_on`, `.full`, `.full_minimal`, `.semantic`)

Order: `input_audio_buffer.speech_started` → `speech_stopped` → `committed` → `conversation.item.added` and `.done` (the user item) → `response.created` → input transcription deltas → `response.output_item.added` → transcript and audio deltas → `conversation.item.input_audio_transcription.completed` → `response.output_audio.done` → `response.done` → `rate_limits.updated`.

Milliseconds after the last sample of speech was sent, four runs:

| | mini, server | mini, semantic | full | full, effort minimal |
|---|---|---|---|---|
| `speech_stopped` | 571 | 557 | 587 | 599 |
| `response.created` | 579 | 576 | 610 | 623 |
| first audio | 1,063 | 1,165 | 1,218 | 1,699 |
| words of the turn complete | 1,150 | 1,365 | 1,528 | 1,224 |
| `response.done` | 1,372 | 1,552 | 2,046 | 2,464 |

The detector's 500 ms of silence is half of the wait. From `response.create` to first audio, over every spoken response of the spike: mini 376 to 1,175 ms (median 626, 21 responses), full 625 to 1,016 ms (median 813, 9).

### Text against audio

The transcript of a response leads its audio and carries no times. In `detector_on.full` all 87 letters had come when 3.2 s of the 5.2 s of audio had; in `detector_on` the text was complete at 1.6 of 3.25 s (`tools/digest.py --align`). Audio itself comes far faster than it plays: in the 1.5 s after the first audio the mini model had sent 13.6 to 15.6 s of audio and the full model 7.6 s (`interrupt`, `interrupt.full`). So text does not trail, and neither text nor audio arrival says anything about what was heard. That has to come from playout.

### A turn ended by the client (`client_turn`, `.live_transcribe`, `.full`)

With `turn_detection: null`, `input_audio_buffer.commit` and `response.create` sent in one breath. Milliseconds after the commit:

| | mini | mini, `gpt-live-transcribe` | full |
|---|---|---|---|
| `input_audio_buffer.committed`, item added and done | 165 | 160 to 169 | 169 to 230 |
| `response.created` | 208 | 169 | 230 |
| first word of the turn | 587 | 280 | 487 |
| first audio of the response | 760 | 763 | 985 |
| words of the turn complete | 932 | 934 | 839 |
| `response.done` | 993 | 1,146 | 1,940 |

- The words of a turn come after its commit, always, and finish about when the response starts to speak: before the first audio in one run, after it in two. With no response asked for, a one-second utterance had its words 641 to 1,016 ms after the commit (turn C of each).
- No word came before a commit in any run, with either transcriber. Transcription in a conversation session starts at the commit.
- `response.create` does not commit. Asked for with audio in the buffer and no commit, the response answered the previous turn again and left the audio in the buffer, where a later commit found it (turn B).
- Committing less than 100 ms is refused: `input_audio_buffer_commit_empty` (`drop_input`, `limits`).

### The detector as a signal only (`signal_only`)

`server_vad` or `semantic_vad` with `create_response: false` and `interrupt_response: false`.

- `speech_started` came about 350 ms after speech began (with `audio_start_ms` 300 ms earlier than it was detected, the prefix), and `speech_stopped` about 15 ms after the 500 ms of silence was complete, with `audio_end_ms`. No response was created (step 1). The semantic detector gave the same events (step 4).
- The provider committed by itself at `speech_stopped`: `input_audio_buffer.committed`, a user item, then its transcription 350 ms later.
- A long response was not stopped by the user speaking over it; `speech_started` came and the response went on until the client cancelled it (step 2).
- An item the provider committed can be taken out: `conversation.item.delete` → `conversation.item.deleted` (step 3).
- **A commit by the client under the detector is accepted, and the detector then commits again.** With a 2 s silence window, the client committed as soon as the speech was sent and got its item. Two seconds later the provider sent `speech_stopped` and committed a second item holding only the silence, with an empty transcript (step 6).
- `input_audio_buffer.clear` in the middle of detected speech is acknowledged, the detector does not report a stop, and what it later commits holds only the audio after the clear, under a new item id. The id announced at `speech_started` is never committed (step 5).
- With `idle_timeout_ms` set and automatic responses on, the provider speaks by itself: it adds a system item of its own, commits the silent buffer as a user item and responds. It fired only when audio was flowing: 8 s with nothing sent did nothing, and the first silence sent after that fired it at once (`unasked`, step g). This is the only thing found that makes it speak unasked.

### Dropping input (`drop_input`)

`input_audio_buffer.clear` after 3 s of an utterance → `input_audio_buffer.cleared` in 145 ms. A commit right after is refused as empty. The next turn, committed, held only its own audio: the model asked to repeat everything the user had said repeated the second utterance alone, and the retrieved item's audio was that utterance's length. A clear of an empty buffer is acknowledged too. Audio left in the buffer with no commit does nothing: 8 s, no message (`unasked`, step a).

### An interruption (`interrupt`, `interrupt.full`)

`response.cancel` 1.5 s after the first audio:

- Audio went on arriving for 135 to 157 ms (three or four more messages, 1.2 to 1.6 s of audio). Then `response.output_audio.done`, the transcript's `done` with all the text generated, the item `done` with status `incomplete`, and `response.done` with status `cancelled`, reason `client_cancelled`, 182 to 199 ms after the cancel.
- A cancel sent right behind `response.create` gives `response.created` and `response.done` (cancelled) 44 ms apart with no item at all (part C). A cancel with nothing to cancel is refused: `response_cancel_not_active`.

`conversation.item.truncate {item_id, content_index: 0, audio_end_ms}` → `conversation.item.truncated`:

| Sent | The item afterwards (`conversation.item.retrieve`) | What the model then said it had spoken |
|---|---|---|
| With the cancel, before `response.done` (part A) | 1.5 s of audio; **the whole transcript**, twenty-one numbers | "1 through 21" (mini), "1, 2, … 10" (full, all it had generated) |
| After `response.done` (part D) | 1.5 s of audio; "forty, forty-one," | "40, 41" |
| With the cancel, and again after `response.done` (part E) | whole transcript after the first; "seventy, seventy" after the second | — |
| On a response that had finished, at 1.0 s (part B) | 1.0 s of audio; "the quick brown fo" | — |

- The cut of the text is the provider's own estimate and lands in the middle of a word.
- Truncating again at the same place is accepted. Earlier is accepted. Later than the item now is, or beyond its end, is refused: `invalid_value`, "Audio content of 1000ms is already shorter than 1400ms", with no `event_id`. An item can only shrink.
- Truncating at zero leaves an item with an empty transcript and no audio; the mini model then said it had spoken nothing.
- A user item cannot be truncated (`unsupported_content_type`).

### Responses nobody wanted, and responses that do not come (`unasked`)

- `response.create` on an empty conversation spoke a sentence. Asked again straight after its own turn, it spoke again. Told by its instructions to produce nothing, with no input, it greeted the user, in audio and in text.
- A second `response.create` while one is active is refused: `conversation_already_has_active_response`, naming the command. Out-of-band responses (`conversation: "none"`) may run side by side: two at once both finished (`inject`).
- `max_output_tokens: 16` ended `incomplete`, reason `max_output_tokens`, with no output item. `max_output_tokens: 1` ended `failed` with a `server_error` asking to retry. A failed response is a `response.done`, not an `error` event.

### A function call (`tool`, `tool.full`)

- One response held two items: a spoken message ("Let me check the current time for you.") and then the call. `response.output_item.added` (function_call) → `response.function_call_arguments.delta` (five pieces for `{"timezone":"UTC"}`) → `.done` → the item's `done` → `response.done`. The arguments began 1.1 to 1.4 s after the request, after the sentence's audio.
- The result is an item: `conversation.item.create` with `function_call_output` → `conversation.item.added` and `.done` (190 to 290 ms). It starts nothing. `response.create` after it spoke the answer.
- **The same result sent again is accepted as another item**, and the model asked afterwards reported both times. The duplicate can be deleted. A result for an unknown call is refused: `invalid_tool_call_id`, with no `event_id`.
- A call left open does not block. The user spoke again, and the response answered the new question with no complaint. The late result was accepted; the mini model then reported the time, the full model repeated its previous answer instead. A late result needs instructions on its response to be spoken reliably.

### Injecting items and overriding a response (`inject`, `aside`)

- `conversation.item.create` → `conversation.item.added` and `.done` in 145 to 190 ms, for a user text, an assistant text, a `system` text, and a user item holding audio. The client may choose the item's id. `previous_item_id` places it after any item, or first with `root`.
- Refused: an id already used (`item_create_duplicate_item_id`), an unknown `previous_item_id`, the role `developer`, and any assistant item that is not `output_text`. An assistant's audio cannot be put back; history is text.
- `response.create` takes, for that response alone: `instructions` (they replaced the session's: the answer was "override"), `output_modalities` (text only works and streams `response.output_text.delta`), `max_output_tokens`, `tools`, `tool_choice`, `audio.output.voice` and `.format`, `input` (items and references to items, or none), `metadata`, and `conversation: "none"`.
- An out-of-band response does not reach the conversation: asked for its most recent message, the model said there was none, and deleting the response's item said it did not exist. The item can still be retrieved by id, and a `conversation.item.done` is sent for it, which an adapter must not take for a conversation item.
- Output is audio or text, never both: `["audio", "text"]` is refused.

### Changing the session (`update`)

`session.update` → `session.updated` with the whole effective session, in 139 to 226 ms. Accepted at any time, also while a response is being produced: instructions, tools and tool choice, turn detection, the transcriber, noise reduction, the input and the output format (even after audio has passed in both directions), speed, `max_output_tokens`, `truncation`, reasoning effort, the modality. Refused: the voice once the assistant has spoken (`cannot_update_voice`), and the kind of session. A `model` is acknowledged and ignored: the session's model did not change. An unknown field is refused, and so is an update without `session.type`.

### Audio formats (`formats`, `.ulaw_both`, `.ulaw_in_pcm_out`)

| Format | In | Out |
|---|---|---|
| `audio/pcm` at 24,000 | taken | given |
| `audio/pcm` at 8,000, 16,000, 48,000, or with no rate | refused | refused |
| `audio/pcmu`, `audio/pcma` | taken | given |
| `audio/opus`, and the names of the beta (`pcm16`, `g711_ulaw`) | refused | refused |

A turn sent as mu-law at 8 kHz was understood and answered in mu-law (42,800 bytes for 107 audio tokens, which is 5.35 s both ways) and, in another session, in PCM at 24 kHz. Input and output formats are independent.

### Errors, limits and usage (`limits`)

- Every `error` seen had the type `invalid_request_error` and a `code`. None closed the connection, not text that is not JSON (`invalid_json`) and not a binary frame (`invalid_event`).
- Codes seen: `invalid_value`, `invalid_type`, `invalid_json`, `invalid_event`, `invalid_parameter`, `unknown_parameter`, `missing_required_parameter`, `integer_below_min_value`, `integer_above_max_value`, `input_audio_buffer_commit_empty`, `response_cancel_not_active`, `response_cancel_not_found`, `conversation_already_has_active_response`, `item_create_duplicate_item_id`, `item_create_invalid_previous_item_id`, `item_retrieve_invalid_item_id`, `item_delete_invalid_item_id`, `item_truncate_invalid_item_id`, `invalid_tool_call_id`, `unsupported_content_type`, `cannot_update_voice`.
- Without the command's `event_id`: both truncate refusals, `invalid_tool_call_id`, `response_cancel_not_found`, `cannot_update_voice`, invalid audio in an append, the two-modality refusal, `invalid_parameter`, `invalid_json`, `invalid_event`.
- `rate_limits.updated` follows every `response.created`: one entry, `tokens`, with `limit`, `remaining`, `reset_seconds`.
- Usage is on `response.done`: `input_tokens` and `output_tokens`, each split into text and audio, with `cached_tokens` (and its own split) and `reasoning_tokens`. Cached tokens appeared from the second response of a session on (384 of 405; 4,160 of 4,215). Both models spend reasoning tokens by default. Input transcription reports its own usage on its `completed` event. Several text-only responses of the mini model reported `input_tokens: 0` with a conversation behind them, so the figure is not to be trusted to the token.
- Context is 128,000 tokens and output 32,000 for both models (their model pages, read).

### A new session given an old conversation (`seed.20`, `.100`, `.400`)

Items sent in one burst, without waiting, are taken one after the other:

| Items | Until the last was acknowledged |
|---|---|
| 20 | 363 ms |
| 100 | 2,397 ms |
| 400 | 10,949 ms |

The model answered from them (the third code word, and the time a seeded tool call reported). A `function_call` and its `function_call_output` can be seeded as items. The first response over 400 items took 1,170 ms to its first word, against about 600 ms over 20.

## Gemini Live: read, not run

From the Live API guides and the WebSocket reference as they stood on 16 November 2026. The API is labelled a preview; the model `gemini-3.8-live` is labelled stable (September 2026).

- **Connection.** `wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent`, the key as `?key=` (documented) or the `x-goog-api-key` header. The first message is `setup`; the server answers `setupComplete`; nothing else is acknowledged, ever. Configuration is given once: there is no message that changes it.
  - **Measured:** a wrong key is not refused at the upgrade. The WebSocket opens, and after the `setup` message it is closed with code 1007 and the reason "API key not valid. Please pass a valid API key.", with the key in a header or in the address (`recordings/gemini/manual_turn.wrong_key_*.jsonl`). The product's kit sorts 1007 as permanent; an adapter has to read the reason to call it a matter of the key.
- **Audio.** In: 16-bit PCM at any rate named in the MIME type, native at 16 kHz, in pieces of 20 to 40 ms. Out: 24 kHz, always. Audio is the only response modality: no text-only response.
- **Turns by the client.** `realtimeInputConfig.automaticActivityDetection.disabled: true`, then `realtimeInput.activityStart`, audio, `activityEnd`. "The server acts immediately on your `activityEnd` signal": the end of the turn and the request to respond are one message. The server prepends no audio from before `activityStart`. `activityStart` interrupts the model unless `activityHandling` is `NO_INTERRUPTION`.
- **Turns by the provider, with the client forcing the end.** With detection on, `realtimeInput.audioStreamEnd` is "an immediate finalization prompt, bypassing the default server-side silence detection delay". `voiceActivity` messages (`ACTIVITY_START`, `ACTIVITY_END`, with an offset in the audio) report what its detector sees.
- **Stopping.** No cancel and no truncate. Generation is interrupted by `activityStart`, by detected speech, and by any `clientContent` ("A message here will interrupt any current model generation"). The server then sends `serverContent.interrupted` and `turnComplete`, with no `generationComplete`. "Only the information already sent to the client is retained in the session history."
- **Injecting.** `clientContent { turns: [{role: "user" | "model", parts}], turnComplete }`, at any time in the session. With `turnComplete: true` generation starts. There is no system role after setup.
- **Words.** `inputTranscription` after the fact, with no guaranteed order against other messages; `interimInputTranscription` "updated while the user is speaking"; `outputTranscription` sent "close to the corresponding audio output", the last of it before `generationComplete` or `interrupted`. Transcriptions may carry start and end offsets.
- **Silence.** Proactive audio is permanently on in this model: "the model can reject responding to the last prompt". `serverContent.waitingForInput` says it is not generating because it expects more.
- **Tools.** `toolCall { functionCalls: [{id, name, args}] }`; the client answers `toolResponse { functionResponses: [{id, name, response}] }`. Calls are non-blocking by default (`behavior: BLOCKING` restores waiting). A non-blocking result carries `scheduling`: `INTERRUPT`, `WHEN_IDLE` or `SILENT`. `toolCallCancellation { ids }` tells the client that calls "should not have been executed and should be cancelled", when the client interrupted the turn that made them.
- **Lifetime.** A connection lasts about ten minutes; `goAway { timeLeft }` comes before it ends. A session without context compression lasts 15 minutes of audio. `sessionResumption` in the setup makes the server send `sessionResumptionUpdate { newHandle, resumable }`; a handle given to the next connection's setup resumes, for two hours. "Resumption is not possible at some points in the session. For example, when the model is executing function calls or generating": then `resumable` is false and the handle empty. `contextWindowCompression.slidingWindow` drops the oldest turns at 80% of the context by default; audio costs about 25 tokens a second; the context is 131,072 tokens.
- **Seeding.** `historyConfig.initialHistoryInClientContent: true`: after `setupComplete` the server takes `clientContent` as history, without a model call, until `turnComplete: true`.

## The planned port against the two

| Intent | OpenAI Realtime, measured | Gemini Live, read |
|---|---|---|
| `TurnBegins` | Nothing to send with detection off: audio gathers in the buffer until a commit. With the detector as a signal the provider chooses where its item begins | `activityStart`, required before the turn's audio. Also stops the model unless set otherwise at setup. Not acknowledged |
| `TurnEnds { respond }` | `input_audio_buffer.commit` → `input_audio_buffer.committed` with the item's id (160 to 230 ms). `respond` is a second command, `response.create`, and may be left out. A response asked for without the commit does not see the audio | `activityEnd`: the end and the request to respond are one. `respond: false` cannot be said. Not acknowledged |
| `DropInput` | `input_audio_buffer.clear` → `input_audio_buffer.cleared` (145 ms), also in mid-speech. After a commit by the provider's detector: `conversation.item.delete` → `conversation.item.deleted` | No such command. Input that may be dropped has to be held back by the client until the turn commits; what an abandoned activity costs is for the first run |
| `Respond { overrides }` | `response.create` → `response.created` (150 to 230 ms) → first audio (median 626 ms mini, 813 ms full) → `response.done`. Overrides: instructions, modality, tools, voice, format, input, out of band. Refused while one is active | No such command. Generation starts at `activityEnd`, at `clientContent` with `turnComplete`, or by a tool result's scheduling. No overrides: configuration is the setup's |
| `Stop { heard }` | `response.cancel` → `response.done` (cancelled) in 182 to 199 ms, audio still arriving for 135 to 157 ms; then `conversation.item.truncate` → `conversation.item.truncated`. The truncate has to follow `response.done`, or be sent again after it. `heard` can only shrink and cannot pass what was generated | `activityStart` or any `clientContent` → `serverContent.interrupted`, then `turnComplete`. `heard` cannot be told: the history keeps what was sent |
| `Inject(Item)` | `conversation.item.create` → `conversation.item.done` (145 to 190 ms). User text or audio, assistant text, system text, tool calls and results; ids of the client's; anywhere in the order | `clientContent` with `turnComplete: false`. User and model text. It interrupts the model if it is speaking. Not acknowledged |
| `ToolResult { call, result, when }` | An item (`function_call_output`) → `conversation.item.done`. It starts nothing, so `when` is the driver's: `response.create` now, later, or never. A duplicate is accepted | `toolResponse`, with `when` as its `scheduling`. The provider may cancel a call. Not acknowledged |
| `Configure(..)` | `session.update` → `session.updated`, at any time and in part. Not the voice after speech, not the model | Only at setup. A change means a new connection with the handle, if a resumed session may differ in its setup, which is for the first run |

| Outcome | OpenAI Realtime, measured | Gemini Live, read |
|---|---|---|
| `Sent` | The only outcome of `input_audio_buffer.append`, which is never acknowledged (bad audio gets an error without the command's id) | The outcome of every command after `setup` |
| `Acknowledged` | Every other command has an event of its own kind, at about one round trip. None carries the client's `event_id`: matched by kind, order, and the ids the client chose | `setupComplete` only |
| `Observed` | `response.created`, the first `response.output_audio.delta`, `response.done` with its status; `conversation.item.truncated`; what `conversation.item.retrieve` returns | `serverContent` with `modelTurn`, `generationComplete`, `turnComplete`, `interrupted`; `toolCall` |
| `Refused(why)` | An `error` event with a `code`, always `invalid_request_error` in what was seen, the connection kept. Often without the command's id. A provider-side failure is not an `error` but `response.done` with status `failed` | No error message is documented. A wrong key closed the connection with 1007 and a sentence; what else does is for the first run |
| `NoResponse` | Did not occur. A response always spoke. The response-less endings are `cancelled`, `incomplete` (no output item) and `failed` | Expected and by design (proactive audio). What it looks like on the wire is for the first run |

### Where the planned port does not fit

- **`TurnBegins` should be optional and say whether it interrupts.** One provider has no such command; the other fuses it with a stop.
- **`TurnEnds { respond: false }` needs a capability.** Gemini cannot end a turn without asking for a response. A session that needs it there has to hold the turn's audio.
- **`Stop { heard }` is not one command anywhere.** On OpenAI it is a cancel, a wait for `response.done`, and a truncate. The adapter owns the sequence and reports the stop as observed only at `conversation.item.truncated` after `response.done`. `heard` beyond what was generated is refused. On Gemini `heard` goes nowhere, and the kernel's conversation is the only record of it.
- **Outcomes cannot be tied to a command by an id.** The port needs either one command of each kind in flight, or outcomes that name the kind and the item or response they concern. `Refused` in particular often comes with nothing to tie it to.
- **`NoResponse` should mean "the response ended with nothing said", with its reason:** the model chose silence (Gemini), ran out of tokens before speaking, failed at the provider, or was cancelled before its first item. Only the first is the planned meaning.
- **`Respond` must never be sent on speculation.** On OpenAI it produces speech whether or not there is anything to answer.
- **`ToolResult`'s `when` has two owners.** On OpenAI the driver decides by when it sends `Respond`; on Gemini the provider decides by `scheduling`. And a result must be sent once: OpenAI keeps both copies.
- **`Configure` is live on one and a reconnect on the other**, and even the live one refuses the voice after speech and ignores the model. The capability has to say which settings.
- **`Inject` may interrupt** (Gemini), and what can be injected differs: OpenAI has system items and items placed mid-order; Gemini has user and model text at the end only.
- **Capabilities the sketch lacks:** input rates taken (OpenAI 24 kHz and G.711 only); words before the turn ends (Gemini's interim transcription; OpenAI none); a tool call that speaks first; whether the provider's detector, used as a signal, also commits (OpenAI: yes); whether a client's forced end of turn exists under the provider's detector (Gemini `audioStreamEnd`; OpenAI a commit, which leaves a second item behind); the cost of seeding per item; text-only output (OpenAI yes, Gemini no), which decides whether our own synthesiser could speak for the model.

### GPT-Live, read, for the vocabulary

Its commands as its guide names them: `session.start` (with `input` as history), `session.input_audio.append`, `session.input_audio.mute` and `.unmute`, `session.instructions.append`, `session.thinking.append`, `session.commentary.append`, `session.update` (delegation settings only), `session.close`. No turn commands at all. Its acknowledgements do name the command (`client_event_id`), and an append is acknowledged only "when the session timeline reaches the estimated end of the added context", so an acknowledgement there can stay pending. For the port: `Inject` with a kind (instruction, fact, something to say), a mute as the only input control, `Configure` at start only, and an acknowledgement that may never come.

## What it cost

About 30 US cents. The recordings that were kept hold 76 responses: on the mini model 13,550 text and 571 audio tokens in, 3,685 text and 2,514 audio tokens out, about 7 cents at the prices on its model page; on the full model 3,241 and 249 in, 1,079 and 1,153 out, about 12 cents. Earlier runs of scenarios that were then sharpened and run again are not in those counts and were of the same size on the mini model. Gemini cost nothing: a wrong key is refused before any model runs.

## What waits for the Gemini key

Everything in the Gemini column above, which is a reading. With `GEMINI_API_KEY` set:

```sh
cd spikes/realtime && ./target/release/gemini all --quiet && ./target/release/gemini go_away --quiet
```

The questions the first run answers, by scenario:

- `manual_turn`: is anything acknowledged; the time from `activityEnd` to the first audio; when the words of the turn come; whether text trails audio.
- `held_input`: what holding a turn until it commits costs, against streaming it.
- `drop_input`: what becomes of audio in an activity that is never ended, and of audio sent outside any activity.
- `interrupt`: how long audio goes on after each of the three ways to stop; whether the history keeps what was sent; whether a model turn injected by the client corrects it.
- `silence`: what a turn the model does not answer looks like, and how long until the client can know.
- `tool`: whether the model speaks before a call; each scheduling; a late result; a duplicate; a cancellation.
- `seed`: how long initial history of 20, 100 and 400 turns takes.
- `update`: whether a second `setup` is an error or the end of the connection; which input formats are refused and how; what wrong messages get.
- `resume`: when handles come; the windows without one; whether a resumed session may be set up differently.
- `detector`: `voiceActivity` and its offsets, interim words, `audioStreamEnd`.
- `go_away`: when the notice comes, how much time it gives, how the connection ends.

The runner was written without the provider to try it against. Its messages follow the reference, and a field name may still be wrong.
