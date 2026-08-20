import test from "node:test";
import assert from "node:assert/strict";

import {
  buildWorkProjection,
  compareWorks,
  normalizeAgent,
  workNeedsAttention,
} from "../src/dashboard/web/work-model.mjs";

const panes = [
  {
    pane_id: "%1",
    session_id: "$1",
    session: "muxa",
    window_id: "@1",
    window_name: "TEST-123",
    window_index: "1",
    pane_index: "0",
    socket: "/tmp/tmux-1000/default",
    current_command: "codex",
    title: "implement auth",
  },
  {
    pane_id: "%2",
    session_id: "$1",
    session: "muxa",
    window_id: "@1",
    window_name: "TEST-123",
    window_index: "1",
    pane_index: "1",
    socket: "/tmp/tmux-1000/default",
    current_command: "claude",
    title: "review auth",
  },
  {
    pane_id: "%1",
    session_id: "$2",
    session: "another",
    window_id: "@2",
    window_name: "DOCS-9",
    window_index: "0",
    pane_index: "0",
    socket: "/tmp/tmux-1000/agents",
    current_command: "codex",
    title: "write docs",
  },
];

const agents = [
  {
    agent_session_id: "agent-1",
    pane: "%1",
    tmux_socket: "default",
    kind: "codex",
    state: "working",
    ai_title: "implement refresh lock",
    last_activity_at: "2026-08-20T01:03:00Z",
  },
  {
    agent_session_id: "agent-2",
    pane: "%2",
    tmux_socket: "default",
    kind: "claude_code",
    state: "waiting_input",
    last_prompt: "review the authentication change",
    last_activity_at: "2026-08-20T01:04:00Z",
  },
  {
    agent_session_id: "agent-3",
    pane: "%1",
    tmux_socket: "agents",
    kind: "codex",
    state: "idle",
    last_activity_at: "2026-08-20T00:30:00Z",
  },
];

test("projects panes as workspace work items with agents as participants", () => {
  const workspaces = buildWorkProjection(panes, agents);

  assert.equal(workspaces.length, 2);
  const workspace = workspaces.find((item) => item.name === "muxa");
  assert.equal(workspace.works.length, 1);
  assert.equal(workspace.works[0].name, "TEST-123");
  assert.equal(workspace.works[0].participants.length, 2);
  assert.equal(workspace.works[0].state, "needs_attention");
  assert.equal(workspace.works[0].summary, "review the authentication change");
  assert.equal(workspace.attention, 1);
  assert.equal(workspace.active, 1);
  assert.equal(workNeedsAttention(workspace.works[0]), true);
});

test("does not collide identical pane ids across tmux sockets", () => {
  const workspaces = buildWorkProjection(panes, agents);
  const docs = workspaces.find((item) => item.name === "another").works[0];

  assert.equal(docs.participants.length, 1);
  assert.equal(docs.participants[0].agent_session_id, "agent-3");
});

test("orders attention work ahead of active and available work", () => {
  const works = [
    { name: "idle", errors: 0, waiting: 0, working: 0, latest: "" },
    { name: "active", errors: 0, waiting: 0, working: 1, latest: "" },
    { name: "attention", errors: 0, waiting: 1, working: 0, latest: "" },
  ];

  works.sort(compareWorks);
  assert.deepEqual(works.map((work) => work.name), ["attention", "active", "idle"]);
});

test("normalizes the current agent_session_id wire field without overwriting legacy ids", () => {
  assert.equal(normalizeAgent({ agent_session_id: "current" }).session_id, "current");
  assert.equal(
    normalizeAgent({ agent_session_id: "current", session_id: "legacy" }).session_id,
    "legacy"
  );
});
