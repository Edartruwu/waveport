// @ts-check
// The page a daemon serves to try its agents on: it makes a session of
// an agent, and talks or writes to it with the client beside it.
//
// The client is served at `sdk/` beside this page, which is not where
// it is in the repository, so it is asked for by its address.

const sdk = /** @type {typeof import("../src/index.js")} */ (
  await import(new URL("sdk/index.js", document.baseURI).href)
);

/**
 * @typedef {import("../src/index.js").Client} Client
 * @typedef {import("../src/index.js").Voice} Voice
 * @typedef {import("../src/protocol.js").Item} Item
 * @typedef {import("../src/protocol.js").FromServer} FromServer
 */

/**
 * The element of the page with this name.
 *
 * @template {HTMLElement} [T=HTMLElement]
 * @param {string} id
 * @returns {T}
 */
function part(id) {
  const found = document.getElementById(id);
  if (found === null) {
    throw new Error(`the page has no ${id}`);
  }
  return /** @type {T} */ (found);
}

const status = part("status");
const start = /** @type {HTMLFormElement} */ (part("start"));
const agent = /** @type {HTMLSelectElement} */ (part("agent"));
const key = /** @type {HTMLInputElement} */ (part("key"));
const session = part("session");
const conversation = part("conversation");
const live = part("live");
const say = /** @type {HTMLFormElement} */ (part("say"));
const text = /** @type {HTMLInputElement} */ (part("text"));
const mute = part("mute");
const controls = part("controls");
const problem = part("problem");
const about = part("about");
const log = part("log");

/**
 * Why the page has no part in a session any more, in words.
 *
 * @type {Record<string, string>}
 */
const OVER = {
  ended: "The session has ended",
  left: "This page left the session",
  gone: "The session could not be come back to",
  refused: "The daemon turned this page away",
  replaced: "Another page took this one's place",
};

/** How many of the things the session said are kept on the page. */
const MOST_LOGGED = 200;

/**
 * What is going on, for whoever drives the page from outside it.
 *
 * @type {{ client?: Client, voice?: Voice, problem?: string, ended?: string }}
 */
const playground = {};
Object.assign(window, { playground });

/**
 * @param {"idle" | "connecting" | "open" | "away" | "closed"} state
 * @param {string} words
 */
function stands(state, words) {
  status.dataset.state = state;
  status.textContent = words;
}

/** @param {string | undefined} what */
function fails(what) {
  playground.problem = what;
  problem.hidden = what === undefined;
  problem.textContent = what ?? "";
}

/**
 * One line of a list.
 *
 * @param {string} words
 * @param {string} [note]
 */
function line(words, note) {
  const item = document.createElement("li");
  item.textContent = words;
  if (note !== undefined) {
    const small = document.createElement("small");
    small.textContent = note;
    item.append(small);
  }
  return item;
}

/**
 * One thing of a conversation, as the page shows it.
 *
 * @param {Item} item
 */
function shown(item) {
  switch (item.kind) {
    case "turn":
      return line(item.text, "You");
    case "keys":
      return line(item.keys, "You pressed");
    case "utterance":
      return line(item.text, item.truncated ? "Agent, cut off" : "Agent");
    case "tool_call":
      return line(`${item.tool}(${item.args ?? ""})`, "The agent called a tool");
    case "tool_result":
      return line(item.result ?? item.outcome, "The tool answered");
    default:
      return line("Something this page does not know of");
  }
}

/** @param {Client} client */
function drawn(client) {
  const { items, speech, lifecycle } = client.view.state;
  conversation.replaceChildren(
    ...items.map((item) => {
      const entry = shown(item);
      entry.dataset.kind = item.kind;
      return entry;
    }),
  );
  if (speech !== undefined) {
    live.textContent = `The agent is saying: ${speech.heard}`;
  } else if (live.textContent?.startsWith("The agent") === true) {
    live.textContent = "";
  }
  if (lifecycle !== "running" && client.status === "open") {
    stands("open", "The session is closing");
  }
}

/** @param {FromServer} message */
function logged(message) {
  log.append(line(JSON.stringify(message)));
  while (log.childElementCount > MOST_LOGGED) {
    log.firstElementChild?.remove();
  }
}

/**
 * What is said of the session under it.
 *
 * @param {Client} client
 * @param {Voice | undefined} voice
 */
function described(client, voice) {
  /** @type {[string, string][]} */
  const facts = [
    ["Session", client.session],
    ["Life", String(client.welcome.life)],
  ];
  const audio = client.audio;
  if (audio === undefined) {
    facts.push(["Line", "text"]);
  } else {
    facts.push(["Line", `${audio.codec} at ${audio.rate / 1000} kHz`]);
  }
  if (voice !== undefined) {
    facts.push(["Echo taken out", String(voice.settings.echoCancellation ?? "not said")]);
  }
  about.replaceChildren(
    ...facts.flatMap(([name, value]) => {
      const term = document.createElement("dt");
      term.textContent = name;
      const is = document.createElement("dd");
      is.textContent = value;
      return [term, is];
    }),
  );
}

/**
 * Makes a session of `agent` and returns the token its far end
 * connects with.
 *
 * @param {string} name
 * @returns {Promise<string>}
 */
async function made(name) {
  /** @type {Record<string, string>} */
  const headers = { "content-type": "application/json" };
  if (key.value !== "") {
    headers.authorization = `Bearer ${key.value}`;
  }
  const answer = await fetch("/v1/sessions", {
    method: "POST",
    headers,
    body: JSON.stringify({ agent: name }),
  });
  const said = await answer.json().catch(() => ({}));
  if (!answer.ok || typeof said.client_token !== "string") {
    const why = typeof said.detail === "string" ? said.detail : `status ${answer.status}`;
    throw new Error(`The daemon made no session: ${why}`);
  }
  return said.client_token;
}

/** The session is over, or never began: the page is as it was. */
function over() {
  playground.client = undefined;
  playground.voice?.stop();
  playground.voice = undefined;
  start.hidden = false;
  for (const control of start.elements) {
    /** @type {HTMLButtonElement} */ (control).disabled = false;
  }
  mute.hidden = true;
  // What was said stays to be read. There is nobody to say more to.
  controls.hidden = true;
  live.textContent = "";
}

/**
 * Begins a session, spoken or written.
 *
 * @param {boolean} spoken
 */
async function begins(spoken) {
  fails(undefined);
  playground.ended = undefined;
  for (const control of start.elements) {
    /** @type {HTMLButtonElement} */ (control).disabled = true;
  }
  stands("connecting", "Connecting");
  try {
    // `?mic=raw` asks for the microphone as it is: for a page that is
    // driven by a recording, and for someone with headphones on.
    const raw = new URLSearchParams(location.search).get("mic") === "raw";
    const processing = { echoCancellation: !raw, noiseSuppression: !raw, autoGainControl: !raw };
    const { client, voice } = await sdk.call(location.origin, {
      // The session is made once the microphone is there: its token is
      // good for a minute.
      token: () => made(agent.value),
      features: ["said", "hearing", "tools"],
      voice: spoken ? { constraints: processing, onerror: (error) => fails(error.message) } : false,
    });
    playground.client = client;
    playground.voice = voice;
    start.hidden = true;
    session.hidden = false;
    controls.hidden = false;
    mute.hidden = voice === undefined;
    mute.setAttribute("aria-pressed", "false");
    log.replaceChildren();
    conversation.replaceChildren();
    stands("open", spoken ? "On the line, listening" : "On the line");
    described(client, voice);
    client.on("message", logged);
    client.on("view", () => drawn(client));
    client.on("told", (news) => {
      if (news.kind === "hearing") {
        live.textContent = `Hearing: ${news.text}`;
      } else if (news.kind === "user_speaking" && !news.speaking) {
        live.textContent = "";
      }
    });
    client.on("error", (error) => fails(error.message));
    client.on("away", () => stands("away", "The line dropped. Coming back"));
    client.on("back", () => {
      stands("open", "On the line again");
      described(client, voice);
    });
    client.on("closed", ({ reason, error }) => {
      playground.ended = reason;
      stands("closed", OVER[reason] ?? "The session is over");
      if (error !== undefined) {
        fails(error.message);
      }
      over();
    });
  } catch (error) {
    fails(error instanceof Error ? error.message : String(error));
    stands("idle", "Not in a session");
    over();
  }
}

start.addEventListener("submit", (event) => {
  event.preventDefault();
  const pressed = /** @type {HTMLButtonElement | null} */ (event.submitter);
  void begins(pressed?.value !== "write");
});

say.addEventListener("submit", (event) => {
  event.preventDefault();
  const written = text.value;
  if (written.trim() === "" || playground.client === undefined) {
    return;
  }
  try {
    playground.client.text(written);
    text.value = "";
    fails(undefined);
  } catch (error) {
    fails(error instanceof Error ? error.message : String(error));
  }
});

mute.addEventListener("click", () => {
  const voice = playground.voice;
  if (voice !== undefined) {
    voice.muted = !voice.muted;
    mute.setAttribute("aria-pressed", String(voice.muted));
  }
});

part("interrupt").addEventListener("click", () => playground.client?.interrupt());
part("bye").addEventListener("click", () => playground.client?.bye());

// What the daemon has to try: its agents, and whether it wants a key.
try {
  const answer = await fetch("config.json");
  const config = await answer.json();
  for (const name of /** @type {string[]} */ (config.agents)) {
    agent.append(new Option(name, name));
  }
  part("keyed").hidden = config.key !== true;
  key.required = config.key === true;
} catch {
  fails("The daemon did not say what agents it has.");
}
