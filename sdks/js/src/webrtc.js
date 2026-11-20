// @ts-check
/**
 * A call over WebRTC to a session of a daemon: the line a client has on
 * one, and the voice of a client that is on one.
 *
 * A daemon that takes calls is posted an offer where `callsOf` says,
 * with the token of the session, and answers it (the shape of WHIP,
 * RFC 9725). The call that comes of it carries the audio both ways,
 * and a data channel that both ends agreed on before the call
 * (`CHANNEL`), on which a client says and is told what it says and is
 * told over a WebSocket. `dial` makes the call and says hello on it:
 * `Client.connect` does so when it is asked for a call
 * (`over: "webrtc"`).
 *
 * A call that cannot be made is `Unreached`: nothing was said on it,
 * and the token it was offered with is as good as it was. That is what
 * `call` (`index.js`) goes over a WebSocket for instead.
 *
 * `CallVoice` is the voice of a client on a call. The browser plays
 * what the call carries, so there is nothing to decode or to count
 * here. What is here is what a browser does not do by itself:
 *
 * - **Saying what was played.** A browser knows nothing of responses.
 *   It can say which part of the call's audio it gave to be played
 *   last (`getSynchronizationSources`), and the voice says that to the
 *   session as a place in the stream, with how long the output takes
 *   to sound it where the browser says so. A browser that cannot say
 *   which part says how much audio has come out of its buffer, and one
 *   that can say neither says nothing: its session works out what was
 *   played from the pace of what it sent.
 * - **Discarding.** Nothing empties the buffer a browser holds of a
 *   call. When the session says that the rest of a response is not to
 *   be heard (`clear`), the voice mutes what the browser plays until
 *   what it held has come out.
 *
 * @module
 */

import { microphone } from "./audio.js";
import * as protocol from "./protocol.js";
import { Refused } from "./protocol.js";

/**
 * @typedef {import("./client.js").Client} Client
 * @typedef {import("./client.js").Options} Options
 * @typedef {import("./protocol.js").FromClient} FromClient
 * @typedef {import("./protocol.js").FromServer} FromServer
 * @typedef {import("./protocol.js").Welcome} Welcome
 * @typedef {import("./protocol.js").AudioFormat} AudioFormat
 */

/** The data channel both ends of a call agree on before it: its number. */
export const CHANNEL = 0;

/** What the channel is called. */
export const CHANNEL_LABEL = "waveport";

/** How the audio of a call is written: Opus, on a clock of 48 kHz. */
export const FORMAT = Object.freeze({ codec: /** @type {const} */ ("opus"), rate: 48_000 });

/** How many samples a packet of a call's audio holds: 20 ms. */
const PACKET = 960;

/** How long a call may take to be there, from its offer on: 5 s. */
const THERE_WITHIN = 5_000;

/** A call that could not be made. Nothing was said on it. */
export class Unreached extends Error {
  /**
   * @param {string} message
   * @param {{ status?: number, code?: string }} [why]
   */
  constructor(message, why = {}) {
    super(message);
    this.name = "Unreached";
    /** What the daemon answered the offer with, if it answered. */
    this.status = why.status;
    /** Why the daemon turned the offer away, as the protocol names it. */
    this.code = why.code;
  }

  /**
   * Whether asking again would be answered the same: the daemon turned
   * the offer away for what it is or for who made it, and not because
   * it is busy or could not be reached.
   */
  get final() {
    const status = this.status ?? 0;
    return status >= 400 && status < 500 && ![408, 425, 429].includes(status);
  }
}

/**
 * Where a server takes offers. A server is named by the address its
 * HTTP API is at, or by that of its line for clients.
 *
 * @param {string | URL} address
 * @returns {string}
 */
export function callsOf(address) {
  const url = new URL(address);
  if (url.protocol === "ws:") {
    url.protocol = "http:";
  } else if (url.protocol === "wss:") {
    url.protocol = "https:";
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new RangeError("offers are taken at an address that begins http:, https:, ws: or wss:");
  }
  // Who calls is said with the offer, and never in the address.
  if (url.search !== "" || url.username !== "" || url.password !== "") {
    throw new RangeError("the address offers are taken at says nothing of who calls");
  }
  url.hash = "";
  if (url.pathname === "/" || url.pathname === "" || url.pathname === "/v1/client") {
    url.pathname = "/v1/webrtc";
  }
  return url.href;
}

/**
 * The servers a far end is told of with the answer to its offer: what
 * the `Link` headers that say `rel="ice-server"` name (RFC 9725,
 * section 4.6), as a browser takes them.
 *
 * @param {string | null | undefined} link Every `Link` of the answer,
 *   as `Headers.get` gives them: one after the other, with commas
 *   between.
 * @returns {RTCIceServer[]}
 */
export function iceServersOf(link) {
  /** @type {RTCIceServer[]} */
  const servers = [];
  for (const { target, params } of links(link ?? "")) {
    const rel = (params.get("rel") ?? "").toLowerCase().split(/\s+/);
    const scheme = /^(stun|stuns|turn|turns):./i.exec(target)?.[1]?.toLowerCase();
    if (!rel.includes("ice-server") || scheme === undefined) {
      continue;
    }
    const username = params.get("username");
    const credential = params.get("credential");
    if (scheme.startsWith("turn")) {
      // A browser asks a relay as somebody, or refuses to be told of
      // it at all.
      if (username === undefined || credential === undefined) {
        continue;
      }
      servers.push({ urls: [target], username, credential });
    } else {
      servers.push({ urls: [target] });
    }
  }
  return servers;
}

/**
 * The links of a `Link` header (RFC 8288): what each points at, and
 * what is said of it. What is no link is passed over.
 *
 * @param {string} header
 * @returns {{ target: string, params: Map<string, string> }[]}
 */
function links(header) {
  /** @type {{ target: string, params: Map<string, string> }[]} */
  const found = [];
  let at = 0;
  const space = () => {
    while (header[at] === " " || header[at] === "\t") {
      at += 1;
    }
  };
  while (at < header.length) {
    space();
    if (header[at] !== "<") {
      // Not a link: on to what follows the next comma.
      const next = header.indexOf(",", at);
      at = next < 0 ? header.length : next + 1;
      continue;
    }
    const closes = header.indexOf(">", at);
    if (closes < 0) {
      break;
    }
    const target = header.slice(at + 1, closes);
    at = closes + 1;
    /** @type {Map<string, string>} */
    const params = new Map();
    for (;;) {
      space();
      if (header[at] !== ";") {
        break;
      }
      at += 1;
      space();
      const begins = at;
      while (at < header.length && !"=;,".includes(/** @type {string} */ (header[at]))) {
        at += 1;
      }
      const name = header.slice(begins, at).trim().toLowerCase();
      let value = "";
      if (header[at] === "=") {
        at += 1;
        space();
        if (header[at] === '"') {
          at += 1;
          while (at < header.length && header[at] !== '"') {
            // What follows a backslash is itself, a quote too.
            if (header[at] === "\\" && at + 1 < header.length) {
              at += 1;
            }
            value += header[at];
            at += 1;
          }
          at += 1;
        } else {
          const from = at;
          while (at < header.length && !";,".includes(/** @type {string} */ (header[at]))) {
            at += 1;
          }
          value = header.slice(from, at).trim();
        }
      }
      // Of what is said twice the first holds.
      if (name !== "" && !params.has(name)) {
        params.set(name, value);
      }
    }
    found.push({ target, params });
    space();
    if (header[at] === ",") {
      at += 1;
    } else if (at < header.length) {
      // What follows a link and is no comma is not read further.
      const next = header.indexOf(",", at);
      at = next < 0 ? header.length : next + 1;
    }
  }
  return found;
}

/**
 * Whether `a` is at or after `b` in the time of a stream, which comes
 * round to nought after 2^32.
 *
 * @param {number} a
 * @param {number} b
 */
const reached = (a, b) => ((a - b) >>> 0) < 0x8000_0000;

/**
 * A call that is there: what a client says and is told on, as a
 * connection is, and what carries its audio.
 */
export class Call {
  /** @type {RTCPeerConnection} */
  #peer;

  /** @type {RTCDataChannel} */
  #channel;

  /** @type {RTCRtpTransceiver} */
  #audio;

  /** Tells the daemon, where it holds the call, that its far end is gone. @type {(() => void) | undefined} */
  #ends;

  #over = false;

  /** Told what the server says. @type {((event: { data: unknown }) => void) | null} */
  onmessage = null;

  /** Told that the call is gone, and nobody said goodbye. @type {(() => void) | null} */
  onclose = null;

  /** @type {(() => void) | null} */
  onerror = null;

  /**
   * @param {RTCPeerConnection} peer
   * @param {RTCDataChannel} channel
   * @param {RTCRtpTransceiver} audio
   * @param {(() => void) | undefined} ends What tells the daemon, where
   *   it holds the call, that its far end is gone.
   */
  constructor(peer, channel, audio, ends) {
    this.#peer = peer;
    this.#channel = channel;
    this.#audio = audio;
    this.#ends = ends;
    channel.onmessage = (event) => this.onmessage?.({ data: event.data });
    channel.onclose = () => this.#gone();
    channel.onerror = null;
    peer.onconnectionstatechange = () => {
      // A call that is not there any more is not waited for: the
      // client comes back on another.
      if (["disconnected", "failed", "closed"].includes(peer.connectionState)) {
        this.#gone();
      }
    };
  }

  /** What receives the audio of the call. */
  get receiver() {
    return this.#audio.receiver;
  }

  /** What sends the audio of the call. */
  get sender() {
    return this.#audio.sender;
  }

  /** The call itself, for whoever wants to know how it fares. */
  get peer() {
    return this.#peer;
  }

  #gone() {
    if (!this.#over) {
      this.#over = true;
      this.onclose?.();
    }
  }

  /**
   * Says something on the call. Audio is not said here: the call
   * carries what its microphone hears by itself.
   *
   * @param {unknown} data
   */
  send(data) {
    if (typeof data === "string" && this.#channel.readyState === "open") {
      this.#channel.send(data);
    }
  }

  /**
   * Tells the daemon, where the call was offered, that its far end is
   * gone: the session is over. A goodbye that was said on the call can
   * be lost with the call, and this cannot.
   */
  hangUp() {
    const ends = this.#ends;
    this.#ends = undefined;
    ends?.();
  }

  /** Lets go of the call, without a word. */
  close() {
    this.#over = true;
    this.#channel.onmessage = null;
    this.#channel.onclose = null;
    this.#peer.onconnectionstatechange = null;
    try {
      this.#channel.close();
      this.#peer.close();
    } catch {
      // One that is closed already is closed.
    }
  }
}

/**
 * What a daemon said of an offer it did not answer, as a reason.
 *
 * @param {Response} answered
 * @returns {Promise<Unreached>}
 */
async function turnedAway(answered) {
  /** @type {{ detail?: unknown, code?: unknown }} */
  let said = {};
  try {
    said = await answered.json();
  } catch {
    // It said nothing that can be read.
  }
  const detail = typeof said.detail === "string" ? said.detail : `status ${answered.status}`;
  const code = typeof said.code === "string" ? said.code : undefined;
  return new Unreached(`the server took no call: ${detail}`, { status: answered.status, code });
}

/**
 * Where a daemon says a call is, if that is at the daemon: its token
 * is said to nobody else, wherever an answer points.
 *
 * @param {string} offered Where the offer was posted.
 * @param {string | null} location
 * @returns {string | undefined}
 */
export function heldAt(offered, location) {
  if (location === null || location === "") {
    return undefined;
  }
  try {
    const at = new URL(location, offered);
    return at.origin === new URL(offered).origin ? at.href : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Offers a call where `at` says, makes it, says hello on it, and waits
 * to be welcomed.
 *
 * @param {string} at Where the server takes offers (`callsOf`).
 * @param {FromClient} first The hello.
 * @param {string} bearer The token the offer is made with: the one the
 *   hello says.
 * @param {Options} options
 * @returns {Promise<{ socket: Call, welcome: Welcome }>}
 * @throws {Unreached} No call came about, and nothing was said on one.
 * @throws {Refused} The server turned the hello away.
 * @throws {Error} The server did not answer the hello.
 */
export async function dial(at, first, bearer, options) {
  const Peer = options.RTCPeerConnection ?? globalThis.RTCPeerConnection;
  const ask = options.fetch ?? globalThis.fetch;
  if (typeof Peer !== "function" || typeof ask !== "function") {
    throw new Unreached("this browser makes no calls over WebRTC");
  }
  const peer = new Peer({ bundlePolicy: "max-bundle" });
  const stops = new AbortController();
  /** Where the daemon holds the call. @type {string | undefined} */
  let held;
  /** Tells the daemon there that nobody is on the call any more. */
  const letGo = (/** @type {string} */ there) => {
    const headers = { authorization: `Bearer ${bearer}` };
    // It is to arrive though the page may be going.
    void ask(there, { method: "DELETE", headers, keepalive: true }).catch(() => {});
  };
  /** Whether the hello was said: from then on the call is one that was made. */
  let said = false;
  /** @type {ReturnType<typeof setTimeout> | undefined} */
  let timer;
  try {
    const channel = peer.createDataChannel(CHANNEL_LABEL, { negotiated: true, id: CHANNEL });
    const audio = peer.addTransceiver(options.microphone ?? "audio", { direction: "sendrecv" });
    /** What the call comes to: a welcome, or why there is none. @type {Promise<Welcome>} */
    const welcomed = new Promise((resolve, reject) => {
      /** What the server refused the hello with, before its goodbye. @type {Refused | undefined} */
      let refused;
      const unreached = () => reject(new Unreached("the call did not get through"));
      const ended = () => {
        if (said) {
          reject(refused ?? new Error("the call ended before the server answered the hello"));
        } else {
          unreached();
        }
      };
      timer = setTimeout(() => {
        stops.abort();
        unreached();
      }, options.callWithinMs ?? THERE_WITHIN);
      peer.onconnectionstatechange = () => {
        if (peer.connectionState === "failed" || peer.connectionState === "closed") {
          ended();
        }
      };
      channel.onclose = ended;
      channel.onopen = () => {
        // From here on the token is the session's to judge.
        said = true;
        clearTimeout(timer);
        channel.send(protocol.write(first));
        timer = setTimeout(
          () => reject(new Error("the server did not answer the hello")),
          options.welcomeWithinMs ?? 10_000,
        );
      };
      channel.onmessage = (event) => {
        if (typeof event.data !== "string") {
          return;
        }
        /** @type {FromServer} */
        let message;
        try {
          message = protocol.read(event.data);
        } catch {
          reject(new Error("the server answered the hello with what is no message"));
          return;
        }
        if (message.type === "welcome") {
          resolve(message);
        } else if (message.type === "error") {
          refused = new Refused(message.code, message.message);
        } else if (message.type === "bye") {
          reject(refused ?? new Refused("unknown", "the server said goodbye to the hello"));
        }
      };
    });
    const offered = (async () => {
      const offer = await peer.createOffer();
      /** @type {Response} */
      let answered;
      try {
        answered = await ask(at, {
          method: "POST",
          headers: { "content-type": "application/sdp", authorization: `Bearer ${bearer}` },
          body: offer.sdp ?? "",
          signal: stops.signal,
        });
      } catch {
        throw new Unreached("the server cannot be reached with an offer");
      }
      if (answered.status !== 201) {
        throw await turnedAway(answered);
      }
      held = heldAt(at, answered.headers.get("location"));
      const answer = await answered.text();
      // The servers that help a call through are told with the answer.
      // A browser looks for ways once it has taken its own offer, so it
      // is told of them before that.
      const servers = iceServersOf(answered.headers.get("link"));
      if (servers.length > 0) {
        try {
          peer.setConfiguration({ ...peer.getConfiguration(), iceServers: servers });
        } catch {
          // A browser that cannot be told of them calls without.
        }
      }
      await peer.setLocalDescription(offer);
      await peer.setRemoteDescription({ type: "answer", sdp: answer });
    })();
    // Whichever is over first: the time a call has runs while it is
    // offered. Neither is lost when the other fails.
    offered.catch(() => {});
    welcomed.catch(() => {});
    await Promise.race([offered, welcomed]);
    const welcome = await welcomed;
    const there = held;
    const ends = there === undefined ? undefined : () => letGo(there);
    return { socket: new Call(peer, channel, audio, ends), welcome };
  } catch (error) {
    try {
      peer.close();
    } catch {
      // One that is closed already is closed.
    }
    // What the daemon set up for a call that nothing was said on is
    // let go of. A call on which the hello was said is the session's:
    // it is not ended for a welcome that did not come.
    if (!said && held !== undefined) {
      letGo(held);
    }
    if (error instanceof Unreached || error instanceof Refused || said) {
      throw error;
    }
    // Whatever kept the browser from making the call, none was made.
    const why = error instanceof Error ? error.message : "it could not be made";
    throw new Unreached(`the call was not made: ${why}`);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * How long nothing real has to have come out of a browser's buffer
 * before what it held is taken to be gone: what it makes up when its
 * audio runs out can be heard for about an eighth of a second and has
 * faded to nothing after a quarter.
 */
const QUIET = 250;

/**
 * How long a browser that cannot say what it plays is given to play
 * what it held when it was told to discard it, before the quiet.
 */
const HELD = 150;

/**
 * How far before the end of a response a browser may stop saying where
 * it is, though it played the response to its end: 100 ms. One browser
 * stops 40 to 60 ms short of what it was sent last.
 */
const SHORT = 4_800;

/** How long a browser has to have stopped there before the end is taken as played. */
const STOPPED = 120;

/**
 * What a browser says of where it is in a call's audio.
 *
 * @typedef {object} Source
 * @property {number} rtpTimestamp The time in the stream of the packet
 *   it last gave to be played.
 * @property {number} timestamp When it gave it, on the clock of
 *   `performance.now()`.
 */

/**
 * Where a call is in what it plays, from what its browser says: what
 * is to be said to the session, and whether what the browser plays is
 * to be heard.
 *
 * Nothing here asks a browser anything or keeps time: whoever uses it
 * says what the time is and what the browser said.
 */
export class Playing {
  /** The place that was said last. @type {number | undefined} */
  #said;

  /** The time in the stream the browser said last, and when that changed. @type {number | undefined} */
  #source;

  #moved = 0;

  /** Where the response that was sent whole last ends. @type {number | undefined} */
  #ends;

  /**
   * What is being discarded: since when, and where the response that
   * follows it begins, once that is known.
   *
   * @type {{ since: number, run: number, next?: number } | undefined}
   */
  #discarding;

  /** Whether what the browser plays is kept from being heard. */
  get silenced() {
    return this.#discarding !== undefined;
  }

  /**
   * A response begins at `ts` in the stream, or was sent whole and
   * ends there.
   *
   * @param {{ run: number, end?: boolean, ts?: number }} mark
   */
  marked({ run, end, ts }) {
    if (ts === undefined) {
      return;
    }
    if (end === true) {
      this.#ends = ts;
    } else {
      this.#ends = undefined;
      const discarding = this.#discarding;
      // The first response after the one that is discarded is where
      // what is to be heard begins again.
      if (discarding !== undefined && discarding.next === undefined && run > discarding.run) {
        discarding.next = ts;
      }
    }
  }

  /**
   * What the browser holds of the response `run`, and of every one
   * before it, is not to be heard.
   *
   * @param {number} run
   * @param {number} now
   */
  cleared(run, now) {
    this.#ends = undefined;
    this.#discarding = { since: now, run };
  }

  /**
   * Looks at where the browser is.
   *
   * @param {number} now The time, on the clock of `performance.now()`.
   * @param {Source | undefined} source What the browser says, if it
   *   says anything.
   * @returns {number | undefined} The place in the stream that has
   *   been played and was not said yet.
   */
  at(now, source) {
    /** @type {number | undefined} */
    let place;
    if (source !== undefined) {
      if (source.rtpTimestamp !== this.#source) {
        this.#source = source.rtpTimestamp;
        this.#moved = now;
      }
      // A packet is given to be played at its beginning, and plays for
      // as long as it is.
      const since = now - source.timestamp;
      const into = since > 0 && since < 1_000 ? Math.min(PACKET, Math.round(since * 48)) : 0;
      place = (source.rtpTimestamp + into) >>> 0;
    }
    const discarding = this.#discarding;
    if (discarding !== undefined) {
      const { next, since } = discarding;
      if (next !== undefined && place !== undefined && reached(place, next)) {
        // The browser is at a response that is to be heard.
        this.#discarding = undefined;
      } else {
        const quiet = source === undefined ? now - since >= HELD + QUIET : now - Math.max(since, this.#moved) >= QUIET;
        if (quiet) {
          // Nothing of what it held is left.
          this.#discarding = undefined;
        }
        // What was not heard is not said to have been played, and is
        // not said later either.
        if (place !== undefined && (this.#said === undefined || reached(place, this.#said))) {
          this.#said = place;
        }
        return undefined;
      }
    }
    if (place === undefined) {
      return undefined;
    }
    const ends = this.#ends;
    if (ends !== undefined && !reached(place, ends) && ((ends - place) >>> 0) <= SHORT && now - this.#moved >= STOPPED) {
      // It stopped just short of the end of what it was sent, and
      // nothing more is coming: it played that to its end.
      place = ends;
    }
    if (this.#said !== undefined && reached(this.#said, place)) {
      return undefined;
    }
    this.#said = place;
    return place;
  }
}

/**
 * What a browser's statistics say of a call's audio.
 *
 * @param {Iterable<any>} stats
 * @returns {{ emitted?: number, delayS?: number, samples?: number }}
 */
export function playedBy(stats) {
  /** @type {{ emitted?: number, delayS?: number, samples?: number }} */
  const said = {};
  for (const one of stats) {
    if (one?.type === "inbound-rtp" && (one.kind ?? "audio") === "audio" && Number.isFinite(one.jitterBufferEmittedCount)) {
      said.emitted = one.jitterBufferEmittedCount;
    } else if (one?.type === "media-playout" && Number.isFinite(one.totalPlayoutDelay) && Number.isFinite(one.totalSamplesCount)) {
      said.delayS = one.totalPlayoutDelay;
      said.samples = one.totalSamplesCount;
    }
  }
  return said;
}

/**
 * What plays a call, as far as its voice asks: an element of a page
 * does, and so does anything else that takes a stream to play and can
 * be told to keep it from being heard.
 *
 * @typedef {Pick<HTMLMediaElement, "autoplay" | "muted" | "srcObject" | "play">} Plays
 */

/**
 * @typedef {object} CallVoiceOptions
 * @property {MediaStream | null} [microphone] What is heard, in place
 *   of the microphone the browser would be asked for. `null` is none:
 *   the browser is asked for nothing, and the call sends nothing until
 *   its voice is given something to hear (`hears`).
 * @property {boolean} [owned] Whether a microphone that was given is
 *   the voice's to let go of when it stops.
 * @property {MediaTrackConstraints} [constraints] What is asked of the
 *   microphone, as for `Voice`.
 * @property {Plays} [element] What plays the call, in place of an
 *   element the voice makes for itself: an element of the page's, or
 *   what stands in for one where the page plays the call itself.
 * @property {(error: Error) => void} [onerror] Told what went wrong
 *   with the audio once it was started.
 */

/** How often the browser is asked where it is: every 20 ms. */
const LOOKS_EVERY = 20;

/** The voice of a client that is on a call. */
export class CallVoice {
  /** @type {Client | undefined} */
  #client;

  /** @type {Call | undefined} */
  #call;

  /** @type {MediaStream | undefined} */
  #microphone;

  #owns;

  /** @type {Plays} */
  #element;

  #playing = new Playing();

  #muted = false;

  #stopped = false;

  /** How long the output takes to sound what it is given, in milliseconds. */
  #delay = 0;

  /** What the statistics last said of the output. @type {{ delayS: number, samples: number } | undefined} */
  #output;

  /** How much has come out of the buffer, as it was said last. @type {number | undefined} */
  #emitted;

  #looks = 0;

  /** @type {ReturnType<typeof setInterval> | undefined} */
  #timer;

  /** @type {(() => void)[]} */
  #listening = [];

  /** @type {(error: Error) => void} */
  #failed;

  /**
   * @param {MediaStream | undefined} microphone
   * @param {boolean} owns
   * @param {CallVoiceOptions} options
   */
  constructor(microphone, owns, options) {
    this.#microphone = microphone;
    this.#owns = owns && microphone !== undefined;
    this.#failed = options.onerror ?? (() => {});
    this.#element = options.element ?? new Audio();
    this.#element.autoplay = true;
  }

  /**
   * Makes a voice for a call: asks for the microphone, unless it is
   * given one or told that there is none, and is ready to play. It is
   * on no call until it is attached to a client, which is given `track`
   * to call with.
   *
   * @param {CallVoiceOptions} [options]
   * @returns {Promise<CallVoice>}
   * @throws {Error} The page's user did not allow the microphone.
   */
  static async open(options = {}) {
    const given = options.microphone;
    const heard = given === null ? undefined : (given ?? (await microphone(options.constraints)));
    return new CallVoice(heard, given === undefined || options.owned === true, options);
  }

  /**
   * What the microphone hears: what a client calls with (`microphone`).
   * Nothing for a voice that has no microphone.
   *
   * @returns {MediaStreamTrack | undefined}
   */
  get track() {
    return this.#microphone?.getAudioTracks()[0];
  }

  /** The microphone, if the voice has one. */
  get microphone() {
    return this.#microphone;
  }

  /**
   * Hears `microphone` from here on, in place of the one it heard: a
   * page's user chose another, or none (`null`). The call sends it at
   * once, muted if the voice is, and the calls its client comes back on
   * are made with it. The one before is let go of if it was the
   * voice's.
   *
   * @param {MediaStream | null} microphone
   * @param {boolean} [owned] Whether it is the voice's to let go of
   *   when it stops or is given another. It is, if left out.
   * @returns {Promise<void>}
   * @throws {Error} The voice was stopped, or the call does not take
   *   the track: the voice hears what it heard.
   */
  async hears(microphone, owned = true) {
    if (this.#stopped) {
      throw new Error("the voice was stopped");
    }
    const track = microphone?.getAudioTracks()[0];
    if (track !== undefined) {
      // What is muted stays muted, whatever is heard.
      track.enabled = !this.#muted;
    }
    // The call takes it first: one it does not take changes nothing.
    await this.#client?.hears(track);
    const before = this.#owns ? this.#microphone : undefined;
    this.#microphone = microphone ?? undefined;
    this.#owns = owned && microphone !== null;
    if (before !== undefined && before !== microphone) {
      for (const one of before.getTracks()) {
        one.stop();
      }
    }
  }

  /**
   * Puts the voice on the call of `client`: what the call carries is
   * played, and what was played is said to the session. Called as soon
   * as the client is welcomed.
   *
   * @param {Client} client
   * @throws {Error} The voice is on a call already or was stopped, or
   *   the client is on no call.
   */
  attach(client) {
    if (this.#client !== undefined || this.#stopped) {
      throw new Error("the voice is on a call already, or was stopped");
    }
    if (client.call === undefined) {
      throw new Error("the client is on no call");
    }
    this.#client = client;
    this.#listening = [
      client.on("mark", (mark) => this.#playing.marked(mark)),
      client.on("clear", (clear) => {
        this.#playing.cleared(clear.run, performance.now());
        this.#element.muted = true;
      }),
      // A session cuts what it was saying where it was heard when its
      // call drops: what the browser still holds of it is not played.
      client.on("away", () => this.#lets()),
      client.on("back", () => this.#takes(client.call)),
      client.on("closed", () => this.stop()),
    ];
    this.#takes(client.call);
    this.#timer = setInterval(() => this.#look(), LOOKS_EVERY);
  }

  /** How the audio of a call is written. */
  get format() {
    return FORMAT;
  }

  /**
   * What the microphone was given of what it was asked for.
   *
   * @returns {MediaTrackSettings}
   */
  get settings() {
    return this.track?.getSettings() ?? {};
  }

  /** Whether silence is sent in place of what the microphone hears. */
  get muted() {
    return this.#muted;
  }

  set muted(muted) {
    this.#muted = muted;
    const track = this.track;
    if (track !== undefined) {
      track.enabled = !muted;
    }
  }

  /**
   * Whether what the browser plays is kept from being heard: it was
   * told to discard a response, and still holds some of it.
   */
  get silenced() {
    return this.#playing.silenced;
  }

  /**
   * Stops playing and hearing, and lets go of the microphone if it is
   * the voice's. The client goes on.
   */
  stop() {
    if (this.#stopped) {
      return;
    }
    this.#stopped = true;
    for (const stop of this.#listening.splice(0)) {
      stop();
    }
    clearInterval(this.#timer);
    this.#lets();
    if (this.#owns) {
      for (const track of this.#microphone?.getTracks() ?? []) {
        track.stop();
      }
    }
  }

  /**
   * Gives the microphone to whoever asks, and stops: for a voice of
   * another kind that takes this one's place. Nothing where the voice
   * has none.
   *
   * @returns {MediaStream | undefined}
   */
  release() {
    this.#owns = false;
    this.stop();
    return this.#microphone;
  }

  /**
   * Plays the call the client is on.
   *
   * @param {Call | undefined} call
   */
  #takes(call) {
    if (call === undefined || this.#stopped) {
      return;
    }
    this.#call = call;
    this.#playing = new Playing();
    this.#output = undefined;
    this.#emitted = undefined;
    this.#element.muted = false;
    this.#element.srcObject = new MediaStream([call.receiver.track]);
    // A browser that plays nothing for a page its user has not touched
    // says so here.
    void Promise.resolve(this.#element.play()).catch((error) => this.#failed(error));
  }

  /** Lets go of the call that was played. */
  #lets() {
    this.#call = undefined;
    this.#element.srcObject = null;
  }

  /** Looks at where the browser is, and says what is new of it. */
  #look() {
    const call = this.#call;
    const client = this.#client;
    if (call === undefined || client === undefined) {
      return;
    }
    const receiver = call.receiver;
    const knows = typeof receiver.getSynchronizationSources === "function";
    /** @type {Source | undefined} */
    let source;
    if (knows) {
      for (const one of receiver.getSynchronizationSources()) {
        if (source === undefined || one.timestamp > source.timestamp) {
          source = one;
        }
      }
    }
    const place = this.#playing.at(performance.now(), source);
    this.#element.muted = this.#playing.silenced;
    if (place !== undefined) {
      client.playedRtp({ ts: place, delay_ms: this.#delay });
    }
    // The statistics are asked for less often: every second for how
    // long the output takes, and ten times a second where they are all
    // a browser has to say what it played.
    this.#looks += 1;
    if (this.#looks % (knows ? 50 : 5) === 1) {
      void this.#reads(call, client, !knows);
    }
  }

  /**
   * @param {Call} call
   * @param {Client} client
   * @param {boolean} says Whether what the statistics say came out is
   *   said to the session.
   */
  async #reads(call, client, says) {
    /** @type {ReturnType<typeof playedBy>} */
    let said;
    try {
      said = playedBy((await call.receiver.getStats()).values());
    } catch {
      return;
    }
    if (call !== this.#call) {
      return;
    }
    const { delayS, samples, emitted } = said;
    if (delayS !== undefined && samples !== undefined) {
      // How long the output took over what it was given since it was
      // last asked, a sample: at first, since the call began.
      const before = this.#output ?? { delayS: 0, samples: 0 };
      if (samples > before.samples) {
        const each = (delayS - before.delayS) / (samples - before.samples);
        this.#delay = Math.max(0, Math.min(1_000, Math.round(each * 1_000)));
      }
      this.#output = { delayS, samples };
    }
    if (says && emitted !== undefined && emitted !== this.#emitted && !this.#playing.silenced) {
      this.#emitted = emitted;
      client.playedRtp({ emitted, delay_ms: this.#delay });
    }
  }
}
