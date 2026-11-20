// @ts-check
/**
 * A client of one session, over a WebSocket or on a call over WebRTC.
 *
 * `Client.connect` opens the connection, says hello and waits to be
 * welcomed. From then on the client holds what the session has come to
 * (`view`), tells whoever listens what the server says, and says what
 * its user does: `text`, `dtmf`, `interrupt`, `bye`.
 *
 * Stopping, dropping and leaving are three things. `interrupt` stops
 * the agent. A connection that drops leaves the session waiting, and
 * the client comes back by itself, with the token it was welcomed
 * with, the life of the session it was told of and the number of the
 * last fact it has: it says `away`, and `back` when it is welcomed
 * again. `bye` ends the session.
 *
 * Nothing here knows of audio beyond carrying it: `audio.js` plays and
 * records. On a call (`over: "webrtc"`) not even that: the call carries
 * its audio by itself, and the client says and is told the rest on the
 * call's channel (`webrtc.js`). A client that comes back does so on
 * the kind of line it was on, which is what its session waits for.
 *
 * @module
 */

import * as protocol from "./protocol.js";
import { Refused } from "./protocol.js";
import { Call, FORMAT, Unreached, callsOf, dial } from "./webrtc.js";

export { Refused };

/**
 * @typedef {import("./protocol.js").FromServer} FromServer
 * @typedef {import("./protocol.js").FromClient} FromClient
 * @typedef {import("./protocol.js").Welcome} Welcome
 * @typedef {import("./protocol.js").Resume} Resume
 * @typedef {import("./protocol.js").AudioFormat} AudioFormat
 * @typedef {import("./protocol.js").ErrorCode} ErrorCode
 * @typedef {import("./protocol.js").ByeReason} ByeReason
 */

/**
 * How a client's part in a session ended.
 *
 * - `ended`: the session is over.
 * - `replaced`: another client took this one's place on the line.
 * - `refused`: the server turned the client away as it came back.
 * - `gone`: the line dropped and could not be had back in the time the
 *   session waits.
 * - `left`: the client let go of the line itself (`detach`), and the
 *   session goes on without it.
 * - `unknown`: the server said goodbye for a reason this client does
 *   not know.
 *
 * @typedef {"ended" | "replaced" | "refused" | "gone" | "left" | "unknown"} Ending
 */

/**
 * What a client tells whoever listens.
 *
 * @typedef {object} Events
 * @property {(message: FromServer) => void} message Everything the
 *   server says that is not audio, once the view has taken it.
 * @property {(fact: Extract<FromServer, { type: "fact" }>) => void} fact
 *   A fact the client did not have.
 * @property {(news: Extract<FromServer, { type: "told" }>) => void} told
 *   What is going on, which is not kept.
 * @property {(view: protocol.View) => void} view The view changed.
 * @property {(mark: Extract<FromServer, { type: "mark" }>) => void} mark
 *   A run of audio begins, or has been sent whole.
 * @property {(clear: Extract<FromServer, { type: "clear" }>) => void} clear
 *   What is held of a run and was not played is to be discarded.
 * @property {(audio: ArrayBuffer) => void} audio Audio of the run that
 *   was marked last, as the hello said it is written.
 * @property {(error: { code: ErrorCode | "malformed", message: string }) => void} error
 *   The server could not take something, or said something that is no
 *   message. The connection goes on.
 * @property {(why: { reason: "dropped" | "going_away" }) => void} away
 *   The line is gone, and the client is coming back.
 * @property {(welcome: Welcome) => void} back The client is on the line
 *   again.
 * @property {(end: { reason: Ending, error?: Refused }) => void} closed
 *   The client's part in the session is over.
 */

/**
 * @typedef {object} Options
 * @property {string} [token] The client token the session was made
 *   with. It is good once: a client that comes back does so with what
 *   it was welcomed with.
 * @property {AudioFormat} [audio] How audio is written both ways. Left
 *   out, the line is one of text.
 * @property {string[]} [features] What the client wants to be told
 *   beyond the facts: `said`, `hearing`, `tools`.
 * @property {Resume} [resume] Where a client was that comes back to a
 *   session it left (`Client.resume`), in place of a token.
 * @property {number} [patienceMs] How long the client tries to come
 *   back where the welcome names no time. 30 s if left out.
 * @property {number} [welcomeWithinMs] How long a hello may go
 *   unanswered. 10 s if left out.
 * @property {"websocket" | "webrtc"} [over] What the client is on its
 *   session over: a WebSocket if left out, or a call over WebRTC, which
 *   carries its audio by itself (`audio` is of no account then).
 * @property {MediaStreamTrack} [microphone] What a call sends: the
 *   track of a microphone. A call without one hears and sends nothing.
 * @property {number} [callWithinMs] How long a call may take to be
 *   there, from its offer on. 5 s if left out.
 * @property {typeof WebSocket} [WebSocket] What connections are made
 *   with, where it is not the one the platform has.
 * @property {typeof RTCPeerConnection} [RTCPeerConnection] What calls
 *   are made with, where it is not the one the platform has.
 * @property {typeof fetch} [fetch] What offers are posted with, where
 *   it is not the one the platform has.
 * @property {() => number} [random] A number from 0 up to 1, for the
 *   spread of the waits between tries.
 */

/** How long the client waits before each try to come back, in order. */
const WAITS = [0, 250, 500, 1000, 2000];

/** How many things a user wrote wait for a line that is away. */
const MOST_WAITING = 32;

/**
 * The address of a server's line for clients. A server may be named by
 * the address its HTTP API is at.
 *
 * @param {string | URL} address
 * @returns {string}
 */
export function lineOf(address) {
  const url = new URL(address);
  if (url.protocol === "http:") {
    url.protocol = "ws:";
  } else if (url.protocol === "https:") {
    url.protocol = "wss:";
  }
  if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw new RangeError("a line is at an address that begins ws:, wss:, http: or https:");
  }
  // Who connects is said in the hello, and never in the address.
  if (url.search !== "" || url.username !== "" || url.password !== "") {
    throw new RangeError("the address of a line says nothing of who connects");
  }
  url.hash = "";
  if (url.pathname === "/" || url.pathname === "") {
    url.pathname = "/v1/client";
  }
  return url.href;
}

/** A client of one session. */
export class Client {
  /** @type {string} */
  #address;

  /** @type {Options} */
  #options;

  /** @type {typeof WebSocket} */
  #WebSocket;

  /** What the client says and is told on. @type {WebSocket | Call | undefined} */
  #socket;

  /** @type {"open" | "away" | "closed"} */
  #status = "away";

  /** @type {Welcome} */
  #welcome;

  /**
   * What the client comes back with: the token of the last welcome that
   * named one, or the one it came with.
   *
   * @type {string | undefined}
   */
  #token;

  #view = new protocol.View();

  /** @type {{ [K in keyof Events]?: Set<Events[K]> }} */
  #listeners = {};

  /** What the user wrote while the line was away. @type {FromClient[]} */
  #waiting = [];

  /** @type {ReturnType<typeof setTimeout> | undefined} */
  #timer;

  /**
   * @param {string} address
   * @param {Options} options
   * @param {Welcome} welcome
   * @param {WebSocket | Call} socket
   */
  constructor(address, options, welcome, socket) {
    this.#address = address;
    this.#options = options;
    this.#WebSocket = options.WebSocket ?? globalThis.WebSocket;
    this.#welcome = welcome;
    this.#token = welcome.resume_token ?? options.resume?.token;
    this.#attach(socket);
  }

  /**
   * Connects to the server at `address`, says hello, and waits to be
   * welcomed.
   *
   * @param {string | URL} address Where the server's line for clients
   *   is, or where its HTTP API is.
   * @param {Options} [options]
   * @returns {Promise<Client>}
   * @throws {Refused} The server turned the hello away.
   * @throws {Unreached} A call was asked for (`over: "webrtc"`) and
   *   could not be made. Nothing was said on it: its token is as good
   *   as it was.
   * @throws {Error} The server could not be reached, or did not answer.
   */
  static async connect(address, options = {}) {
    const calls = options.over === "webrtc";
    const line = calls ? callsOf(address) : lineOf(address);
    const { token, audio, features, resume } = options;
    // Said here, so that what cannot be said is not tried. The audio
    // of a call is what its offer and its answer agree on.
    const first = protocol.hello({ token, audio: calls ? undefined : audio, features, resume });
    const { socket, welcome } = await reach(line, first, options);
    return new Client(line, options, welcome, socket);
  }

  /** What the server welcomed the client with, the last time it did. */
  get welcome() {
    return this.#welcome;
  }

  /** The name of the session. */
  get session() {
    return this.#welcome.session;
  }

  /** What the session has come to, as far as the client was told. */
  get view() {
    return this.#view;
  }

  /**
   * Whether the client is on the line (`open`), is coming back to it
   * (`away`), or has no part in the session any more (`closed`).
   */
  get status() {
    return this.#status;
  }

  /**
   * How audio is written both ways, as the server took it. Nothing for
   * a line of text.
   *
   * @returns {AudioFormat | undefined}
   */
  get audio() {
    if (this.#options.over === "webrtc") {
      return FORMAT;
    }
    return this.#welcome.audio ?? this.#options.audio;
  }

  /** What the client is on its session over. */
  get over() {
    return this.#options.over ?? "websocket";
  }

  /**
   * The call the client is on, which carries its audio. Nothing for a
   * client on a WebSocket, and for one whose call dropped until it is
   * back on another.
   *
   * @returns {Call | undefined}
   */
  get call() {
    const socket = this.#socket;
    return socket instanceof Call ? socket : undefined;
  }

  /**
   * Where the client is in its session: what `Client.connect` is given
   * as `resume` to come back to it, from this page or the next. Nothing
   * if the server gave the client nothing to come back with.
   *
   * @returns {Resume | undefined}
   */
  get resume() {
    const token = this.#token;
    return token === undefined
      ? undefined
      : { token, life: this.#welcome.life, seen: this.#view.seen };
  }

  /**
   * Listens for something the client tells. Returns what stops the
   * listening.
   *
   * @template {keyof Events} K
   * @param {K} type
   * @param {Events[K]} listener
   * @returns {() => void}
   */
  on(type, listener) {
    /** @type {Set<Events[K]> | undefined} */
    let listeners = this.#listeners[type];
    if (listeners === undefined) {
      listeners = new Set();
      this.#listeners[type] = /** @type {any} */ (listeners);
    }
    const all = listeners;
    all.add(listener);
    return () => {
      all.delete(listener);
    };
  }

  /**
   * Writes to the agent. What is written while the line is away is
   * sent when it is back.
   *
   * @param {string} written
   */
  text(written) {
    this.#say(protocol.text(written), true);
  }

  /**
   * Presses keys of a telephone.
   *
   * @param {string} keys
   */
  dtmf(keys) {
    this.#say(protocol.dtmf(keys), true);
  }

  /** Stops the agent. The session goes on. */
  interrupt() {
    this.#say(protocol.interrupt(), false);
  }

  /**
   * Says how many samples of a run have been played.
   *
   * @param {number} run
   * @param {number} samples
   */
  played(run, samples) {
    this.#say(protocol.played(run, samples), false);
  }

  /**
   * Says what a call has played, as its browser can say it
   * (`protocol.playedRtp`). The voice of a call does.
   *
   * @param {Parameters<typeof protocol.playedRtp>[0]} played
   */
  playedRtp(played) {
    this.#say(protocol.playedRtp(played), false);
  }

  /**
   * Sends audio, as the hello said it is written. Audio for a line that
   * is away is not kept, and a call takes none: it carries what its
   * microphone hears by itself.
   *
   * @param {ArrayBuffer | ArrayBufferView} audio
   */
  send(audio) {
    if (this.#status === "open" && this.#socket !== undefined) {
      this.#socket.send(/** @type {ArrayBuffer} */ (audio));
    }
  }

  /** Ends the session. */
  bye() {
    if (this.#status === "closed") {
      return;
    }
    if (this.#status === "open") {
      this.#write(protocol.bye());
      // A goodbye on a call can be lost with the call: it is said
      // where the call was offered too.
      if (this.#socket instanceof Call) {
        this.#socket.hangUp();
      }
    }
    this.#close("ended");
  }

  /**
   * Lets go of the line, and does not come back. The session goes on
   * for as long as it waits for a client: `resume`, read before this,
   * is what another `Client.connect` comes back to it with.
   */
  detach() {
    if (this.#status !== "closed") {
      this.#close("left");
    }
  }

  /**
   * Lets go of the connection and comes back on another: for a page
   * that knows its network changed before the connection does.
   */
  reconnect() {
    if (this.#status === "open") {
      this.#dropped("dropped");
    }
  }

  /**
   * @template {keyof Events} K
   * @param {K} type
   * @param {Parameters<Events[K]>} told
   */
  #tell(type, ...told) {
    const listeners = this.#listeners[type];
    if (listeners === undefined) {
      return;
    }
    for (const listener of [...listeners]) {
      /** @type {(...told: any[]) => void} */ (listener)(...told);
    }
  }

  /** @param {FromClient} message */
  #write(message) {
    this.#socket?.send(protocol.write(message));
  }

  /**
   * @param {FromClient} message
   * @param {boolean} keeps Whether it waits for a line that is away.
   */
  #say(message, keeps) {
    if (this.#status === "open") {
      this.#write(message);
    } else if (this.#status === "away" && keeps) {
      if (this.#waiting.length >= MOST_WAITING) {
        throw new Error("too much waits for the line to be back");
      }
      this.#waiting.push(message);
    } else if (this.#status === "closed" && keeps) {
      throw new Error("the client has no part in the session any more");
    }
  }

  /**
   * Takes a connection that was welcomed.
   *
   * @param {WebSocket | Call} socket
   */
  #attach(socket) {
    this.#socket = socket;
    this.#status = "open";
    socket.onmessage = (/** @type {{ data: unknown }} */ event) => {
      if (socket === this.#socket) {
        this.#heard(event.data);
      }
    };
    socket.onclose = () => {
      if (socket === this.#socket && this.#status === "open") {
        this.#dropped("dropped");
      }
    };
    socket.onerror = null;
    for (const message of this.#waiting.splice(0)) {
      this.#write(message);
    }
  }

  /** @param {unknown} data */
  #heard(data) {
    if (typeof data !== "string") {
      if (data instanceof ArrayBuffer) {
        this.#tell("audio", data);
      }
      return;
    }
    /** @type {FromServer} */
    let message;
    try {
      message = protocol.read(data);
    } catch (error) {
      const what = error instanceof Error ? error.message : "what was said is no message";
      this.#tell("error", { code: "malformed", message: what });
      return;
    }
    const applied = this.#view.apply(message);
    this.#tell("message", message);
    switch (message.type) {
      case "fact":
        if (applied === "fact") {
          this.#tell("fact", message);
          this.#tell("view", this.#view);
        }
        break;
      case "state":
        this.#tell("view", this.#view);
        break;
      case "told":
        this.#tell("told", message);
        break;
      case "mark":
        this.#tell("mark", message);
        break;
      case "clear":
        this.#tell("clear", message);
        break;
      case "error":
        this.#tell("error", { code: message.code, message: message.message });
        break;
      case "bye":
        this.#parted(message.reason);
        break;
      default:
        break;
    }
  }

  /**
   * The server said goodbye.
   *
   * @param {ByeReason} reason
   */
  #parted(reason) {
    if (reason === "going_away") {
      // The server stops, and the session is kept: its client comes
      // back to the server that is started next.
      this.#dropped("going_away");
    } else {
      this.#close(reason);
    }
  }

  /**
   * The line is gone, and nobody ended the session.
   *
   * @param {"dropped" | "going_away"} reason
   */
  #dropped(reason) {
    this.#release();
    if (this.#token === undefined) {
      this.#close("gone");
      return;
    }
    this.#status = "away";
    this.#tell("away", { reason });
    const patience = this.#welcome.grace_ms ?? this.#options.patienceMs ?? 30_000;
    this.#comeBack(0, Date.now() + patience);
  }

  /** Lets go of the connection there is, without a word. */
  #release() {
    const socket = this.#socket;
    this.#socket = undefined;
    if (socket !== undefined) {
      socket.onmessage = null;
      socket.onclose = null;
      socket.onerror = null;
      try {
        socket.close();
      } catch {
        // One that is closed already is closed.
      }
    }
  }

  /**
   * Tries to come back, and tries again until `until`.
   *
   * @param {number} tries How many tries there were.
   * @param {number} until When the session no longer waits.
   */
  #comeBack(tries, until) {
    const random = this.#options.random ?? Math.random;
    const wait = WAITS[Math.min(tries, WAITS.length - 1)] ?? 0;
    // Clients that lost the same server do not all come back at once.
    const spread = wait * (0.8 + 0.4 * random());
    this.#timer = setTimeout(() => {
      this.#timer = undefined;
      void this.#tryToComeBack(tries, until);
    }, spread);
  }

  /**
   * @param {number} tries
   * @param {number} until
   */
  async #tryToComeBack(tries, until) {
    const token = this.#token;
    if (this.#status !== "away" || token === undefined) {
      return;
    }
    const { audio, features } = this.#options;
    const resume = { token, life: this.#welcome.life, seen: this.#view.seen };
    try {
      const calls = this.#options.over === "webrtc";
      const again = protocol.hello({ audio: calls ? undefined : audio, features, resume });
      const { socket, welcome } = await reach(this.#address, again, this.#options);
      if (this.#status !== "away") {
        // It was let go of while it came back.
        socket.close();
        return;
      }
      this.#welcome = welcome;
      this.#token = welcome.resume_token ?? token;
      this.#attach(socket);
      this.#tell("back", welcome);
    } catch (error) {
      if (this.#status !== "away") {
        return;
      }
      // A server that is busy is one that is stopping or starting: the
      // next try may find the one that follows it. An offer that was
      // turned away for what it is or for who made it is turned away
      // again.
      if (error instanceof Refused && error.code !== "busy") {
        this.#close("refused", error);
      } else if (error instanceof Unreached && error.final) {
        const code = /** @type {ErrorCode} */ (error.code ?? "unknown");
        this.#close("refused", new Refused(code, error.message));
      } else if (Date.now() >= until) {
        this.#close("gone");
      } else {
        this.#comeBack(tries + 1, until);
      }
    }
  }

  /**
   * @param {Ending} reason
   * @param {Refused} [error]
   */
  #close(reason, error) {
    if (this.#status === "closed") {
      return;
    }
    this.#status = "closed";
    if (this.#timer !== undefined) {
      clearTimeout(this.#timer);
      this.#timer = undefined;
    }
    this.#waiting.length = 0;
    this.#release();
    this.#tell("closed", error === undefined ? { reason } : { reason, error });
  }
}

/**
 * Reaches the session where `line` says, over what `options` say, and
 * says `first` there.
 *
 * @param {string} line
 * @param {FromClient} first The hello.
 * @param {Options} options
 * @returns {Promise<{ socket: WebSocket | Call, welcome: Welcome }>}
 */
async function reach(line, first, options) {
  if (options.over !== "webrtc") {
    return greet(line, first, options);
  }
  // An offer is made with the token its hello says.
  const bearer = first.type === "hello" ? (first.resume?.token ?? first.token) : undefined;
  if (bearer === undefined) {
    throw new Unreached("a call is offered with the token of its session, and there is none");
  }
  return dial(line, first, bearer, options);
}

/**
 * Opens a connection, says hello on it, and waits to be welcomed.
 *
 * @param {string} line
 * @param {FromClient} first
 * @param {Options} options
 * @returns {Promise<{ socket: WebSocket, welcome: Welcome }>}
 */
function greet(line, first, options) {
  const Socket = options.WebSocket ?? globalThis.WebSocket;
  const within = options.welcomeWithinMs ?? 10_000;
  return new Promise((resolve, reject) => {
    /** @type {WebSocket} */
    let socket;
    try {
      socket = new Socket(line, protocol.NAME);
    } catch {
      reject(new Error("the server cannot be reached at this address"));
      return;
    }
    socket.binaryType = "arraybuffer";
    /** What the server refused the hello with, before its goodbye. @type {Refused | undefined} */
    let refused;
    /** @param {Error} [error] */
    const over = (error) => {
      clearTimeout(timer);
      socket.onopen = null;
      socket.onmessage = null;
      socket.onclose = null;
      socket.onerror = null;
      if (error !== undefined) {
        try {
          socket.close();
        } catch {
          // One that is closed already is closed.
        }
        reject(error);
      }
    };
    const timer = setTimeout(() => {
      over(new Error("the server did not answer the hello"));
    }, within);
    socket.onopen = () => {
      socket.send(protocol.write(first));
    };
    socket.onmessage = (event) => {
      if (typeof event.data !== "string") {
        return;
      }
      /** @type {FromServer} */
      let message;
      try {
        message = protocol.read(event.data);
      } catch {
        over(new Error("the server answered the hello with what is no message"));
        return;
      }
      if (message.type === "welcome") {
        over();
        resolve({ socket, welcome: message });
      } else if (message.type === "error") {
        refused = new Refused(message.code, message.message);
      } else if (message.type === "bye") {
        over(refused ?? new Refused("unknown", "the server said goodbye to the hello"));
      }
    };
    socket.onclose = () => {
      over(refused ?? new Error("the connection closed before the server answered the hello"));
    };
    socket.onerror = () => {
      over(refused ?? new Error("the server cannot be reached"));
    };
  });
}
