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

`tend hook copilot` prints nothing and always exits 0, whatever it receives:
Copilot reads a `preToolUse` or `permissionRequest` hook's output as a
decision, so watching can never deny a tool.

Notes from the 1.0.110 check:

- `permissionRequest` is not wired. It also fires for a call Copilot
  approves by itself 30 ms later, so it does not mean the prompt is on
  screen; `notification` with `permission_prompt` does.
- No hook payload carries the answer. `agentStop` gives the session log's
  path (`session-state/<id>/events.jsonl`), and tend reads the last
  assistant message from it. Copilot writes the answer to the log up to
  ~200 ms after starting the hook, so the hook waits (at most 1 s) until the
  log holds the stop before reading.
- The npm build runs as `node` with the native `copilot` binary under it; a
  pane is still recognized as Copilot, from its process tree.

## Esc on a permission prompt

Esc cancels the prompt and the turn, and Copilot fires no hook for it: no
`postToolUse`, no `agentStop`. With hooks alone the row stays waiting until
the next prompt. Copilot records the cancel only in its session log
(`abort`, then `assistant.turn_end`).
