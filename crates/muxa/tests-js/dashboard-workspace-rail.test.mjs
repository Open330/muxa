import assert from "node:assert/strict";
import test from "node:test";

import { buildTopology } from "../src/dashboard/web/topology-model.mjs";
import {
  buildRailEntries,
  findRailEntry,
  inRailScope,
  railScope,
  railStateClass,
  sortRailEntries,
  unlinkedInScope,
  worksInScope,
} from "../src/dashboard/web/workspace-rail.mjs";

function pane(pane_id, session, window_id, extra = {}) {
  return {
    host: "tmux",
    pane_id,
    session_id: `$${session}`,
    session,
    window_id,
    window_name: window_id,
    window_index: window_id.replace("@", ""),
    pane_index: "0",
    current_command: "zsh",
    title: "",
    current_path: `/src/${session}`,
    socket: "/tmp/tmux-501/default",
    ...extra,
  };
}

function agent(paneId, state, extra = {}) {
  return {
    session_id: `${paneId}-${state}`,
    pane: paneId,
    state,
    kind: "claude_code",
    tmux_socket: "/tmp/tmux-501/default",
    last_activity_at: "2026-09-29T10:00:00Z",
    ...extra,
  };
}

function run(session, windowId, extra = {}) {
  return {
    id: `tmux/${windowId}`,
    state: "running",
    linked: false,
    work: null,
    execution: { host: "tmux", socket: "/tmp/tmux-501/default", session_id: `$${session}`, window_id: windowId },
    session_name: session,
    window_name: windowId,
    window_index: windowId.replace("@", ""),
    panes: [],
    ...extra,
  };
}

function managed(id, works) {
  return {
    key: id,
    name: id,
    latest: works.reduce((latest, work) => (work.latest > latest ? work.latest : latest), ""),
    works: works.map((work) => ({
      signals: [],
      stage: "queued",
      participants: [],
      runs: [],
      latest: "",
      ...work,
    })),
  };
}

const byName = (entries) => Object.fromEntries(entries.map((entry) => [entry.label, entry]));

test("every tmux session is a rail entry even without managed Work", () => {
  const topology = buildTopology(
    [
      pane("%1", "youtube", "@1"),
      pane("%2", "youtube", "@1"),
      pane("%3", "youtube", "@2"),
      pane("%4", "somun", "@3"),
    ],
    [agent("%1", "working"), agent("%2", "idle"), agent("%4", "waiting_input")],
  );
  const entries = buildRailEntries({ workspaces: [], topology, unlinked: [] });
  const rows = byName(entries);
  assert.deepEqual(Object.keys(rows).sort(), ["somun", "youtube"]);
  assert.equal(rows.youtube.windows, 2);
  assert.equal(rows.youtube.agents, 2);
  assert.equal(rows.youtube.state, "working");
  assert.equal(rows.youtube.workCount, 0);
  assert.equal(rows.youtube.workspaceKey, "");
  assert.ok(rows.youtube.sessionKey, "links to the navigator's session node");
  assert.equal(rows.somun.state, "waiting_input");
  assert.equal(railStateClass(rows.somun.state), "waiting");
});

test("a managed workspace merges into the session of the same name", () => {
  const topology = buildTopology([pane("%1", "muxa", "@1"), pane("%2", "muxa", "@2")], [agent("%1", "idle")]);
  const entries = buildRailEntries({
    topology,
    workspaces: [
      managed("muxa", [{ stage: "in_progress", latest: "2026-09-29T11:00:00Z" }, { signals: ["error"] }]),
      managed("billing", [{ stage: "review", participants: [{ state: "idle" }] }]),
    ],
  });
  const rows = byName(entries);
  assert.deepEqual(Object.keys(rows).sort(), ["billing", "muxa"]);
  assert.equal(rows.muxa.kind, "session");
  assert.equal(rows.muxa.workCount, 2);
  assert.equal(rows.muxa.workspaceKey, "muxa");
  assert.equal(rows.muxa.windows, 2);
  // A Work error outranks idle agents in the roll-up.
  assert.equal(rows.muxa.state, "error");
  assert.equal(rows.muxa.latest, "2026-09-29T11:00:00Z");
  // A managed workspace without a live session keeps its own entry.
  assert.equal(rows.billing.kind, "workspace");
  assert.equal(rows.billing.sessionName, "");
  assert.equal(rows.billing.agents, 1);
  assert.equal(rows.billing.workCount, 1);
});

test("session names that repeat across sockets are disambiguated", () => {
  const topology = buildTopology(
    [
      pane("%1", "api", "@1"),
      pane("%1", "api", "@1", { socket: "/tmp/tmux-501/work" }),
      pane("%2", "web", "@2", { socket: "/tmp/tmux-501/work" }),
    ],
    [agent("%1", "error", { tmux_socket: "work" })],
  );
  const entries = buildRailEntries({
    topology,
    workspaces: [managed("api", [{ runs: [{ execution: { socket: "/tmp/tmux-501/work" } }] }])],
  });
  const labels = entries.map((entry) => entry.label).sort();
  assert.deepEqual(labels, ["api · default", "api · work", "web"]);
  const rows = byName(entries);
  assert.equal(rows["api · work"].state, "error");
  assert.equal(rows["api · default"].state, "");
  // The managed workspace joins the session on the socket its runs use.
  assert.equal(rows["api · work"].workCount, 1);
  assert.equal(rows["api · default"].workCount, 0);
  assert.notEqual(rows["api · work"].key, rows["api · default"].key);
});

test("sessions only known from unlinked executions still appear", () => {
  const topology = buildTopology([pane("%1", "muxa", "@1")], [agent("%1", "working")]);
  const entries = buildRailEntries({
    topology,
    unlinked: [
      run("muxa", "@1", { panes: [{ pane_id: "%1", agent: agent("%1", "working") }] }),
      run("iac", "@7", { panes: [{ pane_id: "%9", agent: agent("%9", "waiting_choice") }], latest_at: "2026-09-29T12:00:00Z" }),
      run("iac", "@8"),
    ],
  });
  const rows = byName(entries);
  // Agents already counted from the pane scan are not counted twice.
  assert.equal(rows.muxa.agents, 1);
  assert.equal(rows.muxa.windows, 1);
  assert.equal(rows.iac.windows, 2);
  assert.equal(rows.iac.agents, 1);
  assert.equal(rows.iac.state, "waiting_choice");
  assert.equal(rows.iac.latest, "2026-09-29T12:00:00Z");
});

test("sort orders: priority by rolled-up state, latest by activity, name", () => {
  const entries = [
    { key: "a", label: "alpha", state: "idle", workCount: 0, agents: 1, latest: "2026-09-29T09:00:00Z" },
    { key: "b", label: "bravo", state: "error", workCount: 0, agents: 1, latest: "2026-09-28T09:00:00Z" },
    { key: "c", label: "charlie", state: "working", workCount: 0, agents: 3, latest: "2026-09-29T12:00:00Z" },
    { key: "d", label: "delta", state: "waiting_input", workCount: 0, agents: 1, latest: "" },
    { key: "e", label: "echo", state: "working", workCount: 1, agents: 1, latest: "" },
  ];
  assert.deepEqual(sortRailEntries(entries, "priority").map((e) => e.label),
    ["bravo", "delta", "echo", "charlie", "alpha"]);
  assert.deepEqual(sortRailEntries(entries, "latest").map((e) => e.label),
    ["charlie", "alpha", "bravo", "delta", "echo"]);
  assert.deepEqual(sortRailEntries(entries, "name").map((e) => e.label),
    ["alpha", "bravo", "charlie", "delta", "echo"]);
  // Sorting does not mutate the input.
  assert.equal(entries[0].label, "alpha");
});

test("selecting an entry scopes Work, unlinked windows, and panes by session", () => {
  const topology = buildTopology(
    [pane("%1", "api", "@1"), pane("%1", "api", "@1", { socket: "/tmp/tmux-501/work" }), pane("%2", "web", "@2")],
    [],
  );
  const workspaces = [managed("web", [{ stage: "queued" }]), managed("billing", [{ stage: "done" }])];
  const entries = buildRailEntries({ topology, workspaces });
  const web = findRailEntry(entries, { session: "web" });
  const scope = railScope(web);
  assert.deepEqual(scope, { session: "web", socket: "default", workspace: "web" });
  assert.equal(worksInScope(workspaces, scope).length, 1);
  assert.equal(worksInScope(workspaces, null).length, 2);

  const apiWork = findRailEntry(entries, { session: "api", socket: "/tmp/tmux-501/work" });
  assert.equal(apiWork.label, "api · work");
  const apiScope = railScope(apiWork);
  assert.deepEqual(worksInScope(workspaces, apiScope), []);
  const runs = [
    run("api", "@1"),
    run("api", "@1", { execution: { socket: "/tmp/tmux-501/work", window_id: "@1" } }),
    run("web", "@2"),
  ];
  assert.equal(unlinkedInScope(runs, apiScope).length, 1);
  assert.equal(unlinkedInScope(runs, null).length, 3);
  assert.ok(inRailScope(apiScope, { session: "api", socket: "work" }));
  assert.ok(!inRailScope(apiScope, { session: "api", socket: "/tmp/tmux-501/default" }));
  // An item that does not know its socket is not excluded by it.
  assert.ok(inRailScope(apiScope, { session: "api" }));
  assert.ok(!inRailScope(apiScope, { session: "web" }));

  // A managed-only workspace is found by id and scopes Work, not tmux.
  const billing = findRailEntry(entries, { session: "billing" });
  assert.equal(billing.kind, "workspace");
  const billingScope = railScope(billing);
  assert.equal(worksInScope(workspaces, billingScope).length, 1);
  assert.equal(unlinkedInScope(runs, billingScope).length, 0);
  assert.equal(findRailEntry(entries, { session: "nope" }), null);
  assert.equal(railScope(null), null);
});
