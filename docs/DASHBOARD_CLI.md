# CLI dashboard

`muxa dashboard` is a workspace-card TUI console. A tmux card is one workspace
session and its panes retain their work-window locations.

Use `muxa watch` when you want the compact picker/table. Use
`muxa dashboard` when you want a richer console with cards, an inspector,
live terminal capture, prompt composition, and workspace-level ACT/WACT totals.
When it runs inside a tracked tmux agent pane, it also becomes that agent's
collaboration-room console.

## Open

For ordinary inspection, run `muxa dashboard` anywhere. For collaboration,
focus the sending agent pane and press `prefix+D` instead. `muxa init` installs
that popup binding.

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
| `Tab`, `[` / `]` | Cycle the action target inside the selected workspace card. |
| `PageUp` / `PageDown` | Scroll live capture history. |
| `G` / `End` | Return capture to the latest output. |
| `f` | Toggle capture fullscreen. |
| `n` | Open dashboard notes when diagnostics are available. |
| `Enter` | Toggle the selected workspace inspector. |
| `p` | Open the prompt composer for the selected pane or muxa PTY session. |
| `m` | Compose a structured request for the selected same-room agent. |
| `b` | Open incoming/sent collaboration mailbox history without claiming requests. |
| `i` | Claim pending collaboration requests and open the incoming mailbox. |
| `c` | Copy the selected workspace's latest prompt. |
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

## Collaboration room

The everyday flow is:

1. Put two agents in panes of the same tmux window.
2. Focus the sender and press `prefix+D`.
3. Select the other agent with `Tab`, press `m`, and send the request.

One window is one room. The focused agent is the sender; selecting a card only
changes the recipient. A popup opened from a normal shell can inspect sessions
but cannot send as an agent.

The header and inspector show the current room, the calling agent's alias,
roles on room participants, and unread request/reply counts. Select a peer's
pane with `Tab`, `[` or `]`, then press `m` to send a durable request pinned to
that exact agent session. In the message composer:

- `Tab` cycles `question`, `review`, `task`, and `notice`.
- `Ctrl-E` toggles the explicit `read-only` / `execute` work contract.
- `Enter` sends and `Esc` returns to the dashboard.

Press `b` for non-claiming incoming/sent history. In the mailbox, `Tab` changes
mailbox, arrows select a request, `i` atomically claims pending incoming work,
`e` replies to a claimed request, and `x` confirms cancellation of a sent
request that is still queued. The reply composer uses `Tab` to cycle
`completed`, `blocked`, `declined`, and `failed`.

If the room has no other agent, start one in another pane of the same window.
If collaboration says it is unavailable, close the popup, focus an agent pane,
and press `prefix+D` again. Under the hood muxad validates that pane and keeps
the same-window security boundary used by the CLI and MCP helpers.

## ACT/WACT

The header and cards use the same last-touch attribution code path as
`muxa stats`, so `WACT` stays a subset of `ACT` for each session. If the
activity ledger is unavailable, the dashboard still opens and surfaces a note
instead of failing the whole TUI. Press `n` to read those notes. Automatic
one-second refreshes are silent so action and mailbox hints remain visible;
only an explicit `r` refresh shows `refreshing` / `refreshed` feedback.
