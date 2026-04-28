# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-04-28

### Added

- Event-driven snapshot of the agent registry to
  `$XDG_DATA_HOME/muxa/state.json`. `Store::apply` (plus `gc` /
  `reconcile`) wakes a writer task via `tokio::sync::Notify`; the
  writer debounces (default 200 ms) and writes via temp-file +
  atomic rename + parent-dir fsync. Idle daemons produce zero disk
  traffic; bursts collapse into one write. Configurable via
  `[state]` (`enabled` / `path` / `debounce_ms`).
- Restart rehydrate is layered: `state.json` hydrates first, then
  `enrich_from_history` replays the most recent `prompts.ndjson`
  entry for any pane the snapshot missed (real `session_id` +
  `last_prompt` recovered without waiting for a fresh hook), and
  finally discovery synthesizes placeholders for whatever's left.
  Synthetics are now the fallback, not the primary mechanism.
- `[watch] hide_paneless = true` (default): hide agents that aren't
  bound to a tmux pane from `muxa watch`. They're not actionable
  anyway (`Enter` is a no-op), so the picker stays focused; the
  footer surfaces a `+N paneless` count so the rows aren't lost.
  `muxa watch --include-paneless` flips the filter for one
  invocation without editing config.
- IPC handler drain on shutdown. `Server::run` tracks handlers on a
  `JoinSet` and drains them with a 5 s bounded timeout before
  returning, so `Store::apply` calls landing during shutdown make
  it into the snapshotter's final flush. The snapshotter listens on
  a dedicated channel so it's the literal last task to die.

### Changed

- `state.json` and `prompts.ndjson` are chmod 0600 — same posture as
  the IPC socket, since both files carry user prompts/responses.
  Tempfiles inherit the mode through `rename(2)`.
- `Server::run` shutdown path now drains in-flight handlers before
  unlinking the socket. The previous behavior was fire-and-forget,
  which left a small lost-update window when an ingest landed mid-
  shutdown.

## [0.1.0] - 2026-04-26

First tagged release. End-to-end agent observability for tmux: a daemon
(`muxad`) ingests hook events from Claude Code, Codex, and Gemini CLI and
surfaces state via a CLI (`muxa status`), a tmux status-line one-liner, a
fullscreen TUI dashboard (`muxa watch`), an opt-in HTTP/SSE web dashboard,
and opt-in desktop notifications. 92 tests green.

### Added

- Single `muxa` library + two binaries (`muxad`, `muxa-cli` shipping the
  `muxa` binary). Versioned wire protocol over a 0600 unix socket.
- Hook adapters for Claude Code, OpenAI Codex, and Google Gemini CLI.
  opencode is deferred (SSE / TS-plugin path, not shell-hook).
- Web dashboard via `muxad --dashboard`: read-only HTTP UI with
  `/api/health`, `/api/agents`, `/api/panes`, plus a live SSE stream at
  `/api/events` (snapshot + transition + lagged events). Embedded
  HTML/JS/CSS via `rust-embed`. Loopback-only by default; non-loopback
  binds require both `allow_public = true` and a non-empty bearer token,
  enforced once at startup. Constant-time token comparison via `subtle`.
- Multi-socket tmux pane scanner — discovers every running tmux server
  under `$TMUX_TMPDIR` / `/tmp/tmux-$UID/` (1 s per-socket timeout) and
  folds results into a single `ScanResult` with per-socket error
  isolation. TTL-cached (`PaneCache`) so HTTP handlers don't re-fork
  tmux per request.
- `docs/DASHBOARD.md` — operator's guide for the web dashboard
  (binding, auth, deploy patterns).
- `muxa status` — session-aware, deduped, ANSI-colored table; honors
  `NO_COLOR`.
- `muxa watch` — fullscreen ratatui TUI at 2 Hz. Lists tracked agents
  on top and untracked tmux panes below; `Enter` jumps to the selected
  pane via `tmux select-pane` + `switch-client`, or `tmux attach-session`
  when launched from a bare shell. Drop-in replacement for `prefix + s`.
- `muxa watch` configurable columns and widths via `[watch]` /
  `[watch.widths]` in `config.toml`. Default order leads with the last
  prompt; `model` / `ctx` / `cost` are opt-in.
- `muxa status-line --pane` — tmux `status-right` one-liner, scoped to
  `$TMUX_PANE` by default.
- `muxa hook claude-statusline --forward CMD` — tees Claude's status-line
  JSON to muxa and a downstream tool (e.g. `ccstatusline`) so users can
  stack both.
- `muxa recap` — show the last prompt for a given pane.
- `muxa sync` and automatic backfill on `muxad` startup — scans
  `tmux list-panes` for known agent CLIs and registers synthetic
  entries, so a daemon restart doesn't blank out live agents. Toggle
  via `[discovery] enabled = ...`.
- Desktop notifications on `*→WaitingInput`, `*→Error`, and
  `Working→Stopped` transitions. libnotify on Linux, NSUserNotification
  on macOS, WinRT toast on Windows. Opt-in via `[notifier]`.
- Korean README translation (`README.ko.md`).
- Example wiring: `examples/muxad.service` (systemd user unit),
  `examples/muxa.tmux.conf`, `examples/claude-settings.json`,
  `examples/claude-settings-with-ccstatusline.json`.
- CI: rustfmt, clippy `-D warnings`, workspace tests on Linux + macOS,
  MSRV 1.88 check, and `cargo-deny`.

### Changed

- Workspace consolidated from 5 crates (`muxa-core`, `-runtime`,
  `-adapters`, `muxad`, `muxa`) into a single `muxa` lib plus the two
  binaries. No public API or wire-protocol changes — the prior split
  was internal scaffolding.
- `TRANSITION_CHANNEL_CAPACITY` raised 64 → 256 to give long-lived SSE
  subscribers headroom against pane-burst events.
- Workspace leans on `strum` for enum derives and `RUST_LOG` for log
  filtering; redundant config knobs removed.
- `muxa watch` now includes untracked tmux panes alongside tracked
  agents in the same selectable table.
- README refreshed with a hero animation, an agents-first quickstart
  block, the live demo GIF, and a tmux-popup recipe (`prefix + s`
  launches `muxa watch`).
- Demo tape reordered to `status → switch → status-line` and runs the
  last two scenes inside a real tmux so `status-right` glyphs are
  legible.
- MSRV bumped to 1.88 (transitive `time-0.3.47` requirement).

### Fixed

- Friendlier client error when the daemon socket isn't reachable —
  no more raw `os error 111` (connection refused) leaking to users.
- `muxa watch` jump-to-pane: 3-command tmux sequence resolves `pane_id`
  to `session:window.pane` before switching, so attach is robust across
  sessions and bare shells.
- `muxa status` table alignment with ANSI color: restored
  `comfy-table` default features (`custom_styling`) so escapes are
  treated as zero-width.
- Hook ingest is best-effort — adapter or daemon hiccups never block
  the agent CLI's actual command from running.

[Unreleased]: https://github.com/Open330/muxa/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Open330/muxa/releases/tag/v0.2.0
[0.1.0]: https://github.com/Open330/muxa/releases/tag/v0.1.0
