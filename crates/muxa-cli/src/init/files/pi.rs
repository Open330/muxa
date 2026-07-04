//! `~/.pi/agent/extensions/muxa/index.ts` content layer.
//!
//! Unlike the shell-hook agents (Claude/Codex/Gemini), Pi exposes an
//! in-process TypeScript extension API. This layer writes a small
//! extension that subscribes to Pi's lifecycle events and forwards a
//! normalized JSON payload to `muxa hook pi --event event` via the
//! muxa-managed `MUXA_SOCKET`.
//!
//! The extension is best-effort: a down/unreachable `muxad` must never
//! block Pi's critical path, so every forwarding call is fire-and-forget
//! with swallowed errors.

use crate::init::marker::{self, Outcome};

const ID: &str = "pi-extension";

const BODY: &str = r#"// Forward Pi lifecycle events to muxa without blocking Pi's agent loop.
//
// muxa is an observability layer; if its daemon is down every call here
// must fail silently so Pi keeps running normally. Native Pi event
// names are forwarded verbatim (the Rust `PiAdapter` discriminates on
// `type`), mirroring how the opencode plugin forwards opencode events.
import { spawn } from "node:child_process";
import { type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const SOCKET = process.env.MUXA_SOCKET ?? "";

// Resolve a stable session id. `getSessionId()` returns the canonical
// UUID for persisted and in-memory sessions; falling back to the pid
// keeps a usable identity even on hosts where the API isn't available.
function sessionId(ctx: { sessionManager?: { getSessionId?: () => string } }): string {
  try {
    return ctx.sessionManager?.getSessionId?.() ?? `pi-${process.pid}`;
  } catch {
    return `pi-${process.pid}`;
  }
}

// `__MUXA_BIN__` is substituted at install time by `muxa init` with the
// absolute path to the `muxa` binary. This matters because the extension
// runs inside Pi's process, whose PATH may not include ~/.cargo/bin; a
// bare `spawn("muxa")` would then ENOENT and we'd silently lose every
// event (the error is swallowed below by design).
const MUXA_BIN = "__MUXA_BIN__";

// Cumulative cost for the current session branch, in USD. Reset on
// `session_start` and summed across `turn_end` events. See the turn_end
// handler for why this must be cumulative rather than per-message.
let sessionCostUsd = 0;

// Pull the readable text out of a Pi message. `content` is either a
// plain string (older path) or — the case that matters here — an array
// of typed blocks. We concatenate every `text` block; non-text blocks
// (thinking, tool calls, images) carry no user-facing reply text.
function extractText(msg: { content?: unknown } | undefined): string | undefined {
  const content = msg?.content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    const texts: string[] = [];
    for (const block of content) {
      if (
        block !== null &&
        typeof block === "object" &&
        (block as { type?: string }).type === "text" &&
        typeof (block as { text?: unknown }).text === "string"
      ) {
        texts.push((block as { text: string }).text);
      }
    }
    return texts.length > 0 ? texts.join("\n") : undefined;
  }
  return undefined;
}

// Forward one event object to `muxa hook pi`, serialized through a
// single in-process delivery chain so events reach muxad in the order
// Pi emitted them. Each callback returns immediately (it only *appends*
// to the chain, never awaits it), so Pi's agent loop is never blocked;
// the actual spawn + wait happens asynchronously, one delivery at a
// time. Without this, rapidly successive lifecycle events (e.g.
// `agent_end` then `session_shutdown`) would each spawn an independent
// `muxa` process racing to the same socket, and a late `agent_end`
// could resurrect an already-stopped row or a late `tool_execution_start`
// could mark a finished turn as Working again.
let deliveryChain: Promise<void> = Promise.resolve();

// Bounded safety timeout for a single delivery. cmux/muxad spawns
// return near-instantly; this only guards against a wedged spawn
// freezing the whole chain. `unref()`d so the timer can never keep
// Pi's process alive on its own.
const DELIVERY_TIMEOUT_MS = 5_000;

function forward(type: string, payload: Record<string, unknown>, ctx: ExtensionContextLike) {
  deliveryChain = deliveryChain
    .then(() => deliverOne(type, payload, ctx))
    // A rejected step must not break subsequent deliveries — an error
    // in one event should never swallow the next.
    .catch(() => {});
}

function deliverOne(
  type: string,
  payload: Record<string, unknown>,
  ctx: ExtensionContextLike,
): Promise<void> {
  return new Promise((resolve) => {
    let settled = false;
    const done = () => {
      if (!settled) {
        settled = true;
        resolve();
      }
    };
    try {
      const body = JSON.stringify({
        type,
        session_id: sessionId(ctx),
        cwd: ctx.cwd,
        pane: process.env.TMUX_PANE ?? process.env.ZELLIJ_PANE_ID,
        pid: process.pid,
        ...payload,
      });
      const child = spawn(MUXA_BIN, ["hook", "pi", "--event", "event"], {
        stdio: ["pipe", "ignore", "ignore"],
        detached: true,
        env: SOCKET ? { ...process.env, MUXA_SOCKET: SOCKET } : process.env,
      });
      // Resolve on any terminal event so the next delivery can start.
      // Multiple events firing is fine — `settled` makes them idempotent.
      child.on("error", done);
      child.on("exit", done);
      child.on("close", done);
      const timer = setTimeout(done, DELIVERY_TIMEOUT_MS);
      timer.unref?.();
      child.stdin?.end(body);
      child.unref();
    } catch {
      // swallow — observability must never break the agent
      done();
    }
  });
}

type ExtensionContextLike = {
  cwd?: string;
  model?: { id?: string };
  sessionManager?: { getSessionId?: () => string };
};

export default function (pi: ExtensionAPI) {
  // session lifecycle
  pi.on("session_start", async (_event, ctx) => {
    // Reset the per-session cost accumulator: a session_start marks a
    // fresh session branch, so costs from a previous turn sequence
    // must not carry over.
    sessionCostUsd = 0;
    forward("session_start", {}, ctx);
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    forward("session_shutdown", {}, ctx);
  });

  // prompt submission — carries the user's prompt text
  pi.on("before_agent_start", async (event, ctx) => {
    forward("before_agent_start", { prompt: event.prompt }, ctx);
  });

  // tool execution lifecycle — the *observation* layer, not the
  // preflight `tool_call` layer. `tool_execution_end` carries `isError`
  // and the final `result`, which is what we want for success/fail.
  pi.on("tool_execution_start", async (event, ctx) => {
    forward("tool_execution_start", { tool: event.toolName }, ctx);
  });

  pi.on("tool_execution_end", async (event, ctx) => {
    forward("tool_execution_end", {
      tool: event.toolName,
      success: !event.isError,
    }, ctx);
  });

  // turn boundary — refresh model + cost info once per turn rather than
  // per streamed message, keeping daemon churn low.
  // Cost is reported as a session-branch cumulative total, not the
  // per-message `usage.cost.total`: the daemon's `apply_heartbeat`
  // overwrites `Agent.cost_usd` and the UI renders it as the running
  // session cost, so sending a per-message value would make the number
  // jump down on a cheaper turn. We accumulate across turns and reset
  // on `session_start` (handled below).
  pi.on("turn_end", async (event, ctx) => {
    const usage = (event.message?.usage as { cost?: { total?: number } } | undefined);
    const turnCost = usage?.cost?.total;
    if (typeof turnCost === "number") sessionCostUsd += turnCost;
    forward("turn_end", {
      model: ctx.model?.id,
      cost_usd: sessionCostUsd,
    }, ctx);
  });

  // agent loop end — final assistant message text, when extractable.
  // Pi's `AssistantMessage.content` is an array of typed blocks
  // (text / thinking / tool-call …), not a bare string, so we walk the
  // last message and concatenate its text blocks. Without this the
  // adapter never sees a `response`, `last_response` stays empty, and a
  // successful `TurnStopped` can't clear a prior Error state.
  pi.on("agent_end", async (event, ctx) => {
    const last = event.messages?.at(-1);
    forward("agent_end", { response: extractText(last) }, ctx);
  });
}
"#;

/// Default install path: `~/.pi/agent/extensions/muxa/index.ts`.
///
/// Pi auto-discovers extensions from subdirectories of
/// `~/.pi/agent/extensions/`, loading `index.ts` as the entry point.
pub fn default_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".pi")
            .join("agent")
            .join("extensions")
            .join("muxa")
            .join("index.ts")
    })
}

pub fn upsert(original: &str) -> (String, Outcome) {
    let body = BODY.replace("__MUXA_BIN__", &super::super::util::locate_muxa());
    marker::upsert_with(original, ID, &body, marker::CommentStyle::Slash)
}

pub fn remove(original: &str) -> (String, Outcome) {
    marker::remove_with(original, ID, marker::CommentStyle::Slash)
}
