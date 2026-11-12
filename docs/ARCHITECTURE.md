# Architecture

`tend` keeps few moving parts: one daemon, one CLI, local files, and an
embedded SQLite collaboration store. No external database service is required.
For the two product layers (Attention and Runtime) and feature tiers, see
[DIRECTION.md](DIRECTION.md).

For the proposed execution-runtime improvements and their staged rollout, see
[Agent runtime review and integrated plan (한국어)](proposals/AGENT_RUNTIME_PLAN.ko.md).
That document tracks rollout progress; unfinished items are proposals rather
than the current runtime contract.

## Work domain and execution bindings

The logical hierarchy is **Workspace → Work → Run → Agent session**. An
external Linear/GitHub/Jira issue is an optional reference on Work, not its
identity. Local Work stage, external issue status, Run state, Agent state, and
attention/error signals are distinct fields.

Managed tmux is currently one execution adapter:

| tmux object | Current binding | Invariant |
| --- | --- | --- |
| Session | Workspace execution context | One managed session contains active Run windows for a workspace. |
| Window | One active Run for Work | `@tend_work_id` links the physical window to logical Work. |
| Pane | Agent execution surface | `@tend_managed_agent` links the pane to an Agent session/role/task. |

Starting the same work in a workspace reuses its compatible active window;
closing that window ends the Run rather than redefining Work. Unmanaged windows
are never inferred into Work. See [WORK_MODEL.md](WORK_MODEL.md).

## Flow

```text
agent hook/status event ─┐
herdr agent_status event ─┤
screen-manifest match ───┘
        |
        v
      tend hook  ---- unix socket ---->  tendd in-memory registry
      (or a daemon                           |
       detection task)                       +--> state.json
                                             +--> prompts.ndjson
                                             +--> activity.ndjson
                                             +--> collaboration.sqlite3
                                             +--> notifications / sinks
                                             +--> dashboard SSE
```

The CLI reads live daemon state over the socket and local retained files
for history/reporting views, and can drive agents back through the socket
(`send_prompt`/`capture`, exposed to other agents via `tend mcp`).

The controller daemon always adds a separate physical-node plane and publishes
its own `local` node directly from the in-process Store/backends. With
`[fleet]` enabled, each remote host task owns one OpenSSH stdio relay to the
remote user's local tendd and writes only to its own `FleetStore` entry. Remote agents are never
inserted into the local `Store`, so local reconciliation, pane-id reuse, and GC
cannot corrupt another node's truth. See [FLEET.md](FLEET.md).
The local-only Fleet watch path reuses the native watch runtime directly. The
multi-node path subscribes to `FleetStore` invalidations over IPC, coalesces
them, and reconstructs only revision-changed host topologies while retaining a
slow full-snapshot reconciliation poll.

Native clients use the same invalidation-first rule for durable subsystems:
Fleet topology, pipeline runs, Ask history, and collaboration mailboxes expose
content-free monotonic revision streams. A revision grants no read authority;
the client re-reads the normal scoped API after coalescing. tend-owned PTYs use
`read_session_wait`, which sleeps on an output/exit condition variable with a
bounded deadline. Control writes and resize requests use a separate client IPC
lane, so a waiting terminal read cannot head-of-line block input.

The daemon supervises the pipeline reconciliation CLI worker with a 60-second
deadline and a 64 KiB combined output limit. Shutdown cancels an in-flight
worker, and failed passes wait at least five seconds before retrying. This
stops the CLI worker, not agents already launched into tmux; the durable
pipeline store and pane adoption retain the recovery path.

## Components

| Component | Role |
| --- | --- |
| `tendd` | Long-running daemon. Owns the registry, IPC server, background tasks, and optional dashboard. |
| `tend` | CLI for status, watch, attend, recap, stats, reports, activity queries, init, hook entrypoints, and the `mcp` control server. |
| `Tend.app` | Native macOS control surface. Renders tendd-owned PTY byte streams through a pinned, locally built libghostty XCFramework; it never owns agent process identity. |
| Agent adapters | Translate Claude/Codex/Antigravity hook events and opencode plugin events into tend state transitions. |
| Pane backends | Resolve panes, sessions, captures, and foreground activity per host (tmux, cmux, rmux, herdr, zellij). The daemon can observe several at once; each non-tmux pane id is namespaced by host. The periodic observers (screen detection, the pane→session cache, periodic discovery) share one `list_panes` per host through a 2.5 s single-flight inventory; request paths, Fleet and liveness reconciliation always list fresh. |
| herdr bridge | Translates herdr's own `agent_status` stream into synthetic rows for agents tend has no hooks for. |
| Screen detection | Classifies hook-less agents (cursor, amp, …) from pane captures against TOML manifests; synthetic, hook-authoritative. |
| Activity ledger | Append-only duration source for state/foreground/human intervals. |
| FleetManager | Always-present local adapter plus independent SSH relay state machines, node identity, authorization, revision reconciliation, and per-host caches. |

Precedence when several producers describe one pane: **hooks > herdr
bridge > screen detection** — synthetic rows are evicted the moment a real
hook claims the pane.

## Data Files

| File | Purpose |
| --- | --- |
| `state.json` | Last daemon snapshot, used to rehydrate after restart. |
| `prompts.ndjson` | Retained prompt audit log. |
| `activity.ndjson` | Append-only duration ledger. |
| `session-activity.json` | Legacy/compat tmux foreground totals. |
| `collaboration.sqlite3` | Indexed durable mailbox, thread/Work metadata, and exact-session aliases/roles. An existing `collaboration.json` is imported once and retained as a migration backup. |
| `dashboard-work.json` | Schema-v2 Work definitions and external issue references keyed by logical Work identity. |
| `host-id` | Owner-only stable physical-node UUID used by Fleet handshakes. |

Paths are configurable; defaults live under `$XDG_DATA_HOME/tend`.

## Security

- IPC socket is hardened to owner-only permissions where supported.
- Dashboard is loopback-only by default.
- Public dashboard binding requires explicit `allow_public`. `public_read`
  exposes anonymous reads while requiring a PAT for mutations; `none` exposes
  reads with mutations disabled.
- External sinks are opt-in.
- Fleet uses fixed SSH command tokens, disables forwarding, validates exact
  global pane identities, defaults hosts to observe-only, and never opens a
  remote network listener.
- The codebase forbids unsafe Rust.
- The macOS app requires the additive `session_bytes_v1` capability and never
  substitutes the legacy lossy UTF-8 projection for terminal bytes.

## Shutdown

`SIGTERM`/`SIGINT` stop the IPC server and general background producers first.
After in-flight handlers and producers drain, tendd drains the activity
transition subscriber, then the prompt/activity writers, and finally flushes
`state.json`. This ordering keeps the ledgers and snapshot aligned with every
event committed before shutdown.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Useful focused checks:

```bash
cargo test -p tend-cli -- --nocapture
cargo test -p tend activity::tests -- --nocapture
cargo check -p tend-cli
```
