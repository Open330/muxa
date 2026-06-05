// muxa dashboard frontend.
//
// Vanilla ES2022 modules — no build step. Loaded as
// <script type="module" src="/static/main.js"> from index.html.
//
// Runtime model:
//   * On boot, capture ?token=... from the URL into localStorage and
//     scrub it from the URL bar. localStorage persists across tab close
//     and browser restart, so the user only needs to paste the token once.
//   * Fetch /api/health to populate the version string and confirm the
//     token is good before opening the SSE.
//   * Fetch /api/agents and /api/panes to paint initial tables.
//   * Open a streaming POST-less fetch on /api/events and parse SSE
//     manually (EventSource can't carry an Authorization header).
//   * On SSE 'snapshot' events, replace the agent set wholesale; on
//     'transition' events, mutate one row by session_id; on 'lagged',
//     refetch /api/agents to resync.
//
// Filtering happens client-side: every render reads the current filter
// state and produces the entire <tbody> from scratch. The data sets
// are small (typically <100 rows total) so virtualization is overkill.

const TOKEN_KEY = "muxa.token";
const COLLAPSED_PANELS_KEY = "muxa.dashboard.collapsedPanels";
const COLLAPSED_TIMELINE_GROUPS_KEY = "muxa.dashboard.collapsedTimelineGroups";
const DATA_TAB_KEY = "muxa.dashboard.dataTab";
const PANES_REFETCH_INTERVAL_MS = 5000;
const TIMELINE_REFETCH_INTERVAL_MS = 5000;

const AGENT_STATES = ["working", "waiting_input", "waiting_choice", "idle", "starting", "error", "stopped"];
const AGENT_KINDS = ["claude_code", "codex", "gemini_cli", "opencode", "unknown"];
const TIMELINE_RANGES = ["24h", "today", "7d"];

// ── Token bootstrap ────────────────────────────────────────────────

function bootstrapToken() {
  const url = new URL(window.location.href);
  const fromUrl = url.searchParams.get("token");
  if (fromUrl) {
    localStorage.setItem(TOKEN_KEY, fromUrl);
    url.searchParams.delete("token");
    window.history.replaceState({}, "", url.toString());
  }
}

function authHeaders() {
  const token = localStorage.getItem(TOKEN_KEY);
  return token ? { Authorization: `Bearer ${token}` } : {};
}

// ── HTTP helpers ───────────────────────────────────────────────────

async function jsonFetch(path) {
  const resp = await fetch(path, { headers: authHeaders() });
  if (resp.status === 401) {
    setConnectionStatus("dead", "401 — bad or missing token");
    throw new Error("unauthorized");
  }
  if (!resp.ok) {
    setConnectionStatus("dead", `${path} → ${resp.status}`);
    throw new Error(`${path} → ${resp.status}`);
  }
  return resp.json();
}

// ── SSE parser (manual; EventSource can't set headers) ────────────

async function streamEvents(onEvent, onLagged) {
  let backoffMs = 500;
  while (true) {
    try {
      setConnectionStatus("connecting", "connecting…");
      const resp = await fetch("/api/events", {
        headers: { ...authHeaders(), Accept: "text/event-stream" },
      });
      if (resp.status === 401) {
        setConnectionStatus("dead", "401 — bad or missing token");
        return;
      }
      if (!resp.ok || !resp.body) {
        throw new Error(`SSE: ${resp.status}`);
      }
      setConnectionStatus("live", "live");
      backoffMs = 500;

      const reader = resp.body.getReader();
      const decoder = new TextDecoder();
      let buf = "";
      let event = "";
      let data = "";

      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let nl;
        while ((nl = buf.indexOf("\n")) !== -1) {
          const line = buf.slice(0, nl).replace(/\r$/, "");
          buf = buf.slice(nl + 1);
          if (line === "") {
            if (event && data) {
              try {
                const payload = JSON.parse(data);
                if (event === "lagged") {
                  onLagged(payload);
                } else {
                  onEvent(event, payload);
                }
              } catch (_) {
                // Server might emit lagged with a non-JSON count;
                // tolerate.
                if (event === "lagged") onLagged({ raw: data });
              }
            }
            event = "";
            data = "";
          } else if (line.startsWith("event: ")) {
            event = line.slice(7);
          } else if (line.startsWith("data: ")) {
            data += line.slice(6);
          }
          // Other SSE fields (id, retry) ignored — we don't use them.
        }
      }
    } catch (e) {
      setConnectionStatus("degraded", `reconnecting in ${backoffMs}ms`);
      await new Promise((r) => setTimeout(r, backoffMs));
      backoffMs = Math.min(backoffMs * 2, 10000);
    }
  }
}

// ── Connection indicator ──────────────────────────────────────────

const dom = {
  conn: document.getElementById("conn"),
  connLabel: document.getElementById("conn-label"),
  counts: document.getElementById("counts"),
  version: document.getElementById("version"),
  sessionList: document.getElementById("session-list"),
  sessionsMeta: document.getElementById("sessions-meta"),
  showAllSessions: document.getElementById("show-all-sessions"),
  metricWorking: document.getElementById("metric-working"),
  metricWaiting: document.getElementById("metric-waiting"),
  metricErrors: document.getElementById("metric-errors"),
  metricHuman: document.getElementById("metric-human"),
  metricTmux: document.getElementById("metric-tmux"),
  agentsBody: document.getElementById("agents-tbody"),
  panesBody: document.getElementById("panes-tbody"),
  timelineAxis: document.getElementById("timeline-axis"),
  timelineBody: document.getElementById("timeline-body"),
  timelineRangeChips: document.getElementById("timeline-range-chips"),
  timelineSession: document.getElementById("timeline-session"),
  timelineMeta: document.getElementById("timeline-meta"),
  agentStateChips: document.getElementById("agent-state-chips"),
  agentKindChips: document.getElementById("agent-kind-chips"),
  agentsMeta: document.getElementById("agents-meta"),
  paneSocketChips: document.getElementById("pane-socket-chips"),
  panesMeta: document.getElementById("panes-meta"),
  dataMeta: document.getElementById("data-meta"),
  dataTabs: document.getElementById("data-tabs"),
  agentsTab: document.getElementById("agents-tab"),
  panesTab: document.getElementById("panes-tab"),
  inspectorBody: document.getElementById("inspector-body"),
  inspectorMeta: document.getElementById("inspector-meta"),
  toast: document.getElementById("toast"),
};

function setConnectionStatus(cls, label) {
  dom.conn.className = `conn ${cls}`;
  dom.connLabel.textContent = label;
}

let toastTimer = null;
function showToast(msg) {
  dom.toast.textContent = msg;
  dom.toast.hidden = false;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    dom.toast.hidden = true;
  }, 1800);
}

function loadSet(key) {
  try {
    const raw = localStorage.getItem(key);
    const vals = raw ? JSON.parse(raw) : [];
    return new Set(Array.isArray(vals) ? vals : []);
  } catch (_) {
    return new Set();
  }
}

function saveSet(key, values) {
  try {
    localStorage.setItem(key, JSON.stringify([...values]));
  } catch (_) {
    // Keep the UI interactive even when storage is blocked.
  }
}

function loadValue(key, fallback) {
  try {
    return localStorage.getItem(key) || fallback;
  } catch (_) {
    return fallback;
  }
}

function saveValue(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch (_) {
    // Ignore storage failures; the visible state is already updated.
  }
}

// ── State ─────────────────────────────────────────────────────────

const store = {
  agents: new Map(), // session_id -> Agent
  panes: [], // PaneSummary[]
  paneErrors: [], // ScanError[]
  timeline: null, // TimelineDocument
  timelineSessions: new Set(),
  ui: {
    collapsedPanels: loadSet(COLLAPSED_PANELS_KEY),
    collapsedTimelineGroups: loadSet(COLLAPSED_TIMELINE_GROUPS_KEY),
    activeTab: loadValue(DATA_TAB_KEY, "agents"),
    selectedSegment: null,
  },
  filters: {
    agentStates: new Set(AGENT_STATES),
    agentKinds: new Set(AGENT_KINDS),
    paneSockets: new Set(), // populated dynamically
    timelineRange: "24h",
    timelineSession: "",
  },
};

// ── Helpers ───────────────────────────────────────────────────────

// Resolve a raw tmux pane id (e.g. "%1645") to a richer label
// "session:window.pane" by cross-referencing the global pane scan. Falls
// back to the raw id when no scan match is available (e.g. before the
// first /api/panes response, or when the pane lives on an unreadable
// socket).
function resolvePaneLabel(paneId) {
  if (!paneId) return "—";
  const match = store.panes.find((p) => p.pane_id === paneId);
  if (!match) return paneId;
  return `${match.session}:${match.window_index}.${paneId}`;
}

function sessionForAgent(agent) {
  if (!agent) return "";
  const paneMatch = agent.pane ? store.panes.find((p) => p.pane_id === agent.pane) : null;
  if (paneMatch?.session) return paneMatch.session;
  const laneMatch = (store.timeline?.lanes || []).find(
    (lane) => lane.kind === "agent" && lane.session_id === agent.session_id
  );
  return laneMatch?.session_name || agent.session_id || "";
}

function selectedSession() {
  return store.filters.timelineSession || "";
}

function agentMatchesSelectedSession(agent) {
  const session = selectedSession();
  if (!session) return true;
  return sessionForAgent(agent) === session || agent.session_id === session;
}

function paneMatchesSelectedSession(pane) {
  const session = selectedSession();
  return !session || pane.session === session;
}

function laneMatchesSelectedSession(lane) {
  const session = selectedSession();
  return !session || lane.session_name === session || lane.session_id === session;
}

function setSelectedSession(session) {
  store.filters.timelineSession = session || "";
  store.ui.selectedSegment = null;
  if (dom.timelineSession) dom.timelineSession.value = store.filters.timelineSession;
  renderTimeline();
  renderAgents();
  renderPanes();
  renderInspector();
}

// Copy text to clipboard. Uses the async Clipboard API when available
// (HTTPS or loopback) and falls back to the legacy execCommand path so
// the dashboard remains usable when reached over plain HTTP via a
// hostname like june.rtzr.ai.
async function copyToClipboard(text) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch (_) {
      // permission denied / blocked — fall through to legacy path
    }
  }
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.top = "0";
  ta.style.left = "0";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch (_) {
    ok = false;
  }
  document.body.removeChild(ta);
  return ok;
}

// ── Rendering ─────────────────────────────────────────────────────

function renderCounts() {
  const a = store.agents.size;
  const p = store.panes.length;
  const e = store.paneErrors.length;
  let s = `${a} agent${a === 1 ? "" : "s"} · ${p} pane${p === 1 ? "" : "s"}`;
  if (e > 0) s += ` · ${e} scan error${e === 1 ? "" : "s"}`;
  dom.counts.textContent = s;
  renderOverview();
  renderSessionSidebar();
  renderInspector();
}

function renderOverview() {
  const agents = [...store.agents.values()];
  const working = agents.filter((a) => a.state === "working").length;
  const waiting = agents.filter((a) => a.state === "waiting_input" || a.state === "waiting_choice").length;
  const errors = agents.filter((a) => a.state === "error").length;
  const lanes = store.timeline?.lanes || [];
  const totals = store.timeline?.totals || {};
  dom.metricWorking.textContent = String(working);
  dom.metricWaiting.textContent = String(waiting);
  dom.metricErrors.textContent = String(errors);
  dom.metricHuman.textContent = formatDuration(humanPresenceSecs(lanes));
  dom.metricTmux.textContent = formatDuration(totals.foreground_secs || 0);
}

function renderSessionSidebar() {
  const summaries = buildSessionSummaries();
  dom.sessionsMeta.textContent = `${summaries.length} total`;
  if (summaries.length === 0) {
    dom.sessionList.innerHTML = `<div class="empty-block">no sessions</div>`;
    return;
  }
  const active = selectedSession();
  const allAgents = store.agents.size;
  const allWaiting = [...store.agents.values()].filter((a) =>
    a.state === "waiting_input" || a.state === "waiting_choice" || a.state === "error"
  ).length;
  const allActive = active === "";
  const allRow = `<button class="session-row${allActive ? " active" : ""}" type="button" data-session="">
    <span class="session-dot ${allWaiting > 0 ? "waiting" : "working"}"></span>
    <span class="session-main">
      <span class="session-name">all sessions</span>
      <span class="session-detail">${allAgents} agents · ${store.panes.length} panes</span>
    </span>
    <span class="session-score">${allWaiting}</span>
  </button>`;
  const rows = summaries.map((s) => {
    const stateClass = s.errors > 0 ? "error" : s.waiting > 0 ? "waiting" : s.working > 0 ? "working" : "idle";
    return `<button class="session-row${s.label === active ? " active" : ""}" type="button" data-session="${esc(s.label)}">
      <span class="session-dot ${stateClass}"></span>
      <span class="session-main">
        <span class="session-name">${esc(s.label)}</span>
        <span class="session-detail">${esc(sessionSummaryLine(s))}</span>
      </span>
      <span class="session-score">${esc(sessionScoreLabel(s))}</span>
    </button>`;
  }).join("");
  dom.sessionList.innerHTML = allRow + rows;
  dom.sessionList.querySelectorAll("[data-session]").forEach((row) => {
    row.addEventListener("click", () => setSelectedSession(row.getAttribute("data-session") || ""));
  });
}

function buildSessionSummaries() {
  const map = new Map();
  const ensure = (label) => {
    const key = label || "no session";
    if (!map.has(key)) {
      map.set(key, {
        label: key,
        agents: 0,
        panes: 0,
        lanes: 0,
        working: 0,
        waiting: 0,
        errors: 0,
        latest: "",
        totals: emptyTimelineTotals(),
        human_presence_secs: 0,
      });
    }
    return map.get(key);
  };

  for (const lane of store.timeline?.lanes || []) {
    const label = lane.session_name || lane.session_id || "no session";
    const s = ensure(label);
    s.lanes += 1;
    addTimelineTotals(s.totals, lane.totals || {});
  }
  for (const agent of store.agents.values()) {
    const s = ensure(sessionForAgent(agent));
    s.agents += 1;
    if (agent.state === "working") s.working += 1;
    if (agent.state === "waiting_input" || agent.state === "waiting_choice") s.waiting += 1;
    if (agent.state === "error") s.errors += 1;
    if ((agent.last_activity_at || "") > s.latest) s.latest = agent.last_activity_at || "";
  }
  for (const pane of store.panes) {
    ensure(pane.session).panes += 1;
  }
  for (const summary of map.values()) {
    summary.human_presence_secs = humanPresenceSecs(
      (store.timeline?.lanes || []).filter((lane) =>
        (lane.session_name || lane.session_id || "no session") === summary.label
      )
    );
  }

  return [...map.values()].sort((a, b) => {
    const scoreA = a.errors * 1000 + a.waiting * 100 + a.working * 10;
    const scoreB = b.errors * 1000 + b.waiting * 100 + b.working * 10;
    if (scoreA !== scoreB) return scoreB - scoreA;
    if (a.latest !== b.latest) return b.latest.localeCompare(a.latest);
    return a.label.localeCompare(b.label);
  });
}

function sessionSummaryLine(s) {
  const parts = [];
  if (s.working) parts.push(`${s.working} work`);
  if (s.waiting) parts.push(`${s.waiting} wait`);
  if (s.errors) parts.push(`${s.errors} err`);
  if (s.agents) parts.push(`${s.agents} agents`);
  if (s.panes) parts.push(`${s.panes} panes`);
  return parts.slice(0, 3).join(" · ") || `${s.lanes} lanes`;
}

function sessionScoreLabel(s) {
  if (s.errors > 0) return String(s.errors);
  if (s.waiting > 0) return String(s.waiting);
  if (s.working > 0) return String(s.working);
  return "·";
}

function renderTimeline() {
  const doc = store.timeline;
  if (!doc) {
    dom.timelineBody.innerHTML = `<div class="timeline-empty">loading…</div>`;
    dom.timelineAxis.innerHTML = "";
    dom.timelineMeta.textContent = "loading";
    renderOverview();
    renderSessionSidebar();
    renderInspector();
    return;
  }
  renderTimelineSessionOptions(doc);
  const start = Date.parse(doc.window_started_at);
  const end = Date.parse(doc.window_ended_at);
  if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) {
    dom.timelineBody.innerHTML = `<div class="timeline-empty">timeline window is invalid</div>`;
    dom.timelineAxis.innerHTML = "";
    dom.timelineMeta.textContent = "invalid window";
    renderOverview();
    renderSessionSidebar();
    renderInspector();
    return;
  }
  renderTimelineAxis(start, end);

  const lanes = (doc.lanes || []).filter(laneMatchesSelectedSession);
  const groups = groupTimelineLanesBySession(lanes);
  const scope = selectedSession() ? `${selectedSession()} · ` : "";
  dom.timelineMeta.textContent = `${scope}${groups.length} session${groups.length === 1 ? "" : "s"} · ${lanes.length} lane${lanes.length === 1 ? "" : "s"}`;
  if (lanes.length === 0) {
    const note = (doc.notes || [])[0] || "no timeline intervals in this view";
    dom.timelineBody.innerHTML = `<div class="timeline-empty">${esc(note)}</div>`;
    renderOverview();
    renderSessionSidebar();
    renderInspector();
    return;
  }

  dom.timelineBody.innerHTML = groups
    .map((group) => {
      const collapsed = store.ui.collapsedTimelineGroups.has(group.key);
      const groupHeader = `<button class="timeline-group-header" type="button" data-timeline-group="${esc(group.key)}" aria-expanded="${collapsed ? "false" : "true"}">
        <span class="timeline-group-toggle" aria-hidden="true">${collapsed ? "›" : "⌄"}</span>
        <div class="timeline-group-label">
          <span>${esc(group.label)}</span>
          <small>${esc(group.lanes.length)} lane${group.lanes.length === 1 ? "" : "s"} · ${esc(laneTotalsLabel(group.totals))}</small>
        </div>
        <div class="timeline-group-rule"></div>
      </button>`;
      const laneRows = group.lanes
        .map((lane) => renderTimelineLane(lane, start, end, true))
        .join("");
      return `<div class="timeline-group${collapsed ? " collapsed" : ""}">${groupHeader}<div class="timeline-group-lanes">${laneRows}</div></div>`;
    })
    .join("");
  bindTimelineGroupToggles();
  bindTimelineSegmentClicks();
  renderOverview();
  renderSessionSidebar();
  renderInspector();
}

function renderTimelineLane(lane, start, end, grouped) {
  const segments = (lane.intervals || [])
    .map((interval) => renderTimelineSegment(interval, start, end))
    .join("");
  const totals = laneTotalsLabel(lane.totals || {});
  const label = grouped ? shortTimelineLaneLabel(lane) : lane.label;
  return `<div class="timeline-lane${grouped ? " grouped" : ""}">
    <div class="timeline-label" title="${esc(lane.label)}">
      <span>${esc(label)}</span>
      <small>${esc(totals)}</small>
    </div>
    <div class="timeline-track">${segments}</div>
  </div>`;
}

function groupTimelineLanesBySession(lanes) {
  const groups = new Map();
  for (const lane of lanes) {
    const label = lane.session_name || lane.session_id || "no session";
    const key = label === "no session" ? "zzzz:no-session" : `session:${label.toLowerCase()}`;
    if (!groups.has(key)) {
      groups.set(key, {
        key,
        label,
        lanes: [],
        totals: emptyTimelineTotals(),
        human_presence_secs: 0,
      });
    }
    const group = groups.get(key);
    group.lanes.push(lane);
    addTimelineTotals(group.totals, lane.totals || {});
  }
  return [...groups.values()]
    .sort((a, b) => a.key.localeCompare(b.key))
    .map((group) => {
      group.lanes.sort(compareTimelineLanesInGroup);
      group.human_presence_secs = humanPresenceSecs(group.lanes);
      group.totals.human_presence_secs = group.human_presence_secs;
      return group;
    });
}

function shortTimelineLaneLabel(lane) {
  if (lane.kind === "agent") return `  ${lane.agent_kind || "agent"}`;
  if (lane.kind === "human") return "  interaction";
  if (lane.kind === "tmux") return "  tmux";
  return `  ${lane.label || "lane"}`;
}

function compareTimelineLanesInGroup(a, b) {
  const rank = timelineLaneRank(a.kind) - timelineLaneRank(b.kind);
  if (rank !== 0) return rank;
  return (shortTimelineLaneLabel(a) || "").localeCompare(shortTimelineLaneLabel(b) || "");
}

function bindTimelineGroupToggles() {
  dom.timelineBody.querySelectorAll("[data-timeline-group]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const key = btn.getAttribute("data-timeline-group");
      if (!key) return;
      if (store.ui.collapsedTimelineGroups.has(key)) {
        store.ui.collapsedTimelineGroups.delete(key);
      } else {
        store.ui.collapsedTimelineGroups.add(key);
      }
      saveSet(COLLAPSED_TIMELINE_GROUPS_KEY, store.ui.collapsedTimelineGroups);
      renderTimeline();
    });
  });
}

function bindTimelineSegmentClicks() {
  dom.timelineBody.querySelectorAll("[data-segment]").forEach((el) => {
    el.addEventListener("click", (event) => {
      event.stopPropagation();
      store.ui.selectedSegment = {
        detail: el.getAttribute("data-detail") || "interval",
        source: el.getAttribute("data-source") || "",
        state: el.getAttribute("data-state") || "",
        started_at: el.getAttribute("data-started") || "",
        ended_at: el.getAttribute("data-ended") || "",
        duration_secs: Number(el.getAttribute("data-duration") || 0),
        session: el.getAttribute("data-session") || "",
        pane: el.getAttribute("data-pane") || "",
      };
      renderInspector();
    });
  });
}

function timelineLaneRank(kind) {
  if (kind === "agent") return 0;
  if (kind === "human") return 1;
  if (kind === "tmux") return 2;
  return 3;
}

function emptyTimelineTotals() {
  return {
    working_secs: 0,
    waiting_secs: 0,
    error_secs: 0,
    idle_secs: 0,
    starting_secs: 0,
    stopped_secs: 0,
    human_secs: 0,
    foreground_secs: 0,
    human_presence_secs: 0,
  };
}

function addTimelineTotals(total, next) {
  for (const key of Object.keys(total)) {
    total[key] += next[key] || 0;
  }
}

function humanPresenceSecs(lanes) {
  const intervals = [];
  for (const lane of lanes || []) {
    for (const interval of lane.intervals || []) {
      if (!isHumanPresenceInterval(interval)) continue;
      const startedAt = Date.parse(interval.started_at);
      const endedAt = Date.parse(interval.ended_at);
      if (!Number.isFinite(startedAt) || !Number.isFinite(endedAt) || endedAt <= startedAt) {
        continue;
      }
      intervals.push({
        scope: intervalScopeKey(interval, lane),
        startedAt,
        endedAt,
      });
    }
  }
  return sumMergedScopedMs(intervals);
}

function isHumanPresenceInterval(interval) {
  return interval.source === "session_foreground" || interval.source === "human_interaction";
}

function intervalScopeKey(interval, lane) {
  if (interval.session_name) return `session:${interval.session_name}`;
  if (lane.session_name) return `session:${lane.session_name}`;
  if (interval.pane) return `pane:${interval.pane}`;
  if (interval.session_id) return `session:${interval.session_id}`;
  if (lane.session_id) return `session:${lane.session_id}`;
  return `lane:${lane.id || lane.label || "unknown"}`;
}

function sumMergedScopedMs(intervals) {
  const byScope = new Map();
  for (const interval of intervals) {
    if (!byScope.has(interval.scope)) byScope.set(interval.scope, []);
    byScope.get(interval.scope).push([interval.startedAt, interval.endedAt]);
  }

  let totalMs = 0;
  for (const ranges of byScope.values()) {
    ranges.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
    let currentStart = null;
    let currentEnd = null;
    for (const [start, end] of ranges) {
      if (currentStart === null) {
        currentStart = start;
        currentEnd = end;
      } else if (start <= currentEnd) {
        currentEnd = Math.max(currentEnd, end);
      } else {
        totalMs += currentEnd - currentStart;
        currentStart = start;
        currentEnd = end;
      }
    }
    if (currentStart !== null) totalMs += currentEnd - currentStart;
  }
  return Math.floor(totalMs / 1000);
}

function renderTimelineAxis(start, end) {
  const ticks = 5;
  const html = Array.from({ length: ticks + 1 }, (_, i) => {
    const pct = (i / ticks) * 100;
    const at = new Date(start + (end - start) * (i / ticks));
    return `<span style="left:${pct}%">${esc(timeLabel(at))}</span>`;
  }).join("");
  dom.timelineAxis.innerHTML = `<div class="timeline-axis-spacer"></div><div class="timeline-axis-track">${html}</div>`;
}

function renderTimelineSegment(interval, start, end) {
  const s = Math.max(start, Date.parse(interval.started_at));
  const e = Math.min(end, Date.parse(interval.ended_at));
  if (!Number.isFinite(s) || !Number.isFinite(e) || e <= s) return "";
  const left = ((s - start) / (end - start)) * 100;
  const width = Math.max(0.5, ((e - s) / (end - start)) * 100);
  const cls = timelineSegmentClass(interval);
  const title = [
    interval.detail || timelineSegmentLabel(interval),
    `${dateTimeLabel(new Date(s))} → ${dateTimeLabel(new Date(e))}`,
    formatDuration(interval.duration_secs || 0),
    interval.open ? "open" : "",
    interval.session_name ? `session ${interval.session_name}` : "",
    interval.pane ? `pane ${interval.pane}` : "",
  ].filter(Boolean).join(" · ");
  return `<span class="timeline-segment ${esc(cls)}${interval.open ? " open" : ""}"
    style="left:${left}%;width:${width}%"
    title="${esc(title)}"
    data-segment="1"
    data-detail="${esc(interval.detail || timelineSegmentLabel(interval))}"
    data-source="${esc(interval.source || "")}"
    data-state="${esc(interval.state || "")}"
    data-started="${esc(interval.started_at || "")}"
    data-ended="${esc(interval.ended_at || "")}"
    data-duration="${esc(interval.duration_secs || 0)}"
    data-session="${esc(interval.session_name || interval.session_id || "")}"
    data-pane="${esc(interval.pane || "")}"></span>`;
}

function timelineSegmentClass(interval) {
  if (interval.source === "human_interaction") return "source-human";
  if (interval.source === "session_foreground") return "source-tmux";
  switch (interval.state) {
    case "working": return "state-working";
    case "waiting_input":
    case "waiting_choice": return "state-waiting";
    case "error": return "state-error";
    case "starting": return "state-starting";
    case "idle":
    case "stopped": return "state-idle";
    default: return "state-idle";
  }
}

function timelineSegmentLabel(interval) {
  if (interval.source === "human_interaction") return "interaction";
  if (interval.source === "session_foreground") return "tmux foreground";
  return interval.state || "agent";
}

function laneTotalsLabel(totals) {
  const parts = [
    ["work", totals.working_secs],
    ["wait", totals.waiting_secs],
    ["err", totals.error_secs],
    ["interact", totals.human_secs],
    ["tmux", totals.foreground_secs],
  ].filter(([, secs]) => secs > 0);
  if (parts.length === 0) return "—";
  return parts.slice(0, 2).map(([label, secs]) => `${label} ${formatDuration(secs)}`).join(" · ");
}

function renderTimelineSessionOptions(doc) {
  for (const p of store.panes || []) {
    if (p.session) store.timelineSessions.add(p.session);
  }
  for (const lane of doc.lanes || []) {
    if (lane.session_name) store.timelineSessions.add(lane.session_name);
    if (lane.session_id) store.timelineSessions.add(lane.session_id);
  }
  const current = store.filters.timelineSession;
  const options = [`<option value="">all sessions</option>`]
    .concat([...store.timelineSessions.values()].sort().map((s) =>
      `<option value="${esc(s)}"${s === current ? " selected" : ""}>${esc(s)}</option>`
    ));
  dom.timelineSession.innerHTML = options.join("");
}

function renderAgents() {
  const rows = [...store.agents.values()].filter(
    (a) =>
      agentMatchesSelectedSession(a) &&
      store.filters.agentStates.has(a.state) &&
      store.filters.agentKinds.has(a.kind)
  );
  rows.sort((x, y) => (y.last_activity_at || "").localeCompare(x.last_activity_at || ""));
  dom.agentsMeta.textContent = selectedSession()
    ? `${rows.length} in ${selectedSession()}`
    : `${rows.length} shown · ${store.agents.size} tracked`;
  renderDataMeta();

  if (rows.length === 0) {
    dom.agentsBody.innerHTML = `<tr class="empty"><td colspan="9">no matching agents</td></tr>`;
    return;
  }

  const now = Date.now();
  const html = rows
    .map((a) => {
      const pane = resolvePaneLabel(a.pane);
      const ctx = a.context_used_pct == null ? "—" : `${Math.round(a.context_used_pct)}%`;
      const cost = a.cost_usd == null ? "—" : `$${a.cost_usd.toFixed(2)}`;
      const limits = renderLimitsCell(a, now);
      const prompt = (a.last_prompt || "—").split("\n")[0].slice(0, 120);
      const activity = relTime(a.last_activity_at);
      return `<tr>
        <td>${esc(pane)}</td>
        <td>${esc(a.kind)}</td>
        <td><span class="state-pill ${esc(a.state)}">${esc(a.state)}</span></td>
        <td>${esc(a.model || "—")}</td>
        <td class="num">${esc(ctx)}</td>
        <td class="num">${esc(cost)}</td>
        <td class="${limits.cls}" title="${esc(limits.title)}">${esc(limits.text)}</td>
        <td>${esc(prompt)}</td>
        <td>${esc(activity)}</td>
      </tr>`;
    })
    .join("");
  dom.agentsBody.innerHTML = html;
}

/// Build the LIMITS cell for one agent row. Mirrors the CLI watch
/// renderer's three-state rule:
///   - red `⛔ <scope> in 2h 14m` — currently capped (rate_limit_scope set,
///     and either no reset known or reset is in the future).
///   - yellow `5h 84%` — utilisation ≥ 80% on either window (warning).
///   - default-or-dim `5h 31%` / `—` otherwise.
/// Returns {text, cls, title} so the caller can splice into the row.
function renderLimitsCell(a, now) {
  if (isCurrentlyCapped(a, now)) {
    const scope = scopePrefix(a.rate_limit_scope);
    const until = a.rate_limited_until ? Date.parse(a.rate_limited_until) : null;
    const tail = until && until > now
      ? formatRelativeUntil(until - now)
      : "capped";
    const body = scope ? `${scope} ${tail}` : (tail === "capped" ? "rate limited" : tail);
    return {
      text: `⛔ ${body}`,
      cls: "limits limits-cap",
      title: a.last_notification || "rate limited",
    };
  }
  const p5 = a.rate_limit_5h_pct;
  const p7 = a.rate_limit_7d_pct;
  const both = p5 != null && p7 != null;
  let chosen = null;
  let label = null;
  if (both) {
    if (p7 > p5) { chosen = p7; label = "7d"; } else { chosen = p5; label = "5h"; }
  } else if (p5 != null) {
    chosen = p5; label = "5h";
  } else if (p7 != null) {
    chosen = p7; label = "7d";
  }
  if (chosen == null) {
    return { text: "—", cls: "limits limits-empty", title: "" };
  }
  const cls = chosen >= 80 ? "limits limits-warn" : "limits";
  return {
    text: `${label} ${Math.round(chosen)}%`,
    cls,
    title: "",
  };
}

function isCurrentlyCapped(a, now) {
  if (!a.rate_limit_scope) return false;
  if (!a.rate_limited_until) return true;
  return Date.parse(a.rate_limited_until) > now;
}

function scopePrefix(scope) {
  switch (scope) {
    case "five_hour": return "5h";
    case "seven_day": return "7d";
    default: return "";
  }
}

/// Render millisecond gap as "in 2h 14m" / "in 47m" / "in 30s".
function formatRelativeUntil(ms) {
  const totalSecs = Math.max(0, Math.floor(ms / 1000));
  const hours = Math.floor(totalSecs / 3600);
  const minutes = Math.floor((totalSecs % 3600) / 60);
  const seconds = totalSecs % 60;
  if (hours > 0) return `in ${hours}h ${String(minutes).padStart(2, "0")}m`;
  if (minutes > 0) return `in ${minutes}m`;
  return `in ${seconds}s`;
}

function renderPanes() {
  // Refresh socket-filter chips from the current dataset.
  const socketsInData = new Set(store.panes.map((p) => p.socket));
  if (store.filters.paneSockets.size === 0) {
    // first paint — start fully on
    socketsInData.forEach((s) => store.filters.paneSockets.add(s));
  }
  // Drop any filter sockets that no longer exist.
  for (const s of [...store.filters.paneSockets]) {
    if (!socketsInData.has(s)) store.filters.paneSockets.delete(s);
  }
  renderSocketChips([...socketsInData].sort());

  const rows = store.panes.filter((p) => store.filters.paneSockets.has(p.socket) && paneMatchesSelectedSession(p));
  dom.panesMeta.textContent = store.paneErrors.length > 0
    ? `${rows.length} shown · ${store.paneErrors.length} errors`
    : `${rows.length} shown`;
  renderDataMeta();
  if (rows.length === 0 && store.paneErrors.length === 0) {
    dom.panesBody.innerHTML = `<tr class="empty"><td colspan="7">no panes (no tmux servers, or all filtered out)</td></tr>`;
    return;
  }

  const html = rows
    .map((p) => {
      const sockShort = p.socket.split("/").pop() || p.socket;
      return `<tr>
        <td title="${esc(p.socket)}">${esc(sockShort)}</td>
        <td>${esc(p.session)}</td>
        <td>${esc(p.window_index)}</td>
        <td>${esc(p.pane_id)}</td>
        <td>${esc(p.current_command)}</td>
        <td>${esc(p.title)}</td>
        <td><button class="attach-btn" data-cmd="${esc(p.attach_command)}">copy attach</button></td>
      </tr>`;
    })
    .join("");
  const errHtml = store.paneErrors
    .map(
      (e) =>
        `<tr class="lagged"><td colspan="7">scan error on ${esc(e.socket)}: ${esc(e.message)}</td></tr>`
    )
    .join("");
  dom.panesBody.innerHTML = errHtml + html;

  dom.panesBody.querySelectorAll(".attach-btn").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const cmd = btn.getAttribute("data-cmd");
      const ok = await copyToClipboard(cmd);
      showToast(ok ? "copied attach command" : "clipboard blocked — copy manually");
    });
  });
}

function renderDataMeta() {
  const scope = selectedSession() || "all sessions";
  dom.dataMeta.textContent = `${store.ui.activeTab} · ${scope}`;
}

function renderSocketChips(sockets) {
  const html = sockets
    .map((s) => {
      const short = s.split("/").pop() || s;
      const active = store.filters.paneSockets.has(s);
      return `<button class="chip${active ? " active" : ""}" data-socket="${esc(s)}" title="${esc(s)}">${esc(short)}</button>`;
    })
    .join("");
  dom.paneSocketChips.innerHTML = html;
  dom.paneSocketChips.querySelectorAll(".chip").forEach((chip) => {
    chip.addEventListener("click", () => {
      const s = chip.getAttribute("data-socket");
      if (store.filters.paneSockets.has(s)) {
        store.filters.paneSockets.delete(s);
      } else {
        store.filters.paneSockets.add(s);
      }
      renderPanes();
      renderInspector();
    });
  });
}

function renderStaticChips() {
  dom.timelineRangeChips.innerHTML = TIMELINE_RANGES.map(
    (range) => `<button class="chip${range === store.filters.timelineRange ? " active" : ""}" data-range="${range}">${range}</button>`
  ).join("");
  dom.timelineRangeChips.querySelectorAll(".chip").forEach((chip) => {
    chip.addEventListener("click", () => {
      store.filters.timelineRange = chip.getAttribute("data-range");
      dom.timelineRangeChips.querySelectorAll(".chip").forEach((c) => c.classList.remove("active"));
      chip.classList.add("active");
      fetchTimeline().catch(() => {});
    });
  });
  dom.timelineSession.addEventListener("change", () => {
    setSelectedSession(dom.timelineSession.value);
  });

  dom.agentStateChips.innerHTML = AGENT_STATES.map(
    (s) => `<button class="chip state-${s} active" data-state="${s}">${s}</button>`
  ).join("");
  dom.agentStateChips.querySelectorAll(".chip").forEach((chip) => {
    chip.addEventListener("click", () => {
      const s = chip.getAttribute("data-state");
      if (store.filters.agentStates.has(s)) {
        store.filters.agentStates.delete(s);
        chip.classList.remove("active");
      } else {
        store.filters.agentStates.add(s);
        chip.classList.add("active");
      }
      renderAgents();
      renderInspector();
    });
  });

  dom.agentKindChips.innerHTML = AGENT_KINDS.map(
    (k) => `<button class="chip active" data-kind="${k}">${k}</button>`
  ).join("");
  dom.agentKindChips.querySelectorAll(".chip").forEach((chip) => {
    chip.addEventListener("click", () => {
      const k = chip.getAttribute("data-kind");
      if (store.filters.agentKinds.has(k)) {
        store.filters.agentKinds.delete(k);
        chip.classList.remove("active");
      } else {
        store.filters.agentKinds.add(k);
        chip.classList.add("active");
      }
      renderAgents();
      renderInspector();
    });
  });
}

function initDataTabs() {
  const apply = (tab) => {
    store.ui.activeTab = tab === "panes" ? "panes" : "agents";
    saveValue(DATA_TAB_KEY, store.ui.activeTab);
    dom.dataTabs.querySelectorAll("[data-tab]").forEach((btn) => {
      btn.classList.toggle("active", btn.getAttribute("data-tab") === store.ui.activeTab);
    });
    dom.agentsTab.classList.toggle("active", store.ui.activeTab === "agents");
    dom.panesTab.classList.toggle("active", store.ui.activeTab === "panes");
    renderDataMeta();
  };
  dom.dataTabs.querySelectorAll("[data-tab]").forEach((btn) => {
    btn.addEventListener("click", () => apply(btn.getAttribute("data-tab")));
  });
  apply(store.ui.activeTab);
}

function initSessionControls() {
  dom.showAllSessions.addEventListener("click", () => setSelectedSession(""));
}

function initCollapseControls() {
  document.querySelectorAll("[data-collapse-target]").forEach((btn) => {
    const panelId = btn.getAttribute("data-collapse-target");
    const panel = document.getElementById(panelId);
    if (!panel) return;

    const collapsed = store.ui.collapsedPanels.has(panelId);
    setPanelCollapsed(panel, btn, collapsed);

    btn.addEventListener("click", () => {
      const next = !panel.classList.contains("collapsed");
      setPanelCollapsed(panel, btn, next);
      if (next) {
        store.ui.collapsedPanels.add(panelId);
      } else {
        store.ui.collapsedPanels.delete(panelId);
      }
      saveSet(COLLAPSED_PANELS_KEY, store.ui.collapsedPanels);
    });
  });
}

function setPanelCollapsed(panel, btn, collapsed) {
  panel.classList.toggle("collapsed", collapsed);
  btn.setAttribute("aria-expanded", collapsed ? "false" : "true");
  const icon = btn.querySelector("span");
  if (icon) icon.textContent = collapsed ? "›" : "⌄";
}

function renderInspector() {
  if (store.ui.selectedSegment) {
    renderSegmentInspector(store.ui.selectedSegment);
    return;
  }

  const session = selectedSession();
  const summaries = buildSessionSummaries();
  const summary = session ? summaries.find((s) => s.label === session) : null;
  const agents = [...store.agents.values()]
    .filter(agentMatchesSelectedSession)
    .sort((a, b) => (b.last_activity_at || "").localeCompare(a.last_activity_at || ""));
  const panes = store.panes.filter(paneMatchesSelectedSession);

  dom.inspectorMeta.textContent = session || "overview";
  const title = session || "all sessions";
  const activeSummary = summary || {
    working: agents.filter((a) => a.state === "working").length,
    waiting: agents.filter((a) => a.state === "waiting_input" || a.state === "waiting_choice").length,
    errors: agents.filter((a) => a.state === "error").length,
    agents: agents.length,
    panes: panes.length,
    totals: store.timeline?.totals || emptyTimelineTotals(),
    human_presence_secs: humanPresenceSecs(store.timeline?.lanes || []),
  };
  const humanSecs = activeSummary.human_presence_secs ?? humanPresenceSecs(store.timeline?.lanes || []);

  dom.inspectorBody.innerHTML = `
    <div class="inspector-title">
      <span>${esc(title)}</span>
      <small>${esc(sessionSummaryLine(activeSummary))}</small>
    </div>
    <div class="inspector-grid">
      ${inspectorMetric("work", activeSummary.working)}
      ${inspectorMetric("wait", activeSummary.waiting)}
      ${inspectorMetric("err", activeSummary.errors)}
      ${inspectorMetric("human", formatDuration(humanSecs))}
      ${inspectorMetric("tmux", formatDuration(activeSummary.totals.foreground_secs || 0))}
      ${inspectorMetric("panes", panes.length)}
    </div>
    <div class="inspector-section">
      <h3>Agents</h3>
      ${renderInspectorAgents(agents.slice(0, 8))}
    </div>
    <div class="inspector-section">
      <h3>Panes</h3>
      ${renderInspectorPanes(panes.slice(0, 8))}
    </div>`;
}

function renderSegmentInspector(segment) {
  dom.inspectorMeta.textContent = "interval";
  dom.inspectorBody.innerHTML = `
    <div class="inspector-title">
      <span>${esc(segment.detail)}</span>
      <small>${esc(segment.source || segment.state || "timeline")}</small>
    </div>
    <div class="inspector-grid">
      ${inspectorMetric("duration", formatDuration(segment.duration_secs || 0))}
      ${inspectorMetric("state", segment.state || "—")}
      ${inspectorMetric("session", segment.session || "—")}
      ${inspectorMetric("pane", segment.pane || "—")}
    </div>
    <dl class="kv-list">
      <dt>start</dt><dd>${esc(segment.started_at ? dateTimeLabel(new Date(segment.started_at)) : "—")}</dd>
      <dt>end</dt><dd>${esc(segment.ended_at ? dateTimeLabel(new Date(segment.ended_at)) : "—")}</dd>
      <dt>source</dt><dd>${esc(segment.source || "—")}</dd>
    </dl>`;
}

function inspectorMetric(label, value) {
  return `<div class="mini-metric"><span>${esc(label)}</span><strong>${esc(value ?? "—")}</strong></div>`;
}

function renderInspectorAgents(agents) {
  if (agents.length === 0) return `<div class="empty-block compact">none</div>`;
  return agents.map((a) => `<div class="mini-row">
    <span class="state-dot ${esc(a.state)}"></span>
    <span>${esc(a.kind)}</span>
    <small>${esc(relTime(a.last_activity_at))}</small>
  </div>`).join("");
}

function renderInspectorPanes(panes) {
  if (panes.length === 0) return `<div class="empty-block compact">none</div>`;
  return panes.map((p) => `<div class="mini-row">
    <span>${esc(p.window_index)}.${esc(p.pane_id)}</span>
    <span>${esc(p.current_command || "—")}</span>
    <small>${esc(p.title || "")}</small>
  </div>`).join("");
}

// ── Helpers ───────────────────────────────────────────────────────

function esc(v) {
  return String(v ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

function relTime(iso) {
  if (!iso) return "—";
  const then = Date.parse(iso);
  if (isNaN(then)) return "—";
  const diff = Math.max(0, Date.now() - then);
  if (diff < 60_000) return `${Math.floor(diff / 1000)}s ago`;
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

function timeLabel(date) {
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function dateTimeLabel(date) {
  return date.toLocaleString([], {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatDuration(totalSecs) {
  const secs = Math.max(0, Math.floor(totalSecs || 0));
  if (secs === 0) return "—";
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  const remMins = mins % 60;
  if (hours < 24) return `${hours}h${String(remMins).padStart(2, "0")}m`;
  const days = Math.floor(hours / 24);
  return `${days}d${String(hours % 24).padStart(2, "0")}h`;
}

// ── Data loading ──────────────────────────────────────────────────

async function fetchAgentsSnapshot() {
  const data = await jsonFetch("/api/agents");
  store.agents.clear();
  for (const a of data.agents || []) store.agents.set(a.session_id, a);
  renderAgents();
  renderCounts();
}

async function fetchPanes() {
  const data = await jsonFetch("/api/panes");
  store.panes = data.panes || [];
  store.paneErrors = data.errors || [];
  renderPanes();
  // Agent rows render their PANE column by looking up store.panes, so a
  // refreshed pane scan can change those labels even when no agent
  // transition fires.
  renderAgents();
  renderCounts();
}

async function fetchTimeline() {
  const params = new URLSearchParams({ since: store.filters.timelineRange });
  store.timeline = await jsonFetch(`/api/timeline?${params.toString()}`);
  renderTimeline();
}

async function fetchHealth() {
  const data = await jsonFetch("/api/health");
  dom.version.textContent = `v${data.version} · proto ${data.protocol}`;
}

// ── SSE handlers ──────────────────────────────────────────────────

function applySnapshot(payload) {
  store.agents.clear();
  for (const a of payload.agents || []) store.agents.set(a.session_id, a);
  renderAgents();
  renderCounts();
  scheduleTimelineRefresh();
}

function applyTransition(t) {
  if (!t || !t.agent) return;
  const a = t.agent;
  if (t.to === "stopped" && t.from === "stopped") {
    store.agents.delete(a.session_id);
  } else {
    store.agents.set(a.session_id, a);
  }
  renderAgents();
  renderCounts();
  scheduleTimelineRefresh();
}

let timelineRefreshTimer = null;
function scheduleTimelineRefresh() {
  if (timelineRefreshTimer) return;
  timelineRefreshTimer = setTimeout(() => {
    timelineRefreshTimer = null;
    fetchTimeline().catch(() => {});
  }, 500);
}

// ── Boot ──────────────────────────────────────────────────────────

async function main() {
  bootstrapToken();
  initCollapseControls();
  initDataTabs();
  initSessionControls();
  renderStaticChips();
  setConnectionStatus("connecting", "loading…");

  try {
    await fetchHealth();
  } catch (_) {
    return; // setConnectionStatus already showed the error
  }

  await Promise.all([fetchAgentsSnapshot(), fetchPanes(), fetchTimeline()]);

  // Periodically refetch panes — they are pull-only on the server.
  setInterval(() => {
    fetchPanes().catch(() => {
      /* swallowed; SSE indicator will reflect downtime */
    });
  }, PANES_REFETCH_INTERVAL_MS);

  setInterval(() => {
    fetchTimeline().catch(() => {});
  }, TIMELINE_REFETCH_INTERVAL_MS);

  // Subscribe to live events.
  streamEvents(
    (event, payload) => {
      if (event === "snapshot") applySnapshot(payload);
      else if (event === "transition") applyTransition(payload);
    },
    (_payload) => {
      // Lagged: refetch a clean snapshot.
      fetchAgentsSnapshot().catch(() => {});
    }
  );
}

main();
