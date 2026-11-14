# GitHub Copilot CLI

tend reads Copilot CLI's state from Copilot's own hooks, so a `copilot` pane
gets the same rows as Claude Code and Codex: working while a turn runs,
waiting on a permission prompt, idle with the turn's answer, stopped when the
session ends. tend watches Copilot; it does not launch it.

Checked against Copilot CLI 1.0.110 (npm `@github/copilot`) on macOS.

## Install

```sh
tend init --component copilot-hooks
```

This writes `~/.copilot/hooks/tend.json` (`$COPILOT_HOME/hooks/tend.json`
when `COPILOT_HOME` is set). Copilot loads every `*.json` file in that folder
as user hooks; user hooks need no folder-trust prompt. tend owns only this one
file: re-running `tend init` rewrites it, `tend init --uninstall --component
copilot-hooks` deletes it, and your other hook files are never touched.
Restart running Copilot sessions after installing. `tend doctor` reports a
missing or incomplete file when Copilot is installed.

## What each hook does

| Copilot hook | Row |
| --- | --- |
| `sessionStart` | the row appears, bound to the pane (`TMUX_PANE`) |
| `userPromptSubmitted` | working, with the prompt |
| `preToolUse` | working, with the tool and its shell command |
| `postToolUse`, `postToolUseFailure` | tool finished (or failed) |
| `notification` with `permission_prompt` | waiting on you, with Copilot's message |
| `agentStop` | idle, with the turn's answer |
| `errorOccurred` | error |
| `sessionEnd` | stopped |

`tend hook copilot` always exits 0 and prints nothing on every hook but one:
Copilot reads a `preToolUse` or `permissionRequest` hook's output as a
decision, so watching can never deny a tool. The exception is `agentStop`,
below.

Notes from the 1.0.110 check:

- `permissionRequest` is not wired. It also fires for a call Copilot
  approves by itself 30 ms later, so it does not mean the prompt is on
  screen; `notification` with `permission_prompt` does.
- No hook payload carries the answer. `agentStop` gives the session log's
  path (`session-state/<id>/events.jsonl`), and tend reads the last
  assistant message from it. Copilot writes the answer to the log up to
  ~200 ms after starting the hook, so the hook waits (at most 1 s) until the
  log holds Copilot's own record of this stop (it carries the hook's
  `timestamp`) and reads the answer written before it. If that record does
  not appear in time, the turn ends without an answer rather than with an
  earlier turn's.
- The npm build runs as `node` with the native `copilot` binary under it; a
  pane is still recognized as Copilot, from its process tree.

## Esc on a permission prompt

Esc cancels the prompt and the turn, and Copilot fires no hook for it: no
`postToolUse`, no `agentStop`. Copilot records the cancel only in its session
log (`permission.completed` cancelled, `assistant.turn_end`, `abort`).

tendd reads that log. Every two seconds, for each Copilot row that is working
or waiting, it reads the end of `$COPILOT_HOME/session-state/<id>/events.jsonl`;
when the turn there ended in `abort` (after the turn's prompt and its last
`assistant.turn_start`), the row goes idle, dated at the abort. A hook newer
than that (the next prompt) always wins. `assistant.turn_end` alone settles
nothing, because Copilot writes one after every model call. Idle rows are not
read. Set `[reconciler] copilot_session_log_enabled = false` to turn it off
(see [CONFIGURATION.md](CONFIGURATION.md)); tendd reads `COPILOT_HOME` from
its own environment, so a Copilot run with a different `COPILOT_HOME` than
tendd's keeps the hooks-only behaviour.

On 1.0.110, Esc on a permission prompt settled the row within two seconds;
the next prompt then worked as usual.

## Turn-boundary delivery

At the end of a turn, `tend hook copilot --event agent_stop` asks tendd for
the requests the user authorized for this session (`tend msg send …
--user-authorized`, or the MCP `user_authorized` flag), the same offers a
Claude Code Stop hook gets. When there are any it prints `{"decision":
"block", "reason": TEXT}`, and Copilot runs another turn with TEXT as its
prompt, so nothing is typed into the pane. The offers are asked for before
the stop is recorded, as at a Claude Code Stop. tend's offer budget bounds
the repeats, and Copilot also ends the turn after 8 blocks in a row.

Copilot has no tend MCP server, so TEXT starts by telling the model to use
the `tend` command in its shell (`tend msg accept ID --json`, then `tend msg
reply ID BODY`). Copilot asks before running a shell command unless you allow
it (`--allow-tool 'shell(tend)'`).

Checked on 1.0.110 against a scratch tendd: a request from a Claude session
in the same window, queued while Copilot was idle, was offered at the end of
Copilot's next turn. Copilot claimed it with `tend msg accept`, and the reply
`PONG` reached the sender with `tend msg reply`.
