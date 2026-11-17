// @ts-check
/**
 * Client protocol v1, as a client reads and writes it.
 *
 * Everything that is not audio is one JSON object whose `type` says what
 * it is. `read` takes what a server said and gives it back with nothing
 * in it that this version does not know: a field it does not know is
 * left out, and a message, a fact or a piece of news of a kind it does
 * not know is given as `unknown`, so a server can say more than this
 * client understands. The writers (`hello`, `played`, `text`, `dtmf`,
 * `interrupt`, `bye`) make what a client says, and refuse to make what
 * a server would refuse.
 *
 * A `View` is what the facts a client was sent add up to.
 *
 * The same messages are defined for servers in the Rust crate
 * `waveport-protocol` (`client::v1`), and both are held to one set of
 * fixtures.
 *
 * @module
 */

/** The version of the protocol. */
export const VERSION = 1;

/** The name of the protocol: the subprotocol of a WebSocket. */
export const NAME = "waveport.client.v1";

/** What a client token begins with. */
export const CLIENT_TOKEN = "wpc1_";

/** What a resume token begins with. */
export const RESUME_TOKEN = "wpr1_";

/** The rates audio can have, in samples a second. */
export const RATES = Object.freeze([8000, 16000, 24000, 48000]);

/** How many keys one `dtmf` message may hold. */
export const MOST_KEYS = 32;

/** How many bytes of text one `text` message may hold. */
export const MOST_TEXT = 16 * 1024;

/** The keys of a telephone, as they are written. */
export const KEYS = "0123456789*#ABCD";

/**
 * @typedef {"pcm16" | "opus"} Codec
 * @typedef {{ codec: Codec, rate: number }} AudioFormat
 * @typedef {{ token: string, life: number, seen: number }} Resume
 *
 * @typedef {"running" | "draining" | "closing" | "closed" | "unknown"} Lifecycle
 * @typedef {"requested" | "abandoned" | "failure" | "unknown"} CloseReason
 * @typedef {"barge_in" | "interrupted" | "handoff" | "superseded" | "failure" | "recovery" | "closing" | "unknown"} Cut
 * @typedef {"backchannel" | "noise" | "empty" | "closed" | "unknown"} Dismissal
 * @typedef {"completed" | "failed" | "cancelled" | "in_doubt" | "unknown"} ToolOutcome
 * @typedef {"listening" | "thinking" | "speaking" | "unknown"} Activity
 * @typedef {"ended" | "replaced" | "going_away" | "refused" | "unknown"} ByeReason
 * @typedef {"version" | "token" | "forbidden" | "malformed" | "unknown_message" | "out_of_place" | "unsupported" | "busy" | "no_session" | "no_agent" | "internal" | "unknown"} ErrorCode
 */

/**
 * Something that became true of a session.
 *
 * @typedef {{ kind: "participant_joined", who: number }
 *   | { kind: "participant_left", who: number }
 *   | { kind: "lifecycle_changed", to: Lifecycle, reason?: CloseReason }
 *   | { kind: "cut_off", cause: Cut }
 *   | { kind: "turn_committed", turn: number, who: number, text: string, addressed: boolean }
 *   | { kind: "keys_committed", turn: number, who: number, keys: string }
 *   | { kind: "turn_dismissed", turn: number, who: number, reason: Dismissal }
 *   | { kind: "utterance_progress", generation: number, agent: number, heard: string }
 *   | { kind: "utterance_committed", generation: number, agent: number, text: string, truncated: boolean }
 *   | { kind: "tool_called", call: number, tool: string, generation: number, args?: string }
 *   | { kind: "tool_sent", call: number, attempt: number }
 *   | { kind: "tool_finished", call: number, outcome: ToolOutcome, result?: string }
 *   | { kind: "agent_activated", agent: number }
 *   | { kind: "unknown" }} Fact
 */

/**
 * What is going on in a session, and is not kept.
 *
 * @typedef {{ kind: "user_speaking", who: number, turn: number, speaking: boolean }
 *   | { kind: "agent", agent: number, generation?: number, activity: Activity }
 *   | { kind: "said", generation: number, text: string }
 *   | { kind: "hearing", who: number, text: string }
 *   | { kind: "unknown" }} News
 */

/**
 * One thing in a conversation.
 *
 * @typedef {{ kind: "turn", turn: number, who: number, text: string, addressed: boolean }
 *   | { kind: "keys", turn: number, who: number, keys: string }
 *   | { kind: "utterance", generation: number, agent: number, text: string, truncated: boolean }
 *   | { kind: "tool_call", call: number, tool: string, generation: number, args?: string }
 *   | { kind: "tool_result", call: number, outcome: ToolOutcome, result?: string }
 *   | { kind: "unknown" }} Item
 */

/**
 * What a session has come to.
 *
 * @typedef {object} State
 * @property {number} upto The number of the last fact the state includes.
 * @property {Lifecycle} lifecycle The stage of the session's life.
 * @property {CloseReason} [closed_for] Why the session is closing, once it is.
 * @property {number} agent The agent that has the floor.
 * @property {number[]} participants The humans in the session.
 * @property {Item[]} items The conversation, oldest item first.
 * @property {{ generation: number, agent: number, heard: string }} [speech]
 *   The response that is being heard, if one is.
 */

/**
 * @typedef {object} Welcome
 * @property {"welcome"} type
 * @property {number} v
 * @property {string} session The name of the session.
 * @property {number} life Which of its lives the session is in.
 * @property {boolean} resumed Whether the client came back to it.
 * @property {string} [resume_token] What the client comes back with.
 * @property {AudioFormat} [audio] How audio is written, if any is.
 * @property {string[]} [features] What the client asked for and gets.
 * @property {number} next_seq The number the session's next fact has.
 * @property {number} [grace_ms] How long the session waits for a client
 *   whose connection dropped.
 */

/**
 * What a server says.
 *
 * @typedef {Welcome
 *   | { type: "mark", run: number, generation: number, end?: true }
 *   | { type: "clear", run: number }
 *   | ({ type: "fact", seq: number } & Fact)
 *   | ({ type: "told" } & News)
 *   | { type: "gap", resumes: number }
 *   | ({ type: "state" } & State)
 *   | { type: "error", code: ErrorCode, message: string }
 *   | { type: "bye", reason: ByeReason }
 *   | { type: "unknown" }} FromServer
 */

/**
 * What a client says.
 *
 * @typedef {{ type: "hello", v: number, token?: string, audio?: AudioFormat, features?: string[], resume?: Resume }
 *   | { type: "played", run: number, samples: number }
 *   | { type: "text", text: string }
 *   | { type: "dtmf", keys: string }
 *   | { type: "interrupt" }
 *   | { type: "bye" }} FromClient
 */

/** A message that does not say what a message of its kind says. */
export class Malformed extends Error {
  /** @param {string} what What is wrong, with nothing of what was sent. */
  constructor(what) {
    super(what);
    this.name = "Malformed";
  }
}

/**
 * What one field of a message holds, and how it is read.
 *
 * @typedef {(value: unknown, name: string) => unknown} Reads
 */

/** @type {Reads} */
const int = (value, name) => {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new Malformed(`${name} is not a whole number`);
  }
  return value;
};

/**
 * @param {unknown} value
 * @param {string} name
 * @returns {string}
 */
const str = (value, name) => {
  if (typeof value !== "string") {
    throw new Malformed(`${name} is not text`);
  }
  return value;
};

/** @type {Reads} */
const bool = (value, name) => {
  if (typeof value !== "boolean") {
    throw new Malformed(`${name} is neither true nor false`);
  }
  return value;
};

/**
 * One of these words. A word that is none of them is `unknown`: a server
 * may know more of them than this client.
 *
 * @param {...string} words
 * @returns {Reads}
 */
const oneOf = (...words) => (value, name) => {
  const word = str(value, name);
  return words.includes(word) ? word : "unknown";
};

/**
 * One of these words, and nothing else.
 *
 * @param {...string} words
 * @returns {Reads}
 */
const only = (...words) => (value, name) => {
  const word = str(value, name);
  if (!words.includes(word)) {
    throw new Malformed(`${name} is none of what it can be`);
  }
  return word;
};

/** A field that may be left out, and is left out of what is read. */
const LEFT_OUT = Symbol("left out");

/**
 * A field that may be left out.
 *
 * @param {Reads} reads
 * @returns {Reads}
 */
const maybe = (reads) => (value, name) =>
  value === undefined || value === null ? LEFT_OUT : reads(value, name);

/**
 * A flag that is left out when it is not raised.
 *
 * @type {Reads}
 */
const flag = (value, name) =>
  value === undefined || bool(value, name) === false ? LEFT_OUT : true;

/**
 * A list.
 *
 * @param {Reads} each
 * @returns {Reads}
 */
const listOf = (each) => (value, name) => {
  if (!Array.isArray(value)) {
    throw new Malformed(`${name} is not a list`);
  }
  return value.map((one) => each(one, name));
};

/**
 * A list that is left out when nothing is in it.
 *
 * @param {Reads} each
 * @returns {Reads}
 */
const someOf = (each) => (value, name) => {
  if (value === undefined) {
    return LEFT_OUT;
  }
  const list = /** @type {unknown[]} */ (listOf(each)(value, name));
  return list.length === 0 ? LEFT_OUT : list;
};

/**
 * An object with these fields, and none it is not known to have.
 *
 * @param {Record<string, Reads>} fields
 * @returns {Reads}
 */
const shaped = (fields) => (value, name) => {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Malformed(`${name} is not an object`);
  }
  const given = /** @type {Record<string, unknown>} */ (value);
  /** @type {Record<string, unknown>} */
  const read = {};
  for (const [field, reads] of Object.entries(fields)) {
    const held = reads(given[field], `${name}.${field}`);
    if (held !== LEFT_OUT) {
      read[field] = held;
    }
  }
  return read;
};

/**
 * An object that says of which kind it is in the field `tag`, with the
 * fields of that kind. One of a kind that is not known is `unknown`,
 * with nothing else in it but `also`.
 *
 * @param {string} tag
 * @param {Record<string, Record<string, Reads>>} kinds
 * @param {Record<string, Reads>} [also] Fields of every kind.
 * @returns {Reads}
 */
const tagged = (tag, kinds, also = {}) => (value, name) => {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Malformed(`${name} is not an object`);
  }
  const given = /** @type {Record<string, unknown>} */ (value);
  const kind = str(given[tag], `${name}.${tag}`);
  const fields = Object.hasOwn(kinds, kind) ? kinds[kind] : undefined;
  const common = /** @type {Record<string, unknown>} */ (shaped(also)(value, name));
  if (fields === undefined) {
    return { ...common, [tag]: "unknown" };
  }
  const own = /** @type {Record<string, unknown>} */ (shaped(fields)(value, `${name} ${kind}`));
  return { ...common, [tag]: kind, ...own };
};

const lifecycle = oneOf("running", "draining", "closing", "closed");
const closeReason = oneOf("requested", "abandoned", "failure");
const toolOutcome = oneOf("completed", "failed", "cancelled", "in_doubt");
const audio = shaped({ codec: only("pcm16", "opus"), rate: int });

/** @type {Record<string, Record<string, Reads>>} */
const FACTS = {
  participant_joined: { who: int },
  participant_left: { who: int },
  lifecycle_changed: { to: lifecycle, reason: maybe(closeReason) },
  cut_off: {
    cause: oneOf(
      "barge_in",
      "interrupted",
      "handoff",
      "superseded",
      "failure",
      "recovery",
      "closing",
    ),
  },
  turn_committed: { turn: int, who: int, text: str, addressed: bool },
  keys_committed: { turn: int, who: int, keys: str },
  turn_dismissed: {
    turn: int,
    who: int,
    reason: oneOf("backchannel", "noise", "empty", "closed"),
  },
  utterance_progress: { generation: int, agent: int, heard: str },
  utterance_committed: { generation: int, agent: int, text: str, truncated: bool },
  tool_called: { call: int, tool: str, generation: int, args: maybe(str) },
  tool_sent: { call: int, attempt: int },
  tool_finished: { call: int, outcome: toolOutcome, result: maybe(str) },
  agent_activated: { agent: int },
};

/** @type {Record<string, Record<string, Reads>>} */
const NEWS = {
  user_speaking: { who: int, turn: int, speaking: bool },
  agent: {
    agent: int,
    generation: maybe(int),
    activity: oneOf("listening", "thinking", "speaking"),
  },
  said: { generation: int, text: str },
  hearing: { who: int, text: str },
};

/** @type {Record<string, Record<string, Reads>>} */
const ITEMS = {
  turn: { turn: int, who: int, text: str, addressed: bool },
  keys: { turn: int, who: int, keys: str },
  utterance: { generation: int, agent: int, text: str, truncated: bool },
  tool_call: { call: int, tool: str, generation: int, args: maybe(str) },
  tool_result: { call: int, outcome: toolOutcome, result: maybe(str) },
};

/** @type {Record<string, Reads>} */
const STATE = {
  upto: int,
  lifecycle,
  closed_for: maybe(closeReason),
  agent: int,
  participants: listOf(int),
  items: listOf(tagged("kind", ITEMS)),
  speech: maybe(shaped({ generation: int, agent: int, heard: str })),
};

/** What a server says, by its `type`. */
const FROM_SERVER = {
  welcome: shaped({
    v: int,
    session: str,
    life: int,
    resumed: bool,
    resume_token: maybe(str),
    audio: maybe(audio),
    features: someOf(str),
    next_seq: int,
    grace_ms: maybe(int),
  }),
  mark: shaped({ run: int, generation: int, end: flag }),
  clear: shaped({ run: int }),
  fact: tagged("kind", FACTS, { seq: int }),
  told: tagged("kind", NEWS),
  gap: shaped({ resumes: int }),
  state: shaped(STATE),
  error: shaped({
    code: oneOf(
      "version",
      "token",
      "forbidden",
      "malformed",
      "unknown_message",
      "out_of_place",
      "unsupported",
      "busy",
      "no_session",
      "no_agent",
      "internal",
    ),
    message: str,
  }),
  bye: shaped({ reason: oneOf("ended", "replaced", "going_away", "refused") }),
};

/**
 * Takes what a server said, as the object its JSON holds.
 *
 * @param {unknown} given
 * @returns {FromServer}
 * @throws {Malformed} It is no message, or one of a kind that is known
 *   which does not say what that kind says.
 */
export function taken(given) {
  if (typeof given !== "object" || given === null || Array.isArray(given)) {
    throw new Malformed("a message is an object");
  }
  const type = str(/** @type {Record<string, unknown>} */ (given).type, "type");
  if (!Object.hasOwn(FROM_SERVER, type)) {
    return { type: "unknown" };
  }
  const reads = FROM_SERVER[/** @type {keyof typeof FROM_SERVER} */ (type)];
  const read = /** @type {Record<string, unknown>} */ (reads(given, type));
  return /** @type {FromServer} */ ({ type, ...read });
}

/**
 * Reads what a server said, as the text of one message.
 *
 * @param {string} text
 * @returns {FromServer}
 * @throws {Malformed} The text is no JSON, or no message.
 */
export function read(text) {
  /** @type {unknown} */
  let given;
  try {
    given = JSON.parse(text);
  } catch {
    throw new Malformed("a message is JSON");
  }
  return taken(given);
}

/**
 * Writes what a client says, as the text of one message.
 *
 * @param {FromClient} message
 * @returns {string}
 */
export function write(message) {
  return JSON.stringify(message);
}

/**
 * Whether `token` begins as a token of its kind does, and goes on.
 *
 * @param {unknown} token
 * @param {string} begins
 */
const looksLike = (token, begins) =>
  typeof token === "string" && token.length > begins.length && token.startsWith(begins);

/**
 * The first thing a client says.
 *
 * @param {object} [of]
 * @param {string} [of.token] The client token the session was made with.
 * @param {AudioFormat} [of.audio] How audio is written both ways. Left
 *   out, the session is one of text.
 * @param {string[]} [of.features] What the client wants to be told
 *   beyond the facts: `said`, `hearing`, `tools`.
 * @param {Resume} [of.resume] Where a client that comes back was.
 * @returns {FromClient}
 * @throws {RangeError} A token is none, or the audio is at a rate there
 *   is not.
 */
export function hello({ token, audio, features, resume } = {}) {
  /** @type {FromClient} */
  const message = { type: "hello", v: VERSION };
  if (token !== undefined) {
    if (!looksLike(token, CLIENT_TOKEN)) {
      throw new RangeError("this is no client token");
    }
    message.token = token;
  }
  if (audio !== undefined) {
    if (audio.codec !== "pcm16" && audio.codec !== "opus") {
      throw new RangeError("audio is written as pcm16 or as opus");
    }
    if (!RATES.includes(audio.rate)) {
      throw new RangeError(`audio is at one of ${RATES.join(", ")} samples a second`);
    }
    message.audio = { codec: audio.codec, rate: audio.rate };
  }
  if (features !== undefined && features.length > 0) {
    message.features = [...features];
  }
  if (resume !== undefined) {
    if (!looksLike(resume.token, RESUME_TOKEN)) {
      throw new RangeError("this is no resume token");
    }
    const { life, seen } = resume;
    if (!Number.isSafeInteger(life) || life < 0 || !Number.isSafeInteger(seen) || seen < 0) {
      throw new RangeError("a life and a fact have whole numbers");
    }
    message.resume = { token: resume.token, life, seen };
  }
  return message;
}

/**
 * How many samples of a run have been played.
 *
 * @param {number} run
 * @param {number} samples
 * @returns {FromClient}
 */
export function played(run, samples) {
  for (const count of [run, samples]) {
    if (!Number.isInteger(count) || count < 0 || count > 0xffff_ffff) {
      throw new RangeError("a run and its samples have whole numbers");
    }
  }
  return { type: "played", run, samples };
}

/**
 * Something written to the agent.
 *
 * @param {string} written
 * @returns {FromClient}
 * @throws {RangeError} Nothing is written, or more than a message holds.
 */
export function text(written) {
  if (typeof written !== "string" || written.trim() === "") {
    throw new RangeError("nothing is written");
  }
  if (new TextEncoder().encode(written).length > MOST_TEXT) {
    throw new RangeError(`a message holds ${MOST_TEXT} bytes of text at most`);
  }
  return { type: "text", text: written };
}

/**
 * Keys of a telephone that were pressed.
 *
 * @param {string} keys
 * @returns {FromClient}
 * @throws {RangeError} They are no keys, or more than a message holds.
 */
export function dtmf(keys) {
  const all = typeof keys === "string" && [...keys].every((key) => KEYS.includes(key));
  if (!all || keys.length === 0 || keys.length > MOST_KEYS) {
    throw new RangeError(`keys are 1 to ${MOST_KEYS} of ${KEYS}`);
  }
  return { type: "dtmf", keys };
}

/**
 * Stops the agent. The session goes on.
 *
 * @returns {FromClient}
 */
export function interrupt() {
  return { type: "interrupt" };
}

/**
 * Ends the session.
 *
 * @returns {FromClient}
 */
export function bye() {
  return { type: "bye" };
}

/**
 * The state of a session of which nothing is known yet.
 *
 * @returns {State}
 */
function nothingYet() {
  return { upto: 0, lifecycle: "running", agent: 0, participants: [], items: [] };
}

/**
 * What a client holds of a session: what the facts it was sent add up
 * to. It is what a `state` says, and goes on from one.
 */
export class View {
  /** @type {State} */
  #state = nothingYet();

  /** Whether facts were missed that no state has made up for yet. */
  #behind = false;

  /**
   * What the view holds, as a `state` says it. It is the view's own:
   * whoever wants to keep it copies it.
   *
   * @returns {Readonly<State>}
   */
  get state() {
    return this.#state;
  }

  /**
   * The number of the last fact the view includes: what a client that
   * comes back says it has seen.
   */
  get seen() {
    return this.#state.upto;
  }

  /**
   * Whether facts were missed that no state has made up for yet. What
   * the view holds is then older than the session.
   */
  get behind() {
    return this.#behind;
  }

  /**
   * Takes what a server said.
   *
   * @param {FromServer} message
   * @returns {"fact" | "seen" | "state" | "gap" | "nothing"} What it did
   *   to the view: a fact it did not have, one it had already, a state
   *   that it is now, a gap it is behind by, or nothing.
   */
  apply(message) {
    switch (message.type) {
      case "fact":
        if (message.seq <= this.#state.upto) {
          return "seen";
        }
        this.#state.upto = message.seq;
        this.#fact(message);
        return "fact";
      case "state": {
        const { type: _, ...state } = message;
        this.#state = structuredClone(state);
        this.#behind = false;
        return "state";
      }
      case "gap":
        this.#behind = true;
        return "gap";
      default:
        return "nothing";
    }
  }

  /**
   * Makes a fact true of the view.
   *
   * @param {Fact} fact
   */
  #fact(fact) {
    const state = this.#state;
    switch (fact.kind) {
      case "participant_joined":
        if (!state.participants.includes(fact.who)) {
          state.participants.push(fact.who);
        }
        break;
      case "participant_left":
        state.participants = state.participants.filter((known) => known !== fact.who);
        break;
      case "lifecycle_changed":
        state.lifecycle = fact.to;
        if (fact.reason !== undefined) {
          state.closed_for = fact.reason;
        }
        break;
      case "turn_committed": {
        const { turn, who, text, addressed } = fact;
        state.items.push({ kind: "turn", turn, who, text, addressed });
        break;
      }
      case "keys_committed": {
        const { turn, who, keys } = fact;
        state.items.push({ kind: "keys", turn, who, keys });
        break;
      }
      case "utterance_progress": {
        const { generation, agent, heard } = fact;
        const speech = state.speech;
        if (speech !== undefined && speech.generation === generation && speech.agent === agent) {
          speech.heard += heard;
        } else {
          state.speech = { generation, agent, heard };
        }
        break;
      }
      case "utterance_committed": {
        const { generation, agent, text, truncated } = fact;
        state.items.push({ kind: "utterance", generation, agent, text, truncated });
        if (state.speech?.generation === generation) {
          delete state.speech;
        }
        break;
      }
      case "tool_called": {
        const { call, tool, generation, args } = fact;
        state.items.push({
          kind: "tool_call",
          call,
          tool,
          generation,
          ...(args === undefined ? {} : { args }),
        });
        break;
      }
      case "tool_finished": {
        const { call, outcome, result } = fact;
        state.items.push({
          kind: "tool_result",
          call,
          outcome,
          ...(result === undefined ? {} : { result }),
        });
        break;
      }
      case "agent_activated":
        state.agent = fact.agent;
        break;
      // What happened and left nothing in what a view holds.
      case "cut_off":
      case "turn_dismissed":
      case "tool_sent":
      case "unknown":
        break;
    }
  }
}
