# muxa

Agent CLI observability & orchestration layer for tmux.

`muxa` is a small daemon that watches terminal-based agent CLIs
(Claude Code, OpenAI Codex, Google Gemini CLI, opencode) running in your
tmux panes and surfaces their state — who's working, who's waiting for
input, what the last prompt was, how much context is used — via the tmux
status line, popup menus, and a thin CLI.

It does **not** fork tmux. The daemon talks to tmux via the tmux CLI (and,
eventually, control mode) and to each agent via that agent's own
hook/event-emission system.

## Status

Pre-alpha. The event ingest path, daemon, CLI, and three hook adapters
(Claude, Codex, Gemini) work end-to-end. opencode is deferred (its
integration is SSE/plugin-based, not shell-hook).

## Quickstart

```bash
# build
cargo build --release --workspace

# run the daemon
./target/release/muxad

# wire up Claude Code by appending this to ~/.claude/settings.json
#   (see examples/claude-settings.json for the full snippet)
#
#   "hooks": {
#     "UserPromptSubmit": [{ "hooks": [{ "type": "command",
#         "command": "muxa hook claude --event user_prompt_submit" }]}],
#     "Stop":             [{ "hooks": [{ "type": "command",
#         "command": "muxa hook claude --event stop" }]}]
#   }

# inspect state
./target/release/muxa status

# tmux status-right snippet
#   set -g status-right '#(muxa status-line --pane #{pane_id})'
```

## Layout

```
muxa/
├── crates/
│   ├── muxa-core/       # types, state, config, paths, errors — no I/O
│   ├── muxa-runtime/    # unix-socket IPC server/client + tmux CLI wrapper
│   ├── muxa-adapters/   # HookAdapter trait + claude/codex/gemini adapters
│   ├── muxad/           # daemon binary
│   └── muxa/            # CLI binary
├── examples/            # drop-in agent configs
├── PROTOCOL.md          # wire-protocol specification
├── CONTRIBUTING.md      # development & extension guide
└── .github/workflows/   # fmt, clippy, test, MSRV, cargo-deny
```

See [`PROTOCOL.md`](./PROTOCOL.md) for the wire format and
[`CONTRIBUTING.md`](./CONTRIBUTING.md) for adding a new adapter.

## Design

- Events are the API. Adapters normalize agent-native events (Claude's
  `UserPromptSubmit`, Codex's `Stop`, Gemini's `Notification`) into a
  single `AgentEvent` type. The daemon never sees agent-specific shapes.
- One daemon per user. Unix-socket at `$XDG_RUNTIME_DIR/muxa.sock` with
  `0600` permissions. Graceful shutdown on `SIGTERM`.
- Hook adapters are just `muxa hook <agent> --event <name>` subcommands
  that read JSON on stdin, normalize, and send via the local socket. No
  agent modification needed — you just point it at the existing hook
  mechanism.
- The wire protocol is versioned (`PROTOCOL_VERSION=1`). Mismatched
  clients are rejected.

## License

Dual-licensed under [MIT](./LICENSE-MIT) or
[Apache 2.0](./LICENSE-APACHE) at your option.
