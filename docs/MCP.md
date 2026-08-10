# muxa MCP server

`muxa mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
stdio server that turns muxa into a **control plane** for coding agents. An
agent that speaks MCP (Claude Code, and any other MCP host) can ask muxa what
the *other* agents are doing, send one of them a prompt, read its screen, and
block until it changes state — the orchestration primitives that close the gap
with agent-native multiplexers whose socket API agents can drive.

Most tools proxy the daemon over its existing owner-only unix socket. The
deterministic agent launcher invokes same-user tmux locally, using the same
socket routing as the CLI. The MCP server adds no network listener: it can only
do what a shell on the same machine could already do through `muxa` and tmux.

## Wire it into Claude Code and Codex

Start the daemon (`muxad`) first — the MCP server **refuses to start** when
the socket is unreachable, with a clear error, so an agent never talks to a
dead control plane. Then register it:

```bash
claude mcp add --scope user muxa -- muxa mcp
codex mcp add muxa -- muxa mcp
```

Codex sanitizes the environment of stdio MCP processes unless variables are
explicitly allowed. After `codex mcp add`, add `env_vars` to its generated
table in `~/.codex/config.toml`:

```toml
[mcp_servers.muxa]
command = "muxa"
args = ["mcp"]
env_vars = ["TMUX", "TMUX_PANE", "MUXA_SOCKET"]
```

This preserves the exact pane, tmux socket, and non-default muxa socket.
For existing Codex registrations, muxa can recover a default-socket pane by
walking the MCP process ancestry back to the pane shell. Explicit forwarding
remains the reliable configuration for custom or multiple sockets.

That runs `muxa mcp` as a stdio MCP server. Point it at a non-default socket
with the global flag or env var if you run an isolated daemon:

```bash
claude mcp add --scope user muxa -- muxa --socket /run/user/1000/muxa.sock mcp
# or
claude mcp add --scope user muxa -e MUXA_SOCKET=/run/user/1000/muxa.sock -- muxa mcp
```

Restart agents that were already running, then verify with `claude mcp list`
or `codex mcp list` (the `muxa` server should list sixteen tools). Other MCP
hosts can run `muxa mcp` as a stdio server command in their config.

At initialization muxa tells the agent how to use same-window peers as a
reviewer, focused question target, or delegated subagent. The agent can call
`muxa_collaboration_guide` to retrieve that contract again at any time.

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
| `muxa_status` | `pane?`, `include_capture?`, `history_limit?`, `max_capture_lines?` | Snapshot all agents, or observe one pane with its screen and recent prompts in one call. |
| `muxa_recent_prompts` | `pane?`, `limit?` | Recent prompt-history entries (newest first), optionally scoped to one pane. |
| `muxa_start_agent` | `agent`, `work?`, `role?`, `task?`, `placement?`, `target?`, `cwd?`, `prompt?`, `name?`, `direction?` | Create/reuse a work session or start an allowlisted agent in a detached tmux surface. |
| `muxa_manage_tmux` | `action`, `pane?`, `work?`, `confirm?` | List/show managed work, interrupt/terminate an agent pane, or close a work session. |
| `muxa_send_prompt` | `pane`, `text`, `submit?` | Inject `text` into a pane; `submit` (default `true`) presses Enter to commit the line. |
| `muxa_capture_pane` | `pane` | Capture the visible contents of a pane. |
| `muxa_wait_for_change` | `timeout_secs?`, `pane?`, `until?`, `include_capture?` | Wait for any change or a focused settled/idle/blocked/stopped state, optionally returning the screen. |
| `muxa_collaboration_guide` | — | Show the recommended reviewer, question, delegated-subagent, incoming-work, and AIR handoff contracts. |
| `muxa_room_context` | — | Identify self, list same-window peers, and report unread request/reply counts. |
| `muxa_set_identity` | `alias?`, `roles?` | Replace this exact session's room-local alias and role set; empty input clears it. |
| `muxa_send_message` | `target`, `body`, `kind?`, `work_mode?`, `paths?`, `air_artifacts?` | Create a durable same-window peer request. |
| `muxa_inbox` | — | Claim and read requests addressed to this exact agent session. |
| `muxa_list_messages` | `mailbox?` | List incoming, sent, or all requests without claiming. |
| `muxa_reply` | `request_id`, `status`, `body`, `artifacts?`, `air_artifacts?` | Return a structured terminal response. |
| `muxa_wait_reply` | `request_id`, `timeout_secs?` | Wait for a structured peer response. |
| `muxa_cancel_message` | `request_id` | Cancel a sent request while it is still queued. |

`muxa_send_prompt` is refused (surfaced to the model as a tool error) when the
pane's backend can't inject keystrokes — e.g. zellij, where CLI `write-chars`
only reaches the focused pane. tmux and herdr support it.

Pane ids carry their host namespace: tmux `%12`, herdr `herdr:p1`. Use the
`pane` field from `muxa_status` verbatim.

### Deterministic agent launch

Use `muxa_start_agent` when creating the tmux surface is the deterministic
part of a larger task. The calling model supplies only the agent profile,
location, and optional first task; muxa handles the exact tmux invocation and
returns the new pane id for later `muxa_capture_pane`, `muxa_send_prompt`, and
`muxa_wait_for_change` calls.

```text
muxa_start_agent {
  "agent": "codex",
  "work": "CAL-7041",
  "role": "reviewer",
  "cwd": "/home/june/personal/muxa",
  "prompt": "Review the current changes and report findings only"
}
→ { "pane": "%24", "work": "CAL-7041", "created_work": false, ... }
```

The managed policy is **tmux session = work/ticket, pane = agent, window =
layout only**. With `work`, muxa creates the session once and stores the exact
work id and cwd in tmux user options. Later calls reuse that session and add an
agent pane; a conflicting cwd is refused. `role` and `task` become durable pane
metadata. Without `work`, `placement` retains the lower-level detached
pane/window/session behavior. `cwd` must already exist.

Lifecycle operations stay in this same MCP server:

```text
muxa_manage_tmux(action="list_work")
muxa_manage_tmux(action="interrupt_agent", pane="%24")
muxa_manage_tmux(action="terminate_agent", pane="%24", confirm=true)
muxa_manage_tmux(action="close_work", work="CAL-7041", confirm=true)
```

Terminate and close refuse unconfirmed or unmanaged targets. Muxa does not
expose arbitrary shell or generic tmux commands.

Profiles are allowlisted (`claude`, `codex`, `gemini`, `opencode`) rather than
accepting an arbitrary shell command. They intentionally use each CLI's
bypass/yolo mode. In particular, `codex` expands the local `cx` behavior to
`codex --yolo` directly, so it does not depend on an interactive shell loading
aliases.

The collaboration tools are higher-level than `muxa_send_prompt`: muxad pins
each request to the target's current agent session, persists it before wake-up,
and restricts routing to the caller's stable tmux window. Request and reply
wake prompts contain no message body and are sent only to idle agents. Enable
the tools with `[collaboration] enabled = true`; see
`docs/COLLABORATION.ko.md`.

For rooms with several agents, call `muxa_set_identity` once per agent and
route with `@alias` or `role:<name>`. Aliases must be unique among live peers;
role routing refuses multiple matches instead of picking one arbitrarily.

### Peer reviewer or delegated subagent

For substantial work, start with `muxa_collaboration_guide`, then inspect
`muxa_room_context`. Continue independent work while the peer handles a narrow,
non-overlapping request:

```text
# reviewer: findings only, no edits
muxa_send_message(target="peer", kind="review", work_mode="read_only",
                  paths=["crates/auth/**"], body="Review this change for races and regressions")

# delegated subagent: explicitly authorized edits, with a narrow path scope
muxa_send_message(target="peer", kind="task", work_mode="execute",
                  paths=["crates/auth/tests/**"], body="Add regression tests; do not edit production code")
```

The receiver should claim promptly with `muxa_inbox`, honor the request kind,
work mode, and paths, and always terminate with `muxa_reply`. The sender waits
with `muxa_wait_reply`, then verifies and integrates the result. Avoid
concurrent edits to the same files; use separate worktrees when scopes cannot
be isolated.

### AIR artifact handoff

Collaboration requests and replies can carry typed references to existing AIR
1.0 artifacts. muxa transports and visualizes the reference; AIR Workbench
remains the validator, editor, and graph viewer. This does not turn muxa into
an AIR executor and does not make an unvalidated document conformant.

```json
{
  "artifact_id": "urn:air:sha256:<64 lowercase hex characters>",
  "profile": "https://open330.github.io/air/profiles/1.0.0/plan-native-cli",
  "label": "CAL-6924 execution plan",
  "locator": { "display": ".air/cal-6924-plan.air.json", "disclosure": "local-only" }
}
```

The exact supported profiles are workflow skill, native CLI plan, native run
trace, and metadata-only session snapshot. Locators are display-only hints,
never authority. Session snapshot references must not be used to smuggle
prompts, messages, filesystem paths, or provider identifiers into AIR data.

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
muxa_wait_for_change { "pane": "%18", "until": "settled",
                       "include_capture": true, "timeout_secs": 120 }
→ { "changed": true, "matched": true, "to": "idle", "capture": "..." }
```

`muxa_wait_for_change` returns `{ "changed": false, "reason": "timeout" }`
when nothing matching happened in the window, so a polling loop stays cheap and
bounded (default 30 s, max 600 s).

With `until=settled`, intermediate starting/working transitions are skipped;
the tool returns only for idle, waiting-input/choice, error, or stopped after
at least one state transition. `idle`, `blocked`, and `stopped` select narrower
targets. Non-`any` targets and `include_capture` require a pane.

With the default `until=any`, it returns on the **first observed change OR a
reconciled post-lag state match.** Two signals race under the deadline: the daemon's
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
pane. `muxa_start_agent` is also a control action: it creates a tmux surface
and starts an allowlisted CLI in bypass/yolo mode. It does not accept arbitrary
commands, but callers should still provide trusted paths and prompts.
`muxa_manage_tmux` accepts exact managed identities only; terminating a pane or
closing a whole work session additionally requires `confirm=true`. The IPC
socket is owner-only (`0600`), so only your user can reach it and there is no
network exposure; treat socket and MCP access as equivalent to shell access.
The server never starts against an absent daemon. See `PROTOCOL.md` (Control
methods) for the underlying `send_prompt` / `capture` / `subscribe` IPC.
