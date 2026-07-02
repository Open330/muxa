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

// Forward one event object to `muxa hook pi`. Fire-and-forget: errors
// are swallowed so a down daemon never blocks the agent loop. We spawn
// detached and unref so the child can never hold Pi's process alive.
function forward(type: string, payload: Record<string, unknown>, ctx: ExtensionContextLike) {
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
    child.on("error", () => {});
    child.stdin?.end(body);
    child.unref();
  } catch {
    // swallow — observability must never break the agent
  }
}

type ExtensionContextLike = {
  cwd?: string;
  model?: { id?: string };
  sessionManager?: { getSessionId?: () => string };
};

export default function (pi: ExtensionAPI) {
  // session lifecycle
  pi.on("session_start", async (_event, ctx) => {
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
  pi.on("turn_end", async (event, ctx) => {
    const usage = event.message?.usage;
    forward("turn_end", {
      model: ctx.model?.id,
      cost_usd: usage?.cost?.total,
    }, ctx);
  });

  // agent loop end — final assistant message text, when extractable.
  pi.on("agent_end", async (event, ctx) => {
    const last = event.messages?.at(-1);
    forward("agent_end", {
      response:
        typeof last?.content === "string" ? last.content : undefined,
    }, ctx);
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
