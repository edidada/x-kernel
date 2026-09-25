import test from "node:test";
import assert from "node:assert/strict";
import { terminalRecordings } from "../src/features/terminal-player/model/recordings";
import {
  createTimeline,
  frameAt,
  type TerminalFrame,
} from "../src/features/terminal-player/lib/playback";

const textOf = (frame: TerminalFrame) =>
  frame.tokens.map((token) => token.text).join("");

test("typing preserves the source scheduling, including the post-typing delay", () => {
  const timeline = createTimeline([
    { t: 100, type: "prompt", text: "$ " },
    { t: 200, type: "type", text: "abc" },
    { t: 10, type: "enter" },
  ]);
  assert.equal(timeline.duration, 600);
  assert.equal(textOf(frameAt(timeline, 299)), "$ ");
  assert.equal(textOf(frameAt(timeline, 300)), "$ a");
  assert.equal(textOf(frameAt(timeline, 345)), "$ ab");
  assert.equal(textOf(frameAt(timeline, 390)), "$ abc");
  assert.equal(textOf(frameAt(timeline, 600)), "$ abc\n");
});

test("spinner replaces only the last line and preserves colored earlier output", () => {
  const timeline = createTimeline([
    { type: "output", text: "Keep\nold spinner", cls: "tp-bold" },
    { t: 10, type: "replace-line", text: "new spinner" },
    { t: 10, type: "replace-line", text: "" },
    { t: 10, type: "output", text: "Ready\n" },
  ]);
  assert.equal(textOf(frameAt(timeline, 10)), "Keep\nnew spinner");
  assert.equal(textOf(frameAt(timeline, 20)), "Keep\n");
  assert.deepEqual(frameAt(timeline, 30).tokens[0], {
    text: "Keep\n",
    className: "tp-bold",
  });
  assert.equal(textOf(frameAt(timeline, 30)), "Keep\nReady\n");
});

test("all X-kernel demos finish with their expected output", () => {
  const expected = [
    "Welcome to Kylin X!",
    "make rootfs",
    "Symbol table updated; relinking kernel",
    "Hello, X-kernel!",
  ];
  assert.equal(terminalRecordings.length, expected.length);
  terminalRecordings.forEach((recording, index) => {
    const timeline = createTimeline(recording.events);
    assert.ok(timeline.duration > 0);
    const final = frameAt(timeline, timeline.duration);
    assert.ok(textOf(final).includes(expected[index] ?? ""));
    assert.equal(final.cursor, true);
    // Seeking backwards is independent of any preceding rendered frame.
    assert.deepEqual(frameAt(timeline, 0), { tokens: [], cursor: false });
  });
});

test("wait delays the next display action and clear removes text and cursor", () => {
  const timeline = createTimeline([
    { type: "prompt", text: "$ " },
    { type: "wait", ms: 250 },
    { type: "clear" },
    { type: "output", t: 100, text: "Fresh screen" },
  ]);

  assert.equal(timeline.duration, 350);
  assert.equal(frameAt(timeline, 249).cursor, true);
  assert.equal(textOf(frameAt(timeline, 249)), "$ ");
  assert.deepEqual(frameAt(timeline, 250), { tokens: [], cursor: false });
  assert.equal(textOf(frameAt(timeline, 350)), "Fresh screen");
});
