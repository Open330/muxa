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
sort = ["session", "activity"]
hide_paneless = true

[watch.widths]
prompt = "min:20"
activity = 5

[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

See [WATCH.md](WATCH.md) for TUI behavior, columns, sort, and keybindings.

## Reconciler

```toml
[reconciler]
enabled = true
interval_secs = 30
stuck_working_timeout_secs = 0
stuck_waiting_timeout_secs = 0
```

The reconciler keeps stale states from staying misleading forever. Timeout
values of `0` disable that timeout.

## Dashboard

```toml
[dashboard]
enabled = false
bind = "127.0.0.1:7878"
token = ""
allow_public = false
```

The dashboard is loopback-only unless public binding is explicitly
allowed. See [DASHBOARD.md](DASHBOARD.md).

## External Sinks

Sinks are opt-in fan-out targets. The current documented sink forwards
prompts to oh-my-prompt. See [SINKS.md](SINKS.md).

## Zellij

`MUXA_HOST=tmux|zellij` can pin host selection. tmux is the full backend;
zellij support is still planned beyond the CLI baseline. See
[ZELLIJ.md](ZELLIJ.md).
