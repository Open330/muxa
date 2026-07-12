import { barshelf, ui, type UINode, type WidgetLoadContext } from "barshelf";

type AgentState =
  | "starting"
  | "working"
  | "idle"
  | "waiting_input"
  | "waiting_choice"
  | "error"
  | "stopped";

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

interface SourceResult extends WatchSource {
  snapshot?: MuxaStatus;
  error?: string;
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

function parseStatus(value: unknown): MuxaStatus {
  if (
    !isRecord(value) || value.schema_version !== 1 ||
    !Array.isArray(value.agents)
  ) {
    throw new Error("unsupported muxa status payload");
  }

  const agents = value.agents.filter((item): item is MuxaAgent =>
    isRecord(item) &&
    typeof item.kind === "string" &&
    typeof item.session_id === "string" &&
    isAgentState(item.state) &&
    typeof item.location === "string" &&
    typeof item.last_activity_at === "string" &&
    typeof item.state_entered_at === "string"
  );

  return {
    schema_version: 1,
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

/// Native two-line row — colored state dot, name + relative activity on the
/// first line, kind/state + last prompt as the caption line. No column
/// headers, no per-row dividers: the same shape as the system-style lists
/// used elsewhere in the shelf.
function agentRow(
  agent: MuxaAgent,
  sourceKey: string,
  index: number,
  now: number,
  showPrompts: boolean,
): UINode {
  const style = STATE_STYLE[agent.state];
  const subtitle = agentSubtitle(agent, showPrompts);
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
        ui.text(age(agent.last_activity_at, now), {
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
    ], { spacing: 1, widthFill: true }),
  ], { id: `${sourceKey}-agent-${index}`, spacing: 8 });
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
  const showPrompts = !redacted &&
    booleanSetting(ctx.settings.showPrompts, true);
  const maxAgents = integerSetting(ctx.settings.maxAgents, 5, 1, 10);
  const agents = filteredAgents(ctx, snapshot);
  const visible = agents.slice(0, maxAgents);
  const hidden = Math.max(agents.length - visible.length, 0);
  const rows = visible.map((agent, index) =>
    agentRow(agent, sourceKey, index, ctx.now, showPrompts)
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
  const status = {
    label: statusLabel(attention, working, active),
    tooltip:
      `${active} active · ${working} working · ${agentAttention} need you${sourceSummary}`,
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

async function renderUnavailable(
  ctx: WidgetLoadContext,
  error: unknown,
): Promise<void> {
  const message = error instanceof Error ? error.message : String(error);
  await ctx.log("warn", `muxa status failed: ${oneLine(message, 240)}`);
  const missing = /not found|ExecNotFound/i.test(message);
  const outdated = /does not support status --json/i.test(message);

  await ctx.render(
    ui.empty({
      icon: missing || outdated ? "terminal" : "bolt.horizontal.circle",
      title: missing
        ? "muxa CLI not found"
        : outdated
        ? "Update muxa CLI"
        : "muxa is unavailable",
      subtitle: missing
        ? "Install muxa or set MUXA_BIN, then refresh this widget."
        : outdated
        ? "This widget needs a muxa build that supports status --json."
        : "Make sure muxad is running and the socket setting is correct.",
    }),
    {
      status: { label: "Offline", tooltip: "muxa status is unavailable" },
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

function friendlySourceError(source: WatchSource, error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  if (/does not support status --json/i.test(message)) {
    return source.kind === "ssh"
      ? "Update remote muxa to v0.8.18 or newer"
      : "muxa CLI does not support status --json";
  }
  if (/timed out|timeout/i.test(message)) return "Connection timed out";
  if (/Could not resolve hostname|nodename nor servname/i.test(message)) {
    return "SSH host not found";
  }
  if (/Host key verification failed/i.test(message)) {
    return "SSH host key is not trusted yet";
  }
  if (/Permission denied/i.test(message)) return "SSH authentication failed";
  if (/Connection refused/i.test(message)) return "SSH connection refused";
  if (/command not found|not found|ExecNotFound/i.test(message)) {
    return source.kind === "ssh"
      ? "muxa was not found in the remote SSH PATH"
      : "muxa CLI not found";
  }
  return oneLine(message, 160) || "Unavailable";
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
    return { ...source, error: friendlySourceError(source, error) };
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
    ? [{ ...source, error: ssh.error }]
    : [await loadSource(ctx, source, socket)];

  const successful = results.filter((source) => source.snapshot).length;
  if (
    successful === 0 && runtimeState.hasRendered &&
    runtimeState.sourceSignature === sourceSignature
  ) {
    await ctx.log("warn", "muxa refresh kept the last good source render");
    return;
  }
  if (
    results.length === 1 && results[0].kind === "local" && results[0].error &&
    !runtimeState.hasRendered
  ) {
    await renderUnavailable(ctx, new Error(results[0].error));
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
