# Timeline

`tend timeline` shows how agent sessions moved through work, waiting,
errors, human interaction, and tmux foreground time. It is the visual view of
the same duration data used by `tend stats` and `tend stats report`.

## Quick Start

```bash
tend timeline --since today
tend timeline --since today --session main
tend timeline --since today --exclude-session 'monitor*'
tend timeline --since 24h --agent codex
tend timeline --since today --group-by kind
tend timeline --since today --sort waiting
tend timeline --view heatmap --since 12w
tend timeline --day 2026-06-06
tend timeline --since today --format json
```

The default overview is grouped by tmux session. Each session group can show:

| Lane | Meaning |
| --- | --- |
| Agent | Agent state spans: working, waiting, error, starting, idle. |
| Human | Human interaction spans recorded by tend, such as prompt input or tmux attach. |
| tmux | Time the tmux session was foregrounded by an interactive client. |

## Options

| Option | Values |
| --- | --- |
| `--since` | `today`, `yesterday`, `week` for rolling 7 days, `month` for rolling 30 days, `last-week` / `"last week"` for the previous Monday-Sunday calendar week, `last-month` / `"last month"` for the previous calendar month, rolling durations like `24h`/`7d`/`4w`, local dates like `2026-06-06`, RFC3339 timestamps, or `all`. |
| `--day` | Shortcut for one local calendar day, e.g. `--day 2026-06-06`. |
| `--session` | tmux session name, tmux session id, or pane id. |
| `--exclude-pane` | Drop pane ids matching a case-sensitive glob. Repeat or comma-separate values. |
| `--exclude-session` | Drop tmux session names or ids matching a case-sensitive glob. Repeat or comma-separate values. |
| `--agent` | `codex`, `claude-code`, `gemini-cli`, `antigravity` (alias `agy`), `opencode`, `unknown`. |
| `--view` | `timeline` default, or `heatmap` for a terminal contribution-map summary. |
| `--group-by` | `session` default, `kind`, or `flat`. TUI only. |
| `--sort` | `latest` default, `name`, `duration`, `working`, `waiting`, `error`, `human`, or `foreground`. Also accepts aliases like `dur`, `work`, `wait`, `err`, and `tmux`. |
| `--format` | `tui` default or `json`. |
| `--theme` | Same one-shot theme override style as other tend TUIs. |

## Heatmap View

`tend timeline --view heatmap --since 12w` prints a compact daily activity
map in the terminal. Each cell is a local calendar day; intensity is based on
agent work, waiting, errors, human interaction, and tmux foreground time.
Week rows are ISO-style Monday-first, matching `--since last-week`.
Below the grid, tend lists the busiest days and, for a single-day view, the
top sessions for that day.

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
`tend stats`.

## Dashboard

The dashboard timeline uses the same `/api/timeline` document and groups
lanes by session by default. It also renders a daily contribution-map style
heatmap above the lane graph; click a day to drill into that calendar day.
Use the dashboard when you want a persistent browser view; use
`tend timeline` when you want keyboard navigation, focus view, terminal
heatmaps, terminal-native JSON export, and scope exclusions.
