# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-04-26

First tagged release. End-to-end agent observability for tmux: a daemon
(`muxad`) ingests hook events from Claude Code, Codex, and Gemini CLI and
surfaces state via a CLI (`muxa status`), a tmux status-line one-liner, a
fullscreen TUI dashboard (`muxa watch`), and opt-in desktop notifications.
49 tests green.

### Added

- Five-crate workspace: `muxa-core`, `muxa-runtime`, `muxa-adapters`,
  `muxad`, `muxa`. Versioned wire protocol over a 0600 unix socket.
- Hook adapters for Claude Code, OpenAI Codex, and Google Gemini CLI.
  opencode is deferred (SSE / TS-plugin path, not shell-hook).
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

[0.1.0]: https://github.com/Open330/muxa/releases/tag/v0.1.0
