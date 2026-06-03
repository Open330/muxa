# Activity Ledger

`muxa` keeps duration data in `activity.ndjson`. This is the source for
`muxa stats`, `muxa report`, and the raw `muxa activity` query command.

## Quick Views

```bash
muxa stats --since today
muxa stats --since yesterday --group-by session
muxa stats --since week --group-by project --sort human
muxa report --since week
muxa activity --since today --type human
muxa activity --since today --type agent --format json
```

`--since` accepts:

- `today`: local calendar day from 00:00 to now.
- `yesterday`: previous local calendar day, 00:00 to 00:00.
- `week`: rolling last 7 days.
- `24h`, `7d`, `4w`: rolling durations.
- RFC3339 timestamp: everything since that instant.
- `all`: all retained ledger entries.

## Ledger Types

`muxa activity --type ...` filters the raw ledger:

| Type | Meaning |
| ---- | ------- |
| `agent` | Agent state transition intervals, such as working, waiting, or error. |
| `tmux` | Closed tmux foreground intervals observed from interactive tmux clients. |
| `human` | Human interaction intervals recorded by muxa itself. |

`--type state` is kept as a hidden compatibility alias for `--type agent`.

## Stats Columns

| Column | Source |
| ------ | ------ |
| `WORK` | Time spent in agent working states. |
| `WAIT` | Time spent waiting for input or choice. |
| `ERR` | Time spent in error states, including quota/rate-limit style blocks when the agent reports them as error. |
| `TMUX` | Time a tmux session was foregrounded by an interactive tmux client. |
| `HUMAN` | Union of tmux foreground time plus muxa human interaction intervals. |
| `THINK` | Overlap of attention states with human presence. |
| `BLOCK` | Count of transitions into Waiting/Error attention states. |

`THINK` is intentionally narrower than `HUMAN`. It counts time where the
agent needs attention (`WaitingInput`, `WaitingChoice`, or `Error`) and
there is human presence from tmux foreground, muxa prompt input, or tmux
attach. A plain open `muxa watch` interval counts toward `HUMAN`, but not
`THINK`, because simply watching the dashboard can be idle time.

Use `muxa stats --sort human` to order rows by `HUMAN`. Other sort keys
include `prompts`, `foreground`/`tmux`, `thinking`, `working`, `waiting`,
`error`, `attention`/`blocks`, `last-prompt`, `key`, `agent-sessions`,
`live-agents`, `token-estimate`, and `words`.

## Retention

`activity.ndjson` is append-only and retained according to `[activity]`.
Older `session-activity.json` totals remain as a legacy fallback until
foreground intervals exist in the activity ledger.
