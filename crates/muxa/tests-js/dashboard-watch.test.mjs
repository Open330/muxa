import assert from "node:assert/strict";
import test from "node:test";
import { adjacentPane, watchFrame, textWidth, watchPaneOrder } from "../src/dashboard/web/landing-watch.mjs";

const details = {
  shell: { state: "shell", summary: "zsh", capture: ["demo@local:~/acme$ echo keep me", "keep me"] },
  claude: { state: "waiting_input", glyph: "▶", age: "4m", summary: "Choose retry strategy", capture: ["How should we retry?"] },
  codex: { state: "idle", glyph: "○", age: "18s", summary: "Ready for review", capture: ["OpenAI Codex"] },
  gemini: { state: "working", glyph: "●", age: "2s", summary: "Running tests", capture: ["npm test"] },
};

test("pane navigation follows the 2x2 layout without wrapping across edges", () => {
  assert.equal(adjacentPane("shell", "right"), "claude");
  assert.equal(adjacentPane("claude", "down"), "gemini");
  assert.equal(adjacentPane("gemini", "left"), "codex");
  assert.equal(adjacentPane("codex", "up"), "shell");
  assert.equal(adjacentPane("shell", "left"), "shell");
});

test("watch sorts by state while pane geometry stays unchanged", () => {
  assert.deepEqual(watchPaneOrder(details), ["claude", "gemini", "codex", "shell"]);
  assert.equal(adjacentPane("shell", "right"), "claude");
});

test("watch uses the real topology vocabulary and the 120-column inspector threshold", () => {
  const narrow = watchFrame({ cols: 119, rows: 28, selected: "claude", details });
  assert.equal(narrow.inspector, false);
  const wide = watchFrame({ cols: 150, rows: 28, selected: "claude", details });
  assert.equal(wide.inspector, true);
  const screen = wide.lines.join("\n");
  for (const label of ["Topology", "SESSION / WINDOW / PANE", "Inspector · pane %12", "acme", "retry", "%13", "Enter jump"]) assert.ok(screen.includes(label), label);
  assert.ok(screen.includes("How should we retry?"));
});

test("filtering keeps ancestry and omits unrelated pane rows", () => {
  const screen = watchFrame({ cols: 100, rows: 28, filter: "codex", filterOpen: true, details }).lines.join("\n");
  assert.ok(screen.includes("filter: codex"));
  assert.ok(screen.includes("acme") && screen.includes("retry"));
  assert.ok(screen.includes("%13"));
  assert.ok(!screen.includes("%12"));
});

test("every screen fits its terminal including CJK content and a composer cursor", () => {
  const request = { body: "재시도 구현을 리뷰해주세요", target: "codex", reply: "문제 두 개를 발견했습니다" };
  for (const cols of [27, 80, 120, 150]) for (const view of ["watch", "preview", "help", "inbox", "compose"]) {
    const frame = watchFrame({ cols, rows: 28, selected: "codex", details, view, request, tab: "sent", input: "리뷰해주세요", cursor: 6 });
    assert.equal(frame.lines.length, 28);
    for (const line of frame.lines) assert.ok(textWidth(line) <= cols, `${view} at ${cols}: ${line}`);
    if (view === "compose") assert.ok(frame.cursor.col < cols);
  }
});

test("session and window navigation uses separate levels and local pane geometry", async () => {
  const { navigateTopology, topologyRows, paneWindow, DEMO_PANES } = await import("../src/dashboard/web/landing-watch.mjs");
  assert.equal(DEMO_PANES.length, 9);
  assert.equal(navigateTopology("claude", "left", details), "window:acme/retry");
  assert.equal(navigateTopology("window:acme/retry", "left", details), "session:acme");
  assert.equal(navigateTopology("session:acme", "down", details), "session:ops");
  assert.equal(navigateTopology("session:ops", "right", details), "window:ops/build");
  assert.equal(adjacentPane("build-shell", "right"), "build-codex");
  assert.equal(adjacentPane("build-codex", "down"), "build-codex");
  assert.equal(adjacentPane("logs-shell", "right"), "logs-shell");
  assert.equal(paneWindow("docs-claude").name, "docs");
  assert.deepEqual(topologyRows(details, "shell", "codex").filter(row => row.type === "pane").map(row => row.key), ["codex", "build-codex"]);
});

test("mailbox preserves multiple requests and selects their own replies", () => {
  const requests = [
    {target:"codex",kind:"REVIEW",body:"first unique request",reply:"first unique answer"},
    {target:"build-codex",kind:"TASK",body:"second unique request",reply:"second unique answer"},
  ];
  const sent = watchFrame({cols:150,rows:28,selected:"codex",details,view:"inbox",requests,tab:"sent",mailboxIndex:1}).lines.join("\n");
  assert.ok(sent.includes("codex: first unique answer"));
  const incoming = watchFrame({cols:150,rows:28,selected:"codex",details,view:"inbox",requests}).lines.join("\n");
  assert.ok(!incoming.includes("second unique request"));
});

test("default window focus folds pane rows and shows their live geometry in inspector", async () => {
  const { topologyRows } = await import("../src/dashboard/web/landing-watch.mjs");
  const tree=topologyRows(details,"window:acme/retry");
  assert.equal(tree.filter(row=>row.type==="pane").length,0);
  const screen=watchFrame({cols:150,rows:36,selected:"window:acme/retry",details}).lines.join("\n");
  for(const label of ["Inspector · window retry","scope  4 panes · 3 agents","live layout","%12 · claude","How should we retry?"])assert.ok(screen.includes(label),label);
  assert.equal(topologyRows(details,"claude").filter(row=>row.type==="pane").length,4);
});
