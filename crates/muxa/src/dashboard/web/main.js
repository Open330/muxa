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
const PANES_REFETCH_INTERVAL_MS = 5000;

const AGENT_STATES = ["working", "waiting_input", "idle", "starting", "error", "stopped"];
const AGENT_KINDS = ["claude_code", "codex", "gemini_cli", "opencode", "unknown"];

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
  agentsBody: document.getElementById("agents-tbody"),
  panesBody: document.getElementById("panes-tbody"),
  agentStateChips: document.getElementById("agent-state-chips"),
  agentKindChips: document.getElementById("agent-kind-chips"),
  paneSocketChips: document.getElementById("pane-socket-chips"),
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

// ── State ─────────────────────────────────────────────────────────

const store = {
  agents: new Map(), // session_id -> Agent
  panes: [], // PaneSummary[]
  paneErrors: [], // ScanError[]
  filters: {
    agentStates: new Set(AGENT_STATES),
    agentKinds: new Set(AGENT_KINDS),
    paneSockets: new Set(), // populated dynamically
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
}

function renderAgents() {
  const rows = [...store.agents.values()].filter(
    (a) =>
      store.filters.agentStates.has(a.state) &&
      store.filters.agentKinds.has(a.kind)
  );
  rows.sort((x, y) => (y.last_activity_at || "").localeCompare(x.last_activity_at || ""));

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

  const rows = store.panes.filter((p) => store.filters.paneSockets.has(p.socket));
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
    });
  });
}

function renderStaticChips() {
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
    });
  });
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
}

// ── Boot ──────────────────────────────────────────────────────────

async function main() {
  bootstrapToken();
  renderStaticChips();
  setConnectionStatus("connecting", "loading…");

  try {
    await fetchHealth();
  } catch (_) {
    return; // setConnectionStatus already showed the error
  }

  await Promise.all([fetchAgentsSnapshot(), fetchPanes()]);

  // Periodically refetch panes — they are pull-only on the server.
  setInterval(() => {
    fetchPanes().catch(() => {
      /* swallowed; SSE indicator will reflect downtime */
    });
  }, PANES_REFETCH_INTERVAL_MS);

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
