// Fleet panel: this node plus the SSH hosts in `[fleet.hosts]`, from
// /api/fleet. Each host's `remote` snapshot carries the same agent and pane
// records the local /api/agents and /api/panes serve, so a remote host gets
// the same states and the same attention rule as the local one.
//
// Pure helpers (summary, ordering, markup) live here so they can be tested;
// main.js owns fetching, polling and the click wiring.

const WAITING = new Set(["waiting_input", "waiting_choice"]);
const STATE_RANK = { waiting_input: 0, waiting_choice: 0, error: 1, working: 2, starting: 3, idle: 4, stopped: 5 };

/** An agent needs the operator when it waits on input or a choice, or failed. */
export function needsAttention(agent) {
  return WAITING.has(agent?.state) || agent?.state === "error";
}

/** One host's card numbers. Offline hosts keep their last error, no counts. */
export function summarizeHost(host) {
  const remote = host?.remote || {};
  const agents = Array.isArray(remote.agents) ? remote.agents : [];
  const count = (pred) => agents.filter(pred).length;
  return {
    alias: host?.alias || "?",
    local: Boolean(host?.local),
    state: host?.state || "unknown",
    online: host?.state === "online",
    version: host?.muxa_version || "",
    latencyMs: Number.isFinite(host?.latency_ms) ? host.latency_ms : null,
    lastSeenAt: host?.last_seen_at || remote.observed_at || null,
    error: host?.error || "",
    agents: agents.length,
    panes: Array.isArray(remote.panes) ? remote.panes.length : 0,
    sessions: Array.isArray(remote.sessions) ? remote.sessions.length : 0,
    attention: count(needsAttention),
    working: count((a) => a.state === "working"),
    errors: count((a) => a.state === "error"),
  };
}

/** Local first, then online before the rest, then by name. */
export function orderHosts(hosts) {
  return [...(hosts || [])].sort((a, b) =>
    Number(Boolean(b.local)) - Number(Boolean(a.local))
    || Number(b.state === "online") - Number(a.state === "online")
    || String(a.alias || "").localeCompare(String(b.alias || "")));
}

/** What needs the operator first, then busy agents, then most recent. */
export function orderAgents(agents) {
  return [...(agents || [])].sort((a, b) =>
    (STATE_RANK[a.state] ?? 9) - (STATE_RANK[b.state] ?? 9)
    || String(b.last_activity_at || "").localeCompare(String(a.last_activity_at || "")));
}

/** Header numbers across every online host. */
export function fleetTotals(hosts) {
  const summaries = (hosts || []).map(summarizeHost);
  return {
    hosts: summaries.length,
    online: summaries.filter((s) => s.online).length,
    agents: summaries.reduce((n, s) => n + s.agents, 0),
    attention: summaries.reduce((n, s) => n + s.attention, 0),
  };
}

function agentLocation(agent) {
  const session = agent.tmux_session || "";
  const pane = agent.pane || "";
  return session && pane ? `${session} · ${pane}` : session || pane || "—";
}

function agentTask(agent) {
  return agent.ai_title || agent.recap || agent.last_prompt || agent.last_notification || "";
}

/**
 * Markup for the panel body. `selected` is the alias whose agents are open;
 * `esc` and `relTime` come from the dashboard so escaping and time labels
 * stay consistent with the rest of the page.
 */
export function renderFleet(hosts, { selected, esc, relTime }) {
  const ordered = orderHosts(hosts);
  if (!ordered.length) {
    return `<div class="empty-block">No fleet hosts. Add SSH hosts under <code>[fleet.hosts]</code> in muxa's config.toml.</div>`;
  }
  const cards = ordered.map((host) => {
    const s = summarizeHost(host);
    const detail = s.online
      ? `<span>${s.agents} agents</span><span>${s.panes} panes</span>${s.attention ? `<span class="fleet-attn">${s.attention} need you</span>` : ""}`
      : s.error
        ? `<span class="fleet-error" title="${esc(s.error)}">${esc(s.error)}</span>`
        : `<span class="fleet-pending">${esc(s.state)}…</span>`;
    const meta = [
      s.local ? "this node" : null,
      s.version ? `v${s.version}` : null,
      s.latencyMs !== null && !s.local ? `${s.latencyMs} ms` : null,
      s.lastSeenAt ? relTime(s.lastSeenAt) : null,
    ].filter(Boolean).map(esc).join(" · ");
    return `<button type="button" class="fleet-host state-${esc(s.state)}${s.alias === selected ? " selected" : ""}${s.attention ? " has-attention" : ""}"
        data-fleet-host="${esc(s.alias)}" aria-pressed="${s.alias === selected}"${s.online ? "" : " disabled"}>
      <span class="fleet-host-head"><span class="fleet-dot" aria-hidden="true"></span><b>${esc(s.alias)}</b><em>${esc(s.state)}</em></span>
      <span class="fleet-host-counts">${detail}</span>
      <span class="fleet-host-meta">${meta}</span>
    </button>`;
  }).join("");

  const open = ordered.find((h) => h.alias === selected && h.state === "online");
  let agents = "";
  if (open) {
    const rows = orderAgents(open.remote?.agents).map((agent) => `<tr>
        <td><span class="state-dot ${esc(agent.state)}"></span> ${esc(String(agent.state || "").replaceAll("_", " "))}</td>
        <td>${esc(agent.kind || "")}</td>
        <td class="mono">${esc(agentLocation(agent))}</td>
        <td class="fleet-task" title="${esc(agentTask(agent))}">${esc(agentTask(agent)) || "—"}</td>
        <td class="mono">${esc(relTime(agent.last_activity_at || agent.state_entered_at))}</td>
      </tr>`).join("");
    agents = `<div class="fleet-agents">
      <div class="fleet-agents-head"><b>${esc(open.alias)}</b><span>${esc(summarizeHost(open).agents)} agents · ${esc(summarizeHost(open).sessions)} sessions</span></div>
      <div class="table-wrap"><table class="data">
        <thead><tr><th>STATE</th><th>KIND</th><th>WHERE</th><th>TASK</th><th>ACTIVITY</th></tr></thead>
        <tbody>${rows || `<tr class="empty"><td colspan="5">no agents on this host</td></tr>`}</tbody>
      </table></div>
    </div>`;
  }
  return `<div class="fleet-hosts">${cards}</div>${agents}`;
}
