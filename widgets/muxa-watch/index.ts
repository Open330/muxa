import { barshelf, ui, type UINode, type WidgetLoadContext } from "barshelf";

type AgentState =
  | "starting"
  | "working"
  | "idle"
  | "waiting_input"
  | "waiting_choice"
  | "error"
  | "stopped";

interface MuxaWorkload {
  subagents: number;
  shells: number;
  processes: number;
}

interface MuxaSubagent {
  kind: string;
  description: string | null;
}

interface MuxaAgent {
  kind: string;
  session_id: string;
  state: AgentState;
  pane: string | null;
  location: string;
  cwd: string | null;
  model: string | null;
  last_prompt: string | null;
  last_notification: string | null;
  context_used_pct: number | null;
  cost_usd: number | null;
  /// Child-process rollup (schema 2+). Absent on quiet agents and on
  /// schema 1 payloads.
  workload: MuxaWorkload | null;
  /// Named Task subagents in flight (schema 2+), newest last.
  subagents: MuxaSubagent[];
  started_at: string;
  last_activity_at: string;
  state_entered_at: string;
}

interface MuxaStatus {
  schema_version: number;
  generated_at: string;
  agents: MuxaAgent[];
}

interface StateStyle {
  label: string;
  tone: string;
  marker: string;
}

interface PreparedView {
  root: UINode;
  status: { label: string; tooltip: string };
}

interface WatchSource {
  key: string;
  label: string;
  kind: "local" | "ssh";
  host?: string;
}

/// Why a source has no snapshot. Classified once where the failure happens
/// so the card and the empty-state never re-parse each other's prose.
type SourceFault = "missing" | "outdatedCli" | "outdatedWidget" | "offline";

interface SourceResult extends WatchSource {
  snapshot?: MuxaStatus;
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
  error: {
    label: "Error",
    tone: "danger",
    marker: "■",
  },
  waiting_choice: {
    label: "Choose",
    tone: "warning",
    marker: "◆",
  },
  waiting_input: {
    label: "Needs input",
    tone: "warning",
    marker: "▶",
  },
  working: { label: "Working", tone: "good", marker: "●" },
  starting: { label: "Starting", tone: "accent", marker: "◌" },
  idle: { label: "Idle", tone: "neutral", marker: "○" },
  stopped: { label: "Stopped", tone: "neutral", marker: "×" },
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isAgentState(value: unknown): value is AgentState {
  return typeof value === "string" && value in STATE_STYLE;
}

function optionalText(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function count(value: unknown): number {
  const number = optionalNumber(value);
  return number === null ? 0 : Math.max(0, Math.round(number));
}

function parseWorkload(value: unknown): MuxaWorkload | null {
  if (!isRecord(value)) return null;
  const workload = {
    subagents: count(value.subagents),
    shells: count(value.shells),
    processes: count(value.processes),
  };
  return workload.subagents + workload.shells + workload.processes > 0
    ? workload
    : null;
}

function parseSubagents(value: unknown): MuxaSubagent[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item) =>
    isRecord(item) && typeof item.kind === "string"
      ? [{ kind: item.kind, description: optionalText(item.description) }]
      : []
  );
}

function parseAgent(value: unknown): MuxaAgent | null {
  if (
    !isRecord(value) ||
    typeof value.kind !== "string" ||
    typeof value.session_id !== "string" ||
    !isAgentState(value.state) ||
    typeof value.location !== "string" ||
    typeof value.last_activity_at !== "string" ||
    typeof value.state_entered_at !== "string"
  ) {
    return null;
  }

  return {
    kind: value.kind,
    session_id: value.session_id,
    state: value.state,
    pane: optionalText(value.pane),
    location: value.location,
    cwd: optionalText(value.cwd),
    model: optionalText(value.model),
    last_prompt: optionalText(value.last_prompt),
    last_notification: optionalText(value.last_notification),
    context_used_pct: optionalNumber(value.context_used_pct),
    cost_usd: optionalNumber(value.cost_usd),
    workload: parseWorkload(value.workload),
    subagents: parseSubagents(value.subagents),
    started_at: typeof value.started_at === "string"
      ? value.started_at
      : value.state_entered_at,
    last_activity_at: value.last_activity_at,
    state_entered_at: value.state_entered_at,
  };
}

/// `muxa status --json` is additive by contract: new releases add fields and
/// bump `schema_version`, they do not repurpose the ones this widget reads.
/// So parse forward instead of pinning a version — pinning is exactly what
/// blanked this widget when muxa 0.8.19 shipped schema 2 (workload +
/// subagents). The only genuinely unsupported case is a future schema that
/// drops the fields below, which surfaces as "nothing parsed".
const KNOWN_SCHEMA_VERSION = 2;

function parseStatus(value: unknown): MuxaStatus {
  if (!isRecord(value) || !Array.isArray(value.agents)) {
    throw new Error("unexpected muxa status payload");
  }
  const version = optionalNumber(value.schema_version);
  if (version === null || version < 1) {
    throw new Error("unexpected muxa status payload");
  }

  const agents = value.agents.flatMap((item) => {
    const agent = parseAgent(item);
    return agent ? [agent] : [];
  });
  if (
    version > KNOWN_SCHEMA_VERSION && agents.length === 0 &&
    value.agents.length > 0
  ) {
    throw new Error(`muxa status schema ${version} is newer than this widget`);
  }

  return {
    schema_version: version,
    generated_at: typeof value.generated_at === "string"
      ? value.generated_at
      : new Date().toISOString(),
    agents,
  };
}

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

function kindLabel(kind: string): string {
  switch (kind) {
    case "claude_code":
      return "Claude Code";
    case "codex":
      return "Codex";
    case "gemini_cli":
      return "Gemini CLI";
    case "opencode":
      return "opencode";
    case "task":
      return "Task";
    default:
      return kind.replaceAll("_", " ");
  }
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

function oneLine(value: string, maxLength = 160): string {
  const compact = value.replace(/\s+/g, " ").trim();
  return compact.length > maxLength
    ? `${compact.slice(0, maxLength - 1)}…`
    : compact;
}

function agentTitle(agent: MuxaAgent): string {
  if (agent.location && agent.location !== "-") return agent.location;
  return basename(agent.cwd) ?? kindLabel(agent.kind);
}

function promptFor(agent: MuxaAgent): string | null {
  const prompt = agent.last_prompt ?? agent.last_notification;
  return prompt ? oneLine(prompt) : null;
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

/// Second line: what the agent is / why it wants you, then the last prompt.
/// Attention states lead with their label in the state's tone so waiting
/// rows read at a glance; calm rows lead with the agent kind.
function agentSubtitle(
  agent: MuxaAgent,
  showPrompts: boolean,
): { text: string; tone: string } {
  const style = STATE_STYLE[agent.state];
  const attention = needsAttention(agent.state);
  const lead = attention ? style.label : kindLabel(agent.kind);
  const prompt = showPrompts ? promptFor(agent) : null;
  return {
    text: prompt ? `${lead} · ${prompt}` : lead,
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
  const other = Math.max(
    workload.processes - workload.subagents - workload.shells,
    0,
  );
  if (other > 0) parts.push(`+${other}`);
  return parts.join(" ");
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
  showPrompts: boolean;
  showSubagents: boolean;
}

/// Native two-line row — colored state dot, name + workload badge + relative
/// activity on the first line, kind/state + last prompt as the caption line,
/// then one line per in-flight subagent. No column headers, no per-row
/// dividers: the same shape as the system-style lists used elsewhere in the
/// shelf.
function agentRow(
  agent: MuxaAgent,
  sourceKey: string,
  index: number,
  options: RowOptions,
): UINode {
  const style = STATE_STYLE[agent.state];
  const subtitle = agentSubtitle(agent, options.showPrompts);
  const rowID = `${sourceKey}-agent-${index}`;
  const badge = loadBadge(agent);
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
        ui.text(age(agent.last_activity_at, options.now), {
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
  ], { id: rowID, spacing: 8 });
}

function sortedAgents(agents: MuxaAgent[]): MuxaAgent[] {
  return [...agents].sort((left, right) => {
    const rank = ATTENTION_RANK[left.state] - ATTENTION_RANK[right.state];
    if (rank !== 0) return rank;
    const activity = Date.parse(right.last_activity_at) -
      Date.parse(left.last_activity_at);
    return activity !== 0
      ? activity
      : agentTitle(left).localeCompare(agentTitle(right));
  });
}

function statusLabel(
  attention: number,
  working: number,
  active: number,
): string {
  if (attention > 0) return `⚠ ${attention}`;
  if (working > 0) return `● ${working}`;
  return active > 0 ? String(active) : "Idle";
}

function redactedCacheStatus(snapshot: MuxaStatus): MuxaStatus {
  return {
    ...snapshot,
    agents: snapshot.agents.map((agent) => ({
      ...agent,
      session_id: "",
      pane: null,
      cwd: null,
      model: null,
      last_prompt: null,
      last_notification: null,
      context_used_pct: null,
      cost_usd: null,
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
  snapshot: MuxaStatus,
): MuxaAgent[] {
  const includeStopped = booleanSetting(ctx.settings.includeStopped, false);
  return sortedAgents(snapshot.agents).filter((agent) =>
    includeStopped || agent.state !== "stopped"
  );
}

function agentTable(
  ctx: WidgetLoadContext,
  sourceKey: string,
  snapshot: MuxaStatus,
  redacted: boolean,
): UINode {
  const options: RowOptions = {
    now: ctx.now,
    showPrompts: !redacted && booleanSetting(ctx.settings.showPrompts, true),
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
  const agentAttention =
    agents.filter((agent) =>
      agent.state === "waiting_input" || agent.state === "waiting_choice" ||
      agent.state === "error"
    ).length;
  const offline = sources.filter((source) => source.error).length;
  const attention = agentAttention + offline;
  const online = sources.filter((source) => source.snapshot).length;
  const sourceSummary = sources.length > 1 || offline > 0
    ? ` · ${online}/${sources.length} sources online`
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
          "Leave SSH host empty for this Mac, or enter one SSH host.",
      }),
      status: { label: "Setup", tooltip: "No muxa sources configured" },
    };
  }

  if (sources.length === 1 && sources[0].error) {
    return {
      root: ui.banner(sources[0].error, {
        tone: "warning",
        title: sources[0].kind === "ssh" ? "SSH source offline" : "Offline",
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
    const label = source.kind === "local" ? "Local" : `Remote ${++remoteIndex}`;
    return {
      ...source,
      label,
      host: undefined,
      snapshot: source.snapshot
        ? redactedCacheStatus(source.snapshot)
        : undefined,
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
    subtitle: "This widget needs a muxa build that supports status --json.",
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

/// First-run failure on the only (local) source: no cached render exists yet,
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

async function fetchStatus(
  ctx: WidgetLoadContext,
  command: string,
  args: string[],
  retry: boolean,
): Promise<MuxaStatus> {
  let result = await ctx.exec.run({
    command,
    args,
    parse: "json",
    timeoutMs: command === "ssh" ? 5_000 : 4_000,
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
      timeoutMs: 4_000,
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
    const detail = oneLine(result.stderr, 200);
    throw new Error(detail || `${command} exited with code ${result.exitCode}`);
  }
  return parseStatus(result.json);
}

function describeSourceError(
  source: WatchSource,
  error: unknown,
): { text: string; fault: SourceFault } {
  const message = error instanceof Error ? error.message : String(error);
  const ssh = source.kind === "ssh";
  const offline = (text: string) => ({ text, fault: "offline" as const });

  if (/does not support status --json/i.test(message)) {
    return {
      text: ssh
        ? "Update remote muxa to v0.8.18 or newer"
        : "muxa CLI does not support status --json",
      fault: "outdatedCli",
    };
  }
  if (/newer than this widget/i.test(message)) {
    return { text: "Update the muxa Watch widget", fault: "outdatedWidget" };
  }
  if (/unexpected muxa status payload/i.test(message)) {
    return {
      text: "muxa returned an unexpected status payload",
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

async function loadSource(
  ctx: WidgetLoadContext,
  source: WatchSource,
  socket: string,
): Promise<SourceResult> {
  try {
    if (source.kind === "local") {
      const args = socket
        ? ["--socket", socket, "status", "--json"]
        : ["status", "--json"];
      return {
        ...source,
        snapshot: await fetchStatus(ctx, "muxa", args, true),
      };
    }

    const args = [
      ...SSH_OPTIONS,
      source.host ?? "",
      "muxa",
      "status",
      "--json",
    ];
    return {
      ...source,
      snapshot: await fetchStatus(ctx, "ssh", args, false),
    };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await ctx.log(
      "warn",
      `${source.label} muxa status failed: ${oneLine(message, 240)}`,
    );
    const described = describeSourceError(source, error);
    return { ...source, error: described.text, fault: described.fault };
  }
}

async function load(ctx: WidgetLoadContext): Promise<void> {
  const socket = typeof ctx.settings.socket === "string"
    ? ctx.settings.socket.trim()
    : "";
  const ssh = parseSSHHost(ctx.settings.sshHost);
  const source: WatchSource = ssh.host
    ? { key: "ssh", label: ssh.host, kind: "ssh", host: ssh.host }
    : { key: "local", label: "Local", kind: "local" };

  const sourceSignature = JSON.stringify({
    host: ssh.host ?? null,
    socket: ssh.host ? null : socket,
    error: ssh.error ?? null,
  });
  const results: SourceResult[] = ssh.error
    ? [{ ...source, error: ssh.error, fault: "offline" }]
    : [await loadSource(ctx, source, socket)];

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
    results.length === 1 && results[0].kind === "local" && results[0].error &&
    (actionable || !runtimeState.hasRendered)
  ) {
    await renderUnavailable(ctx, results[0]);
    return;
  }

  const live = prepareStatusView(ctx, results);
  const fallback = prepareStatusView(
    ctx,
    redactedCacheSources(results),
    true,
  );
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
