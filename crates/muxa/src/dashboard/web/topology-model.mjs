// Session → window → pane topology for the dashboard's Sessions navigator.
//
// Pure functions only (no DOM), so the roll-up and layout rules are tested
// under node (tests-js/dashboard-topology-model.test.mjs). The inputs are the
// dashboard's existing /api/panes rows (which carry socket, session, window
// and pane ids) and the /api/agents rows (joined to panes by pane id and
// socket), plus optional /api/windows/{id}/layout geometry.

/// Attention order for rolled-up state: the most urgent state below a node
/// is the one it shows. Errors first, then agents waiting on a person, then
/// work in progress; idle and stopped are quiet.
const STATE_PRIORITY = {
  error: 60,
  waiting_input: 50,
  waiting_choice: 50,
  working: 40,
  starting: 30,
  idle: 20,
  stopped: 10,
};

export function stateRank(state) {
  return STATE_PRIORITY[state] ?? 0;
}

/// The most urgent state among `states`, or "" when none is known.
export function rollupState(states) {
  let best = "";
  for (const state of states) {
    if (state && stateRank(state) > stateRank(best)) best = state;
  }
  return best;
}

export function socketShort(socket) {
  return String(socket || "default").split("/").pop() || "default";
}

function numeric(value) {
  const n = Number.parseInt(value, 10);
  return Number.isNaN(n) ? Number.MAX_SAFE_INTEGER : n;
}

function byIndex(a, b) {
  return numeric(a.index) - numeric(b.index) || String(a.index).localeCompare(String(b.index));
}

/// Node keys travel through data-* attributes, so they must survive HTML
/// parsing (no NUL); JSON keeps the parts unambiguous.
export function topologyKey(kind, socket, id) {
  return JSON.stringify([kind, String(socket || ""), String(id || "")]);
}

// Agents that belong to one pane: same pane id and, when the agent recorded
// its server, the same socket. A pane id that exists on several servers is
// only matched when the socket settles it.
function agentsByPane(panes, agents) {
  const byPaneId = new Map();
  for (const pane of panes) {
    const list = byPaneId.get(pane.pane_id) || [];
    list.push(pane);
    byPaneId.set(pane.pane_id, list);
  }
  const attached = new Map();
  const orphans = [];
  for (const agent of agents) {
    if (!agent?.pane) {
      orphans.push(agent);
      continue;
    }
    let candidates = byPaneId.get(agent.pane) || [];
    if (agent.tmux_socket && candidates.length > 1) {
      candidates = candidates.filter((pane) => socketShort(pane.socket) === socketShort(agent.tmux_socket));
    }
    if (candidates.length !== 1) {
      orphans.push(agent);
      continue;
    }
    const pane = candidates[0];
    const list = attached.get(pane) || [];
    list.push(agent);
    attached.set(pane, list);
  }
  return { attached, orphans };
}

function summarize(node, children) {
  node.state = rollupState(children.map((child) => child.state));
  node.paneCount = children.reduce((sum, child) => sum + child.paneCount, 0);
  node.agentCount = children.reduce((sum, child) => sum + child.agentCount, 0);
  node.kinds = [...new Set(children.flatMap((child) => child.kinds))].sort();
}

/// Build the socket → session → window → pane tree. `nodes` maps every key
/// to its node; `multiSocket` says whether the tree needs a socket level.
export function buildTopology(panes = [], agents = []) {
  const { attached, orphans } = agentsByPane(panes, agents);
  const sockets = new Map();
  const nodes = new Map();

  for (const row of panes) {
    const socket = String(row.socket || "");
    let socketNode = sockets.get(socket);
    if (!socketNode) {
      socketNode = {
        type: "socket",
        key: topologyKey("socket", socket, ""),
        socket,
        host: row.host || "tmux",
        label: socketShort(socket),
        sessions: new Map(),
      };
      sockets.set(socket, socketNode);
    }
    let session = socketNode.sessions.get(row.session_id);
    if (!session) {
      session = {
        type: "session",
        key: topologyKey("session", socket, row.session_id),
        socket,
        host: row.host || "tmux",
        id: row.session_id,
        name: row.session || row.session_id,
        windows: new Map(),
      };
      socketNode.sessions.set(row.session_id, session);
    }
    let window = session.windows.get(row.window_id);
    if (!window) {
      window = {
        type: "window",
        key: topologyKey("window", socket, row.window_id),
        socket,
        host: row.host || "tmux",
        id: row.window_id,
        index: row.window_index,
        name: row.window_name || "",
        sessionKey: session.key,
        sessionName: session.name,
        panes: [],
      };
      session.windows.set(row.window_id, window);
    }
    const paneAgents = (attached.get(row) || []).slice().sort((a, b) =>
      stateRank(b.state) - stateRank(a.state)
      || String(b.last_activity_at || "").localeCompare(String(a.last_activity_at || "")));
    window.panes.push({
      type: "pane",
      key: topologyKey("pane", socket, row.pane_id),
      socket,
      host: row.host || "tmux",
      id: row.pane_id,
      index: row.pane_index,
      command: row.current_command || "",
      title: row.title || "",
      cwd: paneAgents[0]?.cwd || row.current_path || "",
      windowKey: window.key,
      sessionKey: session.key,
      agents: paneAgents,
      agent: paneAgents[0] || null,
      state: rollupState(paneAgents.map((agent) => agent.state)),
      paneCount: 1,
      agentCount: paneAgents.length,
      kinds: [...new Set(paneAgents.map((agent) => agent.kind).filter(Boolean))].sort(),
    });
  }

  const socketList = [...sockets.values()]
    .sort((a, b) => a.label.localeCompare(b.label) || a.socket.localeCompare(b.socket))
    .map((socketNode) => {
      const sessions = [...socketNode.sessions.values()]
        .sort((a, b) => a.name.localeCompare(b.name))
        .map((session) => {
          const windows = [...session.windows.values()].sort(byIndex).map((window) => {
            window.panes.sort(byIndex);
            summarize(window, window.panes);
            window.cwd = window.panes[0]?.cwd || "";
            nodes.set(window.key, window);
            for (const pane of window.panes) nodes.set(pane.key, pane);
            return window;
          });
          const node = { ...session, windows };
          summarize(node, windows);
          for (const window of windows) window.sessionNode = node;
          nodes.set(node.key, node);
          return node;
        });
      const node = { ...socketNode, sessions };
      summarize(node, sessions);
      nodes.set(node.key, node);
      return node;
    });

  return { sockets: socketList, multiSocket: socketList.length > 1, nodes, orphans };
}

function haystack(node) {
  switch (node.type) {
    case "session":
      return [node.name, node.id];
    case "window":
      return [node.name, node.index, `${node.sessionName}:${node.index}`, node.id, node.cwd];
    case "pane":
      return [node.id, node.index, node.command, node.title, node.cwd, ...node.kinds,
        ...node.agents.map((agent) => agent.model || "")];
    default:
      return [node.label, node.socket];
  }
}

function matches(node, terms) {
  const text = haystack(node).join(" ").toLowerCase();
  return terms.every((term) => text.includes(term));
}

/// Keys of the nodes to show for a filter query (session, window, cwd, agent
/// kind, …; whitespace-separated terms must all match somewhere on the path
/// from the session down). `null` means "no filter: show everything".
export function filterTopology(topology, query) {
  const terms = String(query || "").toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return null;
  const visible = new Set();
  for (const socketNode of topology.sockets) {
    for (const session of socketNode.sessions) {
      for (const window of session.windows) {
        for (const pane of window.panes) {
          const path = [session, window, pane];
          const text = path.map((node) => haystack(node).join(" ")).join(" ").toLowerCase();
          if (terms.every((term) => text.includes(term))) {
            visible.add(pane.key);
            visible.add(window.key);
            visible.add(session.key);
            visible.add(socketNode.key);
          }
        }
        if (matches(window, terms) || matches(session, terms)) {
          visible.add(window.key);
          visible.add(session.key);
          visible.add(socketNode.key);
        }
      }
    }
  }
  return visible;
}

/// Place a window's panes the way tmux draws them. `geometry` is the
/// /api/windows/{id}/layout `panes` list (cells). Returns `null` when the
/// geometry does not describe exactly the panes we know about, so the caller
/// falls back to a plain grid. A zoomed window shows only its active pane.
export function layoutTiles(geometry, paneIds, zoomed = false) {
  if (!Array.isArray(geometry) || geometry.length === 0) return null;
  let rows = geometry.filter((pane) => pane.width > 0 && pane.height > 0);
  if (zoomed) {
    const active = rows.find((pane) => pane.active);
    if (active) rows = [{ ...active, left: 0, top: 0 }];
  }
  const known = new Set(paneIds);
  if (!zoomed && (rows.length !== known.size || rows.some((pane) => !known.has(pane.pane_id)))) {
    return null;
  }
  const width = Math.max(...rows.map((pane) => pane.left + pane.width));
  const height = Math.max(...rows.map((pane) => pane.top + pane.height));
  if (!(width > 0 && height > 0)) return null;
  const pct = (value, total) => Math.round((value / total) * 10000) / 100;
  return {
    // Terminal cells are about twice as tall as they are wide.
    aspect: width / (height * 2),
    tiles: rows.map((pane) => ({
      paneId: pane.pane_id,
      left: pct(pane.left, width),
      top: pct(pane.top, height),
      width: pct(pane.width, width),
      height: pct(pane.height, height),
      active: Boolean(pane.active),
    })),
  };
}

/// `#session=…&window=…&socket=…` → selection, or null.
export function parseTopologyHash(hash) {
  const params = new URLSearchParams(String(hash || "").replace(/^#/, ""));
  const session = params.get("session");
  const window = params.get("window");
  if (!session && !window) return null;
  return { session: session || null, window: window || null, socket: params.get("socket") || null };
}

/// Return `hash` with the navigator selection set (or removed), keeping any
/// other fragment parameters (such as an open pane).
export function withTopologyHash(hash, selection) {
  const params = new URLSearchParams(String(hash || "").replace(/^#/, ""));
  params.delete("session");
  params.delete("window");
  if (!params.get("pane")) params.delete("socket");
  if (selection?.session) params.set("session", selection.session);
  if (selection?.window) params.set("window", selection.window);
  if (selection?.socket && (selection.session || selection.window)) params.set("socket", selection.socket);
  const rest = params.toString();
  return rest ? `#${rest}` : "";
}

/// Session nodes inside a rail scope (`{ session, socket }`, see
/// workspace-rail.mjs railScope). `null` scope means every session; a scope
/// without a session (a managed workspace with no live tmux session) has
/// none. An empty scope socket matches the session on any server.
export function sessionsInScope(topology, scope) {
  const sessions = (topology?.sockets || []).flatMap((socketNode) => socketNode.sessions);
  if (!scope) return sessions;
  if (!scope.session) return [];
  return sessions.filter((session) => session.name === scope.session
    && (!scope.socket || socketShort(session.socket) === socketShort(scope.socket)));
}

/// A function resolving a pane (id + optional socket) to the tmux session it
/// lives in, `{ session, socket }`, or null when the pane scan does not know
/// it. A pane id that exists on several servers only resolves when the
/// socket settles it.
export function paneSessionResolver(topology) {
  const byId = new Map();
  for (const node of topology?.nodes?.values() || []) {
    if (node.type !== "pane") continue;
    const list = byId.get(node.id) || [];
    list.push(node);
    byId.set(node.id, list);
  }
  return (paneId, socket) => {
    let candidates = byId.get(paneId) || [];
    if (socket) candidates = candidates.filter((pane) => socketShort(pane.socket) === socketShort(socket));
    if (candidates.length !== 1) return null;
    const session = topology.nodes.get(candidates[0].sessionKey);
    return session ? { session: session.name, socket: socketShort(session.socket) } : null;
  };
}
