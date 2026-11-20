// @ts-check
// A client on a call, with what a browser has for calls stood in for:
// the offer it posts and what it does with the answer, the hello it
// says on the call, how it comes back, and how it hangs up.
//
// A real browser on a real call is in `browser/run.js`.

import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";

import { Client, Refused } from "../src/client.js";
import { Unreached, callsOf, heldAt, iceServersOf } from "../src/webrtc.js";
import { Daemon, Peer, RTCPeerConnection, lastCall } from "./support/call.js";
import { WELCOME, settle } from "./support/line.js";

const TOKEN = "wpc1_madeup";
const AT = "https://agents.example";

/** @type {Daemon} */
let daemon = new Daemon();

beforeEach(() => {
  Peer.made = [];
  Peer.refusesServers = undefined;
  daemon = new Daemon();
});

/**
 * What a client that calls is given beside its token.
 *
 * @param {import("../src/client.js").Options} [more]
 * @returns {import("../src/client.js").Options}
 */
const calling = (more = {}) => ({
  over: "webrtc",
  token: TOKEN,
  RTCPeerConnection,
  fetch: daemon.fetch,
  random: () => 0.5,
  ...more,
});

/** A client on a call that was welcomed. */
async function onACall(/** @type {import("../src/client.js").Options} */ more = {}) {
  const connecting = Client.connect(AT, calling(more));
  await settle();
  const call = lastCall();
  call.channel?.opens();
  call.channel?.says(WELCOME);
  return { client: await connecting, call };
}

test("offers are taken at the address of a server, which says nothing of who calls", () => {
  assert.equal(callsOf("https://agents.example"), "https://agents.example/v1/webrtc");
  assert.equal(callsOf("http://127.0.0.1:7070/"), "http://127.0.0.1:7070/v1/webrtc");
  // A server may be named by its line for clients.
  assert.equal(callsOf("wss://agents.example"), "https://agents.example/v1/webrtc");
  assert.equal(callsOf("ws://127.0.0.1:7070/v1/client"), "http://127.0.0.1:7070/v1/webrtc");
  // One that takes offers elsewhere is asked there.
  assert.equal(callsOf("https://agents.example/voice/offers#top"), "https://agents.example/voice/offers");
  for (const not of ["ftp://agents.example", "https://agents.example/?token=wpc1_madeup", "https://someone:wpc1_madeup@agents.example"]) {
    assert.throws(() => callsOf(not), RangeError, not);
  }
});

test("a far end is told of the servers that the links of an answer name", () => {
  assert.deepEqual(iceServersOf(null), []);
  assert.deepEqual(iceServersOf(""), []);
  // As a daemon says them: one header after the other, with commas
  // between.
  const said = [
    '<stun:stun.example.net>; rel="ice-server"',
    '<turn:turn.example.net?transport=udp>; rel="ice-server"; username="1700000000:2a"; credential="bWFkZSB1cA=="',
    '<turns:turn.example.net:5349?transport=tcp>; rel="ice-server"; username="every\\"one"; credential="a \\"word\\\\, and more"',
  ].join(", ");
  assert.deepEqual(iceServersOf(said), [
    { urls: ["stun:stun.example.net"] },
    { urls: ["turn:turn.example.net?transport=udp"], username: "1700000000:2a", credential: "bWFkZSB1cA==" },
    { urls: ["turns:turn.example.net:5349?transport=tcp"], username: 'every"one', credential: 'a "word\\, and more' },
  ]);
  // What is no server, or is one a browser cannot be told of, is
  // passed over, and what follows it is still read.
  const mixed = [
    '<https://agents.example/next>; rel="next"',
    "not a link at all",
    '<https://agents.example/extension>; rel="ice-server"',
    '<turn:turn.example.net>; rel="ice-server"',
    '<turn:turn.example.net>; rel="ice-server"; username="somebody"',
    "<stun:stun.example.net:3479>; rel=ice-server",
    '<stun:other.example.net>; rel="alternate ICE-Server"; title="a, b; c"',
  ].join(",");
  assert.deepEqual(iceServersOf(mixed), [{ urls: ["stun:stun.example.net:3479"] }, { urls: ["stun:other.example.net"] }]);
  // One that is cut off is not a server.
  assert.deepEqual(iceServersOf("<stun:stun.example.net"), []);
  // A server is what a link says is one, whatever it points at.
  assert.deepEqual(iceServersOf('<stun:stun.example.net>; rel="next", <stun:stun.example.net>'), []);
  // Of what a link says twice, the first holds (RFC 8288).
  const twice = '<turn:turn.example.net>; rel="ice-server"; username="one"; username="another"; credential="first"; credential="second"; rel="next"';
  assert.deepEqual(iceServersOf(twice), [{ urls: ["turn:turn.example.net"], username: "one", credential: "first" }]);
});

test("a call is spoken of at the daemon that answered and nowhere else", () => {
  const offered = "https://agents.example/v1/webrtc";
  assert.equal(heldAt(offered, "/v1/webrtc/0123abcd"), "https://agents.example/v1/webrtc/0123abcd");
  assert.equal(heldAt(offered, "https://agents.example/v1/webrtc/0123abcd"), "https://agents.example/v1/webrtc/0123abcd");
  // The token would be said to whoever is there.
  for (const elsewhere of [null, "", "https://elsewhere.example/v1/webrtc/0123abcd", "//elsewhere.example/v1/webrtc/0123abcd", "http://agents.example/v1/webrtc/0123abcd"]) {
    assert.equal(heldAt(offered, elsewhere), undefined, String(elsewhere));
  }
});

test("a client offers a call with its token, takes the answer, and says hello on the call", async () => {
  const microphone = /** @type {MediaStreamTrack} */ (/** @type {unknown} */ ({ kind: "audio", id: "a microphone" }));
  const connecting = Client.connect(AT, calling({ microphone, features: ["said"] }));
  await settle();
  const call = lastCall();
  // The offer went where the server takes offers, as what it is, with
  // the token and not in the address.
  assert.equal(daemon.asked.length, 1);
  const [offered] = daemon.asked;
  assert.equal(offered?.url, "https://agents.example/v1/webrtc");
  assert.equal(offered?.method, "POST");
  assert.equal(offered?.headers["content-type"], "application/sdp");
  assert.equal(offered?.headers.authorization, `Bearer ${TOKEN}`);
  assert.equal(offered?.body, "v=0\r\ns=an offer\r\n");
  // The call has the channel both ends agreed on, sends what the
  // microphone hears and is sent what the agent says.
  assert.equal(call.channel?.label, "waveport");
  assert.deepEqual(call.channel?.options, { negotiated: true, id: 0 });
  assert.equal(call.sends, microphone);
  assert.deepEqual(call.direction, { direction: "sendrecv" });
  assert.deepEqual(call.did, ["offer", "local offer", "remote answer"]);
  assert.equal(call.remote?.sdp, "v=0\r\ns=an answer\r\n");
  // Nothing is said before the call is there.
  assert.deepEqual(call.channel?.said, []);
  call.channel?.opens();
  assert.deepEqual(call.channel?.said, [{ type: "hello", v: 1, token: TOKEN, features: ["said"] }]);
  call.channel?.says(WELCOME);
  const client = await connecting;
  assert.equal(client.status, "open");
  assert.equal(client.session, WELCOME.session);
  assert.equal(client.over, "webrtc");
  assert.deepEqual(client.audio, { codec: "opus", rate: 48_000 });
  assert.equal(client.call?.receiver, call.receiver);
});

test("a browser is told of the servers before it looks for ways to its far end", async () => {
  daemon.answers = () => Daemon.answer({ link: '<stun:stun.example.net>; rel="ice-server"' });
  const { call } = await onACall();
  assert.deepEqual(call.did, ["offer", "servers", "local offer", "remote answer"]);
  assert.deepEqual(call.configuration.iceServers, [{ urls: ["stun:stun.example.net"] }]);
  assert.equal(call.configuration.bundlePolicy, "max-bundle");

  // One that cannot be told of them calls without.
  Peer.refusesServers = new SyntaxError("no such server");
  const { call: other } = await onACall();
  assert.deepEqual(other.did, ["offer", "local offer", "remote answer"]);
});

test("a call that the server does not take is one that was not made", async () => {
  for (const [status, code, detail] of /** @type {const} */ ([
    [404, "unsupported", "the daemon answers no calls over WebRTC"],
    [401, "token", "that is no token of a session"],
    [503, "busy", "the daemon is stopping"],
    [429, "busy", "too many calls are on their way"],
  ])) {
    daemon.answers = () => Daemon.problem(status, code, detail);
    const failed = await Client.connect(AT, calling()).catch((error) => error);
    assert.ok(failed instanceof Unreached, String(failed));
    assert.equal(failed.status, status);
    assert.equal(failed.code, code);
    assert.equal(failed.message, `the server took no call: ${detail}`);
    // Asked again, a daemon that is busy may answer otherwise.
    assert.equal(failed.final, status === 404 || status === 401, String(status));
    assert.equal(lastCall().closed, true);
    assert.deepEqual(lastCall().channel?.said, []);
  }
  // One that says nothing that can be read, and one that cannot be
  // reached at all.
  daemon.answers = () => new Response("<html>", { status: 502 });
  const unread = await Client.connect(AT, calling()).catch((error) => error);
  assert.ok(unread instanceof Unreached);
  assert.equal(unread.message, "the server took no call: status 502");
  assert.equal(unread.final, false);
  daemon.answers = () => Promise.reject(new TypeError("fetch failed"));
  const nowhere = await Client.connect(AT, calling()).catch((error) => error);
  assert.ok(nowhere instanceof Unreached);
  assert.equal(nowhere.status, undefined);
  assert.equal(nowhere.final, false);
  // Nothing of these is asked about afterwards: no call was set up.
  assert.deepEqual(daemon.asked.map((asked) => asked.method), ["POST", "POST", "POST", "POST", "POST", "POST"]);
});

test("a call that does not get through in time is let go of, and nothing was said on it", async () => {
  const failed = Client.connect(AT, calling({ callWithinMs: 30 })).catch((error) => error);
  await settle();
  const call = lastCall();
  assert.deepEqual(call.did, ["offer", "local offer", "remote answer"]);
  const error = await failed;
  assert.ok(error instanceof Unreached, String(error));
  assert.equal(error.message, "the call did not get through");
  assert.equal(error.final, false);
  assert.equal(call.closed, true);
  assert.deepEqual(call.channel?.said, []);
  // The daemon is told that nobody comes for what it set up, with the
  // token the offer was made with.
  await settle();
  assert.deepEqual(daemon.asked.map((asked) => [asked.method, asked.url]), [
    ["POST", "https://agents.example/v1/webrtc"],
    ["DELETE", "https://agents.example/v1/webrtc/0123456789abcdef0123456789abcdef"],
  ]);
  assert.equal(daemon.asked[1]?.headers.authorization, `Bearer ${TOKEN}`);

  // So is one whose ways all failed, and one whose offer is not
  // answered in time.
  const other = Client.connect(AT, calling()).catch((error) => error);
  await settle();
  lastCall().becomes("failed");
  assert.ok((await other) instanceof Unreached);
  daemon.answers = () => new Promise(() => {});
  const slow = await Client.connect(AT, calling({ callWithinMs: 20 })).catch((error) => error);
  assert.ok(slow instanceof Unreached, String(slow));
  assert.equal(lastCall().closed, true);
});

test("a hello that is refused on a call is no client, and the call is the session's to end", async () => {
  const connecting = Client.connect(AT, calling());
  await settle();
  const call = lastCall();
  call.channel?.opens();
  call.channel?.says({ type: "error", code: "token", message: "the token was used" });
  call.channel?.says({ type: "bye", reason: "refused" });
  const failed = await connecting.catch((error) => error);
  assert.ok(failed instanceof Refused, String(failed));
  assert.equal(failed.code, "token");
  assert.equal(failed.message, "the token was used");
  assert.equal(call.closed, true);
  // The hello was said: what became of the call is not undone from
  // here.
  assert.deepEqual(daemon.asked.map((asked) => asked.method), ["POST"]);

  // A call that ends, or stays silent, after the hello is no call that
  // was not made.
  const dropped = Client.connect(AT, calling());
  await settle();
  lastCall().channel?.opens();
  lastCall().channel?.drops();
  const error = await dropped.catch((error) => error);
  assert.ok(!(error instanceof Unreached) && !(error instanceof Refused), String(error));
  assert.equal(error.message, "the call ended before the server answered the hello");
  const silent = Client.connect(AT, calling({ welcomeWithinMs: 20 }));
  await settle();
  lastCall().channel?.opens();
  const late = await silent.catch((error) => error);
  assert.equal(late.message, "the server did not answer the hello");
  assert.ok(!(late instanceof Unreached));
  assert.deepEqual(daemon.asked.map((asked) => asked.method), ["POST", "POST", "POST"]);
});

test("a client on a call says what its user does on the call, and no audio", async () => {
  // How audio would be written on a line is of no account on a call,
  // and is not said.
  const { client, call } = await onACall({ audio: { codec: "pcm16", rate: 16_000 } });
  assert.deepEqual(call.channel?.said, [{ type: "hello", v: 1, token: TOKEN }]);
  assert.deepEqual(client.audio, { codec: "opus", rate: 48_000 });
  client.text("What time is it?");
  client.dtmf("12#");
  client.interrupt();
  client.playedRtp({ ts: 4_132_500, delay_ms: 38 });
  // The call carries what its microphone hears by itself.
  client.send(new Int16Array(960));
  assert.deepEqual(call.channel?.said.slice(1), [
    { type: "text", text: "What time is it?" },
    { type: "dtmf", keys: "12#" },
    { type: "interrupt" },
    { type: "played_rtp", ts: 4_132_500, delay_ms: 38 },
  ]);
  // What was played is a place in the stream or how much came out,
  // in whole numbers that can be one.
  for (const not of [
    {},
    { delay_ms: 20 },
    { ts: -1 },
    { ts: 1.5 },
    { ts: 0x1_0000_0000 },
    { ts: 960, delay_ms: -1 },
    { emitted: -1 },
    { emitted: 0.5 },
    { emitted: 2 ** 53 },
    { emitted: 960, concealed: -1 },
  ]) {
    assert.throws(() => client.playedRtp(not), RangeError, JSON.stringify(not));
  }
  client.playedRtp({ ts: 0xffff_ffff });
  client.playedRtp({ emitted: 2 ** 53 - 1, concealed: 960, delay_ms: 20 });
  assert.deepEqual(call.channel?.said.slice(5), [
    { type: "played_rtp", ts: 0xffff_ffff, delay_ms: 0 },
    { type: "played_rtp", emitted: 2 ** 53 - 1, concealed: 960, delay_ms: 20 },
  ]);
  // And is told what the session says, as on any line.
  /** @type {unknown[]} */
  const marks = [];
  client.on("mark", (mark) => marks.push(mark));
  call.channel?.says({ type: "mark", run: 0, generation: 1, ts: 96_000 });
  assert.deepEqual(marks, [{ type: "mark", run: 0, generation: 1, ts: 96_000 }]);
});

test("a client that ends its session says so on the call and where it offered it", async () => {
  const { client, call } = await onACall();
  const on = client.call;
  assert.ok(on !== undefined);
  /** @type {unknown[]} */
  const closed = [];
  client.on("closed", (end) => closed.push(end));
  client.bye();
  assert.deepEqual(call.channel?.said.at(-1), { type: "bye" });
  assert.equal(call.closed, true);
  assert.deepEqual(closed, [{ reason: "ended" }]);
  // A client whose session is over is on no call.
  assert.equal(client.call, undefined);
  // A goodbye on the call can be lost with the call. This one cannot,
  // and is to arrive though the page may be going.
  const hungUp = daemon.asked.at(-1);
  assert.equal(hungUp?.method, "DELETE");
  assert.equal(hungUp?.url, "https://agents.example/v1/webrtc/0123456789abcdef0123456789abcdef");
  assert.equal(hungUp?.headers.authorization, `Bearer ${TOKEN}`);
  assert.equal(hungUp?.keepalive, true);
  // It is said once, whoever else hangs the call up.
  const told = daemon.asked.length;
  on.hangUp();
  client.bye();
  assert.equal(daemon.asked.length, told);

  // One that only lets go of its call ends nothing.
  const { client: other, call: left } = await onACall();
  const before = daemon.asked.length;
  other.detach();
  assert.equal(left.closed, true);
  assert.equal(daemon.asked.length, before);
  assert.deepEqual(left.channel?.said.length, 1);

  // And a daemon that points elsewhere for the call is not told there.
  daemon.answers = () => Daemon.answer({ location: "https://elsewhere.example/v1/webrtc/0123" });
  const { client: third } = await onACall();
  const asked = daemon.asked.length;
  third.bye();
  assert.equal(daemon.asked.length, asked);
});

test("a client whose call drops offers another with what it was welcomed with", async () => {
  const { client, call } = await onACall({ features: ["said"], audio: { codec: "pcm16", rate: 16_000 } });
  /** @type {string[]} */
  const told = [];
  client.on("away", ({ reason }) => told.push(`away: ${reason}`));
  client.on("back", (welcome) => told.push(`back: ${welcome.resumed}`));
  /** The call the client is on, asked anew each time. */
  const on = () => client.call;
  call.channel?.says({ type: "fact", seq: 1, kind: "participant_joined", who: 1 });
  // The call is not there any more, as a browser says it.
  call.becomes("disconnected");
  assert.equal(client.status, "away");
  assert.equal(on(), undefined);
  assert.equal(call.closed, true);
  await new Promise((resolve) => setTimeout(resolve, 5));
  await settle();
  const again = lastCall();
  assert.notEqual(again, call);
  // The offer is made with the token the client was given to come
  // back with, and the hello says where it was and not what it came
  // with first.
  const offered = daemon.asked.at(-1);
  assert.equal(offered?.method, "POST");
  assert.equal(offered?.headers.authorization, "Bearer wpr1_madeup");
  again.channel?.opens();
  assert.deepEqual(again.channel?.said, [
    { type: "hello", v: 1, features: ["said"], resume: { token: "wpr1_madeup", life: 1, seen: 1 } },
  ]);
  again.channel?.says({ ...WELCOME, resumed: true, resume_token: "wpr1_another", next_seq: 2 });
  await settle();
  assert.equal(client.status, "open");
  assert.equal(on()?.receiver, again.receiver);
  assert.deepEqual(told, ["away: dropped", "back: true"]);
  // Nobody was told that the far end is gone: it is not.
  assert.deepEqual(daemon.asked.map((asked) => asked.method), ["POST", "POST"]);

  // The call that is hung up is the one the client is on, with the
  // token that one was offered with.
  client.bye();
  assert.equal(daemon.asked.at(-1)?.method, "DELETE");
  assert.equal(daemon.asked.at(-1)?.headers.authorization, "Bearer wpr1_madeup");
});

test("a client that the server turns away as it comes back does not offer again", async () => {
  const { client, call } = await onACall();
  /** @type {any[]} */
  const closed = [];
  client.on("closed", (end) => closed.push(end));
  // A daemon that is stopping is asked again, and the one that follows
  // it says that the session is over.
  let offers = 0;
  daemon.answers = () => {
    offers += 1;
    return offers === 1
      ? Daemon.problem(503, "busy", "the daemon is stopping")
      : Daemon.problem(401, "token", "that is no token of a session");
  };
  call.channel?.drops();
  assert.equal(client.status, "away");
  await new Promise((resolve) => setTimeout(resolve, 400));
  assert.equal(offers, 2);
  assert.equal(client.status, "closed");
  assert.equal(closed.length, 1);
  assert.equal(closed[0].reason, "refused");
  assert.ok(closed[0].error instanceof Refused);
  assert.equal(closed[0].error.code, "token");
  await new Promise((resolve) => setTimeout(resolve, 300));
  assert.equal(offers, 2);
});
