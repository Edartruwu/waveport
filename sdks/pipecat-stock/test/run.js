// The stock client of Pipecat's in a browser, against a daemon: what an
// application that was written for a Pipecat server sees of a Waveport
// daemon when it is given the daemon's address and nothing else.
//
//   node test/run.js --daemon <stand_in> [--chrome <chrome>] [--notes] [--only <words>]
//   node test/run.js --can-run
//
// The client is `@pipecat-ai/client-js` with
// `@pipecat-ai/small-webrtc-transport`, as published: `page/stock.js`
// is all there is of a page, bundled here with esbuild as an
// application's build would. It is served from another address than
// the daemon's, as an application is, so the daemon answers a page from
// elsewhere that it was told of.
//
// The daemon is `apps/waveportd/examples/stand_in.rs`, the real daemon
// with stand-ins for its providers, and the browser a Chrome that is
// driven over its pipe (`sdks/js/test/support/browser.js`): no sound is
// made, no microphone is opened, and nothing beyond this machine is
// asked for anything. The browser is told that no name leads anywhere,
// so that what the client's packages would ask elsewhere of their own
// accord is not asked, and what they tried is counted.
//
// `--notes` prints what the page wrote to its console, `--only` runs
// the steps whose names have `<words>` in them, and `--can-run` says
// whether there is what the test needs and runs nothing.

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";

import { build } from "esbuild";

import { Chrome, NODE, calledAt, chrome, daemon } from "../../js/test/support/browser.js";

/** How long the whole of it may take. */
const ALL_WITHIN = 180_000;

const { values: asked } = parseArgs({
  options: {
    daemon: { type: "string" },
    chrome: { type: "string" },
    notes: { type: "boolean" },
    only: { type: "string" },
    "can-run": { type: "boolean" },
  },
});

/** The version of RTVI the gateway speaks. */
const RTVI = "2.3.0";

/** What the agent says first, is asked and answers, as the stand-ins have it. */
const GREETING = "Good morning.";
const QUESTION = "What time is it right now?";
const ANSWER = "It is ten past three. Goodbye!";

/** What a client writes, which is not what the recogniser hears. */
const WRITTEN = "What time is it, please?";

/** What a client tells its application of what an agent says. */
const SAID_BY_AN_AGENT = [
  "onBotStartedSpeaking",
  "onBotStoppedSpeaking",
  "onBotLlmStarted",
  "onBotLlmStopped",
  "onBotLlmText",
  "onBotTtsStarted",
  "onBotTtsStopped",
  "onBotTtsText",
  "onBotOutput",
];

/** Where the package is, and the packages it is a test of. */
const HERE = join(import.meta.dirname, "..");
const CLIENT = "@pipecat-ai/client-js";
const TRANSPORT = "@pipecat-ai/small-webrtc-transport";

/** The version of `name` that is installed. */
function installed(name) {
  return JSON.parse(readFileSync(join(HERE, "node_modules", name, "package.json"), "utf8")).version;
}

/**
 * Serves the page from an address of its own, as an application's is.
 *
 * @returns {Promise<{ address: string, close: () => Promise<void> }>}
 */
async function served() {
  const bundled = await build({
    entryPoints: [join(HERE, "page/stock.js")],
    bundle: true,
    format: "esm",
    platform: "browser",
    write: false,
    logLevel: "silent",
  });
  const pages = new Map([
    ["/", ["text/html; charset=utf-8", await readFile(join(HERE, "page/index.html"))]],
    ["/stock.js", ["text/javascript; charset=utf-8", bundled.outputFiles[0].contents]],
  ]);
  const server = createServer((request, response) => {
    const page = pages.get(new URL(request.url ?? "/", "http://page").pathname);
    if (page === undefined) {
      response.writeHead(404).end();
    } else {
      response.writeHead(200, { "Content-Type": page[0], "Cache-Control": "no-store" }).end(page[1]);
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(undefined)));
  const { port } = /** @type {import("node:net").AddressInfo} */ (server.address());
  return {
    address: `http://127.0.0.1:${port}`,
    close: () => new Promise((resolve) => server.close(() => resolve(undefined))),
  };
}

/**
 * Keeps what the page asks the daemon (`window.asked`: how, where, what
 * was answered, and what an offer said of a call there was), and every
 * call it makes (`window.peers`).
 */
const KEEPS_WHAT_IS_ASKED = `(() => {
  window.asked = [];
  window.peers = [];
  const asks = window.fetch;
  window.fetch = async function (at, how) {
    // What is asked is given as an address and how, or as one thing
    // that holds both.
    const whole = at instanceof Request ? at : undefined;
    const body = whole === undefined ? how?.body : await whole.clone().text();
    const answered = await asks.call(this, at, how);
    const { pathname, search } = new URL(whole?.url ?? String(at), location.href);
    let said;
    try {
      said = JSON.parse(body);
    } catch {
      said = undefined;
    }
    window.asked.push({
      method: how?.method ?? whole?.method ?? "GET",
      path: pathname + search,
      status: answered.status,
      call: said?.pc_id ?? null,
      restart: said?.restart_pc ?? null,
    });
    return answered;
  };
  const Peer = window.RTCPeerConnection;
  window.RTCPeerConnection = class extends Peer {
    constructor(...of) {
      super(...of);
      window.peers.push(this);
    }
  };
})()`;

/** What the page says of where it is, for whoever reads that something did not come about. */
const SHOWN = `({
  state: window.stock?.client?.state,
  told: window.stock?.told.map(([name, first]) => name + " " + JSON.stringify(first ?? "").slice(0, 80)).slice(-40),
  asked: window.asked,
  peers: window.peers.map((peer) => peer.connectionState),
})`;

/**
 * What comes of `promise` on the page, which is to come within a
 * moment: a client that waits for ever for its server says nothing.
 *
 * @param {Chrome} browser
 * @param {string} promise
 * @param {string} what What is waited for, for whoever reads that it did not come.
 * @returns {Promise<any>}
 */
async function comes(browser, promise, what) {
  const came = await browser.does(`Promise.race([
    (${promise}).then(
      (value) => ({ came: true, value }),
      (refused) => ({ came: false, why: String(refused?.stack ?? refused) }),
    ),
    new Promise((resolve) => setTimeout(() => resolve({ came: false, why: "nothing came of it" }), 20000)),
  ])`);
  if (came.came !== true) {
    const page = await browser.does(`JSON.stringify(${SHOWN}, null, 1)`);
    throw new Error(`${what} did not come about: ${came.why}\n${page}\n${browser.noted.join("\n")}`);
  }
  return came.value;
}

/** Whether the client was told `name`, at least `times` times. */
const told = (name, times = 1) =>
  `window.stock.told.filter(([name]) => name === ${JSON.stringify(name)}).length >= ${times}`;

/**
 * What the client told its application under `name`: what each call
 * was given first.
 *
 * @param {Chrome} browser
 * @param {string} name
 * @returns {Promise<any[]>}
 */
function tellings(browser, name) {
  return browser.does(
    `window.stock.told.filter(([name]) => name === ${JSON.stringify(name)}).map(([, first]) => first ?? null)`,
  );
}

/**
 * The names of what the client told its application, in order.
 *
 * @param {Chrome} browser
 * @returns {Promise<string[]>}
 */
function names(browser) {
  return browser.does("window.stock.told.map(([name]) => name)");
}

/**
 * The sessions the daemon has.
 *
 * @param {string} address
 * @returns {Promise<any[]>}
 */
async function sessions(address) {
  const answered = await fetch(`${address}/v1/sessions`);
  return (await answered.json()).sessions;
}

/**
 * Waits until the daemon has `count` sessions.
 *
 * @param {string} address
 * @param {number} count
 */
async function hasSessions(address, count) {
  const began = Date.now();
  for (;;) {
    const has = await sessions(address);
    if (has.length === count) {
      return has;
    }
    assert.ok(Date.now() - began < 10_000, `the daemon has ${JSON.stringify(has)}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

/** What the page asked the daemon: how, where and what was answered. */
const asks = (browser) => browser.does("window.asked");

/**
 * The agent was heard: sound arrived on the call, and was not silence.
 *
 * @param {Chrome} browser
 */
async function wasHeard(browser) {
  const heard = await browser.does(`(async () => {
    const stats = await window.peers.at(-1).getStats();
    for (const one of stats.values()) {
      if (one.type === "inbound-rtp" && one.kind === "audio") {
        return { packets: one.packetsReceived, energy: one.totalAudioEnergy };
      }
    }
    return undefined;
  })()`);
  assert.ok(heard !== undefined && heard.packets > 10 && heard.energy > 0, JSON.stringify(heard));
}

/**
 * The client is ready on a call: its server answered that it was, in
 * the version the gateway speaks, and it was told nothing went wrong.
 *
 * @param {Chrome} browser
 * @param {any} ready What its `connect` came to.
 */
async function isReady(browser, ready) {
  assert.equal(ready?.version, RTVI, JSON.stringify(ready));
  assert.equal(ready.about?.library, "waveport", JSON.stringify(ready));
  assert.equal(await browser.does("window.stock.client.state"), "ready");
  assert.equal(await browser.does("window.stock.client.connected"), true);
  const [told] = await tellings(browser, "onBotReady");
  assert.equal(told?.version, RTVI, JSON.stringify(told));
}

/**
 * The client was told the agent's greeting, as text that was spoken
 * and that no model wrote.
 *
 * @param {Chrome} browser
 */
async function wasGreeted(browser) {
  await browser.until(told("onBotStoppedSpeaking"), "the end of the greeting");
  const outputs = await tellings(browser, "onBotOutput");
  const shown = JSON.stringify(outputs);
  assert.equal(outputs[0]?.text, GREETING, shown);
  assert.equal(outputs[0]?.will_be_spoken, true, shown);
  assert.equal(outputs.at(-1)?.spoken_status, "completed", shown);
  assert.deepEqual(await tellings(browser, "onBotLlmText"), []);
  const order = await names(browser);
  const places = ["onBotTtsStarted", "onBotStartedSpeaking", "onBotTtsStopped", "onBotStoppedSpeaking"].map((name) =>
    order.indexOf(name),
  );
  assert.deepEqual(places, places.toSorted((a, b) => a - b), JSON.stringify(order));
  assert.ok(places[0] >= 0, JSON.stringify(order));
}

/**
 * The client was told the answer, from the model to the last word that
 * was heard, with the tool the agent called by its name alone, and
 * then that its bot left: it ended its call, and made no other.
 *
 * @param {Chrome} browser
 * @param {string} address Where the daemon is.
 * @param {number} offers How many offers it had made by then.
 */
async function wasAnsweredAndLeft(browser, address, offers) {
  await browser.until(told("onDisconnected"), "the end of the call");
  const order = await names(browser);
  const shown = JSON.stringify(order);
  const outputs = await tellings(browser, "onBotOutput");
  const answer = outputs.filter((output) => output.spoken_status === "new" && output.text !== GREETING);
  assert.equal(answer.map((output) => output.text).join(""), ANSWER, JSON.stringify(outputs));
  assert.equal((await tellings(browser, "onBotLlmText")).map((said) => said.text).join(""), ANSWER);
  const heard = (await tellings(browser, "onBotTtsText")).map((said) => said.text).join("");
  assert.ok(heard.endsWith(ANSWER), heard);
  for (const [begins, ends] of [
    ["onBotLlmStarted", "onBotLlmStopped"],
    ["onBotTtsStarted", "onBotTtsStopped"],
    ["onBotStartedSpeaking", "onBotStoppedSpeaking"],
  ]) {
    const count = (name) => order.filter((told) => told === name).length;
    assert.ok(count(begins) > 0 && count(begins) === count(ends), `${begins}: ${shown}`);
  }
  // The tool that ends the call, by its name: nothing of what it was
  // given or answered.
  const [called] = await tellings(browser, "onLLMFunctionCallStarted");
  assert.deepEqual(called, { function_name: "end_call" }, shown);
  const [stopped] = await tellings(browser, "onLLMFunctionCallStopped");
  assert.equal(stopped?.function_name, "end_call", shown);
  assert.equal(stopped.cancelled, false);
  assert.equal(stopped.result, undefined);
  // Its bot left, and it left: in that order, and with nothing wrong.
  assert.ok(order.indexOf("onBotDisconnected") >= 0, shown);
  assert.ok(order.indexOf("onBotDisconnected") < order.lastIndexOf("onDisconnected"), shown);
  for (const wrong of ["onError", "onMessageError", "onDeviceError", "onUnsupportedFeature"]) {
    assert.deepEqual(await tellings(browser, wrong), [], wrong);
  }
  assert.equal(await browser.does("window.stock.client.state"), "disconnected");
  // It does not take the end of its call for a network that failed
  // and call again: it has let go of its call, and makes no other.
  await new Promise((resolve) => setTimeout(resolve, 2_500));
  const offered = (await asks(browser)).filter((one) => one.method === "POST" && one.path.includes("/api/offer"));
  assert.equal(offered.length, offers, JSON.stringify(offered));
  await hasSessions(address, 0);
}

/**
 * A call that is spoken, at the address a client is given when it is
 * given one: the daemon has one agent, and nobody need say which.
 *
 * @param {Chrome} browser
 * @param {string} address
 */
async function spoken(browser, address) {
  await browser.does("window.stock.begin({ mic: true })");
  const offered = `window.stock.client.connect({
    webrtcRequestParams: { endpoint: ${JSON.stringify(`${address}/api/offer`)} },
  })`;
  const ready = await comes(browser, offered, "a call");
  await isReady(browser, ready);
  await wasGreeted(browser);
  await wasHeard(browser);
  // The recording asks, and what was recognised of it is told as it
  // is and as the older message has it.
  await browser.until(told("onUserStoppedSpeaking"), "the end of the question");
  await wasAnsweredAndLeft(browser, address, 1);
  const inputs = (await tellings(browser, "onUserInput")).filter((input) => input.final === true);
  assert.equal(inputs.at(-1)?.text, QUESTION, JSON.stringify(inputs));
  assert.equal(inputs.at(-1)?.input_type, "transcription");
  const transcripts = (await tellings(browser, "onUserTranscript")).filter((transcript) => transcript.final === true);
  assert.equal(transcripts.at(-1)?.text, QUESTION, JSON.stringify(transcripts));
  // One offer, and where the browser was is said after it, as it
  // finds out: every one of those is taken.
  const asked = await asks(browser);
  assert.deepEqual(
    asked.filter((one) => one.method === "POST").map((one) => [one.path, one.status, one.call, one.restart]),
    [["/api/offer", 200, null, false]],
    JSON.stringify(asked),
  );
  const trickled = asked.filter((one) => one.method === "PATCH");
  assert.ok(trickled.length > 0, JSON.stringify(asked));
  assert.ok(
    trickled.every((one) => one.status === 200 && one.call !== null),
    JSON.stringify(asked),
  );
}

/**
 * A session that is started first, as an application does that has a
 * server start its bot: the call is made under the name it is given.
 * Its network then fails, as the client sees it, and it comes back on
 * another call to the session it was on. It writes there, and is
 * answered.
 *
 * @param {Chrome} browser
 * @param {string} address
 */
async function started(browser, address) {
  await browser.does("window.stock.begin({ mic: false })");
  const begun = `window.stock.client.startBotAndConnect({
    endpoint: ${JSON.stringify(`${address}/start`)},
    requestData: { transport: "webrtc" },
  })`;
  const ready = await comes(browser, begun, "a session that is started, and a call to it");
  await isReady(browser, ready);
  await wasGreeted(browser);
  const [session] = await hasSessions(address, 1);
  let asked = await asks(browser);
  const posted = asked.filter((one) => one.method === "POST");
  assert.deepEqual(posted[0], { method: "POST", path: "/start", status: 200, call: null, restart: null });
  const under = /^\/sessions\/([0-9a-f]{32})\/api\/offer$/.exec(posted[1]?.path);
  assert.ok(under !== null && posted[1].status === 200 && posted.length === 2, JSON.stringify(posted));
  // The name is not the session, which stays the daemon's to know.
  assert.notEqual(under[1], session.session);

  // What the client does when its network has failed: it makes another
  // call in the place of the one it names.
  const first = await browser.does("window.stock.transport.pc_id");
  const before = (await names(browser)).length;
  await browser.does("void window.stock.transport.attemptReconnection(true)");
  await browser.until(
    `window.peers.length === 2 && window.peers[1].connectionState === "connected" && window.stock.transport.pc_id !== ${JSON.stringify(first)}`,
    "the call in the place of the first",
  );
  asked = await asks(browser);
  const again = asked.filter((one) => one.method === "POST").at(-1);
  assert.deepEqual([again?.path, again?.status, again?.call, again?.restart], [posted[1].path, 200, first, true]);
  // It is on the session it was on, and is still ready there.
  assert.deepEqual(
    (await hasSessions(address, 1)).map((one) => one.session),
    [session.session],
  );
  assert.equal(await browser.does("window.stock.client.state"), "ready");
  // Nothing the agent said is told again: it was said before the call.
  // (No turn was taken before it either, so this does not hold that a
  // turn is not told twice: the daemon's own tests do.)
  await new Promise((resolve) => setTimeout(resolve, 300));
  const since = (await names(browser)).slice(before);
  assert.deepEqual(
    since.filter((name) => SAID_BY_AN_AGENT.includes(name)),
    [],
    JSON.stringify(since),
  );

  // What it writes is a turn, told back as what it sent and not as
  // something that was recognised. (The stand-in for the recogniser
  // says its words to whatever it is sent, which here is the silence
  // of a microphone that is off: words in passing are told, and no
  // turn comes of them.)
  await browser.does(`window.stock.client.sendText(${JSON.stringify(WRITTEN)})`);
  await wasAnsweredAndLeft(browser, address, 2);
  const inputs = (await tellings(browser, "onUserInput")).filter((input) => input.final === true);
  assert.equal(inputs.length, 1, JSON.stringify(inputs));
  assert.equal(inputs[0].text, WRITTEN);
  assert.equal(inputs[0].input_type, "chat");
  assert.equal(typeof inputs[0].msg_id, "string");
  const transcripts = (await tellings(browser, "onUserTranscript")).filter((transcript) => transcript.final === true);
  assert.deepEqual(transcripts, []);
}

/**
 * What a session says of itself to whoever follows it, until `last`
 * is in it.
 *
 * @param {string} address
 * @param {string} session
 * @param {string} last
 */
async function follows(address, session, last) {
  const within = AbortSignal.timeout(10_000);
  const answered = await fetch(`${address}/v1/sessions/${session}/events`, { signal: within });
  assert.equal(answered.status, 200);
  let said = "";
  for await (const part of /** @type {any} */ (answered.body).pipeThrough(new TextDecoderStream())) {
    said += part;
    if (said.includes(last)) {
      return said;
    }
  }
  throw new Error(`the session never said ${last}: ${said}`);
}

/**
 * A page that was given a client token, as the page of an application
 * is whose own server made the session: it says the token with what it
 * asks, and is on that session. Keys that are pressed there are a turn
 * of keys, as a telephone's are.
 *
 * @param {Chrome} browser
 * @param {string} address
 */
async function pressed(browser, address) {
  const json = { "Content-Type": "application/json" };
  const made = await fetch(`${address}/v1/sessions`, { method: "POST", headers: json, body: '{"agent":"desk"}' });
  assert.equal(made.status, 201);
  const { session: named, client_token: token } = await made.json();
  await browser.does("window.stock.begin({ mic: false })");
  const offered = `window.stock.client.connect({
    webrtcRequestParams: {
      endpoint: ${JSON.stringify(`${address}/api/offer`)},
      headers: new Headers({ Authorization: ${JSON.stringify(`Bearer ${token}`)} }),
    },
  })`;
  const ready = await comes(browser, offered, "a call");
  await isReady(browser, ready);
  await wasGreeted(browser);
  // It is on the session its token was made for, and no other was
  // made. The token was used up by its coming: it starts nothing more.
  const [session] = await hasSessions(address, 1);
  assert.equal(session.session, named);
  const again = await fetch(`${address}/start`, {
    method: "POST",
    headers: { ...json, Authorization: `Bearer ${token}` },
    body: '{"transport":"webrtc"}',
  });
  assert.equal(again.status, 401);
  const followed = follows(address, session.session, '"keys_committed"');
  await browser.does(`window.stock.client.sendDTMF("12#")`);
  // The hash ends what was keyed, and is no part of it.
  const said = await followed;
  assert.match(said, /"kind":"keys_committed"[^\n]*"keys":"12"/);
  await wasAnsweredAndLeft(browser, address, 1);
  // Keys are not words: nothing is told of them as of something said.
  const inputs = (await tellings(browser, "onUserInput")).filter((input) => input.final === true);
  assert.deepEqual(inputs, []);
}

/**
 * A client that has its bot leave is told that it left, and makes no
 * other call.
 *
 * @param {Chrome} browser
 * @param {string} address
 */
async function dismissed(browser, address) {
  await browser.does("window.stock.begin({ mic: false })");
  const offered = `window.stock.client.connect({
    webrtcRequestParams: { endpoint: ${JSON.stringify(`${address}/api/offer?agent=desk`)} },
  })`;
  const ready = await comes(browser, offered, "a call");
  await isReady(browser, ready);
  await wasGreeted(browser);
  await hasSessions(address, 1);
  await browser.does("window.stock.client.disconnectBot()");
  await browser.until(told("onDisconnected"), "the end of the call");
  const order = await names(browser);
  assert.ok(order.indexOf("onBotDisconnected") >= 0, JSON.stringify(order));
  assert.deepEqual(await tellings(browser, "onError"), []);
  await hasSessions(address, 0);
  await new Promise((resolve) => setTimeout(resolve, 2_500));
  const posted = (await asks(browser)).filter((one) => one.method === "POST");
  assert.equal(posted.length, 1, JSON.stringify(posted));
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
  if (program === undefined) {
    throw new Error("there is no Chrome here: --chrome, or CHROME_BIN, names one");
  }
  console.log(`${CLIENT} ${installed(CLIENT)} with ${TRANSPORT} ${installed(TRANSPORT)}`);
  const folder = await mkdtemp(join(tmpdir(), "waveport-stock-"));
  /** @type {import("../../js/test/support/browser.js").Started | undefined} */
  let served_by;
  /** @type {Chrome | undefined} */
  let browser;
  /** @type {{ address: string, close: () => Promise<void> } | undefined} */
  let page;
  /** What the page asked of anybody but this machine. @type {string[]} */
  const elsewhere = [];
  const late = setTimeout(() => {
    console.error(`the test took longer than ${ALL_WITHIN / 1000} s`);
    process.exit(1);
  }, ALL_WITHIN);
  try {
    page = await served();
    const question = join(folder, "question.wav");
    // The daemon is told of the page's address: a page from anywhere
    // else may ask it nothing.
    const more = `allowed_origins = ["${page.address}"]\n${calledAt("127.0.0.1")}`;
    const { address, ...begun } = await daemon(asked.daemon, question, more);
    served_by = begun;
    // No name leads anywhere: what the page asks is asked of this
    // machine, or of nobody.
    const chrome = new Chrome(program, join(folder, "profile"), question, [
      "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1",
    ]);
    browser = chrome;
    chrome.shows = SHOWN;
    await chrome.open(KEEPS_WHAT_IS_ASKED);
    chrome.on("Network.requestWillBeSent", ({ request }) => {
      const to = new URL(request.url);
      if (to.hostname !== "127.0.0.1" && to.protocol !== "data:" && to.protocol !== "blob:") {
        elsewhere.push(to.origin);
      }
    });
    await chrome.ask("Network.enable");
    /** @type {[string, () => Promise<void>][]} */
    const steps = [
      ["a call that is spoken", () => spoken(chrome, address)],
      ["a session that is started, comes back on another call and is written to", () => started(chrome, address)],
      ["a client token, and keys that are pressed", () => pressed(chrome, address)],
      ["a bot that is told to leave", () => dismissed(chrome, address)],
    ];
    for (const [name, step] of steps) {
      if (asked.only !== undefined && !name.includes(asked.only)) {
        continue;
      }
      // Every step is a page of its own, as an application that is
      // opened again is.
      await chrome.goes(`${page.address}/`);
      assert.equal(await chrome.does("window.stock.speaks"), RTVI);
      const began = Date.now();
      await step();
      console.log(`ok  ${name} (${Date.now() - began} ms)`);
    }
    // Nothing went wrong on the page.
    const wrong = chrome.noted.filter((line) => /^(thrown|error)/.test(line));
    assert.deepEqual(wrong, []);
    if (elsewhere.length > 0) {
      const places = [...new Set(elsewhere)].join(", ");
      console.log(`the client's packages asked ${places}, and were let ask nobody`);
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    console.error(`--- the daemon said ---\n${served_by?.said.join("") ?? ""}`);
    process.exitCode = 1;
  } finally {
    clearTimeout(late);
    if (asked.notes === true) {
      console.log(browser?.noted.join("\n") ?? "");
    }
    await browser?.close();
    served_by?.child.kill("SIGTERM");
    await page?.close();
    for (let tries = 0; tries < 10 && existsSync(folder); tries += 1) {
      await rm(folder, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }).catch(() => {});
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  }
}

await main();
