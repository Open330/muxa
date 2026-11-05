# Zellij backend (planned)

This document captures the design plan for adding a [zellij](https://zellij.dev)
backend alongside the existing tmux backend. **Not yet implemented** — this is
a work item parked for later. Refer back here before starting; update in place
as decisions change.

## Why

muxa is structurally a daemon + CLI that observes per-pane agent state. tmux
is one possible host; nothing in the event ingest, state machine, sinks, or
watch UI is tmux-specific. A zellij backend brings muxa to users who prefer
zellij's layout model and plugin system without forking the project.

## Current tmux coupling

The tmux surface muxa actually uses is small (~600 LOC, all under
`crates/muxa/src/tmux/`):

| Concern                   | Today                                                                 |
| ------------------------- | --------------------------------------------------------------------- |
| Pane enumeration          | `tmux -S <sock> list-panes -a -F '#{pane_id} #{session_name} ...'`    |
| Foreground command per pane | `#{pane_current_command}` from the same format string               |
| Multi-server discovery    | Filesystem walk over `$TMUX_TMPDIR`, `/tmp/tmux-$UID/`, etc.          |
| Pane focus / attach       | `select-window` + `select-pane`, then `switch-client` or `attach-session` |
| Hook context              | Reads `$TMUX_PANE`, `$TMUX` from the agent's environment              |

Outside that module, tmux is invisible. Sinks, watch TUI, recap, daemon IPC,
and agent stdin hooks all operate on `PaneId` strings and don't care where
those IDs come from.

## Zellij capability gaps

Zellij does not expose pane metadata at parity with tmux's `list-panes -F`:

| Capability                    | tmux                          | zellij                                                      |
| ----------------------------- | ----------------------------- | ----------------------------------------------------------- |
| List panes with metadata      | `list-panes -F` (CLI)         | **Plugin API only** (`PaneInfo` via `PaneUpdate` event)     |
| Foreground command per pane   | `#{pane_current_command}`     | `PaneInfo.terminal_command` — plugin-only                   |
| Focus a pane by ID            | `select-pane -t %N`           | `zellij action focus-pane-with-id <id>` ✅                  |
| Screen capture                | `capture-pane -p`             | `zellij action dump-screen` ✅                              |
| Multi-server enumeration      | Required (socket scan)        | **Not needed** — single server per machine                  |
| Real-time pane events         | (unused today; control mode)  | `PaneUpdate` / `TabUpdate` push from plugin host            |

The blocker is foreground-command detection. Without it, agent auto-discovery
collapses to "trust whatever pane the stdin hook self-reports," and panes
without a hook installed are invisible. That's a regression we should not ship.

## Approach: trait extraction + WASM plugin

### Step 1 — Extract `PaneBackend` trait

Refactor `crates/muxa/src/tmux/` into `crates/muxa/src/backend/` with a trait
roughly shaped as:

```rust
pub trait PaneBackend: Send + Sync {
    fn list_panes(&self) -> Result<Vec<PaneSnapshot>>;
    fn resolve_pane(&self, id: &PaneId) -> Result<Option<PaneSnapshot>>;
    fn focus_pane(&self, id: &PaneId) -> Result<()>;
    fn detect_host_env() -> Option<HostKind>; // reads $TMUX / $ZELLIJ
}
```

`PaneSnapshot` is the existing record (id, session, window, pane index, tty,
current command, title). The tmux impl moves under `backend/tmux.rs` mostly
unchanged. This is a non-feature refactor that's worth doing on its own merits
— it also unblocks a future control-mode (`-C`) tmux backend.

### Step 2 — `muxa-zellij-plugin`

A small Rust → WASM plugin (target `wasm32-wasip1`) that:

1. Subscribes to `PaneUpdate` and `TabUpdate` via the zellij plugin API.
2. On every update, serializes the relevant `PaneInfo` fields and pushes them
   to `$XDG_RUNTIME_DIR/muxa.sock` over a new daemon command (e.g.
   `BackendPaneSnapshot { panes: Vec<PaneSnapshot> }`).
3. Holds no state — the daemon is the source of truth. Plugin is a pure
   adapter.

Lives in a new workspace crate, builds to a `.wasm` artifact shipped with
releases. User installs via `zellij plugin --configuration ... load <path>` or
adds it to their layout file.

### Step 3 — Zellij CLI backend

`backend/zellij.rs` implements `PaneBackend` by:

- Maintaining the latest pane snapshot pushed by the plugin (cached in the
  daemon, not re-queried on every call).
- Calling `zellij action focus-pane-with-id` for `focus_pane`.
- Reading `$ZELLIJ` / `$ZELLIJ_PANE_ID` for `detect_host_env`.

Zellij has no multi-server enumeration, so the scanner module collapses to a
no-op for this backend.

### Step 4 — CLI wiring

`muxa status`, `muxa watch`, `muxa hook <agent>` pick the backend through
[`backend::detect_host_env`]. The resolution order is:

1. `MUXA_HOST=tmux|zellij` — explicit operator override; wins regardless
   of which host env vars are present. Useful for nested-multiplexer
   setups where `tmux new-session` from inside zellij leaves `ZELLIJ`
   set in the new shell's env even though the actual host is tmux.
2. `ZELLIJ` set → zellij.
3. `TMUX` set → tmux.
4. Neither set → no-op backend (operator running outside any
   multiplexer; CLI commands that don't need a backend keep working).

When both `$TMUX` and `$ZELLIJ` are present, zellij wins by default —
this is a pragmatic pick rather than a principled one (the env doesn't
carry "set last" ordering). `MUXA_HOST` is the escape hatch.

## Capability story (CLI-only zellij)

Several `PaneBackend` methods are **plugin-only on zellij** because the
zellij CLI doesn't expose pane metadata at parity with tmux's
`list-panes -F`. The trait surfaces this through [`BackendCaps`] so
callers can branch on what's actually available rather than silently
degrading on empty results:

| Capability             | tmux backend | zellij CLI-only | zellij + plugin |
| ---------------------- | ------------ | --------------- | --------------- |
| `list_panes` (any rows) | ✅           | ⚠️ via `list-clients` only | ✅       |
| `current_command` field | ✅           | ❌ never        | ✅              |
| `pane_pid_map`          | ✅           | ❌ never        | ✅              |
| `capture_pane`          | ✅           | ✅ via `dump-screen` | ✅          |
| `focus_pane`            | ✅           | ✅              | ✅              |

The "CLI-only" column is what users get out of the box if they don't
install the WASM plugin. It's intentionally degraded — agent
auto-discovery falls back to "trust whatever pane the stdin hook
self-reports" because there's no way to classify panes by foreground
command. The plugin is what closes that gap; the CLI-only mode exists
mainly so an operator who installs the muxa binary on a zellij host
gets *something* working immediately, even if it's not full parity.

Hook ancestry (`adapters/hook.rs`), discovery (`discovery.rs`), and
the watch loop should consult `caps()` before walking pid chains or
classifying by command, and pick the appropriate fallback for the
"structurally unsupported" case (vs the existing "transient empty"
case which the trait already handles via best-effort returns).

## Pane ID wire format

**Decision: backend-prefixed strings (`tmux:%4`, `zellij:3`).**

tmux's `%N` and zellij's numeric ids would collide if we tried to put
them in the same on-wire `PaneId` namespace. We pick prefixed strings
over a `(host, raw_id)` tuple because:

- The wire format already has `pane_id: String` everywhere — IPC
  schema, `state.json`, `prompts.ndjson`, `Agent.pane`. Switching to a
  tuple is a breaking schema change for every consumer; prefixing is
  an additive convention that just looks like a longer string to old
  readers.
- `state.json` and `prompts.ndjson` are already on disk in production;
  retroactively prefixing is a one-shot migration in
  [`crate::snapshot::load`] / [`crate::history::load_from_disk`]
  rather than a breaking version bump. Plan for that migration is to
  treat any unprefixed pane id as `tmux:` since pre-zellij operators
  only ever ran tmux.

Lock this in **before** the watch migration lands so the daemon's IPC
schema doesn't have to flip mid-transition.

## Open questions

- **Plugin distribution.** Ship the `.wasm` in the GitHub release, or
  expect users to `cargo build` it? First option is friendlier; second
  avoids us signing/hosting a WASM blob.
- **UX for "go to this pane".** zellij's focus model (tabs + floating
  panes + stacked panes) doesn't map 1:1 onto tmux's session/window/
  pane. The watch TUI's "press Enter to jump" needs a separate design
  pass — the trait method `focus_pane(&str) -> bool` is the seam, but
  what the daemon shows in the picker for a zellij floating pane is
  open.
- **Plugin API stability.** zellij plugin API is pre-1.0; assume one
  breaking bump per zellij minor release and pin a minimum supported
  version.

## Rough effort

| Step                              | Estimate |
| --------------------------------- | -------- |
| `PaneBackend` trait extraction    | ~1 day   |
| Zellij WASM plugin (PoC + polish) | ~2 days  |
| Zellij CLI backend + daemon glue  | ~1 day   |
| Watch / recap / sinks regression  | ~1 day   |
| Docs + release plumbing for `.wasm` | ~0.5 day |

**~1 week** of focused work, assuming the zellij plugin API does what the
docs claim. De-risk with a 2-hour PoC that just prints `PaneUpdate` payloads
to stderr before committing to the rest.

## Not doing

- A CLI-only zellij backend (no plugin). Tested mentally and rejected: without
  `terminal_command` from the plugin API, agent auto-discovery is too weak to
  ship.
- Replacing the tmux backend. Both coexist; the trait is the seam.
- Zellij-specific features (layouts, floating panes as first-class muxa
  concepts). Out of scope for the initial port.
