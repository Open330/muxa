# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.2] - 2026-04-30

### Added

- **`muxad-launchd` component** (#19). — macOS auto-start parity with the
  Linux `muxad-systemd` component. `muxa init` writes
  `~/Library/LaunchAgents/dev.open330.muxad.plist`, then runs
  `launchctl bootstrap gui/<uid>` + `kickstart -k` so the agent is
  loaded immediately and re-launched on every login. Uninstall does
  the inverse (`launchctl bootout` + plist removal). Resolved through
  the standard marker-block / outcome flow, so dry-run + uninstall
  work the same as everywhere else.
- **`muxad-shellrc` component** — cross-platform fallback for hosts
  with no service manager (containers, BSD, WSL1, minimal Linux,
  CI sandboxes). Appends a `# >>> muxa managed (muxad-shellrc) >>>`
  block to the user's primary shell rc (auto-detected from `$SHELL`
  → `~/.zshrc` for zsh, `~/.bashrc` for bash, fish config dir for
  fish, `~/.profile` otherwise) that lazy-starts `muxad` from the
  next interactive shell.
- **`--start-daemon` flag (default `true`)** — at the end of `muxa
  init`, if `muxad` isn't already running, spawn it detached. Closes
  the gap reported on macOS where the wizard finished cleanly but
  `muxa status` / `muxa watch` failed because the daemon had never
  been started. Override with `--start-daemon=false` for dotfile
  bootstraps that prefer to manage the daemon out-of-band.

### Changed

- **Smart daemon-manager selection.** `Preset::Standard` /
  `Preset::Full` now pick exactly one daemon-manager component
  (`muxad-systemd` / `muxad-launchd` / `muxad-shellrc`) based on the
  host OS, and the wizard's multi-select hides the OS-irrelevant
  candidates (no more "muxad: systemd user service" option on
  macOS). `Detection::recommended_daemon_manager()` further degrades
  to `muxad-shellrc` when systemctl/launchctl is missing on a host
  that would normally support them — the `Preset::Standard` ⇒
  working install invariant holds even in stripped-down envs.

## [0.4.1] - 2026-04-30

### Fixed

- **Critical: `muxa init` no longer kills running tmux sessions** (#18). The
  v0.4.0 post-apply verification step shelled out to
  `tmux -f <conf> start-server \; kill-server` to syntax-check the
  edited `~/.tmux.conf`. The `-f` flag only scopes the config file —
  the command still attached to the user's *default* tmux socket and
  the trailing `kill-server` killed every running session on the box.
  The check has been removed entirely; the modest upside (parse
  validation after a write we control end-to-end) was not worth the
  catastrophic blast radius. Users see the diff in the review step
  before apply, and `tmux source-file` surfaces any error when it
  runs. The "tmux config syntax check" line in the final summary is
  gone too.

## [0.4.0] - 2026-04-30

### Added

- **`muxa init` install wizard** (#17). Interactive (cliclack-styled flow:
  pre-flight → multi-select → review → apply → verify) and
  non-interactive (`--preset {minimal,standard,full}`, `--yes`,
  `--component`, `--no <id>`, `--dry-run`, `--uninstall`) entry points
  cover both the "first install" and "in CI / dotfile bootstrap" cases.
  Components: tmux popup + status-line, Claude / Codex / Gemini hook
  merge, `muxad` systemd user service, web dashboard token. Each
  component owns a `# >>> muxa managed (id) >>>` marker block (or a
  command-prefix match in JSON / TOML), so `--uninstall` is a clean
  surgical reverse rather than a delete-all. Auto-detects existing
  agent configs and pre-checks the matching components in the picker.
  Backups land at `<file>.muxa-backup-<unix_ts>` before any write.
  Companion `scripts/install.sh` is a 30-line `curl | sh` bootstrap
  that hands off to `muxa init`, so all real install logic stays in
  one auditable place.

## [0.3.2] - 2026-04-30

### Added

- **Web dashboard `LIMITS` column** (#16). Brings the dashboard to
  parity with the CLI `muxa watch` rate-limit rendering shipped in
  v0.3.1. The data was already on `/api/agents` — only the frontend
  was missing. Three render states matching the CLI:
  - red `⛔ 5h in 2h 14m` — currently capped (with tooltip surfacing
    `last_notification` so hover shows the upstream message).
  - yellow `5h 84%` — utilisation ≥ 80% on either window.
  - dim `5h 31%` / `—` — non-warning utilisation / no data.
- JS helpers (`isCurrentlyCapped`, `scopePrefix`, `formatRelativeUntil`)
  mirror the Rust renderer 1:1 so behaviour drift between the two
  surfaces stays trivially portable.

## [0.3.1] - 2026-04-30

### Added

- **Claude Code rate-limit detection** in `muxa watch` (#15). Three-layer
  signal pipeline:
  - **Statusline `rate_limits` JSON** (primary, official): parses
    `rate_limits.{five_hour,seven_day}.{used_percentage,resets_at}` from
    Claude Code 2.1.80+'s documented statusline schema. Plumbs both
    windows' percentage + reset times through `Heartbeat` onto the
    agent row.
  - **`StopFailure` hook** (mid-turn 429s): new
    `muxa hook claude --event stop_failure` entrypoint.
    `error == "rate_limit"` emits the new `AgentEvent::RateLimited`;
    other error kinds (auth, billing, server) flip the row to `Error`
    with the upstream message.
  - **Transcript fallback** (older Claude Code, sub-agent caps): scans
    the JSONL tail for `error:"rate_limit"` + `apiErrorStatus:429`
    markers on synthetic assistant entries, plus the legacy
    `"You've hit your limit"` / `"Claude usage limit reached"`
    `tool_result` form. Two-tracker walk so a normal turn after a
    `continue` correctly clears the cap signal.
- New `limits` column for `[watch] columns`. Renders three states:
  red `⛔ 5h in 2h 14m` cap badge (rate-limit active), yellow
  `5h 84%` warning (≥80% utilisation), default-or-dim
  utilisation/empty fallback. Uses relative-time formatting — no
  syscall, no locale dependency, no UTC fallback ambiguity.
- New detail-line placeholders: `{rate_limit}` (mirrors the column,
  human-readable), `{rate_limit_resets_at}` (RFC 3339, machine-
  readable), `{rate_limit_scope}` (`5h` / `7d` / `unknown` / `-`).
- `RateLimitScope` and `RateLimitSource` enums on the public API.
  `Agent` gains `rate_limit_5h_pct`, `rate_limit_5h_resets_at`,
  `rate_limit_7d_pct`, `rate_limit_7d_resets_at`, `rate_limited_until`,
  `rate_limit_scope`, `rate_limit_source`. `AgentEvent::RateLimited`
  is a new variant; `AgentEvent::Heartbeat` gains optional rate-limit
  fields. All additions are serde-additive — old peers and on-disk
  `state.json` keep loading without a `STATE_SCHEMA_VERSION` bump.

### Changed

- `apply_heartbeat` now derives a soft cap when statusline utilisation
  hits 100% on either window, and auto-clears it the moment the next
  heartbeat reports utilisation back below saturation. Hard caps
  (`StopFailure` / `Transcript`) persist until the next `Started` or
  a successful `TurnStopped` (a successful turn is empirical proof
  the cap isn't blocking).
- `TurnStopped` with a captured response now lifts the row out of
  `Error` state and clears any active cap. Previously a transient
  429 followed by a successful retry left the row stuck red until a
  full session restart.

## [0.3.0] - 2026-04-29

First **beta** release. Pan-agent observability across Claude Code,
Codex, and Gemini CLI is feature-complete; APIs are stabilizing but
minor breaking changes are still possible until 1.0. opencode
integration deferred — see [#14](https://github.com/Open330/muxa/issues/14).

### Added

- `tracing::instrument` + structured `debug!`/`trace!` events on hot paths
  (`Store::apply`, snapshot writes, reconciler ticks, IPC handlers, SSE
  fanout). All fields are structured (no `format!` in hot paths).
- `GET /api/metrics` JSON endpoint exposing agent counts, event/snapshot/
  reconcile counters, and SSE subscriber count. Lock-free atomics in
  `AppState`. Reuses the existing dashboard auth middleware. JSON shape
  unstable until 1.0; `#[doc(hidden)]` at the crate root.
- `events_received_per_sec_1m` deliberately omitted from the metrics
  payload — accurate 1-minute rates need a ring buffer; operators can
  compute rates from successive scrapes instead.
- **Zellij CLI baseline** (`feat/zellij` branch). muxa now runs against
  zellij as a first-class host alongside tmux. The CLI baseline (no
  plugin install required) supports `muxa status`, `muxa watch`,
  `muxa hook`, `muxa recap`, and Enter-to-jump via
  `zellij action focus-pane-with-id`. Pane-classification discovery,
  live pane preview, and hook ancestry walks are gated off via
  `BackendCaps` until the WASM plugin lands in `feat/zellij-plugin`.
- **`PaneBackend` trait + `Arc<dyn>` plumbing.** `crate::backend` now
  hosts `PaneBackend`, `BackendCaps`, `HostKind`, `TmuxBackend`,
  `ZellijBackend`, and a `default_backend()` constructor. Reconciler,
  discovery, hook ancestry, watch refresh + live capture, daemon
  enrichment, and the read-side CLI commands all consult the shared
  backend instead of `crate::tmux::*` directly. `Arc<dyn PaneBackend>`
  is itself a `PaneBackend` so the daemon constructs one at startup
  and threads `.clone()`s without juggling lifetimes.
- **`MUXA_HOST=tmux|zellij`** env override. Wins over auto-detect from
  `$ZELLIJ` / `$TMUX` for nested-multiplexer setups.
- **Hook adapter reads `$ZELLIJ_PANE_ID`** in addition to `$TMUX_PANE`.
  Ancestry walk consults `caps().pane_pid_map` and skips on hosts
  where the lookup is structurally unsupported.
- **`examples/muxa.zellij.kdl`** layout starter for zellij users.
- `muxa watch` preview popup gains a content-axis toggle (`c`): flip
  between the agent's last prompt + last response and a live snapshot
  of the tmux pane itself, captured via `tmux capture-pane -ep` and
  rendered with ANSI colors preserved through `ansi-to-tui`. Same
  shape as tmux's `prefix + s` choose-tree preview. Re-captures on
  every refresh tick (debounced to ≤2 Hz) while the preview stays
  open. Geometry (popup ↔ fullscreen, `f`) and content (prompt ↔ live
  pane, `c`) are independent axes — both compose freely.
- `[watch.preview] default_content` config knob picks the overlay's
  first-paint shape: `"live_pane"` (default) or `"prompt_response"`.
  `c` still toggles in either direction at runtime regardless of the
  default. The flipped default lines `muxa watch` up with tmux's
  `prefix + s` first-impression — operators primarily using muxa as
  a session picker get the same instant visual context.
- `muxa::tmux::capture_pane(pane_id)` — minimal wrapper around
  `tmux capture-pane -ep -t <pane>` for callers that need the live
  pane contents with ANSI escapes intact.

### Changed

- `muxa watch` preview now opens straight into the live pane view
  instead of the prompt/response text view. The text view remains a
  one-keystroke toggle (`c`) away, and `[watch.preview]
  default_content = "prompt_response"` opts back into the previous
  default for users on text-focused workflows.
- Tighten public API surface for the upcoming beta. Internal task
  wiring (`Snapshotter`, `Reconciler`, discovery helpers) is now
  `#[doc(hidden)]` — still callable from the workspace bins, but
  excluded from rustdoc and treated as semver-exempt. The stable
  surface is `Config`, `Error`, `Agent`, `Store`/`SharedStore`,
  `Transition`, `ReconcileReport`, `PromptRecord`, `PromptHistory`,
  `HistoryEntry`, `AgentEvent`/`AgentId`/`AgentKind`/`AgentState`,
  `NotificationLevel`, plus the `PaneBackend`/`HostKind` family.
  `PROTOCOL_VERSION` / `HISTORY_SCHEMA_VERSION` / `STATE_SCHEMA_VERSION`
  remain pub but are documented as unstable wire formats.
- `Config::load` now validates dashboard bind / token / sink endpoint
  rules at load time and emits a clear error instead of crashing later
  during daemon startup. Dashboard-specific rules are gated behind a
  `validate_for_daemon()` so CLI commands like `muxa watch` and
  `muxa status` are unaffected by daemon-only misconfiguration.
- `[watch] columns` / `widths` / `[watch.detail] template` placeholders
  emit a `tracing::warn!` when unknown keys are encountered, instead
  of silently dropping them at render time.
- README and CLI now reflect that opencode integration is deferred —
  the four-adapter claim was inaccurate. Existing three-adapter coverage
  (Claude Code, Codex, Gemini CLI) is unchanged.

### Removed

- `reconcile::TmuxLiveness` (back-compat shim with no remaining
  callers). Backends are themselves `LivenessSource` via a blanket
  impl on `PaneBackend`.

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

[Unreleased]: https://github.com/Open330/muxa/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/Open330/muxa/releases/tag/v0.3.2
[0.3.1]: https://github.com/Open330/muxa/releases/tag/v0.3.1
[0.3.0]: https://github.com/Open330/muxa/releases/tag/v0.3.0
[0.2.0]: https://github.com/Open330/muxa/releases/tag/v0.2.0
[0.1.0]: https://github.com/Open330/muxa/releases/tag/v0.1.0
