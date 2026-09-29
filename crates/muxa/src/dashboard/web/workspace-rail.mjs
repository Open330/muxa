// Entries for the dashboard's Workspaces rail.
//
// muxa's managed layout is Workspace = tmux session, Run = window, Agent =
// pane, so every live tmux session is shown as a workspace even when nothing
// in it was started with `muxa work up`. This is presentation only: an entry
// built from a session never creates Work, and its Work count comes solely
// from the managed Works that /api/works reports for a workspace of the same
// id. Pure functions (no DOM) so the merge, roll-up and ordering rules are
// tested under node (tests-js/dashboard-workspace-rail.test.mjs).

import { rollupState, socketShort, stateRank } from "./topology-model.mjs";

export const RAIL_SORTS = ["priority", "latest", "name"];

/// Rail entry keys travel through data-* attributes; JSON keeps the parts
/// unambiguous with printable characters only.
export function railSessionKey(socket, name) {
  return JSON.stringify(["session", socketShort(socket), String(name || "")]);
}

export function railWorkspaceKey(id) {
  return JSON.stringify(["workspace", String(id || "")]);
}

function later(a, b) {
  return String(a || "") > String(b || "") ? String(a || "") : String(b || "");
}

// A managed Work contributes to the roll-up through its signals and stage,
// expressed in the agent-state vocabulary the roll-up already ranks.
function workStates(work) {
  const signals = work.signals || [];
  const states = [];
  if (signals.includes("error")) states.push("error");
  if (signals.includes("attention") || signals.includes("blocked")) states.push("waiting_input");
  if (work.stage === "in_progress") states.push("working");
  for (const agent of work.participants || []) states.push(agent?.state);
  return states;
}

function workSockets(workspace) {
  const sockets = new Set();
  for (const work of workspace.works || []) {
    for (const run of work.runs || []) {
      if (run.execution?.socket) sockets.add(socketShort(run.execution.socket));
    }
  }
  return sockets;
}

function sessionEntry(socket, name) {
  return {
    key: railSessionKey(socket, name),
    kind: "session",
    name,
    label: name,
    socket: socketShort(socket),
    sessionName: name,
    sessionKey: "",
    workspaceKey: "",
    windowIds: new Set(),
    windows: 0,
    agents: 0,
    states: [],
    works: [],
    latest: "",
  };
}

/// Build the rail from managed workspaces (the dashboard's adapted
/// /api/works workspaces: `{ key, name, works: [{ runs, signals, stage,
/// participants, latest }], latest }`), the session → window → pane topology
/// (buildTopology), and /api/works `unlinked_executions` (sessions the pane
/// scan has not reported yet still get an entry).
///
/// A managed workspace whose id equals a session name merges into that
/// session's entry; when the name exists on several tmux servers, the entry
/// on the socket its runs use wins (else the first by socket). Session names
/// that repeat across sockets are labelled `name · socket`.
export function buildRailEntries({ workspaces = [], topology = null, unlinked = [] } = {}) {
  const sessions = new Map();
  const ensure = (socket, name) => {
    const key = railSessionKey(socket, name);
    if (!sessions.has(key)) sessions.set(key, sessionEntry(socket, name));
    return sessions.get(key);
  };

  for (const socketNode of topology?.sockets || []) {
    for (const session of socketNode.sessions || []) {
      const entry = ensure(session.socket, session.name);
      entry.sessionKey = session.key;
      for (const window of session.windows || []) {
        entry.windowIds.add(window.id);
        for (const pane of window.panes || []) {
          for (const agent of pane.agents || []) {
            entry.agents += 1;
            entry.states.push(agent.state);
            entry.latest = later(entry.latest, agent.last_activity_at);
          }
        }
      }
    }
  }

  for (const run of unlinked) {
    const name = run.session_name || run.execution?.session_id;
    if (!name) continue;
    const entry = ensure(run.execution?.socket, name);
    const known = entry.sessionKey !== "";
    entry.windowIds.add(run.execution?.window_id || run.id);
    entry.latest = later(entry.latest, run.latest_at);
    if (known) continue;
    // Only count agents from the Work snapshot when the pane scan did not
    // already report this session; otherwise they would be counted twice.
    for (const pane of run.panes || []) {
      if (!pane.agent) continue;
      entry.agents += 1;
      entry.states.push(pane.agent.state);
      entry.latest = later(entry.latest, pane.agent.last_activity_at);
    }
  }

  const bySessionName = new Map();
  for (const entry of sessions.values()) {
    const list = bySessionName.get(entry.name) || [];
    list.push(entry);
    bySessionName.set(entry.name, list);
  }
  for (const list of bySessionName.values()) {
    list.sort((a, b) => a.socket.localeCompare(b.socket));
    if (list.length > 1) for (const entry of list) entry.label = `${entry.name} · ${entry.socket}`;
  }

  const entries = [...sessions.values()];
  for (const workspace of workspaces) {
    const candidates = bySessionName.get(workspace.key) || [];
    const sockets = workSockets(workspace);
    const target = candidates.find((entry) => sockets.has(entry.socket)) || candidates[0];
    const entry = target || {
      ...sessionEntry("", workspace.name || workspace.key),
      key: railWorkspaceKey(workspace.key),
      kind: "workspace",
      socket: "",
      sessionName: "",
    };
    if (!target) {
      entry.label = entry.name;
      entries.push(entry);
    }
    entry.workspaceKey = workspace.key;
    entry.works = workspace.works || [];
    for (const work of entry.works) {
      entry.states.push(...workStates(work));
      entry.latest = later(entry.latest, work.latest);
      if (!target) entry.agents += (work.participants || []).length;
      for (const run of work.runs || []) {
        if (!target) entry.windowIds.add(run.execution?.window_id || run.id);
      }
    }
    entry.latest = later(entry.latest, workspace.latest);
  }

  return entries.map(({ windowIds, states, ...entry }) => ({
    ...entry,
    windows: windowIds.size,
    state: rollupState(states),
    workCount: entry.works.length,
  }));
}

/// Dot class for a rolled-up state.
export function railStateClass(state) {
  if (state === "error") return "error";
  if (state === "waiting_input" || state === "waiting_choice") return "waiting";
  if (state === "working" || state === "starting") return "working";
  return "idle";
}

/// Order entries for the rail's sort control: `priority` is the rolled-up
/// state (then Work, agents, recency), `latest` the last agent or Work
/// activity, `name` the label.
export function sortRailEntries(entries, sort = "priority") {
  const byName = (a, b) => a.label.localeCompare(b.label) || a.key.localeCompare(b.key);
  const byLatest = (a, b) => String(b.latest || "").localeCompare(String(a.latest || ""));
  const compare = sort === "name"
    ? byName
    : sort === "latest"
      ? (a, b) => byLatest(a, b) || byName(a, b)
      : (a, b) => stateRank(b.state) - stateRank(a.state)
        || b.workCount - a.workCount
        || b.agents - a.agents
        || byLatest(a, b)
        || byName(a, b);
  return [...entries].sort(compare);
}

/// The entry a `#session=<name>[&socket=<socket>]` link names: a session
/// entry by name (and socket when given), else a managed-only workspace by
/// id.
export function findRailEntry(entries, { session, socket } = {}) {
  if (!session) return null;
  const onSocket = (entry) => !socket || entry.socket === socketShort(socket);
  return entries.find((entry) => entry.sessionName === session && onSocket(entry))
    || entries.find((entry) => entry.sessionName === session)
    || entries.find((entry) => !entry.sessionName && entry.workspaceKey === session)
    || null;
}

/// What selecting an entry filters by. `null` scope means all workspaces.
export function railScope(entry) {
  if (!entry) return null;
  return {
    session: entry.sessionName || "",
    socket: entry.sessionName ? entry.socket : "",
    workspace: entry.workspaceKey || "",
  };
}

/// Whether something that lives in tmux session `session` on `socket` is
/// inside `scope`. An unknown socket on the item does not exclude it.
export function inRailScope(scope, { session, socket } = {}) {
  if (!scope) return true;
  if (!scope.session) return false;
  if (session !== scope.session) return false;
  return !scope.socket || !socket || socketShort(socket) === scope.socket;
}

/// Unlinked executions (tmux windows not tracked as Work) inside `scope`.
export function unlinkedInScope(runs, scope) {
  if (!scope) return runs;
  return runs.filter((run) =>
    inRailScope(scope, { session: run.session_name, socket: run.execution?.socket }));
}

/// Managed Works inside `scope`.
export function worksInScope(workspaces, scope) {
  if (!scope) return workspaces.flatMap((workspace) => workspace.works || []);
  if (!scope.workspace) return [];
  return workspaces.find((workspace) => workspace.key === scope.workspace)?.works || [];
}

// ── Page scope in the URL fragment ─────────────────────────────────
//
// The rail's selection is the page *scope*: `#workspace=<name>` names a
// session entry by tmux session name (or a managed-only workspace by id), and
// `&wsocket=<socket>` pins the tmux server when that session name exists on
// more than one. It is independent of the Sessions navigator's *detail
// selection* (`#session=` / `#window=` / `#pane=`, which share `socket=`), so
// picking a window there never changes what the rest of the page is scoped
// to. The fragment form is a scope reference `{ workspace, socket }`; resolve
// it to a rail entry with findScopeEntry.

function hashParams(hash) {
  return new URLSearchParams(String(hash || "").replace(/^#/, ""));
}

function hashString(params) {
  const rest = params.toString();
  return rest ? `#${rest}` : "";
}

/// `#workspace=<name>[&wsocket=<socket>]` → `{ workspace, socket }`, or null
/// for "all workspaces".
export function parseScopeHash(hash) {
  const params = hashParams(hash);
  const workspace = params.get("workspace");
  if (!workspace) return null;
  return { workspace, socket: params.get("wsocket") || null };
}

/// Return `hash` with the page scope set (or removed for `null`), keeping the
/// navigator selection, an open pane and anything else in the fragment.
export function withScopeHash(hash, ref) {
  const params = hashParams(hash);
  params.delete("workspace");
  params.delete("wsocket");
  if (ref?.workspace) {
    params.set("workspace", ref.workspace);
    if (ref.socket) params.set("wsocket", socketShort(ref.socket));
  }
  return hashString(params);
}

/// Links from before the scope/selection split (`#session=<name>[&socket=]`
/// with no `workspace=`) scoped the whole page to that session. Returns the
/// fragment with the equivalent `workspace=`/`wsocket=` added — `session`,
/// `window` and `socket` stay as the navigator selection — or null when
/// nothing needs migrating. Applied once, on load.
export function migrateLegacyScopeHash(hash) {
  const params = hashParams(hash);
  if (params.has("workspace")) return null;
  const session = params.get("session");
  if (!session) return null;
  params.set("workspace", session);
  const socket = params.get("socket");
  if (socket) params.set("wsocket", socketShort(socket));
  return hashString(params);
}

/// The scope reference a rail entry is written to the fragment as. The
/// socket is only spelled out when the session name exists on several tmux
/// servers, so ordinary links stay `#workspace=<name>`.
export function railEntryScopeRef(entry, entries = []) {
  if (!entry) return null;
  if (!entry.sessionName) return { workspace: entry.workspaceKey, socket: null };
  const ambiguous = entries.some((other) => other !== entry
    && other.sessionName === entry.sessionName && other.socket !== entry.socket);
  return { workspace: entry.sessionName, socket: ambiguous ? entry.socket : null };
}

/// The rail entry a scope reference names, or null.
export function findScopeEntry(entries, ref) {
  if (!ref?.workspace) return null;
  return findRailEntry(entries, { session: ref.workspace, socket: ref.socket });
}

/// Windows not tracked as Work inside `scope`, each with the navigator
/// selection (`#window=` + session/socket) its row opens.
export function untrackedWindowRows(runs, scope) {
  return unlinkedInScope(runs || [], scope).map((run) => ({
    run,
    session: run.session_name || run.execution?.session_id || "",
    window: run.execution?.window_id || "",
    socket: run.execution?.socket ? socketShort(run.execution.socket) : "",
  }));
}

/// Whether the "Windows not tracked as Work" section starts expanded: the
/// remembered choice when there is one, else collapsed while no Work is
/// tracked anywhere (the sessions navigator already lists those windows).
export function untrackedExpanded(stored, workCount) {
  if (stored === "1") return true;
  if (stored === "0") return false;
  return workCount > 0;
}
