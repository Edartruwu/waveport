// @ts-check
/**
 * A client of a Waveport session, for a page in a browser.
 *
 * ```js
 * import { call } from "./index.js";
 *
 * // The page's server makes the session and gives the page its token.
 * const token = () => fetch("/token", { method: "POST" }).then((made) => made.text());
 * const { client, voice } = await call("https://agents.example", { token, voice: true });
 * client.on("fact", (fact) => console.log(fact));
 * client.text("What time is it?");
 * ```
 *
 * `Client` is the connection: what the session says and what its far
 * end does, with a view of the session that it keeps as it is told.
 * `Voice` plays and hears on a WebSocket, and `CallVoice` on a call
 * over WebRTC. `call` is a client and its voice together, in the order
 * that loses nothing: the voice is ready before the client connects.
 *
 * A call that is spoken is made over WebRTC where that gets through,
 * and over a WebSocket where it does not.
 *
 * @module
 */

import { Voice } from "./audio.js";
import { Client } from "./client.js";
import { CallVoice, Unreached } from "./webrtc.js";

export { Voice } from "./audio.js";
export { Client, lineOf, Refused } from "./client.js";
export * as protocol from "./protocol.js";
export { CallVoice, Unreached, callsOf } from "./webrtc.js";

/**
 * @typedef {object} CallOptions
 * @property {boolean | import("./audio.js").VoiceOptions} [voice]
 *   Whether the call is spoken. Left out or `false`, it is written.
 * @property {string | (() => string | Promise<string>)} [token] The
 *   token the client connects with, or what asks for one. A token is
 *   good for a short while, and a page's user can take longer than
 *   that to allow a microphone: what asks for one is called once the
 *   voice is ready.
 * @property {"websocket" | "webrtc"} [over] What a call that is spoken
 *   is made over. Left out, it is made over WebRTC where the browser
 *   makes such calls and one gets through to the server, and over a
 *   WebSocket otherwise. A call that is written is on a WebSocket.
 */

/**
 * Whether a call over WebRTC is tried: the browser makes such calls,
 * and the server is named by where it is, and not by the address of
 * its line for clients alone.
 *
 * @param {string | URL} address
 * @param {import("./client.js").Options} options
 */
function calls(address, options) {
  const Peer = options.RTCPeerConnection ?? globalThis.RTCPeerConnection;
  const path = new URL(address).pathname;
  return typeof Peer === "function" && (path === "/" || path === "");
}

/**
 * Connects to a session, and speaks with it if asked to. Called from a
 * click or a key when it is to speak: a browser plays no sound for a
 * page its user has not touched.
 *
 * A call that is spoken has its voice ready before it connects, and
 * on the line the moment it is welcomed: what an agent says first, it
 * says at once.
 *
 * It is made over WebRTC, and over a WebSocket when no call comes
 * about: the server takes none, or neither datagrams nor a connection
 * get through to it in the time a call has (`callWithinMs`). The
 * token is the same for both, since a call that was not made has said
 * it to nobody but the server it was offered to.
 *
 * @param {string | URL} address Where the server is.
 * @param {Omit<import("./client.js").Options, "audio" | "token" | "over" | "microphone"> & CallOptions} [options]
 * @returns {Promise<{ client: Client, voice?: Voice | CallVoice }>}
 */
export async function call(address, options = {}) {
  const { voice: spoken = false, token: held, over, ...rest } = options;
  const token = async () => (typeof held === "function" ? await held() : held);
  if (spoken === false) {
    return { client: await Client.connect(address, { ...rest, token: await token() }) };
  }
  /** @type {import("./audio.js").VoiceOptions} */
  let asked = spoken === true ? {} : spoken;
  /** The token, once it was asked for. @type {{ token: string | undefined } | undefined} */
  let said;
  if (over === "webrtc" || (over === undefined && calls(address, rest))) {
    const voice = await CallVoice.open(asked);
    try {
      said = { token: await token() };
      const client = await Client.connect(address, {
        ...rest,
        over: "webrtc",
        microphone: voice.track,
        token: said.token,
      });
      try {
        // At once: nothing is waited for between the welcome and this.
        voice.attach(client);
      } catch (error) {
        // A call that cannot be spoken is not left running.
        client.bye();
        throw error;
      }
      return { client, voice };
    } catch (error) {
      if (!(error instanceof Unreached) || over === "webrtc") {
        voice.stop();
        throw error;
      }
      // No call came about. The microphone is there already, and goes
      // to the voice of a WebSocket.
      asked = { ...asked, microphone: voice.release(), owned: true };
    }
  }
  const audio = await Voice.format();
  const voice = await Voice.open(audio, asked).catch((error) => {
    // A microphone that was taken over is let go of with the voice
    // that was not made.
    if (asked.owned === true) {
      for (const track of asked.microphone?.getTracks() ?? []) {
        track.stop();
      }
    }
    throw error;
  });
  try {
    said ??= { token: await token() };
    const client = await Client.connect(address, { ...rest, audio, token: said.token });
    try {
      // At once: nothing is waited for between the welcome and this.
      voice.attach(client);
    } catch (error) {
      // A call that cannot be spoken is not left running.
      client.bye();
      throw error;
    }
    return { client, voice };
  } catch (error) {
    voice.stop();
    throw error;
  }
}
