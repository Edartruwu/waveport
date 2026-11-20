// @ts-check
// What drives a browser for a test: the daemon with stand-ins for its
// providers (`apps/waveportd/examples/stand_in.rs`), and a Chrome that
// is driven over the pipe it offers for that, with nothing installed
// to drive it. The Chrome is given a recording in place of a
// microphone and plays what it is sent to nothing: no sound is made,
// and no microphone is opened.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";

/** How long one step of a test may take. */
export const STEP_WITHIN = 45_000;

/** The first Node whose npm keeps a version out until it is a week old. */
export const NODE = 24;

/**
 * Where a Chrome is, if one is.
 *
 * @param {string} [named] Where whoever runs the test says one is.
 */
export function chrome(named) {
  const places = [
    named,
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
 * What the daemon is told to have it answer calls: at `address`, on
 * ports that are free.
 *
 * @param {string} address
 */
export const calledAt = (address) => `[webrtc]\nudp = "${address}:0"\ntcp = "${address}:0"\n`;

/**
 * Starts the daemon and waits until it says where it listens.
 *
 * @param {string} program
 * @param {string} question Where it writes what its recogniser hears.
 * @param {string} more More of its configuration: the lines of TOML that come before its agents.
 * @returns {Promise<Started & { address: string }>}
 */
export function daemon(program, question, more) {
  // It is given nothing of the environment it does not need: no key of
  // anyone's is its to see.
  const env = Object.fromEntries(
    ["PATH", "HOME", "TMPDIR", "SystemRoot", "WAVEPORT_ASSETS"].flatMap((name) => {
      const value = process.env[name];
      return value === undefined ? [] : [[name, value]];
    }),
  );
  const child = spawn(program, ["--question", question, "--sessions", "10", "--more", more], {
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

/**
 * A browser with one page, as the steps drive it. How it is driven is
 * the browser's own: `Chrome` here, and whatever else extends this.
 */
export class Browser {
  /** What went wrong on the page, and what it wrote. @type {string[]} */
  noted = [];

  /**
   * What the page works out to say where it is, for whoever reads that
   * something did not come about on it.
   */
  shows = "document.title";

  /**
   * Opens a page, which runs `first` before anything of whatever it
   * goes to.
   *
   * @param {string} first
   * @returns {Promise<void>}
   */
  async open(first) {
    throw new Error(`no browser runs ${first.length} characters`);
  }

  /**
   * Has the page go to `address`, and waits until it has loaded.
   *
   * @param {string} address
   * @returns {Promise<void>}
   */
  async goes(address) {
    throw new Error(`no browser goes to ${address}`);
  }

  /**
   * Has the page work `expression` out, as if its user had asked, and
   * returns what comes of it.
   *
   * @param {string} expression
   * @returns {Promise<any>}
   */
  async does(expression) {
    throw new Error(`no browser works out ${expression.length} characters`);
  }

  /**
   * Closes the browser, and returns once it is gone.
   *
   * @returns {Promise<void>}
   */
  async close() {}

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
      const page = await this.does(`JSON.stringify(${this.shows}, null, 1)`);
      throw new Error(`${what} did not come about.\n${page}\n${this.noted.join("\n")}`);
    }
  }
}

/** A Chrome, which is driven over the pipe it offers for that. */
export class Chrome extends Browser {
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

  /**
   * @param {string} program
   * @param {string} profile A folder for it that is nobody's.
   * @param {string} microphone The recording it hears in place of one.
   * @param {string[]} [more] What else it is started with.
   */
  constructor(program, profile, microphone, more = []) {
    super();
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
      ...more,
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

  /** Told that the page has loaded. @type {(() => void) | undefined} */
  #loaded;

  /**
   * @override
   * @param {string} first
   */
  async open(first) {
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
    this.on("Page.loadEventFired", () => this.#loaded?.());
    await Promise.all([this.ask("Runtime.enable"), this.ask("Log.enable"), this.ask("Page.enable")]);
    await this.ask("Page.addScriptToEvaluateOnNewDocument", { source: first });
  }

  /**
   * @override
   * @param {string} address
   */
  async goes(address) {
    const loaded = new Promise((resolve) => {
      this.#loaded = () => resolve(undefined);
    });
    await this.ask("Page.navigate", { url: address });
    await loaded;
  }

  /**
   * @override
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

  /** @override */
  async close() {
    const gone = new Promise((resolve) => this.#child.once("exit", resolve));
    this.ask("Browser.close").catch(() => {});
    const late = setTimeout(() => this.#child.kill("SIGKILL"), 5_000);
    await gone;
    clearTimeout(late);
  }
}
