# herdr support

Status: **Phase 1 + Phase 2 implemented.** Verified against herdr **0.7.4**
(socket protocol **16**, schema dumped via `herdr api schema --json`).

[herdr](https://herdr.dev) is an agent-native terminal multiplexer. Unlike
zellij — where muxa's rich path needs a WASM plugin — herdr ships a full
local socket API out of the box, so muxa gets pane enumeration, captures,
pid maps, focus, *and* herdr's own agent-state detection without installing
anything into herdr.

## Why

muxa's positioning is a multiplexer-neutral observability/analytics layer:
tmux is the mature backend, zellij has a CLI baseline, herdr is the third
host. On herdr, muxa keeps its unique value (activity ledger, ACT/WACT,
stats/report/timeline, prompt history, notifications) while herdr provides
the pane host and — in Phase 2 — agent-state detection for agents muxa has
no hooks for.

## herdr ground truth (verified locally)

- Socket: `$HERDR_SOCKET_PATH`, else `~/.config/herdr/herdr.sock`.
  Named sessions (`herdr --session <name>`) serve their API on
  `~/.config/herdr/sessions/<name>/herdr.sock` instead — inside panes
  `$HERDR_SOCKET_PATH` always points at the right one, so env-first
  resolution is what makes named sessions work; the hardcoded default
  only covers the daemon observing the default session. A daemon
  observing a named session needs `HERDR_SOCKET_PATH` set explicitly.
- Wire: newline-delimited JSON. Request `{"id","method","params"}`;
  response echoes `id` with `result` (`type`-tagged) or `error`
  (`{code,message}`).
- Pane env vars inside panes: `HERDR_ENV=1`, `HERDR_PANE_ID`,
  `HERDR_SOCKET_PATH`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`.
- Methods muxa uses: `pane.list`, `pane.get`, `pane.read`
  (`source: visible|recent|recent_unwrapped|detection`),
  `pane.process_info` (shell pid, tty, foreground processes),
  `pane.focus`, `ping`; Phase 2 adds `events.subscribe`. The subscribable
  kinds are `pane.agent_status_changed`, `pane.output_matched`, and
  `pane.scroll_changed` — pane create/close are *not* subscribable, so
  pane liveness stays with the reconciler's `observe_panes` polling.
  Optional later: `pane.report_agent` (reverse path: muxa's hook-derived
  state shown in herdr's UI).
- `PaneInfo` (herdr): `pane_id`, `tab_id`, `workspace_id`, `terminal_id`,
  `cwd`, `focused`, `title`, `terminal_title`, `agent`, `agent_status`
  (`idle|working|blocked|done|unknown`), …
- Schema/API details: `herdr api schema --json`.

## Pane-id namespace

herdr pane ids are namespaced as **`herdr:<herdr_pane_id>`** everywhere
inside muxa (registry rows, prompt-history keys, `by_pane` queries),
following the zellij plugin's `zellij:terminal:<id>` precedent. The prefix
(`backend::herdr::PANE_ID_PREFIX`) is stripped before ids go back over the
herdr socket. Rationale: no collision with tmux `%N`, and cross-host code
can tell which host governs a row (see reaping guard below).

## Phase 1 — pane host parity

1. **Host detection** (`backend/mod.rs`, done): `MUXA_HOST=herdr`
   override; auto-detect `HERDR_PANE_ID`/`HERDR_ENV`, ordered
   `ZELLIJ` → `HERDR` → `TMUX` (newer hosts win nested-env ties; the
   override is the escape hatch). The daemon usually runs outside any
   host env — set `MUXA_HOST=herdr` in its environment (launchd/systemd
   or shell) to observe herdr.
2. **`HerdrBackend`** (`backend/herdr.rs`): direct-query `PaneBackend`
   over the socket, tmux-shape (stateless; callers already wrap calls in
   `spawn_blocking`). Per-call connect with ~1s timeout, matching the
   tmux command timeout.
   - `list_panes` → `pane.list` + per-pane `pane.process_info` to fill
     `current_command`/`pane_pid`/`tty`. Mapping into muxa `PaneInfo`:
     `session` = herdr `workspace_id`, `window_index` = `tab_id`,
     `pane_index` = raw herdr pane id, `current_path` = `cwd`,
     `socket` = `None` (never reuse the tmux-socket identity channel).
   - `observe_panes` → `complete` on a successful query; `complete(empty)`
     when the socket file is absent (server truly down ⇒ its panes are
     gone, tmux semantics); `incomplete` on connect/protocol errors or
     timeout (transient ⇒ must not reap).
   - `capture_pane` → `pane.read` (`visible`), `focus_pane` → `pane.focus`,
     `pane_pid_map` → shell pids from `pane.process_info`,
     `current_pane` → `$HERDR_PANE_ID` (prefixed).
   - `caps()`: all true when reachable.
3. **Hook correlation** (`adapters/hook.rs`): `host_pane_env()` learns
   `HERDR_PANE_ID` and applies the `herdr:` prefix. `$TMUX` is unset
   inside herdr, so `AgentId.tmux_socket` stays `None` and the
   `MUXA_TMUX_SOCKET` ingest scope gate passes herdr events untouched
   (`event_in_scope_with` treats `None` as in-scope). Do **not** stamp
   `HERDR_SOCKET_PATH` into `tmux_socket` — the gate would canonicalize
   and drop it.
4. **Cross-host reaping guard** (`state.rs` reconcile): a
   `PaneObservation` only governs rows whose pane id belongs to the
   observing backend's host: `%…` → tmux, `zellij:…` → zellij,
   `herdr:…` → herdr; unknown shapes stay governed by the active backend
   (today's behavior). Without this, a tmux-backend daemon reaps live
   herdr rows every reconcile tick (and vice versa) whenever both hosts
   are in use during migration.
5. **CLI attach** (`main.rs jump_to_pane`, done): `HostKind::Herdr` arm →
   `focus_pane`, zellij-shape.

Out of scope for Phase 1: `session_activity` (tmux foreground time —
returns empty on herdr hosts; a herdr workspace-focus analog can come
later), the web dashboard's tmux scanner panes view, `muxa status-line`
(a tmux status-right concept; herdr has its own sidebar).

## Phase 2 — herdr-native agent state (event bridge)

**Implemented** (`muxad::herdr_bridge`). A muxad background task — spawned
only when the active backend is herdr (`backend.kind() == HostKind::Herdr`)
— holds a socket connection and translates herdr's own agent-state
detection into synthetic muxa rows, giving muxa visibility into every
agent herdr detects (cursor, amp, copilot, …) without muxa hooks.

**Translation** (`translate`, a pure event-json → events fn, unit-tested):

| herdr `agent_status` | muxa `AgentEvent`   | resulting state |
|----------------------|---------------------|-----------------|
| `working`            | `ToolStarted`       | `Working`       |
| `blocked`            | `NotificationFired` (`NeedsInput`) | `WaitingInput` |
| `idle` / `done`      | `TurnStopped`       | `Idle`          |
| `unknown`            | — (dropped)         | —               |

Rows are keyed by `herdr:<pane_id>` with the SYNTHETIC session id
`synthetic-herdr:<pane_id>` (the same shape `discovery` mints for a
socket-less pane), so a discovery placeholder and a bridge row collapse
onto one registry key. herdr agent names map `claude`→`ClaudeCode`,
`codex`→`Codex`, `gemini`→`GeminiCli`, `opencode`→`Opencode` (loose,
lowercased substring match — herdr's exact slugs aren't pinned in the
schema); everything else is `AgentKind::Unknown` with the herdr
`display_agent`/`agent` name carried in the row's **`model`** field (set
via a trailing `Heartbeat`) so an Unknown row still names its agent.
Panes herdr attributes to no agent are dropped (a plain shell is not an
agent row).

**Precedence — hooks stay authoritative.** Before applying any bridge
update the task queries `Store::by_pane`; if a *non-synthetic* row already
owns the pane, the update is dropped. And because bridge rows are
synthetic, `Store::apply`'s synthetic-eviction pass drops them the moment
a real hook `Started`/tool/prompt event claims the pane — the
hook-authoritative rule falls out of the existing machinery for free.

**Subscription mechanics — a deviation from the original sketch.** herdr's
`pane.agent_status_changed` subscription is *per pane* (each subscription
item requires a `pane_id`; no wildcard), and subscribing does **not**
replay the pane's current status — only future changes stream. So on each
connect the bridge (1) enumerates panes via `pane.list` (which reports each
pane's current `agent`/`agent_status`), (2) subscribes to all of them in
one `events.subscribe` call, (3) *seeds* current state from step 1, then
(4) streams deltas, re-listing on a 10 s timer and reconnecting whenever
the pane set changes so newly-created panes get covered. The connection is
tokio `UnixStream` + `BufReader` lines with a capped-exponential-backoff
reconnect loop, wired into the daemon shutdown drain like the other
background producers. The bridge applies directly to the in-process
`Store` (not over IPC), matching the reconciler and other internal
producers.

Pane liveness stays with the reconciler (pane close is not a subscribable
herdr event kind). Optional reverse path — push muxa's hook-derived state
into herdr's UI via `pane.report_agent` — remains future work.

## Risks

- herdr is pre-1.0; the protocol number (16) is checked at connect time
  and mismatches degrade to "backend unreachable" with a logged warning,
  never a crash.
- Nested-host auto-detection is heuristic; `MUXA_HOST` overrides.
