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
| `session.created`, or the first prompt into a session tend has not seen created | `Started` | idle |
| `chat.message` (non-synthetic text parts of the user message) | `PromptSubmitted` | working |
| tool part `state.status` → `running` | `ToolStarted` (`bash` carries its command) | working |
| tool part `state.status` → `completed` / `error` | `ToolCompleted` | — |
| `permission.updated` (v1), `permission.asked`, `permission.v2.asked` | `NotificationFired` needs input | waiting input |
| `question.asked`, `question.v2.asked` | `NotificationFired` needs choice | waiting choice |
| `permission.replied`, `question.replied` / `rejected` | `ToolCompleted` | working |
| `session.idle` (confirmed idle; the assistant's text only when the turn had no `session.error`) | `TurnStopped` | idle, or error kept |
| `session.error` (except `MessageAbortedError`, i.e. Esc) | `NotificationFired` error | error |
| `session.status` `retry` | `NotificationFired` warning | — |
| assistant `message.updated` with `time.completed` | `Heartbeat` (model, cost) | — |
| `session.deleted` | `SessionEnded` | stopped |

- **Session identity** is opencode's session id (`ses_…`), read only from
  `properties.sessionID`, `properties.info.sessionID` or
  `properties.part.sessionID`.
- **Child sessions** (subagent tasks, `Session.parentID` set) are dropped in
  the plugin, so one pane is one conversation.
- **Pane binding comes from prompts, not from event arrival.** opencode hands
  the plugin every session's events in its directory, so an event does not
  say which session the TUI is showing. A session is bound to the pane when
  it is created, or by the first prompt sent into it (a resumed session),
  once. Events from other sessions update only their own rows. Switching
  back to a session already bound in this pane does not move the row.
- **Responses** are reset at each prompt and reported only for a turn that
  ended without `session.error`; a failed or aborted turn's partial output is
  not recorded as its answer, and a failed turn stays in error after idle.
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
- **Modes.** Discovery sees the process name only, so a pane running
  `opencode serve` or `opencode run` is registered too and stays idle; only
  the interactive TUI (what `tend agent start --agent opencode` launches)
  produces events.
