# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **`muxa watch` prompt composer** — delay the submit key briefly after
  injecting prompt text into a tmux pane, so Codex treats `Enter` as a
  distinct submit key instead of folding it into a fast paste/input burst.
- **tmux shell-outs from `muxad` now find the binary, the right socket, and
  preserve their format separators.** Three independent failures piled on
  top of each other under macOS launchd:
  1. **Binary not on `PATH`.** launchd's gui-domain inherited
     `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, missing both `/opt/homebrew/bin`
     and `/usr/local/bin`. `Command::new("tmux")` then failed with "No
     such file or directory" before tmux ever ran.
  2. **Bare `tmux list-panes -a` hit the wrong default socket.** Once tmux
     could spawn, its default-socket lookup pointed at a temp dir the
     user's server wasn't bound to, so the daemon got exit 0 with zero
     rows.
  3. **POSIX locale corrupted format output.** With `LC_ALL` unset, tmux
     transliterated the literal TAB byte we used as a field separator —
     and every non-ASCII byte in `#{pane_title}` — to `_`, so the bytes
     that did come back parsed to zero rows.

  Each step lined up so the reconciler ran with `live_panes = []`, reaped
  every paned agent on every 30 s tick, and lost the `last_prompt` /
  `started_at` set by the next inbound hook event.

  Resolved by adding two helpers in `muxa::tmux`:
  - `tmux_binary()` resolves the binary once per process via `$PATH`
    probe + Homebrew fallbacks.
  - `tmux_command()` returns a `Command` pre-configured with the resolved
    binary and `LC_ALL=en_US.UTF-8`.

  `tmux::list_panes()` now enumerates the known socket dirs and aggregates
  rows across every server it can reach, instead of relying on tmux's
  default-socket lookup. Every tmux shell-out site across `muxad`,
  `muxa-cli`, and the library now routes through `tmux_command()`.

### Changed

- **Default `muxa status` / `muxa watch` columns tightened to NAME / ST /
  ACT / LAST PROMPT.** `KIND` and `MODEL` are no longer in the default
  view — operators get one row that leads with identity, then state, age,
  and content. Both columns remain available via `[watch] columns` for
  users who want them back.

## [0.7.0] - 2026-06-02

### Added

- **`muxa watch` prompt composer** — pressing Enter on a selected row now
  opens a compact bottom input bar for sending a prompt directly to that
  agent's tmux pane. A second Enter on an empty composer keeps the old
  attach behavior, so operators can prompt first or attach intentionally.
  The same Enter prompt path works from preview mode, including the live
  pane preview.
- **Terminal themes** — `classic`, `oh-my-muxa`, `focus`, `ops`, `mono`,
  `high-contrast`, and `minimal` presets are now available. `muxa watch`
  themes cover TUI chrome, footer hints, selection, and state markers.
  `muxa status`, `muxa stats`, and `muxa activity` also accept
  `--theme <THEME>` for styled table output.
- **Shared `[ui] theme` config** — human-facing terminal output can now
  share one visual default from `[ui] theme`. `muxa watch` inherits it
  unless `[watch] theme` is set, preserving a watch-only override when
  desired.

## [0.6.0] - 2026-06-01

### Changed

- **`muxa watch` now defaults to session view** (`[watch] view`'s default
  flipped `pane` → `session`). One row per tmux session — collapsing the
  panes in a session onto its most-recently-active agent — is the
  fleet-at-a-glance shape for operators juggling many sessions, and surfaces
  the `DUR` (session foreground-time) column by default. The finer-grained
  one-row-per-agent view is still a flag/config away: `muxa watch --view
  pane` or `[watch] view = "pane"`. No wire/protocol change — purely the
  default value of a CLI-side setting; configs that pin `view` explicitly
  are unaffected.
- **`PROTOCOL_VERSION` bumped 1 → 2.** Adding new variants to wire-visible
  enums (`AgentState`, `NotificationLevel`) is breaking because serde
  rejects unknown variant values on deserialize — an old client receiving
  `waiting_choice` from a new daemon (or vice versa) would crash rather
  than ignore. The daemon `chmod`s the new protocol on its socket and
  rejects mismatched requests with `{"ok":false,"error":"protocol mismatch: …"}`.
  **`muxad` and `muxa` (the CLI) must upgrade together.** Existing adapters
  that pin `protocol: 1` keep working only against a v1 daemon.
- **`Transition.agent` is now `Arc<Agent>`** (was `Agent`). The in-process
  `tokio::sync::broadcast` channel that fans out state-change events
  to the notifier task, in-process sinks, and every live `muxa watch`
  IPC subscriber clones the payload once per subscriber per `recv()`.
  With the post-v0.5.0 dashboard SSE handler holding a long-lived
  subscription on top of the existing notifier + watch CLIs, an `Agent`
  carrying ~4 KB of `last_prompt` and ~4 KB of `last_response` was
  showing up as a meaningful fraction of `Store::apply` wall time
  under modest fanout (≥ 4 subscribers). Wrapping the agent in an
  `Arc` keeps the per-subscriber cost a refcount bump instead of an
  `Agent`-sized allocation + memcpy, and shrinks `Store::apply`
  end-to-end time by ~30% at N = 16 subscribers on the
  `crates/muxa/benches/store_apply.rs` microbenchmark.

  **Wire format unchanged.** `serde::Serialize` on `Arc<T>` is
  transparent (serializes as `T`), and `Arc::deserialize` always
  produces a fresh, single-strong `Arc<T>` — so newline-delimited JSON
  flowing over the unix socket between `muxad` and `muxa watch` /
  dashboard SSE looks identical on the wire. No protocol bump.
  Internal API consumers that match on `Transition.agent` will need
  to deref through the `Arc` (e.g., via `&*t.agent` or implicit
  `Deref`), which the in-tree consumers already do.
- Workspace `serde` now enables the `rc` feature so `Arc<T>` derives
  `Deserialize` automatically — required by the change above.

### Added

- **`activity.ndjson` duration ledger** — `muxad` now appends closed agent
  state intervals and closed tmux foreground intervals to a bounded
  `$XDG_DATA_HOME/muxa/activity.ndjson` file. `muxa stats` / `muxa report`
  read it for windowed `WORK`, `WAIT`, `ERR`, `TMUX`, and `BLOCK` columns,
  while falling back to legacy `session-activity.json` totals until the
  first foreground interval lands. Foreground intervals close on detach or
  when a tmux session disappears, so duration remains reportable after the
  session is gone.
- Prompt history and agent state intervals now persist the observed tmux
  session name alongside the pane/session id, so new `muxa stats --group-by
  session` rows keep the human-readable session name even after tmux deletes
  the live session.
- **`AgentState::waiting_choice` + `NotificationLevel::needs_choice`** —
  menu-style user blocks (Claude Code's `AskUserQuestion` and
  `ExitPlanMode`) now land in `waiting_choice` instead of being lumped
  in with free-text `waiting_input`. The operator can tell at a glance
  "pick an option" vs "type a reply / approve permission". The Claude
  adapter routes both `AskUserQuestion` and `ExitPlanMode` `PreToolUse`
  hooks through `needs_choice`; the matching `PostToolUse` →
  `ToolCompleted` recovers `waiting_choice` → `working` (same recovery
  path that already existed for `waiting_input`). Carries through to
  the notifier (still wakes you), webhook sink (`default_on_states`
  includes both; tag `"needs choice"`, glyph `?`), dashboard
  `/metrics` (counted as its own bucket), the reconciler stuck-state
  sweeper (uses the same `stuck_waiting_timeout`), and the `muxa
  watch` TUI (`LightYellow` next to `WaitingInput`'s `Yellow`). See
  PROTOCOL.md "v1 → v2" for the wire-format delta.
- **`muxa attend`** (alias `go`) — jump straight to the agent that needs
  you. The daemon already knows which agents are blocked on a human
  (`WaitingInput` / `WaitingChoice` / `Error`); this turns that into one
  action. A bare `muxa attend` focuses the pane that's been blocked
  longest (oldest `state_entered_at`); `--cycle` rotates to the next
  blocked pane *after* the current one in `session:window.pane` order,
  wrapping — meant to be bound to a tmux key
  (`bind-key a run-shell "muxa attend --cycle"`) so you can tab through
  everything waiting on you. `--list` prints the ranked queue (glyph,
  location, kind, how long blocked, last-prompt snippet) without jumping.
  Reuses the same `jump_to_pane` machinery as the `muxa watch` Enter
  action, so the tmux/zellij and inside/outside-multiplexer cases are
  handled identically; agents with no pane are skipped (nothing to focus).
  No protocol change — it's a pure CLI consumer of the existing
  `snapshot` query.
- **`muxa watch` quick actions** — the picker is no longer read-only.
  Four new keybindings act on the currently-selected row:
  - `c` — copy the agent's last prompt to the system clipboard
    (`pbcopy` / `wl-copy` / `xclip` in order; falls back to a
    `/tmp/muxa-clip-<ts>.txt` dump with a hint when none are
    available).
  - Capital `K` — kill the pane via `tmux kill-pane -t <pane_id>`,
    behind a y/N confirm popup. Shift is required so a fat-fingered
    `k` (which moves the cursor up) can't blow up a pane.
  - Capital `R` — abort the agent's current turn by sending Ctrl-C
    to the pane, also behind a confirm popup. Documented as "abort
    current turn" rather than "restart" because we don't reliably
    know the original launch command across the Claude / Codex /
    Gemini wrappers.
  - `?` — toggle a help overlay listing every binding (existing +
    new). The default footer also gains a `?  help` hint so the
    overlay is discoverable.

  Confirm popups default to "No" (only `y` / `Y` / Enter accept;
  Esc / Tab / arrows / any other key cancels). Action results land
  in a transient ~2 s footer hint (`✔ killed pane main:2.0` /
  `✗ kill-pane failed: …` / `✔ copied prompt via pbcopy`). Disabled
  actions (e.g. `K` on a paneless row, `c` on a row with no last
  prompt) surface a one-line "not applicable" hint instead of
  silently doing nothing.

- **Slack/Discord webhook sink** for state-transition push
  notifications. New `[sinks.webhook]` config table with
  `enabled` / `endpoint` / `endpoint_env` / `flavor` / `on_states`
  / `rate_limit_secs` keys. Defaults forward only `WaitingInput`
  and `Error` transitions — most agent transitions are routine
  `Idle ↔ Working` and would spam. Auto-detects Slack
  (`hooks.slack.com` → `{"text": "..."}`) and Discord
  (`discord.com/api/webhooks` → `{"content": "..."}`) from the
  URL; falls back to a generic flavor that posts the full
  `Transition` JSON. Per-`(kind, session_id, state)` in-task
  rate-limit (default 60s) prevents flapping permission loops
  from paging the operator 30 times a minute. Best-effort by
  design — failed POSTs log at WARN and drop, no on-disk queue,
  no retry backoff. The webhook URL is the secret on Slack and
  Discord, so prefer `endpoint_env` over inline TOML.
- **`muxa logs`** — tail muxad's stdout/stderr without remembering
  `/tmp/muxad.log` and `/tmp/muxad.err`. Default streams the last 30
  lines of both files (configurable via `-n/--lines`) then follows
  `tail -f`-style until Ctrl-C. Flags: `-N/--no-follow` for one-shot
  output, `--err-only` to skip the stdout log, and `--filter <substr>`
  for case-insensitive substring filtering. Lines containing `ERROR`
  or `panic` render red, `WARN` yellow, when stdout is a TTY (honors
  `NO_COLOR`). On Linux hosts where the systemd user unit handles
  muxad — and so logs go to journald rather than `/tmp` — falls back
  to `journalctl --user -u muxad`, printing the exact command in the
  header so users can run it themselves.
- **`muxa upgrade`** — one-command source-build update flow.
  Walks up from the cwd to find the muxa source repo, then runs
  `git pull` → `cargo install --path crates/muxad --locked --force`
  → `cargo install --path crates/muxa-cli --locked --force` →
  daemon restart (`launchctl kickstart -k` on macOS,
  `systemctl --user restart muxad` on Linux, falling back to
  `pkill -USR1` and a `nohup` respawn) → IPC socket probe to
  verify the new daemon is responsive. Flags: `--no-pull` builds
  from the current HEAD, `--no-restart` skips touching the daemon,
  `--dry-run` prints the plan without doing anything.

### Fixed

- **`muxa watch`'s `c` (copy prompt) action now works in headless
  / remote / tmux sessions.** Previously the priority list
  (`pbcopy` → `wl-copy` → `xclip`) treated `xclip` as viable
  whenever the binary was on PATH — which surfaced
  `xclip exited with 1` on SSH hosts where `$DISPLAY` is unset.
  Two changes:
  - **Priority list now leads with `tmux load-buffer`** when
    `$TMUX` is set. Lands the prompt in tmux's paste buffer
    (`prefix + ]`); on tmux 3.2+ with `set -g set-clipboard on`
    it also forwards via OSC 52 to the host terminal's clipboard
    — single backend covers most of the "remote dev over SSH"
    case.
  - **Pre-flight env checks + cascade on failure**: `wl-copy`
    requires `$WAYLAND_DISPLAY`, `xclip` / `xsel` require
    `$DISPLAY`. If any backend fails (NotFound or Failed) we move
    to the next one instead of surfacing the failure. The
    `/tmp/muxa-clip-<ts>.txt` fallback at the bottom is the
    safety net.
  - Added `xsel` as another X11 candidate for users who don't
    install xclip.
- **`muxa watch` no longer redraws every row on every transition.**
  v0.5.0's push-based subscribe was only used as a "wake" signal:
  each Transition triggered a fresh `client.snapshot()` call and the
  watch replaced its entire agent list. Even though only one row had
  changed on the daemon side, every row's STATE / LAST PROMPT /
  model / cost values were rewritten — visually indistinguishable
  from constant churn. The user perceived this as "all columns
  updating even when nothing should be changing".

  `RefreshOutcome` is now an enum: `Full(FullRefresh)` for the
  periodic 5 s sync (and the priming wake from `run`), and
  `SingleAgent(Agent)` for push deliveries. The subscribe arm in
  `refresh_task` ships the `Transition.agent` payload directly via
  `SingleAgent` instead of triggering a snapshot. `apply_outcome`
  finds the matching `(kind, session_id)` row and replaces just
  that one entry — every other row keeps its prior bytes. New rows
  whose `session_id` we haven't seen are appended; the next `Full`
  tick handles sort order. The fallback poll still catches up after
  `Lagged` drops on the broadcast.
- **STATE no longer gets stuck on `Starting` (cyan) for agents whose
  first event doesn't carry an explicit transition.** v0.5.0
  introduced row creation via `or_insert_with(Agent::new)` in
  `Store::apply` — a fresh row defaults to `state = Starting` and
  most events flip it explicitly (`Started → Idle`,
  `PromptSubmitted → Working`, etc.), but `Heartbeat`,
  `ToolCompleted`, and (occasionally) `RateLimited` carry only side-
  effect data with no transition. If one of those was the *first*
  event for a session — most commonly Claude's statusLine
  Heartbeat landing on a synthetic discovery placeholder — the row
  stayed cyan indefinitely. `mutate_for_event` now ends with a
  catch-all: any event for a `Starting` agent promotes to `Idle`.
  Synthetic placeholders that have *never* received a hook event
  remain `Starting` (the catch-all only fires once an event lands)
  and explicit transitions (Working/WaitingInput/Error/Stopped)
  still win because they leave the state non-`Starting` before the
  catch-all runs.


- **`muxa watch` no longer flickers the STATE column through
  `Starting` for steady-state rows.** With v0.5.0's push-based
  `Subscribe`, every transition triggers a fresh snapshot fetch; if
  any of those snapshots momentarily contained a `Starting`
  placeholder for an `(kind, session_id)` we already knew was in a
  steady state (`Working` / `Idle` / `WaitingInput` / `Error` /
  `Stopped`), the row briefly repainted cyan before settling back.
  `apply_outcome` now keeps the previously-known steady state for
  rows where the snapshot's `Starting` would otherwise overwrite
  one — brand-new rows still come through as `Starting`, and real
  daemon-driven transitions still propagate.

- **STATE column now reads `WaitingInput` while a Claude
  `AskUserQuestion` menu is open**, not `Working`. The Claude
  adapter routes `PreToolUse` for known user-blocking tools
  (`AskUserQuestion`, `ExitPlanMode`) through
  `NotificationFired { NeedsInput }` instead of `ToolStarted`, so
  the row turns yellow while the operator is being asked something.
  The matching `PostToolUse` lands as `ToolCompleted` and the
  state machine flips back (see below).
- **Tool activity recovers a row from `WaitingInput`** instead of
  leaving it stuck. `mutate_for_event` now flips
  `WaitingInput → Working` on either `ToolStarted` (Codex resuming
  after a permission grant — the next tool fires) or
  `ToolCompleted` (the matching post-hook for the
  `AskUserQuestion` case above). Auto-recovers Codex's
  permission-grant gap without needing the timeout sweep.
  `Error` state is preserved through `ToolStarted` so a real
  failure isn't masked.

- **`muxa watch` paints its first frame instantly** instead of
  blocking on the priming snapshot (~50–100 ms tmux + IPC) and the
  v0.5.0 streaming-subscribe ack (~5 ms). Reordered `run()` to draw
  the empty table scaffold immediately after terminal setup and
  defer both compute_refresh and `subscribe()` to the background
  refresh task, with an immediate wake to force the first real
  refresh. The total time-to-data is unchanged; the perceived popup
  latency drops from "blank for ~80 ms then content" to "scaffold
  instant, rows fill in within ~80 ms".

### Added

- **`muxa doctor`** — end-to-end diagnostic command. Answers "is muxa
  working correctly?" without making the user know paths or
  service-manager incantations. Six checks render as cliclack-styled
  lines with `✔ / ✗ / ⚠` glyphs and actionable hints: muxad IPC
  responsiveness (1.5 s timeout), service-manager loaded state
  (`launchctl print` on macOS, `systemctl --user is-active` on
  Linux), unit/plist file present on disk, per-agent hook entry in
  Claude/Codex/Gemini settings, tmux marker blocks (popup +
  statusline) in `~/.tmux.conf`, and recent ERROR/panic lines in
  `/tmp/muxad.err`. Exits 0 regardless of failures so it composes in
  scripts; the summary footer counts issues and points at
  `muxa init` / `muxa upgrade`.
- **`stuck-WaitingInput` sweep** (Codex permission-grant recovery).
  New config knob `[reconciler] stuck_waiting_timeout_secs` with
  the same shape as `stuck_working_timeout_secs`. Specifically
  fixes Codex's hook-surface gap: `permission_request` flips a row
  to `WaitingInput`, the user grants permission, Codex resumes —
  but Codex never fires another hook, so the row stays yellow
  indefinitely. With `stuck_waiting_timeout_secs = 600` the
  reconciler recovers it after 10 min of inactivity. Default `0`
  (disabled).
- `Store::mark_stuck_idle` was generalized to
  `mark_stuck_idle_from(state, threshold)` so the reconciler runs
  one pass for `Working` and another for `WaitingInput`, with
  independent thresholds. `Reconciler::with_stuck_waiting_timeout()`
  is the matching builder method.

## [0.5.0] - 2026-05-04

### Added

- **`muxa watch` is now push-based** instead of polling. New
  streaming `Subscribe` IPC RPC: client opens a long-lived unix-
  socket connection, daemon writes one JSON-encoded `Transition` per
  state change. The TUI's background refresh task races the
  subscription stream against a much slower fallback poll
  (`STREAMING_FALLBACK_INTERVAL = 5s`, was 500 ms unconditionally).
  State changes now hit the screen in ~milliseconds; idle CPU drops
  to effectively zero. The fallback poll handles `Lagged` drops on
  the broadcast and reconnects after daemon restarts. Falls back
  cleanly to historical 500 ms polling against an old daemon that
  doesn't speak the streaming variant.
- **`Reconciler` stuck-Working sweep**: new
  `[reconciler] stuck_working_timeout_secs = N` config (default `0`,
  disabled). When non-zero, every reconciler tick auto-flips agents
  whose `state == Working` and whose `last_activity_at` is older
  than the threshold to `Idle`. Insurance against missed
  `Stop`/`TurnStopped` hook firings — without it a single dropped
  hook would leave a row glowing green forever. Each flip emits a
  synthetic `Transition` so subscribed `muxa watch` instances see
  the correction live. Off by default to preserve the historical
  "state changes only on explicit events" guarantee; opt-in to a
  reasonable value (`300` for 5 min) for interactive use.

### Changed

- `Transition` (was `Serialize`-only) is now also `Deserialize`. The
  type is on the IPC wire as a result of the streaming subscribe
  RPC. In-process consumers (sinks, notifier) are unaffected.
- `lib::Client::subscribe()` (new method) returns `TransitionStream`,
  a small async handle whose `recv()` yields the next `Transition`.

## [0.4.5] - 2026-05-02

### Fixed

- **`muxa init` no longer hits `Bootstrap failed: 5: Input/output
  error` on re-run against an already-installed launchd agent.**
  Two changes:
  - `launchctl bootstrap` immediately after `bootout` raced launchd's
    teardown — the previous agent's state wasn't fully reaped before
    we tried to re-register, and launchd returned EIO. Added a
    1500 ms sleep between bootout and bootstrap. The fresh-install
    path (no prior bootout) doesn't hit the sleep at all.
  - Fast-path skip when the agent is already loaded (`launchctl
    print` succeeds): just `kickstart -k` to pick up any binary path
    changes and return. Avoids the teardown-then-rebuild cycle on
    every `muxa init` re-run, and side-steps the EIO race entirely
    for the common idempotent case. Reported by a user re-running
    the wizard on a machine where v0.4.2 had already wired the
    plist.

## [0.4.4] - 2026-05-02

### Fixed

- **`muxa init` no longer races the service manager's spawn** — the
  `--start-daemon` action is now suppressed when `muxad-systemd` or
  `muxad-launchd` is in the plan. Previously both fired on every
  install and the wizard's direct `nohup muxad &` won the socket-bind
  race against the manager's child, producing an orphan muxad that
  *worked* immediately but wasn't supervised — `pkill -9 muxad` left
  it gone with no auto-restart, and `launchctl print` showed
  `active count = 0 / state = spawn scheduled`. The shellrc-only path
  (where no real manager owns the lifecycle) still gets the
  start-daemon action so `muxa init` leaves muxad responsive in the
  same session.
- **Pre-flight label** — `muxad already running` rendered with a `·`
  bullet even when muxad was *not* running, which read as a
  contradiction. Replaced with state-dependent text:
  `✔ muxad responding` vs `· muxad not running (will be started on apply)`.

## [0.4.3] - 2026-04-30

### Fixed

- **`muxa init` daemon-liveness check now probes the IPC socket
  instead of `pgrep`** (#20). — both pre-flight ("muxad already running"
  green tick) and the `--start-daemon` action's "is it already up?"
  short-circuit. The pgrep approach was misleading after a v0.4.0
  incident where a stale muxad pid lingered with its socket gone:
  pgrep said "running", we skipped the spawn, and the user's next
  `muxa status` still failed with `daemon not reachable`. Socket-
  connect captures the only thing that actually matters — "is the
  daemon answering" — and a true cold-start errors in microseconds.
- **Replaced the static 300 ms post-spawn sleep with bounded
  polling** (3 s timeout, 20 ms interval). Slow VMs / CI runners
  used to race muxad's socket bind and surface a misleading "muxad
  not responding" warning right after a successful spawn; the poll
  loop adapts.
- **`locate_muxad` (macOS launchd plist) now also checks
  `/opt/homebrew/bin/muxad` and `/usr/local/bin/muxad`** so a
  brew-installed muxad lands at the correct path on first install.
  Cargo path stays first.

### Internal

- New `init::util` module deduplicates `uid_string()` (was in both
  `detect.rs` and `files/launchd.rs`) and hosts the new
  `default_muxad_socket()` / `muxad_responsive()` /
  `wait_for_muxad()` helpers. Six new unit tests cover the polling
  helper end-to-end against a real `UnixListener`.

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

[Unreleased]: https://github.com/Open330/muxa/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/Open330/muxa/releases/tag/v0.6.0
[0.5.0]: https://github.com/Open330/muxa/releases/tag/v0.5.0
[0.4.5]: https://github.com/Open330/muxa/releases/tag/v0.4.5
[0.4.4]: https://github.com/Open330/muxa/releases/tag/v0.4.4
[0.4.3]: https://github.com/Open330/muxa/releases/tag/v0.4.3
[0.4.2]: https://github.com/Open330/muxa/releases/tag/v0.4.2
[0.4.1]: https://github.com/Open330/muxa/releases/tag/v0.4.1
[0.4.0]: https://github.com/Open330/muxa/releases/tag/v0.4.0
[0.3.2]: https://github.com/Open330/muxa/releases/tag/v0.3.2
[0.3.1]: https://github.com/Open330/muxa/releases/tag/v0.3.1
[0.3.0]: https://github.com/Open330/muxa/releases/tag/v0.3.0
[0.2.0]: https://github.com/Open330/muxa/releases/tag/v0.2.0
[0.1.0]: https://github.com/Open330/muxa/releases/tag/v0.1.0
