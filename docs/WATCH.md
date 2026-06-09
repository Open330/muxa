# Live TUI

`muxa watch` is the main interactive surface. It shows tracked agents and
plain tmux panes, lets you jump to panes, and can compose prompts directly
from the picker.

## Open

```bash
muxa watch
muxa watch --view session
muxa watch --view pane
muxa watch --include-paneless
```

`view = "session"` groups panes by tmux session. `view = "pane"` shows one
row per pane.

## Common Keys

| Key | Action |
| --- | --- |
| `Enter` | Open prompt composer for the selected pane. Empty `Enter` attaches. |
| `Esc` / `q` | Quit or close the active popup. |
| `p` | Open live preview. |
| `[` / `]` | In preview, show the previous / next agent in the selected session. |
| `c` | Toggle preview content. |
| `f` | Toggle popup/fullscreen preview. |
| `?` | Help. |
| `l` / `a` | Sort by latest activity. |
| `d` | Sort by session duration. |
| `s` | Sort by session grouping. |
| `t` | Sort attention states first. |
| `r` | Refresh. |

## Prompt Composer

Press `Enter` on a pane-bearing row to open the prompt composer. Type the
prompt and press `Enter` to send it to that pane. Press `Esc` to cancel.
If the composer is empty, `Enter` attaches to the pane instead.

Prompt input time is recorded as a human interaction interval in
`activity.ndjson` when activity logging is enabled.

## Preview

Press `p` to preview the selected pane. In session view, if the selected
session has multiple agent panes, press `]` for the next agent or `[` for the
previous agent. `Tab` and `Shift+Tab` work as aliases. The preview title shows
the current position, such as `2/3`, when more than one agent is available.

## tmux Popup Binding

```tmux
bind-key s display-popup -E -w 90% -h 80% "muxa watch"
```

## Columns

Columns are configured under `[watch]`:

```toml
[watch]
view = "session"
columns = ["pane", "state", "model", "ctx", "cost", "prompt", "activity"]

[watch.widths]
prompt = "min:20"
activity = 5
```

Available column keys include `pane`, `pane_id`, `state`, `kind`, `model`,
`ctx`, `cost`, `limits`, `prompt`, `activity`, and `session_time`.

## Sort

```toml
[watch]
sort = ["session", "latest"]
# sort = ["latest"]
# sort = ["session_time"]
# sort = ["state", "latest"]
# sort = ["session", "pane"]
# sort = ["pane_id"]
```

Runtime sort keys mirror these presets and save the selected preset back to
`[watch].sort`. The `--sort` flag remains a one-shot launch override until
you press a runtime sort key. The default groups by tmux session and floats
the most recently active agent in each group. `activity` and `act` remain
accepted aliases for `latest`.

## Detail Row

```toml
[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

Available variables include `pane`, `kind`, `state`, `model`, `ctx`, `cost`,
`activity`, `last_prompt`, `last_response`, `last_notification`, and `cwd`.

Long detail content is truncated for the table. Use preview mode for pane
captures or prompt/response text when you need more context.
