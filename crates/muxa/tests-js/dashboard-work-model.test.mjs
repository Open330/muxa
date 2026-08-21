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
  assert.equal(workspace.active, 0);
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
    { title: "idle", stage: "queued", latest: "" },
    { title: "active", stage: "in_progress", latest: "" },
    { title: "attention", stage: "attention", latest: "" },
  ];

  works.sort(compareWorks);
  assert.deepEqual(works.map((work) => work.title), ["attention", "active", "idle"]);
});

test("groups ticket-shaped sessions under one repository workspace", () => {
  const repoPanes = [
    {
      pane_id: "%10",
      session_id: "$10",
      session: "CAL-101",
      window_id: "@10",
      window_name: "codex",
      socket: "default",
      current_path: "/work/muxa",
    },
    {
      pane_id: "%11",
      session_id: "$11",
      session: "CAL-102",
      window_id: "@11",
      window_name: "claude",
      socket: "default",
      current_path: "/work/muxa",
    },
  ];

  const workspaces = buildWorkProjection(repoPanes, []);
  assert.equal(workspaces.length, 1);
  assert.equal(workspaces[0].name, "muxa");
  assert.equal(workspaces[0].source, "inferred");
  assert.deepEqual(workspaces[0].works.map((work) => work.ticketId), ["CAL-101", "CAL-102"]);
});

test("prefers managed identities and overlays durable workflow metadata", () => {
  const managedPane = {
    pane_id: "%20",
    session_id: "$20",
    session: "physical-session",
    window_id: "@20",
    window_name: "shell",
    socket: "/tmp/tmux-1000/default",
    current_path: "/work/physical",
    muxa: {
      managed_workspace: true,
      managed_work: true,
      workspace_id: "payments",
      workspace_cwd: "/work/payments",
      work_id: "PAY-42",
    },
  };
  const metadata = [{
    key: { host: "tmux", socket: "default", session_id: "$20", window_id: "@20" },
    metadata: {
      title: "Repair settlement retries",
      goal: "No duplicate settlements",
      next_action: "Run the recovery suite",
      stage: "review",
    },
  }];

  const [workspace] = buildWorkProjection([managedPane], [], metadata);
  const [work] = workspace.works;
  assert.equal(workspace.name, "payments");
  assert.equal(workspace.source, "managed");
  assert.equal(work.ticketId, "PAY-42");
  assert.equal(work.title, "Repair settlement retries");
  assert.equal(work.goal, "No duplicate settlements");
  assert.equal(work.nextAction, "Run the recovery suite");
  assert.equal(work.stage, "review");
});

test("live attention overrides in-progress metadata while done remains explicit", () => {
  const metadata = [{
    key: { host: "tmux", socket: "default", session_id: "$1", window_id: "@1" },
    metadata: { title: "Auth", goal: "", next_action: "", stage: "in_progress" },
  }];
  const workspace = buildWorkProjection(panes.slice(0, 2), agents.slice(0, 2), metadata)[0];
  assert.equal(workspace.works[0].stage, "attention");

  metadata[0].metadata.stage = "done";
  const done = buildWorkProjection(panes.slice(0, 2), agents.slice(0, 2), metadata)[0];
  assert.equal(done.works[0].stage, "done");
});

test("normalizes the current agent_session_id wire field without overwriting legacy ids", () => {
  assert.equal(normalizeAgent({ agent_session_id: "current" }).session_id, "current");
  assert.equal(
    normalizeAgent({ agent_session_id: "current", session_id: "legacy" }).session_id,
    "legacy"
  );
});
