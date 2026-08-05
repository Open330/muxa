# Live TUI

`muxa watch` is the main interactive surface. It shows tracked agents and
plain tmux panes, lets you jump to panes, composes prompts, and exchanges
durable collaboration requests between agents in the same tmux window.

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
| Printable text | Immediately filter by session, agent, cwd, model, or prompt. |
| `/` | Explicitly start filtering, including queries beginning with a reserved key. |
| `Backspace` / `Ctrl-W` / `Ctrl-U` | Delete a character / word / the whole filter. |
| `j` / `k`, `↑` / `↓` | Move between sessions; after entering a child, move between agents. |
| `h` / `l`, `←` / `→` | Return to the parent session / select the first child agent. |
| `gg` / `G`, `Home` / `End` | Jump to the first / last selectable row. |
| `Ctrl-U` / `Ctrl-D`, `PageUp` / `PageDown` | Move half / full pages while browsing. |
| `Enter` | Open prompt composer for the selected pane. Empty `Enter` attaches. |
| `m` | Message the selected same-window agent. |
| `b` | Open incoming/sent mailbox; `i` claims and `e` replies. |
| `o` / `Alt-P` | Open live preview. |
| `:` | Open the command palette; `Tab` completes the first match. |
| `r` / `Ctrl-R` / `Alt-R` | Refresh while browsing. |
| `?` / `F1` / `Alt-?` | Help. |
| `q` / `Ctrl-C` | Quit while browsing / quit globally. |
| `Alt-I` | Toggle the persistent wide-screen inspector. |
| `Alt-E` | Open the completion/error/attention event inbox. |
| `Alt-A` | Toggle attention-only filtering. |
| `[` / `]` | In preview, show the previous / next agent in the selected session. |
| `c` | Toggle preview content. |
| `f` | Toggle popup/fullscreen preview. |
| `Alt-L/D/S/T` | Sort by latest / duration / session / attention state. |

### If `Alt` does nothing on macOS

macOS treats Option as a compose key unless the terminal is told otherwise, so
`Alt-I` arrives as `ˆ` and `Alt-E` as `´` — the keystroke never reaches watch.
Whether you hit this depends on the terminal and the active keyboard layout;
Ghostty, for one, only enables Alt by default on the U.S. Standard and U.S.
International layouts.

- **Ghostty** — `macos-option-as-alt = left` in
  `~/Library/Application Support/com.mitchellh.ghostty/config` (that path
  overrides `~/.config/ghostty/config` on macOS), then `cmd+shift+,` to reload.
  `left` keeps the right Option free for composing characters; `true` claims
  both.
- **iTerm2** — Settings → Profiles → Keys → *Left Option key* → **Esc+**.
- **Terminal.app** — Settings → Profiles → Keyboard → *Use Option as Meta key*.

To check: run `cat -v` and press `Alt-I`. `^[i` means it works, `ˆ` means it
does not.

Every `Alt` binding also has an `Alt`-free equivalent in the command palette
(`:inspector`, `:events`, `:preview`, …), which works regardless of terminal
configuration.

## Filter, Inspector, and Events

Printable characters immediately narrow the table case-insensitively. While
the query is empty, conventional browse keys such as `hjkl`, `q`, `r`, `o`,
and `g` remain commands. After any non-reserved character starts a query,
those keys become ordinary search text. Press `/` first when the query itself
must begin with a reserved key; Backspace may then return to an empty but still
armed filter. `Ctrl-W` deletes a word, while `Ctrl-U` or `Esc` clears the query
and returns to browsing. Search and the `Alt-A` attention-only filter compose.
With no query, `Esc` disables attention-only mode before a subsequent `Esc`
quits.

In session view, the selected session's children appear automatically, but
`↑`/`↓` (and `j`/`k` while browsing) skip them and continue moving between
session rows. Press `→` or `l` to enter child selection; then the same vertical
keys cycle that session's agents, and `←` or `h` returns to the parent. Moving
to another session folds the previous one and opens the new one in its place.
A single-pane session does not add a redundant child row.
The existing `↳ detail` line remains visible for both selected parents and
selected children; process-tree detail shares the same secondary row when
available.

At 120 columns or wider, the selected pane's live capture stays visible in a
right-hand inspector. `Alt-I` toggles it. That 120 is the width `muxa watch`
itself receives, not your terminal's: an inset `display-popup` subtracts its
own inset *and* its border, so a 134-column terminal hands a `-w 90%` popup
only 118. This is why the bundled `prefix + s` binding is borderless and
full-client (`-B -w 100% -h 100%`). Completion, error, and input/choice
transitions remain in a 50-entry in-process inbox opened with `Alt-E`; the
header shows the unread count.

## Command Palette

Press `:` while browsing to open the command palette. Type a command and press
`Enter`; `Tab` completes the first visible match and `Esc` cancels. Available
commands include `refresh`, `preview`, `copy`, `attention`, `events`,
`inspector`, `sort latest|duration|session|state`, `view pane|session|swarm`,
`help`, and `quit`. `kill` and `abort` still open the normal confirmation popup.
Runtime `view` changes use the cached snapshot immediately and remain active
for subsequent refreshes in the current watch process.

## Prompt Composer

Press `Enter` on a pane-bearing row to open the prompt composer. Type the
prompt and press `Enter` to send it to that pane. Press `Esc` to cancel.
If the composer is empty, `Enter` attaches to the pane instead.

Prompt input time is recorded as a human interaction interval in
`activity.ndjson` when activity logging is enabled.

## Agent collaboration

Focus the sending agent pane and open watch with `prefix+s`. Select another
agent in the same tmux window and press `m`. When the room has exactly one
peer, watch selects it automatically. `Tab` changes request kind and `Ctrl-E`
switches between `read-only` and `execute`; `Enter` sends.

- `? QUESTION` (cyan) asks for an answer.
- `◆ REVIEW` (magenta) asks for review findings.
- `▶ TASK` (yellow) delegates a concrete task.
- `! NOTICE` (blue) is informational and does not expect a reply.

`○ READ-ONLY` (green) authorizes investigation and an answer, not changes.
`Ctrl-E` switches to `● EXECUTE` (red), explicitly authorizing commands and
file changes. This is a contract delivered to the receiving agent, not a
command that muxa executes immediately. Watch has no separate path-scope
field, so include the intended edit scope in an execute request's body.

Press `b` for incoming/sent history. In the mailbox, `Tab` switches mailbox,
`j`/`k` selects a request, `i` claims pending incoming work, and `e` replies.
If watch was opened from a normal shell, observation still works and the UI
explains that collaboration requires opening `prefix+s` from an agent pane.

Requests with AIR artifact references carry profile-colored mailbox badges:
blue `AIR WORKFLOW`, magenta `AIR PLAN`, cyan `AIR TRACE`, and light-cyan
`AIR SESSION`. The selected detail shows whether each reference is an input or
reply output, plus its short digest, label, and display-only locator.

## Preview

Press `o` or `Alt-P` to preview the selected pane. In session view, if the selected
session has multiple agent panes, press `]` for the next agent or `[` for the
previous agent. `Tab` and `Shift+Tab` work as aliases. The preview title shows
the current position, such as `2/3`, when more than one agent is available.

## tmux Popup Binding

```tmux
bind-key s display-popup -B -E -w 100% -h 100% -x 0 -y 0 "muxa watch"
bind-key D display-popup -E -w 95% -h 90% "muxa dashboard"
```

`prefix+s` is the normal watch and collaboration entry point. `prefix+D` is an
optional shortcut to the richer Dashboard.

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
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6
```

Available column keys include `pane`, `state`, `state_age`, `kind`, `model`, `ctx`,
`cost`, `limits`, `workload`, `prompt`, `activity`, and `session_time`.
The default `state_age` column renders values such as `▶ WAIT 3m` and
`● WORK 42s`; use `state` when only the compact glyph is wanted.
By default, child shell/subagent work is shown only on the selected row's
detail line as `tree ◇1 ▸1 +2`. Add `workload` to `columns` to render the
always-visible `TREE` column. `◇` means subagent, `▸` means shell, and `+`
means other visible process.

## Sort

```toml
[watch]
sort = ["state", "session", "latest"]
# sort = ["latest"]
# sort = ["session_time"]
# sort = ["state", "latest"]
# sort = ["session", "pane"]
# sort = ["pane_id"]
```

Runtime sort keys mirror these presets and save the selected preset back to
`[watch].sort`. The `--sort` flag remains a one-shot launch override until
you press a runtime sort key. The default floats attention states first,
then groups by tmux session and floats the most recently active agent in each
group. `activity` and `act` remain accepted aliases for `latest`.

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
