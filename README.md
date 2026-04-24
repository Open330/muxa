<div align="center">

# `muxa`

**Agent CLI observability & orchestration layer for tmux.**

See which agents are working, waiting, or idle — right from your status line.

[![CI](https://github.com/Open330/muxa/actions/workflows/ci.yml/badge.svg)](https://github.com/Open330/muxa/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-informational)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-pre--alpha-orange)

</div>

---

`muxa` is a small daemon that watches agent CLIs — Claude Code, OpenAI Codex,
Google Gemini CLI, opencode — running inside tmux panes and surfaces their
state to the tmux status line, popup menus, and a thin CLI.

It does **not** fork tmux. It talks to tmux via the tmux CLI and to each
agent via that agent's own hook / event-emission system.

```text
┌─ tmux status-right ─────────────────────────────────────────┐
│  ...  │ ⚙ claude_code │ ! codex │ · gemini_cli │  12:34     │
└───────┼───────────────┼─────────┼──────────────┴────────────┘
        │               │         └─ idle
        │               └─ waiting for input
        └─ working
```

```console
$ muxa status
PANE     KIND          STATE            MODEL     LAST PROMPT
%10      claude_code   working          Opus      refactor the ipc module to use …
%11      codex         waiting_input    -         -
%12      gemini_cli    idle             Gemini    summarize this PR
```

> [!IMPORTANT]
> Pre-alpha. Event ingest, adapters, daemon, and CLI work end-to-end with
> 11 tests green. APIs may still shift. opencode support is deferred.

## Contents

- [Features](#features)
- [Agent support](#agent-support)
- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Development](#development)
- [License](#license)

## Features

|                         |                                                                                   |
| ----------------------- | --------------------------------------------------------------------------------- |
| **Pan-agent**           | One daemon. One CLI. Four adapters (Claude · Codex · Gemini · opencode&nbsp;[†]). |
| **tmux-native**         | Pane correlation via `$TMUX_PANE`; status-line- / popup-ready output.             |
| **Zero coupling**       | No changes to tmux or to agent CLIs — just their existing hook systems.           |
| **Safe by default**     | Socket is `0600`; `SIGTERM` drains and unlinks; `unsafe_code = forbid`.           |
| **Versioned protocol**  | Explicit `PROTOCOL_VERSION`; mismatched clients are rejected.                     |
| **Fast**                | In-memory registry; no database, no external services.                            |

<sub>[†] opencode adapter is deferred — its integration is SSE / in-process
plugin-based, not shell-hook.</sub>

## Agent support

| Agent               | Integration                                        | Config file                 |
| ------------------- | -------------------------------------------------- | --------------------------- |
| Claude Code         | ✓ shell hooks + status-line Heartbeat              | `~/.claude/settings.json`   |
| OpenAI Codex        | ✓ shell hooks (Claude-protocol clone upstream)     | `~/.codex/config.toml`      |
| Google Gemini CLI   | ✓ shell hooks (Claude-compatible upstream)         | `~/.gemini/settings.json`   |
| opencode            | deferred — SSE subscription / TS plugin planned    | —                           |

## Install

Requires **Rust 1.88+**, **tmux 3.x**, and a Unix-y OS.

```bash
git clone https://github.com/Open330/muxa.git && cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa  --locked
```

That installs two bins to `~/.cargo/bin/`:

- `muxad` — the daemon
- `muxa`  — the user-facing CLI

Make sure `~/.cargo/bin` is on your `PATH` (it usually is).

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

Append to `~/.tmux.conf`:

```tmux
set -g status-interval 2
set -g status-right "#(muxa status-line --pane #{pane_id}) | #[fg=white]%H:%M"
```

Reload: `tmux source-file ~/.tmux.conf`.

### 4. Confirm

```console
$ muxa status
PANE     KIND          STATE          MODEL     LAST PROMPT
%10      claude_code   working        Opus      refactor …
```

## Commands

|                                       |                                                                        |
| ------------------------------------- | ---------------------------------------------------------------------- |
| `muxa status`                         | Human-readable table of all tracked agents.                            |
| `muxa status-line [--pane %N]`        | One-liner for tmux `status-right`; scoped to `$TMUX_PANE` by default.  |
| `muxa recap [--pane %N]`              | Show the last prompt for the given pane.                               |
| `muxa panes`                          | Debug: dump tmux pane inventory.                                       |
| `muxa hook <agent> --event <e>`       | Hook adapter entry point. Invoked by the agent CLIs themselves.        |
| `muxad`                               | The daemon. Listens on `$XDG_RUNTIME_DIR/muxa.sock` by default.        |

## Configuration

`muxad` reads `$XDG_CONFIG_HOME/muxa/config.toml` if it exists. All fields
have sensible defaults — see [`config.example.toml`](config.example.toml)
for the full schema.

<details>
<summary>Environment variables</summary>

| Variable       | Purpose                                                |
| -------------- | ------------------------------------------------------ |
| `MUXA_SOCKET`  | Override the unix socket path.                         |
| `MUXA_CONFIG`  | Override the config file path.                         |
| `RUST_LOG`     | Tracing filter. Example: `muxa=debug,tokio=warn`.      |

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
      ├── in-memory agent registry       └── status-line / popup / recap
      ├── GC task (stopped-agent TTL)
      └── graceful SIGTERM → drain → unlink socket
```

Five-crate workspace:

- `muxa-core`     — types, state, config, paths, errors (no I/O)
- `muxa-runtime`  — unix-socket IPC server/client + tmux CLI wrapper
- `muxa-adapters` — `HookAdapter` trait + claude / codex / gemini adapters
- `muxad`         — daemon binary
- `muxa`          — CLI binary

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

## License

Dual-licensed under [MIT](LICENSE-MIT) **or** [Apache 2.0](LICENSE-APACHE),
at your option.
