// @ts-check
// The voice of a client on a call, with what a browser has for calls
// and for sound stood in for: what it plays, what it says was played,
// what it keeps from being heard, and what a call that is not made is
// made over instead.
//
// What a place in a call is, with the time in the test's hands, is in
// `playing.test.js`. Here the time is the machine's, and the tests wait
// for what the voice does. A real browser on a real call is in
// `browser/run.js`.

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

import { Voice } from "../src/audio.js";
import { CallVoice, Unreached, call } from "../src/index.js";
import { Daemon, Peer, RTCPeerConnection, lastCall } from "./support/call.js";
import { Socket, WebSocket, WELCOME, last, settle } from "./support/line.js";

const TOKEN = "wpc1_madeup";
const AT = "http://127.0.0.1:7070";

/** What is done, in the order it is done. @type {string[]} */
let order = [];

/** A microphone, as a browser gives one. */
class Microphone {
  stopped = false;

  /** What it was asked for. @type {unknown} */
  asked = undefined;

  track = {
    kind: "audio",
    enabled: true,
    stop: () => {
      this.stopped = true;
    },
    getSettings: () => ({ echoCancellation: true, sampleRate: 48_000 }),
  };

  getTracks() {
    return [this.track];
  }

  getAudioTracks() {
    return [this.track];
  }
}

/** What plays a call, as a page has it. */
class Element {
  /** Every one that was made. @type {Element[]} */
  static made = [];

  /** What a browser that plays nothing for a page nobody touched says. @type {Error | undefined} */
  static refuses = undefined;

  autoplay = false;
  muted = false;
  /** What it plays. @type {{ tracks: unknown[] } | null} */
  srcObject = null;
  plays = 0;

  constructor() {
    Element.made.push(this);
  }

  async play() {
    this.plays += 1;
    if (Element.refuses !== undefined) {
      throw Element.refuses;
    }
  }
}

/** Tracks together, as a browser plays them. */
class Stream {
  /** @param {unknown[]} tracks */
  constructor(tracks) {
    this.tracks = tracks;
  }
}

/** What a browser has for sound on a line, as far as a voice asks. */
class Node {
  port = { postMessage() {}, onmessage: null };

  /** @param {unknown} to */
  connect(to) {
    return to;
  }
}

class Context {
  sampleRate = 48_000;
  destination = {};
  audioWorklet = { addModule: async () => {} };

  createMediaStreamSource() {
    return { connect: (/** @type {unknown} */ to) => to };
  }

  createGain() {
    return { gain: { value: 1 }, connect: (/** @type {unknown} */ to) => to };
  }

  async resume() {}

  async close() {}
}

/** The microphone the browser gives whoever asks for one. */
let given = new Microphone();

/** The calls of a test, which are over when it is. @type {{ client: import("../src/client.js").Client, voice?: { stop(): void } }[]} */
let made = [];

/** How often it was asked for. */
let asked = 0;

/** @type {Daemon} */
let daemon = new Daemon();

Object.assign(globalThis, { Audio: Element, MediaStream: Stream, AudioContext: Context, AudioWorkletNode: Node });
Object.defineProperty(globalThis.navigator, "mediaDevices", {
  configurable: true,
  value: {
    getUserMedia: async (/** @type {unknown} */ constraints) => {
      order.push("the microphone is there");
      asked += 1;
      given.asked = constraints;
      return given;
    },
  },
});

beforeEach(() => {
  Peer.made = [];
  Peer.refusesServers = undefined;
  Socket.made = [];
  Element.made = [];
  Element.refuses = undefined;
  order = [];
  given = new Microphone();
  asked = 0;
  daemon = new Daemon();
});

afterEach(() => {
  // Nothing of a test goes on looking at a browser after it.
  for (const { client, voice } of made.splice(0)) {
    voice?.stop();
    client.bye();
  }
});

/** What plays the call of these tests. */
const element = () => /** @type {Element} */ (Element.made.at(-1));

/**
 * Waits until `there` holds, for as long as a test may.
 *
 * @param {() => boolean} there
 * @param {string} what
 */
async function until(there, what) {
  const began = performance.now();
  while (!there()) {
    assert.ok(performance.now() - began < 5_000, `it did not come about: ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

/**
 * What a spoken call is asked for beside its token.
 *
 * @param {object} [more]
 */
const spoken = (more = {}) => ({
  token: TOKEN,
  voice: /** @type {const} */ (true),
  RTCPeerConnection,
  WebSocket,
  fetch: daemon.fetch,
  random: () => 0.5,
  ...more,
});

/** A spoken call that was made and welcomed, with the server's end of it. */
async function onACall(/** @type {object} */ more = {}) {
  const calling = call(AT, spoken(more));
  while (Peer.made.length === 0) {
    await settle();
  }
  await settle();
  const peer = lastCall();
  peer.channel?.opens();
  peer.channel?.says(WELCOME);
  const { client, voice } = await calling;
  made.push({ client, voice });
  assert.ok(voice instanceof CallVoice);
  return { client, voice, peer, channel: /** @type {import("./support/call.js").Channel} */ (peer.channel) };
}

/** What the client said it played, of what it said on the call. */
const played = (/** @type {import("./support/call.js").Channel} */ channel) =>
  channel.said.filter((said) => said.type === "played_rtp");

test("a call that is spoken is made over WebRTC, with its voice before its offer", async () => {
  const token = () => {
    order.push("a token is asked for");
    return TOKEN;
  };
  const calling = call(AT, spoken({ token, voice: { constraints: { noiseSuppression: false } } }));
  while (Peer.made.length === 0) {
    await settle();
  }
  await settle();
  // The token is good for a minute, and is asked for last.
  assert.deepEqual(order, ["the microphone is there", "a token is asked for"]);
  assert.deepEqual(given.asked, {
    audio: { channelCount: 1, echoCancellation: true, noiseSuppression: false, autoGainControl: true },
  });
  const peer = lastCall();
  assert.equal(daemon.asked[0]?.url, "http://127.0.0.1:7070/v1/webrtc");
  assert.equal(daemon.asked[0]?.headers.authorization, `Bearer ${TOKEN}`);
  // The call sends what the microphone hears.
  assert.equal(peer.sends, given.track);
  peer.channel?.opens();
  // No audio is spoken of in the hello: a call says how its own is
  // written.
  assert.deepEqual(peer.channel?.said, [{ type: "hello", v: 1, token: TOKEN }]);
  peer.channel?.says(WELCOME);
  const { client, voice } = await calling;
  made.push({ client, voice });
  assert.ok(voice instanceof CallVoice);
  assert.equal(client.over, "webrtc");
  assert.deepEqual(voice.format, { codec: "opus", rate: 48_000 });
  assert.equal(voice.settings.echoCancellation, true);
  // What the call carries is played, by the time the welcome is taken.
  assert.deepEqual(element().srcObject?.tracks, [peer.receiver.track]);
  assert.equal(element().autoplay, true);
  assert.equal(element().plays, 1);
  assert.equal(element().muted, false);
  // No connection was made beside the call, and nothing was asked of
  // the browser twice.
  assert.equal(Socket.made.length, 0);
  assert.equal(asked, 1);
  voice.stop();
});

test("what a session says once its far end is welcomed reaches the voice of the call", async () => {
  const calling = call(AT, spoken());
  while (Peer.made.length === 0) {
    await settle();
  }
  await settle();
  const peer = lastCall();
  peer.channel?.opens();
  peer.channel?.says(WELCOME);
  // A browser gives a page what arrives next once the page has done
  // with what arrived before, and no later: an agent that greets and
  // is cut off has said both by then.
  await settle();
  peer.channel?.says({ type: "mark", run: 0, generation: 1, ts: 48_000 });
  peer.channel?.says({ type: "clear", run: 0 });
  assert.equal(element().muted, true);
  const { client, voice } = await calling;
  made.push({ client, voice });
  assert.ok(voice instanceof CallVoice);
  assert.equal(voice.silenced, true);
});

test("where the browser is in a call is said to the session as it gets there", async () => {
  const { voice, peer, channel } = await onACall();
  assert.deepEqual(played(channel), []);
  // The browser gave the packet at 48,000 to be played half a second
  // ago: it played all of it.
  peer.receiver.sources = [{ rtpTimestamp: 48_000, timestamp: performance.now() - 500 }];
  await until(() => played(channel).length === 1, "what was played is said");
  assert.deepEqual(played(channel), [{ type: "played_rtp", ts: 48_960, delay_ms: 0 }]);
  // Of what a browser says of more than one source, the newest holds.
  peer.receiver.sources = [
    { rtpTimestamp: 48_000, timestamp: performance.now() - 500 },
    { rtpTimestamp: 96_000, timestamp: performance.now() - 400 },
  ];
  await until(() => played(channel).length === 2, "what was played next is said");
  assert.deepEqual(played(channel).at(-1), { type: "played_rtp", ts: 96_960, delay_ms: 0 });
  // A place is said once.
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(played(channel).length, 2);
  voice.stop();
  // A voice that was stopped says nothing more.
  peer.receiver.sources = [{ rtpTimestamp: 144_000, timestamp: performance.now() - 400 }];
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(played(channel).length, 2);
});

test("how long the output takes is said with what was played", async () => {
  const { voice, peer, channel } = await onACall();
  // The output took 38 ms over each sample it was given.
  peer.receiver.stats = [
    { type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 4_800 },
    { type: "media-playout", totalPlayoutDelay: 182.4, totalSamplesCount: 4_800 },
  ];
  await new Promise((resolve) => setTimeout(resolve, 80));
  peer.receiver.sources = [{ rtpTimestamp: 48_000, timestamp: performance.now() - 500 }];
  await until(() => played(channel).length === 1, "what was played is said");
  assert.deepEqual(played(channel), [{ type: "played_rtp", ts: 48_960, delay_ms: 38 }]);
  // Asked again a second later, it took 20 ms over each sample since:
  // that is what is said from then on, and not the 29 ms it has taken
  // over each since the call began.
  peer.receiver.stats = [
    { type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 9_600 },
    { type: "media-playout", totalPlayoutDelay: 278.4, totalSamplesCount: 9_600 },
  ];
  let ts = 48_000;
  await until(() => {
    ts += 960;
    peer.receiver.sources = [{ rtpTimestamp: ts, timestamp: performance.now() - 500 }];
    return played(channel).at(-1)?.delay_ms !== 38;
  }, "the output is asked about again");
  assert.equal(played(channel).at(-1)?.delay_ms, 20);
  voice.stop();
});

test("a browser that cannot say where it is says how much has come out of its buffer", async () => {
  const { voice, peer, channel } = await onACall();
  Object.assign(peer.receiver, { getSynchronizationSources: undefined });
  peer.receiver.stats = [{ type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 4_800 }];
  await until(() => played(channel).length === 1, "what came out is said");
  assert.deepEqual(played(channel), [{ type: "played_rtp", emitted: 4_800, delay_ms: 0 }]);
  // What did not change is not said again.
  await new Promise((resolve) => setTimeout(resolve, 250));
  assert.equal(played(channel).length, 1);
  peer.receiver.stats = [{ type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 9_600 }];
  await until(() => played(channel).length === 2, "what came out since is said");
  assert.deepEqual(played(channel).at(-1), { type: "played_rtp", emitted: 9_600, delay_ms: 0 });
  // What comes out while the browser is kept from being heard was not
  // played to anybody, and is not said while it is.
  channel.says({ type: "clear", run: 0 });
  peer.receiver.stats = [{ type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 14_400 }];
  await new Promise((resolve) => setTimeout(resolve, 250));
  assert.equal(voice.silenced, true);
  assert.equal(played(channel).length, 2);
  voice.stop();
});

test("what a browser says of a call that is gone is not said on the call that follows it", async () => {
  const { client, peer } = await onACall();
  // A browser that takes its time to say what came out.
  Object.assign(peer.receiver, { getSynchronizationSources: undefined });
  let reads = 0;
  /** @type {(stats: Map<string, object>) => void} */
  let answers = () => {};
  /** @type {Promise<Map<string, object>>} */
  const slow = new Promise((resolve) => {
    answers = resolve;
  });
  peer.receiver.getStats = () => {
    reads += 1;
    return slow;
  };
  await until(() => reads > 0, "the browser is asked what came out");
  // The call drops, and the client is on another before the browser
  // has answered.
  peer.becomes("disconnected");
  await until(() => lastCall() !== peer, "the client offers another call");
  await settle();
  const again = lastCall();
  const channel = /** @type {import("./support/call.js").Channel} */ (again.channel);
  Object.assign(again.receiver, { getSynchronizationSources: undefined });
  channel.opens();
  channel.says({ ...WELCOME, resumed: true, resume_token: "wpr1_another" });
  await settle();
  assert.equal(client.status, "open");
  answers(new Map([["s0", { type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 48_000 }]]));
  await settle();
  assert.deepEqual(played(channel), []);
  // What came out of the call the client is on is said, from its
  // first sample.
  again.receiver.stats = [{ type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 960 }];
  await until(() => played(channel).length > 0, "what came out of this call is said");
  assert.deepEqual(played(channel), [{ type: "played_rtp", emitted: 960, delay_ms: 0 }]);
});

test("what a browser holds of a response that is discarded is not heard", async () => {
  const { voice, peer, channel } = await onACall();
  const now = () => performance.now();
  channel.says({ type: "mark", run: 3, generation: 2, ts: 48_000 });
  peer.receiver.sources = [{ rtpTimestamp: 48_000, timestamp: now() - 500 }];
  await until(() => played(channel).length === 1, "what was played is said");
  assert.equal(element().muted, false);
  assert.equal(voice.silenced, false);
  // The session cuts the response: the browser is muted at once, and
  // what it goes on to play of the response is not said to have been
  // played.
  channel.says({ type: "clear", run: 3 });
  assert.equal(element().muted, true);
  assert.equal(voice.silenced, true);
  peer.receiver.sources = [{ rtpTimestamp: 52_800, timestamp: now() - 500 }];
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(played(channel).length, 1);
  assert.equal(element().muted, true);
  // The response that follows is heard from where it begins.
  channel.says({ type: "mark", run: 4, generation: 3, ts: 96_000 });
  peer.receiver.sources = [{ rtpTimestamp: 96_000, timestamp: now() - 500 }];
  await until(() => !element().muted, "what follows is heard");
  assert.equal(voice.silenced, false);
  await until(() => played(channel).length === 2, "what was played of it is said");
  assert.deepEqual(played(channel).at(-1), { type: "played_rtp", ts: 96_960, delay_ms: 0 });
  voice.stop();
});

test("what a browser held is gone a while after it was told to discard it", async () => {
  const { voice, peer, channel } = await onACall();
  peer.receiver.sources = [{ rtpTimestamp: 48_000, timestamp: performance.now() - 500 }];
  await until(() => played(channel).length === 1, "what was played is said");
  const cut = performance.now();
  channel.says({ type: "clear", run: 0 });
  assert.equal(element().muted, true);
  // Nothing follows: the browser is heard again once what it held has
  // come out, a quarter of a second after it last moved.
  await until(() => !element().muted, "the browser is heard again");
  assert.ok(performance.now() - cut >= 240, `it was heard again after ${performance.now() - cut} ms`);
  assert.equal(voice.silenced, false);
  voice.stop();
});

test("a microphone that is muted sends silence, and one that is the voice's is let go of with it", async () => {
  const { voice } = await onACall();
  assert.equal(voice.muted, false);
  voice.muted = true;
  assert.equal(given.track.enabled, false);
  assert.equal(voice.muted, true);
  voice.muted = false;
  assert.equal(given.track.enabled, true);
  assert.equal(given.stopped, false);
  voice.stop();
  assert.equal(given.stopped, true);
  assert.equal(element().srcObject, null);

  // One the page gave is the page's to let go of.
  const own = new Microphone();
  const { voice: second } = await onACall({ voice: { microphone: own } });
  assert.equal(asked, 1);
  second.stop();
  assert.equal(own.stopped, false);
});

test("a voice follows its client from a call that dropped to the one it comes back on", async () => {
  const { client, voice, peer, channel } = await onACall();
  peer.receiver.sources = [{ rtpTimestamp: 48_000, timestamp: performance.now() - 500 }];
  await until(() => played(channel).length === 1, "what was played is said");
  channel.says({ type: "clear", run: 0 });
  peer.becomes("disconnected");
  assert.equal(client.status, "away");
  // What the browser held of a call that is gone is not played.
  assert.equal(element().srcObject, null);
  await until(() => lastCall() !== peer, "the client offers another call");
  await settle();
  const again = lastCall();
  // The call it comes back on sends the same microphone.
  assert.equal(again.sends, given.track);
  assert.equal(asked, 1);
  again.channel?.opens();
  again.channel?.says({ ...WELCOME, resumed: true, resume_token: "wpr1_another" });
  await settle();
  assert.equal(client.status, "open");
  assert.deepEqual(element().srcObject?.tracks, [again.receiver.track]);
  // A new call is a new stream: nothing is kept from being heard for
  // what was discarded on the one before, and places are said afresh.
  assert.equal(element().muted, false);
  again.receiver.sources = [{ rtpTimestamp: 960, timestamp: performance.now() - 500 }];
  await until(() => played(/** @type {any} */ (again.channel)).length === 1, "what was played is said on it");
  assert.deepEqual(played(/** @type {any} */ (again.channel)), [{ type: "played_rtp", ts: 1_920, delay_ms: 0 }]);
  // The session ends: the voice stops with it.
  again.channel?.says({ type: "bye", reason: "ended" });
  await settle();
  assert.equal(given.stopped, true);
  assert.equal(element().srcObject, null);
  assert.equal(voice.silenced, false);
});

test("a call that the server does not take is made over a WebSocket, with the same token and microphone", async () => {
  let tokens = 0;
  const token = () => {
    tokens += 1;
    return TOKEN;
  };
  daemon.answers = () => Daemon.problem(404, "unsupported", "the daemon answers no calls over WebRTC");
  const calling = call(AT, spoken({ token }));
  while (Socket.made.length === 0) {
    await settle();
  }
  // The offer was made and turned away, and nothing was said on a call.
  assert.equal(daemon.asked.length, 1);
  assert.equal(lastCall().closed, true);
  const socket = last();
  assert.equal(socket.url, "ws://127.0.0.1:7070/v1/client");
  socket.opens();
  assert.equal(socket.said[0]?.token, TOKEN);
  assert.equal(socket.said[0]?.audio?.rate, 48_000);
  socket.says({ ...WELCOME, audio: socket.said[0]?.audio });
  const { client, voice } = await calling;
  made.push({ client, voice });
  assert.ok(voice instanceof Voice);
  assert.equal(client.over, "websocket");
  // A token is good once, and the page's user was asked for the
  // microphone once.
  assert.equal(tokens, 1);
  assert.equal(asked, 1);
  assert.equal(given.stopped, false);
  // The microphone went with the voice that took it over.
  voice.stop();
  assert.equal(given.stopped, true);
});

test("a call that has to be over WebRTC is not made over anything else", async () => {
  daemon.answers = () => Daemon.problem(404, "unsupported", "the daemon answers no calls over WebRTC");
  await assert.rejects(call(AT, spoken({ over: "webrtc" })), Unreached);
  assert.equal(Socket.made.length, 0);
  // The microphone is let go of with the call that was not made.
  assert.equal(given.stopped, true);
});

test("a call that is to be over a WebSocket, or to a line for clients alone, offers nothing", async () => {
  for (const [at, more] of /** @type {const} */ ([
    [AT, { over: "websocket" }],
    ["ws://127.0.0.1:7070/v1/client", {}],
    [AT, { RTCPeerConnection: undefined }],
  ])) {
    Socket.made = [];
    const calling = call(at, spoken(more));
    calling.catch(() => {});
    const began = performance.now();
    while (Socket.made.length === 0 && performance.now() - began < 2_000) {
      await settle();
    }
    assert.equal(Socket.made.length, 1, at);
    assert.equal(Peer.made.length, 0, at);
    assert.equal(daemon.asked.length, 0, at);
    last().drops();
    await assert.rejects(calling);
  }
});

test("a hello that is refused on a call is not said again over a WebSocket", async () => {
  const calling = call(AT, spoken());
  calling.catch(() => {});
  while (Peer.made.length === 0) {
    await settle();
  }
  await settle();
  const peer = lastCall();
  peer.channel?.opens();
  peer.channel?.says({ type: "error", code: "token", message: "the token is not good" });
  peer.channel?.says({ type: "bye", reason: "error" });
  await assert.rejects(calling, { name: "Refused", code: "token" });
  assert.equal(Socket.made.length, 0);
  assert.equal(given.stopped, true);
});

test("a browser that plays nothing for a page nobody touched says so to whoever asked", async () => {
  Element.refuses = new Error("play() failed because the user didn't interact with the document first");
  /** @type {Error[]} */
  const errors = [];
  const { voice } = await onACall({ voice: { onerror: (/** @type {Error} */ error) => errors.push(error) } });
  await settle();
  assert.equal(errors.length, 1);
  assert.match(String(errors[0]?.message), /interact/);
  voice.stop();
});
