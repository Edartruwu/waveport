// @ts-check
// A browser on the page a daemon serves, with a recording for a
// microphone: a call that is spoken, over a WebSocket and over WebRTC,
// one that is written, one whose line drops, and one that does not get
// through over WebRTC and is made over a WebSocket instead.
//
//   node test/browser/run.js --daemon <stand_in> [--chrome <chrome>] [--notes] [--only <words>]
//   node test/browser/run.js --daemon <stand_in> --firefox <firefox>
//   node test/browser/run.js --can-run
//
// `--notes` prints what the page wrote to its console, for whoever
// looks into a page that does not do what it should, and `--only` runs
// the steps whose names have `<words>` in them. `--can-run` says
// whether there is what the test needs, a Node that is new enough and
// a Chrome, and runs nothing.
//
// The daemon is `apps/waveportd/examples/stand_in.rs`: the real daemon
// with stand-ins for its providers, so nothing beyond this machine is
// asked for anything. The browser is a Chrome that is driven over the
// pipe it offers for that, with nothing installed to drive it. It is
// given the question the stand-in for the recogniser hears in place of
// a microphone, and plays what the agent says to nothing: no sound is
// made, and no microphone is opened.
//
// `--firefox` runs the same steps in a Firefox, which is driven over
// WebDriver BiDi and whose page is given the recording through Web
// Audio where its microphone is asked for. It is for whoever has one:
// the gates run a Chrome. Firefox makes no call to an address of this
// machine that no other machine reaches, so the daemon then takes
// calls at the address the machine has on its network.
//
// The agent greets whoever comes on its line, at once. A page that
// was not ready to play when it was welcomed would not play that, and
// would not say that it had. The session would not notice: it takes a
// far end that says nothing of what it played to play as it is sent.
// So what the page said it played is looked at here, run by run.
//
// A call over WebRTC is made to the address of this machine that no
// other machine reaches, over datagrams and then over a connection
// alone: the test takes the ways it is not to go out of what the
// daemon answers, before the browser reads it. With every way taken
// out no call comes about, which is what a network that lets nothing
// of a call through looks like from a page.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createSocket } from "node:dgram";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";

import { Browser, Chrome, NODE, calledAt, chrome, daemon } from "../support/browser.js";

/** How long the whole of it may take. */
const ALL_WITHIN = 300_000;

const { values: asked } = parseArgs({
  options: {
    daemon: { type: "string" },
    chrome: { type: "string" },
    firefox: { type: "string" },
    notes: { type: "boolean" },
    only: { type: "string" },
    "can-run": { type: "boolean" },
  },
});

/**
 * The address this machine has on its network: the one it would send
 * from to another machine. Nothing is sent to find it.
 *
 * @returns {Promise<string>}
 */
function onTheNetwork() {
  return new Promise((resolve, reject) => {
    const socket = createSocket("udp4");
    socket.once("error", () => reject(new Error("this machine is on no network: Firefox makes no call to it")));
    // An address that is nobody's (RFC 5737), to ask the way to.
    socket.connect(9, "192.0.2.1", () => {
      const { address } = socket.address();
      socket.close();
      resolve(address);
    });
  });
}

/**
 * What a Firefox is told before it starts. No microphone is opened and
 * no sound is made, whatever a page does: what it would hear is the
 * browser's own stand-in, and what it plays has no volume.
 */
const FIREFOX_IS_TOLD = [
  ["media.navigator.streams.fake", true],
  ["media.navigator.permission.disabled", true],
  ["media.volume_scale", "0.0"],
  ["media.autoplay.default", 0],
  ["media.autoplay.blocking_policy", 0],
  ["media.autoplay.block-webaudio", false],
  // It asks nothing of anybody but the daemon.
  ["browser.shell.checkDefaultBrowser", false],
  ["browser.aboutwelcome.enabled", false],
  ["datareporting.policy.dataSubmissionEnabled", false],
  ["toolkit.telemetry.reportingpolicy.firstRun", false],
  ["app.update.enabled", false],
]
  .map(([name, value]) => `user_pref(${JSON.stringify(name)}, ${JSON.stringify(value)});`)
  .join("\n");

/**
 * Gives a page the recording `wav` where it asks for a microphone: it
 * plays from its beginning when it is asked for, once.
 *
 * @param {string} wav The recording, in base64.
 */
const hearsARecording = (wav) => `(() => {
  // A page that is none yet has no microphone to ask for.
  if (navigator.mediaDevices === undefined) {
    return;
  }
  const recording = Uint8Array.from(atob("${wav}"), (character) => character.charCodeAt(0));
  navigator.mediaDevices.getUserMedia = async () => {
    const context = new AudioContext({ sampleRate: 48000 });
    const source = context.createBufferSource();
    source.buffer = await context.decodeAudioData(recording.buffer.slice(0));
    const heard = context.createMediaStreamDestination();
    source.connect(heard);
    source.start();
    return heard.stream;
  };
})()`;

/** A Firefox, which is driven over WebDriver BiDi. */
class Firefox extends Browser {
  /** @type {import("node:child_process").ChildProcess} */
  #child;

  #profile;

  #microphone;

  /** Where it takes what drives it, once it has said so. @type {Promise<string>} */
  #listens;

  /** @type {WebSocket | undefined} */
  #socket;

  #next = 1;

  /** @type {Map<number, { resolve: (result: any) => void, reject: (error: Error) => void }>} */
  #waiting = new Map();

  /** The page that is driven. @type {string | undefined} */
  #page;

  /**
   * @param {string} program
   * @param {string} profile A folder for it that is nobody's, with
   *   what it is told in it (`FIREFOX_IS_TOLD`, as `user.js`).
   * @param {string} microphone The recording it hears in place of one.
   */
  constructor(program, profile, microphone) {
    super();
    this.#profile = profile;
    this.#microphone = microphone;
    // `--no-remote`: it is not the Firefox somebody has open here, and
    // takes nothing from it.
    const args = ["--headless", "--no-remote", "--profile", profile, "--remote-debugging-port", "0"];
    this.#child = spawn(program, args, { stdio: ["ignore", "ignore", "pipe"] });
    const said = /** @type {import("node:stream").Readable} */ (this.#child.stderr).setEncoding("utf8");
    this.#listens = new Promise((resolve, reject) => {
      let told = "";
      const late = setTimeout(() => reject(new Error(`Firefox did not say where it is driven: ${told}`)), 60_000);
      said.on("data", (text) => {
        told += text;
        const at = /WebDriver BiDi listening on (ws:\/\/\S+)/.exec(told);
        if (at?.[1] !== undefined) {
          clearTimeout(late);
          resolve(at[1]);
        }
      });
    });
  }

  /**
   * @param {string} method
   * @param {object} [params]
   * @returns {Promise<any>}
   */
  ask(method, params = {}) {
    const id = this.#next;
    this.#next += 1;
    this.#socket?.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => {
      this.#waiting.set(id, { resolve, reject });
    });
  }

  /** @param {any} message */
  #heard(message) {
    if (message.id !== undefined) {
      const waiting = this.#waiting.get(message.id);
      this.#waiting.delete(message.id);
      if (message.type === "error") {
        waiting?.reject(new Error(`the browser refused: ${message.error}: ${message.message}`));
      } else {
        waiting?.resolve(message.result);
      }
    } else if (message.method === "log.entryAdded") {
      this.noted.push(`${message.params.level}: ${message.params.text}`);
    }
  }

  /**
   * @override
   * @param {string} first
   */
  async open(first) {
    const socket = new WebSocket(`${await this.#listens}/session`);
    this.#socket = socket;
    await new Promise((resolve, reject) => {
      socket.onopen = resolve;
      socket.onerror = () => reject(new Error("Firefox is not driven where it said"));
    });
    socket.onmessage = (event) => this.#heard(JSON.parse(event.data));
    await this.ask("session.new", { capabilities: {} });
    await this.ask("session.subscribe", { events: ["log.entryAdded"] });
    const { contexts } = await this.ask("browsingContext.getTree");
    this.#page = contexts[0].context;
    const wav = readFileSync(this.#microphone).toString("base64");
    const before = `() => { ${first}; ${hearsARecording(wav)}; }`;
    await this.ask("script.addPreloadScript", { functionDeclaration: before });
  }

  /**
   * @override
   * @param {string} address
   */
  async goes(address) {
    await this.ask("browsingContext.navigate", { context: this.#page, url: address, wait: "complete" });
  }

  /**
   * @override
   * @param {string} expression
   * @returns {Promise<any>}
   */
  async does(expression) {
    // What comes of it is brought back written out, as what it is.
    const done = await this.ask("script.evaluate", {
      expression: `(async () => JSON.stringify(await (${expression})))()`,
      target: { context: this.#page },
      awaitPromise: true,
      userActivation: true,
      resultOwnership: "none",
    });
    if (done.type === "exception") {
      throw new Error(`the page threw: ${done.exceptionDetails?.text}`);
    }
    return done.result?.type === "string" ? JSON.parse(done.result.value) : undefined;
  }

  /** @override */
  async close() {
    const gone = new Promise((resolve) => this.#child.once("exit", resolve));
    this.ask("browser.close").catch(() => {});
    const late = setTimeout(() => this.#child.kill("SIGKILL"), 5_000);
    await gone;
    clearTimeout(late);
    this.#socket?.close();
    // A Firefox that is started through something else lives on when
    // that is gone. What runs with this profile is this test's, and
    // nothing else is.
    for (const [pid, line] of running()) {
      if (line.includes(this.#profile)) {
        try {
          process.kill(pid, "SIGKILL");
        } catch {
          // Gone already.
        }
      }
    }
  }
}

/**
 * What runs on this machine, where that can be read: each with the
 * line it was started with.
 *
 * @returns {[number, string][]}
 */
function running() {
  /** @type {[number, string][]} */
  const found = [];
  /** @type {string[]} */
  let names = [];
  try {
    names = readdirSync("/proc");
  } catch {
    // Not a system that says.
  }
  for (const name of names) {
    const pid = Number(name);
    if (Number.isInteger(pid) && pid !== process.pid) {
      try {
        found.push([pid, readFileSync(`/proc/${name}/cmdline`, "utf8")]);
      } catch {
        // Gone already, or somebody else's.
      }
    }
  }
  return found;
}

/** What the agent says first, is asked and answers, as the stand-ins have it. */
const GREETING = "Good morning.";
const QUESTION = "What time is it right now?";
const ANSWER = "It is ten past three. Goodbye!";

/**
 * How long the stand-in for the synthesiser speaks, in samples of the
 * line: 400 ms for the greeting and 1.2 s for the answer, at 48 kHz.
 */
const GREETED = 19_200;
const ANSWERED = 57_600;

/**
 * Keeps what the page says to the daemon, for the test to read: on its
 * line, `window.said`, and on every call it makes, with what it is told
 * there, `window.calls`. And takes out of what the daemon answers an
 * offer with the ways the call is not to go, as `window.ways` says:
 * every way but a connection (`"tcp"`), or every way (`"none"`).
 */
const KEEPS_WHAT_IS_SAID = `(() => {
  window.said = [];
  window.calls = [];
  window.asked = [];
  const asks = window.fetch;
  window.fetch = async function (at, how) {
    const answered = await asks.call(this, at, how);
    const path = new URL(String(at), location.href).pathname;
    if (path.startsWith("/v1/webrtc")) {
      window.asked.push({ method: how?.method ?? "GET", path, status: answered.status });
    }
    return answered;
  };
  const send = WebSocket.prototype.send;
  WebSocket.prototype.send = function (data) {
    if (typeof data === "string") {
      window.said.push(JSON.parse(data));
    }
    return send.call(this, data);
  };
  const makes = RTCPeerConnection.prototype.createDataChannel;
  RTCPeerConnection.prototype.createDataChannel = function (...of) {
    const channel = makes.apply(this, of);
    const call = { said: [], told: [] };
    window.calls.push(call);
    const says = channel.send;
    channel.send = function (data) {
      if (typeof data === "string") {
        call.said.push(JSON.parse(data));
      }
      return says.call(this, data);
    };
    channel.addEventListener("message", (event) => {
      if (typeof event.data === "string") {
        call.told.push(JSON.parse(event.data));
      }
    });
    return channel;
  };
  const takes = RTCPeerConnection.prototype.setRemoteDescription;
  RTCPeerConnection.prototype.setRemoteDescription = function (answer) {
    const ways = window.ways;
    if (ways === undefined || typeof answer?.sdp !== "string") {
      return takes.call(this, answer);
    }
    const kept = answer.sdp.split("\\r\\n").filter((line) => {
      if (!line.startsWith("a=candidate:")) {
        return true;
      }
      return ways === "tcp" && line.split(" ")[2]?.toLowerCase() === "tcp";
    });
    return takes.call(this, { type: answer.type, sdp: kept.join("\\r\\n") });
  };
})()`;

/**
 * What the call the page is on goes over: `"udp"` for datagrams, and
 * `"tcp"` for a connection.
 */
const WAY = `(async () => {
  const stats = await window.playground.client.call.peer.getStats();
  for (const one of stats.values()) {
    // One browser says which pair of ways a call goes over with what
    // it says of the call, and the other with what it says of the pair.
    const pair =
      one.type === "transport" && one.selectedCandidatePairId !== undefined
        ? stats.get(one.selectedCandidatePairId)
        : one.type === "candidate-pair" && one.selected === true
          ? one
          : undefined;
    if (pair !== undefined) {
      return stats.get(pair.localCandidateId)?.protocol;
    }
  }
  return undefined;
})()`;

/**
 * Whether `a` is at or after `b` in the time of a stream, which comes
 * round to nought after 2^32.
 *
 * @param {number} a
 * @param {number} b
 */
const reached = (a, b) => ((a - b) >>> 0) < 0x8000_0000;

/** What the page says of where it is, for whoever reads that something did not come about. */
const SHOWN = `({
  status: document.getElementById("status").textContent,
  problem: window.playground?.problem,
  ended: window.playground?.ended,
  log: [...document.querySelectorAll("#log li")].map((line) => line.textContent).slice(-25),
})`;

/**
 * What the page shows of the conversation: who, and what.
 *
 * @param {Browser} browser
 */
function conversation(browser) {
  return browser.does(`[...document.querySelectorAll("#conversation li")].map((item) => [
    item.dataset.kind,
    item.firstChild.textContent,
    item.querySelector("small")?.textContent,
  ])`);
}

/**
 * What the page shows of a session that went as every session of the
 * stand-in goes: the greeting, the question and the answer, each heard
 * or read to its end.
 *
 * @param {Browser} browser
 */
async function wentAsEver(browser) {
  const said = await conversation(browser);
  const shown = JSON.stringify(said);
  assert.deepEqual(said[0], ["utterance", GREETING, "Agent"], shown);
  assert.deepEqual(said[1], ["turn", QUESTION, "You"], shown);
  const answered = said.some(
    (/** @type {string[]} */ [kind, text, who]) => kind === "utterance" && text === ANSWER && who === "Agent",
  );
  assert.ok(answered, shown);
}

/** The page has no session, and is ready for one. */
const READY = `document.getElementById("agent").options.length > 0
  && !document.getElementById("start").hidden
  && !document.getElementById("talk").disabled`;

/** The session the page was in is over. */
const ENDED = "window.playground.ended !== undefined";

/** The page is on the line of a session. */
const OPEN = `window.playground.client?.status === "open"`;

/**
 * A call that is spoken over a WebSocket: the browser hears the
 * question from its recording, sends it as `line` says, and plays the
 * answer.
 *
 * @param {Browser} browser
 * @param {string} line How the audio is to be written, and over what.
 * @param {"none"} [ways] The ways a call over WebRTC is left, where
 *   one is tried first.
 */
async function spoken(browser, line, ways) {
  await browser.until(READY, "a page that is ready");
  await browser.does(`(() => {
    window.said = [];
    window.calls = [];
    window.asked = [];
    window.ways = ${JSON.stringify(ways)};
  })()`);
  await browser.does(`document.getElementById("talk").click()`);
  await browser.until(`${OPEN} || window.playground.problem !== undefined`, "a call");
  assert.equal(await browser.does("window.playground.problem"), undefined);
  const about = await browser.does(`document.getElementById("about").textContent`);
  assert.ok(about.includes(line), `the line is ${line}: ${about}`);
  // The page says what it played as it plays it, and the session takes
  // the answer as heard to its end by that: it is not cut off.
  await browser.until(ENDED, "the end of the call");
  assert.equal(await browser.does("window.playground.ended"), "ended");
  await wentAsEver(browser);
  assert.equal(await browser.does("window.playground.problem"), undefined);
  // The page played the greeting and the answer, each to its last
  // sample, and said so as it went.
  const played = await browser.does(`window.said.filter((message) => message.type === "played")`);
  /** How much of each run the page said it played. @type {Map<number, number>} */
  const most = new Map();
  for (const { run, samples } of played) {
    assert.ok(samples >= (most.get(run) ?? 0), `what was played went back: ${JSON.stringify(played)}`);
    most.set(run, samples);
  }
  assert.deepEqual(
    [...most],
    [
      [0, GREETED],
      [1, ANSWERED],
    ],
    JSON.stringify(played),
  );
  // More than once for a run that lasts: a session that is interrupted
  // has to know how far its far end had heard.
  assert.ok(played.length >= 8, JSON.stringify(played));
  if (ways === "none") {
    // A call was offered first, and nothing was said on it: the hello
    // went over the line, with the token the call was offered with.
    const calls = await browser.does("window.calls");
    assert.equal(calls.length, 1, JSON.stringify(calls));
    assert.deepEqual(calls[0], { said: [], told: [] });
    assert.equal(await browser.does(`window.said[0]?.type`), "hello");
    // The daemon was told that nobody comes for the call it had set
    // up, where it said the call is, and let go of it.
    /** @type {Asked[]} */
    const asked = await browser.does("window.asked");
    const shown = JSON.stringify(asked);
    assert.deepEqual(
      asked.map(({ method, status }) => [method, status]),
      [
        ["POST", 201],
        ["DELETE", 200],
      ],
      shown,
    );
    assert.equal(asked[0]?.path, "/v1/webrtc", shown);
    assert.match(asked[1]?.path ?? "", /^\/v1\/webrtc\/[0-9a-f]+$/, shown);
  } else {
    // Where a call is not to be tried, none is offered.
    assert.deepEqual(await browser.does("window.asked"), []);
  }
}

/**
 * What a call was told of where its responses are, and what the page
 * said of where it had played to.
 *
 * @typedef {{ said: any[], told: any[] }} Kept
 */

/**
 * What the page asked the daemon about its calls, and how the daemon
 * answered: nothing of who asked.
 *
 * @typedef {{ method: string, path: string, status: number }} Asked
 */

/**
 * The responses a call was told of from beginning to end: how long
 * each is, in samples of the call, and whether the page said that it
 * played it to its end.
 *
 * @param {Kept} call
 * @returns {{ run: number, samples: number, heard: boolean }[]}
 */
function responses(call) {
  /** @type {number[]} */
  const places = call.said.filter((message) => message.type === "played_rtp").map((message) => message.ts);
  /** Where each run begins. @type {Map<number, number>} */
  const begins = new Map();
  const whole = [];
  for (const mark of call.told.filter((message) => message.type === "mark")) {
    assert.ok(Number.isInteger(mark.ts), `a mark of a call says where it is: ${JSON.stringify(mark)}`);
    const began = begins.get(mark.run);
    if (mark.end !== true) {
      begins.set(mark.run, mark.ts);
    } else if (began !== undefined) {
      const heard = places.some((place) => reached(place, mark.ts));
      whole.push({ run: mark.run, samples: (mark.ts - began) >>> 0, heard });
    }
  }
  return whole;
}

/**
 * A call that is spoken over WebRTC: the call carries the question
 * from the recording and the answer, and the page says where in the
 * call's audio it has played to.
 *
 * @param {Browser} browser
 * @param {object} how
 * @param {"tcp"} [how.ways] The ways the call is left.
 * @param {"udp" | "tcp"} how.way What it is to go over.
 * @param {boolean} [how.drops] Whether the call drops once the agent
 *   has greeted, and the page comes back on another.
 */
async function called(browser, how) {
  await browser.until(READY, "a page that is ready");
  await browser.does(`(() => {
    window.said = [];
    window.calls = [];
    window.asked = [];
    window.ways = ${JSON.stringify(how.ways)};
  })()`);
  await browser.does(`document.getElementById("talk").click()`);
  await browser.until(`${OPEN} || window.playground.problem !== undefined`, "a call");
  assert.equal(await browser.does("window.playground.problem"), undefined);
  const line = "opus at 48 kHz, on a call over WebRTC";
  const about = await browser.does(`document.getElementById("about").textContent`);
  assert.ok(about.includes(line), `the line is ${line}: ${about}`);
  assert.equal(await browser.does(WAY), how.way);
  if (how.drops === true) {
    // The agent is heard out first: a session cuts what it is saying
    // where it was heard when its call drops, and says it again.
    const greeted = `[...document.querySelectorAll("#conversation li")].some(
      (item) => item.dataset.kind === "utterance" && item.firstChild.textContent === ${JSON.stringify(GREETING)},
    )`;
    await browser.until(greeted, "the greeting");
    await browser.does(`(() => {
      window.before = window.playground.client.welcome;
      window.playground.client.reconnect();
    })()`);
    await browser.until(`${OPEN} && window.playground.client.welcome !== window.before`, "the call again");
    const back = await browser.does("window.playground.client.welcome");
    assert.equal(back.resumed, true, JSON.stringify(back));
    assert.equal(back.session, await browser.does("window.before.session"));
    assert.equal(await browser.does(WAY), how.way);
  }
  // The page says where it has played to as it plays, and the session
  // takes the answer as heard to its end by that: it is not cut off,
  // and the call is not held open for a far end that says too little.
  await browser.until(ENDED, "the end of the call");
  /** @type {Kept[]} */
  const calls = await browser.does("window.calls");
  const shown = JSON.stringify(calls);
  const problem = await browser.does("window.playground.problem");
  assert.equal(await browser.does("window.playground.ended"), "ended", `${problem}: ${shown}`);
  await wentAsEver(browser);
  assert.equal(problem, undefined);
  // Nothing of it went over the line.
  assert.deepEqual(await browser.does("window.said"), []);
  assert.equal(calls.length, how.drops === true ? 2 : 1, shown);
  // Each call was offered once and answered. Nobody told the daemon
  // that the far end is gone, which would have ended the session: not
  // the page that came back on another call, and not the one whose
  // session the agent ended.
  /** @type {Asked[]} */
  const asked = await browser.does("window.asked");
  assert.deepEqual(
    asked.map(({ method, path, status }) => [method, path, status]),
    calls.map(() => ["POST", "/v1/webrtc", 201]),
    JSON.stringify(asked),
  );
  for (const call of calls) {
    assert.equal(call.said[0]?.type, "hello", shown);
    // Where the page had played to never went back, and was said with
    // how long the output takes.
    const played = call.said.filter((message) => message.type === "played_rtp");
    for (const [index, { ts, delay_ms: delay }] of played.entries()) {
      assert.ok(Number.isInteger(ts) && Number.isInteger(delay), shown);
      assert.ok(index === 0 || reached(ts, played[index - 1].ts), `what was played went back: ${shown}`);
    }
  }
  // The greeting and the answer were each sent whole, as long as the
  // stand-in for the synthesiser speaks, and the page said that it
  // played each to its end.
  const whole = calls.flatMap(responses);
  assert.deepEqual(
    whole.map(({ samples, heard }) => [samples, heard]),
    [
      [GREETED, true],
      [ANSWERED, true],
    ],
    shown,
  );
  // More than once for a response that lasts: a session that is
  // interrupted has to know how far its far end had heard.
  const last = /** @type {Kept} */ (calls.at(-1));
  assert.ok(last.said.filter((message) => message.type === "played_rtp").length >= 8, shown);
}

/**
 * A session that is written, on a line that drops in the middle of it
 * if `drops`.
 *
 * @param {Browser} browser
 * @param {boolean} drops
 */
async function written(browser, drops) {
  await browser.until(READY, "a page that is ready");
  await browser.does(`document.getElementById("write").click()`);
  await browser.until(`${OPEN} || window.playground.problem !== undefined`, "a session");
  assert.equal(await browser.does("window.playground.problem"), undefined);
  const about = await browser.does(`document.getElementById("about").textContent`);
  assert.ok(about.includes("text"), about);
  if (drops) {
    // The page lets go of its connection and comes back on another:
    // the daemon gives it the session again.
    await browser.does(`(() => {
      window.before = window.playground.client.welcome;
      window.playground.client.reconnect();
    })()`);
    await browser.until(`${OPEN} && window.playground.client.welcome !== window.before`, "the line again");
    const back = await browser.does("window.playground.client.welcome");
    assert.equal(back.resumed, true, JSON.stringify(back));
    assert.equal(back.session, await browser.does("window.before.session"));
  }
  await browser.does(`(() => {
    document.getElementById("text").value = ${JSON.stringify(QUESTION)};
    document.getElementById("say").requestSubmit();
  })()`);
  await browser.until(ENDED, "the end of the session");
  assert.equal(await browser.does("window.playground.ended"), "ended");
  await wentAsEver(browser);
}

async function main() {
  const program = chrome(asked.chrome);
  if (asked["can-run"] === true) {
    const node = Number(process.versions.node.split(".")[0]);
    if (node < NODE) {
      console.error(`this is Node ${process.versions.node}: the test needs ${NODE} or later`);
      process.exitCode = 1;
    }
    if (program === undefined) {
      console.error("there is no Chrome here: --chrome, or CHROME_BIN, names one");
      process.exitCode = 1;
    }
    return;
  }
  if (asked.daemon === undefined || !existsSync(asked.daemon)) {
    throw new Error("--daemon names the stand-in daemon, which `cargo build -p waveportd --example stand_in` makes");
  }
  const firefox = asked.firefox;
  if (firefox !== undefined && !existsSync(firefox)) {
    throw new Error("--firefox names a Firefox, and there is none there");
  }
  if (firefox === undefined && program === undefined) {
    throw new Error("there is no Chrome here: --chrome, or CHROME_BIN, names one");
  }
  // A Firefox that is packed as a snap reads nothing where what is
  // temporary is kept: its folder is under the repository's `target`.
  const under = firefox === undefined ? tmpdir() : join(import.meta.dirname, "../../../../target/tmp");
  await mkdir(under, { recursive: true });
  const folder = await mkdtemp(join(under, "waveport-browser-"));
  /** @type {import("../support/browser.js").Started | undefined} */
  let served;
  /** @type {Browser | undefined} */
  let browser;
  /** Where the daemon listens. @type {string | undefined} */
  let listens;
  const late = setTimeout(() => {
    console.error(`the browser test took longer than ${ALL_WITHIN / 1000} s`);
    process.exit(1);
  }, ALL_WITHIN);
  try {
    const question = join(folder, "question.wav");
    const calls = firefox === undefined ? "127.0.0.1" : await onTheNetwork();
    const { address, ...started } = await daemon(asked.daemon, question, calledAt(calls));
    served = started;
    listens = address;
    const profile = join(folder, "profile");
    /** @type {Browser} */
    let page;
    if (firefox === undefined) {
      page = new Chrome(/** @type {string} */ (program), profile, question);
    } else {
      await mkdir(profile);
      await writeFile(join(profile, "user.js"), FIREFOX_IS_TOLD);
      page = new Firefox(firefox, profile, question);
    }
    page.shows = SHOWN;
    browser = page;
    // Each step with the page it is on, where that is another than the
    // step before it was on. `?mic=raw`: the recording is heard as it
    // is.
    /** @type {[string | undefined, string, () => Promise<void>][]} */
    const steps = [
      [
        "?mic=raw&over=websocket",
        "a call that is spoken, as Opus",
        () => spoken(page, "opus at 48 kHz, over a WebSocket"),
      ],
      [undefined, "a session that is written", () => written(page, false)],
      [undefined, "a session whose line drops", () => written(page, true)],
      [
        undefined,
        "a call that is spoken, as plain samples",
        async () => {
          // A browser that cannot code Opus writes plain samples.
          await page.does("window.AudioEncoder = undefined");
          await spoken(page, "pcm16 at 48 kHz, over a WebSocket");
        },
      ],
      // Nothing says what a call is made over: it is made over WebRTC,
      // which gets through here.
      ["?mic=raw", "a call over WebRTC", () => called(page, { way: "udp" })],
      [undefined, "a call over WebRTC that only a connection carries", () => called(page, { ways: "tcp", way: "tcp" })],
      [undefined, "a call over WebRTC that drops", () => called(page, { way: "udp", drops: true })],
      // The page gives a call less than a second to be there: the
      // recording goes on while it waits, and the question is in it
      // after three.
      [
        "?mic=raw&within=800",
        "a call that does not get through, and is made over a WebSocket",
        () => spoken(page, "opus at 48 kHz, over a WebSocket", "none"),
      ],
    ];
    await page.open(KEEPS_WHAT_IS_SAID);
    /** The page the step before was on. @type {string | undefined} */
    let on;
    for (const [asks, name, step] of steps) {
      on = asks ?? on;
      if (asked.only !== undefined && !name.includes(asked.only)) {
        continue;
      }
      if (asks !== undefined || asked.only !== undefined) {
        await page.goes(`${address}/playground/${on}`);
      }
      const began = Date.now();
      await step();
      console.log(`ok  ${name} (${Date.now() - began} ms)`);
    }
    // The page ran what the daemon served and nothing went wrong on it.
    const wrong = browser.noted.filter((line) => /^(thrown|error|warning)/.test(line));
    assert.deepEqual(wrong, []);
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    console.error(`--- the daemon said ---\n${served?.said.join("") ?? ""}`);
    if (listens !== undefined) {
      // What became of its sessions, for whoever reads this.
      const sessions = await fetch(`${listens}/v1/sessions`)
        .then((answered) => answered.text())
        .catch((failed) => String(failed));
      console.error(`--- its sessions ---\n${sessions}`);
    }
    process.exitCode = 1;
  } finally {
    clearTimeout(late);
    if (asked.notes === true) {
      console.log(browser?.noted.join("\n") ?? "");
    }
    await browser?.close();
    served?.child.kill("SIGTERM");
    // What a browser started may write to its folder a moment longer
    // than the browser lives.
    for (let tries = 0; tries < 10 && existsSync(folder); tries += 1) {
      await rm(folder, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }).catch(() => {});
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  }
}

await main();
