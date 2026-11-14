// Pure view model for the "Needs you" page: turns the daemon's
// `decision_queue` / `decision_history` entries into cards. No DOM here, so
// node's test runner covers it; the page renders cards with textContent
// only, never as HTML.

const AGENT_STATES = {
  error: "error",
  waiting_choice: "waiting for a choice",
  waiting_input: "waiting for input",
  idle: "asked you something",
};

export function ago(iso, now = Date.now()) {
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return "";
  const seconds = Math.max(0, Math.round((now - then) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 48) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

function firstLine(text) {
  return String(text || "").split("\n").find((line) => line.trim())?.trim() || "";
}

/// Only http(s) links are clickable; anything else stays text.
export function safeHref(url) {
  try {
    const parsed = new URL(String(url));
    return parsed.protocol === "https:" || parsed.protocol === "http:" ? parsed.href : null;
  } catch {
    return null;
  }
}

export function evidenceLine(evidence) {
  switch (evidence?.kind) {
    case "commit":
      return { text: `commit ${evidence.sha}${evidence.repo ? ` in ${evidence.repo}` : ""}` };
    case "pull_request":
      return { text: `pull request ${evidence.url}`, href: safeHref(evidence.url) };
    case "diff":
      return { text: `diff ${evidence.range}${evidence.repo ? ` in ${evidence.repo}` : ""}` };
    case "test": {
      const outcome = evidence.passed === true ? "passed" : evidence.passed === false ? "failed" : "ran";
      return {
        text: `test ${outcome}: ${evidence.command}`,
        cls: evidence.passed === true ? "pass" : evidence.passed === false ? "fail" : "",
        tail: evidence.tail || null,
      };
    }
    case "path":
      return { text: `path ${evidence.path}` };
    default:
      return { text: "evidence of an unknown kind" };
  }
}

function decisionParts(decision) {
  return {
    needed: decision?.needed || null,
    options: Array.isArray(decision?.options) ? decision.options : [],
    recommendation: decision?.recommendation || null,
    evidence: Array.isArray(decision?.evidence) ? decision.evidence.map(evidenceLine) : [],
  };
}

function target(review) {
  const t = review?.target || {};
  if (t.kind === "work") return `${t.workspace}/${t.work}${t.alias ? ` @${t.alias}` : ""}`;
  if (t.kind === "session") return `session ${t.provider ? `${t.provider} ` : ""}${String(t.session_id || "").slice(0, 8)}`;
  if (t.kind === "request") return `request ${t.request_id}`;
  return "";
}

function asker(entry) {
  const who = entry.from_alias ? `@${entry.from_alias}` : entry.from_pane || entry.from_session_id || "an agent";
  return entry.workspace_id && entry.work_id ? `${who} · ${entry.workspace_id}/${entry.work_id}` : who;
}

/// One card for a pending entry.
export function pendingCard(item, now = Date.now()) {
  switch (item?.kind) {
    case "human_request":
      return {
        id: item.request_id,
        kind: "ask",
        label: item.action ? `asks you · ${item.action}` : "asks you",
        title: item.body,
        meta: `${asker(item)} · ${ago(item.created_at, now)}`,
        ...decisionParts(item.decision),
      };
    case "agent_waiting":
      return {
        id: item.session_id,
        kind: "agent",
        label: AGENT_STATES[item.state] || item.state,
        title: item.message || "waiting in its pane",
        meta: `${item.agent_kind} ${item.pane || item.session_id} · ${ago(item.since, now)}`,
        pane: item.pane || null,
        socket: item.tmux_socket || null,
        ...decisionParts(null),
      };
    case "review": {
      const parts = decisionParts(item.decision);
      return {
        id: item.id,
        kind: "review",
        label: "to review",
        title: item.summary,
        meta: `${target(item)} · ${ago(item.requested_at, now)}`,
        ...parts,
      };
    }
    default:
      return null;
  }
}

/// One card for a past decision.
export function decidedCard(item, now = Date.now()) {
  if (item?.kind === "review") {
    const d = item.disposition || {};
    return {
      id: item.id,
      kind: "review",
      label: d.kind || "decided",
      title: item.summary,
      meta: `${target(item)} · ${ago(d.at, now)}${d.by?.label ? ` · ${d.by.label}` : ""}`,
      outcome: d.note ? `${d.kind}: ${firstLine(d.note)}` : d.kind || "",
      ...decisionParts(item.decision),
    };
  }
  if (item?.kind === "human_answer") {
    return {
      id: item.request_id,
      kind: "ask",
      label: item.status === "completed" ? "answered" : item.status,
      title: item.body,
      meta: `${asker(item)} · ${ago(item.answered_at, now)}`,
      outcome: firstLine(item.answer),
      ...decisionParts(item.decision),
    };
  }
  return null;
}

/// The per-host queue as sections of cards. Each card knows its host;
/// cards from another host carry `remote` (no local pane link) and can be
/// acted on only where the host allows it (`control`; for review items also
/// `decides_reviews`). An older response
/// with a single top-level `queue` reads as one local section.
export function hostSections(data, now = Date.now()) {
  const hosts = Array.isArray(data?.hosts)
    ? data.hosts
    : [{ alias: data?.covers || "this host", local: true, control: true, covers: [], unavailable: data?.queue?.unavailable || [], queue: data?.queue }];
  return hosts.map((host) => {
    const remote = !host.local;
    const cards = (host.queue?.items || [])
      .map((item) => pendingCard(item, now))
      .filter(Boolean)
      .map((card) => ({
        ...card,
        host: host.alias,
        remote,
        // A remote review item only where that host applies decisions
        // exactly over Fleet (`decides_reviews`).
        actionable:
          !remote ||
          (host.control && card.kind === "ask") ||
          (host.control && host.decides_reviews === true && card.kind === "review"),
        pane: remote ? null : card.pane,
      }));
    const notes = [];
    if (host.queue?.truncated) notes.push("more than shown");
    for (const source of host.unavailable || []) notes.push(`unavailable: ${source}`);
    if (remote && !host.control) notes.push("observe-only: answer on that host");
    else if (remote && host.decides_reviews !== true && cards.some((card) => card.kind === "review")) {
      notes.push("results: decide on that host (its tend cannot take decisions over Fleet)");
    }
    return {
      alias: host.alias,
      local: !remote,
      covers: Array.isArray(host.covers) ? host.covers : [],
      notes,
      cards,
    };
  });
}

/// Request ids are per host (two hosts can hold the same one), so an unsent
/// answer belongs to the host, the kind of item and its id together.
export function draftKey(card) {
  return JSON.stringify([card.host || "", card.remote ? 1 : 0, card.kind, card.id]);
}

/// The card a notification links to (`#item=<id>`), if any.
export function linkedItemOf(hash) {
  return new URLSearchParams(String(hash || "").replace(/^#/, "")).get("item");
}

/// Where sign-in should come back to: this page, and the linked card.
export function needsReturnPath(hash) {
  const item = linkedItemOf(hash);
  return item ? `/needs#item=${encodeURIComponent(item)}` : "/needs";
}
