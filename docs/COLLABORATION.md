# Agent collaboration

Muxa can treat a tmux window as a collaboration room and broker durable
request/reply messages between its top-level agents. tmux supplies topology;
the message body travels through muxad's owner-only Unix socket and local
mailbox, not through terminal screen scraping.

## The whole model

- One tmux window is one collaboration room. In the managed Muxa model, that
  window is also the work/ticket boundary inside its workspace session.
- The focused agent is the **represented sender** when you open `prefix+s` watch.
- Select another agent in that window; `m` sends and `M` opens the mailbox
  (`b` remains an alias).

So the normal workflow is simply: put two agents in one window, focus the
sender, press `prefix+s`, choose the peer, and press `m`. A normal shell pane is
not an agent and cannot be the sender. The Dashboard is optional.

`from` therefore identifies whose mailbox authority created the request and
where its reply returns; it does not claim that the represented agent process
made the IPC call itself. Requests now carry separate `provenance`: the local
surface (`watch`, MCP, CLI, or dashboard), OS-observed PID/UID, pane recovered
from the process environment or ancestry (with the evidence type), and whether
that pane matches the asserted origin. Wake prompts make the same distinction
explicit.

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
| `muxa_set_identity` | Replace this exact session's room-local alias and roles. |
| `muxa_send_message` | Create a durable peer request. |
| `muxa_inbox` | Claim/read requests for this exact agent session. |
| `muxa_list_messages` | List incoming, sent, or all requests without claiming. |
| `muxa_reply` | Return a completed/blocked/declined/failed response. |
| `muxa_wait_reply` | Wait for the structured terminal response. |
| `muxa_cancel_message` | Cancel a sent request while it is still queued. |

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
delivery. With `idle_only`, muxad injects a short notification for new requests
and terminal replies, and only when the exact hook-authoritative participant
is `Idle`. Message bodies never enter the terminal. It never injects into
`Working`, `WaitingInput`, `WaitingChoice`, or `Error` panes. Synthetic
screen-detected agents have no stable session identity, so they are not room
participants or auto-wake targets.

Reading a terminal result through `muxa_wait_reply` acknowledges it and
prevents a later reply wake. Room context reports incoming unread requests and
unacknowledged replies separately. `muxa_list_messages` is non-claiming history;
the sender may cancel only while a request remains `queued`, before the
recipient has claimed it.

Every collaboration IPC operation (context, identity, send, inbox, list,
reply, get, and cancel) is also appended to
`$XDG_DATA_HOME/muxa/collaboration-audit.ndjson`. The `0600` ledger records the
represented origin/session, OS-observed caller, target/request id, and outcome,
but never duplicates message or reply bodies. `muxa msg list --json` and
`muxa_list_messages` expose the creation provenance stored on each request;
requests created before this field existed simply omit it.

Provenance remains audit evidence, not a permission gate. An observed/asserted
pane mismatch is recorded as `mismatched` but does not block the operation, so
host-scoped explicit-pane routing and the existing high-authority owner-only
socket workflow remain unchanged.
