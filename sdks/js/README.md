# Waveport client for JavaScript

A client of a Waveport session for a page in a browser. It speaks [client protocol v1](../../docs/architecture.md#86-the-client-protocol) over a WebSocket, to `waveportd` or to anything else that serves the protocol, and it has a voice: what the agent says is played, what the microphone hears is sent, and the session is told what was played, by the sample.

It is ES modules that a browser runs as they are written. Nothing is bundled and nothing is installed for it to run, so a daemon serves it as part of itself, beside the [playground](#the-playground). Its types are in its comments and are checked by the TypeScript compiler.

**Status: experimental.** It is not published to a registry yet (`"private": true`): serve the files of `src/` together, or import them from a checkout. A call is on a WebSocket. A call over WebRTC comes with the WebRTC transport.

## A call

```js
import { call } from "./src/index.js";

button.addEventListener("click", async () => {
  // The page's own server makes the session and gives the page its token.
  const token = () => fetch("/session", { method: "POST" }).then((made) => made.text());
  const { client, voice } = await call("https://agents.example", { token, voice: true });

  client.on("view", (view) => render(view.state.items));
  client.on("closed", ({ reason }) => console.log("over:", reason));
});
```

`call` is called from a click or a key when it is to speak, because a browser plays no sound for a page its user has not touched. Left without `voice`, the call is one in writing: `client.text("What time is it?")`.

A spoken call does things in an order that matters, and `call` keeps to it:

1. It asks what the browser can do (`Voice.format`), then for the microphone, and has what plays ready (`Voice.open`).
2. Only then does it ask for the token, when `token` is a function. A client token is good for a minute, and a person can take longer than that to allow a microphone.
3. It connects (`Client.connect`), and puts the voice on the line the moment it is welcomed (`voice.attach`). An agent that greets speaks as soon as its far end is on the line. A voice that was not there for it would not play the greeting and would not say that it had, and the session would have it on record as never heard.

Whoever uses `Client` and `Voice` by themselves keeps to the same order.

### Where the token comes from

A session is made by whoever holds a control key of the daemon: the application's server, with `POST /v1/sessions`. The answer holds a client token (`wpc1_…`), which names its session, connects to it once, and is good for a minute. That is what a page is given. A control key is never given to a page.

A token is said in the first message on the connection, and never in an address: `lineOf` refuses an address with a `?` or a name and password in it.

## The client

```js
import { Client } from "./src/client.js";

const client = await Client.connect("https://agents.example", { token, features: ["said", "hearing"] });
```

| | |
|---|---|
| `client.text(written)`, `client.dtmf(keys)` | Says something to the agent. Kept while the line is away, 32 at most, and sent when it is back |
| `client.interrupt()` | Stops the agent. The session goes on |
| `client.bye()` | Ends the session |
| `client.detach()` | Lets go of the line and does not come back. The session waits, and `client.resume`, read before, is what another `Client.connect(address, { resume })` comes back to it with, from this page or the next |
| `client.reconnect()` | Lets go of the connection and comes back on another, for a page that knows its network changed before the connection does |
| `client.view` | What the session has come to, as far as the client was told: `view.state.items` is the conversation, `view.state.speech` what the agent is saying and how much of it was heard |
| `client.status` | `open`, `away` while the client comes back, `closed` |
| `client.on(type, listener)` | Listens, and returns what stops listening |

| Event | Told |
|---|---|
| `fact`, `view` | A fact the client did not have, and the view once it holds it |
| `told` | News that is not kept: what the agent says as it writes it (`said`), what is being heard (`hearing`), who speaks, what the agent does |
| `mark`, `audio`, `clear` | The audio of the agent, in runs. `Voice` listens to these |
| `away`, `back` | The line dropped and the client is coming back, and it is on the line again |
| `closed` | The client's part is over: `ended`, `left` when the page let go of the line, `gone` when the client could not come back in time or had nothing to come back with, `refused`, `replaced` when another client took its place |
| `error` | The server could not take something. The connection goes on |
| `message` | Everything the server says that is not audio |

**A line that drops.** The client comes back by itself, with the resume token of its welcome: at once, then after a quarter of a second, a half, one and two, each a fifth more or less so that the clients of one server do not all return together, for as long as the welcome says the session waits (`grace_ms`). It says the number of the last fact it was sent, and is sent the ones after it, or what the session has come to if it was away for more than the session holds. A server that is stopping says `going_away`, and the client comes back to the one that is started after it. A server that is `busy` is tried again. Anything else that is refused is final.

**What is refused before it is sent.** A token of the wrong kind, text longer than a message holds, keys that are no keys: the writers of `protocol.js` throw a `RangeError` for what a server would refuse.

`protocol.js` can be used by itself, by a client that is not this one: `read` and `write`, the writers, and `View`, which applies messages as the Rust `View` does. Both are held to the same fixtures (`crates/core/waveport-protocol/tests/fixtures/client-v1/`).

## The voice

```js
import { Voice } from "./src/audio.js";

const format = await Voice.format();           // { codec: "opus", rate: 48000 }, or pcm16
const voice = await Voice.open(format);        // asks for the microphone
const client = await Client.connect(address, { token, audio: format });
voice.attach(client);                          // at once
voice.muted = true;                            // silence is sent in place of the microphone
voice.stop();                                  // the client goes on
```

- **How audio is written.** As Opus at 48 kHz where the browser codes and decodes Opus itself (WebCodecs), and as plain samples of 16 bits otherwise. A browser's Opus decoder gives 48 kHz whatever it is asked, so Opus is at that rate or not used.
- **What was played.** Audio is played on the audio thread (`worklet.js`), which counts each sample as heard once the output has had the time to sound it: the longest of what the browser says that time is. It says what was played of a run a few times a second, ten at 48 kHz, and when the run has been played to its end, and the client tells the session. That is how the session knows what its far end heard when it is interrupted. When the session discards a run (`clear`), nothing more of it is played, and what had been given to the output by then is said at once.
- **The microphone** is sent 20 ms at a time, silence too: the session hears at the pace it is sent.
- **Echo.** The microphone is asked for with the echo of what is played taken out of it (`echoCancellation: true`), which leaves how much is taken out to the browser. `voice.settings` says what the browser gave. `constraints` in the options of `Voice.open` asks for something else, and `microphone` gives a stream in place of the browser's.

`worklet.js` is loaded by its address, beside `audio.js`. Where a bundler moves it, `Voice.open(format, { worklet })` says where it is.

### Browsers

| Browser | Tested here | Opus through WebCodecs |
|---|---|---|
| Chrome 157 on Linux, headless | In every test run: spoken calls as Opus and as plain samples, a written session, a line that drops | Yes |
| Firefox | Not tested | In its documentation from version 130 on desktop. Not on Android, where audio is plain samples |
| Safari | Not tested | In its documentation from version 26. Plain samples before |

The two rows that are not tested are a reading of the browsers' documentation on 17 November 2026. A browser that cannot do what its row says falls back to plain samples by itself, which every browser with an audio worklet plays.

## The playground

`waveportd` serves a page at `/playground/` that makes a session of one of its agents and talks or writes to it, with this client. It is served by a daemon that listens on this machine alone, unless its configuration says otherwise (`playground = true` or `false`).

```sh
cargo xtask fetch-assets
cargo run -p waveportd --example stand_in -- --listen 127.0.0.1:8080
# then open http://127.0.0.1:8080/playground/
```

That is a daemon whose agent has stand-ins for its providers, and needs no key: every session greets, hears "What time is it right now?" whatever is said, answers with a tone for a voice, and ends. A daemon with real providers serves the same page for its own agents.

Where the daemon has a control key, the page asks for it and sends it to the daemon it came from and nowhere else. It is a page for whoever runs the daemon. An application's own page is given a client token by its server, as above.

`?mic=raw` in the page's address asks for the microphone as it is, without the browser's echo cancellation, noise suppression and gain: for someone with headphones on.

The page and the client are read into the daemon when it is built. A change to either is seen after the daemon is built again.

## Working on it

Node 24 or later, which brings an npm that keeps a version out until it is a week old (`.npmrc`). Nothing is installed to run the client. What is installed is to check it: the TypeScript compiler and the types of Node, each at one version, locked.

```sh
npm ci
npm run check      # the types the comments give
npm test           # the tests that need no browser
cargo build -p waveportd --example stand_in
node test/browser/run.js --daemon ../../target/debug/examples/stand_in
```

`cargo xtask ci --only js` runs all of it, as CI does.

| Tests | What they hold |
|---|---|
| `test/protocol.test.js` | Every message of the Rust fixtures is read and written as it stands, what is unknown is tolerated as the protocol says, and a view is what the facts add up to |
| `test/client.test.js` | The connection, with a server that is the test: the hello, what is told, coming back and when not to |
| `test/worklet.test.js` | What is played and what is said to have been played, sample by sample |
| `test/voice.test.js` | The voice, with a browser's sound stood in for: that it is ready before the client connects and on the line as it is welcomed, what it tells the audio thread, what it sends |
| `test/browser/run.js` | A headless Chrome on the playground of the stand-in daemon, driven over the pipe Chrome offers, with nothing installed to drive it. It is given a recording for a microphone and plays to nothing: no sound is made and no microphone is opened |

A rule of the repository holds every `package.json` under `sdks/` to this: Apache-2.0, nothing to run with, the two tools to check with at exact versions, a lock file whose every entry has its hash, and an `.npmrc` that installs nothing younger than seven days and runs nothing as it installs (`xtask/src/js.rs`).
