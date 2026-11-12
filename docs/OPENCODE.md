# opencode

tend reads opencode's state from opencode's own plugin events, so an opencode
pane gets the same attention states as Claude Code and Codex: working, waiting
on a permission prompt or a question, idle with the turn's response, error.

## Install

```sh
tend init --component opencode-hooks
```

This writes a managed block into `~/.config/opencode/plugins/tend.ts`
(`$XDG_CONFIG_HOME/opencode/plugins/tend.ts` when set). opencode loads every
plugin in that directory at startup, so restart running opencode sessions
after installing or upgrading. Re-running `tend init` replaces the block, which
is how an older tend plugin is upgraded. `tend init --uninstall --component
opencode-hooks` removes it.

The plugin runs `tend hook opencode --event event` once per forwarded event,
so `tend` must be on the `PATH` opencode was started with.

## What tend reads

The plugin forwards a filtered subset of opencode's events; `tend hook
opencode` maps the documented shapes (`@opencode-ai/sdk`, checked against
1.19.5, v1 and v2 event shapes) and ignores everything else.

| opencode signal | tend event | State |
| --- | --- | --- |
| `session.created`, or the first event of a session the TUI switched to | `Started` | idle |
| `chat.message` (non-synthetic text parts of the user message) | `PromptSubmitted` | working |
| tool part `state.status` → `running` | `ToolStarted` (`bash` carries its command) | working |
| tool part `state.status` → `completed` / `error` | `ToolCompleted` | — |
| `permission.updated` (v1), `permission.asked`, `permission.v2.asked` | `NotificationFired` needs input | waiting input |
| `question.asked`, `question.v2.asked` | `NotificationFired` needs choice | waiting choice |
| `permission.replied`, `question.replied` / `rejected` | `ToolCompleted` | working |
| `session.idle` (with the last assistant message's text) | `TurnStopped` | idle |
| `session.error` (except `MessageAbortedError`, i.e. Esc) | `NotificationFired` error | error |
| `session.status` `retry` | `NotificationFired` warning | — |
| assistant `message.updated` with `time.completed` | `Heartbeat` (model, cost) | — |
| `session.deleted` | `SessionEnded` | stopped |

- **Session identity** is opencode's session id (`ses_…`), read only from
  `properties.sessionID`, `properties.info.sessionID` or
  `properties.part.sessionID`.
- **Child sessions** (subagent tasks, `Session.parentID` set) are dropped in
  the plugin, so one pane is one conversation.
- **Switching sessions** in the TUI moves the pane's row to the session that
  is producing events. opencode can run several sessions at once in one
  server; while two are busy in the same pane, the row follows whichever
  reported last.
- **Discovery.** A pane whose process is `opencode` is registered before its
  first event, like `claude`, `codex` and `agy`. opencode creates a session
  only when the first prompt is sent, so until then the row has a placeholder
  id; the first event replaces it.

## Not available

- **Rate limits and context usage.** opencode's events carry cost and model
  but no quota windows, so `rate_limit_*` and `context_used_pct` stay empty
  and usage-limit recovery does not apply to opencode.
- **Turn-boundary prompt delivery.** Claude Code and Codex hooks can hand a
  queued collaboration prompt back to the agent at a turn boundary. The
  opencode bridge is one-way, so peer requests reach an opencode pane the way
  they reach any pane-based agent without that hook: delivered into the pane
  when its row is idle.
- **Account routing** is Claude/Codex only.
