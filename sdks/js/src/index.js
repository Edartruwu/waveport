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
 * `Voice` plays and hears. `call` is the two together, in the order
 * that loses nothing: the voice is ready before the client connects.
 *
 * @module
 */

import { Voice } from "./audio.js";
import { Client } from "./client.js";

export { Voice } from "./audio.js";
export { Client, lineOf, Refused } from "./client.js";
export * as protocol from "./protocol.js";

/**
 * @typedef {object} CallOptions
 * @property {boolean | import("./audio.js").VoiceOptions} [voice]
 *   Whether the call is spoken. Left out or `false`, it is written.
 * @property {string | (() => string | Promise<string>)} [token] The
 *   token the client connects with, or what asks for one. A token is
 *   good for a short while, and a page's user can take longer than
 *   that to allow a microphone: what asks for one is called once the
 *   voice is ready.
 */

/**
 * Connects to a session, and speaks with it if asked to. Called from a
 * click or a key when it is to speak: a browser plays no sound for a
 * page its user has not touched.
 *
 * A call that is spoken has its voice ready before it connects, and
 * on the line the moment it is welcomed: what an agent says first, it
 * says at once.
 *
 * @param {string | URL} address Where the server is.
 * @param {Omit<import("./client.js").Options, "audio" | "token"> & CallOptions} [options]
 * @returns {Promise<{ client: Client, voice?: Voice }>}
 */
export async function call(address, options = {}) {
  const { voice: spoken = false, token: held, ...rest } = options;
  const token = async () => (typeof held === "function" ? await held() : held);
  if (spoken === false) {
    return { client: await Client.connect(address, { ...rest, token: await token() }) };
  }
  const audio = await Voice.format();
  const voice = await Voice.open(audio, spoken === true ? {} : spoken);
  try {
    const client = await Client.connect(address, { ...rest, audio, token: await token() });
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
