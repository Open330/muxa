// muxa dashboard frontend.
//
// Vanilla ES2022 modules — no build step. Loaded as
// <script type="module" src="/static/main.js"> from index.html.
//
// Runtime model:
//   * On boot, capture the token from the URL #fragment (#token=...) into
//     localStorage and scrub it from the URL bar. The fragment is never
//     sent to the server, so the secret can't leak into request logs or a
//     Referer header. A legacy ?token=... query param is still accepted
//     for compatibility but is scrubbed immediately. localStorage persists
//     across tab close and browser restart, so the user pastes it once.
//   * Fetch /api/health and /api/access. In `public_read` mode reads and SSE
//     work anonymously while the same bearer token acts as a browser PAT for
//     the separate control routes.
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
const SESSION_SORT_KEY = "muxa.dashboard.sessionSort";
const PANES_REFETCH_INTERVAL_MS = 5000;
const TERMINALS_REFETCH_INTERVAL_MS = 2000;
const TIMELINE_REFETCH_INTERVAL_MS = 30000;
const TIMELINE_MIN_REFRESH_INTERVAL_MS = 10000;
const TIMELINE_EVENT_DEBOUNCE_MS = 2000;
const SESSION_PAGE_SIZE = 40;

const AGENT_STATES = ["working", "waiting_input", "waiting_choice", "idle", "starting", "error", "stopped"];
const AGENT_KINDS = ["claude_code", "codex", "gemini_cli", "opencode", "unknown"];
const TIMELINE_RANGES = ["24h", "today", "last week", "month", "last month", "7d", "30d", "12w"];
const SESSION_SORTS = new Set(["priority", "latest", "name", "active", "human", "tmux"]);

// ── Token bootstrap ────────────────────────────────────────────────

function bootstrapToken() {
  const url = new URL(window.location.href);
  let token = null;

  // Primary path: token delivered in the URL fragment (#token=…). The
  // fragment stays client-side, so the secret never reaches the server in
  // a query string, request log, or Referer header.
  if (url.hash) {
    const hashParams = new URLSearchParams(url.hash.replace(/^#/, ""));
    const fromHash = hashParams.get("token");
    if (fromHash) {
      token = fromHash;
      hashParams.delete("token");
      const rest = hashParams.toString();
      url.hash = rest ? `#${rest}` : "";
    }
  }

  // Backward-compat: also honour a legacy ?token=… query param, but scrub
  // it from the address bar right away. (It may already have hit the
  // server via this path — rotate the token if you relied on it.)
  const fromQuery = url.searchParams.get("token");
  if (fromQuery) {
    token = token || fromQuery;
    url.searchParams.delete("token");
  }

  if (token) {
    localStorage.setItem(TOKEN_KEY, token);
    window.history.replaceState({}, "", url.toString());
  }
}

function authHeaders() {
  const token = localStorage.getItem(TOKEN_KEY);
  return token ? { Authorization: `Bearer ${token}` } : {};
}

// ── HTTP helpers ───────────────────────────────────────────────────

async function jsonFetch(path, options = {}) {
  const resp = await fetch(path, {
    ...options,
    headers: { ...authHeaders(), ...(options.headers || {}) },
  });
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

async function controlFetch(path, options = {}) {
  const resp = await fetch(path, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      ...authHeaders(),
      ...(options.headers || {}),
    },
  });
  let payload = null;
  try {
    payload = await resp.json();
  } catch (_) {
    // Status-only middleware rejections have an empty body.
  }
  if (resp.status === 401 || resp.status === 403) {
    store.access.writeAuthorized = false;
    renderAccess();
    throw new Error(resp.status === 401 ? "invalid or missing edit PAT" : "dashboard is read-only");
  }
  if (!resp.ok) {
    throw new Error(payload?.error || `${path} → ${resp.status}`);
  }
  return payload || { ok: true };
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
      throw new Error("SSE stream ended");
    } catch (e) {
      setConnectionStatus("degraded", `reconnecting in ${backoffMs}ms`);
      await new Promise((r) => setTimeout(r, backoffMs));
      backoffMs = Math.min(backoffMs * 2, 10000);
    }
  }
}

// ── Connection indicator ──────────────────────────────────────────

const dom = {
  accessMode: document.getElementById("access-mode"),
  editAccess: document.getElementById("edit-access"),
  conn: document.getElementById("conn"),
  connLabel: document.getElementById("conn-label"),
  counts: document.getElementById("counts"),
  version: document.getElementById("version"),
  sessionList: document.getElementById("session-list"),
  sessionsMeta: document.getElementById("sessions-meta"),
  sessionSort: document.getElementById("session-sort"),
  showAllSessions: document.getElementById("show-all-sessions"),
  metricWorking: document.getElementById("metric-working"),
  metricWaiting: document.getElementById("metric-waiting"),
  metricErrors: document.getElementById("metric-errors"),
  metricActive: document.getElementById("metric-active"),
  metricHuman: document.getElementById("metric-human"),
  metricTmux: document.getElementById("metric-tmux"),
  agentsBody: document.getElementById("agents-tbody"),
  panesBody: document.getElementById("panes-tbody"),
  terminalsBody: document.getElementById("terminals-tbody"),
  timelineHeatmap: document.getElementById("timeline-heatmap"),
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
  terminalsMeta: document.getElementById("terminals-meta"),
  dataMeta: document.getElementById("data-meta"),
  dataTabs: document.getElementById("data-tabs"),
  agentsTab: document.getElementById("agents-tab"),
  panesTab: document.getElementById("panes-tab"),
  terminalsTab: document.getElementById("terminals-tab"),
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

function renderAccess() {
  const access = store.access;
  const editing = access.writeAuthorized;
  dom.accessMode.textContent = editing
    ? "edit unlocked"
    : access.mode === "public_read"
      ? "public read-only"
      : access.mode === "token"
        ? "private"
        : "read-only";
  dom.accessMode.classList.toggle("edit", editing);
  dom.editAccess.hidden = !access.writeAvailable;
  dom.editAccess.disabled = access.mode === "token" && editing;
  dom.editAccess.textContent = editing
    ? access.mode === "public_read" ? "lock edit" : "PAT active"
    : "unlock edit";
  if (store.agents.size > 0) renderAgents();
  if (store.panes.length > 0) renderPanes();
  if (store.terminalSessions.length > 0) renderTerminals();
}

async function fetchAccess() {
  const data = await jsonFetch("/api/access");
  store.access = {
    mode: data.mode || "read_only",
    readRequiresToken: Boolean(data.read_requires_token),
    writeAvailable: Boolean(data.write_available),
    writeAuthorized: Boolean(data.write_authorized),
  };
  renderAccess();
  return store.access;
}

function initAccessControl() {
  dom.editAccess.addEventListener("click", async () => {
    if (store.access.writeAuthorized && store.access.mode === "public_read") {
      localStorage.removeItem(TOKEN_KEY);
      await fetchAccess().catch(() => {});
      showToast("edit locked · public read-only");
      return;
    }
    if (store.access.writeAuthorized) return;

    const token = window.prompt("Muxa edit PAT");
    if (!token) return;
    localStorage.setItem(TOKEN_KEY, token.trim());
    try {
      const access = await fetchAccess();
      if (!access.writeAuthorized) {
        localStorage.removeItem(TOKEN_KEY);
        await fetchAccess().catch(() => {});
        showToast("invalid edit PAT");
        return;
      }
      showToast("edit unlocked");
    } catch (_) {
      localStorage.removeItem(TOKEN_KEY);
      showToast("invalid edit PAT");
    }
  });
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
  access: {
    mode: "read_only",
    readRequiresToken: false,
    writeAvailable: false,
    writeAuthorized: false,
  },
  agents: new Map(), // session_id -> Agent
  panes: [], // PaneSummary[]
  terminalSessions: [], // SessionRef[]
  paneErrors: [], // ScanError[]
  timeline: null, // TimelineDocument
  timelineSummary: null, // compact all-session TimelineDocument
  timelineSessions: new Set(),
  indexes: {
    paneBySocketAndId: new Map(),
    panesById: new Map(),
    timelineSessionByAgent: new Map(),
  },
  revisions: { agents: 0, panes: 0, timeline: 0 },
  cache: {
    paneFingerprint: "",
    terminalFingerprint: "",
    sessionSummariesKey: "",
    sessionSummaries: [],
  },
  ui: {
    collapsedPanels: loadSet(COLLAPSED_PANELS_KEY),
    collapsedTimelineGroups: loadSet(COLLAPSED_TIMELINE_GROUPS_KEY),
    activeTab: loadValue(DATA_TAB_KEY, "agents"),
    sessionSort: normalizeSessionSort(loadValue(SESSION_SORT_KEY, "priority")),
    sessionLimit: SESSION_PAGE_SIZE,
    selectedSegment: null,
    terminalCapture: null,
    selectedTimelineDay: "",
  },
  filters: {
    agentStates: new Set(AGENT_STATES),
    agentKinds: new Set(AGENT_KINDS),
    paneSockets: new Set(), // populated dynamically
    timelineRange: "7d",
    timelineSession: "",
  },
};

// ── Helpers ───────────────────────────────────────────────────────

// Resolve a raw tmux pane id (e.g. "%1645") to a richer label
// "session:window.pane" by cross-referencing the global pane scan. Falls
// back to the raw id when no scan match is available (e.g. before the
// first /api/panes response, or when the pane lives on an unreadable
// socket).
function socketShortName(socket) {
  return (socket || "default").split("/").pop() || "default";
}

function paneLookupKey(socket, paneId) {
  return `${socketShortName(socket)}\u0000${paneId || ""}`;
}

function rebuildPaneIndexes() {
  store.indexes.paneBySocketAndId.clear();
  store.indexes.panesById.clear();
  for (const pane of store.panes) {
    store.indexes.paneBySocketAndId.set(paneLookupKey(pane.socket, pane.pane_id), pane);
    if (!store.indexes.panesById.has(pane.pane_id)) store.indexes.panesById.set(pane.pane_id, []);
    store.indexes.panesById.get(pane.pane_id).push(pane);
  }
}

function rebuildTimelineIndexes() {
  store.indexes.timelineSessionByAgent.clear();
  for (const lane of store.timeline?.lanes || []) {
    if (lane.kind === "agent" && lane.session_id) {
      store.indexes.timelineSessionByAgent.set(lane.session_id, lane.session_name || lane.session_id);
    }
  }
}

function paneForAgent(agent) {
  if (!agent?.pane) return null;
  const exact = store.indexes.paneBySocketAndId.get(paneLookupKey(agent.tmux_socket, agent.pane));
  if (exact) return exact;
  const candidates = store.indexes.panesById.get(agent.pane) || [];
  return candidates.length === 1 ? candidates[0] : null;
}

function resolvePaneLabel(paneId, socket = null) {
  if (!paneId) return "—";
  const exact = socket
    ? store.indexes.paneBySocketAndId.get(paneLookupKey(socket, paneId))
    : null;
  const candidates = store.indexes.panesById.get(paneId) || [];
  const match = exact || (candidates.length === 1 ? candidates[0] : null);
  if (!match) return paneId;
  return `${match.session}:${match.window_index}.${paneId}`;
}

function sessionForAgent(agent) {
  if (!agent) return "";
  const paneMatch = paneForAgent(agent);
  if (paneMatch?.session) return paneMatch.session;
  return store.indexes.timelineSessionByAgent.get(agent.session_id) || agent.session_id || "";
}

function selectedSession() {
  return store.filters.timelineSession || "";
}

function normalizeSessionSort(sort) {
  return SESSION_SORTS.has(sort) ? sort : "priority";
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
  const next = session || "";
  if (store.filters.timelineSession === next && store.timeline) return;
  store.filters.timelineSession = next;
  store.ui.selectedSegment = null;
  store.ui.terminalCapture = null;
  store.indexes.timelineSessionByAgent.clear();
  if (dom.timelineSession) dom.timelineSession.value = store.filters.timelineSession;
  store.timeline = null;
  renderTimeline();
  renderAgents();
  renderPanes();
  renderInspector();
  fetchTimeline({ force: true }).catch(() => {});
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
  const agents = [...store.agents.values()].filter(agentMatchesSelectedSession);
  const working = agents.filter((a) => a.state === "working").length;
  const waiting = agents.filter((a) => a.state === "waiting_input" || a.state === "waiting_choice").length;
  const errors = agents.filter((a) => a.state === "error").length;
  const lanes = store.timeline?.lanes || [];
  const totals = store.timeline?.totals || {};
  dom.metricWorking.textContent = String(working);
  dom.metricWaiting.textContent = String(waiting);
  dom.metricErrors.textContent = String(errors);
  dom.metricActive.textContent = formatDuration(totals.active_secs || 0);
  dom.metricHuman.textContent = formatDuration(
    store.timeline?.summary?.human_presence_secs ?? humanPresenceSecs(lanes)
  );
  dom.metricTmux.textContent = formatDuration(totals.foreground_secs || 0);
}

function renderSessionSidebar() {
  const summaries = buildSessionSummaries();
  const visible = summaries.slice(0, store.ui.sessionLimit);
  const active = selectedSession();
  if (active && !visible.some((summary) => summary.label === active)) {
    const selected = summaries.find((summary) => summary.label === active);
    if (selected) visible.push(selected);
  }
  dom.sessionsMeta.textContent = `${Math.min(store.ui.sessionLimit, summaries.length)}/${summaries.length}`;
  if (dom.sessionSort) dom.sessionSort.value = store.ui.sessionSort;
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
  const rows = visible.map((s) => {
    const stateClass = s.errors > 0 ? "error" : s.waiting > 0 ? "waiting" : s.working > 0 ? "working" : "idle";
    return `<button class="session-row${s.label === active ? " active" : ""}" type="button" data-session="${esc(s.label)}">
      <span class="session-dot ${stateClass}"></span>
      <span class="session-main">
        <span class="session-name">${esc(s.label)}</span>
        <span class="session-detail">${esc(sessionSummaryLine(s))}</span>
      </span>
      <span class="session-score">${esc(sessionScoreLabel(s, store.ui.sessionSort))}</span>
    </button>`;
  }).join("");
  const remaining = Math.max(0, summaries.length - store.ui.sessionLimit);
  const loadMore = remaining > 0
    ? `<button class="session-load-more" type="button" data-load-more-sessions>show ${Math.min(SESSION_PAGE_SIZE, remaining)} more · ${remaining} hidden</button>`
    : "";
  dom.sessionList.innerHTML = allRow + rows + loadMore;
}

function buildSessionSummaries() {
  const cacheKey = `${store.revisions.agents}:${store.revisions.panes}:${store.revisions.timeline}:${store.ui.sessionSort}`;
  if (store.cache.sessionSummariesKey === cacheKey) return store.cache.sessionSummaries;
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

  const compactSessions = store.timelineSummary?.summary?.sessions || [];
  for (const summary of compactSessions) {
    const s = ensure(summary.label);
    s.lanes = summary.lanes || 0;
    s.latest = summary.latest_at || "";
    s.totals = { ...emptyTimelineTotals(), ...(summary.totals || {}) };
    s.human_presence_secs = summary.human_presence_secs || 0;
  }
  if (compactSessions.length === 0) {
    for (const lane of store.timeline?.lanes || []) {
      const label = lane.session_name || lane.session_id || "no session";
      const s = ensure(label);
      s.lanes += 1;
      addTimelineTotals(s.totals, lane.totals || {});
      for (const interval of lane.intervals || []) {
        if (interval.open) continue;
        const latest = interval.ended_at || interval.started_at || "";
        if (latest > s.latest) s.latest = latest;
      }
    }
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
  for (const active of store.timelineSummary?.active_sessions || store.timeline?.active_sessions || []) {
    ensure(active.label).totals.active_secs = active.active_secs || 0;
  }
  if (compactSessions.length === 0) {
    for (const summary of map.values()) {
      summary.human_presence_secs = humanPresenceSecs(
        (store.timeline?.lanes || []).filter((lane) =>
          (lane.session_name || lane.session_id || "no session") === summary.label
        )
      );
    }
  }

  const summaries = [...map.values()].sort(compareSessionSummaries);
  store.cache.sessionSummariesKey = cacheKey;
  store.cache.sessionSummaries = summaries;
  return summaries;
}

function sessionSummaryLine(s) {
  const parts = [];
  if (s.working) parts.push(`${s.working} work`);
  if (s.waiting) parts.push(`${s.waiting} wait`);
  if (s.errors) parts.push(`${s.errors} err`);
  if (s.totals?.active_secs) parts.push(`act ${formatDuration(s.totals.active_secs)}`);
  if (s.agents) parts.push(`${s.agents} agents`);
  if (s.panes) parts.push(`${s.panes} panes`);
  return parts.slice(0, 3).join(" · ") || `${s.lanes} lanes`;
}

function compareSessionSummaries(a, b) {
  switch (store.ui.sessionSort) {
    case "latest":
      return compareLatest(a, b) || comparePriority(a, b) || compareName(a, b);
    case "name":
      return compareName(a, b);
    case "active":
      return compareNumeric(b.totals.active_secs || 0, a.totals.active_secs || 0) ||
        compareLatest(a, b) ||
        compareName(a, b);
    case "human":
      return compareNumeric(b.human_presence_secs, a.human_presence_secs) ||
        compareLatest(a, b) ||
        compareName(a, b);
    case "tmux":
      return compareNumeric(b.totals.foreground_secs, a.totals.foreground_secs) ||
        compareLatest(a, b) ||
        compareName(a, b);
    case "priority":
    default:
      return comparePriority(a, b) || compareLatest(a, b) || compareName(a, b);
  }
}

function comparePriority(a, b) {
  const scoreA = a.errors * 1000 + a.waiting * 100 + a.working * 10;
  const scoreB = b.errors * 1000 + b.waiting * 100 + b.working * 10;
  return compareNumeric(scoreB, scoreA);
}

function compareLatest(a, b) {
  return (b.latest || "").localeCompare(a.latest || "");
}

function compareName(a, b) {
  return a.label.localeCompare(b.label);
}

function compareNumeric(a, b) {
  return a === b ? 0 : a > b ? 1 : -1;
}

function sessionScoreLabel(s, sort) {
  if (sort === "latest") return relTime(s.latest);
  if (sort === "active") return formatDuration(s.totals.active_secs || 0);
  if (sort === "human") return formatDuration(s.human_presence_secs || 0);
  if (sort === "tmux") return formatDuration(s.totals.foreground_secs || 0);
  if (s.errors > 0) return String(s.errors);
  if (s.waiting > 0) return String(s.waiting);
  if (s.working > 0) return String(s.working);
  return "·";
}

function renderTimeline() {
  const doc = store.timeline;
  if (!doc) {
    dom.timelineHeatmap.innerHTML = `<div class="timeline-empty compact">loading…</div>`;
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
    dom.timelineHeatmap.innerHTML = "";
    dom.timelineBody.innerHTML = `<div class="timeline-empty">timeline window is invalid</div>`;
    dom.timelineAxis.innerHTML = "";
    dom.timelineMeta.textContent = "invalid window";
    renderOverview();
    renderSessionSidebar();
    renderInspector();
    return;
  }

  const compactSummary = !selectedSession() ? doc.summary : null;
  if (compactSummary) {
    dom.timelineAxis.innerHTML = "";
    renderTimelineHeatmap(summaryDayBuckets(compactSummary.days || []));
    renderAllSessionsTimelineSummary(compactSummary);
    dom.timelineMeta.textContent = `${compactSummary.sessions?.length || 0} sessions · partial summary`;
    renderOverview();
    renderSessionSidebar();
    renderInspector();
    return;
  }

  renderTimelineAxis(start, end);

  const lanes = (doc.lanes || []).filter(laneMatchesSelectedSession);
  const dayBuckets = buildTimelineDayBuckets(lanes, start, end);
  renderTimelineHeatmap(dayBuckets);
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
      const laneRows = collapsed
        ? ""
        : group.lanes.map((lane) => renderTimelineLane(lane, start, end, true)).join("");
      return `<div class="timeline-group${collapsed ? " collapsed" : ""}">${groupHeader}<div class="timeline-group-lanes">${laneRows}</div></div>`;
    })
    .join("");
  renderOverview();
  renderSessionSidebar();
  renderInspector();
}

function renderAllSessionsTimelineSummary(summary) {
  const sources = summary.sources || [];
  const maxSecs = Math.max(1, ...sources.map((source) => sourceSummarySecs(source)));
  dom.timelineBody.innerHTML = `
    <div class="timeline-summary-head">
      <strong>All sessions overview</strong>
      <span>aggregated · select a session for exact intervals</span>
    </div>
    <div class="timeline-summary-lanes">
      ${sources.map((source) => renderTimelineSummaryLane(source, maxSecs)).join("")}
    </div>`;
}

function renderTimelineSummaryLane(source, maxSecs) {
  const secs = sourceSummarySecs(source);
  const width = secs > 0 ? Math.max(1, (secs / maxSecs) * 100) : 0;
  const totals = source.totals || {};
  const label = source.kind === "human" ? "interaction" : source.kind || "agent";
  const segments = source.kind === "agent"
    ? renderAgentSummarySegments(totals)
    : `<span class="timeline-summary-fill source-${source.kind === "tmux" ? "tmux" : "human"}" style="width:100%"></span>`;
  return `<div class="timeline-summary-lane">
    <div class="timeline-label">
      <span>${esc(label)}</span>
      <small>${esc(sourceSummaryLabel(source))}</small>
    </div>
    <div class="timeline-summary-track">
      <div class="timeline-summary-scale" style="width:${width}%">${segments}</div>
    </div>
  </div>`;
}

function renderAgentSummarySegments(totals) {
  const parts = [
    ["state-working", totals.working_secs || 0],
    ["state-waiting", totals.waiting_secs || 0],
    ["state-error", totals.error_secs || 0],
  ];
  const total = parts.reduce((sum, [, secs]) => sum + secs, 0);
  if (!total) return "";
  return parts
    .filter(([, secs]) => secs > 0)
    .map(([cls, secs]) => `<span class="timeline-summary-fill ${cls}" style="width:${(secs / total) * 100}%"></span>`)
    .join("");
}

function sourceSummarySecs(source) {
  const totals = source.totals || {};
  if (source.kind === "human") return totals.human_secs || 0;
  if (source.kind === "tmux") return totals.foreground_secs || 0;
  return (totals.working_secs || 0) +
    (totals.waiting_secs || 0) +
    (totals.error_secs || 0);
}

function sourceSummaryLabel(source) {
  const totals = source.totals || {};
  const suffix = `${source.sessions || 0} sessions · ${source.lanes || 0} lanes`;
  if (source.kind === "human") return `interact ${formatDuration(totals.human_secs || 0)} · ${suffix}`;
  if (source.kind === "tmux") return `tmux ${formatDuration(totals.foreground_secs || 0)} · ${suffix}`;
  return [
    totals.active_secs ? `act ${formatDuration(totals.active_secs)}` : "",
    totals.working_secs ? `work ${formatDuration(totals.working_secs)}` : "",
    totals.waiting_secs ? `wait ${formatDuration(totals.waiting_secs)}` : "",
    totals.error_secs ? `err ${formatDuration(totals.error_secs)}` : "",
    suffix,
  ].filter(Boolean).join(" · ");
}

function summaryDayBuckets(days) {
  return days.map((day) => ({
    key: day.date,
    dateMs: new Date(`${day.date}T00:00:00`).getTime(),
    totals: { ...emptyTimelineTotals(), ...(day.totals || {}) },
    sessions: new Map((day.top_sessions || []).map((session) => [session.label, session.active_secs || 0])),
  }));
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
  const activeBySession = activeSessionsByLabel();
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
      group.totals.active_secs = activeBySession.get(group.label) || 0;
      return group;
    });
}

function renderTimelineHeatmap(buckets) {
  if (!dom.timelineHeatmap) return;
  if (!buckets.length) {
    dom.timelineHeatmap.innerHTML = `<div class="timeline-empty compact">no daily activity in this view</div>`;
    return;
  }
  const maxSecs = Math.max(...buckets.map((bucket) => activeTimelineSecs(bucket.totals)));
  const leading = mondayWeekdayIndex(buckets[0].dateMs);
  const cells = Array.from({ length: leading }, () => null).concat(buckets);
  while (cells.length % 7 !== 0) cells.push(null);
  const selected =
    buckets.find((bucket) => bucket.key === store.ui.selectedTimelineDay) ||
    [...buckets].reverse().find((bucket) => activeTimelineSecs(bucket.totals) > 0) ||
    buckets[buckets.length - 1];
  const weekdayLabels = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
    .map((label) => `<span>${label}</span>`)
    .join("");
  const dayCells = cells
    .map((bucket) => {
      if (!bucket) return `<span class="timeline-day spacer"></span>`;
      const active = activeTimelineSecs(bucket.totals);
      const level = heatmapLevel(active, maxSecs);
      const selectedClass = bucket.key === selected.key ? " selected" : "";
      const title = `${bucket.key} · ${dayTotalsLabel(bucket.totals)}`;
      return `<button class="timeline-day level-${level}${selectedClass}" type="button" data-day="${esc(bucket.key)}" title="${esc(title)}" aria-label="${esc(title)}"></button>`;
    })
    .join("");
  dom.timelineHeatmap.innerHTML = `
    <div class="timeline-heatmap-head">
      <strong>Daily activity</strong>
      <span>${buckets.length} day${buckets.length === 1 ? "" : "s"} · peak ${esc(formatDuration(maxSecs))}</span>
    </div>
    <div class="timeline-heatmap-map">
      <div class="timeline-heatmap-weekdays">${weekdayLabels}</div>
      <div class="timeline-heatmap-grid">${dayCells}</div>
    </div>
    <div class="timeline-day-detail">
      <strong>${esc(selected.key)}</strong>
      <span>${esc(dayTotalsLabel(selected.totals))}</span>
      <small>${esc(topDaySessionsLabel(selected))}</small>
    </div>`;
}

function buildTimelineDayBuckets(lanes, windowStart, windowEnd) {
  if (!Number.isFinite(windowStart) || !Number.isFinite(windowEnd) || windowEnd <= windowStart) {
    return [];
  }
  const startDay = localDayStartMs(windowStart);
  const endDay = localDayStartMs(Math.max(windowStart, windowEnd - 1));
  const buckets = [];
  const byKey = new Map();
  for (let day = startDay; day <= endDay; day = addLocalDays(day, 1)) {
    const key = localDateKey(day);
    const bucket = {
      key,
      dateMs: day,
      totals: emptyTimelineTotals(),
      sessions: new Map(),
    };
    buckets.push(bucket);
    byKey.set(key, bucket);
  }

  for (const lane of lanes || []) {
    for (const interval of lane.intervals || []) {
      addIntervalToDayBuckets(byKey, lane, interval, windowStart, windowEnd);
    }
  }
  return buckets;
}

function addIntervalToDayBuckets(byKey, lane, interval, windowStart, windowEnd) {
  let cursor = Math.max(windowStart, Date.parse(interval.started_at));
  const endedAt = Math.min(windowEnd, Date.parse(interval.ended_at));
  if (!Number.isFinite(cursor) || !Number.isFinite(endedAt) || endedAt <= cursor) return;
  while (cursor < endedAt) {
    const key = localDateKey(cursor);
    const nextDay = addLocalDays(localDayStartMs(cursor), 1);
    const segmentEnd = Math.min(endedAt, nextDay);
    const secs = Math.max(0, Math.floor((segmentEnd - cursor) / 1000));
    const bucket = byKey.get(key);
    if (bucket && secs > 0) {
      addIntervalSecs(bucket.totals, interval, secs);
      const active = activeIntervalSecs(interval, secs);
      if (active > 0) {
        const session = interval.session_name || lane.session_name || interval.session_id || lane.session_id || lane.label || "unknown";
        bucket.sessions.set(session, (bucket.sessions.get(session) || 0) + active);
      }
    }
    if (segmentEnd <= cursor) break;
    cursor = segmentEnd;
  }
}

function addIntervalSecs(totals, interval, secs) {
  if (interval.source === "human_interaction") {
    totals.human_secs += secs;
    return;
  }
  if (interval.source === "session_foreground") {
    totals.foreground_secs += secs;
    return;
  }
  switch (interval.state) {
    case "working":
      totals.working_secs += secs;
      break;
    case "waiting_input":
    case "waiting_choice":
      totals.waiting_secs += secs;
      break;
    case "error":
      totals.error_secs += secs;
      break;
    case "starting":
      totals.starting_secs += secs;
      break;
    case "stopped":
      totals.stopped_secs += secs;
      break;
    default:
      totals.idle_secs += secs;
      break;
  }
}

function activeIntervalSecs(interval, secs) {
  if (interval.source === "human_interaction" || interval.source === "session_foreground") return secs;
  return ["working", "waiting_input", "waiting_choice", "error"].includes(interval.state) ? secs : 0;
}

function activeTimelineSecs(totals) {
  return (totals.working_secs || 0) +
    (totals.waiting_secs || 0) +
    (totals.error_secs || 0) +
    (totals.human_secs || 0) +
    (totals.foreground_secs || 0);
}

function heatmapLevel(secs, maxSecs) {
  if (!secs) return 0;
  if (!maxSecs) return 1;
  return Math.max(1, Math.min(4, Math.ceil((secs / maxSecs) * 4)));
}

function dayTotalsLabel(totals) {
  const active = activeTimelineSecs(totals);
  if (!active) return "—";
  return [
    ["activity", active],
    ["work", totals.working_secs],
    ["wait", totals.waiting_secs],
    ["err", totals.error_secs],
    ["human", totals.human_secs],
    ["tmux", totals.foreground_secs],
  ]
    .filter(([, secs]) => secs > 0)
    .map(([label, secs]) => `${label} ${formatDuration(secs)}`)
    .join(" · ");
}

function topDaySessionsLabel(bucket) {
  const sessions = [...bucket.sessions.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, 2)
    .map(([session, secs]) => `${session} ${formatDuration(secs)}`);
  return sessions.length ? sessions.join(" · ") : "no active sessions";
}

function localDayStartMs(ms) {
  const date = new Date(ms);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

function addLocalDays(ms, days) {
  const date = new Date(ms);
  date.setDate(date.getDate() + days);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

function mondayWeekdayIndex(ms) {
  return (new Date(ms).getDay() + 6) % 7;
}

function localDateKey(ms) {
  const date = new Date(ms);
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
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

function timelineLaneRank(kind) {
  if (kind === "agent") return 0;
  if (kind === "human") return 1;
  if (kind === "tmux") return 2;
  return 3;
}

function emptyTimelineTotals() {
  return {
    active_secs: 0,
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

function activeSessionsByLabel() {
  const map = new Map();
  for (const session of store.timeline?.active_sessions || []) {
    if (session.label) map.set(session.label, session.active_secs || 0);
  }
  return map;
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
    ["act", totals.active_secs],
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
  store.timelineSessions.clear();
  for (const p of store.panes || []) {
    if (p.session) store.timelineSessions.add(p.session);
  }
  for (const lane of doc.lanes || []) {
    if (lane.session_name) store.timelineSessions.add(lane.session_name);
    if (lane.session_id) store.timelineSessions.add(lane.session_id);
  }
  for (const session of doc.active_sessions || []) {
    if (session.label) store.timelineSessions.add(session.label);
  }
  for (const session of store.timelineSummary?.summary?.sessions || doc.summary?.sessions || []) {
    if (session.label) store.timelineSessions.add(session.label);
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
    dom.agentsBody.innerHTML = `<tr class="empty"><td colspan="10">no matching agents</td></tr>`;
    return;
  }

  const now = Date.now();
  const html = rows
    .map((a) => {
      const pane = resolvePaneLabel(a.pane, a.tmux_socket);
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
        <td>${paneControlButtons(a.pane, a.tmux_socket)}</td>
      </tr>`;
    })
    .join("");
  dom.agentsBody.innerHTML = html;
}

function paneControlButtons(pane, socket = "") {
  if (!store.access.writeAuthorized || !pane) return "—";
  return `<span class="control-actions">
    <button class="control-btn" type="button" data-pane-action="prompt" data-pane="${esc(pane)}" data-pane-socket="${esc(socket || "")}">prompt</button>
    <button class="control-btn danger" type="button" data-pane-action="abort" data-pane="${esc(pane)}" data-pane-socket="${esc(socket || "")}">abort</button>
  </span>`;
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
    dom.panesBody.innerHTML = `<tr class="empty"><td colspan="7">no panes (no running multiplexer server, or all filtered out)</td></tr>`;
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
        <td><span class="control-actions">
          <button class="attach-btn" data-cmd="${esc(p.attach_command)}">copy attach</button>
          ${paneControlButtons(p.pane_id, p.socket)}
        </span></td>
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
}

function renderTerminals() {
  const rows = store.terminalSessions;
  dom.terminalsMeta.textContent = `${rows.length} sessions`;
  renderDataMeta();
  if (rows.length === 0) {
    dom.terminalsBody.innerHTML = `<tr class="empty"><td colspan="7">no muxa-owned terminal sessions</td></tr>`;
    return;
  }
  dom.terminalsBody.innerHTML = rows
    .map((s) => `<tr>
      <td>${esc(s.id)}</td>
      <td>${esc(s.backend)}</td>
      <td>${esc(s.display_name || "—")}</td>
      <td title="${esc(s.cwd || "")}">${esc(shortPath(s.cwd || "—"))}</td>
      <td class="num">${esc(String(s.attached_clients || 0))}</td>
      <td>${s.exited ? `exited ${esc(String(s.exit_status ?? ""))}` : "running"}</td>
      <td><span class="control-actions">
        <button class="attach-btn" data-terminal-action="capture" data-session="${esc(s.id)}">capture</button>
        ${store.access.writeAuthorized && !s.exited
          ? `<button class="control-btn" data-terminal-action="input" data-session="${esc(s.id)}">input</button>
             <button class="control-btn danger" data-terminal-action="terminate" data-session="${esc(s.id)}">terminate</button>`
          : ""}
      </span></td>
    </tr>`)
    .join("");
}

async function showTerminalCapture(id) {
  const snap = await jsonFetch(`/api/terminal-sessions/${encodeURIComponent(id)}/capture`);
  store.ui.selectedSegment = null;
  store.ui.terminalCapture = { id, snapshot: snap };
  renderInspector();
}

async function runPaneControl(button) {
  const action = button.getAttribute("data-pane-action");
  const pane = button.getAttribute("data-pane");
  const socket = button.getAttribute("data-pane-socket") || null;
  if (!action || !pane) return;

  if (action === "prompt") {
    const text = window.prompt(`Send prompt to ${pane}`);
    if (!text) return;
    await controlFetch(`/api/panes/${encodeURIComponent(pane)}/prompt`, {
      method: "POST",
      body: JSON.stringify({ text, submit: true, socket }),
    });
    showToast(`prompt sent to ${pane}`);
    return;
  }

  if (action === "abort") {
    if (!window.confirm(`Send Ctrl-C to ${pane}?`)) return;
    await controlFetch(`/api/panes/${encodeURIComponent(pane)}/abort`, {
      method: "POST",
      body: JSON.stringify({ socket }),
    });
    showToast(`abort sent to ${pane}`);
  }
}

async function runTerminalControl(button) {
  const action = button.getAttribute("data-terminal-action");
  const id = button.getAttribute("data-session");
  if (!action || !id) return;

  if (action === "capture") {
    await showTerminalCapture(id);
    return;
  }
  if (action === "input") {
    const data = window.prompt(`Send input to ${id}`);
    if (data == null) return;
    await controlFetch(`/api/terminal-sessions/${encodeURIComponent(id)}/input`, {
      method: "POST",
      body: JSON.stringify({ data: `${data}\r` }),
    });
    showToast(`input sent to ${id}`);
    return;
  }
  if (action === "terminate") {
    if (!window.confirm(`Terminate terminal ${id}?`)) return;
    await controlFetch(`/api/terminal-sessions/${encodeURIComponent(id)}/terminate`, {
      method: "POST",
    });
    showToast(`terminated ${id}`);
    await fetchTerminalSessions();
  }
}

function renderTerminalCapture(capture) {
  const snap = capture.snapshot || {};
  const text = (snap.lines || []).join("\n");
  dom.inspectorMeta.textContent = "terminal";
  dom.inspectorBody.innerHTML = `
    <div class="inspector-title">
      <span>${esc(snap.session?.display_name || snap.session?.id || capture.id)}</span>
      <small>${esc(snap.session?.cwd || "")}</small>
    </div>
    <pre class="terminal-capture"></pre>`;
  dom.inspectorBody.querySelector(".terminal-capture").textContent = text;
}

function renderDataMeta() {
  const scope = selectedSession() || "all sessions";
  dom.dataMeta.textContent = `${store.ui.activeTab} · ${scope}`;
}

function shortPath(path) {
  if (!path || path === "—") return path || "—";
  const parts = path.split("/").filter(Boolean);
  if (parts.length <= 2) return path;
  return `…/${parts.slice(-2).join("/")}`;
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
}

function renderStaticChips() {
  dom.timelineRangeChips.innerHTML = TIMELINE_RANGES.map(
    (range) => `<button class="chip${range === store.filters.timelineRange ? " active" : ""}" data-range="${range}">${range}</button>`
  ).join("");
  dom.timelineRangeChips.querySelectorAll(".chip").forEach((chip) => {
    chip.addEventListener("click", () => {
      setTimelineRange(chip.getAttribute("data-range") || "7d");
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

function setTimelineRange(range, selectedDay = "") {
  store.filters.timelineRange = range;
  store.ui.selectedTimelineDay = selectedDay;
  store.timelineSummary = null;
  store.revisions.timeline += 1;
  syncTimelineRangeChips();
  store.timeline = null;
  renderTimeline();
  fetchTimeline({ force: true }).catch(() => {});
}

function syncTimelineRangeChips() {
  dom.timelineRangeChips.querySelectorAll(".chip").forEach((chip) => {
    chip.classList.toggle("active", chip.getAttribute("data-range") === store.filters.timelineRange);
  });
}

function initDataTabs() {
  const apply = (tab) => {
    store.ui.activeTab = tab === "panes" || tab === "terminals" ? tab : "agents";
    saveValue(DATA_TAB_KEY, store.ui.activeTab);
    dom.dataTabs.querySelectorAll("[data-tab]").forEach((btn) => {
      btn.classList.toggle("active", btn.getAttribute("data-tab") === store.ui.activeTab);
    });
    dom.agentsTab.classList.toggle("active", store.ui.activeTab === "agents");
    dom.panesTab.classList.toggle("active", store.ui.activeTab === "panes");
    dom.terminalsTab.classList.toggle("active", store.ui.activeTab === "terminals");
    renderDataMeta();
    if (store.ui.activeTab === "agents") renderAgents();
    if (store.ui.activeTab === "panes") renderPanes();
    if (store.ui.activeTab === "terminals") {
      renderTerminals();
      fetchTerminalSessions().catch(() => {});
    }
  };
  dom.dataTabs.querySelectorAll("[data-tab]").forEach((btn) => {
    btn.addEventListener("click", () => apply(btn.getAttribute("data-tab")));
  });
  apply(store.ui.activeTab);
}

function initSessionControls() {
  dom.showAllSessions.addEventListener("click", () => setSelectedSession(""));
  dom.sessionSort?.addEventListener("change", () => {
    store.ui.sessionSort = normalizeSessionSort(dom.sessionSort.value);
    store.ui.sessionLimit = SESSION_PAGE_SIZE;
    saveValue(SESSION_SORT_KEY, store.ui.sessionSort);
    renderSessionSidebar();
  });
}

function initDynamicEventDelegation() {
  dom.sessionList.addEventListener("click", (event) => {
    const loadMore = event.target.closest("[data-load-more-sessions]");
    if (loadMore) {
      store.ui.sessionLimit += SESSION_PAGE_SIZE;
      renderSessionSidebar();
      return;
    }
    const row = event.target.closest("[data-session]");
    if (row) setSelectedSession(row.getAttribute("data-session") || "");
  });

  dom.timelineHeatmap.addEventListener("click", (event) => {
    const dayButton = event.target.closest("[data-day]");
    const day = dayButton?.getAttribute("data-day");
    if (day) setTimelineRange(day, day);
  });

  dom.timelineBody.addEventListener("click", (event) => {
    const segment = event.target.closest("[data-segment]");
    if (segment) {
      store.ui.terminalCapture = null;
      store.ui.selectedSegment = {
        detail: segment.getAttribute("data-detail") || "interval",
        source: segment.getAttribute("data-source") || "",
        state: segment.getAttribute("data-state") || "",
        started_at: segment.getAttribute("data-started") || "",
        ended_at: segment.getAttribute("data-ended") || "",
        duration_secs: Number(segment.getAttribute("data-duration") || 0),
        session: segment.getAttribute("data-session") || "",
        pane: segment.getAttribute("data-pane") || "",
      };
      renderInspector();
      return;
    }
    const group = event.target.closest("[data-timeline-group]");
    const key = group?.getAttribute("data-timeline-group");
    if (!key) return;
    if (store.ui.collapsedTimelineGroups.has(key)) {
      store.ui.collapsedTimelineGroups.delete(key);
    } else {
      store.ui.collapsedTimelineGroups.add(key);
    }
    saveSet(COLLAPSED_TIMELINE_GROUPS_KEY, store.ui.collapsedTimelineGroups);
    renderTimeline();
  });

  dom.agentsBody.addEventListener("click", (event) => {
    const button = event.target.closest("[data-pane-action]");
    if (button) runPaneControl(button).catch((error) => showToast(error.message));
  });

  dom.panesBody.addEventListener("click", async (event) => {
    const control = event.target.closest("[data-pane-action]");
    if (control) {
      await runPaneControl(control).catch((error) => showToast(error.message));
      return;
    }
    const copy = event.target.closest("[data-cmd]");
    if (!copy) return;
    const ok = await copyToClipboard(copy.getAttribute("data-cmd") || "");
    showToast(ok ? "copied attach command" : "clipboard blocked — copy manually");
  });

  dom.terminalsBody.addEventListener("click", (event) => {
    const button = event.target.closest("[data-terminal-action]");
    if (button) runTerminalControl(button).catch((error) => showToast(error.message));
  });

  dom.paneSocketChips.addEventListener("click", (event) => {
    const chip = event.target.closest("[data-socket]");
    const socket = chip?.getAttribute("data-socket");
    if (!socket) return;
    if (store.filters.paneSockets.has(socket)) {
      store.filters.paneSockets.delete(socket);
    } else {
      store.filters.paneSockets.add(socket);
    }
    renderPanes();
    renderInspector();
  });
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
      if (panelId === "timeline-panel" && !next) {
        fetchTimeline({ force: true }).catch(() => {});
      }
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
  if (store.ui.terminalCapture) {
    renderTerminalCapture(store.ui.terminalCapture);
    return;
  }
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
    human_presence_secs: store.timeline?.summary?.human_presence_secs ?? humanPresenceSecs(store.timeline?.lanes || []),
  };
  const humanSecs = activeSummary.human_presence_secs ??
    store.timeline?.summary?.human_presence_secs ??
    humanPresenceSecs(store.timeline?.lanes || []);

  dom.inspectorBody.innerHTML = `
    <div class="inspector-title">
      <span>${esc(title)}</span>
      <small>${esc(sessionSummaryLine(activeSummary))}</small>
    </div>
    <div class="inspector-grid">
      ${inspectorMetric("work", activeSummary.working)}
      ${inspectorMetric("wait", activeSummary.waiting)}
      ${inspectorMetric("err", activeSummary.errors)}
      ${inspectorMetric("act", formatDuration(activeSummary.totals.active_secs || 0))}
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

const liveRenderDirty = { panes: false, terminals: false };
let liveRenderFrame = null;

function scheduleLiveRender({ panes = false, terminals = false } = {}) {
  liveRenderDirty.panes ||= panes;
  liveRenderDirty.terminals ||= terminals;
  if (document.hidden || liveRenderFrame) return;
  liveRenderFrame = requestAnimationFrame(() => {
    liveRenderFrame = null;
    if (store.ui.activeTab === "agents") renderAgents();
    if (liveRenderDirty.panes && store.ui.activeTab === "panes") renderPanes();
    if (liveRenderDirty.terminals && store.ui.activeTab === "terminals") renderTerminals();
    liveRenderDirty.panes = false;
    liveRenderDirty.terminals = false;
    renderCounts();
  });
}

async function fetchAgentsSnapshot() {
  const data = await jsonFetch("/api/agents");
  store.agents.clear();
  for (const a of data.agents || []) store.agents.set(a.session_id, a);
  store.revisions.agents += 1;
  scheduleLiveRender();
}

let panesRequest = null;
async function fetchPanes() {
  if (panesRequest) return panesRequest;
  panesRequest = (async () => {
    try {
      const data = await jsonFetch("/api/panes");
      const panes = data.panes || [];
      const errors = data.errors || [];
      const fingerprint = JSON.stringify([panes, errors]);
      if (fingerprint === store.cache.paneFingerprint) {
        scheduleLiveRender();
        return;
      }
      store.cache.paneFingerprint = fingerprint;
      store.panes = panes;
      store.paneErrors = errors;
      store.revisions.panes += 1;
      rebuildPaneIndexes();
      scheduleLiveRender({ panes: true });
    } finally {
      panesRequest = null;
    }
  })();
  return panesRequest;
}

let terminalsRequest = null;
async function fetchTerminalSessions() {
  if (terminalsRequest) return terminalsRequest;
  terminalsRequest = (async () => {
    try {
      const data = await jsonFetch("/api/terminal-sessions");
      const sessions = data.sessions || [];
      const fingerprint = JSON.stringify(sessions);
      if (fingerprint === store.cache.terminalFingerprint) return;
      store.cache.terminalFingerprint = fingerprint;
      store.terminalSessions = sessions;
      scheduleLiveRender({ terminals: true });
    } finally {
      terminalsRequest = null;
    }
  })();
  return terminalsRequest;
}

let timelineRequest = null;
let timelineRequestKey = "";
let timelineRequestController = null;
let lastTimelineFetchAt = 0;
let timelineRefreshPending = false;

function timelineCanRefresh() {
  return !document.hidden &&
    (!store.ui.collapsedPanels.has("timeline-panel") || !store.timelineSummary);
}

function currentTimelineRequestKey() {
  return `${store.filters.timelineRange}\u0000${selectedSession()}`;
}

async function fetchTimeline({ force = false } = {}) {
  if (document.hidden || (!force && !timelineCanRefresh())) return;
  const session = selectedSession();
  const requestKey = currentTimelineRequestKey();
  if (timelineRequest && timelineRequestKey === requestKey) {
    timelineRefreshPending ||= force;
    return timelineRequest;
  }
  if (timelineRequestController) timelineRequestController.abort();

  const params = new URLSearchParams({
    since: store.filters.timelineRange,
    timezone_offset_minutes: String(-new Date().getTimezoneOffset()),
  });
  if (session) {
    params.set("session", session);
  } else {
    params.set("view", "summary");
  }

  const controller = new AbortController();
  timelineRequestController = controller;
  timelineRequestKey = requestKey;
  timelineRequest = (async () => {
    try {
      const data = await jsonFetch(`/api/timeline?${params.toString()}`, { signal: controller.signal });
      if (currentTimelineRequestKey() !== requestKey) return;
      store.timeline = data;
      if (!session) store.timelineSummary = data;
      store.revisions.timeline += 1;
      rebuildTimelineIndexes();
      lastTimelineFetchAt = Date.now();
      renderTimeline();
    } catch (error) {
      if (error?.name !== "AbortError") throw error;
    } finally {
      if (timelineRequestController === controller) {
        timelineRequest = null;
        timelineRequestKey = "";
        timelineRequestController = null;
        if (timelineRefreshPending) {
          timelineRefreshPending = false;
          queueMicrotask(() => fetchTimeline().catch(() => {}));
        }
      }
    }
  })();
  return timelineRequest;
}

async function fetchHealth() {
  const data = await jsonFetch("/api/health");
  dom.version.textContent = `v${data.version} · proto ${data.protocol}`;
}

// ── SSE handlers ──────────────────────────────────────────────────

function applySnapshot(payload) {
  store.agents.clear();
  for (const a of payload.agents || []) store.agents.set(a.session_id, a);
  store.revisions.agents += 1;
  scheduleLiveRender();
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
  store.revisions.agents += 1;
  scheduleLiveRender();
  scheduleTimelineRefresh();
}

let timelineRefreshTimer = null;
function scheduleTimelineRefresh() {
  if (!selectedSession() || timelineRefreshTimer || !timelineCanRefresh()) return;
  const cooldown = Math.max(0, TIMELINE_MIN_REFRESH_INTERVAL_MS - (Date.now() - lastTimelineFetchAt));
  timelineRefreshTimer = setTimeout(() => {
    timelineRefreshTimer = null;
    fetchTimeline().catch(() => {});
  }, Math.max(TIMELINE_EVENT_DEBOUNCE_MS, cooldown));
}

// ── Boot ──────────────────────────────────────────────────────────

async function main() {
  bootstrapToken();
  initAccessControl();
  initCollapseControls();
  initDataTabs();
  initSessionControls();
  initDynamicEventDelegation();
  renderStaticChips();
  setConnectionStatus("connecting", "loading…");

  try {
    await fetchHealth();
    await fetchAccess();
  } catch (_) {
    return; // setConnectionStatus already showed the error
  }

  await Promise.all([
    fetchAgentsSnapshot(),
    fetchPanes(),
    store.ui.activeTab === "terminals" ? fetchTerminalSessions() : Promise.resolve(),
    fetchTimeline({ force: true }),
  ]);

  setTimeout(pollPanes, PANES_REFETCH_INTERVAL_MS);
  setTimeout(pollTerminals, TERMINALS_REFETCH_INTERVAL_MS);
  setTimeout(pollTimeline, TIMELINE_REFETCH_INTERVAL_MS);

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) return;
    scheduleLiveRender({ panes: true, terminals: true });
    fetchPanes().catch(() => {});
    if (store.ui.activeTab === "terminals") fetchTerminalSessions().catch(() => {});
    if (Date.now() - lastTimelineFetchAt >= TIMELINE_MIN_REFRESH_INTERVAL_MS) {
      fetchTimeline().catch(() => {});
    }
  });

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

async function pollPanes() {
  if (!document.hidden) await fetchPanes().catch(() => {});
  setTimeout(pollPanes, PANES_REFETCH_INTERVAL_MS);
}

async function pollTerminals() {
  if (!document.hidden && store.ui.activeTab === "terminals") {
    await fetchTerminalSessions().catch(() => {});
  }
  setTimeout(pollTerminals, TERMINALS_REFETCH_INTERVAL_MS);
}

async function pollTimeline() {
  if (!document.hidden) await fetchTimeline().catch(() => {});
  setTimeout(pollTimeline, TIMELINE_REFETCH_INTERVAL_MS);
}

main();
