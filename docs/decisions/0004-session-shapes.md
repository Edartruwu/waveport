# Decision 0004: session shapes, and what the kernel learns in phase 2

**Date:** 16 November 2026. **Status:** accepted. **Phase:** 2, before milestone 1.

Phase 1 built one shape of session: speech is recognised, a model writes, a synthesiser speaks. Phase 2 of the [roadmap](../../ROADMAP.md) needs two more, a session of text alone and a session on a speech-to-speech model, and it needs the kernel to know four things it does not: a line that drops and comes back, the keys of a telephone, a turn whose words arrive after it, and a reply that is begun before its turn is certain. This record says how each is done, what was decided against, and what the gate on the kernel's cost leaves room for.

## Decisions

| Question | Decision | Why |
|---|---|---|
| How a session of another shape is run | One `Session`. What differs by shape is behind a closed enum with static dispatch, built from four parts split out of the session's ear and voice: listening, recognising, playing, synthesising | A cascaded session uses all four, a text session none, a speech-to-speech session the first and the third. The kernel, the timers, the tools, the journal, the followers and the delegated agents are the same in every shape and stay in the session |
| A session of text | No change to the kernel. A text driver delivers text where a voice session starts playback, and reports it delivered | Traced through the kernel: typed text is a turn that begins and ends in one step, and "played" means delivered. Three scenarios show it on the kernel as it is |
| A line that drops | The kernel is told. It cuts what is being said where it was heard, starts no response while the line is gone, waits for a time, and closes the session as abandoned when the time is up. A line that returns is owed the reply that was cut | Today the two events flip a flag nobody reads, and a transport that ends closes the session. A browser on a train loses its connection for seconds |
| Keys | A key is an event. Keys make a turn of their own in the table of turns, which ends on a terminator, on a count, or when no key has followed for a time. It is committed as its own kind of fact | In the table of turns, "nobody is in the middle of a turn" already covers keys, so no step gains a check. As its own kind of fact, digits can be kept out of a journal or away from a model later without a change of format |
| A key while the agent speaks | It interrupts at once | A key is never a noise of agreement. There is nothing to judge |
| Words that come after the turn | A setting of the agent. Its turns are committed when they are over, without words, and the words fill the turn in place when they come, once | A speech-to-speech model answers the audio and transcribes it afterwards. Today such a turn is dismissed as empty, and its late words then begin a second turn and a second reply |
| Commands for a speech-to-speech model | None are added. The driver translates the commands there are | The architecture had sketched `Command::Realtime`. The kernel says what it wants (a response, a stop, a tool result) and never whether a provider is realtime |
| A reply begun early | A reply may be begun on an estimate that the turn is ending, while the turn is still open. It is held: nothing of it is synthesised, played or dispatched. It is adopted if the turn is committed with the words it was begun on, and cancelled otherwise. Off unless the agent's definition asks for it | The leading frameworks ship it, and measure 200 to 600 ms saved for half as many model calls again. Held until the commit, it cannot say anything the turn did not ask for |
| A provider's word on turns | The runtime feeds it to the kernel as an end-of-turn estimate. The kernel does not change | The turn policy already takes an estimate from any source |
| A provider that fails | The runtime moves to the next one. The kernel is not told | Which provider served a response is not a fact of the conversation. Telemetry and followers are told |
| Where a session is put together | A library of its own, `apps/waveport-assembly`, used by the command and by the daemon | "One agent definition" in the exit criteria has to be one value |

## A session of text

Writing the scenarios, and the driver after them, found three things that such a session has to do. None is a change to the kernel.

- **Typed text takes the floor.** The agent is stopped, as by an interruption, before the text is handed to the kernel. Otherwise the kernel, which makes a reply wait for one that is being heard, has the answer to the new message wait for the old one to be read to its end, and a conversation can end on two messages of the user and one reply. Both leading frameworks stop the agent by default. It holds in every shape: text typed into a call stops what is being said.
- **Text is told as it is written.** A session of text hands on all the model has written, whatever the agent's policy for sentences says. That policy is for a synthesiser, which sounds best on whole sentences.
- **What was delivered is reported at once, and in order.** The kernel is told that text is ready, and then that the reader has it, in the step after the one that released it and before anything else is looked at. The report that all of a response was delivered follows the one that its last text is ready: a kernel told the other way round takes it for a report that came early and waits for another.
- **A session without a transport takes whoever writes to it as there.** A participant joins when a transport says so, and such a session has none.

## What a follower is told is ahead of the journal

A session tells whoever follows it a fact when it makes it, and its journal stores the fact a moment later. That is as it was in phase 1, and it is what makes a call's news prompt. A session of text made it matter: its follower is its reader.

- **A process that dies in between leaves a journal that never had the fact.** The session that goes on from the journal gives the fact's number to another. In a chat an answer can have been read that the conversation does not hold. The session then answers again, and the model is told that something was lost. A test kills the command at exactly that place.
- **A response's number can be given twice** across the death of a process, when no fact of the first response was stored.
- **Not changed:** holding news back until the journal has it would put the journal's latency into every word a reader is shown and every caption of a call.
- **For the client protocol (milestone 7):** a client that comes back names the last fact it saw. When its session's process died meanwhile, the daemon says where the journal stood when the session was rebuilt. A client that holds facts beyond that drops them and takes the state it is sent, as it does after a gap.

## A line that drops

- **`Detached`.** A response that is being heard or prepared is cut, and what was heard of it enters the conversation, as at an interruption. A turn that is under way ends with the words it has. A timer is set for `Timing.detach_wait`.
- **While the line is gone** no response is started. Tool calls go on, and their results wait.
- **`Attached`.** The timer is cancelled and the reply that was cut is owed again. The model sees what the listener heard of the first one.
- **The timer**, if the line is still gone, closes the session as abandoned. A wait of nothing closes at once, which is what a transport that cannot come back gets.
- **`Attached` while attached does nothing**, because every restart of a process sends it.
- **Not journaled.** Whether a line is there is a matter of the moment. A session rebuilt from its journal is told by its transport.

## Keys

- **`Event::Key { who, key }`**, with the sixteen keys of RFC 4733 (digits, star, hash, A to D).
- **A turn of keys ends** on the terminator if one is set (hash by default, and not part of the keys), on the count if one is set, and otherwise when no key has followed for `KeyPolicy.wait` (three seconds by default; the two frameworks use two and four).
- **Speech and keys do not share a turn.** A key ends the speaker's open turn of speech with the words it has, and speech ends their open turn of keys.
- **`Fact::KeysCommitted { turn, who, keys }`**, which owes a reply as a committed turn does. The runtime shows the keys to the model as text of the user.
- **Waiting for a first key** is not the kernel's: only a task that asked for keys can wait for one, and tasks come with the agent layer of phase 4.

## Words that come after the turn

- **`AgentSpec.words`**: `BeforeCommit` as today, or `AfterCommit`.
- **With `AfterCommit`** a turn does not wait for a transcript and is not dismissed for having none. `Event::TurnWords { turn, text }` is taken once, for a committed turn without words, and recorded as `Fact::TurnTranscribed`. Any other is ignored.
- **A reply may be heard before the words of its turn are known.** The conversation is in the order of the turns, not of arrival.

## A reply begun early

- **Begun** when an estimate reaches `AgentSpec.early_from` and the turn has words. The model is asked with the conversation and those words.
- **Held.** Its text is not given to synthesis and its tool calls are not sent. It holds the floor for nobody.
- **Adopted** when the turn is committed with the same words: it becomes the reply, and what it has written is released at once.
- **Cancelled** when the speaker goes on, when the turn is dismissed, or when the words changed. Then the reply is begun as it would have been.
- **Not on a speech-to-speech model.** A response asked of one always speaks, and cannot be held ([decision 0007](0007-speech-to-speech-port.md)).

## What was decided against

- **A kernel for each shape.** Three kernels would have three sets of invariants, and a flow written for one shape would not run on another.
- **A trait object for the driver.** A session's step is the hottest path there is, and there are three shapes, all known when the session is built.
- **`Command::Realtime`.** See the table.
- **Keys as a transcript.** Simple, and it loses that they were keys: they could not be withheld, and a recogniser's words for the same moment would be mixed with them.
- **Keys in a table of their own.** The rule that nothing is said while somebody is in the middle of a turn would need a second check in every step.
- **Closing a session when its line drops**, with the client starting a new one from the journal. It works, and it costs the caller the reply they were in the middle of and a session's start.

## What the gate leaves room for

The kernel's cost is held by a gate: no step may take 2% more instructions than on the revision it is compared with (`cargo xtask bench-ir`), and nothing overrides it. The steps a session takes most often are short, so 2% is two to four instructions on five of them.

Measured on 16 November 2026 on a scratch branch that had the new variants and every check the four changes put on a path that exists, with their bodies as stubs:

| Step | `main` | The dropped line alone | All four |
|---|---|---|---|
| Playback reports its position | 233 | 234 | 235 |
| Someone starts to speak | 243 | 244 | 244 |
| Someone speaks over the agent | 409 | 410 | 406 |
| An interim transcript | 142 | 143 | 139 |
| A result for a response that has ended | 125 | 126 | 126 |
| Model text that is held | 841 | 842 | 842 |
| Model text that is released | 1,575 | 1,576 | 1,577 |
| The verdict that interrupts | 1,385 | 1,386 | 1,378 |
| The transcript that completes a turn | 1,612 | 1,630 | **1,647** |
| A timer for something that has ended | 132 | 133 | 133 |
| The last result of a batch of tool calls | 1,730 | 1,736 | 1,522 |
| A session rebuilt from 200 facts | 35,670 | 35,693 | **36,736** |

What it says:

- **The short steps do not notice.** New kinds of event and of timer, and new fields, cost them one instruction or none.
- **The dropped line fits by itself**: nothing grows by more than 1.2%.
- **All four at once do not fit**, on two steps: the turn that is committed (2.2%) and the session rebuilt (3.0%). They are four pull requests and each is measured against the one before, so the three checks on a committed turn come one at a time.
- **Steps that no change touches move anyway.** The last result of a batch got 12% cheaper and the rebuilding 3% dearer, with the same events in and the same commands and facts out. Keeping the new handlers out of line, keeping the settings out of the struct of timings, and taking out the new kinds of timer and of synthesis state each left both numbers where they were, so which of the changes moves them is not known. Each pull request finds out for itself. When one fails the gate on a step it did not mean to touch, the answer is to hold the shape of the code that moved, not to change what the kernel does.
- **Two of the designs are as they are because of this.** A reply begun early is held by a state of its synthesis, which the rule for releasing text already reads, and not by a flag that rule would have to check. A turn of keys is over by the same fields as a turn of speech.

## What each change must show

The scenarios are written first and are not edited to pass (`crates/core/waveport-domain/tests/scenarios/`).

| Change | Scenarios |
|---|---|
| Text | `typed_chat`, `typed_twice`, `typed_cut_off`, on the kernel as it is |
| A line that drops | `line_drops`, `line_stays_gone` |
| Keys | `keys_are_one_turn`, `keys_after_speech` |
| Words that come later | `late_words`, `words_after_barge_in`, `sound_before_words`, and the six scenarios of phase 1 unedited in a world where words come late |
| A reply begun early | Written with the change: one adopted, one cancelled because the speaker went on, one cancelled because the words changed, and none heard before its turn is committed |
