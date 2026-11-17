// @ts-check
// What stands in for a server in the tests of the client: a connection
// whose far end is in the test's hands.

import assert from "node:assert/strict";

/** A connection as the client makes it, with the server's end in the test's hands. */
export class Socket {
  /** Every connection that was made, in order. @type {Socket[]} */
  static made = [];

  binaryType = "blob";
  closed = false;
  /** What the client sent. @type {unknown[]} */
  sent = [];
  /** @type {(() => void) | null} */
  onopen = null;
  /** @type {((event: { data: unknown }) => void) | null} */
  onmessage = null;
  /** @type {(() => void) | null} */
  onclose = null;
  /** @type {(() => void) | null} */
  onerror = null;

  /**
   * @param {string} url
   * @param {string} named
   */
  constructor(url, named) {
    this.url = url;
    this.named = named;
    Socket.made.push(this);
  }

  /** @param {unknown} data */
  send(data) {
    assert.equal(this.closed, false, "something was sent on a connection that is closed");
    this.sent.push(data);
  }

  close() {
    this.closed = true;
  }

  /** What the client said, as the objects its messages hold. */
  get said() {
    return this.sent.filter((data) => typeof data === "string").map((data) => JSON.parse(data));
  }

  /** The server takes the connection. */
  opens() {
    this.onopen?.();
  }

  /** @param {object} message The server says this. */
  says(message) {
    this.onmessage?.({ data: JSON.stringify(message) });
  }

  /** The connection ends, and nobody said goodbye. */
  drops() {
    this.closed = true;
    this.onclose?.();
  }
}

export const WebSocket = /** @type {typeof globalThis.WebSocket} */ (/** @type {unknown} */ (Socket));

/** The welcome of a session in its first life. */
export const WELCOME = {
  type: "welcome",
  v: 1,
  session: "0000000000000000000000000000002a",
  life: 1,
  resumed: false,
  resume_token: "wpr1_madeup",
  features: ["said"],
  next_seq: 1,
  grace_ms: 30_000,
};

/** Lets what waits on a promise go on. */
export const settle = () => new Promise((resolve) => setImmediate(resolve));

/** The connection that was made last. */
export const last = () => /** @type {Socket} */ (Socket.made.at(-1));
