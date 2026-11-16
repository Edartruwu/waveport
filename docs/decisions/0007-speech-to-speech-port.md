# Decision 0007: the speech-to-speech port

**Date:** 16 November 2026. **Status:** accepted for OpenAI Realtime, which was measured. For Gemini Live it is a reading of the documentation and is confirmed or corrected by the first run with a key. **Phase:** 2, before milestone 31.

A speech-to-speech model hears audio and answers in audio. Phase 2 of the [roadmap](../../ROADMAP.md) adds two of them, and the [architecture](../architecture.md) says that the kernel keeps owning turns, the conversation and the tools when one is used. This record fixes the port they are reached through. It rests on a spike, [`spikes/realtime`](../../spikes/realtime/FINDINGS.md), which drove OpenAI Realtime (`gpt-realtime-2.1` and its mini) through every command the port needs and read Gemini Live's reference (`gemini-3.8-live`) against the same list. There was no Gemini key, so that half was not run.

## Decisions

| Question | Decision | Why |
|---|---|---|
| What the port is written in | Intents. The driver says what it wants (this turn has ended, respond, stop at what was heard, here is a tool's result), and the adapter owns the provider's messages for it and their order | The two providers share almost no commands. Stopping is three messages in a fixed order on one and a side effect of another message on the other |
| How the driver learns what became of a command | From events that name what they are about: a response, an item of the conversation, a tool call. No command has an outcome of its own, and at most one command of a kind is under way | No acknowledgement of OpenAI's names the command it answers, and nine of the twenty-two kinds of error seen came without it. Gemini acknowledges nothing after setup |
| Who decides turns | The kernel, with the session's own listening models. The provider's detector is switched off | It is what the architecture says. A provider that cannot give up turns is refused when the session is planned, by name |
| The provider's detector as a second opinion | Not in phase 2. The capability is recorded | On OpenAI, a detector that creates no response still writes items of its own, and after a turn ended by the client it writes a second, empty one for the silence that follows. Using it means cleaning the provider's conversation up after it |
| A turn's audio | Streamed as it is heard where the provider can drop it, and held until the turn is committed where it cannot | The kernel dismisses turns: a cough, a noise of agreement. OpenAI drops a buffer on request. Gemini has no such command, so audio it was sent is in its history |
| The end of a turn and the request to respond | One intent with a flag, `TurnEnds { respond }`. A provider that cannot end a turn without responding says so | They are two messages on OpenAI and one on Gemini. A session that may leave a turn unanswered (more than one participant, with a policy on who is addressed) is refused on a provider that always answers |
| Asking for a response | Only when the kernel owes one | On OpenAI a response always speaks: asked of an empty conversation, asked twice, or told to say nothing. So a reply is never begun early on such a model ([decision 0004](0004-session-shapes.md)) |
| Stopping | `Stop { heard }`, with what the listener heard taken from playback before the stop. The adapter reports the stop only when the provider's own record has been cut, and says whether it could be | On OpenAI a cut that arrives before the cancelled response has ended shortens the audio and leaves all the text in the model's context. Gemini cannot cut at all |
| Which record of the conversation is true | The kernel's. Where the provider's differs it is a shortfall of the plan, and the next connection is seeded from the kernel's | Gemini keeps what it sent and not what was heard. OpenAI's cut of the text is its own estimate and lands in the middle of a word |
| A response that says nothing | It ends the response without an error, with its reason: the model chose silence, it ran out of room before it spoke, it failed at the provider, or it was cancelled before its first word | Silence is by design on Gemini. On OpenAI the endings without speech are the other three |
| Tools | Through the tool layer, as in every shape. A result is sent to the provider once. A call the provider cancels is cancelled there | OpenAI keeps both copies of a result sent twice, and the model reports both. Tools the provider runs by itself are refused when they have effects, because they pass by at-most-once dispatch |
| When a tool's result is spoken | The driver's decision on a provider where a result starts nothing, and the provider's where the result carries its own scheduling | On OpenAI a result is an item, and the response is asked for separately. On Gemini the result says whether it interrupts, waits or stays silent |
| Changing a session that is under way | A capability that says which settings can change on a live connection. Anything else is a new connection at a turn boundary | OpenAI changes nearly everything live, but refuses the voice once it has spoken and ignores the model. Gemini takes its configuration once |
| A connection that ends | Routine. The next one is opened and seeded while the old one still serves, and is taken at a turn boundary | Gemini announces the end after about ten minutes and gives a handle to resume with. OpenAI ends a session at sixty minutes with none, and seeding costs about 27 ms an item, which is 11 s for 400 |
| The words of a turn | After the turn, on both. The agent of such a session is set to words that follow ([decision 0004](0004-session-shapes.md)) | On OpenAI they came 0.6 to 1.0 s after the commit, about when the response began to speak, and never before it |
| What was spoken | Worked out from playback and the share of the text, as for a synthesiser without word times | The text of a response runs ahead of its audio and carries no times, and audio arrives five to ten times faster than it plays |
| Verbatim speech (`Say`) | The session's synthesiser if it has one. Else the provider is asked, with instructions for that response alone, and the plan names the shortfall. Else it is refused when the session starts | A model asked to repeat a text may not |

## The intents

```rust
pub enum RtCmd<'a> {
    /// The turn the audio from here on belongs to has begun.
    TurnBegins,
    /// That turn is over. `respond` asks for the response to it.
    TurnEnds { respond: bool },
    /// The audio sent since the last turn ended belongs to no turn.
    DropInput,
    /// Respond to the conversation as it stands.
    Respond { with: Overrides<'a> },
    /// Stop the response. This much of it was heard.
    Stop { response: ResponseId, heard: MediaDur },
    /// Something for the conversation that nobody said aloud.
    Inject { item: Item<'a> },
    /// What a tool answered.
    ToolResult { call: ToolCallId, result: &'a str, speak: When },
    /// The session is to be run differently from here on.
    Configure { to: &'a Setup },
}
```

Audio goes beside the commands, as it does to a recogniser, and is never answered.

| Intent | OpenAI Realtime (measured) | Gemini Live (read, not run) |
|---|---|---|
| `TurnBegins` | Nothing is sent | `activityStart`, which also stops the model |
| `TurnEnds { respond }` | `input_audio_buffer.commit`, then `response.create` if asked | `activityEnd`. It always responds |
| `DropInput` | `input_audio_buffer.clear` | Not there. The driver holds a turn's audio until its commit |
| `Respond` | `response.create`, with instructions, tools or voice for that response alone | Not there. A response begins at the end of a turn, at an injected item, or by a tool result |
| `Stop { heard }` | `response.cancel`, a wait for `response.done`, then `conversation.item.truncate` | `clientContent`, which interrupts. What was heard cannot be told |
| `Inject` | `conversation.item.create`, anywhere in the order: user text or audio, assistant text, system text, tool calls and results | `clientContent`, at the end: user and model text. It stops the model if it is speaking |
| `ToolResult` | An item. It starts nothing | `toolResponse`, with its scheduling |
| `Configure` | `session.update`, in part and at any time. Not the voice after speech, not the model | At setup only |

## What the adapter reports

- **A response:** that it began, its audio, its text, a tool call in it, and how it ended. The endings are: said in full, stopped (with whether the provider's record was cut to what was heard), nothing said (with the reason), failed.
- **The words of a turn,** when the provider has them.
- **A tool call it has taken back.**
- **The connection:** that it will end and when, a handle to resume with, and whether resuming is possible at this moment.
- **A refusal:** which kind of command, and the provider's reason. Where the provider says which item or response it is about, that too.

## What a provider says it can do

| Capability | OpenAI Realtime | Gemini Live |
|---|---|---|
| Audio it takes | 24 kHz linear, or G.711 at 8 kHz. Nothing else: 16 kHz is refused | Any rate, 16 kHz natively |
| Audio it gives | 24 kHz linear, or G.711 | 24 kHz linear |
| Turns | The client's, with the end and the response apart | The client's, with the end and the response as one |
| Whether a turn's beginning stops the model | No | Yes |
| Dropping input | Yes | No |
| Stopping | Cancels, and cuts its record to what was heard | Interrupts, and keeps what it sent |
| Injecting | Any role, anywhere, without disturbing a response | User and model text at the end, and it interrupts |
| When a tool's result is spoken | The driver decides | The result says |
| Tool calls it takes back | No | Yes |
| What changes on a live connection | Instructions, tools, turn settings, formats. Not the voice after speech, not the model | Nothing |
| Life of a connection | 60 minutes, known from the start. Nothing outlives it | About 10 minutes, announced. A handle resumes for two hours, except while it generates or runs a tool |
| Seeding a new connection | One item at a time, about 27 ms each | A history given at setup, without a model call |
| Words of a turn | After the commit | After it, and interim words while the user speaks |
| A response in text alone | Yes | No |
| Silence as an answer | No. It always speaks | Yes |
| Its detector as a signal only | Yes, and it writes items of its own | Yes, as reports of voice activity |
| Tools it runs itself | Yes (remote MCP) | Yes |

Two things follow from the first rows. A session on OpenAI resamples what it hears from 16 to 24 kHz, or, on a telephone line that carries G.711, can hand the line's audio over as it came. And only a provider that answers in text alone could have the session's own synthesiser speak for it, which phase 2 does not build.

## The same list against GPT-Live

OpenAI's newer interface was read, not run. It has no commands for turns at all: audio is appended and muted, and instructions, thoughts and commentary are appended to a timeline. The model decides when to speak. It is refused by the rule above, because it cannot give up turns. The port would still fit it: its appends are `Inject` with a kind, its only input control is a mute, its configuration is given at the start, and its acknowledgements may stay pending, which a port without per-command outcomes does not mind.

## What was decided against

- **An outcome for every command** (`Sent`, `Acknowledged`, `Observed`, `Refused`, `NoResponse`), as first sketched. It cannot be built on either provider: one does not say which command an acknowledgement answers, and the other acknowledges nothing.
- **Sending the cut together with the cancel,** as the provider's guide suggests. Measured four times on two models, it leaves the whole text in the model's context.
- **Replaying a whole conversation at a reconnect.** At 27 ms an item it does not fit between two turns of a long call. The next connection is seeded ahead of time, and items that enter the conversation meanwhile are added to it.
- **The provider's detector as a signal on OpenAI.** See the table.
- **A turn commanded into the provider's history and corrected later.** Gemini's record of an interrupted response cannot be corrected on a live connection. It is named as a shortfall, and put right when the next connection is seeded.

## What the first run with a Gemini key has to answer

The runner is written (`spikes/realtime/src/bin/gemini.rs`) and has never met the provider. In the order in which an answer would change this record:

1. Whether anything after setup is acknowledged, and how an error is told.
2. What holding a turn's audio until its commit costs, against streaming it.
3. How long audio goes on arriving after each way of stopping, and whether a model turn injected by the client corrects the history.
4. What a turn the model does not answer looks like, and how soon the client can know.
5. When handles to resume come, how long the windows without one are, and whether a resumed session may be set up differently.
6. Each scheduling of a tool result, a late result, a duplicate, and a call taken back.
7. How long a history of 20, 100 and 400 turns takes at setup.
8. When the notice of a connection's end comes, and how much time it gives.

Until then the Gemini adapter (milestone 35) is written from this reading, and its crate says in bold that it has not been held against the provider.

## What it cost

About 30 US cents, on OpenAI. The recordings are in `spikes/realtime/recordings/`. They hold no audio and no key.
