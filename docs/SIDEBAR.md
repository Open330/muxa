# Agent sidebar

`muxa sidebar show` opens a compact agent explorer as a full-height native pane
on the left. It shares watch's topology, event subscription, state merge and pane
ID colors. It never sends agent messages or starts an LLM.

```sh
muxa sidebar show
muxa sidebar show --side right --width 40
muxa sidebar toggle
muxa sidebar status
muxa sidebar hide
```

Width defaults to 36 columns (24–80). At least 20 columns must remain for work.
Existing content panes and their nested split structure are retained. Repeating
show does not create duplicates; changing side/width replaces only the sidebar.

`prefix + e` toggles the explorer and remembers its side/width. An existing
custom `e` binding is preserved when enabling the sidebar. `muxa init` also
installs this shortcut with the watch bindings.

`prefix + s` opens the normal full watch over the tree; closing watch reveals
the same sidebar. Press `s` in the sidebar or click its header for the same
transition, starting watch at the selected/last work pane. This uses the existing
popup and reserves rmux's status row, without rebuilding the agent tree pane.

Use arrows or `hjkl` to navigate, Enter or a mouse click to focus a pane, and `r`
to refresh. Click session/window rows to collapse/expand them; the wheel scrolls.
`◆` marks the current session/window and `▶` the active work pane, independently
of the `>` navigation cursor. Focusing the sidebar retains the last work pane
marker. The sort order comes from `[watch].sort`; changes saved in watch are
picked up within one second without restarting the sidebar. The explorer stays running when focus moves. Cross-session selection
requires exactly one client focused on the sidebar; it carries the sidebar into
the selected session without switching an unrelated client. Plain `z`/`q` in the
explorer show hints; use the commands above to hide it.

Enabling applies to the current session. Newly created/visited windows in that
session acquire one sidebar through guarded hooks. Sidebar policy lives in the
running multiplexer, not config.toml. Sidebars themselves are not agent rows in
the compact explorer. Hide disables the session policy and removes only marked
sidebar panes; the last sidebar-only window becomes a shell rather than closing.

## Keep it visible during zoom

```sh
muxa sidebar zoom
```

When the existing `prefix + z` is tmux's stock zoom binding, show connects it to
sidebar-aware focus. A custom binding is preserved. Direct native `resize-pane
-Z` is converted by a guarded hook as well, so it does not leave the sidebar
hidden.

This is a focus layout, not tmux's one-pane native zoom flag: the sidebar stays
full-height, the selected work pane gets the remaining area, and other panes are
folded into a one-row strip at the bottom. Every pane keeps its ID, process and
window, so collaboration room addresses are unchanged. Repeat zoom to restore
the saved layout. If panes were created/closed during focus, keep the user's
current pane set/layout instead of recreating or deleting anything.

Hooks occupy index 9105 of `after-select-window`, `after-new-window`,
`client-session-changed`, `after-select-pane`, and `after-resize-pane`. They are guarded by sidebar
policy/zoom state and do not run a sidebar CLI in disabled sessions. The ordinary
zoom binding still behaves normally in windows without a sidebar.

Pin an endpoint for scripts or key bindings; `--mux-socket` is the tmux/rmux
server, whereas Muxa's root `--socket` is the daemon:

```sh
muxa sidebar show --host rmux --mux-socket /tmp/rmux-1044/default --pane rmux:%47
```

Real-host regression tests create only shell panes on a private rmux socket:

```sh
cargo test -p muxa-cli --test sidebar -- --ignored
```
