# Configuration

`muxad` reads `$XDG_CONFIG_HOME/muxa/config.toml` when present. Start from
`config.example.toml` for a full annotated file.

## Socket

```toml
socket = "/tmp/muxa.sock"
```

The CLI also honors `MUXA_SOCKET`. tmux environments are healed on daemon
startup so existing panes can reach the current socket.

## History

```toml
[history]
enabled = true
path = "$XDG_DATA_HOME/muxa/prompts.ndjson"
max_per_pane = 50
max_age_days = 30
```

Prompt history is retained, not an unbounded warehouse. `muxa recap` and
prompt totals in `muxa stats` use this retained window.

## Activity

```toml
[activity]
enabled = true
path = "$XDG_DATA_HOME/muxa/activity.ndjson"
max_age_days = 30
```

The activity ledger stores agent state intervals, tmux foreground
intervals, and muxa human interaction intervals. See
[ACTIVITY.md](ACTIVITY.md).

## Session Activity

```toml
[session_activity]
enabled = true
path = "$XDG_DATA_HOME/muxa/session-activity.json"
interval_secs = 5
```

This tmux foreground sampler remains as a compatibility source and helps
stats while newer activity ledger intervals are accumulating.

## Ask

```toml
[ask]
enabled = true
agent = "claude"     # claude | codex
cwd = "~"            # where the headless process runs; defaults to $HOME
permission_mode = "bypass" # bypass (default) | edit | default
additional_dirs = [] # extra real paths, e.g. ["/nfs/home/june"]
timeout_secs = 1800    # 30-minute wall-clock limit
keep = 200           # answers retained before the oldest are dropped
```

Opt-in headless questions from `muxa watch`: `a` composes one, `A` browses
the answers. muxad runs the agent in print mode and captures the reply, so
there is no session to manage and completion is an exit code rather than a
guess. Each agent keeps its own conversation and every question after the
first resumes it, reusing the cached context the first one paid for; `n` in
the panel starts a fresh thread. `path` defaults to
`$XDG_DATA_HOME/muxa/ask.json` and holds both the history and the per-agent
thread ids. Off by default because enabling it lets the daemon spawn a CLI
that bills your account. See [WATCH.md](WATCH.md).

`permission_mode = "bypass"` is the default because the headless agent cannot
answer approval prompts. It is intended for unattended workflows such as a
full issue resolver and disables approvals/sandboxing; use ask only for prompts
and directories you trust. `edit` enables workspace edits while retaining the
agent's sandbox or automated review layer, and `default` preserves the agent
CLI's normal permissions. `additional_dirs` is also passed to the agent CLI.
Add the resolved target when files under `cwd` are symlinks outside it—for
example `["/nfs/home/june"]` when `/home/june/workspace` points there.
`timeout_secs` defaults to 30 minutes so skills have time to prepare a
persistent worker. Reaching it terminates the headless agent process; it is a
wall-clock safety limit, not an inactivity detector.

## Collaboration

```toml
[collaboration]
enabled = true
wake = "idle_only" # idle_only | never
scope = "window"   # window | host
max_message_bytes = 16384
```

Opt-in durable request/reply between agents in the same stable tmux window.
The optional `path` defaults to `$XDG_DATA_HOME/muxa/collaboration.json` and
stores both mailbox state and exact-session aliases/roles.
`idle_only` injects short request/reply notifications only at a
hook-authoritative top-level Idle prompt; message bodies stay in the mailbox.
`scope = "host"` lets watch address the selected tracked agent in another
tmux window or session by its exact pane id.
See [COLLABORATION.md](COLLABORATION.md).

## Watch

```toml
[watch]
view = "work"
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]
sort = ["state", "workspace", "latest"]
hide_paneless = true
collaboration_kind = "question"   # question | review | task | notice
collaboration_mode = "read_only"  # read_only | execute | just_send

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6

[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

Watch rewrites `collaboration_kind` and `collaboration_mode` when `Tab` or
`Ctrl-E` changes the `m` composer badges. The last selection therefore
survives both closing the composer and restarting watch.

See [WATCH.md](WATCH.md) for TUI behavior, columns, sort, and keybindings.

## UI

```toml
[ui]
theme = "classic"
icons = "unicode"
```

Shared visual defaults for human-facing terminal output (`status`,
`status-line`, `attend`, and `watch`).

- `theme` — visual preset: `classic`, `oh-my-muxa`, `focus`, `ops`, or a
  monochrome preset. A one-shot `--theme` flag overrides it per run.
- `icons` — agent-state glyph set:
  - `unicode` (default) — Geometric Shapes glyphs (`●` working, `▶` input,
    `◆` choice, `■` error, `○` idle, `◌` starting, `×` stopped).
  - `ascii` — single-character fallbacks (`*` working, `>` input, `?`
    choice, `!` error, `o` idle, `~` starting, `x` stopped) for terminals
    whose font lacks the Unicode glyphs or substitutes a mismatched-size
    fallback font for them.

## Discovery

```toml
[discovery]
enabled = true
interval_secs = 30
```

Discovery scans tmux panes for known agent CLIs (`claude` / `codex` /
`gemini`) and backfills the registry without waiting for a hook to fire. It
runs once at daemon startup and then every `interval_secs`, so a fresh agent
session in a new tmux session shows up in `muxa status` within that window
instead of only after its first hook. Set `interval_secs = 0` to keep the
legacy run-once-at-startup behavior; `enabled = false` turns discovery off
entirely. The rescan reuses the same `tmux list-panes` the reconciler
already runs, so the cost is negligible.

## Reconciler

```toml
[reconciler]
enabled = true
interval_secs = 30
stuck_working_timeout_secs = 0
stuck_waiting_timeout_secs = 0
```

The reconciler keeps stale states from staying misleading forever. Timeout
values of `0` disable that timeout. The same loop also runs the pid-liveness
sweep that flips registered background tasks (see `muxa register`) to
`stopped` once their process exits.

## Dashboard

```toml
[dashboard]
enabled = false
bind = "127.0.0.1:7878"
auth = "token"
token = ""
allow_public = false
```

The dashboard is loopback-only unless public binding is explicitly
allowed. Use `auth = "public_read"` with a token to expose anonymous reads
while keeping browser control actions PAT-gated. `auth = "none"` exposes reads
and disables control actions entirely. See [DASHBOARD.md](DASHBOARD.md).

## External Sinks

Sinks are opt-in fan-out targets. The current documented sink forwards
prompts to oh-my-prompt. See [SINKS.md](SINKS.md).

## Pane host selection

`MUXA_HOST=tmux|rmux|herdr|zellij` pins a single host. `MUXA_HOSTS` accepts an
ordered comma-separated set, for example `MUXA_HOSTS=rmux,tmux`. rmux's native
`RMUX` variables take precedence over the `TMUX` compatibility variables it
also exports. See [RMUX.md](RMUX.md), [HERDR.md](HERDR.md), and
[ZELLIJ.md](ZELLIJ.md).
