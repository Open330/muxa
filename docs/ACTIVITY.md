# Activity Ledger

`muxa` keeps duration data in `activity.ndjson`. This is the source for
`muxa stats`, `muxa report`, and the raw `muxa activity` query command.

## Quick Views

```bash
muxa stats --since today
muxa stats --since yesterday --group-by session
muxa stats --since week --group-by project
muxa stats --since month --exclude-session 'monitor*'
muxa report --since week
muxa report --since last-month --exclude-pane '%42'
muxa timeline --since today --session main
muxa activity --since today --type human
muxa activity --since today --type agent --format json
```

`--since` accepts:

- `today`: local calendar day from 00:00 to now.
- `yesterday`: previous local calendar day, 00:00 to 00:00.
- `week`: rolling last 7 days.
- `month`: rolling last 30 days.
- `last-week`, `"last week"`: previous local Monday-Sunday calendar week.
- `last-month`, `"last month"`: previous local calendar month.
- `24h`, `7d`, `4w`, `30d`: rolling durations.
- `YYYY-MM-DD`: one local calendar day.
- RFC3339 timestamp: everything since that instant.
- `all`: all retained ledger entries.

## Sorting

`muxa stats` rows are ordered by prompt count by default. Use `--sort` to
order by any column and `--reverse` to flip the direction. Numeric columns
default to descending (largest first); `name` defaults to ascending.

```bash
muxa stats --since today --sort wait              # longest waiting first
muxa stats --since today --sort work --reverse    # least working first
muxa stats --since today --group-by session --sort name
```

Use `--exclude-pane` and `--exclude-session` to remove long-lived monitoring
scopes from both rows and totals. Values can be repeated or comma-separated;
patterns are case-sensitive and support `*` and `?`, e.g.
`--exclude-session 'monitor*'`.

`--sort` accepts: `prompts`, `work`, `wait`, `err`, `tmux`, `human`,
`think`, `active`, `block`, `tok`, `words`, `sess`, `agents`, `last`, `name`.

## Stats Time Graph

Add `--graph` to `muxa stats` to render only a terminal-native time graph for
`WACT`, the hands-on subset of active human time:

```bash
muxa stats --since today --graph
muxa stats --since week --graph
```

The bucket size follows the selected window: ranges up to two days use hourly
buckets, ranges up to 60 days use daily buckets, ranges up to 400 days use
weekly buckets, and longer ranges use monthly buckets. JSON output includes the
same data under `time_graph` only when `--graph` is set, with both
`work_active_secs` and `active_secs` per bucket. Markdown output includes a
bucket table plus the usual stats tables when `--graph` is set.

`ACTIVE` (the `ACT` column, `active` / `active_secs` in JSON) estimates engaged
human time as the union of three signals, none of which a forgotten attach ever
triggers:

- **Prompts** — a window around each submitted prompt (60s before, 5m after).
- **tmux input** — the daemon samples each client's `client_activity` and records
  a `tmux_input` interaction whenever it advances (a keypress or scroll), so
  *reading* a session while attached is credited, not just typing. (Visible via
  `muxa activity --type human`.)
- **Thinking** — time present while an agent is blocked on you
  (`WaitingInput`/`WaitingChoice`/`Error`): reading its question and deciding.

Unlike `HUMAN` (raw presence: tmux foreground, prompt input, attach, or
`muxa watch`), a pane left attached and untouched accrues none of these, so it
does not inflate `ACTIVE`. Prompt and tmux-input windows are clipped to matching
active presence (tmux foreground, prompt input, or attach; not a plain open
`muxa watch` interval), so padding cannot make a single session's `ACT` exceed
its observed foreground/interaction time.

Across sessions, `ACTIVE` is **de-duplicated**: a human does one thing at a time,
so each instant is attributed to the most recently touched session ("last
touch"). Per-session `ACT` therefore sums to a grand total that stays within real
elapsed time, rather than multiplying it when many agents run at once. Window
padding is configurable — `[stats] active_lookback_secs` (default 60),
`active_timeout_secs` for prompts (default 300), and
`active_tick_timeout_secs` for tmux input ticks (default 90); smaller values
count more conservatively.

`WACT` (`work_active` / `work_active_secs` in JSON) is the hands-on subset of
`ACT`: the same last-touch attribution is used, but a second only counts when
the winning window came from a prompt, keypress, or thinking span. Scrollback
ticks can still count as engaged `ACT`, but not as `WACT`, so every row's
`WACT` stays within that row's `ACT`.

The default terminal table is intentionally narrow and shows the core review
columns: group name, `WACT`, `ACT`, `WORK`, `WAIT`, `BLK`, `PROMPTS`, and
`LAST`. Use `--verbose` when you need the remaining diagnostic columns. JSON
and Markdown output keep the full field set.

The table closes with a `TOTAL` footer row. It holds the grand total across
every group and reflects all data even when `--limit` truncates the rows
above it.

## Timeline

`muxa timeline` renders the same duration source as an interactive TUI.
The overview groups lanes by session by default, with agent, human, and tmux
foreground lanes inside each session; focus view follows the selected lane as
timestamped intervals.

```bash
muxa timeline --since today
muxa timeline --since today --session main
muxa timeline --since today --exclude-session 'monitor*'
muxa timeline --since 24h --agent codex
muxa timeline --since today --group-by kind --sort waiting
muxa timeline --view heatmap --since 12w
muxa timeline --day 2026-06-06
muxa timeline --since today --format json
```

Agent transition rows render the state they left. For example, a ledger row
`working -> waiting_input` contributes a `working` span from
`state_entered_at` to the transition timestamp.

For TUI keybindings, grouping modes, dashboard behavior, and JSON export
details, see [docs/TIMELINE.md](TIMELINE.md).

## Ledger Types

`muxa activity --type ...` filters the raw ledger:

| Type | Meaning |
| ---- | ------- |
| `agent` | Agent state transition intervals, such as working, waiting, or error. |
| `tmux` | Closed tmux foreground intervals observed from interactive tmux clients. |
| `human` | Human interaction intervals recorded by muxa itself. |

`--type state` is kept as a hidden compatibility alias for `--type agent`.

## Stats Columns

The default terminal table shows the core review columns. Remaining diagnostic
columns are shown in terminal output with `--verbose` and are always present in
JSON and Markdown exports.

| Column | Source |
| ------ | ------ |
| `PROMPTS` | Prompt count retained in daemon history for the selected range. |
| `WORK` | Time spent in agent working states. |
| `WAIT` | Time spent waiting for input or choice. |
| `ERR` | Time spent in error states, including quota/rate-limit style blocks when the agent reports them as error. |
| `TMUX` | Time a tmux session was foregrounded by an interactive tmux client. |
| `HUMAN` | Union of tmux foreground time plus muxa human interaction intervals. |
| `THINK` | Overlap of attention states with human presence. |
| `ACT` | Engaged human time from prompts, tmux input ticks, and thinking. |
| `WACT` | Hands-on subset of `ACT`; excludes scrollback-owned engaged time. |
| `BLOCK` / `BLK` | Count of transitions into Waiting/Error attention states. |
| `LAST` | Age of the latest retained prompt in the group. |

`THINK` is intentionally narrower than `HUMAN`. It counts time where the
agent needs attention (`WaitingInput`, `WaitingChoice`, or `Error`) and
there is human presence from tmux foreground, muxa prompt input, or tmux
attach. A plain open `muxa watch` interval counts toward `HUMAN`, but not
`THINK`, because simply watching the dashboard can be idle time.

## Retention

`activity.ndjson` is append-only and retained according to `[activity]`.
Older `session-activity.json` totals remain as a legacy fallback until
foreground intervals exist in the activity ledger.
