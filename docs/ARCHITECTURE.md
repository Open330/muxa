# Architecture

`muxa` is intentionally small: one daemon, one CLI, local files, and no
database.

## Managed tmux domain model

Muxa is optimized around a work-oriented tmux model:

| tmux object | Domain identity | Invariant |
| --- | --- | --- |
| Session | Workspace/project | One managed session contains the project's work windows. |
| Window | Work/ticket | One managed window is created or reused for a work ID and cwd. It is also the collaboration room. |
| Pane | Agent | Every managed agent is placed in its own pane inside the work window. |

This mapping is shared by the CLI, `muxa watch`, the daemon registry, and
`muxa mcp`. Starting the same work in a workspace must reuse its window or fail
on an incompatible cwd; adding an agent must add a pane; closing work kills the
window, while closing a workspace kills the session. Destructive controls refuse
unmanaged targets.

## Flow

```text
agent hook/status event ─┐
herdr agent_status event ─┤
screen-manifest match ───┘
        |
        v
      muxa hook  ---- unix socket ---->  muxad in-memory registry
      (or a daemon                           |
       detection task)                       +--> state.json
                                             +--> prompts.ndjson
                                             +--> activity.ndjson
                                             +--> collaboration.json
                                             +--> notifications / sinks
                                             +--> dashboard SSE
```

The CLI reads live daemon state over the socket and local retained files
for history/reporting views, and can drive agents back through the socket
(`send_prompt`/`capture`, exposed to other agents via `muxa mcp`).

## Components

| Component | Role |
| --- | --- |
| `muxad` | Long-running daemon. Owns the registry, IPC server, background tasks, and optional dashboard. |
| `muxa` | CLI for status, watch, attend, recap, stats, reports, activity queries, init, hook entrypoints, and the `mcp` control server. |
| Agent adapters | Translate Claude/Codex/Gemini hook events into muxa state transitions. |
| Pane backends | Resolve panes, sessions, captures, and foreground activity per host (tmux, herdr, zellij). The daemon can observe several at once; each pane id is namespaced by host. |
| herdr bridge | Translates herdr's own `agent_status` stream into synthetic rows for agents muxa has no hooks for. |
| Screen detection | Classifies hook-less agents (cursor, amp, …) from pane captures against TOML manifests; synthetic, hook-authoritative. |
| Activity ledger | Append-only duration source for state/foreground/human intervals. |

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
| `collaboration.json` | Durable same-window mailbox plus exact-session aliases and roles. |

Paths are configurable; defaults live under `$XDG_DATA_HOME/muxa`.

## Security

- IPC socket is hardened to owner-only permissions where supported.
- Dashboard is loopback-only by default.
- Public dashboard binding requires explicit `allow_public`. `public_read`
  exposes anonymous reads while requiring a PAT for mutations; `none` exposes
  reads with mutations disabled.
- External sinks are opt-in.
- The codebase forbids unsafe Rust.

## Shutdown

`SIGTERM`/`SIGINT` stop the IPC server and general background producers first.
After in-flight handlers and producers drain, muxad drains the activity
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
cargo test -p muxa-cli -- --nocapture
cargo test -p muxa activity::tests -- --nocapture
cargo check -p muxa-cli
```
