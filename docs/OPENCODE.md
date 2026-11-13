# opencode

tend reads opencode's state from opencode's own plugin events, so an opencode
pane gets the same attention states as Claude Code and Codex: working, waiting
on a permission prompt or a question, idle with the turn's response, error.

## Install

```sh
tend init --component opencode-hooks
```

This writes two plugins under `~/.config/opencode/plugins/`
(`$XDG_CONFIG_HOME/opencode/plugins/` when set, on macOS too):

| File | Loaded by | Runs in |
| --- | --- | --- |
| `tend.ts` | opencode 1.x | opencode's process in the pane |
| `tend/tui.ts` | opencode 2.x | the 2.x TUI in the pane |

opencode 2.x replaced the plugin API and runs sessions in a background server
that every TUI shares, so a server plugin there has no pane. tend's 2.x plugin
is a TUI plugin for that reason. Each opencode version loads only its own file
(2.x also loads `tend.ts`, whose default export is a no-op there), so an
opencode upgrade needs no new `tend init`. Restart running opencode sessions
after installing or upgrading the plugins. Re-running `tend init` replaces both
managed blocks, and `tend init --uninstall --component opencode-hooks` removes
them.

Earlier tends wrote `tend.ts` to `~/Library/Application Support/opencode/` on
macOS, which opencode never reads; `tend init` removes that copy.

Each plugin spawns `tend hook opencode --event event` once per forwarded
event, so `tend` must be on the `PATH` opencode was started with.

Both plugins were run against the real opencode TUIs, 1.19.5 (npm
`opencode-ai`) and 2.2.7 (Homebrew), through a turn with an approved
permission, a question, and a rejected permission. What they forwarded is
replayed in tend's tests (`crates/tend/src/adapters/testdata/`).

## What tend reads

The 1.x plugin forwards a filtered subset of opencode's events; `tend hook
opencode` maps the documented shapes (`@opencode-ai/sdk`, checked against
1.19.5, v1 and v2 event shapes) and ignores everything else. The 2.x plugin
translates the 2.x stream into the same payloads (second table).

| opencode signal | tend event | State |
| --- | --- | --- |
| a prompt into a session other than the pane's current one | `Started` | idle |
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

opencode 2.x:

| opencode 2.x event | Forwarded as |
| --- | --- |
| `session.inbox.enqueued` (a `user` item, into the session this TUI shows) | `tend.prompt` |
| `session.execution.started` | `tend.execution.started`: working, also when no prompt was forwarded first |
| `session.tool.called` / `session.tool.success` / `session.tool.failed` | tool part `running` / `completed` / `error` (`shell` carries its command) |
| `permission.asked` (`action`, `resources`), `permission.replied` | the same types |
| `form.created`, `form.replied` / `form.cancelled` | `question.asked`, `question.replied` / `question.rejected` |
| `session.execution.succeeded` | `session.idle` with the last assistant message's text |
| `session.execution.interrupted` (Esc, a rejected permission) | `session.idle` without a response |
| `session.execution.failed` | `session.error`, then `session.idle` without a response |
| `session.retry.scheduled` | `session.status` `retry` |
| `session.step.ended` | assistant `message.updated` (model, cost) |
| `session.deleted` | `session.deleted` |

- **Session identity** is opencode's session id (`ses_…`), read only from
  `properties.sessionID`, `properties.info.sessionID` or
  `properties.part.sessionID`.
- **Child sessions** (subagent tasks, `Session.parentID` set) are dropped in
  the plugin, so one pane is one conversation.
- **Pane binding comes from prompts, not from event arrival.** opencode hands
  the plugin every session's events in its directory, so an event does not
  say which session the TUI is showing. A prompt binds the pane to its
  session, and a prompt into a different session rebinds it, so the pane
  keeps one live row. Only the bound session's events are forwarded: other
  sessions in the same directory (another client's, or one the TUI switched
  away from) are not tracked from this pane. A prompt sent by another client
  through opencode's API rebinds the pane the same way. In 2.x the server's
  stream covers every TUI's sessions, so only a prompt into the session *this
  TUI is showing* binds the pane; viewing another session moves nothing until
  it is prompted. The row keeps the bound session's directory, taken from the
  event that bound it, while the TUI views another project.
- **Responses** are reset at each prompt and reported only for a turn that
  ended without `session.error`; a failed or aborted turn's partial output is
  not recorded as its answer, and a failed turn stays in error after idle.
- **Discovery.** A pane whose process is `opencode` is registered before its
  first event, like `claude`, `codex` and `agy`. opencode creates a session
  only when the first prompt is sent, so until then the row has a placeholder
  id; the first event replaces it.

## Turn-boundary delivery

Requests the user authorized for an opencode session reach it the way they
reach Claude Code and Codex: at the end of a turn, through opencode itself,
never typed into the terminal where an unsent draft could merge with them.
When a turn ends cleanly, `tend hook opencode` asks tendd for turn-boundary
offers (the same ones Claude Code's Stop hook carries) and prints them as
`{"prompt": TEXT}`. The plugin then prompts the bound session with TEXT
through opencode's API: `client.session.promptAsync` on 1.x,
`client.session.prompt` with `delivery: "queue"` on 2.x.

- Only a clean turn end delivers. An Esc-interrupted or failed turn does not,
  so an offer never restarts a turn the user stopped.
- If anyone prompted the session after that turn ended, the offer waits for
  the next boundary. Nothing is consumed until the model claims a request by
  id (`tend msg accept ID`), so a skipped offer loses nothing.
- Requests that are not turn-boundary offers still reach the pane the way
  they reach any pane-based agent: delivered when its row is idle.

Checked against the real opencode 1.19.5 and 2.2.7 TUIs: an offered prompt
returned at the first turn's end ran as the session's next turn.

## Not available

- **Rate limits and context usage.** opencode's events carry cost and model
  but no quota windows, so `rate_limit_*` and `context_used_pct` stay empty
  and usage-limit recovery does not apply to opencode.
- **Account routing** is Claude/Codex only.
- **Modes.** Discovery sees the process name only, so it cannot tell the
  interactive TUI from `opencode serve` or `opencode run`, and registers a
  pane running either. On 1.x the plugin runs in opencode's server in every
  mode; on 2.x only the TUI loads tend's plugin. Only the TUI (what `tend agent start --agent opencode` launches) gives
  the pane an input target for typed delivery.
