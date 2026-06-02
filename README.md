<div align="center">

<img src="assets/logo.svg" alt="muxa" width="260" />

**Agent CLI observability & orchestration layer for tmux.**

See which agents are working, waiting, idle, or blocked from your tmux
status line, a live TUI, desktop notifications, and local reports.

[![CI](https://github.com/Open330/muxa/actions/workflows/ci.yml/badge.svg)](https://github.com/Open330/muxa/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-informational)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-beta-yellow)

**English** · [한국어](README.ko.md)

</div>

---

`muxa` is a small daemon and CLI for observing AI coding agents running
inside terminal multiplexer panes. It supports Claude Code, OpenAI Codex,
and Google Gemini CLI through their existing hook/event systems, then
correlates those events with tmux panes and sessions.

It does not fork tmux or modify agent binaries. tmux is the default full
backend; zellij has a CLI baseline and a planned richer plugin path.

<div align="center">
  <img src="docs/demo.gif" alt="muxa demo" width="900" />
</div>

> [!IMPORTANT]
> Beta. Event ingest, the daemon, CLI, live TUI, desktop notifications,
> stats, and reports work end-to-end, but APIs may still change before 1.0.

## What You Get

| Surface | What it does |
| --- | --- |
| `muxa status-line` | One-line tmux `status-right` summary for the active pane. |
| `muxa watch` | Full-screen TUI for agents and panes, with attach and prompt composition. |
| `muxa attend` | Jump to the agent blocked on input/choice/error longest. |
| `muxa stats` / `muxa report` | Local analytics for prompt history, agent state duration, tmux foreground time, and human thinking time. |
| `muxa activity` | Raw duration ledger query for debugging exactly what fed stats/report. |
| Dashboard | Optional loopback HTTP UI with SSE live updates. |
| Notifications | Optional desktop alerts when agents need attention. |

## Quick Start

Requires Rust 1.88+, tmux 3.x, and a Unix-like OS.

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh
```

Or from source:

```bash
git clone https://github.com/Open330/muxa.git
cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa-cli --locked
muxa init
```

Verify:

```bash
muxad &
muxa status
muxa watch
```

For install modes, `muxa init` presets, systemd, manual hook wiring, and
rollback details, see [docs/INSTALL.md](docs/INSTALL.md).

## Core Commands

| Command | Purpose |
| --- | --- |
| `muxa status` | Human-readable table of tracked agents. |
| `muxa watch [--view pane\|session]` | Live TUI picker/dashboard. |
| `muxa attend [--cycle] [--list]` | Focus or list agents needing attention. |
| `muxa status-line [--pane %N]` | tmux status-line output. |
| `muxa recap [--pane %N]` | Recent prompts from retained disk history. |
| `muxa stats --since today` | Summary table; group by day/project/agent/session. |
| `muxa report --since week` | Markdown stats report. |
| `muxa activity --type agent\|tmux\|human` | Raw activity ledger intervals. |
| `muxa sync` | Backfill the registry by scanning tmux panes. |
| `muxa init` | Interactive install/uninstall wizard. |
| `muxad` | Daemon process. |

Common stats queries:

```bash
muxa stats --since today --group-by session
muxa stats --since yesterday --group-by project
muxa report --since week
muxa activity --since today --type human
```

`--since` accepts `today`, `yesterday`, `week`, rolling durations like
`24h`/`7d`/`4w`, RFC3339 timestamps, and `all`. See
[docs/ACTIVITY.md](docs/ACTIVITY.md) for ledger semantics, including
`HUMAN` and `THINK`.

## Supported Agents

| Agent | Status | Config |
| --- | --- | --- |
| Claude Code | Supported | `~/.claude/settings.json` |
| OpenAI Codex | Supported | `~/.codex/config.toml` |
| Google Gemini CLI | Supported | `~/.gemini/settings.json` |
| opencode | Planned | [tracking issue](https://github.com/Open330/muxa/issues/14) |

## More Docs

| Topic | Doc |
| --- | --- |
| Install and wiring | [docs/INSTALL.md](docs/INSTALL.md) |
| Live TUI and prompt composer | [docs/WATCH.md](docs/WATCH.md) |
| Stats, reports, activity ledger | [docs/ACTIVITY.md](docs/ACTIVITY.md) |
| Configuration reference | [docs/CONFIGURATION.md](docs/CONFIGURATION.md) |
| Web dashboard | [docs/DASHBOARD.md](docs/DASHBOARD.md) |
| External sinks | [docs/SINKS.md](docs/SINKS.md) |
| Zellij plan | [docs/ZELLIJ.md](docs/ZELLIJ.md) |
| Architecture and development | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## License

MIT OR Apache-2.0.
