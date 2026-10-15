import assert from "node:assert/strict";
import test from "node:test";
import { initialPractice, practiceAction } from "../src/dashboard/web/landing-learn.mjs";

test("practice requires a sent request before its reply can be read", () => {
  let state = initialPractice();
  assert.equal(practiceAction(state, "inbox"), state);
  for (const action of ["watch", "attend", "answer", "review", "inbox"]) state = practiceAction(state, action);
  assert.equal(state.step, 5);
  assert.equal(state.mailbox, true);
  assert.deepEqual(practiceAction(state, "reset"), initialPractice());
});

test("attend chooses the waiting agent regardless of current selection", () => {
  let state = practiceAction(initialPractice(), "watch");
  state = practiceAction(state, "gemini");
  state = practiceAction(state, "attend");
  assert.equal(state.selected, "claude");
  assert.equal(state.step, 2);
});

test("answer cannot be sent from a different agent's pane", () => {
  let state = practiceAction(practiceAction(initialPractice(), "watch"), "attend");
  state = practiceAction(state, "gemini");
  assert.equal(practiceAction(state, "answer"), state);
  state = practiceAction(state, "claude");
  assert.equal(practiceAction(state, "answer").step, 3);
});

test("duplicate and out-of-order actions cannot advance the tour", () => {
  const state = practiceAction(initialPractice(), "watch");
  assert.equal(practiceAction(state, "watch"), state);
  assert.equal(practiceAction(state, "review"), state);
  assert.equal(practiceAction(state, "unknown"), state);
});

test("tmux prefix waits for the second key and can be cancelled", () => {
  const initial = initialPractice();
  assert.equal(practiceAction(initial, "prefix-watch"), initial);
  const armed = practiceAction(initial, "prefix");
  assert.equal(armed.prefix, true);
  assert.equal(armed.step, 0);
  const cancelled = practiceAction(armed, "cancel-prefix");
  assert.equal(cancelled.prefix, false);
  assert.equal(cancelled.step, 0);
  const opened = practiceAction(armed, "prefix-watch");
  assert.equal(opened.prefix, false);
  assert.equal(opened.step, 1);
});
