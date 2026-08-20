// Logical dashboard projection.
//
// The multiplexer topology remains the execution substrate, but the dashboard
// presents it as workspace -> work item -> participant.  Stable backend ids
// stay attached to every projection node so an expanded work item can still
// target the exact session/window/pane that produced it.

const ATTENTION_STATES = new Set(["waiting_input", "waiting_choice", "error"]);

// `agent_session_id` is the current public wire name. Keep the older local
// alias because older daemons and cached fixtures can still send `session_id`.
export function normalizeAgent(agent) {
  if (!agent || agent.session_id || !agent.agent_session_id) return agent;
  return { ...agent, session_id: agent.agent_session_id };
}

function hostForPane(pane) {
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
  return agent?.recap || agent?.ai_title || agent?.last_prompt || agent?.last_notification || "";
}

function summarizeWork(work) {
  const participants = work.panes.flatMap((pane) => pane.agent ? [pane.agent] : []);
  const working = participants.filter((agent) => agent.state === "working").length;
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
  work.summary = latestAgentSummary(latestAgent) ||
    work.panes.find((pane) => pane.title)?.title ||
    "No recent work summary";
  work.state = errors > 0
    ? "error"
    : waiting > 0
      ? "needs_attention"
      : working > 0
        ? "active"
        : participants.length > 0
          ? "available"
          : "untracked";
  return work;
}

function summarizeWorkspace(workspace) {
  workspace.works.sort(compareWorks);
  workspace.active = workspace.works.filter((work) => work.working > 0).length;
  workspace.attention = workspace.works.filter((work) => work.attention > 0).length;
  workspace.errors = workspace.works.filter((work) => work.errors > 0).length;
  workspace.agents = workspace.works.reduce((total, work) => total + work.participants.length, 0);
  workspace.panes = workspace.works.reduce((total, work) => total + work.panes.length, 0);
  workspace.latest = workspace.works.reduce(
    (latest, work) => work.latest > latest ? work.latest : latest,
    ""
  );
  return workspace;
}

export function compareWorks(left, right) {
  const priority = (work) => work.errors * 1000 + work.waiting * 100 + work.working * 10;
  return priority(right) - priority(left) ||
    (right.latest || "").localeCompare(left.latest || "") ||
    left.name.localeCompare(right.name);
}

export function buildWorkProjection(panes, agents) {
  const exactAgents = new Map();
  const agentsByPane = new Map();
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
    const sessionId = pane.session_id || pane.session || "unknown";
    const workspaceKey = key(["workspace", host, endpoint, sessionId]);
    let workspace = workspaces.get(workspaceKey);
    if (!workspace) {
      workspace = {
        key: workspaceKey,
        host,
        endpoint,
        sessionId,
        name: pane.session || sessionId,
        worksByKey: new Map(),
      };
      workspaces.set(workspaceKey, workspace);
    }

    const windowId = pane.window_id || pane.window_index || "unknown";
    const workKey = key([workspaceKey, "work", windowId]);
    let work = workspace.worksByKey.get(workKey);
    if (!work) {
      work = {
        key: workKey,
        workspaceKey,
        workspaceName: workspace.name,
        host,
        endpoint,
        sessionId,
        windowId,
        index: pane.window_index || "",
        name: pane.window_name || `window ${pane.window_index || windowId}`,
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
      return summarizeWork(work);
    });
    delete workspace.worksByKey;
    return summarizeWorkspace(workspace);
  });

  result.sort((left, right) =>
    right.errors - left.errors ||
    right.attention - left.attention ||
    right.active - left.active ||
    (right.latest || "").localeCompare(left.latest || "") ||
    left.name.localeCompare(right.name)
  );
  return result;
}

export function workNeedsAttention(work) {
  return work?.participants?.some((agent) => ATTENTION_STATES.has(agent.state)) || false;
}
