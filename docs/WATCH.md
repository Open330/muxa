# Live TUI

`muxa watch` is the main interactive surface. It shows tracked agents and
plain tmux panes, lets you jump to panes, composes prompts, and exchanges
durable collaboration requests between agents in the same tmux window.

For the workspace-card console that keeps you inside the TUI while sending
prompts, aborting turns, and inspecting live captures, use
[`muxa dashboard`](DASHBOARD_CLI.md).

## Open

```bash
muxa watch
muxa watch --view work
muxa watch --view pane
muxa watch --include-paneless
```

`view = "work"` groups tmux panes by window and labels each parent as
`workspace › work`. This is an execution topology view: a session binds a
Workspace, a window binds the current Run, and each child pane binds an agent
session. `view = "pane"` shows one row per pane. Durable Work and external issue
state belong to `muxa dashboard`, not this topology tree.

The default tree is an accordion: selecting a session reveals its windows and
folds the previously selected session. In pane view, selecting a window also
reveals its panes. In this focus mode, `j`/`k` move between siblings at the
current level, so visible child context does not add extra stops while scanning
sessions. If that level contains only one node, navigation automatically falls
back to the parent's sibling group: a lone pane advances between windows, and
a lone window advances between sessions. Use `l`/Right to descend into a window
or pane and `h`/Left to return one level. Each `l` selects the first child
immediately; an explicitly selected pane keeps its session/window ancestry
visible even when the automatic `view` depth is `"session"` or `"window"`. Set
`[watch].tree_expansion = "always"` to keep every node through the configured
`view` depth visible, or `"manual"` to start collapsed and expand only with
`l`/Right; those two policies retain visible-row traversal. The default is
`"focus"`.

In pane view, an unfiltered session with exactly one real window is rendered as
one `session › window` row, with the panes directly underneath it. The compacted
row keeps session selection semantics (`n`, rename, and close still target the
session), and switching to window view exposes the exact window row for
window-level actions. Sessions with multiple windows keep the complete
hierarchy. Filtering also restores the complete ancestry, so one matching
window is not confused with a genuinely single-window session.

The tree also avoids repeating an identical state marker down a single-child
chain: an expanded parent with one child leaves its state cell empty and the
deepest visible row carries the state. Collapsed parents and nodes with multiple
children keep their aggregate state markers.

## Common Keys

| Key | Action |
| --- | --- |
| `/` | Start filtering — the only way in. Typed characters then narrow by workspace, work, agent, cwd, model, or prompt, and every browse key is an ordinary character while the filter is armed. |
| `Backspace` / `Ctrl-W` / `Ctrl-U` | Delete a character / word / the whole filter. |
| `j` / `k`, `↑` / `↓` | In focus mode, move between siblings at the current hierarchy level; otherwise move visible rows. |
| `h` / `l`, `←` / `→` | Return to the parent / descend into the first child. |
| `gg` / `G`, `Home` / `End` | Jump to the first / last sibling in focus mode, or visible row otherwise. |
| `Ctrl-U` / `Ctrl-D`, `PageUp` / `PageDown` | Move half / full pages while browsing. |
| `Enter` | Attach to the selected pane. |
| `C` | Open a plain shell window in the selected row's session and jump into it — `prefix + c` without attaching first. |
| `n` | Create/reuse a workspace session and work window, then add an agent pane. |
| `w` | Run a pipeline: type a work id and hand off to a window running `muxa work up`. |
| `R` / `:rename` | Rename the selected tmux session or window, or set the selected pane title. |
| `\|` | Cycle the list/inspector split: 50/50 → 70/30 → 30/70. |
| `a` / `A` | Ask the configured agent a headless question / browse the answers. `Enter` reads the selected answer in full. |
| `m` / `M` | Message the resolved agent — or every marked one — / open history for the selected topology scope. |
| `Space` | Mark or unmark the agent under the cursor. With marks set, `m` composes once and sends one ordinary request to each; the result popup keeps a row per recipient. |
| `b` | Legacy alias for `M`; `i` claims and `e` replies inside the mailbox. |
| `v` | On the collaboration screen, toggle table / chronological sequence. |
| `o` / `Alt-P` | Open live preview. |
| `:` | Open the command palette; `Tab` completes the first match. |
| `r` / `Ctrl-R` / `Alt-R` | Refresh while browsing. |
| `?` / `F1` / `Alt-?` | Help. |
| `q` / `Ctrl-C` | Quit while browsing / quit globally. |
| `Alt-I` | Toggle the persistent wide-screen inspector. |
| `Alt-E` | Open the completion/error/attention event inbox. |
| `Alt-A` | Toggle attention-only filtering. |
| `[` / `]` | In preview, show the previous / next agent in the selected work. |
| `c` | In preview, toggle content. |
| `f` | Toggle popup/fullscreen preview. |
| `Alt-L/D/S/T` | Sort by latest / workspace duration / workspace / attention state. |

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

In work view, the selected work window's child agents appear automatically, but
`↑`/`↓` (and `j`/`k` while browsing) skip them and continue moving between
work rows. Press `→` or `l` to enter child selection; then the same vertical
keys cycle that work's agents, and `←` or `h` returns to the parent. Moving
to another work folds the previous one and opens the new one in its place.
A single-pane work does not add a redundant child row.
The existing `↳ detail` line remains visible for both selected parents and
selected children; process-tree detail shares the same secondary row when
available.

The canonical session/window/pane tree uses the same principle in focus mode:
expanded descendants provide context, while vertical keys stay in the current
sibling group. For example, one `j` moves directly from a selected session to
the next session even though the first session's windows are visible. `l` then
enters that session's first window; window-level `j`/`k` stay among its windows,
and pane-level `j`/`k` stay within the selected window. At a hierarchy level
with only one sibling, the movement bubbles upward until it finds a real
sibling group, preventing a single window or pane from becoming a dead end.
To act on that singleton pane itself, press `l` while its window is selected;
the pane becomes the selected row immediately and stays visible until movement
leaves it or `h` returns to the window.

At 120 columns or wider, the selected hierarchy node stays visible in a
right-hand inspector. A session selection rolls up scope, client presence,
cumulative attached time, the most urgent attention item, latest activity, and
compact window rows with their pane children nested underneath. The available
Inspector height is filled in topology order, with any remainder summarized as
`+N more`.

A window selection adds process/shell/subagent load, peak context, total cost,
and collaboration mailbox state above a live miniature of the selected tmux
window. The miniature maps `pane_left`, `pane_top`, `pane_width`, and
`pane_height` into the Inspector canvas, so horizontal and vertical splits
keep their real proportions. Each cell shows the pane's captured terminal,
with a double border for the active pane and a state-colored border when an
agent needs attention. Captures refresh at most once per second; a zoomed
window shows only its active pane. Geometry and pane reads happen in a bounded
background capture, so changing selection and typing remain responsive; ANSI
is parsed once per snapshot rather than once per redraw. Small Inspector areas
and backends without tmux-compatible geometry fall back to the responsive pane
roster. Selecting an individual pane still opens its denser metadata and full
single-pane capture.

`Alt-I` toggles the inspector, and `|` cycles 50/50, 70/30, and 30/70
list/inspector splits. That 120 is the width `muxa watch` itself receives, not
your terminal's: an inset `display-popup` subtracts its own inset *and* its
border, so a 134-column terminal hands a `-w 90%` popup only 118. This is why
the bundled `prefix + s` binding is borderless and full-client
(`-B -w 100% -h 100%`). Completion, error, and input/choice transitions remain
in a 50-entry in-process inbox opened with `Alt-E`; the header shows the unread
count.

## Command Palette

Press `:` while browsing to open the command palette. Type a command and press
`Enter`; `Tab` completes the first visible match and `Esc` cancels. Available
commands include `refresh`, `preview`, `copy`, `attention`, `events`,
`inspector`, `sort latest|duration|session|state`, `view pane|session|swarm`,
`layout tree|swarm|work`, `layout table|sequence`,
`screen topology|collab`, `help`, and `quit`. The table/sequence commands only
change the collaboration screen; topology keeps its own layout. `kill` and
`abort` still open the normal confirmation popup.
Runtime `view` changes use the cached snapshot immediately and remain active
for subsequent refreshes in the current watch process.

## The work layout

`W` swaps the tree for a flat table of Works — one row each, mirroring
`muxa work list` column for column (WORK, WORKSPACE, GEN, ALIASES, DONE, CWD)
and adding the live state gauge a CLI table cannot show. `W` again returns to
whatever layout it interrupted, so a swarm is not silently discarded; the
palette's `layout work` and `--layout work` reach it too, as does
`[watch] layout = "work"`.

The rows *are* the window nodes, so attach, preview, the composer and kill all
address a Work exactly as they do in the tree, and the cursor survives a layout
switch without translation. Only the session and pane levels fold away: the
workspace is a column here, and a Work's panes are summarised by their alias
statuses. Movement is over the whole list rather than the tree's sibling group,
since a flat table shows no ancestry to move within.

## Screens

`muxa watch` shows one of two lists, chosen with `Alt-1` / `Alt-2`, the
`screen topology|collab` palette command, `--screen`, or `[watch] screen`.

`topology` is the session → window → pane tree, and everything `view` and
`layout` describe applies to it. `collab` lists collaboration *requests*
instead — every room the daemon holds, not just the one under the cursor,
which is what `M` shows. That difference in what a row *is* is why this is a
screen rather than another `layout`.

The `view` axis carries over: `session` groups the listing by tmux session,
`window` by room, and `pane` leaves it flat. A request is filed under the room
it was raised in, so a listing at any grouping says where the work happened.
Typing filters on either end's alias, its `session:window`, the message, the
kind, and the status. `Enter` attaches to the peer's pane — requests outlive
panes, so a request whose peer is gone says so rather than doing nothing.

A row can only hold an excerpt, so the selected request also opens in a detail
pane under the table: both ends with their rooms, the kind and delivery mode it
was sent under, the body in full, and the reply when one has come back. The
pane costs seven rows, so terminals shorter than sixteen rows keep the listing
instead and leave the body to `M`.

The default `table` layout is newest-first. Press `v`, run `:layout sequence`,
start with `--collab-layout sequence`, or set
`[watch] collab_layout = "sequence"` to draw the same filtered history as
chronological participant lifelines. Requests point from sender to recipient;
replies return on a dashed reverse arrow. This is a presentation change only:
filtering, selection, attach, and durable history are shared with the table.

Widening past your own mailbox is an operator-console operation, so the screen
needs a daemon built from the same tree as the CLI; an older one is reported
rather than silently answered with a caller-scoped listing.

## Ask

`a` composes a headless question for the agent named in the composer title;
`Tab` switches between claude and codex, `Ctrl-V` pastes, `/` opens the shared
skill palette at any point in the draft, and `Enter` sends.
muxad runs the agent in print mode and captures the answer, so nothing is
typed into a pane and no session has to be managed. `Esc` cancels; Backspace
also cancels when the input is already empty.

`A` opens the history: `j`/`k` selects, `Enter` opens the selected answer
in a full reader, `|` grows the detail pane, `Tab` filters by agent
(all → claude → codex), and `n` starts a fresh conversation. Everything before that `n` is one thread — each question
resumes the last, so the second onward reuses the cached context the first
paid for. Threads are per agent, so switching back picks that conversation
up where it left off.

The daemon owns execution: an answer lands in the history whether or not
the popup is still open, and the history outlives restarts in
`$XDG_DATA_HOME/muxa/ask.json`. Requires `[ask] enabled = true` — see
[CONFIGURATION.md](CONFIGURATION.md).

The detail pane under the list only ever shows an answer's first screenful.
`Enter` (or `o`) opens the selected entry in a reader that shows the question
and the whole answer: `j`/`k` scroll a line, `PgUp`/`PgDn` a page, `g`/`G`
jump to the top/bottom, and the foot of the box tracks which lines are on
screen. `Esc`, `Enter`, or `q` returns to the list; `A` closes the panel. The
reader is read-only — `d` and `D` do not delete the answer being read — and it
follows the entry itself, so an answer that arrives while it is open appears
without reopening it.

Inside ask history, `n` starts a fresh conversation without deleting entries.
`d` confirms deletion of the selected completed entry. `D` confirms clearing
completed history across all agent filters. Running asks and conversation ids
are preserved by both operations.

Ask defaults to `[ask].permission_mode = "bypass"` because headless sessions
cannot answer approval prompts and ask is designed to run unattended skills.
This allows file edits, commands, and publishing without confirmation, so only
send trusted prompts. Select `edit` or `default` to restore stricter agent
controls. Symlink targets outside the configured `cwd` must also be listed in
`[ask].additional_dirs`.

An inserted skill is only question text. It never changes the selected agent,
`permission_mode`, cwd, additional directories, or timeout; those remain the
daemon-owned `[ask]` contract.

## Agent collaboration

Open watch with `prefix+s` from anywhere, select an agent, and press `m`. Watch
sends as the **operator console** — you are the sender, not whichever agent
occupies the pane you opened the popup from — so the launch pane is an ordinary
recipient like every other row, and a bare shell is a perfectly good place to
open watch from. When the room has exactly one peer, watch selects it
automatically. `Tab` changes request kind; `Enter` sends; `Esc` cancels.
Backspace also cancels when the input is already empty. The last kind and mode
are saved immediately and restored by the next `m`, including after watch
restarts.

From any point in an `m` composer, `/` opens the reusable message-skill palette.
Typing filters names and prompt text, arrows or `Tab` move the selection, and
`Enter` inserts the selected template at the current cursor without replacing
existing text. Adjacent content is separated as paragraphs, and more skills
can be inserted into the same draft. Insertion never sends: edit or verify the
expanded body, then press `Enter` again. Register templates with
`muxa skill add <name> <prompt>` or `[message.skills]` in config. Inside either
watch palette, `F2` opens the add/update form and `Delete` confirms removal of
the selected skill. `Ctrl-A` and `Ctrl-D` remain compatibility aliases.

Skills are outside neither request kind nor send mode: they contain text only.
The kind badge selected with `Tab` and the mode selected with `Ctrl-E` remain
unchanged when a skill is inserted and are applied only when the expanded text
is explicitly sent with the second `Enter`.

A console has no pane of its own, so replies are not routed back to it: they
stay on the request in the recipient's mailbox. `M` follows the topology level
under the cursor. On a pane, `incoming` is that agent's mailbox and `sent` is
the console's dispatch log across every target; `i` (claim), `e` (reply), and
`m` remain available because watch can act as that recipient. On a window,
`M` combines the whole room. On a session, it combines all of its rooms and
groups the rows by window. Window/session history is deliberately read-only:
select a pane before claiming, replying, or composing a new request.

With `[collaboration].scope = "host"`, the selected session, window, or pane
takes precedence over the launch window's only peer. Parent nodes are directly
messageable without descending with `l`: a window chooses its lowest-index
live tracked agent, while a session chooses by lowest numeric window index and
then pane index. The composer title shows the exact resolved agent and pane
before send. For a request such as
“create a new pane, start codex with the `cx` alias, then review our changes”,
choose `TASK` and `EXECUTE`; the receiving agent gets an explicit executable
work contract rather than raw keystrokes.

`Ctrl-E` cycles what leaves the composer: `read-only` and `execute` are the
request contract, and `just send` types the text into the pane as raw
keystrokes — no request, no reply, no contract — which is also how you send a
plain prompt now that `Enter` attaches directly.

- `? QUESTION` (cyan) asks for an answer.
- `◆ REVIEW` (magenta) asks for review findings.
- `▶ TASK` (yellow) delegates a concrete task.
- `! NOTICE` (blue) is informational and does not expect a reply.

`○ READ-ONLY` (green) authorizes investigation and an answer, not changes.
`Ctrl-E` switches to `● EXECUTE` (red), explicitly authorizing commands and
file changes. This is a contract delivered to the receiving agent, not a
command that muxa executes immediately. Watch has no separate path-scope
field, so include the intended edit scope in an execute request's body.

Press `M` for history (`b` remains an alias). In a pane mailbox, `m` opens a
new message and `M` closes the mailbox. `Tab` switches mailbox, `j`/`k`
selects a request, `i` claims pending incoming work, and `e` replies. Aggregate
window/session history has one combined stream, so `Tab`, `m`, `i`, and `e`
are disabled there. A watch opened from a normal shell still collaborates as
the operator console; it never borrows an agent identity from the launch pane.

Requests with AIR artifact references carry profile-colored mailbox badges:
blue `AIR WORKFLOW`, magenta `AIR PLAN`, cyan `AIR TRACE`, and light-cyan
`AIR SESSION`. The selected detail shows whether each reference is an input or
reply output, plus its short digest, label, and display-only locator.

## Preview

Press `o` or `Alt-P` to preview the selected pane. In work view, if the selected
work window has multiple agent panes, press `]` for the next agent or `[` for the
previous agent. `Tab` and `Shift+Tab` work as aliases. The preview title shows
the current position, such as `2/3`, when more than one agent is available.

## tmux Popup Binding

```tmux
bind-key s display-popup -B -E -w 100% -h 100% -x 0 -y 0 "muxa watch"
bind-key D display-popup -E -w 95% -h 90% "muxa dashboard"
```

`prefix+s` is the normal watch and collaboration entry point. `prefix+D` is an
optional shortcut to the richer Dashboard.

## Stable tmux Names

`muxa init` enables the `tmux-window-names` component by default. It turns off
tmux's process-based `automatic-rename`, so a Work window keeps its useful name
instead of becoming `node` or `claude`, and routes the familiar `prefix + ,`
prompt through `muxa window rename`. Window whitespace is normalized to `-`
and duplicate names in one session are refused. Restore dynamic naming for one
window with `muxa window rename --auto`.

## Two Terminals on One Workspace

One tmux session has one current window, and every client attached to it shows
that window. Two terminals attached to the same session therefore cannot sit on
two different Work windows — switching one switches the other. That is tmux's
model, not a muxa limitation, and a *session group* is the only thing that
changes it: the window list stays shared, but each session in the group keeps
its own current window.

`muxa init` installs this as the `tmux-auto-view` component, on by default. It
sets two hooks that hand an arriving client its own view:

```tmux
set-hook -g 'client-attached[9000]' "if -F '#{&&:#{>:#{session_attached},1},#{==:#{@no_auto_view},}}' 'run-shell \"muxa workspace view --client #{client_name}\"'"
set-hook -g 'client-session-changed[9000]' "…same…"
```

The dedicated hook-array slot keeps reloading idempotent and leaves unrelated
user hooks in their own slots intact.

**Both hooks are needed.** `client-attached` covers a terminal running `tmux
attach`. `client-session-changed` covers `switch-client` — which is what
watch's `Enter` does, and what a terminal that was already open when the
component was installed goes through. Measured on tmux 3.4: with only the
attach hook, jumping from watch put both terminals back into one session and
they followed each other from there.

Nothing runs for a lone terminal. `muxa workspace view` is a no-op when its
client is the session's only one, so a single-terminal workspace never grows a
second session, and a terminal that regroups reuses its own view rather than
leaving a trail of sessions behind every jump.

You can also run it by hand — on a terminal that was already attached, for
instance:

```sh
muxa workspace view
```

The view is named `<session>~view~<pid>`, so it sorts beside the session it
mirrors. That is safe despite tmux matching session names by prefix, because an
exact match wins: with `callabo`, `callabo-set` and `callabo~view~1734560` all
present, `-t callabo` resolves to `callabo`. It disappears on detach, so views
never pile up.

Pair it with per-window sizing, so a window is sized to the terminals actually
looking at it rather than to the smallest client anywhere on the session:

```tmux
set -g window-size smallest
setw -g aggressive-resize on
```

Both lines are required, and `aggressive-resize` alone does nothing — it only
applies to windows whose `window-size` is `smallest` or `largest`, and tmux 3.x
defaults to `latest`. Measured on tmux 3.4 with a 200x50 and an 80x24 client,
each on its own window:

| setting | window the 200x50 client is on | window the 80x24 client is on |
| --- | --- | --- |
| `window-size latest` (default) | 80x23 | 80x23 |
| `smallest` + `aggressive-resize on` | **200x49** | 80x23 |
| `smallest` + `aggressive-resize off` | 80x23 | 80x23 |

`smallest` on its own is the classic footgun — any small client anywhere
shrinks your window. `aggressive-resize` narrows "any client" to "a client
whose current window this is", which is exactly the separation the views
create. With one terminal attached, smallest is that terminal, so nothing
changes for ordinary single-terminal use.

Jumping from watch (`Enter`) addresses the target window as
`<session_id>:<window_id>`, so it moves only the terminal that asked and leaves
the grouped sibling on the window it was showing.

To mirror instead — two terminals deliberately showing the same thing, for
pairing or screen sharing:

```sh
tmux set-option -t <session> @no_auto_view 1
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
view = "work"
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6
```

Available column keys include `pane`, `state`, `state_age`, `kind`, `model`, `ctx`,
`cost`, `limits`, `workload`, `prompt`, `activity`, and `workspace_time`.
The default `state_age` column renders values such as `▶ WAIT 3m` and
`● WORK 42s`; use `state` when only the compact glyph is wanted.
By default, child shell/subagent work is shown only on the selected row's
detail line as `tree ◇1 ▸1 +2`. Add `workload` to `columns` to render the
always-visible `TREE` column. `◇` means subagent, `▸` means shell, and `+`
means other visible process.

## Sort

```toml
[watch]
sort = ["state", "workspace", "latest"]
# sort = ["latest"]
# sort = ["workspace_time"]
# sort = ["state", "latest"]
# sort = ["workspace", "pane"]
# sort = ["pane_id"]
```

Runtime sort keys mirror these presets and save the selected preset back to
`[watch].sort`. The `--sort` flag remains a one-shot launch override until
you press a runtime sort key. The default floats attention states first,
then groups by workspace and floats the most recently active work in each
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
