// @ts-check
// The voice of a client, with what a browser has for sound stood in
// for: when it is ready, what it tells the audio thread of what the
// session sends, and what it sends of what the microphone hears.
//
// The audio thread itself is in `worklet.test.js`, and a real browser
// with both in `browser/run.js`. Here audio is plain samples: nothing
// in Node codes Opus.

import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";

import { Voice } from "../src/audio.js";
import { Client, Refused } from "../src/client.js";
import { call } from "../src/index.js";
import { last, settle, Socket, WebSocket, WELCOME } from "./support/line.js";

/** What is done, in the order it is done. @type {string[]} */
let order = [];

/** The end of a node on the audio thread that the page holds. */
class Port {
  /** What the page told the audio thread. @type {any[]} */
  posted = [];

  /** @type {((event: { data: unknown }) => void) | null} */
  onmessage = null;

  /** @param {unknown} message */
  postMessage(message) {
    this.posted.push(message);
  }

  /** The audio thread says this. @param {unknown} data */
  says(data) {
    this.onmessage?.({ data });
  }
}

/** A node on the audio thread. */
class WorkletNode {
  port = new Port();

  /**
   * @param {Context} context
   * @param {string} name
   * @param {{ processorOptions?: { frame: number } }} options
   */
  constructor(context, name, options) {
    this.name = name;
    this.options = options;
    context.nodes.push(this);
  }

  /** @param {unknown} to */
  connect(to) {
    return to;
  }
}

/** What plays, as a browser has it. */
class Context {
  /** Every one that was made. @type {Context[]} */
  static made = [];

  /** The rate a browser plays at whatever it is asked, if it has one. @type {number | undefined} */
  static only = undefined;

  /** @type {WorkletNode[]} */
  nodes = [];

  /** @type {string[]} */
  modules = [];

  closed = false;
  resumed = false;
  destination = {};
  // What is given to the output now sounds 30 ms from now, by the time
  // the output says it has reached. The other two say less.
  currentTime = 1;
  outputLatency = 0.02;
  baseLatency = 0.01;

  audioWorklet = {
    addModule: async (/** @type {string | URL} */ module) => {
      order.push("the audio thread is ready");
      this.modules.push(String(module));
    },
  };

  /** @param {{ sampleRate: number }} options */
  constructor(options) {
    this.sampleRate = Context.only ?? options.sampleRate;
    Context.made.push(this);
  }

  getOutputTimestamp() {
    return { contextTime: 0.97, performanceTime: 0 };
  }

  createMediaStreamSource() {
    return { connect: (/** @type {unknown} */ to) => to };
  }

  createGain() {
    return { gain: { value: 1 }, connect: (/** @type {unknown} */ to) => to };
  }

  async resume() {
    order.push("sound may be played");
    this.resumed = true;
  }

  async close() {
    this.closed = true;
  }

  /** The node that plays, and the one that hears. */
  get plays() {
    return /** @type {WorkletNode} */ (this.nodes.find((node) => node.name === "waveport-plays"));
  }

  get hears() {
    return /** @type {WorkletNode} */ (this.nodes.find((node) => node.name === "waveport-hears"));
  }
}

/** A microphone, as a browser gives one. */
class Microphone {
  stopped = false;

  /** What it was asked for. @type {unknown} */
  asked = undefined;

  track = {
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

/** The microphone the browser gives whoever asks for one. */
let given = new Microphone();

/** Whether the page's user lets it have the microphone. */
let allowed = true;

Object.assign(globalThis, { AudioContext: Context, AudioWorkletNode: WorkletNode });
Object.defineProperty(globalThis.navigator, "mediaDevices", {
  configurable: true,
  value: {
    getUserMedia: async (/** @type {unknown} */ constraints) => {
      if (!allowed) {
        throw new Error("the microphone was not allowed");
      }
      order.push("the microphone is there");
      given.asked = constraints;
      return given;
    },
  },
});

/** How the line of these tests is written. */
const PCM = /** @type {const} */ ({ codec: "pcm16", rate: 48_000 });

/** A microphone as the page's own: one it was not given by the browser. */
const own = () => /** @type {MediaStream} */ (/** @type {unknown} */ (new Microphone()));

/** The last context that was made. */
const context = () => /** @type {Context} */ (Context.made.at(-1));

/** What the page told the node that plays, without how long the output takes. */
const told = () => context().plays.port.posted.filter((message) => message.type !== "latency");

/**
 * A spoken call that was welcomed, with the server's end of it.
 *
 * @param {import("../src/audio.js").VoiceOptions | true} [voice]
 */
async function spoken(voice = true) {
  const calling = call("http://127.0.0.1:8080", { token: "wpc1_madeup", voice, WebSocket });
  while (Socket.made.length === 0) {
    await settle();
  }
  const socket = last();
  socket.opens();
  socket.says({ ...WELCOME, audio: PCM });
  // What a server says after its welcome is in the browser's next
  // task: all that waited on the welcome has run by then, and nothing
  // else has.
  await settle();
  return { calling, socket };
}

beforeEach(() => {
  Socket.made = [];
  Context.made = [];
  Context.only = undefined;
  order = [];
  given = new Microphone();
  allowed = true;
  Object.assign(globalThis, { AudioEncoder: undefined, AudioDecoder: undefined });
});

test("a call that is written has no voice, and connects with the token it is given or asks for", async () => {
  for (const token of ["wpc1_madeup", () => "wpc1_madeup", async () => "wpc1_madeup"]) {
    const calling = call("http://127.0.0.1:8080", { token, features: ["said"], WebSocket });
    while (Socket.made.length === 0) {
      await settle();
    }
    const socket = last();
    socket.opens();
    assert.deepEqual(socket.said, [{ type: "hello", v: 1, token: "wpc1_madeup", features: ["said"] }]);
    socket.says(WELCOME);
    const { client, voice } = await calling;
    assert.equal(client.status, "open");
    assert.equal(voice, undefined);
    Socket.made = [];
  }
  assert.equal(Context.made.length, 0);
});

test("a call that is spoken has its voice before it has a line", async () => {
  const token = () => {
    order.push("a token is asked for");
    return "wpc1_madeup";
  };
  const constraints = { noiseSuppression: false };
  const calling = call("http://127.0.0.1:8080", { token, voice: { constraints }, WebSocket });
  while (Socket.made.length === 0) {
    await settle();
  }
  // The token is good for a minute, and is asked for last.
  assert.deepEqual(order, [
    "the audio thread is ready",
    "the microphone is there",
    "sound may be played",
    "a token is asked for",
  ]);
  assert.equal(context().sampleRate, 48_000);
  assert.ok(context().modules[0]?.endsWith("/src/worklet.js"), context().modules[0]);
  // One channel with the echo of what is played taken out of it, and
  // what the page asked for in place of what is asked for otherwise.
  assert.deepEqual(given.asked, {
    audio: { channelCount: 1, echoCancellation: true, noiseSuppression: false, autoGainControl: true },
  });
  // The microphone is cut into 20 ms.
  assert.deepEqual(context().hears.options.processorOptions, { frame: 960 });
  const socket = last();
  socket.opens();
  // Nothing in Node codes Opus, so the line is one of plain samples.
  assert.deepEqual(socket.said, [{ type: "hello", v: 1, token: "wpc1_madeup", audio: PCM }]);
  socket.says({ ...WELCOME, audio: PCM });
  const { client, voice } = await calling;
  assert.equal(client.status, "open");
  assert.deepEqual(voice?.format, PCM);
  assert.equal(voice?.settings.echoCancellation, true);
});

test("what an agent says the moment its far end is welcomed is played", async () => {
  const { calling, socket } = await spoken();
  const samples = Int16Array.of(0, 16_384, -32_768, 32_767);
  socket.says({ type: "mark", run: 0, generation: 1 });
  socket.onmessage?.({ data: samples.buffer });
  socket.says({ type: "mark", run: 0, generation: 1, end: true });
  const posted = context().plays.port.posted;
  // The audio thread is told how long the output takes before a run
  // begins: 30 ms, the longest of what the browser says of it.
  assert.deepEqual(posted[0], { type: "latency", frames: 1440 });
  assert.deepEqual(told(), [
    { type: "begin", run: 0 },
    { type: "audio", run: 0, samples: Float32Array.of(0, 0.5, -1, 32_767 / 32_768) },
    { type: "end", run: 0 },
  ]);
  // What it says it played is said to the session.
  context().plays.port.says([{ run: 0, samples: 2, last: false }]);
  context().plays.port.says([{ run: 0, samples: 4, last: true }]);
  assert.deepEqual(socket.said.slice(1), [
    { type: "played", run: 0, samples: 2 },
    { type: "played", run: 0, samples: 4 },
  ]);
  await calling;
});

test("what the microphone hears is sent as it is heard, and silence in its place when muted", async () => {
  const { calling, socket } = await spoken();
  const { voice } = await calling;
  assert.ok(voice !== undefined);
  const heard = () => Float32Array.of(0, 0.5, -0.5, 1, -1, 2, -2);
  context().hears.port.says(heard());
  assert.deepEqual(socket.sent.at(-1), Int16Array.of(0, 16_384, -16_383, 32_767, -32_767, 32_767, -32_767));
  voice.muted = true;
  context().hears.port.says(heard());
  assert.deepEqual(socket.sent.at(-1), new Int16Array(7));
  voice.muted = false;
  context().hears.port.says(heard());
  assert.equal(/** @type {Int16Array} */ (socket.sent.at(-1))[1], 16_384);
  // One message of audio for each frame, and nothing else.
  assert.equal(socket.sent.length, 4);
});

test("what is discarded is not played, nor what comes of it afterwards", async () => {
  const { calling, socket } = await spoken();
  const { client } = await calling;
  const audio = () => socket.onmessage?.({ data: new Int16Array(960).buffer });
  socket.says({ type: "mark", run: 3, generation: 2 });
  audio();
  socket.says({ type: "clear", run: 3 });
  // Audio that was on its way when the session cut the run.
  audio();
  socket.says({ type: "mark", run: 3, generation: 2, end: true });
  assert.deepEqual(
    told().map(({ type, run }) => [type, run]),
    [
      ["begin", 3],
      ["audio", 3],
      ["clear", 3],
      ["end", 3],
    ],
  );
  // A line that drops takes what was being said with it: the session
  // cuts that where it was heard.
  socket.says({ type: "mark", run: 4, generation: 3 });
  audio();
  socket.drops();
  assert.deepEqual(told().at(-1), { type: "clear", run: 4 });
  // A run that was played to its end is not discarded then.
  assert.equal(told().filter(({ type }) => type === "clear").length, 2);
  // The client would come back by itself: the test is over.
  assert.equal(client.status, "away");
  client.detach();
});

test("a voice is on one line, which is written as the voice writes", async () => {
  const microphone = own();
  const voice = await Voice.open(PCM, { microphone });
  // What the microphone hears before there is a line goes nowhere.
  context().hears.port.says(new Float32Array(960));

  /** @param {object} welcome */
  const client = async (welcome) => {
    const connecting = Client.connect("http://127.0.0.1:8080", { token: "wpc1_madeup", WebSocket });
    last().opens();
    last().says(welcome);
    return connecting;
  };
  assert.throws(() => voice.attach(/** @type {Client} */ ({ audio: undefined })), /a line of text/);
  const coded = await client({ ...WELCOME, audio: { codec: "opus", rate: 48_000 } });
  assert.throws(() => voice.attach(coded), /not written as the voice writes/);
  const slower = await client({ ...WELCOME, audio: { codec: "pcm16", rate: 24_000 } });
  assert.throws(() => voice.attach(slower), /not written as the voice writes/);

  const plain = await client({ ...WELCOME, audio: PCM });
  const socket = last();
  voice.attach(plain);
  assert.throws(() => voice.attach(plain), /on a line already/);
  context().hears.port.says(new Float32Array(960));
  assert.equal(socket.sent.length, 2);

  // A voice that is stopped plays and sends nothing more, and leaves a
  // microphone it was given to whoever gave it.
  voice.stop();
  assert.equal(context().closed, true);
  assert.equal(/** @type {Microphone} */ (/** @type {unknown} */ (microphone)).stopped, false);
  context().hears.port.says(new Float32Array(960));
  socket.says({ type: "mark", run: 0, generation: 1 });
  assert.equal(socket.sent.length, 2);
  assert.deepEqual(told(), []);
  assert.equal(plain.status, "open");
});

test("a session that is over has no voice", async () => {
  const { calling, socket } = await spoken();
  await calling;
  socket.says({ type: "bye", reason: "ended" });
  assert.equal(context().closed, true);
  // The microphone was the voice's own, and is let go of.
  assert.equal(given.stopped, true);
});

test("a call that is not made lets go of what it took", async () => {
  // The hello is refused.
  const refused = call("http://127.0.0.1:8080", { token: "wpc1_madeup", voice: true, WebSocket });
  while (Socket.made.length === 0) {
    await settle();
  }
  last().opens();
  last().says({ type: "error", code: "token", message: "this is no client token" });
  last().says({ type: "bye", reason: "refused" });
  await assert.rejects(refused, Refused);
  assert.equal(context().closed, true);
  assert.equal(given.stopped, true);

  // No token is to be had.
  given = new Microphone();
  const none = () => Promise.reject(new Error("the session was not made"));
  await assert.rejects(call("http://127.0.0.1:8080", { token: none, voice: true, WebSocket }), /not made/);
  assert.equal(context().closed, true);
  assert.equal(given.stopped, true);
  assert.equal(Socket.made.length, 1);

  // The line is one of text: the agent does not speak.
  given = new Microphone();
  const written = call("http://127.0.0.1:8080", { token: "wpc1_madeup", voice: true, WebSocket });
  while (Socket.made.length === 1) {
    await settle();
  }
  last().opens();
  last().says({ ...WELCOME, audio: { codec: "opus", rate: 48_000 } });
  await assert.rejects(written, /not written as the voice writes/);
  assert.equal(given.stopped, true);
  // The session is told that its far end has left.
  assert.deepEqual(last().said.at(-1), { type: "bye" });
});

test("a page that may not have the microphone, or cannot play at the rate, makes no call", async () => {
  allowed = false;
  const token = () => {
    order.push("a token is asked for");
    return "wpc1_madeup";
  };
  await assert.rejects(call("http://127.0.0.1:8080", { token, voice: true, WebSocket }), /not allowed/);
  assert.equal(context().closed, true);
  allowed = true;
  Context.only = 44_100;
  await assert.rejects(call("http://127.0.0.1:8080", { token, voice: true, WebSocket }), /does not play at 48000/);
  assert.equal(context().closed, true);
  // No session was made for either, and nothing was connected to.
  assert.ok(!order.includes("a token is asked for"));
  assert.equal(Socket.made.length, 0);
});

test("audio is written as Opus where the browser codes it at the rate, and as plain samples elsewhere", async () => {
  assert.deepEqual(await Voice.format(), PCM);
  /** What a browser is asked whether it can do. @type {any[]} */
  const asked = [];
  /** @param {boolean | Error} can */
  const coder = (can) => ({
    isConfigSupported: async (/** @type {unknown} */ config) => {
      asked.push(config);
      if (can instanceof Error) {
        throw can;
      }
      return { supported: can };
    },
  });
  /**
   * @param {boolean | Error} codes
   * @param {boolean | Error} decodes
   */
  const browser = (codes, decodes) => {
    // A coder is a class in a browser, and a function to whoever asks.
    const AudioEncoder = Object.assign(() => {}, coder(codes));
    const AudioDecoder = Object.assign(() => {}, coder(decodes));
    Object.assign(globalThis, { AudioEncoder, AudioDecoder });
  };
  browser(true, true);
  assert.deepEqual(await Voice.format(), { codec: "opus", rate: 48_000 });
  assert.deepEqual(asked, [
    { codec: "opus", sampleRate: 48_000, numberOfChannels: 1, bitrate: 32_000 },
    { codec: "opus", sampleRate: 48_000, numberOfChannels: 1 },
  ]);
  // A browser's decoder gives 48 kHz whatever it is asked, so Opus is
  // at that rate or not at all.
  assert.deepEqual(await Voice.format(24_000), { codec: "pcm16", rate: 24_000 });
  // It has to do both, and to be able to say so.
  browser(true, false);
  assert.deepEqual(await Voice.format(), PCM);
  browser(false, true);
  assert.deepEqual(await Voice.format(), PCM);
  browser(new TypeError("no such codec"), true);
  assert.deepEqual(await Voice.format(), PCM);
});
