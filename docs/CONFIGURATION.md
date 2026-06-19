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

## Watch

```toml
[watch]
view = "session"
columns = ["pane", "state", "model", "ctx", "cost", "prompt", "activity"]
sort = ["session", "latest"]
hide_paneless = true

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6

[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

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
allowed. Public API without a token requires the additional explicit
`auth = "none"` opt-in. See [DASHBOARD.md](DASHBOARD.md).

## External Sinks

Sinks are opt-in fan-out targets. The current documented sink forwards
prompts to oh-my-prompt. See [SINKS.md](SINKS.md).

## Zellij

`MUXA_HOST=tmux|zellij` can pin host selection. tmux is the full backend;
zellij support is still planned beyond the CLI baseline. See
[ZELLIJ.md](ZELLIJ.md).
