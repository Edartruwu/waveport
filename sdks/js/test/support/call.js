// @ts-check
// What stands in for a browser's calls in the tests of the client: a
// call whose far end is in the test's hands, and the daemon that is
// posted its offer.

import assert from "node:assert/strict";

/** The data channel of a call, with the server's end in the test's hands. */
export class Channel {
  readyState = "connecting";
  /** What the client sent. @type {string[]} */
  sent = [];
  /** @type {((event: unknown) => void) | null} */
  onopen = null;
  /** @type {((event: { data: unknown }) => void) | null} */
  onmessage = null;
  /** @type {(() => void) | null} */
  onclose = null;
  /** @type {(() => void) | null} */
  onerror = null;

  /**
   * @param {string} label
   * @param {object} options
   */
  constructor(label, options) {
    this.label = label;
    this.options = options;
  }

  /** @param {string} data */
  send(data) {
    assert.equal(this.readyState, "open", "something was said on a channel that is not open");
    this.sent.push(data);
  }

  close() {
    this.readyState = "closed";
  }

  /** What the client said, as the objects its messages hold. */
  get said() {
    return this.sent.map((data) => JSON.parse(data));
  }

  /** The call is there, and its channel with it. */
  opens() {
    this.readyState = "open";
    this.onopen?.({});
  }

  /** @param {object} message The server says this. */
  says(message) {
    this.onmessage?.({ data: JSON.stringify(message) });
  }

  /** The call ends, and nobody said goodbye. */
  drops() {
    this.readyState = "closed";
    this.onclose?.();
  }
}

/** What receives the audio of a call, which says what the test has it say. */
export class Receiver {
  track = { kind: "audio", id: "what the agent says" };

  /** Where the browser says it is. @type {{ rtpTimestamp: number, timestamp: number }[]} */
  sources = [];

  /** What its statistics say. @type {object[]} */
  stats = [];

  getSynchronizationSources() {
    return this.sources;
  }

  async getStats() {
    return new Map(this.stats.map((one, n) => [`s${n}`, one]));
  }
}

/** A call as a browser makes it, with its far end in the test's hands. */
export class Peer {
  /** Every call that was made, in order. @type {Peer[]} */
  static made = [];

  /** What a browser that cannot be told of servers does when it is. @type {Error | undefined} */
  static refusesServers = undefined;

  connectionState = "new";
  closed = false;
  /** What was done to the call, in the order it was done. @type {string[]} */
  did = [];
  /** @type {(() => void) | null} */
  onconnectionstatechange = null;
  /** @type {Channel | undefined} */
  channel = undefined;
  receiver = new Receiver();
  sender = {};
  /** What the call sends: a track, or the kind of one. @type {unknown} */
  sends = undefined;
  /** @type {{ type: string, sdp: string } | undefined} */
  remote = undefined;

  /** @param {RTCConfiguration} configuration */
  constructor(configuration) {
    this.configuration = configuration;
    Peer.made.push(this);
  }

  /**
   * @param {string} label
   * @param {object} options
   */
  createDataChannel(label, options) {
    this.channel = new Channel(label, options);
    return this.channel;
  }

  /**
   * @param {unknown} track
   * @param {object} options
   */
  addTransceiver(track, options) {
    this.sends = track;
    this.direction = options;
    return { receiver: this.receiver, sender: this.sender };
  }

  async createOffer() {
    this.did.push("offer");
    return { type: "offer", sdp: "v=0\r\ns=an offer\r\n" };
  }

  getConfiguration() {
    return this.configuration;
  }

  /** @param {RTCConfiguration} configuration */
  setConfiguration(configuration) {
    if (Peer.refusesServers !== undefined) {
      throw Peer.refusesServers;
    }
    this.did.push("servers");
    this.configuration = configuration;
  }

  /** @param {{ type: string, sdp: string }} description */
  async setLocalDescription(description) {
    this.did.push(`local ${description.type}`);
  }

  /** @param {{ type: string, sdp: string }} description */
  async setRemoteDescription(description) {
    if (!description.sdp.startsWith("v=0")) {
      throw new Error("that is no description");
    }
    this.did.push(`remote ${description.type}`);
    this.remote = description;
  }

  close() {
    this.closed = true;
    this.connectionState = "closed";
  }

  /** The call comes to `state`. @param {string} state */
  becomes(state) {
    this.connectionState = state;
    this.onconnectionstatechange?.();
  }
}

export const RTCPeerConnection = /** @type {typeof globalThis.RTCPeerConnection} */ (
  /** @type {unknown} */ (Peer)
);

/** The call that was made last. */
export const lastCall = () => /** @type {Peer} */ (Peer.made.at(-1));

/**
 * A daemon that is posted offers, and answers as the test has it
 * answer.
 */
export class Daemon {
  /** What it was asked, in order. @type {{ url: string, method: string, headers: Record<string, string>, body?: string, keepalive?: boolean }[]} */
  asked = [];

  /** What it answers the next offer with. @type {() => Response | Promise<Response>} */
  answers = () => Daemon.answer();

  /**
   * The answer to an offer, with what a daemon says beside it.
   *
   * @param {Record<string, string>} [headers]
   */
  static answer(headers = {}) {
    return new Response("v=0\r\ns=an answer\r\n", {
      status: 201,
      headers: {
        "content-type": "application/sdp",
        location: "/v1/webrtc/0123456789abcdef0123456789abcdef",
        etag: '"1"',
        ...headers,
      },
    });
  }

  /**
   * An offer that is turned away.
   *
   * @param {number} status
   * @param {string} code
   * @param {string} detail
   */
  static problem(status, code, detail) {
    return new Response(JSON.stringify({ status, title: "", code, detail }), {
      status,
      headers: { "content-type": "application/problem+json" },
    });
  }

  /** What a client asks with. */
  get fetch() {
    return /** @type {typeof globalThis.fetch} */ (
      async (url, init = {}) => {
        const headers = /** @type {Record<string, string>} */ (init.headers ?? {});
        const method = init.method ?? "GET";
        const body = typeof init.body === "string" ? init.body : undefined;
        this.asked.push({ url: String(url), method, headers, body, keepalive: init.keepalive });
        if (method === "DELETE") {
          return new Response(null, { status: 200 });
        }
        const signal = init.signal;
        const answered = this.answers();
        if (signal === undefined || signal === null) {
          return answered;
        }
        // An offer that is let go of is not answered.
        return await Promise.race([
          answered,
          new Promise((_, reject) => {
            signal.addEventListener("abort", () => reject(new Error("the offer was let go of")));
          }),
        ]);
      }
    );
  }
}
