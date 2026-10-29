# CLI dashboard

`muxa dashboard` opens `muxa watch` on its **Work board** screen — the same
screen as `Alt-3` / `:screen board` / `muxa watch --screen board`. It reads the
canonical `WorkSnapshot` the web dashboard uses: persisted Work, its external
issue, every Run and the agents in it. See [WATCH.md](WATCH.md#screens) for the
board itself; this page covers the command and what changed when it replaced
the earlier card console.

## Open

```bash
muxa dashboard
muxa dashboard --since 7d        # ACT/WACT window for the summary
muxa dashboard --sort active     # attention (default), activity, active or name
muxa dashboard --include-paneless
muxa dashboard --theme mono
```

`prefix + D` runs `muxa dashboard` in a popup; `muxa init` installs it. Every
other watch screen is one key away (`Alt-1` topology, `Alt-2` collaboration).

The flags apply to that run only. `--since` accepts what `muxa stats --since`
accepts (`today`, `24h`, `7d`, local dates, RFC3339, `all`) and an invalid
value is an error before the screen opens. Inside the board, `:sort` changes
and saves the order (`[watch] board_sort`).

## Keys

| Key | Does |
| --- | --- |
| `j`/`k`, `g`/`G` | Move between Works |
| `Tab` / `]`, `Shift-Tab` / `[` | Move the agent cursor (`▸`) within the Work |
| `Enter` / `o` | Go to / preview the `▸` agent's pane |
| `p` / `P` | Prompt the `▸` agent / every live agent of the Work |
| `R` / `A` | Interrupt the `▸` agent / every live agent (confirmed) |
| `K` | Close the `▸` agent's pane (confirmed) |
| `c` | Copy the `▸` agent's last prompt |
| `m` | Message the `▸` agent (exact agent session) |
| `b` / `M` | That agent's mailbox; inside it `i` claims, `e` replies, `x` withdraws a queued sent request |
| `/` then type | Filter Works (plain letters are actions) |
| `?` | Help |

## Changes from the earlier dashboard

- **ACT/WACT is attributed per pane**, then summed over each Work's panes.
  The old console split it by tmux session name, so every Work in one session
  showed the same figure. Each row has an `ACT` column, the detail shows ACT
  and WACT, and the corner sums all sessions; `--sort active` orders by it.
  `busy` (agents at work) is a further order, from `:sort`.
- **Muxa-owned PTY sessions** are not Work and do not appear; `muxa watch`
  lists them.
- The previous card console has been removed.
