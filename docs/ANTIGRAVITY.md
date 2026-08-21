# Antigravity CLI (`agy`)

Google's Antigravity CLI succeeded the Gemini CLI. It keeps the `~/.gemini`
directory and it still runs Gemini models, which makes it easy to assume muxa's
existing Gemini support carries over. It does not — **none** of the three things
muxa needs is the same.

| | Gemini CLI (`gemini`) | Antigravity CLI (`agy`) |
| --- | --- | --- |
| Hook config | `hooks` key inside `~/.gemini/settings.json` | its own `~/.gemini/config/hooks.json` |
| Events | `SessionStart`, `BeforeAgent`, `AfterAgent`, `BeforeTool`, `AfterTool`, `Notification`, `SessionEnd` | `SessionStart`, `PreInvocation`, `PostInvocation`, `PreToolUse`, `PostToolUse`, `Stop` |
| Payload | snake_case, `session_id` | camelCase protojson, `conversationId` |
| muxa kind | `gemini_cli` | `antigravity` |
| muxa hook | `muxa hook gemini` | `muxa hook agy` |

The failure mode is silent in both directions: hooks written into
`settings.json` make agy log `loaded 0 named hooks from 0 hooks.json file(s)`
and carry on, and the Gemini adapter's required `session_id` would reject an agy
payload outright. Hence a separate `AgentKind`, adapter, and init component
rather than a rename. Both CLIs stay supported and can be installed side by
side.

## Install

```sh
muxa init --component agy-hooks
muxa doctor          # → "Hooks · Antigravity — hook installed in …"
```

That writes a single `muxa` key into `~/.gemini/config/hooks.json`:

```json
{
  "muxa": {
    "SessionStart":   [{ "type": "command", "command": "muxa hook agy --event session_start",   "timeout": 10 }],
    "PreInvocation":  [{ "type": "command", "command": "muxa hook agy --event pre_invocation",  "timeout": 10 }],
    "PostInvocation": [{ "type": "command", "command": "muxa hook agy --event post_invocation", "timeout": 10 }],
    "Stop":           [{ "type": "command", "command": "muxa hook agy --event stop",            "timeout": 10 }],
    "PreToolUse":  [{ "matcher": "*", "hooks": [{ "type": "command", "command": "muxa hook agy --event pre_tool_use",  "timeout": 10 }] }],
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "muxa hook agy --event post_tool_use", "timeout": 10 }] }]
  }
}
```

`hooks.json` is keyed by hook **name**, so muxa owns exactly one top-level key.
Entries written by agy's own `/hooks` command, by plugins, or by hand are never
touched, and `muxa init --uninstall --component agy-hooks` drops only the `muxa`
key.

Two shapes matter and are easy to get backwards: `PreToolUse`/`PostToolUse`
wrap their handlers in a `{matcher, hooks[]}` group, while the lifecycle events
take a flat handler list. A handler in the wrong shape is dropped without an
error.

> **agy caches hooks in a long-lived backend process.** Editing `hooks.json`
> does not take effect in an agy session that is already running — restart it.
> The same cache means a stale hook can outlive the file that declared it.

## Event mapping

| agy event | muxa event | Row effect |
| --- | --- | --- |
| `SessionStart` | `Started` | `Idle`. Fires on a new session only — `agy -c` resumes without it. |
| `PreInvocation`, `invocationNum == 0` | `PromptSubmitted` | `Working`, prompt read from the transcript |
| `PreInvocation`, `invocationNum > 0` | `Heartbeat` | keeps the row warm, refreshes `model` |
| `PreToolUse` | `ToolStarted` | `Working`, tool name from `toolCall.name` |
| `PostToolUse` | `ToolCompleted` | `success` from a non-empty `error` |
| `PostInvocation` | `Heartbeat` | refreshes `model` |
| `Stop` (normal) | `TurnStopped` | `Idle`, response read from the transcript |
| `Stop` (`terminationReason: ERROR`, or non-empty `error`) | `NotificationFired{Error}` | `Error` until the next prompt |

### Turn boundaries

`invocationNum` counts model calls **within one turn** and resets to `0` for
each new user request — verified against agy 1.1.17 across a two-turn session,
where one turn ran four invocations. `PreInvocation` at `0` is therefore the
only place a prompt may be reported; treating every invocation as a turn start
would restate the prompt once per model call.

### Prompts and responses come from the transcript

No agy payload carries prompt or response text — they carry a `transcriptPath`.
muxa reads the tail of that JSONL: the last `USER_INPUT`/`USER_EXPLICIT` step
(unwrapped from agy's `<USER_REQUEST>` envelope) for the prompt, and the last
`PLANNER_RESPONSE` step with string `content` for the response. Both fail
silently — a hook runs synchronously inside agy's loop, so a missing or
malformed transcript must cost a blank cell, never a stall.

## `muxa hook agy` is fail-open and silent

agy reads a hook's **stdout as a verdict**. An empty stdout means "no opinion";
a `PreToolUse` reply that is JSON but carries no valid `decision` blocks the
call outright (`tool call denied by pre-tool hook`), and so does a non-zero
exit. `muxa hook agy` therefore:

- writes **zero bytes** to stdout on every event, and
- exits `0` even when the payload is unparseable, the event flag is unknown, or
  muxad is down.

Observation must never be able to block the agent. This is the one hook handler
in muxa that swallows its own errors for that reason.

## What agy does not expose

- **No permission/notification hook.** An agy row cannot reach `WaitingInput`
  from hooks. The bundled `agy` screen manifest covers that for panes with no
  hooks wired; where hooks *are* wired they take precedence and the approval
  prompt is not observed. See [SCREEN_DETECTION.md](SCREEN_DETECTION.md).
- **No session-end hook.** `Stop` is a turn boundary, not a session boundary, so
  agy rows are reaped by pane liveness like Codex's.
- **`workspacePaths` is empty in print mode.** `cwd` is populated for
  interactive sessions with a folder open and absent for `agy -p`. A hook's own
  process cwd is the directory holding `hooks.json`, so there is nothing to fall
  back to.
- **No rate-limit or cost signal.** `modelName` rides on every payload (more
  than Codex or the Gemini CLI give), but the usage fields stay `None`.

## Elsewhere in muxa

| Surface | Value |
| --- | --- |
| Discovery (`pane_current_command`) | `agy`, `antigravity` |
| `muxa agent start --agent` | `agy` (alias `antigravity`) → `agy --dangerously-skip-permissions [-i <prompt>]` |
| `muxa watch` spawn form | `agy` |
| `muxa timeline --agent` | `antigravity` (alias `agy`) |
| MCP `muxa_start_agent` | `"agy"` |
| Wire / dashboard kind | `antigravity` |
| omp sink | `source: antigravity`, `cli_name: agy` |
