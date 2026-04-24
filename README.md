# muxa

Agent CLI observability & orchestration layer for tmux.

`muxa` is an external daemon that watches terminal-based agent CLIs
(Claude Code, opencode, codex, gemini-cli, ...) running inside tmux panes
and surfaces their state — who's working, who's waiting for input,
what the last prompt was — via the tmux status line, popup menus, and a
thin CLI.

It does **not** fork tmux. It talks to tmux via control mode and to each
agent via that agent's own hook / event-emission system (falling back to
a shell wrapper when the agent doesn't expose one).

## Status

Pre-alpha scaffold. Nothing works yet.

## Architecture

```
agent CLIs  ──hooks/wrapper──▶  muxad (daemon)  ──control mode──▶  tmux
                                    │
                                    ├── state store
                                    ├── unix socket IPC
                                    └── notifier
                                          │
                                          ▼
                              muxa CLI, status line, popups
```

## Layout

- `src/bin/muxad.rs` — long-running daemon
- `src/bin/muxa.rs`  — user-facing CLI (`muxa status`, `muxa recap`, ...)
- `src/event.rs`     — event schema shared across adapters
- `src/state.rs`     — in-memory agent registry
- `src/ipc.rs`       — unix-socket server for CLI + adapter ingress
- `src/tmux.rs`      — tmux control-mode client
- `src/adapter/`     — per-agent adapters (claude, opencode, codex, gemini)

## Build

```
cargo build
```
