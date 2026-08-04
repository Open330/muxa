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

Verify from Claude Code with `/mcp` (the `muxa` server should list ten
tools). Other MCP hosts: run `muxa mcp` as a stdio server command in their
config.

## Implementation

Hand-rolled JSON-RPC 2.0 over stdio — a **tools-only** server implementing
`initialize`, `tools/list`, `tools/call`, `ping`, and the `initialized`
notification. That subset is small and stable, so muxa implements it directly
on its existing `serde_json`/`tokio` deps rather than pulling the `rmcp` SDK's
dependency tree (which would have to clear MSRV 1.88 and the workspace's
cargo-deny policy). No new dependencies. Protocol revision: `2024-11-05`.

### Concurrency and framing

Requests are read line-by-line from stdin and **each is dispatched on its own
task**, so a long-running tool (`muxa_wait_for_change`, up to 600 s) never
blocks unrelated traffic — a `ping` or `tools/list` issued while a wait is
outstanding is answered immediately. Responses may therefore interleave in
time; that is expected for concurrent JSON-RPC, and the `id` echoed on each
response lets the client correlate. Output framing stays strict: the shared
stdout writer is locked across each whole `write` + newline, so two
concurrent responses never splice mid-line (one JSON object per line).

Framing is robust against non-conforming input rather than silently dropping
it:

- A line that isn't valid JSON → `-32700` parse error, `id: null`.
- A **batch array** → a single `-32600` error (`"batch requests are not
  supported"`). muxa **does not implement JSON-RPC batching**; MCP hosts in
  practice send one request per line, so batches are rejected explicitly.
- A **bare value** (number/string/bool/null) → `-32600`, `id: null`.
- An **object with an `id`** but a missing/invalid `jsonrpc` or `method` →
  `-32600` addressed to that `id`; an unknown method → `-32601`.
- An **object with no `id`** is a notification and draws no response (per
  JSON-RPC), even if otherwise malformed — there is no `id` to reply to.

## Tools

| Tool | Arguments | Does |
| --- | --- | --- |
| `muxa_status` | — | Snapshot of every tracked agent: state, pane, session, model, last prompt, last notification. |
| `muxa_recent_prompts` | `pane?`, `limit?` | Recent prompt-history entries (newest first), optionally scoped to one pane. |
| `muxa_send_prompt` | `pane`, `text`, `submit?` | Inject `text` into a pane; `submit` (default `true`) presses Enter to commit the line. |
| `muxa_capture_pane` | `pane` | Capture the visible contents of a pane. |
| `muxa_wait_for_change` | `timeout_secs?`, `pane?` | Block until an agent's state changes (or timeout); returns the transition. |
| `muxa_room_context` | — | Identify self, list same-window peers, and report unread request/reply counts. |
| `muxa_set_identity` | `alias?`, `roles?` | Replace this exact session's room-local alias and role set; empty input clears it. |
| `muxa_send_message` | `target`, `body`, `kind?`, `work_mode?`, `paths?` | Create a durable same-window peer request. |
| `muxa_inbox` | — | Claim and read requests addressed to this exact agent session. |
| `muxa_list_messages` | `mailbox?` | List incoming, sent, or all requests without claiming. |
| `muxa_reply` | `request_id`, `status`, `body`, `artifacts?` | Return a structured terminal response. |
| `muxa_wait_reply` | `request_id`, `timeout_secs?` | Wait for a structured peer response. |
| `muxa_cancel_message` | `request_id` | Cancel a sent request while it is still queued. |

`muxa_send_prompt` is refused (surfaced to the model as a tool error) when the
pane's backend can't inject keystrokes — e.g. zellij, where CLI `write-chars`
only reaches the focused pane. tmux and herdr support it.

Pane ids carry their host namespace: tmux `%12`, herdr `herdr:p1`. Use the
`pane` field from `muxa_status` verbatim.

The collaboration tools are higher-level than `muxa_send_prompt`: muxad pins
each request to the target's current agent session, persists it before wake-up,
and restricts routing to the caller's stable tmux window. Request and reply
wake prompts contain no message body and are sent only to idle agents. Enable
the tools with `[collaboration] enabled = true`; see
`docs/COLLABORATION.ko.md`.

For rooms with several agents, call `muxa_set_identity` once per agent and
route with `@alias` or `role:<name>`. Aliases must be unique among live peers;
role routing refuses multiple matches instead of picking one arbitrarily.

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

**Semantics:** it returns on the **first observed change OR a reconciled
post-lag state match.** Two signals race under the deadline: the daemon's
push transition stream, and a periodic snapshot **reconcile** (every 2 s) that
compares the target pane's current state against a baseline captured when the
wait began. The reconcile is a backstop for a daemon-side broadcast *lag*: a
lag can drop the very transition being waited for, which would otherwise be
misreported as a timeout. A change surfaced by the reconcile (rather than the
live stream) carries `"reconciled": true`, and its `from` may be `null` when
the pane wasn't present at baseline. This poll backstop is deliberately
self-contained in the MCP server, so it holds regardless of whether the
subscribe stream ever exposes the lag marker to this consumer.

`muxa_send_prompt` reports whether the line was committed. When the underlying
control path can confirm that the text was injected but the submitting Enter
was **not** delivered, the result says so explicitly ("text sent but not
submitted") so the caller doesn't assume the agent started working.

## Safety

`muxa_send_prompt` is a **control action** — it types into another agent's
pane. The IPC socket is owner-only (`0600`), so only your user can reach it and
there is no network exposure; treat socket access as equivalent to shell
access. The server never starts against an absent daemon. See `PROTOCOL.md`
(Control methods) for the underlying `send_prompt` / `capture` / `subscribe`
IPC.
