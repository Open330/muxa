// Work-centric projection for the dashboard control plane.
//
// Backend topology is execution detail. Managed muxa metadata is preferred;
// unmanaged legacy panes are grouped by repository cwd so a dozen ticket-like
// tmux sessions do not masquerade as a dozen unrelated workspaces.

const ATTENTION_STATES = new Set(["waiting_input", "waiting_choice", "error"]);
const GENERIC_WINDOW_NAMES = new Set([
  "bash", "claude", "codex", "fish", "node", "nu", "pwsh", "python", "sh", "shell", "zsh",
]);
const TICKET_PATTERN = /^(?:[a-z][a-z0-9]+-\d+|#\d+)$/i;

export const WORK_STAGES = ["auto", "queued", "in_progress", "review", "blocked", "done"];

// `agent_session_id` is the current public wire name. Keep the older local
// alias because older daemons and cached fixtures can still send `session_id`.
export function normalizeAgent(agent) {
  if (!agent || agent.session_id || !agent.agent_session_id) return agent;
  return { ...agent, session_id: agent.agent_session_id };
}

function hostForPane(pane) {
  if (pane?.host) return pane.host;
  const id = pane?.pane_id || "";
  if (id.startsWith("rmux:")) return "rmux";
  if (id.startsWith("zellij:")) return "zellij";
  if (id.startsWith("herdr:")) return "herdr";
  return "tmux";
}

function endpointFor(host, socket) {
  const raw = String(socket || host || "default");
  if (host === "rmux") return raw;
  return raw.split("/").pop() || raw;
}

function key(parts) {
  return parts.map((part) => encodeURIComponent(String(part || ""))).join("/");
}

function identityKey(identity) {
  return key([
    identity?.host,
    identity?.socket,
    identity?.session_id,
    identity?.window_id,
  ]);
}

function agentPaneKey(agent) {
  if (!agent?.pane) return null;
  const host = hostForPane({ pane_id: agent.pane });
  return key([host, endpointFor(host, agent.tmux_socket), agent.pane]);
}

function paneAgentKey(pane) {
  const host = hostForPane(pane);
  return key([host, endpointFor(host, pane.socket), pane.pane_id]);
}

function latestAgentSummary(agent) {
  return agent?.recap || agent?.ai_title || agent?.last_response ||
    agent?.last_prompt || agent?.last_notification || "";
}

function pathWorkspace(path) {
  const parts = String(path || "").split("/").filter(Boolean);
  return parts.at(-1) || "";
}

function logicalWorkspace(pane, host, endpoint) {
  const managed = pane.muxa?.managed_workspace && pane.muxa?.workspace_id;
  if (managed) {
    return {
      key: key(["workspace", host, endpoint, "managed", pane.muxa.workspace_id]),
      name: pane.muxa.workspace_id,
      source: "managed",
      cwd: pane.muxa.workspace_cwd || pane.current_path || "",
    };
  }
  const cwd = pane.muxa?.workspace_cwd || pane.muxa?.work_cwd || pane.current_path || "";
  const name = pathWorkspace(cwd);
  if (name) {
    return {
      key: key(["workspace", host, endpoint, "cwd", cwd]),
      name,
      source: "inferred",
      cwd,
    };
  }
  const sessionId = pane.session_id || pane.session || "unknown";
  return {
    key: key(["workspace", host, endpoint, "session", sessionId]),
    name: pane.session || sessionId,
    source: "session",
    cwd: "",
  };
}

function logicalTicket(pane) {
  if (pane.muxa?.managed_work && pane.muxa?.work_id) {
    return { id: pane.muxa.work_id, name: pane.muxa.work_id, source: "managed" };
  }
  const windowName = String(pane.window_name || "").trim();
  const sessionName = String(pane.session || "").trim();
  if (TICKET_PATTERN.test(sessionName) && GENERIC_WINDOW_NAMES.has(windowName.toLowerCase())) {
    return { id: sessionName.toUpperCase(), name: sessionName.toUpperCase(), source: "inferred" };
  }
  const fallback = windowName || `window ${pane.window_index || pane.window_id || "?"}`;
  return { id: fallback, name: fallback, source: "window" };
}

function metadataByIdentity(records) {
  const metadata = new Map();
  for (const record of records || []) {
    if (record?.key && record?.metadata) metadata.set(identityKey(record.key), record.metadata);
  }
  return metadata;
}

function effectiveStage(work) {
  const manual = work.metadata?.stage || "auto";
  if (manual === "done") return "done";
  if (manual === "review") return "review";
  if (manual === "blocked") return "attention";
  if (work.errors > 0 || work.waiting > 0) return "attention";
  if (manual === "in_progress" || work.working > 0) return "in_progress";
  return "queued";
}

function summarizeWork(work, metadata) {
  const participants = work.panes.flatMap((pane) => pane.agent ? [pane.agent] : []);
  const working = participants.filter((agent) => agent.state === "working" || agent.state === "starting").length;
  const waiting = participants.filter((agent) =>
    agent.state === "waiting_input" || agent.state === "waiting_choice"
  ).length;
  const errors = participants.filter((agent) => agent.state === "error").length;
  const latestAgent = [...participants].sort((left, right) =>
    (right.last_activity_at || "").localeCompare(left.last_activity_at || "")
  )[0];

  work.participants = participants;
  work.working = working;
  work.waiting = waiting;
  work.errors = errors;
  work.attention = waiting + errors;
  work.latest = latestAgent?.last_activity_at || "";
  work.metadata = metadata || null;
  work.title = metadata?.title || work.name;
  work.goal = metadata?.goal || "";
  work.nextAction = metadata?.next_action || "";
  work.summary = latestAgentSummary(latestAgent) ||
    work.panes.find((pane) => pane.muxa?.task)?.muxa.task ||
    work.panes.find((pane) => pane.title)?.title ||
    "No recent work signal";
  work.state = errors > 0
    ? "error"
    : waiting > 0
      ? "needs_attention"
      : working > 0
        ? "active"
        : participants.length > 0
          ? "available"
          : "untracked";
  work.stage = effectiveStage(work);
  return work;
}

function summarizeWorkspace(workspace) {
  workspace.works.sort(compareWorks);
  workspace.active = workspace.works.filter((work) => work.stage === "in_progress").length;
  workspace.attention = workspace.works.filter((work) => work.stage === "attention").length;
  workspace.review = workspace.works.filter((work) => work.stage === "review").length;
  workspace.done = workspace.works.filter((work) => work.stage === "done").length;
  workspace.errors = workspace.works.filter((work) => work.errors > 0).length;
  workspace.agents = workspace.works.reduce((total, work) => total + work.participants.length, 0);
  workspace.panes = workspace.works.reduce((total, work) => total + work.panes.length, 0);
  workspace.latest = workspace.works.reduce(
    (latest, work) => work.latest > latest ? work.latest : latest,
    ""
  );
  workspace.sessionNames = [...workspace.sessionNames].sort();
  return workspace;
}

export function compareWorks(left, right) {
  const stagePriority = { attention: 5, in_progress: 4, review: 3, queued: 2, done: 1 };
  return (stagePriority[right.stage] || 0) - (stagePriority[left.stage] || 0) ||
    (right.latest || "").localeCompare(left.latest || "") ||
    left.title.localeCompare(right.title);
}

export function buildWorkProjection(panes, agents, metadataRecords = []) {
  const exactAgents = new Map();
  const agentsByPane = new Map();
  const annotations = metadataByIdentity(metadataRecords);
  for (const agent of agents || []) {
    if (!agent?.pane) continue;
    const exactKey = agentPaneKey(agent);
    if (exactKey) exactAgents.set(exactKey, agent);
    if (!agentsByPane.has(agent.pane)) agentsByPane.set(agent.pane, []);
    agentsByPane.get(agent.pane).push(agent);
  }

  const workspaces = new Map();
  for (const pane of panes || []) {
    const host = hostForPane(pane);
    const endpoint = endpointFor(host, pane.socket);
    const logical = logicalWorkspace(pane, host, endpoint);
    let workspace = workspaces.get(logical.key);
    if (!workspace) {
      workspace = {
        key: logical.key,
        host,
        endpoint,
        name: logical.name,
        source: logical.source,
        cwd: logical.cwd,
        sessionNames: new Set(),
        worksByKey: new Map(),
      };
      workspaces.set(logical.key, workspace);
    }
    workspace.sessionNames.add(pane.session || pane.session_id || "unknown");

    const sessionId = pane.session_id || pane.session || "unknown";
    const windowId = pane.window_id || pane.window_index || "unknown";
    const identity = { host, socket: endpoint, session_id: sessionId, window_id: windowId };
    const workKey = key(["work", host, endpoint, sessionId, windowId]);
    let work = workspace.worksByKey.get(workKey);
    if (!work) {
      const ticket = logicalTicket(pane);
      work = {
        key: workKey,
        identity,
        workspaceKey: workspace.key,
        workspaceName: workspace.name,
        host,
        endpoint,
        sessionId,
        sessionName: pane.session || sessionId,
        windowId,
        index: pane.window_index || "",
        ticketId: ticket.id,
        name: ticket.name,
        source: ticket.source,
        managed: Boolean(pane.muxa?.managed_work),
        panes: [],
      };
      workspace.worksByKey.set(workKey, work);
    }

    const exact = exactAgents.get(paneAgentKey(pane));
    const candidates = agentsByPane.get(pane.pane_id) || [];
    const agent = exact || (candidates.length === 1 ? candidates[0] : null);
    work.panes.push({ ...pane, agent });
  }

  const result = [...workspaces.values()].map((workspace) => {
    workspace.works = [...workspace.worksByKey.values()].map((work) => {
      work.panes.sort((left, right) =>
        String(left.pane_index || "").localeCompare(String(right.pane_index || ""), undefined, { numeric: true })
      );
      return summarizeWork(work, annotations.get(identityKey(work.identity)));
    });
    delete workspace.worksByKey;
    return summarizeWorkspace(workspace);
  });

  result.sort((left, right) =>
    right.attention - left.attention ||
    right.active - left.active ||
    right.review - left.review ||
    (right.latest || "").localeCompare(left.latest || "") ||
    left.name.localeCompare(right.name)
  );
  return result;
}

export function workNeedsAttention(work) {
  return work?.stage === "attention" ||
    work?.participants?.some((agent) => ATTENTION_STATES.has(agent.state)) || false;
}

export function workIdentityKey(identity) {
  return identityKey(identity);
}
