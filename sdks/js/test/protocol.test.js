// @ts-check
// The protocol as this client reads and writes it, held to the fixtures
// the Rust crate that defines it is held to.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  bye,
  dtmf,
  hello,
  interrupt,
  Malformed,
  MOST_TEXT,
  played,
  read,
  taken,
  text,
  View,
  write,
} from "../src/protocol.js";

const FIXTURES = new URL(
  "../../../crates/core/waveport-protocol/tests/fixtures/client-v1/",
  import.meta.url,
);

/**
 * The lines of a fixture, each as the object its JSON holds.
 *
 * @param {string} name
 * @returns {any[]}
 */
function lines(name) {
  return readFileSync(new URL(name, FIXTURES), "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line));
}

test("what a server says is read as it was said", () => {
  const said = lines("from_server.jsonl");
  assert.ok(said.length >= 30);
  for (const message of said) {
    assert.deepEqual(taken(message), message);
    assert.deepEqual(read(JSON.stringify(message)), message);
  }
  // Every kind of message a server has is among them.
  const types = new Set(said.map((message) => message.type));
  assert.deepEqual(
    [...types].sort(),
    ["bye", "clear", "error", "fact", "gap", "mark", "state", "told", "welcome"],
  );
});

test("what a server says that this client does not know is left out or unknown", () => {
  const cases = lines("tolerated.jsonl").filter((line) => line.from === "server");
  assert.ok(cases.length >= 10);
  for (const { given, is } of cases) {
    assert.deepEqual(taken(given), is, JSON.stringify(given));
  }
});

test("what is no message is not taken for one", () => {
  for (const none of ["", "{", "[]", "null", "7", '"hello"', "{}", '{"type":7}']) {
    assert.throws(() => read(none), Malformed, none);
  }
  // A message of a kind that is known says what that kind says.
  for (const wrong of [
    { type: "fact", kind: "cut_off", cause: "barge_in" },
    { type: "fact", seq: -1, kind: "cut_off", cause: "barge_in" },
    { type: "fact", seq: 1.5, kind: "cut_off", cause: "barge_in" },
    { type: "fact", seq: 1, kind: "turn_committed", turn: 1, who: 1, text: 7, addressed: true },
    { type: "mark", run: "3", generation: 5 },
    { type: "welcome", v: 1, session: "2a", life: 1, resumed: false, next_seq: 1, audio: { codec: "mp3", rate: 48000 } },
    { type: "state", upto: 1, lifecycle: "running", agent: 0, participants: {}, items: [] },
    { type: "bye" },
  ]) {
    assert.throws(() => taken(wrong), Malformed, JSON.stringify(wrong));
  }
  // What was sent is not repeated in what is said of it.
  try {
    taken({ type: "error", code: "busy", message: { secret: "wpr1_madeup" } });
    assert.fail("it was taken");
  } catch (error) {
    assert.ok(error instanceof Malformed);
    assert.ok(!error.message.includes("wpr1_madeup"), error.message);
  }
});

test("a client says what the fixtures say a client says", () => {
  const said = lines("from_client.jsonl");
  let written = 0;
  for (const message of said) {
    /** @type {import("../src/protocol.js").FromClient | undefined} */
    let made;
    switch (message.type) {
      case "hello": {
        const { token, audio, features, resume } = message;
        made = hello({ token, audio, features, resume });
        break;
      }
      case "played":
        made = played(message.run, message.samples);
        break;
      case "text":
        made = text(message.text);
        break;
      case "dtmf":
        made = dtmf(message.keys);
        break;
      case "interrupt":
        made = interrupt();
        break;
      case "bye":
        made = bye();
        break;
      // How a WebRTC call says what it played, which is no part of a
      // connection over a WebSocket.
      case "played_rtp":
        continue;
      default:
        assert.fail(`a client says something this client cannot: ${message.type}`);
    }
    assert.deepEqual(JSON.parse(write(made)), message);
    written += 1;
  }
  assert.equal(written, said.length - 2);
});

test("a client does not say what a server would refuse", () => {
  let refused = 0;
  for (const { given, why } of lines("refused.jsonl")) {
    switch (why) {
      // Another version, and a message of no kind, cannot be written
      // with what is here at all.
      case "version":
      case "unknown":
        continue;
      case "rate":
      case "token": {
        const { token, audio, resume } = given;
        assert.throws(() => hello({ token, audio, resume }), RangeError, JSON.stringify(given));
        break;
      }
      case "text":
        assert.throws(() => text(given.text), RangeError, JSON.stringify(given));
        break;
      case "keys":
        assert.throws(() => dtmf(given.keys), RangeError, JSON.stringify(given));
        break;
      default:
        assert.fail(`refused for something this test does not know: ${why}`);
    }
    refused += 1;
  }
  assert.equal(refused, 10);
  // Text is counted in bytes, as the server counts it.
  assert.equal(write(text("é".repeat(MOST_TEXT / 2))).length > MOST_TEXT / 2, true);
  assert.throws(() => text("é".repeat(MOST_TEXT / 2 + 1)), RangeError);
  assert.throws(() => played(1, -1), RangeError);
  assert.throws(() => played(1.5, 1), RangeError);
  assert.throws(() => played(1, 2 ** 32), RangeError);
  // A token says what it is by how it begins: one that connects is
  // not one that comes back, and neither is anything else.
  assert.throws(() => hello({ token: "nothing" }), RangeError);
  assert.throws(() => hello({ token: "wpr1_madeup" }), RangeError);
  assert.throws(() => hello({ resume: { token: "wpc1_madeup", life: 1, seen: 0 } }), RangeError);
});

test("a view is what the facts add up to", () => {
  const cases = JSON.parse(readFileSync(new URL("views.json", FIXTURES), "utf8"));
  assert.ok(cases.length >= 7);
  for (const { name, messages, view: expected, behind } of cases) {
    const view = new View();
    for (const message of messages) {
      view.apply(taken(message));
    }
    assert.deepEqual(view.state, expected, name);
    assert.equal(view.behind, behind, name);
    assert.equal(view.seen, expected.upto, name);
  }
});

test("a view says what each message did to it", () => {
  const view = new View();
  const joined = taken({ type: "fact", seq: 3, kind: "participant_joined", who: 1 });
  assert.equal(view.apply(joined), "fact");
  assert.equal(view.apply(joined), "seen");
  assert.equal(view.apply(taken({ type: "gap", resumes: 9 })), "gap");
  assert.equal(view.behind, true);
  const state = taken({
    type: "state",
    upto: 8,
    lifecycle: "running",
    agent: 0,
    participants: [1],
    items: [],
  });
  assert.equal(view.apply(state), "state");
  assert.equal(view.behind, false);
  assert.equal(view.seen, 8);
  assert.equal(view.apply(taken({ type: "told", kind: "hearing", who: 1, text: "wh" })), "nothing");
  // The state a view was given is not the view's to change.
  view.apply(taken({ type: "fact", seq: 9, kind: "participant_joined", who: 2 }));
  assert.deepEqual(/** @type {any} */ (state).participants, [1]);
  assert.deepEqual(view.state.participants, [1, 2]);
});
