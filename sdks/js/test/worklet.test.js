// @ts-check
// What is played and what is said to have been played, sample by sample.

import assert from "node:assert/strict";
import { test } from "node:test";

import { Framer, Playout } from "../src/worklet.js";

/** How many frames the output asks for at a time. */
const BLOCK = 128;

/**
 * `count` samples that are each their own number, from `from` on, so
 * that what was played where can be told.
 *
 * @param {number} from
 * @param {number} count
 */
function ramp(from, count) {
  return Float32Array.from({ length: count }, (_, n) => from + n);
}

/**
 * Has `playout` play `blocks` blocks from the frame `frame`. Returns
 * what sounded, what was said of it, and the frame after.
 *
 * @param {Playout} playout
 * @param {number} frame
 * @param {number} blocks
 */
function plays(playout, frame, blocks) {
  /** @type {number[]} */
  const sounded = [];
  /** @type {(import("../src/worklet.js").Played & { at: number })[]} */
  const said = [];
  for (let n = 0; n < blocks; n += 1) {
    const out = new Float32Array(BLOCK).fill(-1);
    playout.render(out, frame);
    frame += BLOCK;
    sounded.push(...out);
    for (const played of playout.played(frame)) {
      said.push({ ...played, at: frame });
    }
  }
  return { sounded, said, frame };
}

test("a run is played to its end, and said to have been once it has", () => {
  const playout = new Playout();
  playout.begin(1);
  playout.push(1, ramp(1, 1000));
  playout.push(1, ramp(1001, 1400));
  playout.end(1);
  const { sounded, said } = plays(playout, 0, 24);
  assert.deepEqual(sounded.slice(0, 2400), [...ramp(1, 2400)]);
  // After it there is silence, and not what the output held before.
  assert.ok(sounded.slice(2400).every((sample) => sample === 0));
  // 2400 samples are over in the nineteenth block.
  assert.deepEqual(said, [{ run: 1, samples: 2400, last: true, at: 19 * BLOCK }]);
});

test("what is given to the output is played when it has had time to sound", () => {
  const playout = new Playout();
  playout.latency = 960;
  playout.begin(1);
  playout.push(1, ramp(1, 2400));
  playout.end(1);
  const { said } = plays(playout, 0, 40);
  // The last sample is given at frame 2400 and sounds 960 frames later.
  assert.deepEqual(said, [{ run: 1, samples: 2400, last: true, at: 27 * BLOCK }]);
  assert.ok(26 * BLOCK < 3360 && 3360 <= 27 * BLOCK);
});

test("what was played of a run is said a few times a second, and never more than was", () => {
  const playout = new Playout();
  playout.latency = 480;
  playout.begin(7);
  playout.push(7, ramp(1, 48_000));
  playout.end(7);
  const { said } = plays(playout, 0, 400);
  assert.equal(said.at(-1)?.last, true);
  assert.equal(said.at(-1)?.samples, 48_000);
  const during = said.slice(0, -1);
  // Ten times in the second it lasts, give or take a block.
  assert.ok(during.length >= 9 && during.length <= 10, `${during.length}`);
  let before = 0;
  for (const { run, samples, last, at } of during) {
    assert.equal(run, 7);
    assert.equal(last, false);
    assert.ok(samples > before, "what was played does not go back");
    // What was given by then, less what has not sounded yet.
    assert.ok(samples <= at - 480, `${samples} at ${at}`);
    assert.ok(samples > at - 480 - BLOCK, `${samples} at ${at}`);
    before = samples;
  }
});

test("where there is nothing to play there is silence, and silence is not played", () => {
  const playout = new Playout();
  playout.begin(1);
  playout.push(1, ramp(1, 300));
  const first = plays(playout, 0, 5);
  assert.deepEqual(first.sounded.slice(0, 300), [...ramp(1, 300)]);
  assert.ok(first.sounded.slice(300).every((sample) => sample === 0));
  // The run is not over: more of it comes, late.
  playout.push(1, ramp(301, 300));
  playout.end(1);
  const second = plays(playout, first.frame, 5);
  assert.deepEqual(second.sounded.slice(0, 300), [...ramp(301, 300)]);
  assert.deepEqual(second.said, [{ run: 1, samples: 600, last: true, at: first.frame + 3 * BLOCK }]);
});

test("what is discarded is not played, and what was played by then is said at once", () => {
  const playout = new Playout();
  playout.latency = 960;
  playout.begin(1);
  playout.push(1, ramp(1, 4800));
  const before = plays(playout, 0, 10);
  assert.deepEqual(before.said, []);
  // Ten blocks were given to the output, which sounds what it is given.
  assert.deepEqual(playout.clear(1), { run: 1, samples: 10 * BLOCK, last: true });
  // Nothing more of the run sounds, nothing more is said of it, and
  // what comes of it late is not played.
  playout.push(1, ramp(9000, 500));
  playout.end(1);
  const after = plays(playout, before.frame, 60);
  assert.ok(after.sounded.every((sample) => sample === 0));
  assert.deepEqual(after.said, []);
  assert.equal(playout.clear(1), undefined);
});

test("runs are played in the order they began", () => {
  const playout = new Playout();
  playout.begin(1);
  playout.push(1, ramp(1, 200));
  // The next run begins before the first has been sent whole.
  playout.begin(2);
  playout.push(2, ramp(1001, 200));
  const first = plays(playout, 0, 3);
  assert.deepEqual(first.sounded.slice(0, 200), [...ramp(1, 200)]);
  assert.ok(first.sounded.slice(200).every((sample) => sample === 0));
  assert.deepEqual(first.said, []);
  // Once the first is whole, the second follows it without a gap.
  playout.push(1, ramp(201, 28));
  playout.end(1);
  playout.end(2);
  const second = plays(playout, first.frame, 3);
  assert.deepEqual(second.sounded.slice(0, 228), [...ramp(201, 28), ...ramp(1001, 200)]);
  assert.deepEqual(second.said, [
    { run: 1, samples: 228, last: true, at: first.frame + BLOCK },
    { run: 2, samples: 200, last: true, at: first.frame + 2 * BLOCK },
  ]);
});

test("audio of a run that did not begin is not played", () => {
  const playout = new Playout();
  playout.push(3, ramp(1, 500));
  const { sounded, said } = plays(playout, 0, 5);
  assert.ok(sounded.every((sample) => sample === 0));
  assert.deepEqual(said, []);
  // A run that begins twice is one run.
  playout.begin(3);
  playout.push(3, ramp(1, 100));
  playout.begin(3);
  playout.end(3);
  assert.deepEqual(plays(playout, 640, 2).said, [{ run: 3, samples: 100, last: true, at: 768 }]);
});

test("what a microphone hears is cut into frames of one length", () => {
  const framer = new Framer(960);
  /** @type {Float32Array[]} */
  const frames = [];
  let sent = 0;
  // Pieces of the length an audio thread gives, and some that are not.
  for (const length of [128, 128, 128, 128, 128, 128, 128, 128, 1, 959, 2000, 37]) {
    frames.push(...framer.push(ramp(sent + 1, length)));
    sent += length;
  }
  assert.equal(frames.length, Math.floor(sent / 960));
  frames.forEach((frame, n) => {
    assert.deepEqual([...frame], [...ramp(n * 960 + 1, 960)]);
  });
  // Each frame is a buffer of its own, which can be given away.
  assert.notEqual(frames[0]?.buffer, frames[1]?.buffer);
});
