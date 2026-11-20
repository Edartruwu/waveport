// @ts-check
// A client on a line, with a server that is the test: what it says as
// it connects, what it tells of what it is told, and how it comes back.

import assert from "node:assert/strict";
import { afterEach, beforeEach, mock, test } from "node:test";

import { Client, lineOf, Refused } from "../src/client.js";
import { NAME } from "../src/protocol.js";
import { last, settle, Socket, WebSocket, WELCOME } from "./support/line.js";

/**
 * A client that was welcomed.
 *
 * @param {import("../src/client.js").Options} [options]
 * @param {object} [welcome]
 */
async function welcomed(options = {}, welcome = WELCOME) {
  const connecting = Client.connect("http://127.0.0.1:8080", {
    token: "wpc1_madeup",
    features: ["said"],
    WebSocket,
    random: () => 0.5,
    ...options,
  });
  const socket = last();
  socket.opens();
  socket.says(welcome);
  const client = await connecting;
  /** Everything the client told, in order. @type {[string, unknown][]} */
  const told = [];
  for (const type of /** @type {const} */ ([
    "fact",
    "told",
    "view",
    "mark",
    "clear",
    "audio",
    "error",
    "away",
    "back",
    "closed",
  ])) {
    client.on(type, (/** @type {unknown} */ what) => told.push([type, what]));
  }
  return { client, socket, told };
}

beforeEach(() => {
  Socket.made = [];
  mock.timers.enable({ apis: ["setTimeout", "Date"] });
});

afterEach(() => {
  mock.timers.reset();
});

test("a client says hello on the line of the server and is welcomed", async () => {
  const audio = /** @type {const} */ ({ codec: "opus", rate: 48000 });
  const { client, socket } = await welcomed({ audio });
  assert.equal(socket.url, "ws://127.0.0.1:8080/v1/client");
  assert.equal(socket.named, NAME);
  assert.equal(socket.binaryType, "arraybuffer");
  assert.deepEqual(socket.said, [
    { type: "hello", v: 1, token: "wpc1_madeup", audio, features: ["said"] },
  ]);
  assert.equal(client.status, "open");
  assert.equal(client.session, WELCOME.session);
  assert.deepEqual(client.audio, audio);
  assert.deepEqual(client.resume, { token: "wpr1_madeup", life: 1, seen: 0 });
});

test("the address of a line is the server's, and says nothing of who connects", () => {
  assert.equal(lineOf("https://agents.example"), "wss://agents.example/v1/client");
  assert.equal(lineOf("ws://127.0.0.1:9/v1/client"), "ws://127.0.0.1:9/v1/client");
  assert.equal(lineOf("wss://agents.example/behind/a/proxy"), "wss://agents.example/behind/a/proxy");
  assert.equal(lineOf(new URL("http://[::1]:8080/#top")), "ws://[::1]:8080/v1/client");
  for (const none of [
    "ws://127.0.0.1:9/v1/client?token=wpc1_madeup",
    "wss://user:wpc1_madeup@agents.example",
    "ftp://agents.example",
  ]) {
    assert.throws(() => lineOf(none), RangeError, none);
  }
});

test("a hello that is refused is no client", async () => {
  const connecting = Client.connect("ws://127.0.0.1:8080", { token: "wpc1_madeup", WebSocket });
  const socket = last();
  socket.opens();
  socket.says({ type: "error", code: "token", message: "this is no client token" });
  socket.says({ type: "bye", reason: "refused" });
  await assert.rejects(connecting, (error) => {
    assert.ok(error instanceof Refused);
    assert.equal(error.code, "token");
    assert.equal(error.message, "this is no client token");
    return true;
  });
  assert.equal(socket.closed, true);

  // Nor is one that is not answered, or whose connection ends first.
  const unanswered = Client.connect("ws://127.0.0.1:8080", { WebSocket, welcomeWithinMs: 500 });
  last().opens();
  mock.timers.tick(499);
  assert.equal(last().closed, false);
  mock.timers.tick(1);
  await assert.rejects(unanswered, /did not answer/);
  assert.equal(last().closed, true);

  const cut = Client.connect("ws://127.0.0.1:8080", { WebSocket });
  last().drops();
  await assert.rejects(cut, /closed before/);

  // What a server would refuse is not said to it: nothing is connected
  // to for it.
  const unsaid = Client.connect("ws://127.0.0.1:8080", { token: "nothing", WebSocket });
  assert.equal(Socket.made.length, 3);
  await assert.rejects(unsaid, RangeError);
});

test("a client tells what it is told, and holds what the facts add up to", async () => {
  const { client, socket, told } = await welcomed();
  const joined = { type: "fact", seq: 1, kind: "participant_joined", who: 0 };
  const said = { type: "told", kind: "said", generation: 1, text: "Good morning." };
  const greeted = {
    type: "fact",
    seq: 2,
    kind: "utterance_committed",
    generation: 1,
    agent: 0,
    text: "Good morning.",
    truncated: false,
  };
  socket.says(joined);
  socket.says(said);
  socket.says(greeted);
  // A fact that is sent again changes nothing, and is not told again.
  socket.says(greeted);
  assert.deepEqual(
    told.map(([type]) => type),
    ["fact", "view", "told", "fact", "view"],
  );
  assert.deepEqual(told[3]?.[1], greeted);
  assert.equal(client.view.seen, 2);
  assert.deepEqual(client.view.state.items, [
    { kind: "utterance", generation: 1, agent: 0, text: "Good morning.", truncated: false },
  ]);
  assert.deepEqual(client.resume, { token: "wpr1_madeup", life: 1, seen: 2 });

  // Audio, and what says whose it is.
  told.length = 0;
  const audio = new Uint8Array([1, 2, 3]).buffer;
  socket.says({ type: "mark", run: 1, generation: 2 });
  socket.onmessage?.({ data: audio });
  socket.says({ type: "mark", run: 1, generation: 2, end: true });
  socket.says({ type: "clear", run: 1 });
  assert.deepEqual(told, [
    ["mark", { type: "mark", run: 1, generation: 2 }],
    ["audio", audio],
    ["mark", { type: "mark", run: 1, generation: 2, end: true }],
    ["clear", { type: "clear", run: 1 }],
  ]);

  // What cannot be taken is told, and the connection goes on.
  told.length = 0;
  socket.says({ type: "error", code: "malformed", message: "nothing is written" });
  socket.onmessage?.({ data: "{" });
  socket.says({ type: "metrics", cpu: 0.5 });
  assert.deepEqual(told, [
    ["error", { code: "malformed", message: "nothing is written" }],
    ["error", { code: "malformed", message: "a message is JSON" }],
  ]);
  assert.equal(client.status, "open");
  assert.equal(socket.closed, false);
});

test("a client says what its user does", async () => {
  const { client, socket } = await welcomed();
  client.text("What time is it?");
  client.dtmf("4071#");
  client.interrupt();
  client.played(3, 28_800);
  const audio = new Uint8Array([9, 9]);
  client.send(audio);
  assert.deepEqual(socket.said.slice(1), [
    { type: "text", text: "What time is it?" },
    { type: "dtmf", keys: "4071#" },
    { type: "interrupt" },
    { type: "played", run: 3, samples: 28_800 },
  ]);
  assert.equal(socket.sent.at(-1), audio);
  assert.throws(() => client.text("  "), RangeError);
  assert.throws(() => client.dtmf("12E"), RangeError);
});

test("a client whose line drops comes back with where it was", async () => {
  const { client, socket, told } = await welcomed({ audio: { codec: "pcm16", rate: 24000 } });
  socket.says({ type: "fact", seq: 4, kind: "participant_joined", who: 0 });
  told.length = 0;
  socket.drops();
  assert.deepEqual(told, [["away", { reason: "dropped" }]]);
  assert.equal(client.status, "away");

  // What is written meanwhile waits. What is only of the moment does not.
  client.text("Are you there?");
  client.interrupt();
  client.played(1, 480);
  client.send(new Uint8Array(4));

  mock.timers.tick(0);
  const again = last();
  assert.notEqual(again, socket);
  again.opens();
  // It names no client token: that one is used up. It says the life it
  // was told of and the last fact it has.
  assert.deepEqual(again.said, [
    {
      type: "hello",
      v: 1,
      audio: { codec: "pcm16", rate: 24000 },
      features: ["said"],
      resume: { token: "wpr1_madeup", life: 1, seen: 4 },
    },
  ]);
  const back = { ...WELCOME, resumed: true, next_seq: 6 };
  delete (/** @type {any} */ (back).resume_token);
  again.says(back);
  await settle();
  assert.deepEqual(told, [
    ["away", { reason: "dropped" }],
    ["back", back],
  ]);
  assert.equal(client.status, "open");
  assert.deepEqual(again.said.slice(1), [{ type: "text", text: "Are you there?" }]);
  assert.equal(again.sent.length, 2);
  // The welcome named no token: the one it has is still good.
  assert.deepEqual(client.resume, { token: "wpr1_madeup", life: 1, seen: 4 });

  // The session went on meanwhile, and what it did is told.
  again.says({ type: "fact", seq: 5, kind: "participant_left", who: 0 });
  assert.equal(client.view.seen, 5);
  // Nothing of the connection before is heard any more.
  socket.says({ type: "fact", seq: 9, kind: "participant_joined", who: 7 });
  assert.equal(client.view.seen, 5);
});

test("what waits for a line that is away is sent in order when it is back, and is not without end", async () => {
  const { client, socket } = await welcomed();
  socket.drops();
  for (let n = 0; n < 32; n += 1) {
    client.text(`Line ${n}`);
  }
  // A line that is away for long is not written to for ever.
  assert.throws(() => client.dtmf("1"), /too much waits/);
  assert.throws(() => client.text("One more."), /too much waits/);
  mock.timers.tick(0);
  const again = last();
  again.opens();
  again.says({ ...WELCOME, resumed: true });
  await settle();
  assert.deepEqual(
    again.said.slice(1),
    Array.from({ length: 32 }, (_, n) => ({ type: "text", text: `Line ${n}` })),
  );
  // Once it is back there is room again.
  client.dtmf("1");
  assert.deepEqual(again.said.at(-1), { type: "dtmf", keys: "1" });
});

test("a client keeps trying for as long as the session waits", async () => {
  const { client, socket, told } = await welcomed({}, { ...WELCOME, grace_ms: 4_000 });
  socket.drops();
  told.length = 0;
  // The waits grow, and are spread around what they are: 0, 250, 500,
  // 1000 and then 2000 ms, each as the try before it fails.
  const waits = [0, 250, 500, 1000, 2000];
  let made = Socket.made.length;
  for (const wait of waits) {
    if (wait > 0) {
      mock.timers.tick(wait - 1);
      assert.equal(Socket.made.length, made, `it tried before ${wait} ms had passed`);
      mock.timers.tick(1);
    } else {
      mock.timers.tick(0);
    }
    made += 1;
    assert.equal(Socket.made.length, made, `it did not try after ${wait} ms`);
    // A server that is not there, and one that is stopping or
    // starting, are tried again.
    if (wait === 500) {
      last().opens();
      last().says({ type: "error", code: "busy", message: "the daemon is stopping" });
      last().says({ type: "bye", reason: "refused" });
    } else {
      last().onerror?.();
    }
    await settle();
    assert.equal(last().closed, true);
  }
  assert.deepEqual(told, []);
  assert.equal(client.status, "away");
  // 3750 ms have passed. The next try is the last the session waits for.
  mock.timers.tick(2000);
  last().drops();
  await settle();
  assert.deepEqual(told, [["closed", { reason: "gone" }]]);
  assert.equal(client.status, "closed");
  mock.timers.tick(60_000);
  assert.equal(Socket.made.length, made + 1);
  assert.throws(() => client.text("Anybody?"), /no part/);
});

test("a client that is turned away as it comes back does not try again", async () => {
  const { client, socket, told } = await welcomed();
  socket.drops();
  told.length = 0;
  mock.timers.tick(0);
  last().opens();
  last().says({ type: "error", code: "no_session", message: "the session is over" });
  last().says({ type: "bye", reason: "refused" });
  await settle();
  assert.equal(told.length, 1);
  const [type, end] = /** @type {[string, { reason: string, error: Refused }]} */ (told[0]);
  assert.equal(type, "closed");
  assert.equal(end.reason, "refused");
  assert.equal(end.error.code, "no_session");
  assert.equal(client.status, "closed");
  mock.timers.tick(60_000);
  assert.equal(Socket.made.length, 2);
});

test("a goodbye says whether the client comes back", async () => {
  // A server that goes away keeps the session: the client comes back
  // to the one that is started next.
  const going = await welcomed();
  going.socket.says({ type: "bye", reason: "going_away" });
  assert.deepEqual(going.told, [["away", { reason: "going_away" }]]);
  assert.equal(going.socket.closed, true);
  mock.timers.tick(0);
  assert.equal(Socket.made.length, 2);
  last().opens();
  last().says({ ...WELCOME, life: 2, resumed: true });
  await settle();
  assert.equal(going.client.status, "open");
  assert.equal(going.client.resume?.life, 2);

  // A session that ended, and a client whose place was taken, do not.
  for (const reason of /** @type {const} */ (["ended", "replaced", "unknown"])) {
    Socket.made = [];
    const { client, socket, told } = await welcomed();
    socket.says({ type: "bye", reason });
    assert.deepEqual(told, [["closed", { reason }]]);
    assert.equal(client.status, "closed");
    assert.equal(socket.closed, true);
    mock.timers.tick(60_000);
    assert.equal(Socket.made.length, 1);
  }

  // A client that was given nothing to come back with cannot.
  Socket.made = [];
  const { resume_token: _, ...tokenless } = WELCOME;
  const { client, socket, told } = await welcomed({}, tokenless);
  assert.equal(client.resume, undefined);
  socket.drops();
  assert.deepEqual(told, [["closed", { reason: "gone" }]]);
  mock.timers.tick(60_000);
  assert.equal(Socket.made.length, 1);
});

test("a client ends the session, or lets go of the line", async () => {
  const ending = await welcomed();
  ending.client.bye();
  assert.deepEqual(ending.socket.said.at(-1), { type: "bye" });
  assert.equal(ending.socket.closed, true);
  assert.deepEqual(ending.told, [["closed", { reason: "ended" }]]);
  // Said twice, it is said once.
  ending.client.bye();
  assert.equal(ending.told.length, 1);

  // Letting go says nothing: the session waits, and what the client
  // held is what another comes back with.
  Socket.made = [];
  const leaving = await welcomed();
  leaving.socket.says({ type: "fact", seq: 3, kind: "participant_joined", who: 0 });
  const resume = leaving.client.resume;
  leaving.client.detach();
  assert.equal(leaving.socket.said.length, 1);
  assert.equal(leaving.socket.closed, true);
  assert.deepEqual(leaving.told.at(-1), ["closed", { reason: "left" }]);
  mock.timers.tick(60_000);
  assert.equal(Socket.made.length, 1);

  const returning = Client.connect("ws://127.0.0.1:8080", { resume, features: ["said"], WebSocket });
  last().opens();
  assert.deepEqual(last().said, [
    {
      type: "hello",
      v: 1,
      features: ["said"],
      resume: { token: "wpr1_madeup", life: 1, seen: 3 },
    },
  ]);
  const { resume_token: _, ...back } = { ...WELCOME, resumed: true };
  last().says(back);
  const client = await returning;
  // It goes on with the token it came back with.
  assert.deepEqual(client.resume, { token: "wpr1_madeup", life: 1, seen: 0 });
});

test("a client on a WebSocket that is given a microphone says nothing of it: its voice sends what it hears", async () => {
  const { client, socket } = await welcomed();
  const sent = socket.sent.length;
  const track = /** @type {MediaStreamTrack} */ (/** @type {unknown} */ ({ kind: "audio" }));
  await client.hears(track);
  await client.hears(undefined);
  assert.equal(client.call, undefined);
  assert.equal(socket.sent.length, sent);
  assert.equal(client.status, "open");
});

test("a client that is told to comes back on another connection", async () => {
  const { client, socket, told } = await welcomed();
  client.reconnect();
  assert.equal(socket.closed, true);
  assert.deepEqual(told, [["away", { reason: "dropped" }]]);
  // A client that lets go while it comes back is not put on the line
  // it was coming back on.
  mock.timers.tick(0);
  last().opens();
  client.detach();
  last().says({ ...WELCOME, resumed: true });
  await settle();
  assert.equal(client.status, "closed");
  assert.equal(last().closed, true);
});
