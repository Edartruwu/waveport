// @ts-check
// A browser on the page a daemon serves, with a recording for a
// microphone: a call that is spoken, one that is written, and one whose
// line drops.
//
//   node test/browser/run.js --daemon <stand_in> [--chrome <chrome>] [--notes]
//   node test/browser/run.js --can-run
//
// `--notes` prints what the page wrote to its console, for whoever
// looks into a page that does not do what it should. `--can-run` says
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
// The agent greets whoever comes on its line, at once. A page that
// was not ready to play when it was welcomed would not play that, and
// would not say that it had. The session would not notice: it takes a
// far end that says nothing of what it played to play as it is sent.
// So what the page said it played is looked at here, run by run.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";

/** How long the whole of it may take, and one step of it. */
const ALL_WITHIN = 180_000;
const STEP_WITHIN = 45_000;

const { values: asked } = parseArgs({
  options: {
    daemon: { type: "string" },
    chrome: { type: "string" },
    notes: { type: "boolean" },
    "can-run": { type: "boolean" },
  },
});

/** The first Node whose npm keeps a version out until it is a week old. */
const NODE = 24;

/** Where a Chrome is, if one is. */
function chrome() {
  const places = [
    asked.chrome,
    process.env.CHROME_BIN,
    "/usr/bin/google-chrome",
    "/opt/google/chrome/chrome",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  ];
  return places.find((place) => place !== undefined && existsSync(place));
}

/**
 * A program that was started, and what it said.
 *
 * @typedef {{ child: import("node:child_process").ChildProcess, said: string[] }} Started
 */

/**
 * Starts the daemon and waits until it says where it listens.
 *
 * @param {string} program
 * @param {string} question Where it writes what its recogniser hears.
 * @returns {Promise<Started & { address: string }>}
 */
function daemon(program, question) {
  // It is given nothing of the environment it does not need: no key of
  // anyone's is its to see.
  const env = Object.fromEntries(
    ["PATH", "HOME", "TMPDIR", "SystemRoot", "WAVEPORT_ASSETS"].flatMap((name) => {
      const value = process.env[name];
      return value === undefined ? [] : [[name, value]];
    }),
  );
  const child = spawn(program, ["--question", question, "--sessions", "6"], {
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  /** @type {string[]} */
  const said = [];
  child.stderr.setEncoding("utf8").on("data", (text) => said.push(text));
  return new Promise((resolve, reject) => {
    let told = "";
    const late = setTimeout(() => reject(new Error(`the daemon did not start: ${said.join("")}`)), 30_000);
    child.stdout.setEncoding("utf8").on("data", (text) => {
      told += text;
      const listening = /listening on (http:\/\/\S+)/.exec(told);
      if (listening?.[1] !== undefined) {
        clearTimeout(late);
        resolve({ child, said, address: listening[1] });
      }
    });
    child.on("exit", (code) => {
      clearTimeout(late);
      reject(new Error(`the daemon stopped with ${code}: ${said.join("")}`));
    });
  });
}

/** A browser that is driven over its pipe. */
class Browser {
  /** @type {import("node:child_process").ChildProcess} */
  #child;

  /** @type {import("node:stream").Writable} */
  #to;

  #next = 1;

  /** @type {Map<number, { resolve: (result: any) => void, reject: (error: Error) => void }>} */
  #waiting = new Map();

  /** @type {Map<string, ((params: any) => void)[]>} */
  #listeners = new Map();

  /** The session of the page that is driven. @type {string | undefined} */
  #page;

  /** What went wrong on the page, and what it wrote. @type {string[]} */
  noted = [];

  /**
   * @param {string} program
   * @param {string} profile A folder for it that is nobody's.
   * @param {string} microphone The recording it hears in place of one.
   */
  constructor(program, profile, microphone) {
    const args = [
      "--headless",
      `--user-data-dir=${profile}`,
      "--remote-debugging-pipe",
      "--no-first-run",
      "--no-default-browser-check",
      // A recording for a microphone, played once, and nobody asked.
      "--use-fake-device-for-media-stream",
      "--use-fake-ui-for-media-stream",
      `--use-file-for-fake-audio-capture=${microphone}%noloop`,
      // What the page plays goes nowhere: no sound is made here.
      "--disable-audio-output",
      "--mute-audio",
      "--autoplay-policy=no-user-gesture-required",
      // It asks nothing of anybody but the daemon.
      "--disable-background-networking",
      "--disable-component-update",
      "--disable-sync",
      "--disable-default-apps",
      "--disable-extensions",
      "--no-pings",
    ];
    this.#child = spawn(program, args, { stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"] });
    const [, , , to, from] = this.#child.stdio;
    this.#to = /** @type {import("node:stream").Writable} */ (to);
    let held = "";
    /** @type {import("node:stream").Readable} */ (from).setEncoding("utf8").on("data", (text) => {
      held += text;
      const messages = held.split("\0");
      held = messages.pop() ?? "";
      for (const message of messages) {
        this.#heard(JSON.parse(message));
      }
    });
  }

  /** @param {any} message */
  #heard(message) {
    if (message.id !== undefined) {
      const waiting = this.#waiting.get(message.id);
      this.#waiting.delete(message.id);
      if (message.error !== undefined) {
        waiting?.reject(new Error(`the browser refused: ${message.error.message}`));
      } else {
        waiting?.resolve(message.result);
      }
    } else {
      for (const listener of this.#listeners.get(message.method) ?? []) {
        listener(message.params);
      }
    }
  }

  /**
   * @param {string} method
   * @param {object} [params]
   * @returns {Promise<any>}
   */
  ask(method, params = {}) {
    const id = this.#next;
    this.#next += 1;
    const sessionId = method.startsWith("Target.") || method.startsWith("Browser.") ? undefined : this.#page;
    this.#to.write(`${JSON.stringify({ id, method, params, sessionId })}\0`);
    return new Promise((resolve, reject) => {
      this.#waiting.set(id, { resolve, reject });
    });
  }

  /**
   * @param {string} event
   * @param {(params: any) => void} listener
   */
  on(event, listener) {
    this.#listeners.set(event, [...(this.#listeners.get(event) ?? []), listener]);
  }

  /** Opens a page at `address` and waits until it has loaded. */
  async open(/** @type {string} */ address) {
    const { targetId } = await this.ask("Target.createTarget", { url: "about:blank" });
    const { sessionId } = await this.ask("Target.attachToTarget", { targetId, flatten: true });
    this.#page = sessionId;
    this.on("Runtime.exceptionThrown", ({ exceptionDetails: thrown }) => {
      this.noted.push(`thrown: ${thrown.exception?.description ?? thrown.text}`);
    });
    this.on("Runtime.consoleAPICalled", ({ type, args }) => {
      const words = args.map((/** @type {any} */ arg) => arg.value ?? arg.description).join(" ");
      this.noted.push(`${type}: ${words}`);
    });
    this.on("Log.entryAdded", ({ entry }) => {
      this.noted.push(`${entry.level}: ${entry.text} ${entry.url ?? ""}`);
    });
    await Promise.all([this.ask("Runtime.enable"), this.ask("Log.enable"), this.ask("Page.enable")]);
    const loaded = new Promise((resolve) => this.on("Page.loadEventFired", resolve));
    await this.ask("Page.navigate", { url: address });
    await loaded;
  }

  /**
   * Has the page work `expression` out, as if its user had asked, and
   * returns what comes of it.
   *
   * @param {string} expression
   * @returns {Promise<any>}
   */
  async does(expression) {
    const { result, exceptionDetails: thrown } = await this.ask("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
      userGesture: true,
    });
    if (thrown !== undefined) {
      throw new Error(`the page threw: ${thrown.exception?.description ?? thrown.text}`);
    }
    return result.value;
  }

  /**
   * Waits until `expression` is true of the page.
   *
   * @param {string} expression
   * @param {string} what What is waited for, for whoever reads that it did not come.
   */
  async until(expression, what) {
    const came = await this.does(`new Promise((resolve) => {
      const began = Date.now();
      const look = () => {
        if (${expression}) {
          resolve(true);
        } else if (Date.now() - began > ${STEP_WITHIN}) {
          resolve(false);
        } else {
          setTimeout(look, 25);
        }
      };
      look();
    })`);
    if (came !== true) {
      const page = await this.does(`JSON.stringify({
        status: document.getElementById("status").textContent,
        problem: window.playground?.problem,
        ended: window.playground?.ended,
        log: [...document.querySelectorAll("#log li")].map((line) => line.textContent).slice(-25),
      }, null, 1)`);
      throw new Error(`${what} did not come about.\n${page}\n${this.noted.join("\n")}`);
    }
  }

  /** What the page shows of the conversation: who, and what. */
  conversation() {
    return this.does(`[...document.querySelectorAll("#conversation li")].map((item) => [
      item.dataset.kind,
      item.firstChild.textContent,
      item.querySelector("small")?.textContent,
    ])`);
  }

  /** Closes the browser, and returns once it is gone. */
  async close() {
    const gone = new Promise((resolve) => this.#child.once("exit", resolve));
    this.ask("Browser.close").catch(() => {});
    const late = setTimeout(() => this.#child.kill("SIGKILL"), 5_000);
    await gone;
    clearTimeout(late);
  }
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
 * Keeps what the page says to the daemon on its line, for the test to
 * read: `window.said`.
 */
const KEEPS_WHAT_IS_SAID = `(() => {
  window.said = [];
  const send = WebSocket.prototype.send;
  WebSocket.prototype.send = function (data) {
    if (typeof data === "string") {
      window.said.push(JSON.parse(data));
    }
    return send.call(this, data);
  };
})()`;

/**
 * What the page shows of a session that went as every session of the
 * stand-in goes: the greeting, the question and the answer, each heard
 * or read to its end.
 *
 * @param {Browser} browser
 */
async function wentAsEver(browser) {
  const said = await browser.conversation();
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
 * A call that is spoken: the browser hears the question from its
 * recording, sends it as `line` says, and plays the answer.
 *
 * @param {Browser} browser
 * @param {string} line How the audio is to be written.
 */
async function spoken(browser, line) {
  await browser.until(READY, "a page that is ready");
  await browser.does("window.said = []");
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
  const program = chrome();
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
  const folder = await mkdtemp(join(tmpdir(), "waveport-browser-"));
  /** @type {Started | undefined} */
  let served;
  /** @type {Browser | undefined} */
  let browser;
  const late = setTimeout(() => {
    console.error(`the browser test took longer than ${ALL_WITHIN / 1000} s`);
    process.exit(1);
  }, ALL_WITHIN);
  try {
    const question = join(folder, "question.wav");
    const { address, ...started } = await daemon(asked.daemon, question);
    served = started;
    browser = new Browser(program, join(folder, "profile"), question);
    const steps = /** @type {const} */ ([
      ["a call that is spoken, as Opus", () => spoken(/** @type {Browser} */ (browser), "opus at 48 kHz")],
      ["a session that is written", () => written(/** @type {Browser} */ (browser), false)],
      ["a session whose line drops", () => written(/** @type {Browser} */ (browser), true)],
      [
        "a call that is spoken, as plain samples",
        async () => {
          // A browser that cannot code Opus writes plain samples.
          await /** @type {Browser} */ (browser).does("window.AudioEncoder = undefined");
          await spoken(/** @type {Browser} */ (browser), "pcm16 at 48 kHz");
        },
      ],
    ]);
    // `?mic=raw`: the recording is heard as it is.
    await browser.open(`${address}/playground/?mic=raw`);
    await browser.does(KEEPS_WHAT_IS_SAID);
    for (const [name, step] of steps) {
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
