# Agent collaboration

Muxa can treat a tmux window as a collaboration room and broker durable
request/reply messages between its top-level agents. tmux supplies topology;
the message body travels through muxad's owner-only Unix socket and local
mailbox, not through terminal screen scraping.

## The whole model

- One tmux window is one collaboration room. In the managed Muxa model, that
  window is the current Run's coordination boundary, not the durable Work or
  external issue identity.
- An **agent** speaking through MCP or `muxa msg` is the represented sender: it
  is the pane it runs in, and the reply routes back there and wakes it.
- `muxa watch` is not an agent. It is the **operator console** — the human at
  the keyboard is the sender, whatever the pane the popup was opened from
  happens to contain.
- Select any agent; `m` sends and `M` opens the mailbox (`b` remains an alias).

So the normal agent-to-agent workflow is: put two agents in one window and let
them message each other over MCP. The operator workflow is separate and needs
no setup: press `prefix+s` anywhere, point at a row, press `m`.

### The console

`muxa watch` sends as `console`, a sender with a fixed identity and no pane of
its own. That has three consequences worth knowing:

- **Every row is a target, including the pane you opened watch from.** There is
  no self-send to guard against, because the sender is you and not that pane's
  agent.
- **It works from a bare shell.** The launch pane no longer has to host a
  tracked agent for messaging to be available.
- **Replies are not routed back to you.** A console has no pane to wake, so a
  reply simply lives on the request in the recipient's mailbox. Read it by
  putting the cursor on that row and pressing `M` — the `incoming` tab is the
  selected agent's mailbox, and `sent` is the console's own dispatch log across
  every target. `i` (claim) and `e` (reply) likewise act *as* the selected
  agent, since claiming and replying are the recipient's moves.

The console borrows the room of the window it was opened from, so window-scoped
peer selection still resolves the agents in front of you, but its identity does
not change with it — one operator, one `sent` thread.

`from` therefore identifies whose mailbox authority created the request and
where its reply returns; it does not claim that the represented agent process
made the IPC call itself. Requests now carry separate `provenance`: the local
surface (`watch`, MCP, CLI, or dashboard), OS-observed PID/UID, pane recovered
from the process environment or ancestry (with the evidence type), and whether
that pane matches the asserted origin. A console request records the pane the
operator dialled from, so the audit trail still says where it came from. Wake
prompts make the same distinction explicit: a recipient sees `from console via
muxa watch (caller %N, pid …)`.

## Enable

Collaboration is off until you grant it, because an incoming request can wake a
peer by typing a short prompt into its pane. `muxa init` asks for that grant
along with everything else it touches, and the `standard` preset includes it:

```bash
muxa init --component collaboration
```

That writes the block below to `config.toml` (`muxa init --component
collaboration --uninstall` removes it). Hand-editing works just as well:

```toml
[collaboration]
enabled = true
wake = "idle_only" # or "never" for pull-only delivery
# Optional: "notice" (default) or "full" for direct request delivery.
wake_payload = "notice"
```

An existing `wake` is never overwritten — a deliberate `never` means "give me
the mailbox but stay out of my panes", and re-running `muxa init` respects it.

Restart `muxad` and register `muxa mcp` with each agent host:

```bash
claude mcp add --scope user muxa -- muxa mcp
codex mcp add muxa -- muxa mcp
```

Codex users should then add the following line under the generated
`[mcp_servers.muxa]` table in `~/.codex/config.toml`:

```toml
env_vars = ["RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE", "MUXA_SOCKET"]
```

Restart agents that were already running so they reload their MCP server list.
Once connected, muxa's initialization instructions surface same-window peers
as reviewers or narrowly scoped delegated subagents. An agent can call
`muxa_collaboration_guide` to retrieve the contract again.

If `muxa doctor` reports a pane as synthetic, that agent does not yet have a
stable session identity and is omitted from room participants and request
targets. Submit a prompt to trigger a hook event; if it remains synthetic,
restart the agent and check again. This prevents a request from being pinned to
a placeholder identity that the later real session could never claim.

Existing `prefix+s` watch bindings need no additional shortcut after upgrading.

Claude's MCP process inherits native pane-host variables; Codex forwards them
via the `env_vars` allowlist above. For an older default-endpoint Codex
registration, muxa also attempts to recover the pane from process ancestry
across active backends. muxad validates the resolved origin against its live
agent and pane registries; collaboration tools do not accept an arbitrary
sender pane.

## Addressing and CLI

Agents sharing `(tmux socket, stable window id)` are peers. `peer` selects the
only other agent; with three or more participants use `%N` or `pane:%N`.
`scope = "window"` refuses cross-window targets; `scope = "host"` widens an
explicit `pane:%N` target to other windows and sessions. Requests are also
pinned to the target's current agent session, so a new process reusing that
pane cannot inherit old work.

An exact agent session can register a room-local alias and advisory roles:

```bash
muxa identity set --alias reviewer --role review --role rust
muxa identity show
muxa msg send @reviewer "review the auth change" --kind review
muxa msg send role:rust "investigate this lifetime error"
muxa identity clear
```

Aliases are unique among live peers in a room. Roles may be shared, but a
`role:<name>` target is accepted only when exactly one peer matches; ambiguity
is refused. Identity never follows a later agent that reuses the pane.

```bash
muxa peers
muxa msg send peer "review the auth change" --kind review
muxa msg send peer "review the risks in this plan" --kind review \
  --air-ref '{"artifact_id":"urn:air:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","profile":"https://open330.github.io/air/profiles/1.0.0/plan-native-cli","label":"CAL-6924 plan","locator":{"display":".air/cal-6924-plan.air.json","disclosure":"local-only"}}'
muxa msg inbox
muxa msg reply req_... "review complete" --status completed
muxa msg wait req_... --timeout-secs 300
muxa msg list --mailbox sent
muxa msg cancel req_... # queued requests only
```

The same lifecycle is available interactively in `muxa watch`: `m` sends to the
selected room peer, `b` opens non-claiming mailbox history, `i` claims the
inbox, and `e` replies. `muxa dashboard` provides the richer workspace-card form
of the same controls.

Inside the composer, `Ctrl-E` cycles how the text leaves: `read-only` and
`execute` are the contract carried by a durable request, while `just send`
types the text into the pane as raw keystrokes — no request, no reply, no
contract. The composer title makes the difference loud: contract modes show
the kind and mode badges, just-send shows `▷ SEND · keystrokes` and nothing
else, because a QUESTION badge over raw keystrokes would claim a contract
that does not exist.

The watch composer gives `? QUESTION`, `◆ REVIEW`, `▶ TASK`, and `! NOTICE`
distinct colors. `○ READ-ONLY` delegates investigation and an answer only;
`● EXECUTE` authorizes commands and file changes. These modes are contracts
delivered to the receiver, not commands executed directly by muxa.

`question` and `review` are read-only contracts by default. Use `--execute`
and one or more `--path` arguments to explicitly delegate edits. Path scopes
are advisory collaboration contracts, not an OS sandbox; separate git
worktrees remain the safest choice for concurrent edits.

## MCP tools

| Tool | Purpose |
| --- | --- |
| `muxa_collaboration_guide` | Retrieve reviewer, question, delegated-subagent, and AIR handoff contracts. |
| `muxa_room_context` | Identify self, list same-window peers, and show unread count. |
| `muxa_call_peer` | Expand a registered skill, select a peer, send a durable request, optionally wait, and explicitly spawn when confirmed. |
| `muxa_peer_report` | Read the newest completed report from a prior peer request, or retrieve an exact request id. |
| `muxa_set_identity` | Replace this exact session's room-local alias and roles. |
| `muxa_send_message` | Create a durable peer request. |
| `muxa_inbox` | Claim/read requests for this exact agent session. |
| `muxa_list_messages` | List incoming, sent, or all requests without claiming. |
| `muxa_reply` | Return a completed/blocked/declined/failed response. |
| `muxa_wait_reply` | Event-driven wait for the structured terminal response. |
| `muxa_cancel_message` | Cancel a sent request while it is still queued. |

## Natural calls from an agent conversation

A Claude or Codex agent connected to `muxa mcp` treats `@peer` and
`@muxa-peer` as reserved Muxa collaboration expressions. Provider mentions such
as `@codex`, a unique `@alias`, `role:name`, or a natural “call a colleague”
request for new work map to `muxa_call_peer`. A registered skill can be included
as `/name`:

```text
@peer review the current changes
@codex /review-plan-feedback using commit abc123 as context
```

Possessive references such as “`@peer`'s report”, “the peer reply”, or “resolve
the peer findings” map to `muxa_peer_report` first, so the agent evaluates the
actual structured mailbox response rather than fabricating external review
state. Without an explicit PR number or GitHub PR URL in user-provided or
grounded context, peer review language must not invoke GitHub PR/review tools or
invent a PR; repository/cwd context alone is insufficient. If Muxa tools are
unavailable, the correct recovery is to restart the agent, never to substitute
GitHub.

The high-level tool keeps the mailbox semantics below while removing the need
for the model to compose several low-level calls. It defaults to
`kind=review`, `work_mode=read_only`, selects a healthy peer deterministically,
and waits for the structured reply. Execute mode requires an explicit task
authorization. If no eligible peer exists, Muxa asks for confirmation instead
of silently creating one; `spawn_if_missing=true` is valid only after that
confirmation. Restart existing agents after changing skills or upgrading Muxa
because their MCP process loads tools and templates at startup.
For a confirmed automatic spawn, Muxa arms the daemon transition subscription
before creating the pane and re-reads room context only when that pane's agent
registers; there is no fixed 500 ms registration loop.

Waiting is a single blocking MCP call, not a model-driven polling loop. muxad
subscribes to a monotonic durable-mailbox revision and re-reads the exact
request only when that revision changes or at the final timeout boundary. If a
new client reaches an older daemon that rejects `collaboration_wait`, it uses a
bounded `collaboration_get` compatibility loop inside that same call, without
additional model turns. If a
call is sent with `wait=false`, the sender can continue independently; muxad
reacts to the reply and injects one short notification after the sender becomes
idle. Agents should not monitor Muxa-managed peers with `sleep`, raw
`tmux capture-pane`, or repeated status/capture calls.

## Reviewers and delegated subagents

For substantial work, call `muxa_collaboration_guide` and
`muxa_room_context` first. Send reviewers `kind=review` with
`work_mode=read_only`. Send implementation work as `kind=task` with
`work_mode=execute` and a narrow, non-overlapping path scope. Continue
independent work while the peer handles the request, then wait for the reply,
verify it, and integrate it.

The receiver should claim its inbox promptly, honor kind/work mode/paths, and
always produce a terminal `muxa_reply`. Avoid concurrent edits to the same
files; use separate worktrees when scopes cannot be isolated.

## AIR artifact handoff and visualization

Requests and replies can attach up to eight typed AIR 1.0 artifact references
through `air_artifacts`. The watch and dashboard mailboxes color the first
reference by profile and show its input/output direction, short digest, label,
and display-only locator in the detail view.

The exact supported profiles are:

- `https://open330.github.io/air/profiles/1.0.0/workflow-skill` → `AIR WORKFLOW`
- `https://open330.github.io/air/profiles/1.0.0/plan-native-cli` → `AIR PLAN`
- `https://open330.github.io/air/profiles/1.0.0/trace-native-run` → `AIR TRACE`
- `https://open330.github.io/air/profiles/1.0.0/trace-session-snapshot` → `AIR SESSION`

Artifact IDs must be `urn:air:sha256:` followed by a 64-character lowercase
SHA-256 digest. A locator has `local-only` or `redacted` disclosure and is only
a display hint, never file or execution authority. muxa validates reference
syntax but does not claim artifact conformance: validate, edit, and graph the
artifact in AIR Workbench. Do not invent a collaboration trace profile or put
prompts, messages, filesystem paths, or provider identifiers into an AIR
session snapshot.

## Wake-up safety

The mailbox is persisted to `$XDG_DATA_HOME/muxa/collaboration.json` before
delivery. With `idle_only`, muxad injects only when the exact
hook-authoritative participant is `Idle`. It never injects into `Working`,
`WaitingInput`, `WaitingChoice`, or `Error` panes. Synthetic screen-detected
agents have no stable session identity, so they are not room participants or
auto-wake targets.

`wake_payload = "notice"` is the default. It injects a short mailbox prompt;
the recipient calls `muxa_inbox`, which atomically claims and returns the body.
`wake_payload = "full"` instead claims one queued request before any terminal
side effect, then injects a structured envelope containing request id, source,
kind, work mode, paths, AIR references, and the original body. The recipient
must not call inbox for that request, so one tool round and its JSON envelope
are removed. Only one direct request is submitted per idle agent generation;
the next waits for a real Idle transition. Terminal reply bodies always remain
in the mailbox in both modes.

Direct delivery deliberately makes the request body part of terminal and
agent prompt history. Use `notice` for secrets that should remain only in the
private mailbox. A body containing terminal control characters automatically
falls back to `notice` rather than being transformed or pasted unsafely.
Direct delivery records whether prompt text was written
before the separate Enter keystroke. After interruption, muxad retries only
Enter when text is known to be buffered; if writing was uncertain, it falls
back to a short inbox recovery notice instead of injecting the body twice. A
manual inbox read supersedes either recovery path.

Reading a terminal result through `muxa_wait_reply` acknowledges it and
prevents a later reply wake. Room context reports incoming unread requests and
unacknowledged replies separately. `muxa_list_messages` is non-claiming history;
the sender may cancel only while a request remains `queued`, before the
recipient has claimed it.

Every collaboration IPC operation (context, identity, send, inbox, list,
reply, get, wait, and cancel) is also appended to
`$XDG_DATA_HOME/muxa/collaboration-audit.ndjson`. The `0600` ledger records the
represented origin/session, OS-observed caller, target/request id, and outcome,
but never duplicates message or reply bodies. `muxa msg list --json` and
`muxa_list_messages` expose the creation provenance stored on each request;
requests created before this field existed simply omit it.

Provenance remains audit evidence, not a permission gate. An observed/asserted
pane mismatch is recorded as `mismatched` but does not block the operation, so
host-scoped explicit-pane routing and the existing high-authority owner-only
socket workflow remain unchanged.
