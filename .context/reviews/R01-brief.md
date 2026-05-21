# Review Brief (R01) — `tmux::current_pane` fallback

## Scope
- One function in `crates/muxa/src/tmux/mod.rs` (`current_pane`)
- ~20 lines changed (no other files touched)

## Problem being fixed
When `muxa watch` is launched via the recipe shipped in
`examples/muxa.tmux.conf`:

    bind-key s display-popup -E -w 90% -h 85% "muxa watch"

…tmux does NOT propagate `$TMUX_PANE` to the spawned child process.
Empirically verified on tmux 3.4 in `tmux run-shell` context:
`$TMUX_PANE` is empty but `tmux display-message -p '#{pane_id}'`
returns the active pane.

Result: `current_pane()` returned `None`, `App::set_initial_pane(None)`
was a no-op, and the watch TUI's `clamp_selection` fell through to row 0
instead of pre-selecting the user's current pane.

## Fix
`current_pane()` now:
1. Fast-path on `$TMUX_PANE` if set/non-empty.
2. Otherwise, if `inside_tmux()`, shells out to
   `tmux display-message -p '#{pane_id}'` and parses the result.
3. Otherwise returns `None` (bare shell — unchanged behaviour).

## Callers (all benefit from the fix)
- `watch.rs:583` — `App::set_initial_pane` (the user-facing bug)
- `main.rs:382` — `cmd_status_line` fallback when no `--pane` arg
- `main.rs:416` — `cmd_recap` fallback when no `--pane` arg

## Focus areas for review
1. Correctness of the shell-out fallback (output parsing, edge cases).
2. Cost / latency — `current_pane()` is called once per `muxa watch`
   startup and per status-line refresh.
3. Test coverage — function shells out so it's hard to unit test
   without DI; is the absence of a test for the fallback path OK?
4. Backward compat — any caller that relied on `None` meaning "no
   `$TMUX_PANE`" specifically would now get `Some(...)` inside tmux.
