import assert from "node:assert/strict";
import test from "node:test";

import {
  buildTopology,
  filterTopology,
  layoutTiles,
  paneSessionResolver,
  parseTopologyHash,
  rollupState,
  sessionsInScope,
  withTopologyHash,
} from "../src/dashboard/web/topology-model.mjs";
import { withPaneHash } from "../src/dashboard/web/pane-drawer.mjs";

function pane(pane_id, session, window_id, window_index, pane_index, extra = {}) {
  return {
    host: "tmux",
    pane_id,
    session_id: `$${session}`,
    session,
    window_id,
    window_name: `w${window_index}`,
    window_index: String(window_index),
    pane_index: String(pane_index),
    current_command: "zsh",
    title: "",
    current_path: `/src/${session}`,
    socket: "/tmp/tmux-501/default",
    ...extra,
  };
}

function agent(paneId, state, kind = "claude_code", extra = {}) {
  return { session_id: `${paneId}-${state}`, pane: paneId, state, kind, tmux_socket: "/tmp/tmux-501/default", ...extra };
}

test("roll-up shows the most urgent state: error > waiting > working > idle", () => {
  assert.equal(rollupState(["idle", "working"]), "working");
  assert.equal(rollupState(["working", "waiting_input", "idle"]), "waiting_input");
  assert.equal(rollupState(["waiting_choice", "error"]), "error");
  assert.equal(rollupState(["stopped", "idle"]), "idle");
  assert.equal(rollupState([]), "");
  assert.equal(rollupState(["", undefined]), "");
});

test("panes and agents become session → window → pane with counts and kinds", () => {
  const topology = buildTopology(
    [
      pane("%3", "muxa", "@2", 1, 0),
      pane("%1", "muxa", "@1", 0, 0),
      pane("%2", "muxa", "@1", 0, 1),
      pane("%9", "api", "@5", 0, 0),
    ],
    [agent("%1", "working"), agent("%2", "waiting_input", "codex"), agent("%404", "error")],
  );
  assert.equal(topology.multiSocket, false);
  const [socket] = topology.sockets;
  assert.deepEqual(socket.sessions.map((s) => s.name), ["api", "muxa"]);
  const muxa = socket.sessions[1];
  assert.equal(muxa.state, "waiting_input");
  assert.equal(muxa.paneCount, 3);
  assert.equal(muxa.agentCount, 2);
  assert.deepEqual(muxa.kinds, ["claude_code", "codex"]);
  assert.deepEqual(muxa.windows.map((w) => w.index), ["0", "1"]);
  assert.deepEqual(muxa.windows[0].panes.map((p) => p.id), ["%1", "%2"]);
  assert.equal(muxa.windows[1].state, "");
  assert.equal(socket.sessions[0].agentCount, 0);
  // An agent whose pane is not in the inventory is reported, not dropped.
  assert.deepEqual(topology.orphans.map((a) => a.pane), ["%404"]);
  assert.equal(topology.nodes.get(muxa.windows[0].panes[1].key).agent.kind, "codex");
});

test("a pane id on two servers is joined by socket", () => {
  const topology = buildTopology(
    [
      pane("%1", "a", "@1", 0, 0),
      pane("%1", "b", "@1", 0, 0, { socket: "/tmp/tmux-501/work" }),
    ],
    [agent("%1", "error", "codex", { tmux_socket: "work" })],
  );
  assert.equal(topology.multiSocket, true);
  const work = topology.sockets.find((s) => s.label === "work");
  assert.equal(work.state, "error");
  assert.equal(topology.sockets.find((s) => s.label === "default").state, "");
});

test("filter keeps matching paths and their ancestors", () => {
  const topology = buildTopology(
    [pane("%1", "muxa", "@1", 0, 0), pane("%2", "api", "@2", 0, 0, { current_path: "/srv/billing" })],
    [agent("%1", "working", "codex")],
  );
  assert.equal(filterTopology(topology, ""), null);
  const byKind = filterTopology(topology, "codex");
  const muxa = topology.sockets[0].sessions.find((s) => s.name === "muxa");
  const api = topology.sockets[0].sessions.find((s) => s.name === "api");
  assert.ok(byKind.has(muxa.key) && byKind.has(muxa.windows[0].panes[0].key));
  assert.ok(!byKind.has(api.key));
  const byCwd = filterTopology(topology, "billing");
  assert.ok(byCwd.has(api.windows[0].panes[0].key) && !byCwd.has(muxa.key));
  // Terms combine across the path: session name + agent kind.
  assert.ok(filterTopology(topology, "muxa codex").has(muxa.windows[0].key));
  assert.equal(filterTopology(topology, "api codex").size, 0);
});

test("geometry becomes percentage tiles; mismatches fall back to a grid", () => {
  const geometry = [
    { pane_id: "%1", left: 0, top: 0, width: 100, height: 50, active: true },
    { pane_id: "%2", left: 101, top: 0, width: 99, height: 24 },
    { pane_id: "%3", left: 101, top: 25, width: 99, height: 25 },
  ];
  const layout = layoutTiles(geometry, ["%1", "%2", "%3"]);
  assert.equal(layout.tiles.length, 3);
  assert.deepEqual(layout.tiles[0], { paneId: "%1", left: 0, top: 0, width: 50, height: 100, active: true });
  assert.equal(layout.tiles[1].left, 50.5);
  assert.equal(layout.tiles[2].top, 50);
  assert.equal(layout.aspect, 2);
  assert.equal(layoutTiles(geometry, ["%1", "%2"]), null);
  assert.equal(layoutTiles([], ["%1"]), null);
  // Zoomed: only the active pane, full size.
  const zoomed = layoutTiles(geometry, ["%1", "%2", "%3"], true);
  assert.deepEqual(zoomed.tiles.map((t) => [t.paneId, t.width, t.height]), [["%1", 100, 100]]);
});

test("navigator and pane deep links share the fragment", () => {
  assert.deepEqual(parseTopologyHash("#session=muxa&socket=default"), { session: "muxa", window: null, socket: "default" });
  assert.equal(parseTopologyHash("#pane=%251"), null);
  let hash = withTopologyHash("", { session: "muxa", window: "@3", socket: "default" });
  assert.deepEqual(parseTopologyHash(hash), { session: "muxa", window: "@3", socket: "default" });
  hash = withPaneHash(hash, { pane: "%5", socket: "default" });
  hash = withPaneHash(hash, null);
  assert.deepEqual(parseTopologyHash(hash), { session: "muxa", window: "@3", socket: "default" });
  assert.equal(withTopologyHash(hash, null), "");
});

test("a rail scope narrows the navigator to its session", () => {
  const topology = buildTopology([
    pane("%1", "youtube", "@1", 1, 0),
    pane("%2", "somun", "@2", 1, 0),
    pane("%3", "youtube", "@3", 1, 0, { socket: "/tmp/tmux-501/work" }),
  ], []);
  assert.equal(sessionsInScope(topology, null).length, 3);
  assert.deepEqual(sessionsInScope(topology, { session: "youtube", socket: "" }).map((s) => s.socket),
    ["/tmp/tmux-501/default", "/tmp/tmux-501/work"]);
  assert.deepEqual(sessionsInScope(topology, { session: "youtube", socket: "work" }).map((s) => s.socket),
    ["/tmp/tmux-501/work"]);
  // A managed workspace without a live session has no tree.
  assert.deepEqual(sessionsInScope(topology, { session: "", socket: "", workspace: "billing" }), []);
});

test("a pane id resolves to its session, by socket when the id repeats", () => {
  const topology = buildTopology([
    pane("%1", "youtube", "@1", 1, 0),
    pane("%1", "api", "@9", 1, 0, { socket: "/tmp/tmux-501/work" }),
    pane("%2", "somun", "@2", 1, 0),
  ], []);
  const resolve = paneSessionResolver(topology);
  assert.deepEqual(resolve("%2"), { session: "somun", socket: "default" });
  assert.deepEqual(resolve("%2", "/tmp/tmux-501/default"), { session: "somun", socket: "default" });
  assert.equal(resolve("%2", "work"), null, "a pane on another server is not this one");
  assert.equal(resolve("%1"), null, "ambiguous without a socket");
  assert.deepEqual(resolve("%1", "work"), { session: "api", socket: "work" });
  assert.equal(resolve("%404"), null);
});
