# The stock Pipecat client, held against the daemon

This folder is a test and ships nothing. It is the proof of one claim: an application written for Pipecat's JavaScript client talks to a `waveportd` by being given its address ([decision 0006](../../docs/decisions/0006-rtvi-gateway.md)).

A page (`page/stock.js`) holds `@pipecat-ai/client-js` with `@pipecat-ai/small-webrtc-transport`, both as they are published, and nothing of Waveport's. `test/run.js` bundles the page as an application's build would, serves it from an address of its own, starts the daemon with stand-ins for its providers, and drives a headless Chrome through four sessions:

| Step | What the client does | What is held |
|---|---|---|
| A call that is spoken | `connect({ webrtcRequestParams: { endpoint: ".../api/offer" } })` with a microphone | It is ready in RTVI 2.3.0, is told the greeting, hears audio, is told the question as it is recognised and the answer from the model to the last word heard, with the tool the agent called by its name alone, and then that its bot left. It ends its call and makes no other |
| A session that is started | `startBotAndConnect({ endpoint: ".../start" })`, then what it does when its network fails, then `sendText` | The offer goes under the name `/start` gave. The second call is for the session of the first, the client stays ready, and it is told nothing again of what the agent said before. What is written comes back as `chat`, with the id of its message |
| A client token, and keys that are pressed | `connect` with `headers` that say a client token as `Authorization: Bearer`, then `sendDTMF("12#")` | It is on the session the token was made for, the token is used up by its coming, and the session takes a turn of keys |
| A bot that is told to leave | `disconnectBot()` | The client is told that its bot left, and the session is over |

```sh
cargo xtask fetch-assets
cargo build --locked -p waveportd --example stand_in
cd sdks/pipecat-stock
npm ci
node test/run.js --daemon ../../target/debug/examples/stand_in
```

`cargo xtask ci --only js` runs it after the tests of the client of ours. `--notes` prints what the page wrote to its console, and `--only <words>` runs the steps whose names have the words in them.

## What is the client's and what is not

- **The client and its transport are unchanged.** The versions are the ones `package-lock.json` names. Every night the `scheduled` workflow runs the same steps with the newest of both that is a week old, and prints which it had: that run says when the client has moved on from what is pinned here.
- **The page is served from another address than the daemon's**, as an application is. The daemon is told of that address (`allowed_origins`), and a page from anywhere else is refused.
- **The transport is given the other of its two ways to open devices** (`WavMediaManager`, which it exports). The one it takes when it is told nothing fetches a program from its maker's servers when it starts, and tells a third party when that fails. A test here asks nothing of anybody beyond the machine, so the browser is started with no name that leads anywhere, and with that the default never gets as far as an offer. From the offer on, everything is the same with either.
- **What the client does when its network fails is called by name** (`attemptReconnection(true)`, which is what its transport calls when its checks fail). No step breaks a network. The client comes back after the greeting and before any turn, since the stand-in ends a call with its one answer: **that a client which comes back after a turn is told nothing twice is held in the daemon's tests** and not here.
- **What the client does not look at is not held here.** It takes `bot-ready` whichever message that answers. That the answer names the message it answers is held where the mapping is tested (`apps/waveportd/src/rtvi.rs`).
- **An interruption is not tried in the browser.** The daemon's own tests hold it (`apps/waveportd/tests/served/pipecat.rs`), with a client that writes while it is answered.

## What it is installed with

Nothing here runs outside a test. The three packages in `package.json` are pinned to one version each, locked with what they hash to, installed with no script of theirs run, and none younger than a week (`.npmrc`). What they bring with them is listed by name in `xtask/src/js.rs`, and a test there fails when the list and the lock file differ: a name that is not listed is something the client took on since, and is looked at before it is installed.
