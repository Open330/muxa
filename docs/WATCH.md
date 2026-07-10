# Live TUI

`muxa watch` is the main interactive surface. It shows tracked agents and
plain tmux panes, lets you jump to panes, and can compose prompts directly
from the picker.

For the session-card console that keeps you inside the TUI while sending
prompts, aborting turns, and inspecting live captures, use
[`muxa dashboard`](DASHBOARD_CLI.md).

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

## macOS Menu Bar with BarShelf

The bundled [BarShelf](https://github.com/Open330/barshelf) `muxa Watch`
widget provides a compact menu-bar popover view of the same agent state. It
shows the five most recently active agents in the familiar `NAME / ST / ACT /
LAST PROMPT` layout. The widget refreshes every five seconds while the popover
is open and does not poll in the background.

Install it from the BarShelf gallery, or directly with:

```bash
barshelf install https://github.com/Open330/barshelf/tree/master/widgets/muxa-watch
```

The widget requires Deno and a `muxa` version that supports the versioned
snapshot command below. Set `MUXA_BIN` or the widget's custom socket setting
when the defaults do not match your installation.

```bash
muxa status --json
```

## Columns

Columns are configured under `[watch]`:

```toml
[watch]
view = "session"
columns = ["pane", "state", "model", "ctx", "cost", "prompt", "activity"]

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6
```

Available column keys include `pane`, `state`, `kind`, `model`, `ctx`,
`cost`, `limits`, `workload`, `prompt`, `activity`, and `session_time`.
By default, child shell/subagent work is shown only on the selected row's
detail line as `tree ◇1 ▸1 +2`. Add `workload` to `columns` to render the
always-visible `TREE` column. `◇` means subagent, `▸` means shell, and `+`
means other visible process.

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
`activity`, `workload`, `last_prompt`, `last_response`, `last_notification`,
and `cwd`.

When visible workload exists, the selected row uses the detail line for
`tree ...` in the session/name column before falling back to the template.

Long detail content is truncated for the table. Use preview mode for pane
captures or prompt/response text when you need more context.
