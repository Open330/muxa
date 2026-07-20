# Multi-host observation (design proposal)

Status: **proposal — not implemented.** Prerequisite for repositioning
`muxa watch` as the cross-multiplexer unified console (tmux + herdr +
zellij in one view), which no single multiplexer can offer.

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
- The daemon threads the set. Single-backend consumers change shape:
  - **Reconciler**: one pass per backend per tick (or one task per
    backend). Each `observe_panes()` feeds
    `reconcile_observation(obs, backend.kind())` — the existing
    signature; completeness stays per-host so a herdr timeout can't
    trigger tmux reaping or vice versa.
  - **Discovery**: iterate the set, concat pane scans.
  - **Session activity**: spawn one sampler per host that has a source
    (tmux, herdr).
  - **herdr bridge + report task**: spawn when herdr ∈ set (today:
    when kind == herdr).
- **CLI reads stay aggregated**: watch/`muxa panes` list from every
  active backend (concat; rows already carry their namespace).
  `current_pane`/`focus_pane` resolve per pane-id namespace.

### Watch as the unified console

With the set in place, watch's remaining work is presentation:

- Session view lists tmux sessions *and* herdr workspaces (both sources
  exist today, gated to one at a time) with a host badge per row —
  reuse the dashboard TUI's `CardHost` labels.
- Pane view concatenates hosts; attach already dispatches per row.
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
