import assert from "node:assert/strict";
import test from "node:test";

import {
  composerKeyAction,
  isNearBottom,
  parsePaneHash,
  updatedLabel,
  withPaneHash,
} from "../src/dashboard/web/pane-drawer.mjs";

test("Enter sends, Shift+Enter is a newline", () => {
  assert.equal(composerKeyAction({ key: "Enter" }), "send");
  assert.equal(composerKeyAction({ key: "Enter", shiftKey: true }), "default");
  assert.equal(composerKeyAction({ key: "Enter", ctrlKey: true }), "send");
  assert.equal(composerKeyAction({ key: "Enter", metaKey: true }), "send");
  assert.equal(composerKeyAction({ key: "a" }), "default");
  assert.equal(composerKeyAction(null), "default");
});

test("Enter that confirms an IME candidate never sends", () => {
  assert.equal(composerKeyAction({ key: "Enter", isComposing: true }), "default");
  // Safari/Chrome report the confirming keydown as keyCode 229.
  assert.equal(composerKeyAction({ key: "Enter", keyCode: 229 }), "default");
});

test("pane deep links round-trip and keep other fragment params", () => {
  assert.deepEqual(parsePaneHash("#pane=%255&socket=default"), { pane: "%5", socket: "default" });
  assert.deepEqual(parsePaneHash("#pane=rmux%3A%251"), { pane: "rmux:%1", socket: null });
  assert.equal(parsePaneHash(""), null);
  assert.equal(parsePaneHash("#token=abc"), null);

  const hash = withPaneHash("#other=1", { pane: "%5", socket: "work" });
  assert.deepEqual(parsePaneHash(hash), { pane: "%5", socket: "work" });
  assert.match(hash, /other=1/);
  assert.equal(withPaneHash(hash, null), "#other=1");
  assert.equal(withPaneHash("#pane=%255", null), "");
});

test("output stays pinned only near the bottom", () => {
  assert.equal(isNearBottom({ scrollTop: 480, scrollHeight: 1000, clientHeight: 500 }), true);
  assert.equal(isNearBottom({ scrollTop: 100, scrollHeight: 1000, clientHeight: 500 }), false);
});

test("updated label counts seconds then minutes", () => {
  assert.equal(updatedLabel(0, 5000), "");
  assert.equal(updatedLabel(1000, 4500), "updated 3s ago");
  assert.equal(updatedLabel(1000, 1000 + 125_000), "updated 2m ago");
});
