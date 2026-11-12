# tend MCP server

`tend mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
stdio server that turns tend into a **control plane** for coding agents. An
agent that speaks MCP (Claude Code, and any other MCP host) can ask tend what
the *other* agents are doing, send one of them a prompt, read its screen, and
block until it changes state — the orchestration primitives that close the gap
with agent-native multiplexers whose socket API agents can drive.

Most tools proxy the daemon over its existing owner-only unix socket. The
deterministic agent launcher invokes same-user tmux locally, using the same
socket routing as the CLI. The MCP server adds no network listener: it can only
do what a shell on the same machine could already do through `tend` and tmux.

For a current agent location, start with `tend_agent_find(query="TASK_KEYWORDS")`.
It returns bounded, ranked candidates with exact host/socket/session/window/pane
keys and short evidence, without dumping full topology or transcripts. All
keywords must match; extract identifiers or a few keywords from prose. It covers
local observed panes; remote Fleet hosts need a separate Fleet query. Matches
do not prove task ownership/completion, and historical records never imply a
current location. Use `tend_status(pane=...)` for a candidate's further details.

`tend_agent_search` searches historical Claude/Codex conversations and tool
executions with a Korean/English natural-language query; `tend_agent_read`
expands a hit into surrounding events and paired command results. These tools
read local provider files without the daemon and report search coverage. See
[agent-record search](AGENT_RECORDS.md) for query interpretation, evidence
handling, indexing limits and CLI equivalents.

## Wire it into Claude Code and Codex

The recommended setup is `tend init --preset standard`, which installs hooks,
collaboration, the daemon manager, and the shared agent integration. For an
existing installation, add the latter with:

```bash
tend init --component agent-instructions,agent-skills,agent-mcp
```

This installs canonical guidance beside tend config, links the personal skill,
and merges each detected host's user MCP registration, including Codex `env_vars`.
See [Global agent integration](AGENT_INTEGRATION.md) for updates and removal.
Use `tend reload mcp` for Codex shared app-server MCP connections; the host
queues the refresh. `--all` discovers stale owners and distinguishes standalone
Codex CLI processes from the shared app-server. A standalone `codex resume`
does not belong to that app-server's loaded connections, even when both use the
same profile. Reconnect in the owning client, or restart/resume it when idle.
The reload command leaves its running work and stdio MCP child intact.
Other clients and global instructions may also require an agent restart. The manual setup below remains available.

Start the daemon (`tendd`) first. The MCP server still starts if the socket
is unreachable, with a warning on stderr: agent hosts launch an MCP server
once and never retry, so refusing would leave that agent without tend tools
for good. Until the daemon is back, tool calls return its error. After an
upgrade an idle server re-executes the new binary on the same stdio and
sends `notifications/tools/list_changed`, so agents pick up new tools
without restarting. Then register it:

```bash
claude mcp add --scope user tend -- tend mcp
codex mcp add tend -- tend mcp
```

Codex sanitizes the environment of stdio MCP processes unless variables are
explicitly allowed. After `codex mcp add`, add `env_vars` to its generated
table in `~/.codex/config.toml`:

```toml
[mcp_servers.tend]
command = "tend"
args = ["mcp"]
env_vars = ["RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE", "TEND_SOCKET"]
```

This preserves the exact pane, tmux/cmux/rmux endpoint, and non-default tend socket.
For existing Codex registrations, tend can recover a default-endpoint pane by
walking the MCP process ancestry across active backends back to the pane shell.
Explicit forwarding remains the reliable configuration for custom or multiple
endpoints.

That runs `tend mcp` as a stdio MCP server. Point it at a non-default socket
with the global flag or env var if you run an isolated daemon:

```bash
claude mcp add --scope user tend -- tend --socket /run/user/1000/tend.sock mcp
# or
claude mcp add --scope user tend -e TEND_SOCKET=/run/user/1000/tend.sock -- tend mcp
```

Restart agents that were already running, then verify with `claude mcp list`
or `codex mcp list` (the `tend` server should list twenty-five tools). Other MCP
hosts can run `tend mcp` as a stdio server command in their config.

At initialization tend tells the agent both the user's configured launch
preferences and how to use same-window peers as a reviewer, focused question
target, or delegated subagent. The agent can call `tend_guide` to retrieve the
surface/agent defaults and `tend_collaboration_guide` to retrieve the peer
contract again at any time. Configure the former under `[mcp.guide]`; see
[CONFIGURATION.md](CONFIGURATION.md#mcp-orchestration-guide).
The same instructions map conversational `@peer`, provider mentions, aliases,
roles, and registered `/skills` to `tend_call_peer`.

### Physical-host Fleet tools

The same MCP server can always operate the controller through host `local`.
When `[fleet] enabled = true`, it additionally operates the cached remote
host/session/window/pane hierarchy:

```text
tend_fleet_status      { "selector": "environment=production" }
tend_fleet_capture     { "host": "local", "pane": "%12" }
tend_fleet_capture     { "host": "dev", "pane": "%12" }
tend_fleet_send_prompt { "host": "dev", "pane": "%12",
                         "text": "Start the interactive session", "submit": true }
tend_fleet_call_peer   { "host": "dev", "pane": "%12",
                         "intent": "review", "body": "Review the current change" }
tend_fleet_wait_reply  { "host": "dev", "pane_key": { "window": { ... },
                         "pane_id": "%12" }, "request_id": "..." }
```

The host and pane are always explicit. Ambiguous pane ids are rejected. The
local in-process adapter and remote relay both verify the complete identity
again, and an observe-mode remote host rejects every control action. Durable
Fleet calls never auto-select or spawn a remote pane. They record the request
on the physical target node and return a structured reply; if the first wait
times out, pass the returned exact `pane_key` and `request_id` to
`tend_fleet_wait_reply`. Fleet calls use tendd's cache/control plane; the MCP
subprocess never opens SSH itself. The remote `tend` must advertise both
`collaboration` and `collaboration_get`. See [FLEET.md](FLEET.md).

Reply waits are shared in the controller by `(host, exact pane_key, request_id)`.
The controller subscribes before its first read, then reconciles mailbox changes,
stream gaps and host reconnections. An unchanged five-minute wait performs one
request read; topology refreshes do not cause mailbox reads. The subscription is
released when its last waiter disconnects or expires, and each waiter has its own
deadline. Hosts advertising `mailbox_watch` use this event path; older hosts use
1/2/4/8/16/30-second capped backoff. Older controllers also use capped backoff.
Mailbox invalidations are host-scoped, so changes to other requests on that host
can still trigger reads; updates from other hosts are ignored.

For ongoing work, keep guidance and progress on the existing durable request:

```text
tend_fleet_update_request { "host": "dev", "pane_key": <returned pane_key>,
                           "request_id": "req_...", "body": "Verified guidance within the agreed scope" }
tend_update_request      { "request_id": "req_...", "body": "Implementation done; running tests" }
tend_fleet_wait_reply    { "host": "dev", "pane_key": <returned pane_key>,
                           "request_id": "req_...", "after_update": 0 }
```

`after_update` returns `reason: "updated"` for a newer sequence without marking the
request completed. Resume using `next_after_update`, or omit `after_update` to wait
only for a terminal reply. Updates do not claim work, widen its execution/path
scope, or inject terminal prompts. Recipients check their request updates through
`tend_list_messages` at agreed checkpoints; urgent interruption remains an explicit
separate action. Finish with exactly one `tend_reply`.

Each update is limited to 8 KiB of UTF-8 text; a request retains at most 32 updates
and 32 KiB of update bodies, with monotonic sequence numbers even after trimming.
Link larger artifacts. Consecutive identical updates by the same participant are
idempotent. Only the existing sender/recipient may update a nonterminal request.
The optional `initiator` records the originating agent when its context resolves;
it is advisory metadata, not additional authorization or a routable console pane.
Fleet calls return explicit progress/guidance routes on the original request.
Remote updates require the `collaboration_update` capability and control mode;
there is no terminal-text fallback when that capability is absent.

## Implementation

Hand-rolled JSON-RPC 2.0 over stdio — a **tools-only** server implementing
`initialize`, `tools/list`, `tools/call`, `ping`, and the `initialized`
notification. That subset is small and stable, so tend implements it directly
on its existing `serde_json`/`tokio` deps rather than pulling the `rmcp` SDK's
dependency tree (which would have to clear MSRV 1.89 and the workspace's
cargo-deny policy). No new dependencies. Protocol revision: `2024-11-05`.

### Concurrency and framing

Requests are read line-by-line from stdin and **each is dispatched on its own
task**, so a long-running tool (`tend_wait_for_change` or `tend_wait_reply`, up
to 600 s) never blocks unrelated traffic — a `ping` or `tools/list` issued
while a wait is outstanding is answered immediately. Responses may therefore
interleave in time; that is expected for concurrent JSON-RPC, and the `id`
echoed on each response lets the client correlate. Output framing stays
strict: the shared stdout writer is locked across each whole `write` + newline,
so two concurrent responses never splice mid-line (one JSON object per line).
Codex may hand a call lasting over 30 seconds back as a background cell; resume
that same cell with the host wait function and `yield_time_ms=60000`. The cell
yield is a host transport boundary, not a reason to start another Tend wait.

Framing is robust against non-conforming input rather than silently dropping
it:

- A line that isn't valid JSON → `-32700` parse error, `id: null`.
- A **batch array** → a single `-32600` error (`"batch requests are not
  supported"`). tend **does not implement JSON-RPC batching**; MCP hosts in
  practice send one request per line, so batches are rejected explicitly.
- A **bare value** (number/string/bool/null) → `-32600`, `id: null`.
- An **object with an `id`** but a missing/invalid `jsonrpc` or `method` →
  `-32600` addressed to that `id`; an unknown method → `-32601`.
- An **object with no `id`** is a notification and draws no response (per
  JSON-RPC), even if otherwise malformed — there is no `id` to reply to.

## Tools

The listed surface is converging on one tool per job
([MCP_SURFACE](proposals/MCP_SURFACE.md)). Retired names keep working for
agents that already hold their schema; they are not listed, and their result
carries `replaced_by` with the tool to use instead:

| Retired | Use |
| --- | --- |
| `tend_wait_reply`, `tend_fleet_wait_reply`, `tend_wait_for_change`, `tend_dispatch_status` | `tend_wait` |
| `tend_collaboration_guide`, `tend_room_context` | `tend_guide` (`topic=collaboration` / `room`) |
| `tend_agent_find`, `tend_capture_pane`, `tend_recent_prompts`, `tend_fleet_status`, `tend_fleet_capture` | `tend_status` (`query`, `pane`+`include_capture`, `history_limit`, `host`) |
| `tend_fleet_send_prompt` | `tend_send_prompt` with `host` |
| `tend_dispatch_work` | `tend_start_work` with `commit` |
| `tend_agent_read` | `tend_agent_search` with `event_id` |
| `tend_send_message`, `tend_fleet_call_peer` | `tend_call_peer` (`exact`/`intent=notice`, `host`) |
| `tend_list_messages`, `tend_peer_report` | `tend_inbox` (`view`) |
| `tend_fleet_update_request`, `tend_cancel_message`, `tend_withdraw_reply_wake` | `tend_update_request` (`host`, `action`) |

| Tool | Arguments | Does |
| --- | --- | --- |
| `tend_status` | `pane?`, `full?`, `include_capture?`, `history_limit?`, `max_capture_lines?`, `query?`, `limit?`, `refresh?`, `exclude_sessions?`, `host?`, `selector?` | Compact fleet/agent summary by default, the complete topology with `full=true`, or one focused pane with its screen and recent prompts. `query` finds where agents working on a task are (ranked panes with evidence excerpts; a match is not proof of ownership). `history_limit` without `pane` lists recent prompts across panes. `host` reads a Fleet host: with `pane`, that pane's screen there; `host=all` is the cached Fleet summary (`selector` filters hosts); `host=local` is this daemon. |
| `tend_guide` | `topic?` (`collaboration`, `room`, `launch`), `workflows?` | Orientation. `collaboration` (default): identity, peers, preferences, guardrails and running-daemon capabilities; `workflows=true` adds detailed examples. `room`: identity, same-window peers and unread counts. `launch`: the configured surface, agent, options, direction and guidance. |
| `tend_wait` | `id?`, `kind?`, `timeout_secs?`, `after_update?`, `host?`, `pane_key?`, `pane?`, `until?`, `include_capture?`, `max_capture_lines?` | Wait on one handle: a request id (its final reply; a Fleet request this MCP process sent needs only the id), a dispatch UUID (its status), a swap job, or, with `pane` and no id, that pane's next state change (`until=settled` skips intermediate working states). |
| `tend_start_agent` | `agent?`, `options?`, `workspace?`, `work?`, `role?`, `task?`, `placement?`, `target?`, `cwd?`, `prompt?`, `name?`, `direction?` | Create/reuse a workspace session and work window, or start an allowlisted agent in a lower-level tmux surface. `agent` is optional only when configured in `[mcp.guide]`. |
| `tend_start_work` | `work`, `pipeline?`, `workspace?`, `cwd?`, `body?`, `skill?`, `context?`, `no_ticket?`, `refresh?`, `dry_run?` | Bring a work item's window to the state its pipeline declares: resolve the ticket, route it, create the missing agent panes, and deliver `body` to the ones already running. With `commit` (and `dispatch_id`, `workspace`, `body`, optional `host`/`selector`/`plan`), dispatches through the Fleet coordinator instead: an isolated worktree at that commit and the configured pipeline (`dry_run` plans it). Launched means started, not completed. |
| `tend_swap_account` | `action?`, `job?`, `request_key?`, `detach?`, `wait_secs?`, `host?`, `targets?`, `all?`, `provider?`, `to?`, `force?`, `continue_text?`, `continue_when?`, `continue_evidence?`, `no_continue?`, `dry_run?` | Move Claude/Codex agents onto another account and resume them in place, here or on a control-mode Fleet host (`tend swap`; see [ACCOUNT_SWAP.md](ACCOUNT_SWAP.md)). `action` is `swap` (default), `retry` (an ended job's safely retryable targets), `status` or `cancel` (with `job`). A start or retry runs under a request key — pass your own `request_key` to hold it before anything is sent, or use the one returned — and every result, errors included, carries it with a `recovery` call (`action=status`); the tool never sends a swap twice. `status`/`cancel` return the job as it is now unless `wait_secs` (≤120) asks to wait. |
| `tend_manage_tmux` | `action`, `pane?`, `workspace?`, `work?`, `confirm?` | List/show/close managed workspaces and work; interrupt/terminate an agent pane. |
| `tend_send_prompt` | `pane`, `text`, `submit?`, `socket?`, `session_id?`, `force?`, `host?` | Inject `text` into an idle agent's pane; `submit` (default `true`) presses Enter to commit the line. `force=true` types into a non-idle or untracked pane. With `host`, types into a pane on that control-mode Fleet host (not gated on Idle yet; `session_id`/`socket` are refused there). |
| `tend_update_request` | `request_id`, `body?`, `action?` (`update`, `cancel`, `withdraw_wake`), `request_ids?`, `host?`, `pane_key?` | `update` (default): append progress/guidance as an existing participant; a Fleet request this MCP process sent needs no `host`/`pane_key`. `cancel`: your own request while still queued. `withdraw_wake`: stop being woken for late replies to your own `request_ids`. |
| `tend_call_peer` | `target?`, `intent?` (`review`, `question`, `task`, `notice`), `body?`, `skill?`, `context?`, `execute?`, `paths?`, `wait?`, `timeout_secs?`, `exact?`, `notify?`, `expects_reply?`, `spawn_if_missing?`, `context_bound?`, `spawn_agent?`, `spawn_timeout_secs?`, `user_authorized?`, `parent_request_id?`, `artifacts?`, `links?`, `withdraw_reply_wakes?`, `task_contract?`, `supersedes_request_id?`, `host?`, `pane?`, `dispatch_id?` | Every durable request to another agent. By default Tend selects or creates a same-window worker for `target`. `exact=true` or `intent=notice` delivers exactly to `target` (a protected primary keeps it queued). `host` sends to one exact pane on a control-mode Fleet host. Returns at once (`wait=false` is the default); the reply arrives as a wake, read it with `tend_inbox(request_id)` or `tend_wait(id)`. `thread_id`/`workspace_id`/`work_id`/`run_id` are stamped from your pane (still accepted). |
| `tend_set_identity` | `alias?`, `roles?` | Replace this exact session's room-local alias and role set; empty input clears it. |
| `tend_file_result` | `summary`, `commit[]?`, `decision_needed?`, `options[]?`, `recommendation?`, `evidence[]?`, `request_id?`, `key?` | File the durable result record for finished work as an open review item, targeted at the caller's managed Work alias or else its session (`request_id` targets a collaboration request). Commit shorthand resolves against the caller's checkout. Not approval; nothing waits on it. See [review items](REVIEW.md). |
| `tend_request_attention` | `body`, `action?`, `dispatch_id?`, `parent_request_id?`, `decision_needed?`, `options[]?`, `recommendation?`, `evidence[]?` | Request required human approval, choice or information; delivered to connected Fleet operator clients. Reuse a UUID dispatch ID on retry; wait for and read the actual human reply. |
| `tend_inbox` | `request_id?`, `full?`, `view?` (`incoming`, `sent`, `all`, `report`), `target?`, `limit?`, `offset?`, `include_body?`, `after_update?` | Without `view`: claim and read incoming work (one `request_id` from a wake, or the queue). With `view`: read without claiming: history (default newest 20, max 100; `request_id` + `include_body` for one instruction, `after_update` for progress), or `report` for the newest peer reply (`target` filters its author). CLI: `tend msg accept`. |
| `tend_reply` | `request_id`, `status`, `body`, `artifacts?` | Return a structured terminal response. |

`tend_send_prompt` types only into an agent whose registry state is `idle`.
A working agent, one at a permission prompt or choice menu, and a pane with no
tracked agent are refused with `target_not_idle` and nothing is typed: text
typed into a running turn merges with it, and a prompt or menu takes it as an
answer. Pass `force=true` only when a person explicitly asked for that
keystroke. Work another agent should pick up belongs in the mailbox
(`tend_send_message` / `tend_call_peer`), which waits for the recipient to be
idle by itself. The gate is the daemon capability `send_prompt_state_gate_v1`;
against an older daemon, which would ignore it, a non-forced call is refused
rather than typed. Operator clients (`tend
watch`, Tend for Mac) omit the field and keep typing on a person's action;
`tend_fleet_send_prompt` is not gated yet.

`tend_send_prompt` is also refused (surfaced to the model as a tool error) when the
pane's backend can't inject keystrokes — e.g. zellij, where CLI `write-chars`
only reaches the focused pane. tmux, cmux, rmux, and herdr support targeted input.

For an omitted `socket`, MCP uses its own `$TMUX`/`$RMUX` endpoint. Untracked
tmux/rmux pane ids without caller server context are refused with `unknown pane`;
callers outside those hosts must supply `socket` explicitly. Registered legacy
rows without a socket retain their backend-default behavior. Optional
`session_id` requires a matching non-stopped agent on that pane and endpoint,
so a forgotten or replaced session cannot silently target another agent. This
precondition checks the registry at dispatch time, not the process atomically.

Pane ids carry their host namespace: tmux `%12`, rmux `rmux:%12`, herdr
`herdr:p1`. Use the `pane` field from `tend_status` verbatim.

### Natural peer calls

Once the MCP server is connected, type requests naturally in the agent
conversation:

```text
@peer review the current changes
@codex /review-plan-feedback using commit abc123 as context
@reviewer check the auth path for races
@peer's report: summarize the findings and apply only the valid ones
```

`@peer` and the more explicit `@tend-peer` are reserved Tend collaboration
expressions. New work maps to `tend_call_peer`; possessive references to an
existing report, reply, findings, or conversation map to `tend_peer_report`
first. Tend does not intercept terminal keystrokes. `target="auto"` prefers a
healthy peer from a
different provider, then applies stable state/provider/numeric-pane ordering.
Use a provider, pane id, unique alias, or `role:name` when the recipient
matters. Ambiguous roles are refused instead of guessed.

Tend report language never implies a GitHub PR. The agent must not invoke a
GitHub PR/review workflow unless the user or grounded conversation context
provides an explicit PR number or GitHub PR URL; knowing the repository or cwd
alone is insufficient. It must never invent a PR, issue, or review thread.
“Resolve the peer feedback” means evaluate each Tend finding, apply valid
advice, record why rejected advice was not used, and optionally send that
decision back to the peer. If the Tend tools are missing, restart the agent;
GitHub is not a fallback transport.

The safe default is `intent="review"` with a read-only contract. `execute=true`
is accepted only with `intent="task"`, so a conversational review cannot
silently acquire edit authority. Calls reuse eligible workers and create one
when missing by default (`collaboration.auto_spawn_workers=true`). New workers
use configured provider permissions; bypass flags are added only with explicit
`collaboration.worker_bypass_permissions=true`. `spawn_if_missing` overrides
automatic creation per call. When creation is disabled, no request is sent and
the tool returns `reason="worker_spawn_disabled"`.
Naming a primary routes generic work to a worker of the same provider. Set
`context_bound=true` when the exact target conversation is necessary; primary
requests then remain queued for explicit acceptance. Low-level exact-pane
messages retain their pinned recipient.
Automatic spawning defaults to the other provider when possible. Before it
creates the pane, the tool arms tendd's transition stream and then re-reads the
authoritative room only when the spawned pane registers. This removes the old
500 ms peer-registration polling loop and closes the fast-start lost-wakeup
race. Registration is a grace period rather than a precondition:
`spawn_timeout_secs` (default 10) bounds it, and a pane that has not registered
by then receives the request as a *pending pane* recipient — tendd delivers it
once the pane reads idle and the session that registers there adopts it. The
result then carries `peer_pending: true`; keep waiting with `tend_wait_reply`
on the returned `request_id` and never fall back to `tmux capture-pane`.

Registered message skills can be selected with or without a leading slash:

```text
tend_call_peer(target="auto", intent="review",
               skill="review-plan-feedback",
               context="Review commit abc123", wait=false)

tend_call_peer(target="@codex", intent="task", execute=true,
               body="Add regression tests only",
               paths=["crates/auth/tests/**"])
```

The MCP process reads `[message.skills]` at startup. Restart an already-running
agent after `tend skill add/remove`, after editing the table, or after upgrading
Tend so it loads the current skills and tool definitions.

### Versioned task contracts

`tend_send_message`, `tend_call_peer` and `tend_fleet_call_peer` accept an optional
`task_contract` and `supersedes_request_id`. `tend_reply` accepts a matching
`report`. Contract completion checks attempt identity, requested revision and
reported criterion evidence; it never independently verifies success. Superseding
read-only investigations preserves late reports as history without waking the
sender or completing the successor. See [Task contracts](COLLABORATION_CONTRACTS.md).

### Delivery observations and declared work results

Tend stores and routes messages; it does not evaluate task success. The default
CLI/MCP views separate transport observations from the recipient's declaration:

- `delivery_state`: `queued`, `claimed`, `delivered` (one-way message),
  `reply_received`, `cancelled`, or `expired`. `closed` covers legacy terminal records without a reply.
- `final_reply_received`: a recipient submitted a final reply, including a
  failure, refusal or blocker. This never asserts work success.
- `reported_status`: the recipient's explicit `completed`, `blocked`, `declined`
  or `failed` declaration, present only with a final reply. `reply.status` carries
  the same declaration in the stored reply.
- `reason: timeout`: the wait ended without a final reply or transport closure;
  it neither fails the work nor authorizes repeating it.

For `tend_call_peer` reviews with an automatic target, Codex routes to Claude
and Claude routes to Codex. Explicit provider/pane/alias targets take precedence;
other intents retain their existing selection. Launch authorization still applies.

Progress uses `tend_update_request`; `tend_reply` is the explicit final response.
Process idle/stopped observations and words in a reply body never establish task
completion. Tend performs no model call to judge a reply.

Mutation receipts and waits omit the original body and audit metadata. Initial
send receipts retain routing. Follow-up update/reply/cancel acknowledgements
return correlation, transport state and any progress cursor or interruption;
read an exact request with `full=true` when its complete record is needed.
`tend_peer_report` returns `result` with a receipt and the new reply; it does not
repeat the prompt. `full=true` opts into the legacy full stored request under `request`.
Inbox retains the entire instruction/authority/path/artifact contract.
Lists return at most 20 metadata records by default, with `has_more` and
`next_offset`; set `include_body=true` with an exact `request_id` for instructions (or explicitly use `full=true` for bulk diagnostics). An exact `request_id`
read also returns an existing reply. For progress, use that ID with
`after_update`, then resume with `next_after_update`; `first_retained_sequence`
identifies the beginning of retained progress. Incremental reads return the compact
status receipt and new updates, without routing, original instructions or final
reply bodies, even with `include_body=true`. They do not mark an unseen final
reply as read or suppress its notification. Retrieve that reply with
`tend_wait_reply` or `tend_peer_report`. Incremental reads require a daemon with
`collaboration_peek`; older daemons reject them rather than acknowledge an unseen
reply. `full=true` returns legacy records (including reply acknowledgement).

The durable store and daemon IPC keep their existing lifecycle statuses for
compatibility. Agent-facing default views no longer expose a generic
`completed` boolean or treat notice delivery as task success. Existing consumers
of full records can opt into `full=true` (CLI `--full`). Consumers of the old
wait `completed`/`status` fields must migrate to the explicit fields above.

MCP initialization contains only a short routing/transport contract because
some hosts repeat server instructions on every tool. Launch preferences,
registered message skills and guardrails are available once through
`tend_collaboration_guide`; detailed workflows require `workflows=true`. This
avoids repeating policy in every tool or returning unused workflow examples.
All JSON tool results are compact on the wire.

### Staffing a whole ticket

`tend_start_agent` creates one pane. `tend_start_work` creates a *team*, and
it is the only tool that can tell an agent it already started from one it
still has to:

```text
tend_start_work {
  "work": "eng-1234",
  "body": "fix the double reap so the reconciler stops eating live panes",
  "skill": "review-plan",
  "context": "tests already pass on main"
}
→ { "work": "ENG-1234", "workspace": "webapp", "pipeline": "triad",
    "cwd": "~/worktrees/eng-1234",
    "ticket": { "id": "ENG-1234", "title": "...", "url": "..." },
    "launched": [ { "alias": "plan", "pane": "%12", ... }, ... ],
    "plan": { "steps": [...], "unclaimed": [] } }
```

`body`, `skill`, and `context` compose exactly as they do in
`tend_call_peer` — the registered skill expands first, then the body, then
the context under an `Invocation context:` heading. Nothing else is
attached; tend never folds in a transcript or a diff on the caller's
behalf.

The same request has two deliveries, chosen by what already exists:

| Pipeline alias | What happens to `body` |
| --- | --- |
| has no pane | goes into that agent's launch prompt |
| has a live pane | is typed into that pane |

So calling it twice does not duplicate the team — the second call steers it.
A pane no pipeline alias claims is reported in `plan.unclaimed` and never
touched. `dry_run: true` returns the same plan without creating or sending
anything, which is the right first call when you are unsure what a route
would do. Requires `[[route]]` and `[pipeline.*]` configuration, or an
explicit `pipeline`. See [PIPELINE.md](PIPELINE.md).

### Deterministic agent launch

Use `tend_start_agent` when creating the tmux surface is the deterministic
part of a larger task. The calling model supplies only the agent profile,
location, and optional first task; tend handles the exact tmux invocation and
returns the new pane id for later `tend_capture_pane`, `tend_send_prompt`, and
`tend_wait_for_change` calls.

When the request does not choose a surface or provider, call `tend_guide` (or
use the same preferences already delivered during MCP initialization).
Configured `placement`, `agent`, `options`, and `direction` become real defaults
for omitted `tend_start_agent` arguments. Explicit tool arguments win. Extra
options are shell-quoted one argument at a time and appended to Tend's built-in
provider profile before the initial prompt.

```text
tend_start_agent {
  "agent": "codex",
  "workspace": "tend",
  "work": "TEST-0001",
  "role": "reviewer",
  "cwd": "/home/june/personal/tend",
  "prompt": "Review the current changes and report findings only"
}
→ { "pane": "%24", "workspace": "tend", "work": "TEST-0001",
    "session": "tend", "window": "@7", "created_work": false, ... }
```

The managed binding policy is **tmux session = Workspace, window = current Run,
pane = agent session**. Work remains a durable logical object and any Linear,
GitHub, or Jira issue is only an external reference. With `work`, tend derives
`workspace` from the cwd basename when it is omitted, creates the session and
first Run window when needed, and stores identity in tmux user options at the
matching scope. Later calls reuse that Run window and add an agent pane; a conflicting cwd is refused. Different Work IDs
can use different worktree paths inside one workspace session. `role` and `task`
become durable pane metadata. Without `work`, `placement` retains the lower-level
detached pane/window/session behavior. `cwd` must already exist.

Lifecycle operations stay in this same MCP server:

```text
tend_manage_tmux(action="list_workspace")
tend_manage_tmux(action="show_workspace", workspace="tend")
tend_manage_tmux(action="list_work", workspace="tend")
tend_manage_tmux(action="interrupt_agent", pane="%24")
tend_manage_tmux(action="terminate_agent", pane="%24", confirm=true)
tend_manage_tmux(action="close_work", workspace="tend", work="TEST-0001", confirm=true)
tend_manage_tmux(action="close_workspace", workspace="tend", confirm=true)
```

Terminate and close refuse unconfirmed or unmanaged targets. Tend does not
expose arbitrary shell or generic tmux commands.

Profiles are allowlisted (`claude`, `codex`, `agy`, `opencode`) rather than
accepting an arbitrary shell command. They intentionally use each CLI's
bypass/yolo mode. In particular, `codex` expands the local `cx` behavior to
`codex --yolo` directly, so it does not depend on an interactive shell loading
aliases.

The collaboration tools are higher-level than `tend_send_prompt`: tendd pins
each request to the target's current agent session, persists it before wake-up,
and restricts routing to the caller's stable tmux window. Request and reply
wake prompts are sent only to idle agents. The default `wake_payload =
"operator_full"` delivers operator-console bodies directly while these
agent-originated MCP requests remain body-free mailbox notices. `notice` keeps
every body in the mailbox; `full` directly delivers every safe request body.
Reply wakes remain body-free. Enable the tools with `[collaboration] enabled =
true`; see `docs/COLLABORATION.ko.md`.

For rooms with several agents, call `tend_set_identity` once per agent and
route with `@alias` or `role:<name>`. Aliases must be unique among live peers;
role routing refuses multiple matches instead of picking one arbitrarily.

### Peer reviewer or delegated subagent

For substantial work, start with `tend_collaboration_guide`, then inspect
`tend_room_context`. Continue independent work while the peer handles a narrow,
non-overlapping request:

```text
# reviewer: findings only, no edits
tend_send_message(target="peer", kind="review", work_mode="read_only",
                  paths=["crates/auth/**"], body="Review this change for races and regressions")

# delegated subagent: explicitly authorized edits, with a narrow path scope
tend_send_message(target="peer", kind="task", work_mode="execute",
                  paths=["crates/auth/tests/**"], body="Add regression tests; do not edit production code")
```

The receiver should claim promptly with `tend_inbox`, honor the request kind,
work mode, and paths, and always terminate with `tend_reply`. The sender waits
with `tend_wait_reply`, then verifies and integrates the result. Avoid
concurrent edits to the same files; use separate worktrees when scopes cannot
be isolated.

`tend_wait_reply` holds one MCP tool call while tendd waits on the durable
mailbox revision. Elapsed wait time causes no repeated model turns, status
calls, or pane captures. If the sender chose `wait=false`, tendd instead reacts
to the terminal mailbox revision and wakes the sender once its pane is idle.
When a new CLI connects to an older daemon without `collaboration_wait`, that
same tool call uses a bounded internal compatibility poll; the model is not
sampled again. Current daemons always use the event-driven path.
Do not replace either path with `sleep`, raw `tmux capture-pane`, or repeated
status/capture calls.

## Orchestration examples

**"What is everyone doing?"**

```
tend_status
→ [{ "pane": "%12", "state": "waiting_input", "last_prompt": "refactor auth", ... },
   { "pane": "%18", "state": "working", ... }]
```

**Unblock an agent that's waiting on input**

```
tend_send_prompt { "pane": "%12", "text": "yes, proceed", "submit": true, "force": true }
```

Only on a person's explicit instruction: a waiting agent is not idle, so this
needs `force`.

**Hand off a task and wait for it to land**

```
tend_send_prompt   { "pane": "%18", "text": "run the test suite", "submit": true }
tend_wait_for_change { "pane": "%18", "until": "settled",
                       "include_capture": true, "timeout_secs": 120 }
→ { "changed": true, "matched": true, "to": "idle", "capture": "..." }
```

`tend_wait_for_change` returns `{ "changed": false, "reason": "timeout" }`
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

`tend_send_prompt` reports whether the line was committed. When the underlying
control path can confirm that the text was injected but the submitting Enter
was **not** delivered, the result says so explicitly ("text sent but not
submitted") so the caller doesn't assume the agent started working.

## Safety

`tend_send_prompt` is a **control action** — it types into another agent's
pane. `tend_start_agent` is also a control action: it creates a tmux surface
and starts an allowlisted CLI in bypass/yolo mode. It does not accept arbitrary
commands, but callers should still provide trusted paths and prompts.
`tend_manage_tmux` accepts exact managed identities only; terminating a pane or
closing a work window or whole workspace session additionally requires
`confirm=true`. The IPC
socket is owner-only (`0600`), so only your user can reach it and there is no
network exposure; treat socket and MCP access as equivalent to shell access.
The server never starts against an absent daemon. See `PROTOCOL.md` (Control
methods) for the underlying `send_prompt` / `capture` / `subscribe` IPC.

### Checking collaboration cost locally

Full-body history queries can opt into `max_bytes`, a stored-payload budget
clamped to 64 KiB–4 MiB. A page always contains at least one whole record; its
continuation cursor prevents skipped history when the budget ends a page early.
Watch keeps conservative full-body pages until budget support is acknowledged.

Watch subscribes to durable mailbox revisions and reuses unchanged history.
Disconnected/older servers fall back to reconciled reads. It uses 256-character
previews in bounded pages without dropping historical
rows or aggregate counts. Conversation selection loads full details with a
non-acknowledging read. Full-body searches load complete content on demand,
then reuse unchanged records. Existing watch processes must be reopened after
installing an updated CLI; MCP reload refreshes supported agent connections.

Optional IPC read views (`metadata`, `preview`, `outcome`, `progress`) retain
normal authorization and read/peek semantics. Full execution contracts remain
the wire default. Progress preserves sequence metadata while omitting already
seen update text; outcome reads retain the complete final reply. Original
messages are never modified by a transport view.

CLI and MCP message lists use server-side filters and pagination. Default
list IPC responses omit task/reply bodies, paths, artifacts and update text;
reply status and the latest update sequence remain available. `--full` and
exact request reads preserve complete content. Older daemons fall back to
local filtering and can still transfer the full mailbox until reloaded.

New daemon audit rows include `response_bytes` (encoded IPC bytes including
newline) and `elapsed_us` (time through encoding including waits, excluding audit IO and
socket writes). The report aggregates measured samples separately: older rows
remain unmeasured, rather than counting as zero. These are daemon wire metrics;
final CLI/MCP output and billed model tokens can differ.

Run `python3 scripts/collaboration-cost-audit.py --hours 24` from the checkout
to break existing daemon audit calls down by client and operation. `watch` calls
are UI traffic; raw IPC counts are not model token counts. To measure recorded
model-facing tool results, add `--session /path/to/session.jsonl` for each Codex
or Claude session (repeat as needed). `--json` produces a comparable report.

The report counts UTF-8 output bytes, maximum response sizes, and identical
results for the same arguments within five minutes. Repeats are investigation
candidates: polling can be necessary and provider caching/billing is unknown.
Shell/exec measurements are explicitly marked mixed because they can contain
other output. Coverage is limited to supplied files; absent session data cannot
establish token savings. Reports contain aggregates and tool names, not message
bodies or tool arguments. The script only reads existing local logs; it adds no
runtime telemetry, network request, mailbox read, or scheduled task.

### Recovering closed cached clients

`tend reload mcp --codex-home DIR --reconnect --json` changes a Tend-owned
connection generation as well as checking the installed binary revision. Use it
when a closed client is retained even though its binary revision is unchanged.
It asks the existing host to rebuild its Tend clients; agent sessions stay running.
The version-checked write preserves user environment settings and creates a
private backup. Ordinary reload does not rotate the generation.

Reload reports `transport_connected` separately from
`current_revision_connections` (host-cached handshake/schema information). An
in-place process upgrade can leave old handshake data behind. `queued` remains
unconfirmed; an API acknowledgement or a current executable alone does not
prove every loaded conversation has a working tool connection.

Before automatic replacement, MCP and tendd probe the exact candidate with
`--check-config`. Rejection or timeout keeps the live process and records a
warning. The same rejected binary/configuration pair is not repeatedly spawned.
A changed binary or configuration is eligible for another check. Daemon restart
IPC uses this check before signalling shutdown. On Linux, native binary handoff uses a held descriptor for the validated inode,
with the original launch path retained as argv[0]. Atomic replacements of its
pathname cannot substitute an unvalidated image. Destructive in-place writes
are refused. Scripts and other platforms recheck the canonical path before
execution; a change after daemon drain can require a cold start. Validation
checks for binary/configuration changes during the probe; configuration edits
after shutdown has begun remain a separate concurrent-writer concern.
