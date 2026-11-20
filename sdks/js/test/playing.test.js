// @ts-check
// What a call has played, from what its browser says of it, and what
// is kept from being heard: `Playing`, with the time and the browser in
// the test's hands.

import assert from "node:assert/strict";
import { test } from "node:test";

import { Playing, playedBy } from "../src/webrtc.js";

/**
 * What a browser says when it gave the packet at `rtpTimestamp` to be
 * played at `timestamp`.
 *
 * @param {number} rtpTimestamp
 * @param {number} timestamp
 */
const source = (rtpTimestamp, timestamp) => ({ rtpTimestamp, timestamp });

test("a place in a call is the packet the browser plays, and how far into it", () => {
  const playing = new Playing();
  // A browser that has played nothing says nothing.
  assert.equal(playing.at(1_000, undefined), undefined);
  // The packet at 48,000 was given to be played just now: the call is
  // at its beginning.
  assert.equal(playing.at(1_000, source(48_000, 1_000)), 48_000);
  // Ten milliseconds later it is half way through it, and after twenty
  // at its end, and no further for as long as nothing follows.
  assert.equal(playing.at(1_010, source(48_000, 1_000)), 48_480);
  assert.equal(playing.at(1_020, source(48_000, 1_000)), 48_960);
  assert.equal(playing.at(1_500, source(48_000, 1_000)), undefined);
  // The same place is said once, and none before it is said after it.
  assert.equal(playing.at(1_510, source(48_000, 1_505)), undefined);
  assert.equal(playing.at(1_520, source(96_000, 1_520)), 96_000);
  assert.equal(playing.at(1_521, source(48_960, 1_521)), undefined);
});

test("a browser whose clock is another one says where a packet begins and no more", () => {
  const playing = new Playing();
  // When the packet was given is said on a clock that is far from this
  // one, either way: how far into the packet the call is cannot be
  // told.
  assert.equal(playing.at(1_000, source(48_000, 1_700_000_000_000)), 48_000);
  assert.equal(playing.at(1_015, source(48_960, -5_000)), 48_960);
});

test("the time of a stream comes round, and a place after that is after", () => {
  const playing = new Playing();
  assert.equal(playing.at(0, source(0xffff_fe00, 0)), 0xffff_fe00);
  // 960 samples on, the time has come round to nought and gone on.
  assert.equal(playing.at(20, source(0xffff_fe00, 0)), 448);
  assert.equal(playing.at(40, source(448, 20)), 1_408);
  assert.equal(playing.at(41, source(0xffff_fe00, 41)), undefined);
});

test("a response a browser stops just short of was played to its end", () => {
  const playing = new Playing();
  playing.marked({ run: 3, ts: 10_000 });
  assert.equal(playing.at(100, source(10_000, 100)), 10_000);
  // It was sent whole: it ends at 14,800, five packets on.
  playing.marked({ run: 3, end: true, ts: 14_800 });
  // The browser says the third packet and no more, as one browser
  // does: it is 40 ms short, and for a while it may only be late.
  assert.equal(playing.at(140, source(11_920, 140)), 11_920);
  assert.equal(playing.at(160, source(11_920, 140)), 12_880);
  assert.equal(playing.at(200, source(11_920, 140)), undefined);
  assert.equal(playing.at(259, source(11_920, 140)), undefined);
  // It has said nothing new for 120 ms: nothing more is coming, and
  // what it held was played.
  assert.equal(playing.at(260, source(11_920, 140)), 14_800);
  assert.equal(playing.at(280, source(11_920, 140)), undefined);
});

test("a response a browser stops far short of was not played to its end", () => {
  const playing = new Playing();
  playing.marked({ run: 3, ts: 10_000 });
  playing.marked({ run: 3, end: true, ts: 58_000 });
  // A second of it, of which the browser played 20 ms and then
  // nothing: packets were lost, or are late.
  assert.equal(playing.at(100, source(10_000, 100)), 10_000);
  assert.equal(playing.at(120, source(10_000, 100)), 10_960);
  assert.equal(playing.at(1_000, source(10_000, 100)), undefined);
  // And one whose end is not known is not taken to have ended.
  const other = new Playing();
  other.marked({ run: 4, ts: 10_000 });
  assert.equal(other.at(100, source(10_000, 100)), 10_000);
  assert.equal(other.at(120, source(10_000, 100)), 10_960);
  assert.equal(other.at(1_000, source(10_000, 100)), undefined);
});

test("what a browser holds of a response that is discarded is not heard, and not said to have been played", () => {
  const playing = new Playing();
  playing.marked({ run: 1, ts: 10_000 });
  assert.equal(playing.at(100, source(10_000, 100)), 10_000);
  assert.equal(playing.silenced, false);
  playing.cleared(1, 110);
  assert.equal(playing.silenced, true);
  // The browser goes on with what it held: three more packets.
  assert.equal(playing.at(120, source(10_960, 120)), undefined);
  assert.equal(playing.at(140, source(11_920, 140)), undefined);
  assert.equal(playing.at(160, source(12_880, 160)), undefined);
  assert.equal(playing.silenced, true);
  // Then what it makes up, which fades: a quarter of a second after
  // the last packet that was real, it is heard again.
  assert.equal(playing.at(409, source(12_880, 160)), undefined);
  assert.equal(playing.silenced, true);
  playing.at(410, source(12_880, 160));
  assert.equal(playing.silenced, false);
});

test("a response that follows one that is discarded is heard from its first sample", () => {
  const playing = new Playing();
  playing.marked({ run: 1, ts: 10_000 });
  assert.equal(playing.at(100, source(10_000, 100)), 10_000);
  playing.cleared(1, 110);
  // The next response begins at once, while the browser still holds
  // some of the one before.
  playing.marked({ run: 2, ts: 30_000 });
  assert.equal(playing.at(120, source(10_960, 120)), undefined);
  assert.equal(playing.at(140, source(11_920, 140)), undefined);
  assert.equal(playing.silenced, true);
  // The browser is at the response that follows: it is heard, and
  // said.
  assert.equal(playing.at(160, source(30_000, 160)), 30_000);
  assert.equal(playing.silenced, false);
  assert.equal(playing.at(170, source(30_000, 160)), 30_480);
});

test("a mark of the response that is discarded does not end the silence", () => {
  const playing = new Playing();
  playing.marked({ run: 4, ts: 10_000 });
  assert.equal(playing.at(100, source(10_000, 100)), 10_000);
  playing.cleared(4, 110);
  // Its own marks, and those of one before it, which were on their
  // way.
  playing.marked({ run: 4, ts: 10_000 });
  playing.marked({ run: 3, ts: 5_000 });
  assert.equal(playing.at(120, source(10_960, 120)), undefined);
  assert.equal(playing.silenced, true);
  // What ended before it was discarded has no end to stop short of.
  playing.marked({ run: 4, end: true, ts: 12_000 });
  playing.cleared(4, 121);
  assert.equal(playing.at(500, source(10_960, 120)), undefined);
  assert.equal(playing.silenced, false);
  assert.equal(playing.at(700, source(10_960, 120)), undefined);
});

test("a browser that cannot say where it is holds what it was told to discard for a while", () => {
  const playing = new Playing();
  playing.cleared(1, 1_000);
  assert.equal(playing.at(1_100, undefined), undefined);
  assert.equal(playing.silenced, true);
  assert.equal(playing.at(1_399, undefined), undefined);
  assert.equal(playing.silenced, true);
  assert.equal(playing.at(1_400, undefined), undefined);
  assert.equal(playing.silenced, false);
});

test("what a browser's statistics say of a call's audio is read where it is said", () => {
  // Chrome says both, Firefox the first, and neither says more of
  // video or of what is sent than concerns this.
  assert.deepEqual(
    playedBy([
      { type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 96_000 },
      { type: "inbound-rtp", kind: "video", jitterBufferEmittedCount: 7 },
      { type: "outbound-rtp", kind: "audio" },
      { type: "media-playout", totalPlayoutDelay: 3_638.4, totalSamplesCount: 96_000 },
    ]),
    { emitted: 96_000, delayS: 3_638.4, samples: 96_000 },
  );
  assert.deepEqual(playedBy([{ type: "inbound-rtp", kind: "audio", jitterBufferEmittedCount: 480 }]), {
    emitted: 480,
  });
  assert.deepEqual(playedBy([{ type: "inbound-rtp", kind: "audio" }, { type: "media-playout" }, undefined]), {});
});
