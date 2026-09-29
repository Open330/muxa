// A small, fictional muxa fleet for the landing page screenshots. Nothing
// here comes from a real machine: names, paths, prompts and ids are made
// up, and timestamps are relative to `now` so the shots always look live.

const MIN = 60_000;

const WORKSPACES = [
  { id: "acme-api", name: "acme-api", cwd: "/home/dev/acme-api" },
  { id: "web-console", name: "web-console", cwd: "/home/dev/web-console" },
  { id: "infra", name: "infra", cwd: "/home/dev/infra" },
];

// One row per agent pane. `session`/`window` are the tmux coordinates.
const AGENTS = [
  { pane: "%11", session: "api", sid: "$1", window: "@1", widx: "0", wname: "orders", pidx: "0", kind: "claude_code", alias: "planner", state: "waiting_input", ago: 4, ws: "acme-api", prompt: "Plan rate limiting for /v1/orders: per-key token bucket, 429 with Retry-After", title: "Rate limiting plan for orders API" },
  { pane: "%12", session: "api", sid: "$1", window: "@1", widx: "0", wname: "orders", pidx: "1", kind: "codex", alias: "impl", state: "working", ago: 1, ws: "acme-api", prompt: "Implement the token bucket middleware and wire it into the orders router", title: "Token bucket middleware" },
  { pane: "%13", session: "api", sid: "$1", window: "@1", widx: "0", wname: "orders", pidx: "2", kind: "claude_code", alias: "reviewer", state: "waiting_choice", ago: 1, ws: "acme-api", prompt: "Review the middleware diff for race conditions under burst traffic", title: "Review: rate limiter diff" },
  { pane: "%14", session: "api", sid: "$1", window: "@2", widx: "1", wname: "billing", pidx: "0", kind: "codex", alias: "billing", state: "working", ago: 0, ws: "acme-api", prompt: "Migrate billing webhooks to the v2 event schema and keep v1 behind a flag", title: "Billing webhooks v2" },
  { pane: "%21", session: "web", sid: "$2", window: "@3", widx: "0", wname: "checkout", pidx: "0", kind: "claude_code", alias: "e2e", state: "error", ago: 12, ws: "web-console", prompt: "Fix the flaky checkout e2e test that times out on the payment iframe", title: "Flaky checkout e2e" },
  { pane: "%22", session: "web", sid: "$2", window: "@3", widx: "0", wname: "checkout", pidx: "1", kind: "gemini_cli", alias: "a11y", state: "working", ago: 2, ws: "web-console", prompt: "Audit the checkout form for keyboard and screen reader issues", title: "Checkout accessibility audit" },
  { pane: "%23", session: "web", sid: "$2", window: "@4", widx: "1", wname: "settings", pidx: "0", kind: "claude_code", alias: "settings", state: "idle", ago: 38, ws: "web-console", prompt: "Split the settings page into account, team and billing tabs", title: "Settings page tabs" },
  { pane: "%31", session: "infra", sid: "$3", window: "@5", widx: "0", wname: "deploy", pidx: "0", kind: "codex", alias: "deploy", state: "waiting_input", ago: 7, ws: "infra", prompt: "Add a canary stage to the deploy pipeline with automatic rollback on 5xx", title: "Canary deploy stage" },
  { pane: "%32", session: "infra", sid: "$3", window: "@5", widx: "0", wname: "deploy", pidx: "1", kind: "gemini_cli", alias: "alerts", state: "working", ago: 3, ws: "infra", prompt: "Tune the p95 latency alerts so they stop paging on single spikes", title: "Latency alert tuning" },
  { pane: "%33", session: "infra", sid: "$3", window: "@6", widx: "1", wname: "db", pidx: "0", kind: "claude_code", alias: "db", state: "idle", ago: 55, ws: "infra", prompt: "Write the migration that backfills order_totals in batches of 10k", title: "order_totals backfill" },
];

const RESPONSES = {
  planner: "Plan ready: per-API-key token bucket (60/min, burst 20). Should 429s include Retry-After in seconds or as an HTTP date?",
  impl: "Middleware in place; running the orders test suite (212 tests).",
  reviewer: "Found one race: the bucket refill and the read are not atomic. Fix with a Lua script or accept ±1 request drift?",
  billing: "v2 handler done for invoice.paid and invoice.failed; working on subscription.updated.",
  e2e: "The payment iframe loads after 8.2s on CI; the test waits 5s. Error: Timeout 5000ms exceeded.",
  a11y: "3 issues so far: missing label on CVC, focus lost after coupon apply, error text not announced.",
  settings: "Tabs split and routes added. Waiting for the next task.",
  deploy: "Canary stage added (10% for 15 min). Roll back automatically on >1% 5xx, or also on p95 > 800ms?",
  alerts: "Switched p95 alerts to a 5-minute window; replaying last week's pages to compare.",
  db: "Migration written and dry-run on a snapshot: 4.1M rows in 412 batches, ~6 min.",
};

const WORKS = [
  { ws: "acme-api", id: "API-142", title: "Rate limit the orders API", goal: "Stop one noisy client from starving everyone else; 429 with Retry-After.", next: "Answer the planner: Retry-After as seconds.", stage: "in_progress", signals: ["attention"], panes: ["%11", "%12", "%13"], ext: { source: "github", display_key: "#142", title: "Orders API needs rate limiting" } },
  { ws: "acme-api", id: "API-151", title: "Billing webhooks v2", goal: "Move to the v2 event schema without breaking v1 consumers.", stage: "in_progress", signals: [], panes: ["%14"], ext: { source: "linear", display_key: "BIL-37", title: "Webhook schema v2" } },
  { ws: "web-console", id: "WEB-88", title: "Fix flaky checkout e2e", goal: "Checkout e2e passes 50 runs in a row on CI.", stage: "in_progress", signals: ["error"], panes: ["%21", "%22"] },
  { ws: "web-console", id: "WEB-91", title: "Settings page tabs", stage: "review", signals: [], panes: ["%23"] },
  { ws: "infra", id: "OPS-19", title: "Canary deploys with auto rollback", goal: "Every deploy goes through a 10% canary that rolls itself back.", stage: "in_progress", signals: ["attention"], panes: ["%31", "%32"] },
  { ws: "infra", id: "OPS-23", title: "Backfill order_totals", stage: "done", signals: [], panes: ["%33"] },
  { ws: "acme-api", id: "API-160", title: "Idempotency keys for POST /payments", goal: "Retried payment requests never charge twice.", stage: "queued", signals: [], panes: [] },
  { ws: "web-console", id: "WEB-95", title: "Dark mode for the console", stage: "queued", signals: [], panes: [] },
];

const iso = (now, minutesAgo) => new Date(now - minutesAgo * MIN).toISOString();

function agentJson(a, now) {
  const ws = WORKSPACES.find((w) => w.id === a.ws);
  const waiting = a.state.startsWith("waiting") || a.state === "error";
  return {
    kind: a.kind,
    agent_session_id: `demo-${a.alias}`,
    pane: a.pane,
    tmux_socket: "default",
    tmux_session: a.session,
    cwd: ws.cwd,
    state: a.state,
    ai_title: a.title,
    last_prompt: a.prompt,
    last_prompt_at: iso(now, a.ago + 6),
    last_response: RESPONSES[a.alias],
    recap: RESPONSES[a.alias],
    last_notification: waiting ? RESPONSES[a.alias] : null,
    model: a.kind === "codex" ? "gpt-5.1-codex" : a.kind === "gemini_cli" ? "gemini-2.5-pro" : "claude-opus-4-1",
    context_used_pct: 20 + (a.pane.charCodeAt(2) % 7) * 9,
    cost_usd: a.kind === "claude_code" ? 0.4 + (a.ago % 5) * 0.37 : null,
    rate_limit_5h_pct: 12 + (a.ago % 6) * 8,
    rate_limit_5h_resets_at: iso(now, -140),
    started_at: iso(now, 180 + a.ago),
    last_activity_at: iso(now, a.ago),
    state_entered_at: iso(now, a.ago),
    workload: { primary_pid: 4000 + Number(a.pane.slice(1)), process_count: 2, shell_count: 1, subagent_count: 0, helper_count: 0, preview: [] },
  };
}

function paneJson(a) {
  const ws = WORKSPACES.find((w) => w.id === a.ws);
  return {
    host: "tmux",
    pane_id: a.pane,
    session_id: a.sid,
    session: a.session,
    window_id: a.window,
    window_name: a.wname,
    window_index: a.widx,
    pane_index: a.pidx,
    tty: `/dev/ttys0${a.pane.slice(1)}`,
    current_command: a.kind === "codex" ? "codex" : a.kind === "gemini_cli" ? "gemini" : "claude",
    title: a.title,
    current_path: ws.cwd,
    socket: "default",
    muxa: { managed_workspace: false, managed_work: false, managed_agent: false },
    attach_command: `tmux attach-session -t ${a.session} \\; select-pane -t ${a.pane}`,
  };
}

function runsFor(paneIds, now, work) {
  const byWindow = new Map();
  for (const id of paneIds) {
    const a = AGENTS.find((x) => x.pane === id);
    if (!byWindow.has(a.window)) byWindow.set(a.window, []);
    byWindow.get(a.window).push(a);
  }
  return [...byWindow.values()].map((agents) => {
    const first = agents[0];
    const ws = WORKSPACES.find((w) => w.id === first.ws);
    const states = agents.map((a) => a.state);
    const state = states.includes("error") ? "failed"
      : states.some((s) => s.startsWith("waiting")) ? "waiting"
        : states.includes("working") ? "running" : "idle";
    return {
      id: `tmux:default:${first.sid}:${first.window}`,
      state,
      linked: Boolean(work),
      ...(work ? { work } : {}),
      execution: { host: "tmux", socket: "default", session_id: first.sid, window_id: first.window },
      session_name: first.session,
      window_name: first.wname,
      window_index: first.widx,
      cwd: ws.cwd,
      latest_at: iso(now, Math.min(...agents.map((a) => a.ago))),
      panes: agents.map((a) => ({
        pane_id: a.pane,
        pane_index: a.pidx,
        current_command: paneJson(a).current_command,
        title: a.title,
        current_path: ws.cwd,
        attach_command: paneJson(a).attach_command,
        role: a.alias,
        agent: agentJson(a, now),
      })),
    };
  });
}

export function worksJson(now = Date.now()) {
  const works = WORKS.map((w, index) => {
    const identity = { workspace_id: w.ws, work_id: w.id };
    const runs = runsFor(w.panes, now, identity);
    return {
      identity,
      title: w.title,
      ...(w.goal ? { goal: w.goal } : {}),
      ...(w.next ? { next_action: w.next } : {}),
      stage: w.stage,
      signals: w.signals,
      external_items: w.ext ? [w.ext] : [],
      runs,
      participants: w.panes.length,
      latest_at: iso(now, 1 + index * 3),
      source: "managed",
      metadata: { stage: "auto", updated_at: iso(now, 30) },
    };
  });
  return {
    schema_version: 2,
    generated_at: new Date(now).toISOString(),
    workspaces: WORKSPACES.map((ws) => {
      const mine = works.filter((w) => w.identity.workspace_id === ws.id);
      return {
        ...ws,
        work_count: mine.length,
        attention_count: mine.filter((w) => w.signals.length).length,
        active_runs: mine.reduce((n, w) => n + w.runs.length, 0),
      };
    }),
    works,
    unlinked_executions: [],
  };
}

export const agentsJson = (now = Date.now()) => ({ agents: AGENTS.map((a) => agentJson(a, now)) });
export const panesJson = (now = Date.now()) => ({ panes: AGENTS.map(paneJson), errors: [], fetched_at: new Date(now).toISOString() });

function participant(alias) {
  const a = AGENTS.find((x) => x.alias === alias);
  const ws = WORKSPACES.find((w) => w.id === a.ws);
  return {
    agent_kind: a.kind,
    agent_session_id: `demo-${a.alias}`,
    alias: a.alias,
    cwd: ws.cwd,
    pane: a.pane,
    room: { host: "tmux", socket: "default", window_id: a.window },
    socket: "default",
    state: a.state,
    tmux_session_id: a.sid,
    tmux_session_name: a.session,
    window_name: a.wname,
  };
}

// [from, to, kind, status, minutes ago, body, reply?]
const MESSAGES = [
  ["planner", "impl", "task", "completed", 95, "Implement the token bucket middleware per the plan in docs/rate-limit.md.", "Done. Middleware + 18 tests; orders suite green."],
  ["impl", "reviewer", "review", "completed", 70, "Please review the rate limiter diff (src/middleware/limit.ts).", "One race between refill and read. See comments."],
  ["reviewer", "impl", "task", "completed", 64, "Make refill + read atomic (Lua script).", "Switched to a Lua script; burst test passes 1000/1000."],
  ["impl", "reviewer", "review", "claimed", 20, "Second pass on the atomic refill, please.", null],
  ["planner", "billing", "question", "completed", 88, "Does the v2 webhook change touch the orders tables?", "No, billing only. It reads orders through the public API."],
  ["e2e", "a11y", "notice", "completed", 40, "Heads up: I'm changing checkout selectors, re-run your audit after.", "Ack, will re-run after your PR."],
  ["a11y", "e2e", "question", "queued", 9, "Is the payment iframe title stable? I need it for the audit.", null],
  ["deploy", "alerts", "task", "completed", 58, "Give me a p95 threshold I can use for canary rollback.", "Use 800ms over 5 minutes; single spikes stay below that."],
  ["alerts", "deploy", "notice", "completed", 30, "Alert rules changed; canary rollback now reads p95_5m.", "Thanks, wired in."],
  ["db", "deploy", "question", "completed", 120, "Can the backfill run during the canary window?", "Yes, it only touches order_totals."],
  ["planner", "reviewer", "review", "completed", 110, "Sanity-check the rate limit plan before we build it.", "Plan is fine; add a per-IP fallback for anonymous calls."],
  ["impl", "planner", "question", "blocked", 6, "Retry-After as seconds or HTTP date?", null],
];

export function collaborationJson(now = Date.now()) {
  const requests = MESSAGES.map(([from, to, kind, status, ago, body, reply], i) => ({
    id: `req_demo_${String(i + 1).padStart(2, "0")}`,
    run_id: `run_demo_${i + 1}`,
    thread_id: `thr_demo_${from}_${to}`,
    kind,
    status,
    body,
    expects_reply: Boolean(reply) || status !== "completed",
    work_mode: "same_work",
    created_at: iso(now, ago),
    from: participant(from),
    to: participant(to),
    ...(reply ? { reply: { at: iso(now, ago - 4), body: reply, status: "completed" } } : {}),
  }));
  return {
    generated_at: new Date(now).toISOString(),
    details_included: true,
    requests,
    pagination: { total: requests.length, limit: 500, has_more: false },
  };
}

const zeroTotals = () => ({ active_secs: 0, working_secs: 0, waiting_secs: 0, error_secs: 0, idle_secs: 0, starting_secs: 0, stopped_secs: 0, human_secs: 0, foreground_secs: 0 });

export function timelineJson(now = Date.now()) {
  const totals = { ...zeroTotals(), active_secs: 61200, working_secs: 41800, waiting_secs: 9400, error_secs: 1800, idle_secs: 8200, human_secs: 7300, foreground_secs: 22100 };
  const days = Array.from({ length: 7 }, (_, i) => {
    const d = new Date(now - (6 - i) * 86_400_000);
    const scale = [0.5, 0.9, 1.2, 0.8, 1.4, 0.3, 1][i];
    return {
      date: d.toISOString().slice(0, 10),
      totals: { ...zeroTotals(), active_secs: Math.round(8700 * scale), working_secs: Math.round(6000 * scale) },
      top_sessions: [{ label: "api", active_secs: Math.round(4200 * scale) }],
    };
  });
  const sessions = ["api", "web", "infra"].map((label, i) => ({
    label, lanes: [4, 3, 3][i], latest_at: iso(now, i * 3),
    totals: { ...zeroTotals(), active_secs: [28000, 19000, 14200][i], working_secs: [19000, 13500, 9300][i] },
    human_presence_secs: [3100, 2500, 1700][i],
  }));
  return {
    generated_at: new Date(now).toISOString(),
    range: { label: "7d", since_at: iso(now, 7 * 1440) },
    window_started_at: iso(now, 7 * 1440),
    window_ended_at: new Date(now).toISOString(),
    lanes: [],
    totals,
    active_sessions: sessions.map((s) => ({ label: s.label, active_secs: s.totals.active_secs })),
    notes: [],
    summary: {
      version: 1,
      sessions,
      days,
      sources: [{ kind: "agent", lanes: 10, sessions: 3, totals }],
      human_presence_secs: 7300,
    },
  };
}

export const healthJson = () => ({ ok: true, version: "0.8.54", protocol: 6 });
export const accessJson = () => ({
  mode: "token",
  read_requires_token: true,
  write_available: true,
  write_authorized: true,
  capabilities: { work_start: true, pane_sharing: false },
  login: { available: false, signed_in: false, email: null, role: "operator", via: null, enrollment: false, login_url: null },
});
