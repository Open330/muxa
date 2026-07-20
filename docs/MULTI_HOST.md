# Multi-host observation

Status: **daemon + CLI implemented.** `muxad` observes every backend in
`muxa::active_backends()` simultaneously (tmux + herdr during a
migration) — the reconciler, discovery, session-activity sampling,
pane-session cache, history enrichment, and the herdr bridge/report
tasks all iterate the set. The CLI reads now aggregate across the same
set: `muxa watch` is the cross-multiplexer unified console (tmux + herdr
in one view, with per-row host badges), which no single multiplexer can
offer, and attach dispatches per row. See the CLI section below.

## Why

Today muxad runs exactly one `PaneBackend`, chosen from env at startup
(`detect_host_env`). During a tmux→herdr migration — the exact situation
muxa's herdr support targets — agents live on both hosts at once, and a
single-backend daemon only observes one side. The other side's rows
still ingest via hooks (env-based pane ids are host-agnostic) but get no
liveness/discovery/foreground coverage, and `muxa watch` shows panes
from only one host.

## What already works in our favor

The herdr work landed the key enablers:

- **Cross-host reaping guard**: `Store::reconcile_observation(obs, kind)`
  already scopes an observation to rows whose pane-id namespace matches
  the observing host (`%…`/`zellij:…`/`herdr:…` via `pane_id_host_kind`).
  Multiple observers converging the same store is safe *by construction*
  — each governs only its own namespace.
- **Hook ingest is host-agnostic**: `host_pane_env` resolves whichever
  host env var is present; no daemon backend involvement.
- **Per-host session activity**: `SessionActivitySource` already branches
  per host; tmux clients and herdr focused-workspace sampling can run
  side by side (ledger keys don't collide: tmux `$N` vs herdr workspace
  ids).
- **CLI attach dispatches per row**: `jump_to_pane` can switch on
  `pane_id_host_kind(pane_id)` instead of the process-global backend.

## Design

### Backend set, not backend

- `active_backends() -> Vec<SharedBackend>`: tmux if a server is
  reachable (or unconditionally — its methods degrade to empty), herdr
  if its socket exists, zellij per current detection. Config override:
  `MUXA_HOSTS=tmux,herdr` (env) / `hosts = ["tmux", "herdr"]` (config);
  `MUXA_HOST=<one>` keeps meaning "exactly this one" for compatibility.
- The daemon threads the set. Single-backend consumers changed shape
  (all **implemented** in `crates/muxad/src/main.rs` unless noted):
  - **Reconciler**: one `Reconciler` over the whole set
    (`Reconciler::with_sources`). Each tick observes every backend
    **concurrently** (`spawn_blocking` fan-out, joined) and reconciles
    each observation under its own `HostKind` via
    `reconcile_observation(obs, kind)` — completeness stays per-host so
    a herdr timeout can't trigger tmux reaping or vice versa. The
    workload scan runs **once** over the union of complete observations
    and is gated on *every* observation being complete (so an
    incompletely-observed host's rows aren't reset). The cross-host
    age-out (`mark_stale_cross_host_stopped`) receives **all** observed
    kinds, so a row on a host in the set is governed by that host's
    reconcile pass while a row on a host *not* in the set ages out.
  - **Discovery**: startup + periodic passes iterate the set and concat
    pane scans (`run_discovery` per backend, reports summed).
  - **Session activity**: a **single** tracker samples one source per
    host that has a foreground signal (tmux, herdr) and merges them into
    one ledger. One tracker = one writer, so the two sources can't
    clobber `session-activity.json` (each `save()` rewrites the whole
    file); merging is safe because the session-id keyspaces are disjoint
    across hosts (tmux `$N` vs herdr workspace ids).
  - **herdr bridge + report task**: spawned when herdr ∈ set (previously:
    when the single backend's kind == herdr).
  - **pane-session cache / `enrich_from_history`**: enumerate the set and
    union the per-backend `list_panes` (namespaces are disjoint, so the
    union can't conflate panes).
  - **IPC server backend**: consumed only for routing an inbound
    `BackendPaneSnapshot` push into `ingest_pane_snapshot` (a no-op on
    every backend except zellij). Routed to the zellij backend in the set
    if present, else the primary — so pushes land in the same instance
    discovery/reconciler read.
- **CLI reads stay aggregated** (presentation follow-up): watch/`muxa
  panes` list from every active backend (concat; rows already carry their
  namespace). `current_pane`/`focus_pane` resolve per pane-id namespace.

### Watch as the unified console (implemented)

With the set in place, the CLI reads aggregate across it:

- **`compute_refresh`** fans `list_panes()` and the per-host session
  source (`sessions_for_host`: tmux `list-sessions`, herdr
  `workspace.list`, zellij empty) across every active backend inside
  `spawn_blocking`, concurrently, then concats. Namespaces keep rows
  distinct (tmux `%N` / herdr `herdr:…`), so a tmux session named `w1`
  and a herdr workspace `w1` remain separate rows.
- **Host badges**: when the visible row set spans more than one host
  (`rows_multi_host`), each row's SESSION/PANE cell gets a subtle dim
  host tag via `prepend_host_badge` (`row_host` classifies by pane-id
  namespace; labels mirror the dashboard TUI's `CardHost`). Single-host
  users see no change — the badge only disambiguates a mixed view.
- **Attach dispatch**: `jump_to_pane` (shared by `watch` Enter and
  `muxa attend`) resolves the host from `pane_id_host_kind(pane_id)`
  *first* — a `herdr:` row focuses via a herdr backend even when the
  process-global host is tmux — falling back to the process-global
  backend's kind for unrecognized ids (`dispatch_kind`).
- **Other surfaces**: `muxa panes`, `stats`, and `timeline` enumerate
  via `all_panes()` (concat across the set); `muxa panes` prints a
  per-host empty hint for any host in the set that contributed zero.
  Live pane captures (`watch` preview, `dashboard`) resolve the backend
  by pane-id namespace (`backend_for_pane`). `current_pane`/status-line
  ("where am I") stay single-host — env-based location is inherently one
  host.
- Inline time data (ACT/DUR from the ledger) needs nothing new — keys
  are already host-scoped.

### Non-goals (this phase)

- Remote/multi-machine aggregation (a different transport problem).
- zellij enumeration improvements (still plugin-gated).
- Merging the web dashboard's tmux scanner with the backend set (the
  scanner is dashboard-only plumbing; follow-up).

## Risks / open questions

- **Startup cost**: probing both hosts each tick is cheap (a unix
  connect + a `tmux list-panes`), but the reconciler's spawn_blocking
  fan-out should run backends concurrently, not serially, to keep the
  tick budget.
- **Row identity collisions**: none known — namespaces are disjoint by
  the pane-id prefix design.
- **`MUXA_TMUX_SOCKET` scoping**: stays tmux-only; herdr has no
  multi-instance scoping (single socket per session; named sessions are
  explicit paths). A `MUXA_HERDR_SOCKET`-style scope can follow if
  needed.
- **Watch session-view sorting** across heterogeneous hosts (tmux
  attached_clients vs herdr's always-1) — pick "activity recency" as the
  cross-host sort key.

## Estimated shape

Moderate: daemon startup + reconciler/discovery/session-activity
iteration (~the size of the Phase 2 bridge), watch aggregation +
badges (~the size of the session-view change). No store/schema changes.
