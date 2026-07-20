# muxa MCP server

`muxa mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
stdio server that turns muxa into a **control plane** for coding agents. An
agent that speaks MCP (Claude Code, and any other MCP host) can ask muxa what
the *other* agents are doing, send one of them a prompt, read its screen, and
block until it changes state — the orchestration primitives that close the gap
with agent-native multiplexers whose socket API agents can drive.

Every tool proxies the daemon over its existing owner-only unix socket, so the
MCP server adds no new surface area: it can only do what a shell on the same
machine could already do through `muxa`.

## Wire it into Claude Code

Start the daemon (`muxad`) first — the MCP server **refuses to start** when
the socket is unreachable, with a clear error, so an agent never talks to a
dead control plane. Then register it:

```bash
claude mcp add muxa -- muxa mcp
```

That runs `muxa mcp` as a stdio MCP server. Point it at a non-default socket
with the global flag or env var if you run an isolated daemon:

```bash
claude mcp add muxa -- muxa --socket /run/user/1000/muxa.sock mcp
# or
MUXA_SOCKET=/run/user/1000/muxa.sock claude mcp add muxa -- muxa mcp
```

Verify from Claude Code with `/mcp` (the `muxa` server should list five
tools). Other MCP hosts: run `muxa mcp` as a stdio server command in their
config.

## Implementation

Hand-rolled JSON-RPC 2.0 over stdio — a **tools-only** server implementing
`initialize`, `tools/list`, `tools/call`, `ping`, and the `initialized`
notification. That subset is small and stable, so muxa implements it directly
on its existing `serde_json`/`tokio` deps rather than pulling the `rmcp` SDK's
dependency tree (which would have to clear MSRV 1.88 and the workspace's
cargo-deny policy). No new dependencies. Protocol revision: `2024-11-05`.

## Tools

| Tool | Arguments | Does |
| --- | --- | --- |
| `muxa_status` | — | Snapshot of every tracked agent: state, pane, session, model, last prompt, last notification. |
| `muxa_recent_prompts` | `pane?`, `limit?` | Recent prompt-history entries (newest first), optionally scoped to one pane. |
| `muxa_send_prompt` | `pane`, `text`, `submit?` | Inject `text` into a pane; `submit` (default `true`) presses Enter to commit the line. |
| `muxa_capture_pane` | `pane` | Capture the visible contents of a pane. |
| `muxa_wait_for_change` | `timeout_secs?`, `pane?` | Block until an agent's state changes (or timeout); returns the transition. |

`muxa_send_prompt` is refused (surfaced to the model as a tool error) when the
pane's backend can't inject keystrokes — e.g. zellij, where CLI `write-chars`
only reaches the focused pane. tmux and herdr support it.

Pane ids carry their host namespace: tmux `%12`, herdr `herdr:p1`. Use the
`pane` field from `muxa_status` verbatim.

## Orchestration examples

**"What is everyone doing?"**

```
muxa_status
→ [{ "pane": "%12", "state": "waiting_input", "last_prompt": "refactor auth", ... },
   { "pane": "%18", "state": "working", ... }]
```

**Unblock an agent that's waiting on input**

```
muxa_send_prompt { "pane": "%12", "text": "yes, proceed", "submit": true }
```

**Hand off a task and wait for it to land**

```
muxa_send_prompt   { "pane": "%18", "text": "run the test suite", "submit": true }
muxa_wait_for_change { "pane": "%18", "timeout_secs": 120 }
→ { "changed": true, "from": "working", "to": "waiting_input", "agent": { ... } }
muxa_capture_pane  { "pane": "%18" }        # read the result on screen
```

`muxa_wait_for_change` returns `{ "changed": false, "reason": "timeout" }`
when nothing matching happened in the window, so a polling loop stays cheap and
bounded (default 30 s, max 600 s).

## Safety

`muxa_send_prompt` is a **control action** — it types into another agent's
pane. The IPC socket is owner-only (`0600`), so only your user can reach it and
there is no network exposure; treat socket access as equivalent to shell
access. The server never starts against an absent daemon. See `PROTOCOL.md`
(Control methods) for the underlying `send_prompt` / `capture` / `subscribe`
IPC.
