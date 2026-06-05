# Timeline

`muxa timeline` shows how agent sessions moved through work, waiting,
errors, human interaction, and tmux foreground time. It is the visual view of
the same duration data used by `muxa stats` and `muxa report`.

## Quick Start

```bash
muxa timeline --since today
muxa timeline --since today --session main
muxa timeline --since 24h --agent codex
muxa timeline --since today --group-by kind
muxa timeline --since today --sort waiting
muxa timeline --since today --format json
```

The default overview is grouped by tmux session. Each session group can show:

| Lane | Meaning |
| --- | --- |
| Agent | Agent state spans: working, waiting, error, starting, idle. |
| Human | Human interaction spans recorded by muxa, such as prompt input or tmux attach. |
| tmux | Time the tmux session was foregrounded by an interactive client. |

## Options

| Option | Values |
| --- | --- |
| `--since` | `today`, `yesterday`, `week`, rolling durations like `24h`/`7d`/`4w`, RFC3339 timestamps, or `all`. |
| `--session` | tmux session name, tmux session id, or pane id. |
| `--agent` | `codex`, `claude-code`, `gemini-cli`, `opencode`, `unknown`. |
| `--group-by` | `session` default, `kind`, or `flat`. TUI only. |
| `--sort` | `latest` default, `name`, `duration`, `working`, `waiting`, `error`, `human`, or `foreground`. Also accepts aliases like `dur`, `work`, `wait`, `err`, and `tmux`. |
| `--format` | `tui` default or `json`. |
| `--theme` | Same one-shot theme override style as other muxa TUIs. |

## TUI Keys

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Select a lane in overview, or an interval in focus view. |
| `h` / `l`, left/right | Pan the visible time window. |
| `+` / `-` | Zoom in or out. |
| `0` | Jump back to the latest view. |
| `f` | Fit the full selected `--since` range. |
| `g` | Cycle grouping: `session` -> `kind` -> `flat`. |
| `s` | Cycle sorting: `latest` -> `duration` -> `working` -> `waiting` -> `error` -> `human` -> `foreground` -> `name`. |
| `tab` / `shift-tab` | Cycle intervals on the selected lane. |
| `enter` / `o` | Toggle overview and focus views. |
| `r` | Reload activity and live agent state. |
| `?` | Show help. |
| `q` / `Esc` | Quit. |

The TUI starts on the latest six-hour viewport when the selected range is
wide enough. That makes `h` useful immediately for moving backward in time.
If you press `l` while already at the newest edge, the footer reports that
there is no later window to show.

## Data Semantics

Closed intervals come from `activity.ndjson`. Currently-open agent states
come from the live daemon snapshot. Currently-open tmux foreground spans come
from `session-activity.json` when session activity tracking is enabled.

Agent transition rows render the state they left. For example, a ledger row
`working -> waiting_input` draws a `working` span from `state_entered_at` to
the transition timestamp. This keeps duration accounting consistent with
`muxa stats`.

## Dashboard

The dashboard timeline uses the same `/api/timeline` document and groups
lanes by session by default. Use the dashboard when you want a persistent
browser view; use `muxa timeline` when you want keyboard navigation, focus
view, and terminal-native JSON export.
