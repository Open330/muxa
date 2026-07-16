# Architecture

`muxa` is intentionally small: one daemon, one CLI, local files, and no
database.

## Flow

```text
agent hook/status event
        |
        v
      muxa hook  ---- unix socket ---->  muxad in-memory registry
                                             |
                                             +--> state.json
                                             +--> prompts.ndjson
                                             +--> activity.ndjson
                                             +--> notifications / sinks
                                             +--> dashboard SSE
```

The CLI reads live daemon state over the socket and local retained files
for history/reporting views.

## Components

| Component | Role |
| --- | --- |
| `muxad` | Long-running daemon. Owns the registry, IPC server, background tasks, and optional dashboard. |
| `muxa` | CLI for status, watch, attend, recap, stats, reports, activity queries, init, and hook entrypoints. |
| Agent adapters | Translate Claude/Codex/Gemini hook events into muxa state transitions. |
| tmux backend | Resolves panes, sessions, pane captures, and foreground session activity. |
| Activity ledger | Append-only duration source for state/tmux/human intervals. |

## Data Files

| File | Purpose |
| --- | --- |
| `state.json` | Last daemon snapshot, used to rehydrate after restart. |
| `prompts.ndjson` | Retained prompt audit log. |
| `activity.ndjson` | Append-only duration ledger. |
| `session-activity.json` | Legacy/compat tmux foreground totals. |

Paths are configurable; defaults live under `$XDG_DATA_HOME/muxa`.

## Security

- IPC socket is hardened to owner-only permissions where supported.
- Dashboard is loopback-only by default.
- Public dashboard binding requires explicit `allow_public`; unauthenticated
  public API requires the additional `dashboard.auth = "none"` opt-in.
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
