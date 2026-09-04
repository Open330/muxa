import { barshelf, ui, type UINode, type WidgetLoadContext } from "barshelf";

type AgentState =
  | "starting"
  | "working"
  | "idle"
  | "waiting_input"
  | "waiting_choice"
  | "error"
  | "stopped";

/// Normalized child-process rollup. muxa has shipped two field spellings for
/// this (see `parseWorkload`), so the widget keeps its own shape and the
/// already-derived `other` count rather than re-deriving it at render time.
interface MuxaWorkload {
  subagents: number;
  shells: number;
  other: number;
}

interface MuxaSubagent {
  kind: string;
  description: string | null;
}

/// Which tier of `pickSummary` produced the caption.
type SummarySource = "recap" | "title" | "prompt" | "notification";

interface MuxaSummary {
  text: string;
  source: SummarySource;
}

interface MuxaAgent {
  kind: string;
  sessionID: string;
  state: AgentState;
  pane: string | null;
  /// Where the agent lives, as `session · window.pane` — built from the
  /// topology tree rather than read from a field, because muxa dropped the
  /// flat `location` string when it went nested.
  location: string | null;
  cwd: string | null;
  summary: MuxaSummary | null;
  contextUsedPct: number | null;
  workload: MuxaWorkload | null;
  subagents: MuxaSubagent[];
  lastActivityAt: string;
}

interface MuxaSnapshot {
  agents: MuxaAgent[];
}

interface StateStyle {
  label: string;
  tone: string;
}

interface PreparedView {
  root: UINode;
  status: { label: string; tooltip: string };
}

/// Why a source has no snapshot. Classified once where the failure happens
/// so the card and the empty-state never re-parse each other's prose.
type SourceFault = "missing" | "outdatedCli" | "outdatedWidget" | "offline";

interface SourceResult {
  key: string;
  label: string;
  kind: "local" | "ssh" | "fleet";
  host?: string;
  snapshot?: MuxaSnapshot;
  error?: string;
  fault?: SourceFault;
}

interface SSHHostSetting {
  host?: string;
  error?: string;
}

const runtimeState = {
  hasRendered: false,
  sourceSignature: "",
};

const SSH_OPTIONS = [
  "-o",
  "BatchMode=yes",
  "-o",
  "ConnectTimeout=3",
  "--",
] as const;

const STATE_STYLE: Record<AgentState, StateStyle> = {
  error: { label: "Error", tone: "danger" },
  waiting_choice: { label: "Choose", tone: "warning" },
  waiting_input: { label: "Needs input", tone: "warning" },
  working: { label: "Working", tone: "good" },
  starting: { label: "Starting", tone: "accent" },
  idle: { label: "Idle", tone: "neutral" },
  stopped: { label: "Stopped", tone: "neutral" },
};

// ----------------------------------------------------------------- scalars

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function isAgentState(value: unknown): value is AgentState {
  return typeof value === "string" && value in STATE_STYLE;
}

function optionalText(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

/// tmux indices arrive as strings, but a backend that starts emitting them as
/// numbers should not silently blank a row's location.
function scalarText(value: unknown): string | null {
  if (typeof value === "string") return value === "" ? null : value;
  if (typeof value === "number" && Number.isFinite(value)) return String(value);
  return null;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function count(value: unknown): number {
  const number = optionalNumber(value);
  return number === null ? 0 : Math.max(0, Math.round(number));
}

function firstLine(value: string): string {
  return value.split("\n", 1)[0] ?? value;
}

function oneLine(value: string, maxLength = 160): string {
  const compact = value.replace(/\s+/g, " ").trim();
  return compact.length > maxLength
    ? `${compact.slice(0, maxLength - 1)}…`
    : compact;
}

function basename(path: string | null): string | null {
  if (!path) return null;
  const parts = path.split("/").filter(Boolean);
  return parts.at(-1) ?? path;
}

function age(iso: string, now: number): string {
  const timestamp = Date.parse(iso);
  if (!Number.isFinite(timestamp)) return "now";
  const seconds = Math.max(0, Math.floor((now - timestamp) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

// ------------------------------------------------------------- agent parse

/// The caption falls through recap → session title → prompt, the same
/// precedence `muxa watch` uses for its SUMMARY column, so the shelf and the
/// console describe an agent identically. Recaps are sparse (Claude Code
/// writes one only when you come back after being away; Codex prints one after
/// context compaction), and rows such as Gemini still have no recap source,
/// which is exactly why every tier degrades instead of blanking.
function pickSummary(value: Record<string, unknown>): MuxaSummary | null {
  const tiers: Array<[SummarySource, unknown]> = [
    ["recap", value.recap],
    ["title", value.ai_title],
    ["prompt", value.last_prompt],
    ["notification", value.last_notification],
  ];
  for (const [source, raw] of tiers) {
    const text = optionalText(raw);
    if (text) return { text: firstLine(text), source };
  }
  return null;
}

/// muxa 0.8.19 shipped `{subagents, shells, processes}` on the old flat status
/// payload; the nested topology serializes the daemon's own `WorkloadSummary`
/// as `{subagent_count, shell_count, process_count, …}`. Read both — an SSH
/// host can be several releases behind the Mac running this widget.
function parseWorkload(value: unknown): MuxaWorkload | null {
  if (!isRecord(value)) return null;
  const subagents = count(value.subagent_count ?? value.subagents);
  const shells = count(value.shell_count ?? value.shells);
  const total = count(value.process_count ?? value.processes);
  // `process_count` is the whole tracked tree, so the leftover is what is
  // neither a subagent nor a shell. Matches muxa's `workload_other_count`.
  const other = Math.max(total - subagents - shells, 0);
  return subagents + shells + other > 0 ? { subagents, shells, other } : null;
}

function parseSubagents(value: unknown): MuxaSubagent[] {
  return asArray(value).flatMap((item) =>
    isRecord(item) && typeof item.kind === "string"
      ? [{ kind: item.kind, description: optionalText(item.description) }]
      : []
  );
}

/// `agent_session_id` is the canonical name; `session_id` is the older
/// spelling still emitted by remote hosts on a pre-topology muxa.
function parseAgent(
  value: unknown,
  location: string | null,
): MuxaAgent | null {
  if (
    !isRecord(value) ||
    typeof value.kind !== "string" ||
    !isAgentState(value.state) ||
    typeof value.last_activity_at !== "string"
  ) {
    return null;
  }

  const sessionID = optionalText(value.agent_session_id) ??
    optionalText(value.session_id) ?? "";
  return {
    kind: value.kind,
    sessionID,
    state: value.state,
    pane: optionalText(value.pane),
    // The flat payload carried its own `location`; the nested one is joined
    // from the tree by the caller.
    location: location ?? optionalText(value.location),
    cwd: optionalText(value.cwd),
    summary: pickSummary(value),
    contextUsedPct: optionalNumber(value.context_used_pct),
    workload: parseWorkload(value.workload),
    subagents: parseSubagents(value.subagents),
    lastActivityAt: value.last_activity_at,
  };
}

// ---------------------------------------------------------------- topology

/// One pane, flattened out of whichever shape the host sent. Both the nested
/// `status --json` tree and the flat `fleet status --json` pane inventory
/// collapse to this, so location naming has exactly one implementation.
interface PaneRow {
  /// Collision-free pane identity. A bare tmux pane id repeats across
  /// servers — `%1` exists on both the `default` and `amux` sockets — so it
  /// is never used on its own as a key.
  key: string;
  paneID: string;
  socket: string | null;
  session: string;
  /// Groups panes by window. Built with `JSON.stringify` rather than string
  /// concatenation so a session or window name containing the separator
  /// cannot merge two different windows into one group.
  windowKey: string;
  windowLabel: string;
  paneIndex: string;
  agent?: unknown;
}

function windowKey(socket: string | null, session: string, window: string): string {
  return JSON.stringify([socket, session, window]);
}

function paneKey(socket: string | null, paneID: string): string {
  return JSON.stringify([socket, paneID]);
}

function paneRowsFromTopology(value: Record<string, unknown>): PaneRow[] {
  const rows: PaneRow[] = [];
  for (const session of asArray(value.sessions)) {
    if (!isRecord(session)) continue;
    const sessionName = optionalText(session.name) ?? "";
    for (const window of asArray(session.windows)) {
      if (!isRecord(window)) continue;
      const index = scalarText(window.index);
      const label = optionalText(window.name) ?? (index ? `#${index}` : "");
      for (const pane of asArray(window.panes)) {
        if (!isRecord(pane)) continue;
        // PaneKey nests WindowKey → SessionKey, and SessionKey flattens its
        // backend endpoint, so the server socket sits on the session object.
        const key = isRecord(pane.key) ? pane.key : {};
        const windowRef = isRecord(key.window) ? key.window : {};
        const sessionRef = isRecord(windowRef.session) ? windowRef.session : {};
        const socket = optionalText(sessionRef.socket);
        const paneID = optionalText(key.pane_id) ?? "";
        rows.push({
          key: paneKey(socket, paneID),
          paneID,
          socket,
          session: sessionName,
          windowKey: windowKey(socket, sessionName, index ?? label),
          windowLabel: label,
          paneIndex: scalarText(pane.index) ?? "",
          agent: pane.agent,
        });
      }
    }
  }
  return rows;
}

/// `fleet status --json` reports each host's raw pane inventory (`PaneInfo`)
/// alongside a flat agent list, rather than a joined tree.
function paneRowsFromPaneInfo(value: unknown): PaneRow[] {
  return asArray(value).flatMap((pane) => {
    if (!isRecord(pane)) return [];
    const paneID = optionalText(pane.pane_id);
    if (!paneID) return [];
    const socket = optionalText(pane.socket);
    const session = optionalText(pane.session) ?? "";
    const index = scalarText(pane.window_index);
    const label = optionalText(pane.window_name) ?? (index ? `#${index}` : "");
    return [{
      key: paneKey(socket, paneID),
      paneID,
      socket,
      session,
      windowKey: windowKey(socket, session, index ?? label),
      windowLabel: label,
      paneIndex: scalarText(pane.pane_index) ?? "",
    }];
  });
}

/// Name each occupied pane as briefly as it can still be told apart: the
/// session alone when it holds one agent, `session · window` once a session
/// runs several, and a trailing `.pane` only when one window holds more than
/// one agent. A tmux coordinate like `muxa:12.3` is unambiguous but tells you
/// nothing; `barshelf · review` is what you actually recognize.
function buildLocations(
  rows: PaneRow[],
  occupied: Set<string>,
): Map<string, string> {
  const active = rows.filter((row) => occupied.has(row.key));
  const windowsBySession = new Map<string, Set<string>>();
  const panesByWindow = new Map<string, number>();
  for (const row of active) {
    let windows = windowsBySession.get(row.session);
    if (!windows) {
      windows = new Set();
      windowsBySession.set(row.session, windows);
    }
    windows.add(row.windowKey);
    panesByWindow.set(row.windowKey, (panesByWindow.get(row.windowKey) ?? 0) + 1);
  }

  const locations = new Map<string, string>();
  for (const row of active) {
    const parts: string[] = [];
    if (row.session) parts.push(row.session);
    if ((windowsBySession.get(row.session)?.size ?? 0) > 1 && row.windowLabel) {
      parts.push(row.windowLabel);
    }
    let label = parts.join(" · ");
    if ((panesByWindow.get(row.windowKey) ?? 0) > 1 && row.paneIndex) {
      label = label ? `${label}.${row.paneIndex}` : row.paneIndex;
    }
    if (label) locations.set(row.key, label);
  }
  return locations;
}

/// Join a pane inventory to a flat agent list, which is how a fleet host
/// reports its state. The join is on the agent's full `(socket, pane)`
/// identity; an agent that predates `tmux_socket`, or whose pane is unknown
/// or already claimed, stays in the list without a location rather than being
/// attached by guess. That mirrors muxa's own rule — a pane id alone resolves
/// only when exactly one server on the host offers it.
function joinAgents(rows: PaneRow[], agents: unknown[]): {
  rows: PaneRow[];
  loose: unknown[];
} {
  const byKey = new Map<string, PaneRow>();
  const byPaneID = new Map<string, PaneRow[]>();
  for (const row of rows) {
    const copy = { ...row };
    byKey.set(copy.key, copy);
    byPaneID.set(copy.paneID, [...(byPaneID.get(copy.paneID) ?? []), copy]);
  }

  const loose: unknown[] = [];
  for (const agent of agents) {
    if (!isRecord(agent)) {
      loose.push(agent);
      continue;
    }
    const paneID = optionalText(agent.pane);
    if (!paneID) {
      loose.push(agent);
      continue;
    }
    const socket = optionalText(agent.tmux_socket);
    const candidates = byPaneID.get(paneID) ?? [];
    const row = socket
      ? byKey.get(paneKey(socket, paneID))
      : candidates.length === 1
      ? candidates[0]
      : undefined;
    if (row && row.agent === undefined) {
      row.agent = agent;
    } else {
      loose.push(agent);
    }
  }
  return { rows: [...byKey.values()], loose };
}

/// Collapse pane rows plus any tree-external agents into parsed agents.
/// Throws when a payload plainly carries agents the widget could not read —
/// that is real schema drift, and it has to surface rather than render empty.
function collectAgents(rows: PaneRow[], loose: unknown[]): MuxaSnapshot {
  const occupied = new Set(
    rows
      .filter((row) => isRecord(row.agent) && row.paneID !== "")
      .map((row) => row.key),
  );
  const locations = buildLocations(rows, occupied);

  const raw: Array<[unknown, string | null]> = [
    ...rows
      .filter((row) => row.agent !== undefined && row.agent !== null)
      .map((row) =>
        [row.agent, locations.get(row.key) ?? null] as [
          unknown,
          string | null,
        ]
      ),
    ...loose.map((agent) => [agent, null] as [unknown, string | null]),
  ];

  const agents = raw.flatMap(([value, location]) => {
    const agent = parseAgent(value, location);
    return agent ? [agent] : [];
  });
  if (agents.length === 0 && raw.length > 0) {
    throw new Error("muxa reports agents this widget cannot read");
  }
  return { agents };
}

/// muxa 0.8.32 replaced the flat `agents` array with the canonical
/// session → window → pane topology, and restarted `schema_version` at 1 for
/// it — so a version number no longer identifies the payload, the shape does.
/// Both are read: a remote SSH host may still be on the flat one.
function parseStatus(value: unknown): MuxaSnapshot {
  if (!isRecord(value)) throw new Error("unexpected muxa status payload");
  if (Array.isArray(value.sessions)) {
    return collectAgents(
      paneRowsFromTopology(value),
      asArray(value.unassigned_agents),
    );
  }
  if (Array.isArray(value.agents)) {
    return collectAgents([], value.agents);
  }
  throw new Error("unexpected muxa status payload");
}

// ------------------------------------------------------------------- fleet

const FLEET_STATE_MESSAGE: Record<string, string> = {
  disabled: "Disabled in muxa config",
  connecting: "Connecting…",
  degraded: "Degraded — snapshot may be stale",
  offline: "Offline",
  auth_failed: "SSH authentication failed",
  version_skew: "muxa version mismatch — upgrade this host",
};

/// One section per muxa host, from the controller's own aggregate. The local
/// daemon already holds every host's snapshot, so this stays a single local
/// call no matter how many machines are in the fleet — the widget itself
/// never opens an SSH connection in this mode.
function parseFleet(value: unknown): SourceResult[] {
  if (!isRecord(value) || !Array.isArray(value.hosts)) {
    throw new Error("unexpected muxa fleet payload");
  }

  return value.hosts.flatMap((host, index): SourceResult[] => {
    if (!isRecord(host)) return [];
    const alias = optionalText(host.alias) ?? `host-${index}`;
    const local = host.local === true;
    const base = {
      key: `fleet-${alias.replace(/[^A-Za-z0-9_-]/g, "-")}`,
      label: local ? "Local" : alias,
      kind: "fleet" as const,
      host: local ? undefined : alias,
    };

    const state = optionalText(host.state) ?? "offline";
    if (state !== "online") {
      return [{
        ...base,
        error: FLEET_STATE_MESSAGE[state] ?? `Host is ${state}`,
        fault: "offline" as const,
      }];
    }

    const remote = isRecord(host.remote) ? host.remote : null;
    if (!remote) {
      return [{
        ...base,
        error: "Waiting for the first snapshot",
        fault: "offline" as const,
      }];
    }

    try {
      const joined = joinAgents(
        paneRowsFromPaneInfo(remote.panes),
        asArray(remote.agents),
      );
      return [{
        ...base,
        snapshot: collectAgents(joined.rows, joined.loose),
      }];
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return [{
        ...base,
        error: oneLine(message, 120),
        fault: "outdatedWidget" as const,
      }];
    }
  });
}

// ---------------------------------------------------------------- settings

function booleanSetting(value: unknown, fallback: boolean): boolean {
  return typeof value === "boolean" ? value : fallback;
}

function integerSetting(
  value: unknown,
  fallback: number,
  min: number,
  max: number,
): number {
  const number = typeof value === "number" ? Math.round(value) : fallback;
  return Math.min(Math.max(number, min), max);
}

/// Fleet is opt-in. Anything else — including an unset value on a card that
/// predates this setting — keeps the original single-host behaviour, so an
/// upgrade never silently repoints an existing card at a different source.
function fleetSelected(value: unknown): boolean {
  return typeof value === "string" && value.trim().toLowerCase() === "fleet";
}

function parseSSHHost(value: unknown): SSHHostSetting {
  if (typeof value !== "string" || value.trim() === "") {
    return {};
  }

  const host = value.trim();
  if (
    host.length > 255 || host.startsWith("-") ||
    !/^[A-Za-z0-9][A-Za-z0-9._:@%+-]*$/.test(host)
  ) {
    return { error: `Invalid SSH host: ${oneLine(host, 48)}` };
  }
  return { host };
}

// -------------------------------------------------------------------- rows

function kindLabel(kind: string): string {
  switch (kind) {
    case "claude_code":
      return "Claude Code";
    case "codex":
      return "Codex";
    case "gemini_cli":
      return "Gemini CLI";
    // `agy`, the Gemini CLI's successor. A distinct muxa AgentKind rather
    // than a rename — the two ship different hooks and install side by side.
    case "antigravity":
      return "Antigravity";
    case "opencode":
      return "opencode";
    case "task":
      return "Task";
    case "unknown":
      return "Agent";
    default:
      return kind.replaceAll("_", " ");
  }
}

function agentTitle(agent: MuxaAgent): string {
  if (agent.location && agent.location !== "-") return agent.location;
  // Paneless background tasks have no place in the tree; muxa falls back to
  // the name they registered under, so match it.
  if (agent.kind === "task" && agent.sessionID) return agent.sessionID;
  return basename(agent.cwd) ?? kindLabel(agent.kind);
}

/// Agents that need the user float above the rest; ties fall back to
/// most-recent activity so the list still reads chronologically.
const ATTENTION_RANK: Record<AgentState, number> = {
  error: 0,
  waiting_choice: 1,
  waiting_input: 1,
  working: 2,
  starting: 2,
  idle: 3,
  stopped: 4,
};

function needsAttention(state: AgentState): boolean {
  return ATTENTION_RANK[state] <= 1;
}

/// Second line: what the agent is / why it wants you, then what it is doing.
/// Attention states lead with their label in the state's tone so waiting rows
/// read at a glance; calm rows lead with the agent kind.
function agentSubtitle(
  agent: MuxaAgent,
  showSummary: boolean,
): { text: string; tone: string } {
  const style = STATE_STYLE[agent.state];
  const attention = needsAttention(agent.state);
  const lead = attention ? style.label : kindLabel(agent.kind);
  const summary = showSummary ? agent.summary : null;
  return {
    text: summary ? `${lead} · ${summary.text}` : lead,
    tone: attention ? style.tone : "secondary",
  };
}

/// `◇n` for hook-tracked Task subagents, else the process-tree rollup
/// (`◇` subagents, `▸` shells, `+` other). Same glyphs as
/// `muxa watch --view swarm`, so the shelf and the console read alike.
function loadBadge(agent: MuxaAgent): string | null {
  if (agent.subagents.length > 0) return `◇${agent.subagents.length}`;
  const workload = agent.workload;
  if (!workload) return null;

  const parts: string[] = [];
  if (workload.subagents > 0) parts.push(`◇${workload.subagents}`);
  if (workload.shells > 0) parts.push(`▸${workload.shells}`);
  // A bare `+n` of plain child processes is true of nearly every live agent,
  // so it would ride along on every row without saying anything. The badge
  // earns its place only when the agent has help — subagents or shells.
  if (parts.length === 0) return null;
  if (workload.other > 0) parts.push(`+${workload.other}`);
  return parts.join(" ");
}

/// Context pressure is only worth a cell once it is close to costing the user
/// something, so it stays hidden until the window is most of the way full and
/// turns warning-toned right before a compaction.
const CONTEXT_NOTICE_PCT = 70;
const CONTEXT_WARN_PCT = 90;

function contextNote(agent: MuxaAgent): { text: string; tone: string } | null {
  const pct = agent.contextUsedPct;
  if (pct === null || pct < CONTEXT_NOTICE_PCT) return null;
  return {
    text: `ctx ${Math.round(pct)}%`,
    tone: pct >= CONTEXT_WARN_PCT ? "warning" : "tertiary",
  };
}

const MAX_SUBAGENT_ROWS = 3;

/// Indented `└─ kind  description` lines under an agent that has named
/// subagents in flight. Newest wins when there are more than fit.
function subagentRows(agent: MuxaAgent, rowID: string): UINode[] {
  if (agent.subagents.length === 0) return [];
  const visible = agent.subagents.slice(-MAX_SUBAGENT_ROWS);
  const hidden = agent.subagents.length - visible.length;

  return visible.map((subagent, index) => {
    const last = index === visible.length - 1;
    const label = hidden > 0 && index === 0
      ? `+${hidden} more · ${subagent.kind}`
      : subagent.kind;
    const text = subagent.description
      ? `${label} · ${subagent.description}`
      : label;
    return ui.hstack([
      ui.text(last ? "└─" : "├─", {
        role: "caption",
        foreground: "tertiary",
      }),
      ui.text(oneLine(text, 80), {
        role: "caption",
        foreground: "secondary",
        lineLimit: 1,
        widthFill: true,
      }),
    ], { id: `${rowID}-sub-${index}`, spacing: 4 });
  });
}

interface RowOptions {
  now: number;
  showSummary: boolean;
  showSubagents: boolean;
}

/// Native two-line row — colored state dot, name + workload badge + context
/// pressure + relative activity on the first line, kind/state and what the
/// agent is doing as the caption line, then one line per in-flight subagent.
/// No column headers, no per-row dividers: the same shape as the system-style
/// lists used elsewhere in the shelf.
function agentRow(
  agent: MuxaAgent,
  sourceKey: string,
  index: number,
  options: RowOptions,
): UINode {
  const style = STATE_STYLE[agent.state];
  const subtitle = agentSubtitle(agent, options.showSummary);
  const rowID = `${sourceKey}-agent-${index}`;
  const badge = loadBadge(agent);
  const context = contextNote(agent);
  return ui.hstack([
    ui.image("circle.fill", {
      size: 8,
      tint: style.tone === "neutral" ? "tertiary" : style.tone,
      accessibilityLabel: style.label,
    }),
    ui.vstack([
      ui.hstack([
        ui.text(agentTitle(agent), {
          role: "body",
          lineLimit: 1,
          widthFill: true,
        }),
        ...(badge
          ? [
            ui.text(badge, {
              role: "caption",
              foreground: agent.subagents.length > 0 ? "accent" : "tertiary",
              monospacedDigit: true,
              lineLimit: 1,
            }),
          ]
          : []),
        ...(context
          ? [
            ui.text(context.text, {
              role: "caption",
              foreground: context.tone,
              monospacedDigit: true,
              lineLimit: 1,
            }),
          ]
          : []),
        ui.text(age(agent.lastActivityAt, options.now), {
          role: "caption",
          foreground: "tertiary",
          monospacedDigit: true,
          lineLimit: 1,
        }),
      ], { spacing: 8 }),
      ui.text(oneLine(subtitle.text, 90), {
        role: "caption",
        foreground: subtitle.tone,
        lineLimit: 1,
      }),
      ...(options.showSubagents ? subagentRows(agent, rowID) : []),
    ], { spacing: 1, widthFill: true }),
    // Baseline, not the default center: a row grows taller when it carries a
    // subagent tree, and centering would slide its state dot down beside the
    // caption while every other row's dot sits on the name.
  ], { id: rowID, spacing: 8, alignment: "baseline" });
}

function sortedAgents(agents: MuxaAgent[]): MuxaAgent[] {
  return [...agents].sort((left, right) => {
    const rank = ATTENTION_RANK[left.state] - ATTENTION_RANK[right.state];
    if (rank !== 0) return rank;
    const activity = Date.parse(right.lastActivityAt) -
      Date.parse(left.lastActivityAt);
    return activity !== 0
      ? activity
      : agentTitle(left).localeCompare(agentTitle(right));
  });
}

// ------------------------------------------------------------------ render

function statusLabel(
  attention: number,
  working: number,
  active: number,
): string {
  if (attention > 0) return `⚠ ${attention}`;
  if (working > 0) return `● ${working}`;
  return active > 0 ? String(active) : "Idle";
}

/// What survives onto a locked screen. Everything an agent has read, typed,
/// or summarized is prompt-adjacent text, so recaps and session titles are
/// dropped along with the prompts rather than treated as safe metadata.
function redactedSnapshot(snapshot: MuxaSnapshot): MuxaSnapshot {
  return {
    agents: snapshot.agents.map((agent) => ({
      ...agent,
      sessionID: "",
      pane: null,
      cwd: null,
      summary: null,
      contextUsedPct: null,
      // Counts are safe on a locked screen; the descriptions are prompt text.
      subagents: agent.subagents.map((subagent) => ({
        ...subagent,
        description: null,
      })),
    })),
  };
}

function filteredAgents(
  ctx: WidgetLoadContext,
  snapshot: MuxaSnapshot,
): MuxaAgent[] {
  const includeStopped = booleanSetting(ctx.settings.includeStopped, false);
  return sortedAgents(snapshot.agents).filter((agent) =>
    includeStopped || agent.state !== "stopped"
  );
}

function agentTable(
  ctx: WidgetLoadContext,
  sourceKey: string,
  snapshot: MuxaSnapshot,
  redacted: boolean,
): UINode {
  const options: RowOptions = {
    now: ctx.now,
    showSummary: !redacted && booleanSetting(ctx.settings.showPrompts, true),
    showSubagents: booleanSetting(ctx.settings.showSubagents, true),
  };
  const maxAgents = integerSetting(ctx.settings.maxAgents, 5, 1, 10);
  const agents = filteredAgents(ctx, snapshot);
  const visible = agents.slice(0, maxAgents);
  const hidden = Math.max(agents.length - visible.length, 0);
  const rows = visible.map((agent, index) =>
    agentRow(agent, sourceKey, index, options)
  );

  if (rows.length === 0) {
    return ui.hstack([
      ui.text("No active agents", {
        role: "caption",
        foreground: "tertiary",
      }),
    ], { padding: 3 });
  }

  return ui.vstack([
    ...rows,
    ...(hidden > 0
      ? [
        ui.hstack([
          ui.spacer(),
          ui.text(`+${hidden} more`, {
            role: "caption",
            foreground: "tertiary",
          }),
        ]),
      ]
      : []),
  ], { spacing: 8 });
}

function prepareStatusView(
  ctx: WidgetLoadContext,
  sources: SourceResult[],
  redacted = false,
): PreparedView {
  const snapshots = sources.flatMap((source) =>
    source.snapshot ? [source.snapshot] : []
  );
  const agents = snapshots.flatMap((snapshot) => snapshot.agents);
  const active = agents.filter((agent) => agent.state !== "stopped").length;
  const working = agents.filter((agent) => agent.state === "working").length;
  const agentAttention = agents.filter((agent) =>
    agent.state === "waiting_input" || agent.state === "waiting_choice" ||
    agent.state === "error"
  ).length;
  const offline = sources.filter((source) => source.error).length;
  const attention = agentAttention + offline;
  const online = sources.filter((source) => source.snapshot).length;
  const sourceSummary = sources.length > 1 || offline > 0
    ? ` · ${online}/${sources.length} hosts online`
    : "";
  const subagents = agents.reduce(
    (total, agent) => total + agent.subagents.length,
    0,
  );
  const subagentSummary = subagents > 0
    ? ` · ${subagents} subagent${subagents === 1 ? "" : "s"}`
    : "";
  const status = {
    label: statusLabel(attention, working, active),
    tooltip:
      `${active} active · ${working} working${subagentSummary} · ${agentAttention} need you${sourceSummary}`,
  };

  if (sources.length === 0) {
    return {
      root: ui.empty({
        icon: "network.slash",
        title: "No muxa sources",
        subtitle:
          "Leave SSH host empty for this Mac, enter one SSH host, or set Source to Fleet.",
      }),
      status: { label: "Setup", tooltip: "No muxa sources configured" },
    };
  }

  if (sources.length === 1 && sources[0].error) {
    return {
      root: ui.banner(sources[0].error, {
        tone: "warning",
        title: sources[0].kind === "local" ? "Offline" : "Host offline",
      }),
      status: { label: "Offline", tooltip: sources[0].error },
    };
  }

  if (
    sources.length === 1 && sources[0].snapshot && !sources[0].error &&
    filteredAgents(ctx, sources[0].snapshot).length === 0
  ) {
    return {
      root: ui.empty({
        icon: "checkmark.circle.fill",
        title: "No active agents",
        subtitle: "Tracked agents will appear here as soon as they start.",
      }),
      status: { label: "Idle", tooltip: "No active muxa agents" },
    };
  }

  if (sources.length === 1 && sources[0].snapshot && !sources[0].error) {
    return {
      root: agentTable(ctx, sources[0].key, sources[0].snapshot, redacted),
      status,
    };
  }

  const sections: UINode[] = [];
  for (const [index, source] of sources.entries()) {
    const content = source.snapshot
      ? agentTable(ctx, source.key, source.snapshot, redacted)
      : ui.banner(source.error ?? "Unavailable", {
        tone: "warning",
        title: "Offline",
      });
    sections.push(
      ui.section(source.label, [content], { id: `source-${source.key}` }),
    );
    if (index < sources.length - 1) sections.push(ui.divider());
  }

  return {
    root: ui.vstack(sections, { spacing: 7 }),
    status,
  };
}

function redactedCacheSources(sources: SourceResult[]): SourceResult[] {
  let remoteIndex = 0;
  return sources.map((source) => {
    const label = source.host === undefined
      ? "Local"
      : `Remote ${++remoteIndex}`;
    return {
      ...source,
      label,
      host: undefined,
      snapshot: source.snapshot ? redactedSnapshot(source.snapshot) : undefined,
      error: source.error ? "Unavailable" : undefined,
    };
  });
}

const FAULT_EMPTY_STATE: Record<
  SourceFault,
  { icon: string; title: string; subtitle: string }
> = {
  missing: {
    icon: "terminal",
    title: "muxa CLI not found",
    subtitle: "Install muxa or set MUXA_BIN, then refresh this widget.",
  },
  outdatedCli: {
    icon: "terminal",
    title: "Update muxa CLI",
    subtitle: "This widget needs a newer muxa build than this host runs.",
  },
  outdatedWidget: {
    icon: "arrow.down.circle",
    title: "Update muxa Watch",
    subtitle: "This muxa build reports a status payload the widget can't read.",
  },
  offline: {
    icon: "bolt.horizontal.circle",
    title: "muxa is unavailable",
    subtitle: "Make sure muxad is running and the socket setting is correct.",
  },
};

/// First-run failure on the only local source: no cached render exists yet,
/// so show the fault itself instead of an empty agent list.
async function renderUnavailable(
  ctx: WidgetLoadContext,
  result: SourceResult,
): Promise<void> {
  const reason = result.error ?? "Unavailable";
  await ctx.log("warn", `muxa status failed: ${oneLine(reason, 240)}`);
  await ctx.render(
    ui.empty(FAULT_EMPTY_STATE[result.fault ?? "offline"]),
    {
      status: { label: "Offline", tooltip: reason },
      cacheTtlMs: 5_000,
      sensitive: false,
    },
  );
}

// -------------------------------------------------------------------- load

async function runMuxa(
  ctx: WidgetLoadContext,
  command: string,
  args: string[],
  retry: boolean,
): Promise<unknown> {
  let result = await ctx.exec.run({
    command,
    args,
    parse: "json",
    timeoutMs: command === "ssh" ? 5_000 : 6_000,
    sensitive: true,
  });
  if (retry && result.exitCode !== 0) {
    // The local daemon can briefly hit the CLI's IPC deadline during a burst
    // of hook traffic. Keep the existing one-shot retry for local snapshots;
    // retrying SSH would only double the cost of a failed connection.
    await new Promise((resolve) => setTimeout(resolve, 250));
    result = await ctx.exec.run({
      command,
      args,
      parse: "json",
      timeoutMs: 6_000,
      sensitive: true,
    });
  }
  if (result.exitCode !== 0) {
    if (
      /unexpected argument ['"]--json['"]|unknown option.*--json/i.test(
        result.stderr,
      )
    ) {
      throw new Error("muxa CLI does not support status --json");
    }
    if (/unrecognized subcommand ['"]?fleet/i.test(result.stderr)) {
      throw new Error("muxa CLI does not support fleet status");
    }
    const detail = oneLine(result.stderr, 200);
    throw new Error(detail || `${command} exited with code ${result.exitCode}`);
  }
  return result.json;
}

function describeSourceError(
  ssh: boolean,
  error: unknown,
): { text: string; fault: SourceFault } {
  const message = error instanceof Error ? error.message : String(error);
  const offline = (text: string) => ({ text, fault: "offline" as const });

  if (/does not support status --json/i.test(message)) {
    return {
      text: ssh
        ? "Update remote muxa to v0.8.18 or newer"
        : "muxa CLI does not support status --json",
      fault: "outdatedCli",
    };
  }
  if (/does not support fleet status/i.test(message)) {
    return {
      text: "Update muxa to v0.8.34 or newer to use Fleet",
      fault: "outdatedCli",
    };
  }
  if (/cannot read|unexpected muxa (status|fleet) payload/i.test(message)) {
    return {
      text: "muxa returned a status payload this widget can't read",
      fault: "outdatedWidget",
    };
  }
  if (/timed out|timeout/i.test(message)) {
    return offline("Connection timed out");
  }
  if (/Could not resolve hostname|nodename nor servname/i.test(message)) {
    return offline("SSH host not found");
  }
  if (/Host key verification failed/i.test(message)) {
    return offline("SSH host key is not trusted yet");
  }
  if (/Permission denied/i.test(message)) {
    return offline("SSH authentication failed");
  }
  if (/Connection refused/i.test(message)) {
    return offline("SSH connection refused");
  }
  if (/output too large|outputTooLarge/i.test(message)) {
    return offline("muxa returned more data than this widget may read");
  }
  if (/command not found|not found|ExecNotFound/i.test(message)) {
    return {
      text: ssh
        ? "muxa was not found in the remote SSH PATH"
        : "muxa CLI not found",
      fault: "missing",
    };
  }
  return offline(oneLine(message, 160) || "Unavailable");
}

async function loadSingleHost(
  ctx: WidgetLoadContext,
  source: SourceResult,
  socket: string,
): Promise<SourceResult> {
  try {
    if (source.kind === "local") {
      const args = socket
        ? ["--socket", socket, "status", "--json"]
        : ["status", "--json"];
      return {
        ...source,
        snapshot: parseStatus(await runMuxa(ctx, "muxa", args, true)),
      };
    }

    const args = [...SSH_OPTIONS, source.host ?? "", "muxa", "status", "--json"];
    return {
      ...source,
      snapshot: parseStatus(await runMuxa(ctx, "ssh", args, false)),
    };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await ctx.log(
      "warn",
      `${source.label} muxa status failed: ${oneLine(message, 240)}`,
    );
    const described = describeSourceError(source.kind === "ssh", error);
    return { ...source, error: described.text, fault: described.fault };
  }
}

async function loadFleet(
  ctx: WidgetLoadContext,
  socket: string,
): Promise<SourceResult[]> {
  const args = socket
    ? ["--socket", socket, "fleet", "status", "--json"]
    : ["fleet", "status", "--json"];
  try {
    return parseFleet(await runMuxa(ctx, "muxa", args, true));
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await ctx.log("warn", `muxa fleet status failed: ${oneLine(message, 240)}`);
    const described = describeSourceError(false, error);
    return [{
      key: "fleet",
      label: "Fleet",
      kind: "fleet",
      error: described.text,
      fault: described.fault,
    }];
  }
}

async function load(ctx: WidgetLoadContext): Promise<void> {
  const socket = typeof ctx.settings.socket === "string"
    ? ctx.settings.socket.trim()
    : "";
  const fleet = fleetSelected(ctx.settings.source);
  const ssh: SSHHostSetting = fleet ? {} : parseSSHHost(ctx.settings.sshHost);

  const sourceSignature = JSON.stringify({
    fleet,
    host: ssh.host ?? null,
    socket,
    error: ssh.error ?? null,
  });

  let results: SourceResult[];
  if (fleet) {
    results = await loadFleet(ctx, socket);
  } else if (ssh.error) {
    results = [{
      key: "ssh",
      label: "SSH",
      kind: "ssh",
      error: ssh.error,
      fault: "offline",
    }];
  } else {
    const source: SourceResult = ssh.host
      ? { key: "ssh", label: ssh.host, kind: "ssh", host: ssh.host }
      : { key: "local", label: "Local", kind: "local" };
    results = [await loadSingleHost(ctx, source, socket)];
  }

  const successful = results.filter((source) => source.snapshot).length;
  // Holding the last good render smooths over a dropped SSH connection, but a
  // CLI/schema fault is not transient: hiding it behind a stale card is how a
  // schema bump silently blanks this widget. Those always render their reason.
  const actionable = results.some((source) =>
    source.fault === "missing" || source.fault === "outdatedCli" ||
    source.fault === "outdatedWidget"
  );
  if (
    successful === 0 && !actionable && runtimeState.hasRendered &&
    runtimeState.sourceSignature === sourceSignature
  ) {
    await ctx.log("warn", "muxa refresh kept the last good source render");
    return;
  }
  if (
    results.length === 1 && results[0].kind !== "ssh" && results[0].error &&
    (actionable || !runtimeState.hasRendered)
  ) {
    await renderUnavailable(ctx, results[0]);
    return;
  }

  const live = prepareStatusView(ctx, results);
  const fallback = prepareStatusView(ctx, redactedCacheSources(results), true);
  await ctx.render(
    live.root,
    {
      status: live.status,
      cacheRoot: fallback.root,
      cacheTtlMs: 5_000,
      sensitive: true,
    },
  );
  runtimeState.hasRendered = successful > 0;
  runtimeState.sourceSignature = sourceSignature;
}

export default barshelf.widget({ load });
