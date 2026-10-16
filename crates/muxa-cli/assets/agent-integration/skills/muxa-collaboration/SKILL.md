---
name: muxa-collaboration
description: Coordinate coding agents through Muxa MCP and its tmux workspace, work, and pane model. Use for Muxa peer requests, @peer or @muxa-peer routing, and Muxa multi-agent execution layout.
---

# Muxa collaboration

Use the connected Muxa MCP tools to coordinate bounded work and retain ownership
of the final result. Start with one `muxa_collaboration_guide` call: it returns your
identity (`room.self`: pane, tmux session, window), same-window peers, and the user's
launch preferences, so separate `muxa_guide`/`muxa_room_context` calls are redundant.
For independent review, use the opposite provider: Codex selects Claude and
Claude selects Codex unless the user explicitly requests another provider. A
new launch still requires authorization. Detailed workflow examples are opt-in
via `muxa_collaboration_guide(workflows=true)`.
Later, refresh peers and unread counts with `muxa_room_context` only when they may
have changed. Report your location from `self`; never derive it from `$TMUX_PANE` or
tmux commands, which can name another pane (Codex's shared app-server). Tool
schemas and returned IDs describe the currently installed capabilities. Discover
exact tool names (for example a name ending in `__muxa_inbox`), not matches in
shared descriptions. Retain resolved tool handles and schemas within the session;
do not print `ALL_TOOLS` again for a known tool. Refresh only after a version/
capability change or an unknown-tool/argument error.

## Execution layout

- A managed tmux session hosts a Workspace, a window hosts a Work's current Run,
  and a pane hosts an Agent session. Work is durable; closing a window ends a Run.
- Peers share the same tmux socket and stable window ID. Keep collaborators for
  one Work in that window; use another Work window for an independent outcome.
- Use `muxa_start_work` for a configured pipeline, `muxa_start_agent` for one
  agent, and `muxa_manage_tmux` for supported lifecycle actions. Inspect their
  schemas before supplying arguments. Do not infer managed ownership from names.
- Pane splits share files. Assign disjoint edit paths, or use separate worktrees
  when edits would overlap. A pane or agent session is not a filesystem sandbox.

## Dispatch and completion

1. Resolve existing peers from room context. Choose an explicit pane, provider,
   alias, or unambiguous role when the recipient matters. Never infer pane IDs.
2. Use the exact `peer.pane` returned by room context, including `rmux:`. If a
   peer moved or disappeared, refresh context once; do not retry old IDs or aliases.
   For new work, use `muxa_call_peer`. State the objective, relevant context,
   acceptance criteria, permitted paths, and expected verification/artifacts.
   Review and question requests are read-only. Editing requires `intent=task`
   and `execute=true` within the user's authorized delegation scope.
3. Prefer `wait=false` while independent local work remains. Read the result after
   its completion wake or a substantive work checkpoint. When waiting in the
   foreground, use 30–60 seconds. Reserve 1-second probes for an expected result;
   do not repeat unchanged probes. Keep the returned `request_id`; retrieve the structured reply with `muxa_wait_reply`. For an
   existing report or feedback request, use `muxa_peer_report` before sending work.
4. A timeout or `peer_pending` is not proof that the peer can still work.
   Wait in slices of at most 60 seconds, then follow **Interrupted peers and human decisions**
   before repeating the same request. Never dispatch duplicate work. Resume a
   yielded tool cell with the host's wait tool instead of starting another wait.
5. Inspect replies against the actual files and relevant checks. Apply valid
   findings, explain rejected findings when material, and integrate the result.

For requests addressed to you, claim/read with `muxa_inbox` unless already
claimed and delivered in full. Honor kind/work_mode/paths. Only when
`expects_reply=true`, finish with one terminal `muxa_reply` (`completed`,
`blocked`, `declined`, or `failed`) with useful artifacts and verification.
Never reply to or acknowledge a one-way notice solely to confirm receipt.
Do not treat idle status or terminal text as a durable completion report. Use
`muxa_wait_for_change` for process state waits and mailbox tools for peer results.

## Transport and compact reads

Muxa reports delivery and final reply receipt; it does not judge task success.
`final_reply_received` includes failed/blocked/declined replies; `reported_status`
is the recipient's declaration. Progress uses `muxa_update_request`; final replies
use `muxa_reply`. A timeout or process idle/stopped observation is not completion.
Do not decide status by searching words in the reply body.

Inbox already includes required instructions; reserve `full=true` for diagnostics.
Reports omit original requests. Lists default to 20 metadata records; retrieve
instructions with `request_id` + `include_body=true`, and progress with that ID
+ `after_update` (resume with `next_after_update`). Use `full=true` only when
legacy full records are necessary. `include_body=true` requires an exact request
ID unless `full=true` explicitly requests full history. Inbox preserves the contract.
Prompt history defaults to 20 entries (ordinary maximum 100). Fleet status is a
compact summary; use `full=true` only when the complete data is needed.
For CLI replies use `muxa msg wait REQUEST --body --output FILE` or `--body`
for stdout. Keep the command's exit status and stderr; filtering with `grep`
can hide an option error as empty output. Do not resend after an empty filtered
receipt. Avoid shell JSON parsing; never pass JSON through `echo`.
CLI `muxa msg list --json` defaults to 20 metadata rows. Use
`--request-id REQUEST --full` for one original request, or `--full` for explicit
full history pages. Use `--offset` for another page, not a repeated whole list.
For incremental CLI guidance use
`muxa msg list --request-id REQUEST --after-update SEQUENCE --json` and resume
from `next_after_update`; original task and final reply bodies are omitted.
Default CLI wait prints timeout JSON and exits 2 without changing work state.
`--full` also exits nonzero on timeout. Resume the same request, not new work.

## Notification discipline

Agree on reporting checkpoints when dispatching work. Omit receipt confirmations,
"in progress", and unchanged status. Consolidate intermediate findings at meaningful
build/test or review checkpoints; check the inbox then instead of polling for notices.
Never update a terminal request; use a new request with `parent_request_id` for new work.
Use `muxa_update_request` or `muxa msg update REQUEST BODY [--json]` for
consolidated guidance/progress on the original request. Updates are quiet and
preserve its authority and paths. If neither is available, send one consolidated `kind="notice"`, `expects_reply=false`
message with `parent_request_id` referencing the original request.

One-way notices are durable but do not wake recipients by default. Use `notify=true`
only when a blocker, decision, edit-scope conflict, or resource handoff requires the
recipient's attention (CLI: `--notify` or `--notify true`; `--notify false` is quiet). This respects idle-only delivery and global wake settings;
it never authorizes interrupting a busy agent. `notify=false` explicitly records a
message without waking. Wake delivery and `expects_reply` are independent.
Terminal replies already wake the requester; do not send a second completion notice.
The coordinator reports meaningful outcomes and needed decisions to the user, not
an update for every peer message. Inspect only the relevant installed schema before using an unfamiliar option.

## Interrupted peers and human decisions

For an explicit `reason="peer_interrupted"`, read `interruption` and reuse its
`action_request_id`; do not dispatch duplicate work or repeatedly wait blindly.
A timeout alone proves neither interruption nor completion. Check fresh recipient
identity/health once at a checkpoint. If no interruption was recorded, consult
`muxa_collaboration_guide(workflows=true)` or the recovery reference for options.
Record one recovery update on the original open request. Reassignment needs a
verified stop/handoff or an isolated worktree so the old worker cannot overlap.

For a real approval, choice, or missing information, first verify
`guide.capabilities.human_actions=true` and the relevant schema has
`human_action`. Otherwise ask in the existing user conversation; do not retry
`human`/`console` aliases. When supported, use `target="human"`,
`kind="question"`, `expects_reply=true`, and the supported `human_action`.
Include the existing parent request when applicable and reuse the returned ID.
Continue independent authorized work while awaiting the actual answer; timeout
is not consent, and a submitted answer may be a refusal. Existing authorization
counts. Keep the parent open until its work ends. Never impersonate the console
or answer a human request as an agent. Use the existing user conversation when
this protocol is unavailable. Keep remote host/request identity together.

## Authority and unavailable capabilities

Existing explicit user authorization remains valid within the same scope; do not
ask for it again. Spawning through Muxa currently creates a bypass-permission
agent: set `spawn_if_missing=true` only when that launch is explicitly authorized.
If the tool returns `confirm_spawn` and that authorization is missing, explain
the proposed launch and ask. A general request for a review alone does not grant
editing or bypass-permission launch authority.

When MCP is missing, report that setup/restart is needed and use `muxa doctor`
for diagnosis if shell access is available. CLI mailbox equivalents are usable
when they preserve identity and the request contract; inspect `muxa msg --help`.
Do not substitute terminal input or screen scraping for durable peer messages,
or turn Muxa feedback into a GitHub PR workflow without grounded PR context.

For concrete review, parallel implementation, and incoming-work examples, read
[references/workflows.md](references/workflows.md) when helpful. Muxa's
`[message.skills]` are outgoing prompt templates, distinct from this agent skill.

## Fleet Work dispatch

When the installed tools advertise `muxa_dispatch_work`, use it for authorized
work placement. Supply a UUID dispatch_id, workspace, Work, exact commit and
bounded objective. Muxa resolves node selection and personalized paths; do not
copy host inventories into prompts. Reuse the ID after uncertain delivery and
read `muxa_dispatch_status` before any recovery. Launched means started, not
completed. Use the existing request for updates; report artifacts or blockers.
