# CLI dashboard

`muxa dashboard` is a session-card TUI console. It is built for operating
multiple agent sessions from one screen without attaching to tmux first.

Use `muxa watch` when you want the compact picker/table. Use
`muxa dashboard` when you want a richer console with cards, an inspector,
live terminal capture, prompt composition, and session-level ACT/WACT totals.

## Open

```bash
muxa dashboard
muxa dashboard --since today
muxa dashboard --sort attention
muxa dashboard --include-paneless
```

`--since` accepts the same time windows as `muxa stats --since`, including
`today`, `24h`, `7d`, local dates, RFC3339 timestamps, and `all`.

## Keys

| Key | Action |
| --- | --- |
| `↑` / `↓` / `←` / `→`, `h` / `j` / `k` / `l` | Move card selection. |
| `Tab`, `[` / `]` | Cycle the action target inside the selected session card. |
| `PageUp` / `PageDown` | Scroll live capture history. |
| `G` / `End` | Return capture to the latest output. |
| `f` | Toggle capture fullscreen. |
| `n` | Open dashboard notes when diagnostics are available. |
| `Enter` | Toggle the selected session inspector. |
| `p` | Open the prompt composer for the selected pane or muxa PTY session. |
| `c` | Copy the selected session's latest prompt. |
| `R` | Confirm and send Ctrl-C to the selected pane or PTY session. |
| `K` | Confirm and terminate the selected pane or PTY session. |
| `o` | Explicitly open the selected pane/session. |
| `r` | Refresh now. |
| `?` | Help. |
| `q` / `Esc` | Quit. |

`o` is the only key that intentionally leaves the dashboard and attaches or
opens the target. Prompt sending, Ctrl-C, copy, and live capture all run from
inside the dashboard.

Pane prompt/abort/terminate actions currently require tmux pane control. zellij
cards are still visible and can be focused with `o`, but write/destructive pane
actions are disabled with an explicit hint until a zellij-safe input path exists.

## Cards and inspector

Cards are grouped by tmux/zellij session, muxa-owned PTY session, or detached
agent session. Each card shows:

- host type, agent count, pane count
- current dominant state
- latest activity age and foreground time
- ACT/WACT for the selected `--since` window
- model/context/cost hints when agents report them
- last prompt or notification preview

The inspector shows the selected card's details plus a live capture of the
primary pane or PTY session when the backend supports capture.

For multi-pane cards, the highlighted action target controls where `p`, `R`,
`K`, `o`, and capture apply. Use `Tab`, `[` or `]` to move that target without
leaving the dashboard. Confirm prompts name the exact pane or PTY session before
running destructive actions.

## ACT/WACT

The header and cards use the same last-touch attribution code path as
`muxa stats`, so `WACT` stays a subset of `ACT` for each session. If the
activity ledger is unavailable, the dashboard still opens and surfaces a note
instead of failing the whole TUI. Press `n` to read those notes.
