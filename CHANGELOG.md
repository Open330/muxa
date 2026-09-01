# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **muxad re-execs onto a new binary on its own.** Installing muxa never
  restarted the daemon: the package manager writes the new build and repoints
  `PATH`, the running process keeps its open inode, and no service manager
  intervenes because `KeepAlive` and `Restart=always` react to a process
  *exiting*. On a live host that left a six-day-old build serving, answering
  `protocol mismatch: server=4 client=6` to every call until it was killed by
  hand.

  muxad now watches the path it would re-exec through — `argv[0]`, followed
  through symlinks, since on a Homebrew install the upgrade happens at the far
  end of the `bin/` link — and re-execs when that path resolves to a different
  file on two consecutive polls. The second poll is what keeps a half-finished
  install from being adopted: an upgrade writes a temporary file, renames it,
  and repoints a symlink, and a poll landing between those steps sees a file
  that is real and about to be replaced again. Identity is device, inode, size
  and mtime, so a write-in-place install that reuses the inode is still seen.

  The daemon replaces itself in place rather than exiting for a supervisor to
  catch, so this behaves the same under launchd, systemd, or a bare terminal,
  and a failed re-exec leaves the old image running rather than a hole where
  the daemon was. `[daemon] restart_on_new_binary = false` turns it off for
  deploys that own their own restart ordering; `binary_poll_secs` (default 30)
  sets the cadence.

- **The Homebrew formula says what an upgrade does not do.** Its `caveats`
  now note that a running muxad is not replaced by the install, name the
  30-second window the daemon takes to pick the new build up, and give
  `muxa daemon restart` for anyone who wants it now.

### Fixed

- **A daemon left running across an upgrade now says so, and can be fixed
  where you read about it.** `brew upgrade` (and `cargo install`) replace the
  binary on disk but never the process already running on it: launchd's
  `KeepAlive` restarts muxad when it *exits*, and nothing about a swapped
  Cellar symlink makes it exit. Measured on a live host: a daemon started six
  days before an upgrade kept serving from the old build, answering `protocol
  mismatch: server=4 client=6` to every CLI call, and the MCP server refused to
  connect at all.

  Three things closed the gap. `hello` now carries the daemon's crate version,
  so `muxa doctor` and `muxa watch` can report skew *before* it turns into a
  failed request — the window where both halves agree on the protocol and
  disagree on everything else was previously silent. `muxa watch` promotes the
  skew to a bar naming both versions and a `:daemon restart` command that fixes
  it without leaving the TUI, instead of clipping a bare `protocol mismatch` at
  terminal width with no remedy attached. And the remedy itself is now
  `muxa daemon restart` rather than `muxa upgrade`, which was actively wrong on
  a Homebrew install: `muxa upgrade` is `git pull` + `cargo install`, writing to
  `~/.cargo/bin`, which the Homebrew prefix shadows on `PATH` — it built a
  binary the user never executed and left the daemon exactly as stale.

  `hello` also steps down to the protocol a refusing daemon names, and `restart`
  and `stop` are sent unversioned. Without this the fix was unreachable from the
  problem: `muxa daemon restart` opens with a `hello`, which the stale daemon
  refused, so the one command that resolved the skew could not be run against a
  daemon skewed far enough to need it.

- **Selecting two components that share a file no longer throws one of them
  away.** Each component planned its edit by reading the file from disk, which
  is right for the first one to touch a path and wrong for every one after it:
  `apply` writes each result in turn, so the second edit — computed from text
  that had never seen the first one's block — silently overwrote it. Nothing
  looked wrong, because every component's own plan was accurate: the dry-run
  reported `~ edit ~/.tmux.conf (tmux-popup) [+9 lines]`, the user confirmed
  it, and the block was not there afterwards. A stale `prefix + s` binding
  could survive any number of reinstalls that way, and `doctor` then blamed a
  stray `bind-key` that did not exist.

  `~/.tmux.conf` is the reported case (`tmux-popup` with `tmux-statusline`,
  which `--preset standard` selects together), and `config.toml` has the same
  shape: `ask`, `collaboration` and `dashboard` each own one table in it. Every
  planner now folds onto what the plan already holds for that path — which
  `plan_tmux_env` was alone in doing — and records that as its `before`, so the
  backup `apply` takes is the file as it stood a moment earlier rather than as
  it stood before the whole run.

## [0.8.39] - 2026-08-31

### Fixed

- **A reply must carry a body.** `create` refuses an empty request body;
  `reply` refused nothing, so an argument that did not survive its shell closed
  the request with an empty answer in it — and, the request now being terminal,
  the real answer could never be posted to that thread. The sender saw
  `completed` with an empty body, which reads as a reviewer who had nothing to
  say rather than as a delivery that went missing. A blank or whitespace-only
  reply is now `EmptyMessage`, leaving the request claimed and still
  answerable, and the stored body is trimmed the way a request body already
  was. This holds for every terminal status: a decline with no reason tells the
  sender as little as an empty completion does.

- **A release syncs the Homebrew tap again.** `tap-bump` listens for
  `release: published`, which worked while a human published each draft and
  stopped the moment the release workflow started publishing for itself:
  GitHub does not start workflows from events raised with the automatic
  `GITHUB_TOKEN`. v0.8.38 published cleanly and left the formula on v0.8.37,
  with nothing failing anywhere to say so — the worst shape a break can take.
  The release run now *calls* `tap-bump` as a reusable workflow once the
  publish job succeeds. The event trigger stays for releases published by
  hand, and the two cannot double up, since the automated publish raises no
  event to begin with.


## [0.8.38] - 2026-08-31

### Fixed

- **`--placement window` no longer collides with a window named after its own
  session.** Adding a window to an existing session failed with `create window
  failed: index 0 in use`, and removing every other window did not help. Muxa
  resolved a pane/window/session target to the owning session's *name* and
  passed it to `tmux new-window -t`, which takes a **window** target: a string
  with no colon is looked up as a window in the caller's current session before
  it is tried as a session. Muxa's own topology makes that collision the common
  case rather than a corner — one session per workspace, whose first window is
  usually named after it — so `-t junia` found the window `junia` at index 0 and
  refused to create anything there. The target is now the session *id* with a
  trailing colon (`$7:`), which cannot be read as a window name and leaves the
  index for tmux to choose.

  Reading the target had the same flaw with a quieter ending: asked plainly for
  `--target junia`, tmux answered with the id of whichever session owned a
  window named `junia` — the caller's — so the window was created there, in the
  wrong session, without an error. A name is now resolved as a session first
  (`junia:`) and falls back to the plain form, which is what a window-name
  target needs and the only form ids accept. A failing launch also names the
  resolved target,
  since the address tmux was given is not the one the caller typed. Covered by
  a regression test that drives the real binary against a private tmux server
  laid out that way, through all four addresses muxa accepts — session id,
  session name, window id, pane id.


### Added

- **MCP agents can call an explicitly authorized agent on any Fleet host.**
  `muxa_fleet_call_peer` sends a durable structured request to an explicit
  host and live pane; it never auto-selects or spawns remotely, defaults to
  review/read-only, and requires remote `mode = "control"`. Its bounded wait
  returns an exact `pane_key` for `muxa_fleet_wait_reply`, which retrieves the
  reply by durable request id even after the target pane exits. A new
  `collaboration_get` relay capability rejects mixed-version nodes before the
  controller sends an unsupported frame.
- **Collaboration history now has stable causal and Work identity.** Requests
  carry canonical thread/parent, workspace/Work/Run, artifact, and link
  metadata. Managed pane metadata fills missing Work identity; CLI and MCP
  callers can provide or override it. Parent links are validated in-room and
  cannot cross participant pairs or conflict with the parent thread.
- **History can be filtered and paged without loading it all into the web
  dashboard.** The indexed query supports time, Work/workspace, thread/parent,
  kind, status, and exact room filters plus newest-first keyset cursors. `muxa
  msg list` exposes compatible conjunctive filters with local `--offset` /
  `--limit`, preserving its legacy bare-array JSON and older-daemon behavior.
- **Collaboration is visual, in both terminal and browser.** Watch's collab
  screen toggles its table and chronological lifeline sequence with `v`,
  `:layout sequence`, `--collab-layout sequence`, or `[watch] collab_layout`.
  The web dashboard adds a cross-room participant graph, aggregated directional
  request/reply edges, room/edge sequence drill-down, filters, and load-more.

### Changed

- **Watch `M` follows hierarchy scope instead of collapsing parents to one
  pane.** A window shows its whole room and a session shows every room grouped
  by window. Aggregate views are read-only; pane history retains compose,
  claim, reply, and incoming/sent actions.
- **The durable mailbox is now indexed SQLite.** The historical
  `collaboration.json` path maps to `collaboration.sqlite3`; existing JSON is
  imported transactionally once and retained as a migration backup. Optional
  `retention_days` prunes only fully delivered terminal threads at daemon
  startup, never partial causal chains or live/unread state.

### Security

- **Dashboard collaboration details require strict token-auth mode.** In
  `public_read` and `none`, the API and graph retain topology/status metadata
  but redact request/reply bodies, provenance, paths, artifacts, links, and AIR
  references; supplying a PAT does not unredact a `public_read` server.
- SQLite, WAL, and shared-memory files are owner-only (`0600`). The retained
  legacy JSON remains a duplicate body copy outside retention; operators
  should archive or remove it under equivalent controls when rollback is no
  longer needed, and must not run an older daemon against it after migration.

## [0.8.37] - 2026-08-28

### Added

- **A spawned peer is addressable before its agent registers.**
  `muxa_call_peer` with `spawn_if_missing` waited for the new pane to become a
  participant before sending, which codex can never satisfy: it fires
  `SessionStart` when its first prompt is submitted, not when its TUI boots —
  measured at no hook through 116s of idling, then one 0.4s after the first
  prompt, against claude's 0.62s from launch. So the sender waited for a
  registration that the request it was holding would have caused, the call
  failed with advice to check hooks and retry, and the retry it named
  (`target="pane:%N"`) hard-errored as well — leaving only the `tmux
  capture-pane` polling this model exists to avoid.

  A request may now carry a *pending pane* recipient: muxad queues it
  immediately, delivers it once the pane reads idle, and the first agent
  session to register on that pane adopts it, after which the request is
  session-pinned like any other. The relaxation is deliberately narrow — only
  an explicit pane selector resolves this way, only within the sender's room,
  only for a pane muxa launched or discovery classified as an agent CLI, and
  only for providers that can report readiness at all, so a spawned `opencode`
  pane still fails fast rather than queueing work nothing would deliver. A
  pane's startup approval gate still blocks delivery, because the bundled
  screen manifests classify it as waiting rather than idle. `spawn_timeout_secs`
  becomes a grace period (default 10s) instead of a deadline.

- **The live tour's ending names the keys `muxa init` binds.** It taught
  `muxa watch` as something you type — true in the sandbox, where nothing is
  installed — and stopped there, so a learner who finished went on typing it
  instead of pressing `prefix + s`. The closing summary now lists `prefix + s`,
  `S`, `D`, `q` and `,` with what each opens, and a test holds that list
  against the bindings `muxa init` actually writes: a hand-kept copy of
  somebody else's keys goes stale the first time they change one, and this one
  tells the learner what their own machine will do.

  It also points at oh-my-zsh's `tmux` plugin for `ta` / `ts` / `tl`, rather
  than writing them into anyone's shell rc. That plugin builds them as
  functions with session-name completion; a plain alias would not have it, and
  ours would shadow the better version depending on load order.

### Fixed

- **An alias reserved with `--alias` reached the daemon the launch was talking
  to.** `mark_agent` registered the name against `paths::default_socket()`
  regardless of `--socket`, `MUXA_SOCKET`, or a socket set in config. Against
  any other daemon the name was taken in a room that knew nothing about the
  pane, while the room that owned it never heard — so its next minted handle
  could hand the same name to a second pane, and two panes answered to one
  alias. With no daemon on the default socket the reservation was skipped
  silently, which is the arbitration this call exists to perform.
  `StartRequest` carries the socket now, `Client::socket()` reports it, and
  `agent_start_carries_the_callers_socket` pins the wiring, and
  `scripts/alias-socket-check.py` — run by hand, not in CI — reserves `codex` in
  a sandbox and asserts the next pane mints `codex2`.

### Changed

- **The live sandbox is now the only `muxa onboard` tour and the default.** The
  20-step Rust and POSIX-shell simulations, their mock renderers, the hidden
  `--emit step-table` parity contract, and the parity/split-arrow drivers are
  gone. `muxa onboard --print` now follows the live tour's sixteen steps.
  `scripts/onboard.sh` remains a no-install entry point by downloading and
  verifying a temporary release binary, but no longer carries an offline
  fallback implementation.

- **`muxa msg send`, `reply`, and `cancel` print a one-line receipt instead of
  the whole stored request.** They dumped every field of the record — each
  timestamp, each `null` — which buries the two facts the caller wants (it went
  through, and whether an answer is coming) under thirty lines of JSON. `--json`
  still prints the record.

- **`muxa msg list` and `muxa msg inbox` show the reply.** They printed the
  request and its status and stopped there, so the sender saw their own question
  marked `Completed` with the answer they came for nowhere on screen.

- **Caller provenance is printed only when it is worth knowing.** Every
  ordinary request is `matched`, and stamping `[via cli pane=%2 pid=3874566
  matched]` on each of them turned a mailbox into a debug log. A `mismatched`
  or `unverifiable` origin still says so.

### Added

- **`muxa daemon start|stop|restart|status` owns the local daemon lifecycle.**
  The CLI resolves the selected socket from its normal flag, environment,
  config, and XDG precedence instead of making operators spell a uid-scoped
  `/tmp` path. Start is detached and idempotent, status requires a real IPC
  handshake, restart proves that the image generation advanced, and stop
  drains the daemon's durable writers through a new owner-socket IPC request.
  The shellrc autostart hook now calls `muxa daemon start --quiet`, so a test or
  onboarding daemon on another socket can no longer fool `pgrep -x muxad` into
  suppressing the user's daemon.

- **`muxa onboard --tour live` — the onboarding stops simulating muxa and
  becomes it.** Sixteen steps in two acts, against a sandbox on its own tmux
  server: Act I is real `tmux new-session`, a real second window, a real detach
  proving the work kept running, and a real reattach; Act II adds two scripted
  agents to the learner's own window and walks them through real `muxa watch`,
  `muxa attend`, `muxa msg send @claude` and `muxa msg inbox` over a real
  mailbox. The states are produced by feeding `muxad` the same hook payloads
  the agent CLIs send, so nothing on screen is drawn.

  Narration goes through tmux's own status rows rather than a pane or a popup:
  a narration pane would be split and zoomed by the very exercises Act I
  teaches, and `display-popup` is modal and does not expand `#{}` formats.
  Because the narration never owns the keyboard, no step can wait on a
  keypress — each one polls real tmux and real muxa state and advances when the
  learner has actually done the thing, which is why the tour is driven with
  real commands instead of a quiz. `scripts/live-tour-smoke.py` types those
  commands in CI and checks the tour keeps up.

  Each invocation now allocates its own private PID-scoped directory, daemon,
  and tmux socket. Concurrent tours cannot erase or reuse one another's state,
  and failed tmux, muxa, or hook commands stop with their actual error instead
  of leaving the learner at a step that can never advance.

  The sandbox is torn down on every exit path, including `Ctrl-C`.

  No step is a dead end. The sandbox server starts with `-f /dev/null`, so a
  learner who rebound their prefix is not told to press `Ctrl-b` and left
  stranded; any step offers `F12` to move past it after 45 seconds, and
  `--no-quiz` offers that from the first step; and skipping performs whatever
  the learner would have done, so Act II still has a session to put its agents
  in. The first step prints its instruction rather than painting it, because
  nobody is attached to a status bar yet. `F2` switches the narration language
  mid-tour.

  The scripted agents look like agents. Each pane tails a transcript the tour
  appends to, so the session *grows* — a prompt, tool calls, and, when codex
  goes to `waiting`, the approval prompt that explains why. `muxa watch`'s
  inspector and preview both render the selected pane's live screen, so a fleet
  parked on one static line makes those features look broken and makes the tour
  assert what it should be showing. Windows are named after the Work rather
  than after whatever is running in them, so the topology reads as
  `checkout · 3 agents` instead of `muxa` and `bash`. None of it shells out to
  an agent CLI, and each transcript says so on its first line.

  Sixteen steps, not nine, and one action each. Compressing them put two
  instructions on a line — "see both: Ctrl-b s · then leave: Ctrl-b d" — which
  reads as one and leaves the learner unsure which half registered. `Ctrl-b s`,
  `tmux ls`, `Ctrl-b ;` and `muxa msg list` are steps in their own right now;
  the learner's shell reports what it ran, so a step whose action is a command
  can be detected rather than bolted onto the end of another cue.

  The fleet stays on screen. The learner's pane was zoomed so `muxa watch`
  could have the whole screen, which hid the two agents that had just arrived —
  the step confirmed an arrival the learner could not see, above an instruction
  to look at the whole Work, over a blank pane. `main-vertical` gives them at
  least 80 columns on the left and stacks claude and codex on the right, which
  is both what watch needs and what the next four steps are about.

  The sandbox stopped reading the caller's `~/.tmux.conf`. It never passed
  `-f`, so the sandbox server loaded it — bindings, options and, the one that
  actually bit, hooks. muxa's own `tmux-auto-view`, on by default since
  v0.8.36, binds `client-attached` to hand an arriving client its own
  session-group view; inside the sandbox that fired the instant the learner
  ran `tmux attach`, moved them straight back out, and ended the tour at step
  six with `[detached (from session muxa-onboarding)]`. `--tmux-config` still
  supplies one deliberately; the default is `/dev/null`, and
  `scripts/config-isolation-check.py` holds it there.

  `exit` is a way out, not a crash. Typing it in the learner's pane closes the
  pane, and with it the last window, the session and the sandbox server the
  tour polls several times a second — which surfaced tmux's own `no server
  running on …` at somebody who had just typed `exit`. The loop treats a
  vanished server the way it treats `Ctrl-b d`, and `scripts/exit-check.py`
  holds it there in CI.

  The ending prints before the teardown rather than after it. Stopping a
  daemon and waiting for it to actually exit takes a couple of seconds, and the
  learner spent them looking at a blank terminal before the part worth reading
  arrived. Everything that does not depend on the teardown goes out first; the
  one line that claims the sandbox is gone waits until it is.

  The block printed when nobody is attached stopped fighting the shell. It was
  written beside a prompt bash had already drawn, which doubled
  `muxa-onboarding $` and ate characters out of the line under it. It clears
  the line first, carries a progress bar, and re-issues the prompt itself —
  and the prompt now shows a one-line reminder of the current step above it,
  through `PROMPT_COMMAND` rather than `PS1`, because `$(cat …)` strips the
  trailing newline and ran the reminder into the prompt.

  The tour ends by saying what to do next: the commands it taught, the parts it
  had no room for (`peek`, `stats`, `timeline`, `work up`, `doctor`), how to
  install muxa for real, and links to the README and the install guide in the
  reader's language.

  The learner presses Enter rather than typing `claude`. `claude` is neither
  tmux nor muxa — the two things the tour teaches — and the sandbox only
  pretends to have it, so asking for it taught a command that does not exist
  outside the tour. The step asks for Enter and says the tour is setting the
  practice agents up.

  `muxa watch` gets two steps instead of half of one, because it is the way
  into everything else: one to open it, one that names `j`/`k`, `h`/`l`,
  `Enter` and `?` and asks them to look around before `q`. Watch owns the
  keyboard while it runs, so leaving it is the only transition the tour can
  see — and it is the one worth gating on.

  Every pane says who it is on its own border, and says what just happened to
  it. `pane-border-status` plus `select-pane -T`, so the label is drawn by tmux
  in the border it was already drawing. The mailbox steps had been asking the
  learner to believe a message left one box and arrived in another, across
  three boxes with nothing to tell them apart.

  The scripted beats are named constants held to their steps by a test.
  Inserting a step upstream had twice moved what a bare integer pointed at
  without failing anything: the send-on-skip fixup fired one step early, so
  `F12` reached claude's inbox before anything was in it and the tour exited on
  the error.

  The learner starts the agent themselves. Splitting a pane is one step and
  typing `claude` in it is the next, because a pane that turns into an agent on
  its own teaches that panes become agents by magic. `claude` resolves to a
  shim on the sandbox `PATH`, so what comes up is the tour's screen and the
  real CLI is never invoked — and the pane the learner ran it in is the pane
  the hook registers, which is what turns "a pane is an agent" from a sentence
  into something they did.

  The tour runs in a workspace of its own. It has its own `HOME` and a
  `checkout-service` tree — the files the scripted agents say they are reading,
  so `cat crates/checkout/src/auth.rs` answers — and every pane starts there,
  so `ls` shows the practice project and `muxa watch` stops printing the
  learner's real path as an agent's cwd. This is a convincing workspace rather
  than a jail: `cd /` still works, because a real filesystem confinement needs
  bubblewrap or a mount namespace and neither is available unprivileged on
  every platform muxa runs on.

  Codex's approval prompt answers. A prompt reading `[y] yes  [n] no` that
  swallows the keystroke invites the learner to do the one thing the tour has
  made impossible; pressing `y` now appends the tool output and fires the hook
  a resuming agent fires, so the row in watch goes back to `working`. claude
  answers the learner's question through the mailbox rather than only on its
  own screen, straight away — a reply they can find with `muxa msg list` is
  what a durable mailbox is for.

  The steps say what their commands do: that `attend` goes to whichever agent
  has been blocked longest and lands you in its pane, and that `Ctrl-b ;`
  returns to the pane you were in before. The placeholder session is removed as
  soon as the learner has one of their own, so the `tmux ls` that step 3 asks
  for shows their session and not the tour's plumbing. And the step is part of
  the shell prompt while they are detached, instead of printed underneath a
  prompt bash had already drawn — which read as the shell having gone away.

  Every step opens by saying what the last action did — `✓ second window
  created, see both in the top row` — because a tour that only ever says what
  to do next leaves the learner typing commands and guessing whether any of
  them landed. tmux's own status row is left alone for the same reason: the
  narration had been painted over the window list, so pressing `Ctrl-b c`
  produced no visible result anywhere on screen.

- **`scripts/muxa-sandbox.sh` — a throwaway muxa that cannot reach the real
  one.** The isolation the demo recordings had grown privately is now a
  supported command: `up` / `daemon` / `env` / `status` / `down` over a private
  `MUXA_SOCKET`, `MUXA_CONFIG` and `XDG_DATA_HOME`, an isolated tmux server
  pinned through `MUXA_TMUX_SOCKET`, and a `tmux` PATH shim so child processes
  land on that server too. `up` refuses to nest inside an existing tmux session
  unless told otherwise, checks that `muxa` and `muxad` are the same build, and
  tears down anything a previous run left behind before building. `status`
  distinguishes healthy from partial and names every artifact it found; `down`
  reaps daemons the pidfile lost track of, waits for them to actually exit, then
  verifies and reports what survived. Every artifact lives below a mode-0700
  root carrying an ownership marker, so a same-named `/tmp` path is refused
  rather than deleted, and tmux uses an explicit socket path that stays stable
  across `TMUX_TMPDIR` changes. A pidfile is only trusted after its process is
  confirmed against this sandbox's exact config, while custom-named `muxad`
  binaries remain discoverable; starting the daemon again reuses the healthy
  process rather than creating a socket race.
  `scripts/sandbox-smoke.sh` holds it to that, asserting teardown is total from
  each state a crash can leave.
  `docs/demo-setup.sh` is the first consumer and now carries only the fixture.

### Changed

- **`scripts/onboard.sh` runs the real `muxa onboard` without installing it.**
  It fetches the release archive for the host into a temporary directory,
  verifies its published SHA-256, runs the live tour, and deletes the archive.
  Unsupported platforms, missing tools, and network failures now produce a
  clear error instead of entering a separately maintained shell tour.

### Fixed

- **The demo fixture could not be rebuilt.** `docs/demo-setup.sh` wrote
  `[watch] view = 'work'`, a value that stopped existing when the watch view
  enum became `session` / `window` / `pane`, so `muxad` refused the config and
  every GIF regeneration failed at daemon start.
- **Pasting into an attached muxa-owned PTY is safe and complete.** The attach
  relay now enables bracketed paste, forwards the restored framing to the child
  PTY so multiline input is not executed line by line, ignores leaked platform
  shortcut keys, and restores the parent terminal state on every exit path.
  Embedded bracket markers are removed before forwarding so clipboard control
  bytes cannot close the paste early and execute the remainder as keystrokes.

## [0.8.36] - 2026-08-27

### Added

- **Every terminal on a workspace gets its own current window.** New
  `tmux-auto-view` init component, on by default: two tmux hooks hand an
  arriving client its own session-group view, so two terminals on one workspace
  stop following each other's window switches. Both hooks matter —
  `client-attached` covers `tmux attach`, and `client-session-changed` covers
  `switch-client`, which is what `muxa watch`'s Enter does and what a terminal
  that was already open goes through. `muxa workspace view` does the work and
  can be run by hand; it is a no-op for a session's sole client and reuses one
  view per terminal, so nothing accumulates. Views are named
  `<session>~view~<pid>` and vanish on detach. Opt one session out with
  `tmux set-option -t <session> @no_auto_view 1`.
- **`R` renames the session, window, or pane under the cursor in `muxa
  watch`.** The form opens prefilled with the current name and cursor at the
  end, since renaming is usually an edit; Enter on an untouched prefill is a
  cancel rather than a write. Window renames go through muxa's naming policy —
  whitespace normalized to `-`, and a name already used in that session refused,
  because tmux matches `session:window` targets by prefix. A pane has no name in
  tmux, so that level sets the pane title, which is the string watch displays.
- **New `tmux-window-names` init component**, on by default: turns off tmux's
  `automatic-rename` and binds `prefix + ,` to `muxa window rename`. A window
  is a Work Run under muxa's model, so naming it after whatever process is
  running in it overwrites the Work with `node` or `claude` the moment an agent
  starts.
- **Minimum supported Rust is now 1.89.** Nothing in the tree requires it
  today; the floor moves so future work can reach for stabilized std APIs
  without the design bending around an old one. Raised while the project is
  young enough for the cost to be small.
- **The daemon arbitrates a room's handle namespace.** A room-local handle
  had three writers — the `@muxa_agent_alias` pane option, a launcher's
  explicit alias, and a registered identity — each enforcing its own rule
  against its own view, so every ordering between them produced a different
  way for one room to answer to `@claude` twice. The daemon is the only
  place that sees all three, so allocation now goes through it via
  `collaboration_issue_handle` (local IPC protocol 6, capability
  `handle_namespace_v1`), which also tracks handles promised to callers that
  have not written them yet. Lifetimes are unchanged: a handle still lives
  on the pane option and outlives the agent restarting in place, while a
  registered identity still belongs to one agent session. Without a daemon
  to referee, a pane stays unnamed rather than being named from a partial
  view.
- **Every agent pane gets a handle, so peers are addressable by name rather
  than by `%1242`.** The first agent of a runtime in a room becomes
  `@claude` / `@codex` / `@gemini` / `@agy` / `@opencode`, a second of the
  same kind becomes `@claude2`, and so on. muxa mints it on the session's
  first hook event — the one installed by `muxa init` — and immediately for
  a pane `muxa agent start` opened, which also reports it in `--json`. The
  name is stored on the pane as `@muxa_agent_alias` so the slot keeps it
  across muxad, CLI, and agent restarts, and a pane that already carries a
  pipeline or hand-set alias is never renamed. `resolve_target` has always
  understood `@alias`; what was missing was anything that minted one.
  Allocation runs under a per-room file lock, because the claim is a
  read-modify-write over a shared namespace and tmux user options have no
  compare-and-set — re-checking after the write cannot close that, since the
  pane writing second can finish its check before the first write lands. The
  claim itself goes through `set-option -o`, so a pipeline's explicit alias
  wins whichever side of it the minting lands on.

### Changed

- **Operator messages now carry their body directly by default, without making
  agent delegation equally permissive.** The new default `wake_payload =
  "operator_full"` claims and injects requests sent by the resolved operator
  console (watch/dashboard), while agent-originated MCP and CLI requests remain
  body-free mailbox notices. Explicit `notice` and `full` retain their strict
  all-notice and all-direct behavior. This is a delivery policy, not an
  authorization signal: `work_mode = "execute"` cannot promote an agent request.
- **Work pipelines now have daemon-owned, generation-aware Run state.** The
  selected pipeline, rendered desired aliases, dependency graph, Run
  generation, and each alias's `pending`/`running`/`blocked`/`done`/`failed`
  state persist in an owner-only store. `work done` is an atomic
  alias-generation event, automatically reconciles newly-ready downstream
  aliases, and rejects stale completions; restart/re-prompt invalidates the
  affected downstream closure. `work list` and `watch` render this durable
  state, including aliases that do not have panes yet. Local IPC protocol 5
  advertises this contract as `pipeline_runs_v1`.
- **Peer reply waits and idle wake delivery are event-driven.** Durable
  collaboration mutations now advance a retained mailbox revision that wakes
  every interested waiter. `muxa_wait_reply`, `muxa_call_peer(wait=true)`, and
  `muxa msg wait` use one bounded `collaboration_wait` IPC request instead of
  500 ms mailbox polling, while the daemon waker reacts to mailbox revisions
  and agent Idle transitions instead of a two-second scan. A slow 30-second
  reconcile remains only as a recovery backstop. MCP instructions explicitly
  steer agents away from `sleep`/raw `tmux capture-pane` monitoring loops.
  New clients fall back inside the same tool call when an older daemon rejects
  `collaboration_wait`, and confirmed peer spawning now arms the transition
  stream before pane creation instead of polling registration every 500 ms.
- **Route-owned environment preparation is accepted by every binary.** A
  `[[route]].prepare` command can provision the rendered `cwd` once before a
  new Work window is created, remains inert during `--dry-run`, refuses
  unresolved placeholders, and cannot be combined with `[route.worktree]`.
- **Multi-node Fleet watch now follows native watch interaction semantics.**
  `j`/`k` and arrows navigate siblings with singleton-parent fallback, while
  `J`/`K` keep the fast global agent-pane jump. The renderer now uses the same
  configured watch theme without Fleet-only hard-coded colors, and `muxa init`
  adds `prefix+S` for Fleet while retaining `prefix+s` for local watch.
- **Ask, durable messages, and mailboxes remain available when a remote host
  makes the Fleet hierarchy visible.** `a`/`A`, `m`, and `M` retain their
  native roles, including Ask agent switching, message kind/mode persistence,
  shared `/` skills with in-watch add/remove, and mailbox claim/reply. Remote
  collaboration stays owned by the selected node and travels over the existing
  exact-pane SSH relay behind an explicit capability gate.
- **`muxa peek` names each pane by its handle and its tmux id.** The
  `prefix + q` overlay now prints `@claude %1242` in every pane's header,
  next to the jump digit — the digit addresses the pane inside peek, but
  peer calls, `muxa send`, and raw tmux are addressed by the other two, and
  peek is where you are already looking to work out which pane is which. A
  narrow box gives up the pane id before the handle and the runtime name
  before either, dropping each rather than clipping it: a truncated
  `%1242` or `@claude2` is still a well-formed address for a different
  pane.

### Changed

- **`scripts/onboard.sh` now runs the real `muxa onboard`.** It fetches the
  release binary for the host into a temporary directory, verifies its
  published SHA-256, runs the onboarding, and deletes it — a download, not an
  install: no daemon, no config, no PATH entry. The embedded shell simulation
  remains the fallback for `--no-download`, an unsupported platform, a missing
  checksum tool, or no network, so the pipe-to-`sh` entry point keeps working
  offline. The fallback was realigned to the real tour's step decomposition and
  keys — the splits are one step, detach and reattach are two, pane movement
  takes `→`, and the attention sort takes `Alt-T` (the macOS compose glyphs
  `†`/`ˇ` included) rather than a stand-in `t`. `muxa onboard --emit
  step-table` publishes the key each step waits for, derived by walking the
  real gates, and `scripts/onboarding-parity.py` presses exactly those keys at
  the fallback in CI so the two cannot drift apart again.

### Fixed

- **Onboarding no longer dead-ends on `Alt-T`, and arrow keys no longer quit
  it.** The `Alt-T` gate was the tour's only step without an `Alt`-free path,
  so a terminal that composes Option instead of sending Meta — the macOS
  default — could never satisfy it. The gate now also accepts the compose
  glyph (`†`, `ˇ`), and two missed attempts surface the terminal
  setting from `docs/WATCH.md` plus `→` to move on. Separately, a lone
  `ESC` byte that arrives in its own read is reported as `Esc`, so an arrow key
  relayed through tmux or a slow pty could tear the tour down mid-step; both
  the tour and the tmux track now confirm an `Esc` before quitting and
  reassemble the split sequence. `scripts/onboard.sh` read one byte per key
  and so quit on *every* arrow key; it now classifies the escape tail the same
  way and accepts the real `Alt-T`.

## [0.8.35] - 2026-08-22

### Added
- **Google Antigravity CLI (`agy`) is a first-class agent.** agy replaced the
  Gemini CLI upstream but shares none of its hook contract, so muxa's existing
  Gemini support was silently inert against it: agy reads
  `~/.gemini/config/hooks.json` (or `<workspace>/.agents/hooks.json`), not the
  `hooks` key of `~/.gemini/settings.json`; its lifecycle is
  `SessionStart`/`PreInvocation`/`PostInvocation`/`PreToolUse`/`PostToolUse`/`Stop`;
  and its payloads are camelCase protojson keyed on `conversationId`. A new
  `AgentKind::Antigravity`, adapter, and `muxa hook agy` handler cover all six
  events. Neither prompts nor responses appear in any agy payload, so
  `PreInvocation` and `Stop` read them out of the transcript agy points at —
  `PreInvocation` only on `invocationNum == 0`, the turn boundary, so a
  multi-invocation turn doesn't restate its prompt. `muxa init` gains an
  `agy-hooks` component that owns one named key in agy's `hooks.json` (leaving
  plugin and `/hooks`-authored entries alone), `muxa doctor` grew a matching
  check, and `agy` panes are discovered, launchable (`muxa agent start --agent
  agy`), filterable in `muxa timeline`, and reported to the omp sink under their
  own `antigravity` slug rather than as `gemini`.

  `muxa hook agy` is deliberately fail-open and byte-silent on stdout: agy reads
  a hook's stdout as a verdict and treats a non-zero exit or a `decision`-less
  reply as `tool call denied by pre-tool hook`, so a muxa parse error or a down
  daemon must never be able to block the user's tool call.

  A bundled `agy` screen manifest supplies the one state agy's hooks cannot:
  `WaitingInput`. agy fires no hook when it raises an approval prompt, so
  hook-authoritative precedence gains its first carve-out — a row whose kind
  reports `hooks_report_attention() == false` stays screen-inferred, and the
  detector applies the attention signal (and only that) to the **real** row
  rather than minting a synthetic one. Claude Code, Codex, the Gemini CLI and
  opencode are unaffected. Unlike the other bundled manifests, agy's patterns
  were derived from a real agy 1.1.17 session rather than written blind.
  See [docs/ANTIGRAVITY.md](docs/ANTIGRAVITY.md) and
  [docs/SCREEN_DETECTION.md](docs/SCREEN_DETECTION.md).

- **Central SSH fleet management adds a physical host → session → window →
  pane(agent) control plane.** `muxad` now maintains one isolated persistent
  OpenSSH stdio relay and last-known cache per configured node, with stable
  UUID identity, duplicate-node protection, bounded reconnect/concurrency,
  keepalive health, revision-gap reconciliation, and explicit
  observe/control authorization. `muxa host` manages inventory plus
  Kubernetes-style labels/annotations and selectors; `muxa fleet` and
  `muxa watch --fleet` provide status, focused hierarchy navigation, high
  density inspectors, lazy pane/window capture with real tmux geometry,
  exact prompt delivery, and separate-TTY remote attach. The same cache is
  exposed through authenticated dashboard APIs and three explicit MCP tools.
  The relay opens no TCP listener, forces SSH forwarding off, keeps prompt
  bodies out of audit logs, bounds/sanitizes terminal data, and never retries
  mutations. See [docs/FLEET.md](docs/FLEET.md).

  The controller is itself an always-present first-class `local` node, even
  when remote Fleet connections are disabled. It exposes the same topology,
  selectors, capture/send/attach, dashboard, and MCP surfaces through an
  in-process adapter. Kubernetes-style system labels identify the local
  hostname/OS/architecture, while user labels and annotations remain editable
  through `muxa host label local` and `muxa host annotate local`.

  Human host tables are terminal-width aware and concise by default; labels
  move behind `--show-labels` or explicit `-L` columns, while `-o wide/json`
  preserve richer output. A Fleet selection containing only `local` now opens
  the full native `muxa watch` experience without a redundant host row. The
  multi-node view shares watch themes, view/expansion/sort choices, swarm mode,
  dense inspectors, `/` message skills, exact capture/send/attach, and uses a
  Fleet invalidation stream with a slow reconciliation poll instead of fixed
  high-frequency full redraws.

- **Agents can call a colleague naturally through Muxa MCP.** Connected Claude
  and Codex conversations now map `@peer`, provider/alias/role targets, and
  registered `/skills` to `muxa_call_peer`. The high-level tool expands the
  template, selects a healthy peer deterministically, sends a durable request,
  and can wait for its structured reply. Calls default to `REVIEW · READ-ONLY`;
  execution requires an explicit task authorization, and creating a new pane
  requires separate user confirmation. `muxa_peer_report` retrieves prior
  structured replies for phrases such as “`@peer`'s report”; reserved routing
  refuses to substitute GitHub or invent a PR when no explicit PR number or
  GitHub PR URL grounds that workflow. Repository/cwd context alone is not
  sufficient.

## [0.8.34] - 2026-08-14

### Changed

- **A hook subcommand no longer aborts on an unreadable `config.toml`.**
  `muxa hook …` now falls back to compiled defaults and puts the parse error
  on stderr instead of exiting non-zero. Every other subcommand still fails
  loudly. This matters because agy reads a non-zero hook exit as its verdict
  (`tool call denied by pre-tool hook`), so one TOML typo would otherwise have
  blocked every tool call in every agy session with nothing naming the cause.

- **Session and window rows can send contracted messages without hierarchy
  descent.** Pressing `m` on a parent deterministically targets the first live
  tracked agent in numeric window/pane order, with the exact resolved pane
  shown in the composer title. Exact pane selections remain unchanged.

- **Message composers now have reusable `/` skills.** Register templates with
  `muxa skill add` or `[message.skills]`, then press `/` in an `m` composer in
  watch/dashboard or a watch `a` composer to filter and insert one at the
  current cursor. Existing draft text is preserved, so skills can be added
  while composing a longer message or combined repeatedly. Watch palettes also
  add/update with `F2` and confirm removal with `Delete` (`Ctrl-A`/`Ctrl-D`
  remain aliases). Skills remain text-only: selection never auto-sends or
  changes request kind, send mode, ask agent, or ask permissions.

## [0.8.33] - 2026-08-13

### Changed

- **Session and window inspectors are now operational rollups.** A selected
  session shows scope, presence, the highest-priority attention item, latest
  activity, and a compact window roster with its pane children. A selected
  window adds aggregate process/shell/subagent load, peak context, total cost,
  collaboration mailbox state, and a live one-second snapshot that preserves
  the window's real pane split geometry. Active and attention panes are
  highlighted; zoomed windows fill the canvas with the active pane. Small
  inspectors and backends without geometry retain the responsive pane roster,
  while an explicitly selected pane still provides the full single-pane view.
- **Watch Inspector work is bounded and stays off the input path.** Each frame
  now builds and sorts the topology once, session rosters stop at the visible
  height, and a ready window mosaic skips its duplicate text roster. Window
  captures run asynchronously with bounded pane parallelism and cache parsed
  ANSI until the next one-second snapshot. Tree spinners redraw at 4 fps and
  only while an active state is actually visible.
- **Focused watch trees navigate by hierarchy level.** In the default
  `tree_expansion = "focus"` mode, `j`/`k` now move between sibling sessions,
  windows, or panes instead of stopping on every visible descendant. A
  one-node sibling group automatically falls back to its parent's siblings, so
  a lone window or pane never traps navigation. Use `l`/Right to descend and
  `h`/Left to return to the parent. Each explicit descent selects the first
  child immediately and keeps that path visible even below the configured
  automatic `view` depth, so panes remain directly selectable. The `always`
  and `manual` policies retain visible-row traversal.
- **`muxa watch` now messages as the operator console, not as the pane it was
  opened from.** Previously the launch pane's agent was the represented sender,
  so `m` silently refused on exactly one row — the row you were sitting on when
  you pressed `prefix+s` — and messaging was unavailable altogether when watch
  was opened from a shell pane. The human at the keyboard is the sender now, so
  every row is addressable from anywhere and no agent has to occupy the launch
  pane. `--caller-pane` still lands the cursor on your current pane and still
  travels with the request as audit provenance; it just no longer supplies the
  sender's identity. MCP and `muxa msg` are unchanged: an agent calling them
  still speaks for its own pane, and its replies still route back and wake it.
- **Watch's `M` mailbox follows the cursor.** A console has no pane for a reply
  to be delivered to, so replies live on the request in the recipient's
  mailbox. `incoming` is now the selected agent's mailbox and `sent` is the
  console's dispatch log across every target; `i` and `e` act as the selected
  agent, since claiming and replying are the recipient's moves.
- **Under host scope, `m` on a row with no agent no longer guesses.** The
  lone-peer shortcut is a room convenience; with the cursor as the selector and
  the table spanning the host, it degrades to keystrokes and says which row is
  the problem instead of quietly addressing the launch window's agent.
- **`muxa dashboard` sends as the operator console too.** Same reasoning and
  same consequences as watch: the launch pane's agent is an ordinary recipient,
  a dashboard opened from a shell can message, and `b` shows the mailbox of the
  card under the cursor while `i`/`e` act as that agent. Its reach is still the
  room — the dashboard does not follow `[collaboration].scope = "host"`.

### Fixed

- **A workspace open in two terminals is one tree again.** A tmux session group
  shares one window list across several sessions — the supported way to put two
  terminals on two Work windows of one workspace — and `list-panes -a` walks
  sessions, so it reports every pane once per member. Topology showed the same
  workspace two or three times over, with the agent attached to whichever copy
  was scanned first and the rest rendered as bare panes. Grouped sessions now
  fold onto the member the group is named after, deduplicated by pane; if that
  session is gone the choice falls back to the lexically first id so it stays
  put across ticks. `watch` sums `attached_clients` across the group, since the
  members it no longer shows are still terminals on that workspace. `PaneInfo`
  and `SessionInfo` carry `#{session_group}`, both `serde(default)` for peers
  built before the field.

- **A refused IPC request no longer reports "no active agents".** `snapshot`,
  `by_pane`, and their timeout variants handed the response straight to a
  lenient decoder that read the absent `agents` array as an empty registry, so
  every daemon refusal became a confident, wrong, and perfectly stable answer
  — and exited 0. The failure this hides in practice is a mixed install: a
  `muxad` predating the protocol 5 bump answers `protocol mismatch: server=4
  client=5` to every call, and `muxa status` reported no agents for a full day
  on a host with 58 registered. Refusals now surface as `RuntimeError::Daemon`
  carrying the daemon's own message, a protocol mismatch adds the restart it
  needs, and a genuinely empty registry still reads as empty.

- **Jumping from watch no longer drags a second terminal off its window.** The
  jump addressed the target as a bare pane id, which names a pane but not a
  session; tmux filled the gap from recent client activity. Under a session
  group — `tmux new-session -t <session>`, the supported way to keep two
  terminals on two Work windows of one workspace — the same window is linked
  into several sessions, and the guess was routinely wrong: measured on tmux
  3.4, jumping one client pulled it into the grouped sibling and dragged the
  other terminal's view along, re-coupling the terminals the group exists to
  separate. Jumps now address the window as `<session_id>:<window_id>`,
  preferring the asking client's own session when the window is linked there
  and otherwise the pane's recorded session, so a cross-session jump is
  deterministic instead of activity-dependent. The bare-shell path carried the
  same defect more visibly — its pre-attach `select-window` moved a grouped
  bystander session and left the session it then attached to on the wrong
  window — and it now targets the session it is about to attach. That attach
  also addresses the session by id rather than by name, which matched by
  prefix (`callabo` against `callabo-set`).

- **`muxa upgrade` no longer strands an old daemon when no service manager is
  usable.** A capable muxad now drains and re-execs itself on the exact socket
  being upgraded, preserving its pid and launch context. The CLI confirms the
  replacement by an IPC generation advance instead of a connect-only timing
  guess, and source, Homebrew, and release-binary upgrades share the same
  restart path. Native service managers remain the compatibility fallback for
  older or stopped daemons. SIGTERM/SIGINT wins atomically over any in-flight
  restart request, so an explicit stop cannot be accidentally re-armed.
- **A stale or non-participant row no longer disables watch's messaging.**
  `muxa register` task rows and just-stopped agents look like agents in the
  topology but are not collaboration participants; pointing at one now costs
  that row its mailbox instead of dropping the room and downgrading `m` to
  keystrokes for the rest of the session.
- **Console replies no longer accumulate forever.** With no pane to wake, a
  console-sent reply used to stay in muxad's pending set and be re-scanned every
  two seconds, and to count toward an unread badge nothing could clear.
- **Watch's inspector no longer prints a permanent `0 unread`.** Nothing is ever
  addressed to a console, so the sender-side unread count is dropped there
  rather than rendered as a zero that reads as "no mail anywhere".

## [0.8.32] - 2026-08-12

### Changed

- **Watch sessions now reveal windows on focus instead of all at once.** The
  default `tree_expansion = "focus"` behaves like an accordion: focusing a
  session opens its windows and folds the previous session; pane view likewise
  opens the focused window's panes. Set it to `"always"` for the original fully
  expanded depth or `"manual"` for navigation-key-only disclosure.
- **Watch no longer repeats one status down a single-child hierarchy.** An
  expanded session with one window, or window with one pane, leaves its state
  cell empty and lets the child carry the same information. Collapsed parents
  and real branch points keep their aggregate state, while all hierarchy rows
  remain independently selectable for attach, spawn, inspect, and close.
- **Peek identifies the most recently prompted pane even when ages tie.**
  Relative labels intentionally collapse exact timestamps into buckets such as
  `1h ago`, which made several panes look equally recent. `prefix + q` now
  marks the pane with the exact newest prompt timestamp as `last · 1h`;
  exact ties are all marked instead of being ordered arbitrarily.
- **The curl onboarding preview no longer downloads Muxa.**
  `scripts/onboard.sh` is now the entire 20-step fullscreen shell tour. ANSI
  rendering preserves the virtual shell, tmux status line, window/pane layout,
  and Muxa watch interactions without fetching an archive, creating temporary
  files, or checking the host architecture. The installed `muxa onboard`
  command remains the native Ratatui version of the same hands-on tour. After
  a completed shell tour exits the alternate screen, it now prints concise
  Homebrew, direct-download, and install-guide next steps.

## [0.8.31] - 2026-08-11

### Added

- **The onboarding tour can run before Muxa is installed.** The new
  `scripts/onboard.sh` curl entrypoint selects the latest pre-built CLI for
  x86_64/arm64 Linux or macOS, verifies its published SHA-256 checksum, runs
  `muxa onboard` from a temporary directory, and removes it on exit. It does
  not install binaries, edit config, start the daemon, or require a live tmux
  session.

### Changed

- **The README recordings show what muxa actually does now.** The old GIF
  predated a month of work: it was rendered before the demo fixture was
  last enlarged, so it showed three sessions in a small popup with a
  footer of keys that no longer exist, and nothing of the inspector,
  mailbox, ask, split cycling, or the swarm view. There are now two —
  `docs/demo.gif` for triage (sixteen sessions, `|`, the swarm, `muxa
  attend`) and `docs/demo-collab.gif` for collaboration (`b`, `m`,
  `a`/`A`) — because one recording covering both ran 45 seconds.

  The fixture behind them was rebuilt to match: ~18 agents across every
  state muxa renders including rate-limited and error, several sessions
  carrying two agents, live Task subagents, a seeded mailbox sent through
  the real `muxa msg` path, and stubbed agent CLIs so the ask feature
  answers instantly, for free, and identically on every render. Every
  pane is painted with a frame matching the prompt it was seeded with, so
  the inspector and preview show an agent rather than an empty box.
  `docs/demo-teardown.sh` is new, `docs/demo-seed.sh` is gone, and
  `docs/demo-README.md` documents the parts that are load-bearing rather
  than the story arc from four releases ago.

## [0.8.30] - 2026-08-07

### Fixed

- **Detail panes keep the author's line breaks.** The wrapper collapsed
  every newline into a space, so a bulleted answer, a quoted block or a
  multi-paragraph reply all arrived as one grey wall — in the ask panel,
  the collaboration mailbox, and anywhere else the shared wrapper renders
  a body. Line breaks are content: they survive now, runs of blank lines
  collapse to one so shape does not cost the whole pane, and continuation
  rows align under the `body:` / `ask:` gutter. Wrapping is measured in
  display cells, so wide glyphs stop one short of the border instead of
  spilling past it.

### Added

- **`Tab` picks the agent in the ask composer, and filters the history in
  the panel.** Two different jobs: choosing who to ask belongs next to the
  question, where the composer title names the target before Enter commits
  to it; the panel is for reading, so its `Tab` cycles all → claude →
  codex without silently repointing the next question. Each agent keeps
  its own conversation — a claude session id means nothing to codex, so
  switching has to switch threads rather than corrupt one — and switching
  back resumes where that agent left off. `n` resets only the current
  thread.
- **`muxa init --component ask` turns it on, and the wizard offers it.**
  Same grant shape as collaboration: enabling ask lets muxad spawn an
  agent CLI that bills your account, so it is asked for where the user is
  already deciding what muxa may touch rather than compiled in. Ships in
  the `standard` preset and pre-checked in the picker — leaving it dark is
  what makes `a` answer "ask is disabled" with nothing pointing at the
  fix. A configured `agent` is preserved: choosing codex was deliberate,
  and re-running init must not quietly repoint your questions.
- **`a` asks the configured agent a headless question; `A` browses the
  answers.** Spinning up a whole session to ask one thing was the only
  option, and it is a heavy one. `a` sends the question to muxad, which
  runs the agent in print mode and captures the answer; `A` opens a
  mailbox-shaped history (`j`/`k` to select, `|` to grow the detail pane,
  `n` for a fresh conversation) that survives restarts in
  `$XDG_DATA_HOME/muxa/ask.json`.

  Print mode rather than a parked interactive session, because a TUI gives
  no machine-readable "the answer ends here" — reading a reply back would
  mean scraping a moving target. It is not the slower choice either: both
  CLIs resume a conversation by id, so every question after the first
  reuses the cached context the first one paid for, which is exactly what
  a parked session was meant to buy. The thread continues until `n`.

  muxad owns execution so an answer survives the watch popup closing, and
  a `running` entry left by a daemon that died is re-labelled failed at
  load rather than hanging forever. Configure under `[ask]`; off by
  default, since enabling it lets the daemon spawn a CLI that bills the
  user's account.

## [0.8.29] - 2026-08-06

### Added

- **The spawn form takes a session name.** Empty derives one from the
  directory, shown as an `(auto: …)` placeholder instead of guessed behind
  the user's back; a typed name is sanitized the same way (`.`/`:` → `-`)
  and still deduplicated against live sessions.
- **`Ctrl-V` pastes in every input surface.** Terminals that deliver it as
  a plain keypress got nothing before; the composer, spawn form, and
  command palette now read the system clipboard through the same backend
  cascade the copy path uses (tmux buffer, pbpaste, wl-paste, xclip, xsel).

### Fixed

- **Pasting into the spawn form filtered the table instead.** The
  bracketed-paste handler covered the composer and the palette but not the
  spawn form, so a paste while `n` was open fell through to the search
  fallback and the table started filtering underneath the form.
- **Long spawn prompts wrap downward instead of clipping at the border.**
  The form grows a row at a time (capped at half the body), width-aware so
  wide glyphs never overflow; dir and name keep a single scrolling line.

## [0.8.28] - 2026-08-06

### Added

- **`muxa upgrade` works without a source clone.** Most users have no
  checkout, and "clone the repo first" is a dead end from a release binary.
  The command now resolves its own install channel: inside a source tree it
  keeps the git-pull + cargo-install flow; a Homebrew-managed binary (Cellar
  in the canonical path) delegates to `brew upgrade muxa`; anything else
  self-updates from the GitHub release for this platform — download the
  target archive and its `.sha256` sidecar, verify, swap `muxa`/`muxad` in
  place atomically with the previous binaries parked as `.bak`, restart the
  daemon, verify the socket. Asset naming and archive layout were measured
  against the real v0.8.27 release, which is how the sidecar's
  `muxa-<tag>-<triple>.sha256` name (no `.tar.gz`) and the nested
  `muxa-<tag>-<triple>/` directory were caught before shipping.

## [0.8.27] - 2026-08-06

### Added

- **`n` spawns a new agent session from inside watch.** The console could
  observe, attach, message and collaborate — but starting work still meant
  leaving it to hand-build a session. `n` opens a three-field form (working
  directory, agent, first prompt); Enter creates a detached tmux session
  named after the directory (deduplicated, never reusing an existing one)
  and launches the agent with the prompt already on its command line, so it
  boots working instead of waiting for someone to type. Launch flags mirror
  callabo-resolve's, permission bypass included: a fire-and-forget session
  parked on its first permission prompt never runs the command it was
  created for. muxa's hooks pick the new session up within seconds and it
  appears as a row like any other.
- **The `|` split is saved.** Cycling list/inspector writes
  `watch.inspector_split` to config.toml the same way runtime sort changes
  are saved, so the divider a user put in place survives the popup closing.
- **Narrow tables fold SUMMARY instead of truncating names.** At the 30/70
  split every column got squeezed and the session *names* were the casualty
  — four-character scraps identifying nothing, next to a summary column
  that was itself unreadable at that width. Below 60 table columns SUMMARY
  folds away and the name column grows into the slack, capped at 30 so it
  does not trade an unreadable summary for a field of trailing blanks. DUR
  and ACT survive at any width: six columns each, and part of the row
  identity.
- **Mailbox request bodies wrap.** The detail pane rendered the body as one
  truncated line under a hard seven-row cap, so expanding it just added
  blank rows under an ellipsis. Body and reply now share the pane's actual
  height, fold-style.
- **Session view drops the STATE column: DUR + ACT tell the timing story.**
  The SESSION cell's leftmost cluster already shows every state as
  icon-with-count, so a 12-column STATE said the same thing twice while
  SUMMARY starved. (The old default-swap matched `state` but the shipped
  default is `state_age` — the swap never fired, which is how both columns
  ended up on screen.) Configured column lists are untouched.
- **`|` in the mailbox cycles the selected-request detail: compact → half →
  expanded.** Long request bodies are the point of opening the mailbox;
  six fixed rows made anything substantial unreadable.
- **`|` cycles the watch list/inspector split: 50/50 → 70/30 → 30/70.** The
  split was hard-coded at roughly half-and-half; reading a long prompt in the
  inspector or scanning a wide table both wanted the other bias. Presets over
  free resize: two keystrokes reach any of them, and a resize mode would cost
  a modal state for a knob with three useful values. Also in the palette as
  `:split`. Session-local — a glance preference, not configuration.
- **`[collaboration] scope = "host"` lets `m` send a request to the row under
  the cursor, any window.** The default keeps the same-window rule — putting
  agents in one window is the consent that lets them talk — but a watch user
  driving a whole host reads the cursor as the recipient, and being told to
  attach to each session first makes the console pointless. Under host scope
  an explicit `pane:%N` target may address any tracked agent; the origin (and
  the reply's destination — this watch's `b` mailbox) remains the launch
  agent, so sender identity is never forged. `peer`, `@alias` and `role:`
  stay window-scoped: a pane id is unique on the host, an alias is only
  unique within one room, and host-wide alias matching would invite
  misdelivery.

### Fixed

- **Attach landed in the right session but the wrong window.**
  `switch-client -t <pane>` resolves only the session; the client arrives on
  whatever window that session had current. The pane's window has to be
  selected *after* the switch — after, so the shared current-window mutation
  is confined to the session being entered instead of rearranging a bystander
  the way the old pre-switch `select-window` did.
- **`m` no longer refuses to type when collaboration is unavailable.** No
  room, no peer, collaboration disabled, or a row outside this window — the
  composer still opens, in a keystrokes-only form against the selected pane,
  with the reason the contract modes are missing shown as a hint. `Tab` and
  `Ctrl-E` explain instead of cycling: what this form cannot do is dress
  keystrokes up as a request.
- **The caller flags never arrived: `display-popup` does not expand formats.**
  The previous fix passed `#{client_name}`/`#{pane_id}` in the popup command,
  but tmux hands that string through verbatim (measured on 3.4) — so muxa
  received the literal text, aimed `switch-client -c` at a client named
  `#{client_name}`, and every Enter became a silent no-op: nothing moved at
  all. The managed binding now goes through `run-shell`, which does expand at
  the keypress, and the popup itself is pinned with `display-popup -c` so it
  opens on the pressing client too. Watch additionally drops caller values
  that still contain `#{` — a stale binding degrades to the old guess instead
  of a dead Enter. End-to-end verified: the argv reaching the popup command is
  the real `/dev/pts/N` and `%N`.
- **The client that pressed the key is now named by the key binding itself.**
  Even with `switch-client -c`, the pin was filled in by asking tmux for "the
  current client" — an activity-based guess a popup cannot make reliably, so
  with two terminals attached the *other* one could still be moved. Measured
  live: a popup opened from `/dev/pts/67` answered `/dev/pts/87`. The managed
  `prefix+s` binding now expands `#{client_name}` and `#{pane_id}` at the
  keypress — the one moment that identity is unambiguous — and passes them to
  `muxa watch --caller-client/--caller-pane`, which pins every later focus
  move and seeds the collaboration room and opening cursor. Re-run
  `muxa init --component tmux-popup` and source `~/.tmux.conf` to pick the
  binding up; without the flags watch falls back to the previous behaviour.
- **Enter in `muxa watch` moved a terminal the user was not looking at.** The
  jump pre-positioned with `select-window -t "<session>:<index>"` *before*
  switching anyone, and a session's current window is shared state — so any
  other terminal already attached to that session jumped on the spot, to a
  window nobody asked it to show. The name-based target was a second hazard:
  tmux matches session names by prefix unless anchored with `=`, and real
  session sets collide (`callabo` against `callabo-set`; 16 such pairs on the
  host this was found on). Address the pane id instead, which cannot be
  ambiguous, and let one `switch-client -c <client> -t <pane>` resolve session,
  window and pane for the asking client only.
- **The collaboration room could be resolved from another terminal's session.**
  Asking tmux for "the current client's pane" resolves against whichever client
  it last saw activity on, so with two terminals attached it answers for the
  other one. Measured inside a popup on `callabo-set`, the unpinned query named
  `%771 callabo-recorder` while `$TMUX` correctly named `%691 callabo-set`.
  That pane seeds the room, so `m` reported peers for a window the user was not
  in. `$TMUX` is rewritten by tmux to the popup's own session and is the
  authority; the client query is not a usable fallback and is gone.
- **`muxa watch` acted on the wrong terminal when two were attached.** Enter
  jumped the user's *other* tab to the target while the tab they pressed it in
  went somewhere else, and `m` reported no peers in a window that plainly had
  them. One cause behind both: `prefix+s` runs watch inside a `display-popup`,
  where tmux leaves `TMUX_PANE` empty, so pane resolution fell back to the
  session id parsed out of `$TMUX` — the popup's *target* session, which is not
  always the session the client is displaying. Measured inside one popup, the
  two paths named different panes (`%771` vs `%773`). Ask the client what it is
  looking at first. `switch-client` now also pins `-c`: without it tmux picks
  the current client from recent activity, which is how the wrong tab moved.

### Added

- **`Shift-Tab` crosses between a prompt and a request without retyping.**
  `Enter` types keystrokes into a pane; `m` sends a durable request with a kind,
  a work mode and a reply. They look alike from the composer and are not alike
  at all, and keeping them on separate keys meant memorising which key produced
  which. Merging them was the wrong fix — it would hide *which one is being
  sent*, and "I thought that carried `read_only`" is not a mistake worth
  enabling. So: one surface, one key to cross between the two, and the draft
  comes along. `Tab` still cycles request kind and `Ctrl-E` still toggles work
  mode; transport is its own axis.
- **`muxa doctor` checks the tmux binding that will actually run, and prints
  where `config.toml` lives.** The marker-block check proves the managed
  `prefix+s` is *in* `~/.tmux.conf`, not that tmux would run it: duplicate
  `bind-key` definitions resolve last-one-wins, and a running server keeps what
  it read at startup. Either way doctor stayed green while `prefix+s` opened an
  inset popup too narrow for the watch inspector's 120-column minimum — which
  reads as "the inspector is on but I never see it". The config line is one
  more line of output and removes a guessing game: `dirs::config_dir()` is
  `~/Library/Application Support` on macOS and `~/.config` on Linux, so advice
  to edit `~/.config/muxa/config.toml` is wrong half the time and the edit
  lands in a file nothing reads.

- **`muxa init --component collaboration` turns the agent mailbox on.** Enabling
  collaboration means a peer's request may type a wake prompt into your pane, so
  it stays a grant rather than a compiled default — but the only way to give that
  grant was to know `[collaboration]` exists and hand-write it into a
  `config.toml` that `muxa init` never creates. Nothing surfaced the option, so
  `m` and `b` in `muxa watch` answered "agent collaboration is disabled" on every
  fresh machine with no hint about where to go next. The component asks for the
  grant where the user is already deciding what muxa may touch, and ships in the
  `standard` preset. An existing `wake` is preserved: a deliberate `never` means
  "mailbox, but stay out of my panes", and re-running init must not re-arm it.

## [0.8.26] - 2026-08-05

### Fixed

- **`Alt-I` now says which way it toggled the inspector.** `:inspector` set a
  footer hint; `Alt-I` ran the same toggle and said nothing. That silence is
  what makes the binding look broken — the inspector is enabled by default, so
  the first press *hides* a panel rather than summoning one, and below 120
  columns neither state renders anything at all. It reads as "`Alt-I` only
  works if I press it twice", when in fact the first press worked and turned
  it off.

## [0.8.25] - 2026-08-05

### Fixed

- **`muxa watch`'s popup is wide enough for its own inspector.** The
  `prefix + s` binding opened watch in a `-w 90%` popup, but the wide-screen
  inspector needs 120 columns of *inner* width and an inset popup spends that
  budget twice — 90% of a 134-column terminal is 120, then the border takes 2
  more, handing watch 118. The cutoff sat at roughly a 136-column terminal, so
  on most displays `Alt-I` appeared to do nothing. watch now gets the same
  borderless full-client popup peek already uses. Existing installs pick this
  up on the next `muxa init`.

### Documentation

- `docs/WATCH.md` explains why `Alt` bindings do nothing on macOS (Option is a
  compose key until the terminal opts out), with the fix for Ghostty, iTerm2,
  and Terminal.app, and a pointer to the command palette — which covers every
  `Alt` binding without any terminal configuration.

## [0.8.24] - 2026-08-05

### Added

- **`muxa peek` — `display-panes` with context.** Takes over `prefix + q`
  with a borderless fullscreen popup that repaints the current window's pane
  layout: each pane's live screen is captured and dimmed as a backdrop, with
  a box over it carrying that pane's agent state glyph, session summary,
  latest prompt (stamped with how long ago you sent it), latest response,
  and a `model · ctx% · 5h%` strip. The box
  claims only the rows it needs (never more than two thirds of the pane), so
  the terminal underneath stays readable; panes running a plain shell get a
  bare digit badge instead. Pressing a pane's digit jumps to it (windows
  with ten or more panes accumulate digits, with `Enter` to commit an
  ambiguous one) and the footer carries the global attention count. Install with
  `muxa init --component tmux-peek` (uninstall restores stock
  `display-panes`); `muxa peek --plain` prints the same per-pane lines as
  text.

### Fixed

- **`muxa init` no longer writes this host's socket path into
  `~/.tmux.conf`.** The `tmux-env` block pinned
  `MUXA_SOCKET=/tmp/muxa-<uid>.sock` on every install, which is wrong on any
  other machine that shares the file through a dotfiles repo — and redundant
  on this one, since every muxa binary derives that same path itself. Only a
  socket that muxad's config points somewhere custom still gets pinned.
  Existing installs have their stale pin scrubbed on the next `muxa init`.
- **`muxa peek`'s prompt age is legible.** The timestamp on the box border
  carried the exact style the overlay uses for the dimmed pane capture
  behind it, so the one number telling you whether a prompt landed a minute
  or a day ago read as background noise.

## [0.8.23] - 2026-08-03

### Added

- **Direct filtering and familiar navigation in `muxa watch`.** Printable text
  now filters immediately, while `/` explicitly arms search for queries that
  begin with a reserved browse key. Empty-query navigation supports `hjkl`,
  arrows, `gg`/`G`, Home/End, Page Up/Down, Ctrl-U/D, `q`, `?`, `r`, and `o`.
  A new `:` command palette exposes refresh, preview, copy, attention/events,
  inspector, sort, view, kill/abort, help, and quit actions with Tab completion.
- **Watch inspector and event inbox.** Wide terminals can keep the selected
  pane's live capture visible alongside the table, and completion, error, and
  attention transitions remain available in a 50-entry in-process inbox with
  an unread count.
- **State-age and workload context.** The default state column includes time in
  the current state, and selected-row details can show child process/subagent
  workload without requiring a permanently visible extra column.

### Changed

- **Session navigation keeps context visible without adding keystrokes.** The
  selected multi-pane session shows its agent rows automatically, but vertical
  navigation still moves between sessions until Right/`l` deliberately enters
  child selection. Single-pane sessions avoid a duplicate child row, selection
  gutters stay aligned, and the existing detail row remains visible for both
  parent and child selections.
- The default watch sort now prioritizes attention state, groups by session,
  and orders each group by latest activity. Runtime sort actions persist their
  selected preset to the watch configuration.

### Fixed

- **Every NAME column collapsing to raw `%42` pane ids on a busy host.** The
  synchronous tmux wrapper waited on the child while its stdout/stderr pipes
  went unread, so any payload larger than the pipe capacity blocked tmux in
  `write` — it never exited, the 1 s timeout killed it, and `list_panes`
  returned an empty inventory that made every status row fall back to a raw
  pane id. The capacity is not the assumed 64 KB: once a user's total pipe
  pages cross `fs.pipe-user-pages-soft`, Linux hands out one-page pipes, and a
  box running dozens of agents crosses that line — an 8 KB pipe against a
  ~10 KB `list-panes -a` payload flapped between correct and broken run to
  run. Both pipes are now drained on reader threads for the whole wait, so
  output size no longer matters. Also fixes the same stall in the paste path
  (`load-buffer` stdin) and in `capture-pane`, whose scrollback payloads clear
  a small pipe routinely.

## [0.8.22] - 2026-07-21

### Added

- **Control plane + MCP server (`muxa mcp`).** muxa can now *drive* agents,
  not only observe them. A new `muxa mcp` subcommand runs a Model Context
  Protocol stdio server so a coding agent can orchestrate the others — wire it
  into Claude Code with `claude mcp add --scope user muxa -- muxa mcp` (see
  [docs/MCP.md](docs/MCP.md)). Tools: `muxa_status`, `muxa_recent_prompts`,
  `muxa_send_prompt`, `muxa_capture_pane`, and `muxa_wait_for_change`. It is a
  hand-rolled, tools-only JSON-RPC 2.0 server (no new dependencies; not the
  `rmcp` SDK) and refuses to start when the daemon socket is unreachable.
- **Control IPC methods** (`PROTOCOL.md`): `send_prompt { pane, text, submit }`
  injects keystrokes into a pane (resolving the backend by pane-id namespace,
  and committing the line with a trailing Enter when `submit`) and `capture
  { pane }` returns a pane's visible contents. `send_prompt` refuses backends
  without the new `send_text` capability with a structured error. The daemon
  threads its full multi-host backend set into the IPC server
  (`Server::with_backends`) for namespace-scoped routing. The socket stays
  owner-only (`0600`).
  - **Wrong-target keystroke hardening.** Control ops are pinned to the
    **specific tmux server** the pane's agent row was recorded on (`tmux -S
    <socket>`), not an env-global socket — a pane id like `%5` exists on every
    server, so an unpinned send/capture could hit the wrong one. A pane in a
    KNOWN-but-unobserved namespace (e.g. `herdr:` on a tmux-only daemon) is
    refused with a `namespace unavailable` error instead of falling through to
    the primary backend. The plain (watch preview / dashboard) `capture_pane`
    stays on the default server; only the control path targets a recorded
    socket.
  - **`send_prompt` reports `{ sent, submitted }`** so the text-send and the
    submit-CR outcomes are distinct: a caller that sees `sent:true,
    submitted:false` knows the text already landed and must retry only the
    submit, never the whole prompt (which would double-inject). The submit CR
    is attempted only after the text lands.
  - **Argument-injection & multi-line hardening.** tmux `send-keys` uses `--`
    so text starting with `-` isn't parsed as a flag; text with an embedded
    newline or a trailing `;` is injected via a bracketed paste
    (`load-buffer` + `paste-buffer -p`) so newlines don't submit line-by-line
    and a trailing `;` (which tmux would eat as a command separator) survives.
  - **`subscribe` lagged marker is now opt-in** (`subscribe { lagged_markers:
    true }`). It defaults off so a pre-marker client isn't broken by the
    `{"event":"lagged"}` frame; the server silently continues past an overflow
    for un-opted clients. muxa's own `watch` / `mcp` client opts in.
- **`PaneBackend::send_text`** capability (and its server-pinned
  `send_text_on` / `capture_pane_on` variants): tmux (`send-keys -l --`, or a
  bracketed paste for multi-line / trailing-`;` text) and herdr
  (`pane.send_text`) support keystroke injection; zellij does not
  (`write-chars` only reaches the focused pane).
- **Screen-manifest fallback detection.** Agent CLIs muxa has no hooks for
  (`cursor-agent`, `amp`, `copilot`, `aider`, `goose`) now surface their state
  on tmux hosts by matching TOML manifests against a pane capture — the
  fallback-detection model herdr validated, with hooks staying authoritative.
  Classification is `blocked → working → idle`, else keep-previous; blocked is
  strict (only unambiguous approval UI) so a false "needs input" is avoided.
  Five conservative manifests ship bundled; override or add your own at
  `$XDG_CONFIG_HOME/muxa/agents/*.toml`. Rows are synthetic, so a real hook
  evicts them the moment it claims the pane (precedence: hooks > herdr bridge >
  screen detection). A `muxad` `[screen_detect]` task (on by default, 3 s)
  captures candidate panes across the multi-host backend set. See
  [docs/SCREEN_DETECTION.md](docs/SCREEN_DETECTION.md).

## [0.8.21] - 2026-07-21

### Added

- **Homebrew tap.** `brew install open330/tap/muxa` installs pre-built
  `muxa`/`muxad` binaries (macOS + Linux, arm64 + x86_64) with a
  `brew services`-compatible `muxad` service. A new `tap-bump` workflow
  rewrites the formula in Open330/homebrew-tap whenever a release is
  published (requires the `TAP_GITHUB_TOKEN` secret; skips with a notice
  otherwise).
- **multi-host observation (daemon).** `muxad` now observes every backend
  in `muxa::active_backends()` at once (tmux + herdr during a migration)
  instead of a single env-detected backend. One reconciler runs over the
  whole set — each tick observes every backend concurrently and reconciles
  each observation under its own `HostKind`, so a herdr timeout can't reap
  tmux rows (completeness stays per-host); the workload scan, paneless-codex
  correlation, and cross-host age-out all key off the *complete-this-tick*
  host set, so a row on an unobserved (or chronically-unanswerable) host ages
  out while every host that answered stays governed by its own pass and keeps
  its metadata. Discovery, the
  pane-session cache, and history enrichment enumerate the set and union
  their scans; session-activity sampling runs one tracker that polls every
  host's foreground source (tmux clients + herdr focused workspace) into a
  single race-free ledger; the herdr bridge/report tasks spawn whenever
  herdr is in the set, not only when it's the sole backend. See
  `docs/MULTI_HOST.md`.
- **cross-multiplexer unified console (CLI).** `muxa watch` and the
  pane-listing surfaces now aggregate across *every* active host at once
  instead of the single env-detected backend — during a tmux→herdr
  migration both sides show in one view. `watch`'s refresh fans
  `list_panes()` and the per-host session sources (tmux `list-sessions`,
  herdr `workspace.list`) across the backend set concurrently and concats
  the rows (namespaces keep them distinct: tmux `%N` / herdr `herdr:…`).
  When the row set spans more than one host, each row gets a subtle dim
  host tag (`tmux`/`herdr`) on its SESSION/PANE cell; single-host users see
  no change. Attach (Enter in `watch`, `muxa attend`) dispatches per row on
  the pane id's namespace, so a `herdr:` row focuses via herdr even when the
  shell is tmux-primary (and vice versa); unrecognized ids fall back to the
  process-global backend. `muxa panes`, `stats`, and `timeline` enumerate
  panes across the set too, and `muxa panes` prints a per-host empty hint
  for any host in the set that contributed zero. Live pane captures
  (`watch` preview, `dashboard`) resolve the capturing backend by namespace.
  `current_pane`/status-line ("where am I") stay single-host by design. See
  `docs/MULTI_HOST.md`.
- **herdr web dashboard panes view.** The dashboard's `/api/panes` route
  (and the timeline's pane→session map) now populate on herdr hosts. The
  daemon threads its active pane backend into the dashboard; when the host
  is herdr the pane-cache refresh sources rows from `HerdrBackend::list_panes()`
  over the socket and folds them into the scanner's result shape instead of
  the tmux multi-socket scanner (which sees nothing on herdr). Panes carry
  their herdr-native fields (`herdr:<id>`, workspace as session, tab as
  window) plus a synthetic `"herdr"` socket identity for the UI's
  socket-filter chip. `MUXA_TMUX_SOCKET` scopes tmux sockets only and is not
  applied to herdr panes; the tmux path is unchanged. See `docs/HERDR.md`.
- **herdr watch session view.** `muxa watch --view session` (the default
  view) now populates on herdr hosts: sessions are derived from herdr
  workspaces over the socket (`workspace.list`) instead of `tmux
  list-sessions`, so each workspace shows as a session row keyed by its raw
  `workspace_id` with the workspace **label** as its display name (falling
  back to the id). The DUR column lights up from the existing
  session-activity ledger (keyed by the same `workspace_id`), and Enter-twice
  attach is host-dispatched to herdr's `pane.focus`, never a tmux-only
  action. See `docs/HERDR.md`.
- **herdr session foreground time.** The foreground-time ledger now works on
  herdr hosts: instead of `tmux list-clients` (which finds no server on
  herdr), the sampler queries the focused herdr **workspace** over the herdr
  socket (`workspace.list`) and credits foreground time to it, keyed by the
  raw `workspace_id` so `muxa stats`/`report` ACT numbers and
  `muxa watch --view session` populate exactly as on tmux. Only the sampling
  source branches; all downstream accounting is shared. Known limitation:
  herdr exposes no client-attach state, so focus time accrues even when the
  server is detached with no client attached (inflates ACT for always-on
  detached servers; mitigation out of scope). See `docs/HERDR.md`.
- **herdr backend (Phase 1).** muxa now observes agents inside
  [herdr](https://herdr.dev) panes: a full `PaneBackend` over herdr's local
  socket API (`pane.list`/`pane.read`/`pane.process_info`/`pane.focus`, so
  pane inventory, live captures, pid maps, and watch-attach all work with
  no plugin), hook correlation via `$HERDR_PANE_ID` (rows are namespaced
  `herdr:<pane_id>`), host auto-detection inside herdr panes plus a
  `MUXA_HOST=herdr` override for the daemon, and a cross-host reaping
  guard so a tmux-backend daemon never reaps live `herdr:`/`zellij:` rows
  (and vice versa) while both hosts are in use. Verified end-to-end
  against herdr 0.7.4 (protocol 16). See `docs/HERDR.md`.
- **herdr event bridge (Phase 2).** On herdr hosts the daemon now
  subscribes to herdr's `pane.agent_status_changed` stream and translates
  it into synthetic muxa rows, so agents muxa has no hooks for (cursor,
  amp, copilot, …) still appear in `muxa status`/`watch`/stats:
  `working`→working, `blocked`→waiting, `idle`/`done`→turn-stop; unknown
  agents map to `AgentKind::Unknown` carrying the herdr agent name.
  Real hook events stay authoritative — a hooked agent owns its pane and
  bridge state never clobbers it. Verified live against herdr 0.7.4. See
  `docs/HERDR.md`.
- **herdr reverse path (`pane.report_agent`).** On herdr hosts the daemon
  now pushes muxa's authoritative hook-derived state for REAL
  (non-synthetic) `herdr:` rows into herdr's own UI: a transition
  subscriber maps `Working`→`working`, `WaitingInput`/`WaitingChoice`/`Error`
  →`blocked` (carrying the notification/error text), `Idle`→`idle`, and
  `Stopped`→`pane.release_agent` (handing authority back to herdr's
  detection). Reports carry `source = "muxa"`, a herdr-aligned agent slug,
  the real session id, and a monotonic `seq`. Synthetic bridge rows are
  never reported back, so the two directions can't feedback-loop. Failures
  are best-effort and non-fatal. Verified live against herdr 0.7.4. See
  `docs/HERDR.md`.

### Fixed

- **The `muxa Watch` BarShelf widget no longer goes blank after a muxa
  upgrade.** The widget pinned `schema_version === 1`, so the schema 1→2 bump
  that shipped subagents and workload made every payload fail to parse: the
  card fell back to "Unavailable"/"SSH source offline" on every host at once,
  with `unsupported muxa status payload` in the widget log. `status --json` is
  additive by contract, so the widget now parses forward across schema
  versions and only reports an unsupported payload when the fields it actually
  reads are gone. That case, and a missing/outdated CLI, are now classified at
  the failure site (`SourceFault`) instead of re-matched from prose, so the
  card names the fix ("Update muxa Watch") rather than showing a generic
  offline state — and an actionable fault is never hidden behind the
  last-good-render fallback the way a dropped SSH connection is.
- **herdr backend hardening (review follow-ups).** Several correctness fixes to
  the herdr support: (1) foreign-host rows the daemon can't observe (e.g. a
  `herdr:` row left after switching the daemon back to tmux) now age out to
  `Stopped` after the paneless-orphan timeout instead of ghosting forever;
  (2) nested-host policy is now consistent — **herdr wins presence ties over
  tmux** in *both* host detection and hook pane-stamping (herdr launched from
  a tmux shell is the common case; its shells inherit the outer `$TMUX_PANE`),
  with `MUXA_HOST=tmux` as the escape hatch for the rarer tmux-inside-herdr
  nesting; (3) socket presence uses `try_exists()` so a stat error
  (EACCES/EIO/stalled automount) degrades to a transient error instead of an
  authoritative "no panes" that mass-reaps; (4) the socket request loop now has
  an aggregate read deadline (a chatty server streaming unrelated lines can no
  longer wedge reconcile/watch) and per-`pane.list` process-info enrichment is
  time-budgeted; (5) the web dashboard on a herdr host now merges the tmux
  scan with the herdr pane list instead of replacing it, so tmux panes no
  longer vanish from `/api/panes`/timeline during mixed-host migration. See
  `docs/HERDR.md`.

- **multi-host CLI/daemon wiring (review follow-ups).** Correctness fixes to the
  cross-multiplexer console: (1) `muxa watch` session rows are now grouped by a
  host-namespaced key, so a tmux session and a herdr workspace that share a raw
  id (both named `w1`) stay two distinct rows with correct per-host pane counts
  instead of merging into one corrupted row — display names, the activity ledger
  lookup, and DUR still key off the raw id (host id-spaces are disjoint), only
  grouping/identity/sort-tiebreak use the composite; (2) `watch`'s initial "where
  am I" cursor resolves `current_pane()` across the backend set in env-preference
  order (first `Some` wins) so a herdr pane launched from tmux lands on the
  env-preferred host; (3) daemon discovery passes and the CLI's cross-host pane
  enumeration (`muxa panes`/`stats`/`timeline`/`attend`) now fan out
  concurrently — a slow or unreachable host no longer serializes behind (or
  blocks the runtime on) the others, and a failed host contributes an empty
  result; (4) the web dashboard's single-backend scanner is documented to bind to
  the env-preferred host (`backends[0]`), matching the pre-multi-host daemon. See
  `docs/MULTI_HOST.md`.

- **multi-host core hardening (review follow-ups).** Correctness fixes to the
  backend set and reconciler so one permanently-unreachable or chronically-slow
  host can't poison the others: (1) herdr auto-detect now *connects* to the
  socket instead of `try_exists()`ing the file, so a stale socket left by a
  crashed server no longer ghosts a dead herdr backend into a tmux-only daemon's
  set forever; (2) the env-preferred host now leads the auto-detected set, so
  `backends[0]` (the "primary" the dashboard and watch cursor use) is the host
  the shell actually lives in (`MUXA_HOSTS` keeps its verbatim order); (3) the
  workload scan/update runs over the union of *complete* observations and only
  governs rows whose host was observed complete this tick — a chronically-
  incomplete host's rows keep their workload metadata instead of being frozen or
  reset for every host; (4) the cross-host ghost age-out is keyed on the
  complete-this-tick kinds, so a host that can't answer past the inactivity
  window ages out its rows like a host outside the set (transient incompleteness
  is safe — the threshold is the 24h paneless window, not one tick); (5) paneless-
  codex cwd correlation runs once per tick over the union of panes from all
  complete hosts, so its many-to-one ambiguity guard sees candidates on every
  host and won't mis-adopt a row whose codex lives in the other host's pane at
  the same cwd; (6) the merged session-activity sampler now applies each source
  independently — a failing source (e.g. tmux on a herdr-only machine with no
  `tmux` binary) contributes nothing and no longer closes the other host's open
  foreground intervals, only its own keyspace is left untouched. Single-host
  behavior is unchanged throughout. See `docs/MULTI_HOST.md`.

- **`code_mode_host` codex sessions now correlate to their tmux pane instead of
  splitting into two rows.** Such a session runs its turns — and fires its
  hooks — from a shared, detached `app-server` (parent PID 1, no `TMUX_PANE`),
  so the real hook row landed paneless even while the session lived in a pane,
  and discovery's synthetic placeholder on that pane never merged with it. The
  reconciler now rejoins them: when a paneless codex hook row's `cwd` uniquely
  matches the `pane_current_path` of a pane already carrying a synthetic codex
  placeholder, the real row adopts that pane and the synthetic is demoted. Only
  unambiguous 1:1 matches act — a cwd shared by several codex panes is left
  alone. `PaneInfo` now carries `current_path` for this.

### Added

- **`muxa Watch` (widget 0.4.1) keeps the state dot on the agent's name.** A
  row that carries a subagent tree is taller than a plain one, and the default
  center alignment slid its dot down beside the caption while every other
  row's sat on the name. The row now aligns on the first text baseline.

- **`muxa Watch` (widget 0.4.0) now shows the swarm.** Agents with parallel
  work carry a load badge (`◇` subagents, `▸` shells, `+` other children) and
  expand into one line per named Task subagent in flight — the same glyphs as
  `muxa watch --view swarm`. A new **Show subagents in flight** setting turns
  the tree off; the locked-screen cache keeps the counts and subagent kinds but
  drops their descriptions, which are prompt text.

- **Orphaned agent rows are now reaped instead of accumulating forever.** A row
  with no pane, no surface, and no pid — the shape a codex session driven
  through a detached `app-server`/remote bridge lands in, since its hooks carry
  no `TMUX_PANE` and its ancestry terminates at launchd — was governed by none
  of the converge paths (`reconcile` reaps only pane-dead rows, `reap_dead_pids`
  only pid-tracked rows, `gc` only `Stopped` rows). Such rows lingered
  indefinitely and inflated `muxa watch`'s `+N paneless` count. The reconciler
  now ages them out to `Stopped` after `[reconciler] paneless_stale_timeout_secs`
  (default `86400` = 24h; `0` disables), after which the existing GC removes
  them. Only registry rows are affected — the underlying tmux session is never
  touched.
- **`muxa prune`** clears those orphan rows on demand instead of waiting for the
  24h sweep. `--older-than <dur>` (default `1h`) spares recently-active
  sessions, `--all` removes every orphan, `--yes` skips the confirmation.
  `muxa stats` now nudges when stale orphan rows exist.

## [0.8.19] - 2026-07-10

### Fixed

- **macOS launchd no longer starves latency-sensitive IPC under heavy system
  load.** The LaunchAgent now uses the `Interactive` process class because
  muxa's Unix socket cannot receive launchd's XPC-only adaptive priority boost.
  Re-running `muxa init` fully reloads the plist so process-class and resource
  changes take effect instead of merely restarting the cached job.
- Reconciler passes exceeding one second now emit a structured warning with
  pane listing, workload scanning, and store update timings.

### Security

- Dashboard startup logs no longer include the bearer token in a URL fragment;
  persistent daemon logs contain only the non-sensitive base URL.

## [0.8.18] - 2026-07-10

### Added

- **Agent rows now carry `tmux_socket` and `tmux_session` on the wire**
  (additive, optional). The reconciler backfills both from its multi-socket
  pane scan each tick, and hook adapters send the pane's server socket path
  from `$TMUX`, so pane ids are no longer ambiguous across tmux servers
  (e.g. a dedicated `tmux -L amux` server vs the default one). Clients such
  as amux join agents to workspaces by session name instead of guessing by
  pane id.
- **Socket-aware liveness and dedup**: an agent tagged with a server socket
  is only considered alive while *that* server has its pane, and two agents
  on same-numbered panes of different servers are never collapsed as
  duplicates.
- **`muxa status --json`** emits a versioned, display-oriented snapshot for
  desktop integrations such as the bundled BarShelf menu-bar widget.

### Fixed

- **Codex hooks on macOS can recover their tmux pane after a paneless start.**
  Hook ancestry now uses a lightweight process snapshot, and later real events
  replace stale discovery placeholders instead of leaving duplicate rows.

## [0.8.17] - 2026-07-08

### Fixed

- **`muxad` could become live-but-unresponsive again under overlapping
  status-line and hook clients.** The IPC server now reserves a handler permit
  before `accept()`, so connections over the daemon's concurrency budget wait
  in the OS backlog or time out client-side instead of consuming another daemon
  file descriptor. Client disconnects such as `Broken pipe` are treated as
  normal closures, and idle request connections are reaped faster.

- **tmux `status-line` calls could amplify a degraded daemon.** The status-line
  path now uses a tight IPC deadline and returns an empty line on timeout
  before shelling out for pane metadata, keeping tmux refreshes fast even when
  the daemon is slow.

- **Codex panes launched through the npm `node` wrapper could be missed on
  macOS.** Discovery now normalizes command basenames, so `/.../bin/codex`
  descendants are classified as Codex and panes such as `iac:0.0` regain their
  status-line icon after daemon restart or `muxa sync`.

## [0.8.16] - 2026-07-07

### Fixed

- **`muxad` could wedge into refusing every connection after a burst of hung
  hook clients.** Each agent tool call spawns a short-lived `muxa hook`
  connection; under a stall these accumulated on the daemon with no bound,
  and once the process hit its file-descriptor soft limit (256 on macOS
  launchd) `accept()` began returning `EMFILE` — which propagated out of the
  accept loop and killed it, leaving a live-but-deaf daemon. Hardened the IPC
  server so this failure mode is no longer reachable:
  - `accept()` errors are non-fatal: on `EMFILE`/`ENFILE` the loop logs and
    backs off briefly instead of exiting, so the listener always recovers.
  - Concurrent connection handlers are capped by a semaphore well under the fd
    budget; connections over budget are shed immediately rather than queued.
  - The per-connection request loop has an idle-read timeout, so a client that
    connects but never sends a complete request (or `EOF`) can't pin an fd.
  - `Subscribe` streams emit a periodic keepalive so a dead `muxa watch` client
    is detected and reaped promptly instead of lingering on an idle daemon.
  - The launchd plist / systemd unit now raise the fd soft limit to 4096 as
    defense-in-depth.

- **Hooks could block the agent's critical path when the daemon was slow or
  wedged.** `muxa hook` ingest had no timeout, so a stalled daemon stalled the
  agent (observed: hook processes blocked for minutes, one per tool call). The
  IPC client now bounds every round trip — a tight deadline for hook ingest
  (fail fast to a best-effort no-op) and a general deadline for all other
  queries and the subscribe handshake — so a degraded daemon can never hang a
  caller.

- **`muxad` did not exit promptly on `SIGTERM` during startup discovery**,
  adding a multi-second tail to every restart and pushing operators toward
  `SIGKILL` (which in turn wedged launchd's relaunch). Startup discovery now
  races against the shutdown signal and is cancelled cleanly.

## [0.8.15] - 2026-07-06

### Added

- `muxa stats --graph` now renders a graph-only WACT timeline with buckets that
  adapt to the selected range.
- `muxa dashboard` adds a session-card TUI for live capture, prompt composition,
  and session ACT/WACT totals.

### Changed

- `muxa stats` table output is now focused by default, showing
  `WACT`/`ACT`/`WORK`/`WAIT`/`BLK`/`PROMPTS`/`LAST`; `--verbose` keeps the
  diagnostic columns.
- ACTIVE/WACT attribution is tighter and can exclude mouse-driven tmux ticks via
  `[stats] count_tmux_input`.
- `muxa watch` surfaces pane workload trees and bounds tmux refresh calls.

### Fixed

- Restored CI checks and ignored quick-xml advisories from the Windows-only
  notify dependency path.

## [0.8.14] - 2026-06-16

### Fixed

- **`muxa watch`/`muxa status` were slow to first paint when stale tmux sockets
  piled up** — `list_panes()` enumerated every socket file under
  `/tmp/tmux-<uid>` and spawned a `tmux -S <sock> list-panes` per file. Orphan
  sockets left behind by tmux servers that exited abnormally (tmux never
  unlinks another server's socket, and a container without a working
  `/tmp` reaper never sweeps them) turned into hundreds of process spawns per
  refresh (~0.5s with ~220 orphans), surfacing as a visibly late first paint.
  `enumerate_sockets()` now drops dead sockets with a cheap `connect()` probe
  before spawning tmux, so cost scales with *live* servers, not orphan files.
  Measured: `muxa status` 485ms → ~20ms with ~220 orphans present.

## [0.8.13] - 2026-06-16

### Fixed

- **`muxa watch` reflected pushed state changes a beat late** — a streaming
  transition updated the row in place but never re-sorted, so with
  `sort = ["state", …]` (the sort key is itself a pushed field) the badge
  changed instantly while the row's position lagged up to 5s until the next
  full fallback refresh. Pushes now re-sort immediately, preserving selection
  by pane id; the surgical merge plus a stable sort means only the changed row
  moves (no row-jump jitter).

### Changed

- **`muxa watch` repaints only when something changed** — the render loop no
  longer does a full redraw on every input-poll tick (~62 fps) while idle. It
  repaints on input, a refresh outcome, or a preview recapture, plus a 1s idle
  cadence that keeps the Activity column's relative timestamps current.

## [0.8.12] - 2026-06-15

### Added

- **`--json` and `--markdown` shortcuts on `muxa stats` and `muxa report`** —
  both commands now accept the two boolean flags (mutually exclusive). On
  `stats` they override `--format`; on `report` they opt out of the new
  default tables. `report` also gained `--theme` and `-v/--verbose` so its
  table mode matches `stats`.

### Changed

- **`muxa report` now renders tables like `muxa stats`** instead of always
  emitting Markdown — a range header followed by one bordered table per
  breakdown (day/project/agent/session), each with the shared `TOTAL` footer.
  The previous Markdown output is still available via `--markdown`, and
  `--json` emits the four section documents as a JSON array.
- **`stats`/`report` split hands-on `WORK ACT` from engaged `ACT`** — engaged
  time no longer inflates from scrollback or idle attach; hands-on active time
  counts prompts, thinking, and keypress ticks only.

## [0.8.11] - 2026-06-12

### Fixed

- **Codex rate-limit detection now catches the common cases** — the initial
  reader only flipped to `error` on a non-null `rate_limit_reached_type`,
  which codex rarely sets, so a genuinely rate-limited session still showed
  `working`. It now also treats window saturation (`used_percent >= 100`) and
  credit-plan exhaustion (`primary`/`secondary` null with
  `credits.has_credits:false`) as caps. The credit check is guarded on both
  windows being null so a window-plan row carrying an incidental
  `has_credits:false` is not misread as a cap.

## [0.8.10] - 2026-06-12

### Added

- **Codex rate-limit detection** — Codex exposes no error/rate-limit hook, so a
  usage cap (often one that blocks a turn before it even starts) never moved a
  Codex row's state. The reconciler now reads each live Codex session's rollout
  file (`~/.codex/sessions/.../rollout-*.jsonl`), maps its `payload.rate_limits`
  (`primary` → 5-hour, `secondary` → 7-day) onto the existing rate-limit
  columns, and flips the row to `error` when Codex stamps a reached cap.
  Emissions are change-guarded so the poll doesn't disturb the stuck-state
  sweeps. Toggle via `[reconciler] codex_rollout_enabled` (default `true`).

### Fixed

- **Bound `ACT` by observed human presence** — `muxa stats`, `report`, timeline,
  and dashboard active-session totals now clip prompt/tmux-input padding to the
  matching `HUMAN` presence interval. This prevents short foreground visits from
  being inflated by the default 60s-before/5m-after active window, so a
  session's `ACT` no longer exceeds its observed foreground/interaction time.

## [0.8.9] - 2026-06-11

### Added

- **Background tasks in `muxa status`** — arbitrary processes can be surfaced
  as pid-tracked rows. `muxa register --name X [--pid N]` adopts an existing
  process (defaults `--pid` to the calling shell); `muxa run` PTY sessions
  auto-register with the child's pid. Liveness is governed by pid (not tmux
  pane): the reconciler flips dead pids to `stopped` and the GC evicts them
  after the usual TTL. A name collision with a real agent is refused;
  duplicate task names are disambiguated.
- **Periodic discovery** — `[discovery] interval_secs` (default 30, `0` =
  run-once-at-startup) reruns the tmux agent scan on a timer, so a fresh
  agent session shows up in `muxa status` within the window instead of only
  after its first hook. Reuses the reconciler's existing `tmux list-panes`,
  so the cost is negligible.

### Changed

- **IPC protocol bumped to v3.** The new `task` agent kind is downgraded to
  `unknown` for clients that negotiated an older protocol, so an older
  `muxa status`/`watch` can still deserialize snapshots that contain task
  rows.

## [0.8.8] - 2026-06-10

### Added

- **`[ui] icons` glyph toggle** — choose `unicode` (default) or `ascii`
  agent-state markers. The ASCII set (`* > ? ! o ~ x`) is for terminals whose
  font lacks the Geometric Shapes glyphs or substitutes a mismatched-size
  fallback font for them. Applies across `status`, `status-line`, `attend`,
  and `watch`; webhook notifications keep Unicode.

### Changed

- **`muxa stats` notes are quieter** — the table now reports only that
  explanatory notes exist and points to `--verbose` / `muxa doctor`; pass
  `--verbose` (`-v`) to print the full methodology text. JSON and markdown
  output still carry the complete notes array.

### Fixed

- **WaitingInput marker no longer renders oversized** — replaced `◐`
  (a half-circle codepoint that many terminals draw from a mismatched-size
  fallback font) with `▶`, which lives in the basic Geometric Shapes block
  alongside the other single-cell state glyphs.

## [0.8.7] - 2026-06-08

### Fixed

- **Session state markers stay visible with long names** — session view now
  renders multi-agent state summaries before the session name, so narrow
  columns or very long session names clip the name instead of hiding the
  status markers.

## [0.8.6] - 2026-06-08

### Fixed

- **Claude idle prompts no longer look like blockers** — Claude Code's
  `idle_prompt` notification now stays informational instead of flipping rows
  to `WaitingInput`. User-blocking signals such as permission prompts,
  elicitation dialogs, and choice-style tools still surface as attention states.
- **Legacy idle-prompt snapshots are normalized on restart** — previously
  persisted Claude rows whose only blocker was `Claude is waiting for your input`
  hydrate back to `Idle`, keeping `muxa watch` consistent with Codex rows.
- **Status markers use balanced glyphs** — `muxa watch`, tmux status-line,
  and webhook notifications now share single-cell geometric markers so idle,
  waiting, working, and error states render at a consistent visual size.

## [0.8.5] - 2026-06-08

### Changed

- **`muxa watch` agent counts** — session rows and the watch header now summarize
  multiple agents with the existing state glyphs instead of spelling out
  `N agents`. Single-agent views keep the simpler `1 agent` / session-name
  display, and one-off state counts omit the trailing `1` (for example,
  `? ●2`). The default session view also drops the separate `ST` column because
  the SESSION label now carries the full per-session state summary; pane view
  and explicitly configured `state` columns still keep it.

## [0.8.4] - 2026-06-08

### Added

- **Engaged ("active") time estimate** — `muxa stats` and `muxa report` now
  surface an `ACT` / "Active (engaged)" column next to `HUMAN`, with `active` /
  `active_secs` in the JSON output and `--sort active`. It is the union of three
  signals, each of which an idle attach never triggers: windows around each
  submitted prompt (60s before, 5m after); **tmux input ticks** — the daemon now
  samples each client's `client_activity` and records a `tmux_input` interaction
  whenever it advances (a keypress or scroll), so *reading* while attached counts,
  not just typing; and `thinking` time (present while an agent is blocked on you).
  A pane left attached but untouched no longer balloons the figure the way raw
  `human` presence does. `active` is not bounded by `human`. In the terminal
  table `ACT` takes the slot previously held by `WORDS` (kept in `--format json`
  / `markdown`) so the full layout still fits a ~128-column terminal; the compact
  layout shows `ACT` too. ACTIVE is **de-duplicated across concurrent sessions**
  (a human does one thing at a time): each instant is attributed to the most
  recently touched session ("last touch"), so per-session `ACT` values sum to a
  grand total that stays within real elapsed time instead of multiplying it.
  Window padding is configurable via `[stats] active_lookback_secs` /
  `active_timeout_secs` (default 60 / 300).

## [0.8.3] - 2026-06-08

### Added

- **Scope exclusions for noisy monitoring sessions** — `muxa stats`,
  `muxa report`, and `muxa timeline` now accept `--exclude-pane` and
  `--exclude-session`. Values can be repeated or comma-separated, support
  case-sensitive `*` / `?` wildcards, and are applied before totals are
  computed so monitoring panes do not skew reports.
- **Rolling month and previous calendar month ranges** — `--since month`
  selects the rolling last 30 days, while `--since last-month` /
  `"last month"` selects the previous local calendar month across stats,
  report, timeline, activity, and the dashboard timeline API.

### Fixed

- **Range parser guidance** — unsupported `--since` values now print the
  accepted keywords, duration syntax, date syntax, and timestamp syntax instead
  of a terse invalid-duration error.

## [0.8.2] - 2026-06-08

### Added

- **Daily timeline heatmaps** — `muxa timeline --view heatmap` prints a
  terminal contribution-map style summary, `muxa timeline --day YYYY-MM-DD`
  focuses one local calendar day, and the dashboard renders a clickable daily
  heatmap above the lane graph.
- **Previous calendar week ranges** — `--since last-week`, `"last week"`,
  `previous-week`, and `prev-week` now select the previous local
  Monday-Sunday calendar week across `muxa stats`, `muxa report`,
  `muxa activity`, `muxa timeline`, and the dashboard timeline API. CLI range
  parsing for stats/activity/report also accepts `YYYY-MM-DD` local day
  ranges, matching timeline.

### Fixed

- **Timeline heatmap week alignment** — CLI and dashboard heatmaps now render
  ISO-style Monday-first weekday rows, so `--since last-week` visually lines
  up with its Monday-Sunday range.

## [0.8.1] - 2026-06-06

### Added

- **`muxa timeline --sort`** — timeline output can now sort groups and lanes
  by `latest`, `name`, `duration`, `working`, `waiting`, `error`, `human`, or
  `foreground`, with short aliases like `dur`, `work`, `wait`, `err`, and
  `tmux`. The TUI also adds `s` to cycle sort modes without restarting.

### Fixed

- **cargo-deny CI after the zellij plugin bridge** — documented and ignored
  the upstream `zellij-tile 0.44.3 -> clap 3` unmaintained advisories that do
  not currently have a safe upgrade path, and allowed the permissive `0BSD`
  license used by zellij plugin transitive dependencies.

## [0.8.0] - 2026-06-05

### Added

- **muxa-owned terminal sessions** — `muxa run <command...>` can now launch
  agent commands without tmux. The daemon owns the child process inside a
  PTY, keeps a bounded output buffer, and exposes session lifecycle,
  capture, input, resize, attach-count, and terminate operations over IPC.
  `muxa attach <session>` reconnects a local terminal to that session, while
  `muxa detach <session>` marks it detached for dashboard/session state.
- **Session surface identity** — agents now carry an optional `surface`
  separate from the legacy `pane` field. This lets muxa-owned `pty:*`
  sessions persist and collect prompt history without being mistaken for
  tmux/zellij panes by the reconciler.
- **Dashboard terminal capture** — the dashboard has a read-only terminal
  sessions tab backed by authenticated `/api/terminal-sessions` routes.
  Captured output is bounded and rendered with `textContent` on the
  frontend.
- **opencode hook support** — `muxa hook opencode` now normalizes common
  opencode session, message, permission, and tool events. `muxa init`
  installs a queued plugin wrapper instead of doing synchronous shell-outs
  from high-frequency event handlers.
- **Zellij plugin bridge** — a new `muxa-zellij-plugin` WASM crate forwards
  terminal pane metadata through `muxa zellij-plugin-snapshot`, with a
  freshness TTL in the zellij backend so stale snapshots do not masquerade
  as live pane inventory.
- **`muxa timeline`** — new interactive TUI and JSON timeline built from the
  activity ledger plus live agent/tmux spans. The default overview groups
  lanes by session, supports `--session`, `--agent`, and `--group-by`, and
  includes pan/zoom/focus navigation for work/wait/error/human/foreground
  intervals. The dashboard now includes the same session-grouped timeline
  panel.
- **`muxa watch` latest sort** — `latest` is now the user-facing name for
  newest-activity-first sorting. Use `l` in the TUI or `--sort latest`;
  existing `a`, `activity`, and `act` aliases remain supported. Runtime sort
  changes are saved back to `[watch].sort`.

### Fixed

- **PTY/session hardening from review** — session ids now include an atomic
  counter to prevent same-millisecond collisions, `muxa run` forwards the
  caller's environment and `MUXA_SOCKET`, attach cleanup decrements
  `attached_clients` on error paths, the IPC socket is chmodded immediately
  after bind, oversized IPC JSON lines are rejected, and exited PTY sessions
  are pruned after a TTL.
- **`muxad` only self-heals the tmux global `MUXA_SOCKET` for the
  canonical daemon.** At startup `muxad` writes its socket into the tmux
  server's global env so panes spawned before it can still reach it — but
  it now does so only when running on the default (XDG / `/tmp`) socket or
  the one named in config. An ephemeral instance started with an explicit
  `--socket` / `MUXA_SOCKET` override (a dashboard demo, an e2e test) no
  longer clobbers the global env that the primary daemon and `muxa init`'s
  `tmux.conf` pin own; previously such an instance, on exit, stranded every
  pane spawned in the meantime on a now-dead socket
  (`daemon not reachable at …`). Note: a *non-default primary* daemon
  should set its socket via config (or the `MUXA_SOCKET` pin) rather than a
  bare `--socket` flag if it needs pre-existing panes auto-healed.

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

[Unreleased]: https://github.com/Open330/muxa/compare/v0.8.39...HEAD
[0.8.26]: https://github.com/Open330/muxa/compare/v0.8.25...v0.8.26
[0.8.25]: https://github.com/Open330/muxa/compare/v0.8.24...v0.8.25
[0.8.24]: https://github.com/Open330/muxa/compare/v0.8.23...v0.8.24
[0.8.23]: https://github.com/Open330/muxa/releases/tag/v0.8.23
[0.8.22]: https://github.com/Open330/muxa/releases/tag/v0.8.22
[0.8.21]: https://github.com/Open330/muxa/releases/tag/v0.8.21
[0.8.15]: https://github.com/Open330/muxa/compare/v0.8.14...v0.8.15
[0.8.14]: https://github.com/Open330/muxa/compare/v0.8.13...v0.8.14
[0.8.13]: https://github.com/Open330/muxa/compare/v0.8.12...v0.8.13
[0.8.12]: https://github.com/Open330/muxa/compare/v0.8.11...v0.8.12
[0.8.7]: https://github.com/Open330/muxa/compare/v0.8.6...v0.8.7
[0.8.6]: https://github.com/Open330/muxa/compare/v0.8.5...v0.8.6
[0.8.5]: https://github.com/Open330/muxa/compare/v0.8.4...v0.8.5
[0.8.4]: https://github.com/Open330/muxa/compare/v0.8.3...v0.8.4
[0.8.3]: https://github.com/Open330/muxa/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/Open330/muxa/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/Open330/muxa/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/Open330/muxa/releases/tag/v0.8.0
[0.7.0]: https://github.com/Open330/muxa/releases/tag/v0.7.0
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
