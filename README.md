<div align="center">

<img src="assets/logo.svg" alt="muxa" width="260" />

**Agent CLI observability & orchestration layer for tmux.**

See which agents are working, waiting, or idle — right from your status line, or in a full-screen dashboard.

[![CI](https://github.com/Open330/muxa/actions/workflows/ci.yml/badge.svg)](https://github.com/Open330/muxa/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-informational)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-pre--alpha-orange)
![tests](https://img.shields.io/badge/tests-49%20green-brightgreen)

**English** · [한국어](README.ko.md)

</div>

---

`muxa` is a small daemon that watches agent CLIs — **Claude Code, OpenAI Codex,
Google Gemini CLI, opencode** — running inside tmux panes and surfaces their
state to the tmux status line, a live TUI dashboard, desktop notifications, and
a thin CLI.

It does **not** fork tmux. It talks to tmux via the tmux CLI and to each agent
via that agent's own hook / event-emission system.

<div align="center">
  <img src="docs/demo.gif" alt="muxa demo — status table, tmux status-right glyphs, fullscreen watch TUI" width="900" />
</div>

```text
┌─ tmux status-right ──────────────────────────────────────────────────┐
│   ...   │ ⚙ main:2 claude_code │ ! work:1 codex │ · review:0 gemini_cli │
└─────────┼──────────────────────┼────────────────┼──────────────────────┘
          │                      │                └─ idle
          │                      └─ waiting for input
          └─ working
```

> [!IMPORTANT]
> Pre-alpha. Event ingest, adapters, daemon, CLI, live TUI, and desktop
> notifications all work end-to-end with 49 tests green. APIs may still shift.
> opencode support is deferred.

## Contents

- [Quickstart for Agents](#quickstart-for-agents)
- [Features](#features)
- [Agent support](#agent-support)
- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Live TUI](#live-tui)
- [Desktop notifications](#desktop-notifications)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Development](#development)
- [License](#license)

## Features

|                          |                                                                                  |
| ------------------------ | -------------------------------------------------------------------------------- |
| **Pan-agent**            | One daemon. One CLI. Four adapters (Claude · Codex · Gemini · opencode [†]).     |
| **tmux-native**          | Pane correlation via `$TMUX_PANE`; output labelled `session:window.pane`.        |
| **Zero coupling**        | No changes to tmux or to agent CLIs — just their existing hook systems.          |
| **Live dashboard**       | `muxa watch` — agents on top, every other tmux pane below, 2 Hz, configurable columns. |
| **Desktop alerts**       | Opt-in libnotify / native-toast pings on `WaitingInput` / `Error` transitions.   |
| **Safe by default**      | Socket is `0600`; `SIGTERM` drains and unlinks; `unsafe_code = forbid`.          |
| **Versioned protocol**   | Explicit `PROTOCOL_VERSION`; mismatched clients are rejected.                    |
| **Fast**                 | In-memory registry; no database, no external services.                           |

<sub>[†] opencode adapter is deferred — its integration is SSE / in-process
plugin-based, not shell-hook.</sub>

## Agent support

| Agent               | Integration                                        | Config file                 |
| ------------------- | -------------------------------------------------- | --------------------------- |
| Claude Code         | ✓ shell hooks + status-line Heartbeat              | `~/.claude/settings.json`   |
| OpenAI Codex        | ✓ shell hooks (Claude-protocol clone upstream)     | `~/.codex/config.toml`      |
| Google Gemini CLI   | ✓ shell hooks (Claude-compatible upstream)         | `~/.gemini/settings.json`   |
| opencode            | deferred — SSE subscription / TS plugin planned    | —                           |

## Quickstart for Agents

Paste the prompt below to an AI coding agent (Claude Code, Codex, Gemini
CLI, etc.) running in a tmux pane. It installs `muxa`, wires the active
agent's hook config, and updates `~/.tmux.conf` so this very pane starts
reporting state within seconds.

<div><img src="https://quickstart-for-agents.vercel.app/api/header.svg?theme=claude-code&mascot=thinking&title=install+muxa&lang=Agents" width="100%" /></div>

```text
You're helping install muxa (https://github.com/Open330/muxa), an agent
CLI observability layer for tmux. Requirements: Rust 1.88+, tmux 3.x.

1) Clone and install binaries
   git clone https://github.com/Open330/muxa.git /tmp/muxa
   cargo install --path /tmp/muxa/crates/muxad --locked
   cargo install --path /tmp/muxa/crates/muxa  --locked

2) Start the daemon (foreground is fine; detach with `&` or systemd)
   muxad &

3) Wire the CLI you're running under (detect from $0 / process tree)
   - Claude Code: merge /tmp/muxa/examples/claude-settings.json into
     ~/.claude/settings.json. Do NOT overwrite existing hooks — jq-append
     to each hooks.<event> array.
   - Codex:       append the [[hooks.*]] blocks from
                  crates/muxa-adapters/src/codex.rs module doc to
                  ~/.codex/config.toml.
   - Gemini CLI:  merge the hooks block from
                  crates/muxa-adapters/src/gemini.rs module doc into
                  ~/.gemini/settings.json.

4) Wire tmux (append if not already present)
   set -g status-interval 2
   set -g status-right "#(muxa status-line --pane #{pane_id}) | %H:%M"
   tmux source-file ~/.tmux.conf

5) Verify. The current pane should appear in both outputs below.
   muxa status
   muxa status-line --pane $TMUX_PANE

Rollback: every file edited above was backed up to <file>.muxa-backup-<ts>;
kill muxad with pkill, restore backups, tmux source-file to reload.
```

<div><img src="https://quickstart-for-agents.vercel.app/api/footer.svg?theme=claude-code&tokens=0.4k&model=Any+agent" width="100%" /></div>

Prefer to do it yourself? Keep reading.

## Install

Requires **Rust 1.88+**, **tmux 3.x**, and a Unix-y OS.

<details open>
<summary><strong>From source</strong></summary>

```bash
git clone https://github.com/Open330/muxa.git && cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa  --locked
```

Installs to `~/.cargo/bin/`. Make sure it's on your `PATH`.

</details>

<details>
<summary><strong>Homebrew</strong> (macOS + Linux, from v0.1.0)</summary>

```bash
brew install Open330/tap/muxa
```

The tap repo lives at [Open330/homebrew-tap](https://github.com/Open330/homebrew-tap).
The formula tracks GitHub Releases, so it's only available from v0.1.0 onward.

</details>

<details>
<summary><strong>Pre-built binaries</strong> (from v0.1.0)</summary>

Grab the archive for your platform from the
[Releases page](https://github.com/Open330/muxa/releases) and drop `muxa` +
`muxad` somewhere on your `PATH`.

Artifacts are built for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

</details>

## Quick start

### 1. Start the daemon

```bash
muxad
```

Or run it as a systemd user service — see [`examples/muxad.service`](examples/muxad.service):

```bash
mkdir -p ~/.config/systemd/user
cp examples/muxad.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now muxad.service
```

### 2. Wire your agent

Claude Code — merge [`examples/claude-settings.json`](examples/claude-settings.json)
into `~/.claude/settings.json`.

<details>
<summary>Already using <code>ccstatusline</code> (or another statusLine tool)?</summary>

Claude Code only runs a single `statusLine.command`, so you can't stack
muxa on top of `ccstatusline` with the default config. Use `--forward` to
tee the status-line JSON to your existing tool — muxa captures a
Heartbeat (model, context %, cost) out of band, then pipes the same JSON
into the forwarded command and passes its stdout + exit code through
unchanged:

```json
"statusLine": {
  "type": "command",
  "command": "muxa hook claude-statusline --forward 'npx -y ccstatusline@latest'",
  "refreshInterval": 5
}
```

See [`examples/claude-settings-with-ccstatusline.json`](examples/claude-settings-with-ccstatusline.json)
for the full drop-in. The forwarded command runs under `/bin/sh -c`, so
any shell one-liner works. If the daemon is down muxa still forwards
normally — hook paths are best-effort.

</details>

Codex and Gemini CLI follow the same pattern with different config files.
See each adapter's module-level docs in `crates/muxa-adapters/src/`.

### 3. Wire tmux

Append to `~/.tmux.conf` (or `source-file` the drop-in
[`examples/muxa.tmux.conf`](examples/muxa.tmux.conf)):

```tmux
# Agent glyph per pane on the right-hand status
set -g status-interval 2
set -g status-right "#(muxa status-line --pane #{pane_id}) | #[fg=white]%H:%M"

# Replace the stock session-switcher with an agent-aware popup.
# Enter on a row attaches to that pane; popup closes on exit.
bind-key s display-popup -E -w 90% -h 85% "muxa watch"
```

Reload: `tmux source-file ~/.tmux.conf`.

> [!TIP]
> `prefix + s` now opens `muxa watch` as a floating popup — a drop-in
> replacement for tmux's built-in `choose-tree`, with live agent state
> baked in. `prefix + w` still opens the stock window/session tree if
> you need it.

### 4. Confirm

```bash
muxa status         # human-readable table
muxa watch          # live TUI
```

## Commands

|                                            |                                                                        |
| ------------------------------------------ | ---------------------------------------------------------------------- |
| `muxa status`                              | Human-readable table of all tracked agents.                            |
| `muxa watch`                               | Full-screen live TUI — see [Live TUI](#live-tui).                      |
| `muxa status-line [--pane %N]`             | One-liner for tmux `status-right`; scoped to `$TMUX_PANE` by default.  |
| `muxa recap [--pane %N]`                   | Show the last prompt for the given pane.                               |
| `muxa sync`                                | Backfill the registry by scanning tmux panes — see [Sync](#sync).      |
| `muxa panes`                               | Debug: dump tmux pane inventory.                                       |
| `muxa hook <agent> --event <e>`            | Hook adapter entry point. Invoked by the agent CLIs themselves.        |
| `muxa hook claude-statusline --forward CMD` | Tee Claude's status-line JSON to muxa + a downstream tool.             |
| `muxad`                                    | The daemon. Listens on `$XDG_RUNTIME_DIR/muxa.sock` by default.        |

### Sync

`muxa sync` scans `tmux list-panes`, matches `pane_current_command` against
known agent CLIs (`claude`, `codex`, `gemini` / `gemini-cli`), and asks the
daemon to register them as synthetic agents. The same one-shot pass runs
automatically on `muxad` startup so a daemon restart doesn't blank out
agents that are still alive in their panes. Idempotent: synthetic entries
are replaced in place when a real hook later fires. Toggle via
`[discovery] enabled = false`.

## Live TUI

`muxa watch` is the headline UI: a full-screen dashboard refreshed at 2 Hz,
rendered with [ratatui](https://ratatui.rs). The terminal is restored
cleanly even on panic.

It lists **two row kinds** in one table — the cumulative effect is a
drop-in replacement for tmux's `prefix + s`:

- **Tracked agents** at the top (full color), with kind, state, model,
  prompt, context %, cost, last activity.
- **Untracked tmux panes** below (dim), labelled `session:window.pane`
  with the pane title or current command. These come straight from
  `tmux list-panes -a`, so a freshly-spawned pane shows up on the next
  poll without any muxad knowledge.

Both kinds are selectable. `Enter` on either attaches you to that pane.

The best way to use it is via a tmux popup (see the
[tmux wiring](#3-wire-tmux) above). Press `prefix + s` from any pane →
popup with the live dashboard → `Enter` on the target row → popup closes
and your client switches to that pane. Press `prefix + s` again to bounce
back.

Run `muxa watch` directly from a bare shell to attach into an existing
tmux session — same Enter semantics, except muxa execs `tmux
attach-session` for you instead of `switch-client`.

**Keybindings**

| Key                   | Action                                                                 |
| --------------------- | ---------------------------------------------------------------------- |
| `↑` / `↓` / `k` / `j` | Move the selection cursor.                                             |
| `Enter`               | Attach to the selected pane (`tmux select-pane` + `switch-client`).    |
| `r`                   | Force an immediate refresh.                                            |
| `q` / `Esc`           | Quit.                                                                  |
| `Ctrl-C`              | Quit.                                                                  |

## Desktop notifications

Opt in via config. On `*→WaitingInput` and `*→Error`, or `Working→Stopped`
(task complete), `muxa` fires a native notification — libnotify on Linux,
NSUserNotification on macOS, WinRT toast on Windows. Useful when an agent has
been crunching for 10 minutes and finally needs your attention.

Enable in `~/.config/muxa/config.toml`:

```toml
[notifier]
enabled = true
backend = "libnotify"
```

Then restart the daemon.

## Configuration

`muxad` reads `$XDG_CONFIG_HOME/muxa/config.toml` if it exists. All fields
have sensible defaults — see [`config.example.toml`](config.example.toml)
for the full schema. The `muxa` CLI reads the same file (mainly for
`[watch]`); override with `MUXA_CONFIG=…`.

### Watch columns

The `muxa watch` TUI columns are configurable. The default view leads with
the last prompt — `model` / `ctx` / `cost` are opt-in:

```toml
[watch]
# Display order. Omitted keys are hidden.
columns = ["pane", "state", "prompt", "activity"]

[watch.widths]
# Numeric         -> Constraint::Length
# "min:N" string  -> Constraint::Min(N)         (takes leftover space)
# "pct:N" string  -> Constraint::Percentage(N)
pane     = 22
state    = 14
prompt   = "min:30"
activity = 10
```

Valid column keys: `pane`, `kind`, `state`, `model`, `ctx`, `cost`,
`prompt`, `activity`. Unknown keys log a warning and are skipped — they
don't prevent muxa from starting.

<details>
<summary>Environment variables</summary>

| Variable       | Purpose                                                |
| -------------- | ------------------------------------------------------ |
| `MUXA_SOCKET`  | Override the unix socket path.                         |
| `MUXA_CONFIG`  | Override the config file path.                         |
| `RUST_LOG`     | Tracing filter. Example: `muxa=debug,tokio=warn`.      |
| `NO_COLOR`     | Disable ANSI color in `muxa status`.                   |

</details>

## Architecture

```text
agent CLIs (Claude, Codex, Gemini)
      │
      │  shell hook runs `muxa hook <agent> --event <e>`
      │  — stdin JSON, ~1 ms per event
      ▼
    muxad  ───  0600 unix socket  ───  muxa CLI
      │                                  │
      ├── in-memory agent registry       └── status / watch TUI / status-line / recap
      ├── transition broadcast ──▶ notifier task (libnotify / native)
      ├── GC task (stopped-agent TTL)
      └── graceful SIGTERM → drain → unlink socket
```

Five-crate workspace:

- `muxa-core`     — types, state, config, paths, errors (no I/O)
- `muxa-runtime`  — unix-socket IPC server/client + tmux CLI wrapper + notifier
- `muxa-adapters` — `HookAdapter` trait + claude / codex / gemini adapters
- `muxad`         — daemon binary
- `muxa`          — CLI binary (incl. `muxa watch` TUI)

See [`PROTOCOL.md`](PROTOCOL.md) for the wire-protocol contract.

## Development

```bash
# build + test + lint
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt   --all
```

CI runs `fmt`, `clippy`, `test` (Linux + macOS), MSRV check, and
`cargo-deny`. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full guide —
especially the step-by-step for adding a new agent adapter.

Regenerate the demo GIF (requires [`vhs`](https://github.com/charmbracelet/vhs)):

```bash
vhs docs/demo.tape
```

## License

Dual-licensed under [MIT](LICENSE-MIT) **or** [Apache 2.0](LICENSE-APACHE),
at your option.
