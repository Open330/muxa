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
  Phase 2 reverse path (implemented): `pane.report_agent` /
  `pane.release_agent` push muxa's hook-derived state into herdr's UI.
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
   `ZELLIJ` → `HERDR` → `TMUX`. The daemon usually runs outside any
   host env — set `MUXA_HOST=herdr` in its environment (launchd/systemd
   or shell) to observe herdr.

   **Nested-host tie-break — one policy, both places.** Env presence
   alone can't tell an inner host from an outer one: a herdr pane launched
   from a tmux shell *inherits* the outer `$TMUX`/`$TMUX_PANE` (empirically
   verified), and tmux launched inside a herdr pane inherits `$HERDR_*`. muxa
   resolves the ambiguity **herdr-wins** — launching herdr from a tmux shell
   is the common migration path, so on a `HERDR_* + TMUX` tie both the host
   detector (`detect_from`) *and* the hook pane-stamper
   (`adapters/hook.rs::host_pane_env`) pick herdr. Keeping the two in lockstep
   is what makes the pane a hook is stamped onto and the backend that observes
   it agree. **Caveat:** the rarer nesting — tmux running *inside* a herdr
   pane — is misdetected by this default; set **`MUXA_HOST=tmux`** to force it
   (the override is honored by both `detect_from` and `host_pane_env`;
   `MUXA_HOST=herdr`/`zellij` force those hosts analogously).
2. **`HerdrBackend`** (`backend/herdr.rs`): direct-query `PaneBackend`
   over the socket, tmux-shape (stateless; callers already wrap calls in
   `spawn_blocking`). Per-call connect with ~1s per-read timeout, matching the
   tmux command timeout, plus an aggregate ~2× read deadline over the whole
   reply loop so a server that *streams* unrelated lines faster than the
   per-read timeout can't loop forever and wedge a reconcile/watch refresh
   (the per-read timeout resets on every line; only the aggregate deadline
   bounds the total).
   - `list_panes` → `pane.list` + per-pane `pane.process_info` to fill
     `current_command`/`pane_pid`/`tty`. Mapping into muxa `PaneInfo`:
     `session` = herdr `workspace_id`, `window_index` = `tab_id`,
     `pane_index` = raw herdr pane id, `current_path` = `cwd`,
     `socket` = `None` (never reuse the tmux-socket identity channel).
     Per-pane enrichment is bounded by a total time budget: once spent, the
     remaining panes return with empty process fields rather than let a slow
     server turn one `pane.list` into an N×1s stall.
   - `observe_panes` → `complete` on a successful query; `complete(empty)`
     **only** when the socket file is authoritatively absent
     (`try_exists() == Ok(false)` ⇒ server truly down ⇒ its panes are gone,
     tmux semantics); `incomplete` on connect/protocol errors, timeout, *or a
     stat that errors* (`try_exists() == Err` — EACCES/EIO/stalled automount;
     transient ⇒ must not reap). Using `try_exists` rather than `exists` is
     what keeps a stat error from being swallowed as an authoritative empty
     and triggering a mass reap.
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

   **Foreign-host age-out** (`Store::mark_stale_cross_host_stopped`): the
   guard exempts a foreign-host row from *immediate* reaping, but a
   single-backend daemon never observes those panes at all and the GC only
   evicts `Stopped` rows — so a `herdr:` row left in `state.json` after the
   operator switches the daemon back to tmux would ghost forever. Each
   reconcile tick the reconciler flips foreign-host rows (pane id classifies
   to a host *not* in the active observing set) to `Stopped` after the same
   inactivity window the paneless orphan sweep uses
   (`[reconciler] paneless_stale_timeout_secs`, default 86400), after which
   the existing GC removes them. A genuinely-live remote row keeps itself
   alive by emitting activity; a dead one ages out. The check takes a *set*
   of observing kinds (today always the one active backend) so a future
   multi-host daemon can pass several without changing shape. Note
   `muxa prune` targets *paneless* orphans only — a foreign-host row still
   carries its `herdr:`/`zellij:` pane id, so this age-out (not `prune`) is
   what clears it.
5. **CLI attach** (`main.rs jump_to_pane`, done): `HostKind::Herdr` arm →
   `focus_pane`, zellij-shape.

6. **Session foreground time** (`session_activity.rs`, done): the tmux
   foreground-time ledger has a herdr analog. On herdr hosts the sampler
   branches (`SessionActivitySource::Herdr`) and, each poll, queries the
   focused herdr **workspace** via `workspace.list` (the cheapest call
   reporting each workspace's `focused` flag; `backend::herdr::herdr_focused_workspace`)
   instead of `tmux list-clients`. The focused workspace becomes a single
   `SessionInfo` with one "attached client", so the *same*
   `apply_sample_report` accounting credits foreground time to it and emits
   the same ledger intervals + `session-activity.json`. The muxa session key
   is the raw `workspace_id` (`w1`), matching `PaneInfo.session` from
   `HerdrBackend::list_panes` so ledger keys line up with pane rows. No
   focused workspace, or an unreachable/absent server, yields an empty
   sample — the tmux "no server running" analog, which closes any open
   interval. Only the *sampling source* branches; downstream accounting is
   shared with tmux.

   **Known limitation (mitigation out of scope for now):** herdr's socket
   API exposes no client-attach state — there is no `client.list` analog.
   So, unlike tmux (which credits time only while an interactive client is
   attached), herdr focus time accrues even when the server sits detached
   with no client attached. This **inflates ACT for always-on detached
   herdr servers**. herdr also has no per-client input/scroll signal, so no
   `HumanInteraction` (keypress/scroll) ticks are emitted on herdr hosts —
   only the workspace-foreground observation.

7. **Watch work view** (`muxa-cli/src/watch.rs`, done): the workspace/work
   view sources its "sessions" per host — tmux shells `list-sessions`,
   herdr derives them from `workspace.list`
   (`backend::herdr::herdr_list_workspaces`, the `list_sessions` analog).
   Each workspace becomes a `SessionInfo` whose id is the raw `workspace_id`
   (matching `PaneInfo.session` and the ledger key, so the DUR column
   resolves) and whose display name is the workspace `label` (falling back
   to the id). Rows still come from the pane inventory as on tmux; sourcing
   the workspace list is what lights the DUR column and surfaces the human
   label instead of the raw `w1`. The activity bridge gained a
   session-id fallback so the herdr key (pane session == ledger key) resolves
   directly. Attach (Enter-twice) is host-dispatched via `jump_to_pane` →
   `focus_pane`, so a herdr workspace row never fires a tmux-only action. The
   dashboard TUI already sources sessions from the daemon's own registry, so
   it was host-agnostic and needed no change. zellij has no session concept
   here and stays empty.

8. **Web dashboard panes view** (`dashboard/server.rs`, done): the
   `/api/panes` route sources its rows from the tmux multi-socket scanner
   (`tmux::scanner::scan`), which sees nothing on a herdr host. The daemon
   now threads its active `SharedBackend` into the dashboard `AppState`;
   the pane-cache refresh closure branches on `backend.kind() ==
   HostKind::Herdr` and runs **both** the tmux `scanner::scan` *and*
   `HerdrBackend::list_panes()` (a blocking socket call, so `spawn_blocking`,
   folded into the scanner's `ScanResult` shape via
   `tmux::scanner::herdr_scan_result`), then concatenates them
   (`merge_pane_scans`) — herdr panes appended onto the tmux scan, tmux
   per-socket errors preserved in `errors`. Running only the herdr side would
   drop live tmux panes during a mixed-host migration (they'd vanish from
   `/api/panes` and the timeline); merging keeps both. Both `/api/panes` and
   the timeline handler's pane→session map go through the same refresh, so
   they agree on herdr. Field mapping reuses the muxa `PaneInfo` the
   backend already returns (`pane_id` `herdr:<id>`, `session` =
   `workspace_id`, `window_index` = `tab_id`, `pane_index` = raw id,
   `current_command`/`current_path` enriched); the two dashboard-only
   fields the backend leaves blank are filled with a synthetic `"herdr"`
   socket identity (the web UI's socket-filter chip splits on `/`, and the
   daemon observes exactly one herdr server) and an empty `attach_command`
   (herdr has no copyable shell attach line — the CLI focuses over the
   socket). `MUXA_TMUX_SOCKET` is *not* consulted on the herdr path — it
   scopes tmux sockets only, consistent with the ingest scope gate that
   passes socket-less herdr events. The tmux scanner path is byte-identical
   (`backend == None`/non-herdr → unchanged `scanner::scan`). zellij stays
   empty (no pane-metadata surface until the WASM plugin lands).

Out of scope for Phase 1: `muxa status-line` (a tmux status-right concept;
herdr has its own sidebar).

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
herdr event kind).

### Reverse path — muxa's hook state into herdr (`pane.report_agent`)

**Implemented** (`muxad::herdr_bridge`, `report_decision` + a second
transition-subscriber task, both herdr-host only). herdr treats an installed
integration's report as authoritative over its *own* screen detection, so when
muxa has real hook truth for a pane, it hands that truth to herdr's sidebar
instead of letting herdr guess from scrollback. The task subscribes to the same
`Store::subscribe()` transition stream the desktop notifier and activity ledger
use; on every state change it decides via the pure `report_decision` fn and, for
a reportable row, fires `pane.report_agent` (`source = "muxa"`) at the herdr
socket. Failures are non-fatal (bounded request timeout, debug-logged, dropped)
and never hold the transition channel back.

**Only REAL rows on `herdr:` panes are reported.** `report_decision` returns
nothing unless the row is *both* on a `herdr:` pane (the prefix is stripped for
the wire `pane_id`) *and* non-synthetic. Mapping:

| muxa `AgentState`              | herdr call / state                       |
|--------------------------------|------------------------------------------|
| `Working`                      | `pane.report_agent` `working`            |
| `WaitingInput`/`WaitingChoice` | `pane.report_agent` `blocked` (+message) |
| `Error`                        | `pane.report_agent` `blocked` (+message) |
| `Idle`                         | `pane.report_agent` `idle`               |
| `Stopped`                      | `pane.release_agent`                     |
| `Starting`                     | — (transient; nothing reported)          |

`agent` is a herdr-aligned slug (`ClaudeCode`→`claude`, `Codex`→`codex`,
`GeminiCli`→`gemini`, `Opencode`→`opencode`, else the kind name), matching the
forward bridge's `classify_agent` so the same pane carries the same label in
both directions. `agent_session_id` is muxa's real session id, and a
process-global monotonic `seq` lets herdr discard out-of-order reports. `blocked`
rows carry `last_notification` as the `message` so herdr shows *what* the agent
is waiting on / erroring about.

**No feedback loop — the load-bearing invariant.** Synthetic rows (the ones the
forward bridge *mints* from herdr's own detection) are NEVER reported back:
echoing them would form a cycle — report → herdr adopts muxa as authority →
herdr stops emitting `pane.agent_status_changed` → the forward bridge starves →
the synthetic row goes stale. The two directions stay disjoint by pane
ownership. A `herdr:` pane with a real hook row is reported here, and the forward
bridge's `apply_update` already *drops* herdr's screen-detection updates for that
same pane — so herdr's flapping is silenced from both ends and muxa is the single
source of truth. A `herdr:` pane with only a synthetic bridge row is never
reported; herdr keeps detecting and the forward bridge keeps mirroring it.

**Release.** `Stopped` is muxa's terminal state — reached on a `SessionEnded`
hook and on reconciler/GC reaping (the reaper flips the row to `Stopped`, which
emits a transition) — and triggers `pane.release_agent`, returning authority to
herdr's own detection. A row GC-evicted *after* it already went `Stopped` emits
no further transition, and there is no distinct "row removed" transition to
observe, so releasing on the `…→Stopped` edge is both necessary and sufficient.
One consequence of being change-driven: a hook row rehydrated from `state.json`
at startup isn't reported to herdr until its *next* transition.

## Risks

- herdr is pre-1.0; the protocol number (16) is checked at connect time
  and mismatches degrade to "backend unreachable" with a logged warning,
  never a crash.
- Nested-host auto-detection is heuristic; `MUXA_HOST` overrides.
