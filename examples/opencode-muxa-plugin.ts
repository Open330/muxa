// muxa opencode plugin
//
// Install: copy or symlink to `~/.config/opencode/plugins/muxa.ts`.
// opencode runs plugins in its Bun process, so `Bun.spawn` and `net.connect`
// are available — we shell out to `muxa hook opencode` instead of opening
// the Unix socket directly, to keep the plugin small and version-independent.
//
// This is an *example* — pin versions / harden paths before production use.

import type { Plugin } from "@opencode-ai/plugin"

type AgentEvent =
  | { type: "started"; id: Id; at: string }
  | { type: "prompt_submitted"; id: Id; prompt: string; at: string }
  | { type: "tool_started"; id: Id; tool: string; at: string }
  | { type: "tool_completed"; id: Id; tool: string; success: boolean; at: string }
  | { type: "notification_fired"; id: Id; level: NotifLevel; message: string; at: string }
  | { type: "turn_stopped"; id: Id; at: string }
  | { type: "session_ended"; id: Id; at: string }
  | { type: "heartbeat"; id: Id; model?: string; context_used_pct?: number; cost_usd?: number; at: string }

type NotifLevel = "info" | "needs_input" | "warning" | "error"

type Id = {
  kind: "opencode"
  session_id: string
  pane: string | null
  cwd: string | null
}

const PANE = process.env.TMUX_PANE ?? null
const nowIso = () => new Date().toISOString()

async function send(ev: AgentEvent) {
  const proc = Bun.spawn(["muxa", "hook", "opencode"], {
    stdin: "pipe",
    stdout: "ignore",
    stderr: "pipe",
  })
  proc.stdin.write(JSON.stringify(ev))
  await proc.stdin.end()
  await proc.exited
}

const plugin: Plugin = async ({ directory }) => ({
  // Wildcard event hook — opencode's bus firehose.
  event: async ({ event }) => {
    const now = nowIso()
    const mkId = (sessionID: string): Id => ({
      kind: "opencode",
      session_id: sessionID,
      pane: PANE,
      cwd: directory ?? null,
    })

    switch (event.type) {
      case "session.created":
        await send({ type: "started", id: mkId(event.properties.sessionID), at: now })
        break

      case "session.status": {
        const status = event.properties.status?.type
        const id = mkId(event.properties.sessionID)
        if (status === "idle") await send({ type: "turn_stopped", id, at: now })
        // "busy" / "retry" don't need their own event — covered by prompt/tool hooks.
        break
      }

      case "permission.asked":
        await send({
          type: "notification_fired",
          id: mkId(event.properties.sessionID),
          level: "needs_input",
          message: `permission: ${event.properties.type ?? "tool"}`,
          at: now,
        })
        break

      case "session.error":
        await send({
          type: "notification_fired",
          id: mkId(event.properties.sessionID),
          level: "error",
          message: event.properties.error?.message ?? "session error",
          at: now,
        })
        break

      case "session.deleted":
        await send({ type: "session_ended", id: mkId(event.properties.sessionID), at: now })
        break

      default:
        // Ignore everything else — delta storms, heartbeats, etc.
        break
    }
  },

  // Prompt-level hook — clean signal for "user just submitted".
  "chat.message": async (input, _output) => {
    const firstUserText = _output.parts
      ?.filter((p: any) => p?.type === "text")
      ?.map((p: any) => p.text as string)
      ?.join(" ")
      ?.slice(0, 4000) ?? ""
    await send({
      type: "prompt_submitted",
      id: {
        kind: "opencode",
        session_id: input.sessionID,
        pane: PANE,
        cwd: directory ?? null,
      },
      prompt: firstUserText,
      at: nowIso(),
    })
  },

  "tool.execute.before": async (input, _output) => {
    await send({
      type: "tool_started",
      id: {
        kind: "opencode",
        session_id: input.sessionID,
        pane: PANE,
        cwd: directory ?? null,
      },
      tool: input.tool,
      at: nowIso(),
    })
  },

  "tool.execute.after": async (input, _output) => {
    await send({
      type: "tool_completed",
      id: {
        kind: "opencode",
        session_id: input.sessionID,
        pane: PANE,
        cwd: directory ?? null,
      },
      tool: input.tool,
      success: true,
      at: nowIso(),
    })
  },
})

export default plugin
