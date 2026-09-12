---
name: muxa-collaboration
description: Coordinate coding agents through Muxa MCP and its tmux workspace, work, and pane model. Use for Muxa peer requests, @peer or @muxa-peer routing, and Muxa multi-agent execution layout.
---

# Muxa collaboration

Use the connected Muxa MCP tools to coordinate bounded work and retain ownership
of the final result. Consult `muxa_guide` for the user's launch preferences and
`muxa_room_context` for identity, same-window peers, and unread messages. Retrieve
`muxa_collaboration_guide` when you need the runtime's detailed contract. Tool
schemas and returned IDs describe the currently installed capabilities.

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
2. For new work, use `muxa_call_peer`. State the objective, relevant context,
   acceptance criteria, permitted paths, and expected verification/artifacts.
   Review and question requests are read-only. Editing requires `intent=task`
   and `execute=true` within the user's authorized delegation scope.
3. Prefer `wait=false` while independent local work remains. Keep the returned
   `request_id`; retrieve the structured reply with `muxa_wait_reply`. For an
   existing report or feedback request, use `muxa_peer_report` before sending work.
4. A timeout or `peer_pending` is not proof that the peer can still work.
   Wait in slices of at most 60 seconds, then follow **Peer interruption recovery**
   before repeating the same request. Never dispatch duplicate work. Resume a
   yielded tool cell with the host's wait tool instead of starting another wait.
5. Inspect replies against the actual files and relevant checks. Apply valid
   findings, explain rejected findings when material, and integrate the result.

For notifications addressed to you, claim work with `muxa_inbox`, honor its
kind/work_mode/paths, and finish with one terminal `muxa_reply` (`completed`,
`blocked`, `declined`, or `failed`). Include useful artifacts and verification.
Do not treat idle status or terminal text as a durable completion report. Use
`muxa_wait_for_change` for process state waits and mailbox tools for peer results.

## Peer interruption recovery

The coordinator owns recovery: a usage-limited agent may be unable to send any
final reply. Before dispatch, retain the exact recipient identity and agree on
an overall wait budget/checkpoint. Prefer `wait=false` and independent work;
when awaiting results use `timeout_secs=60` or less, locally and in Fleet.
A bounded call does not bound an unlimited sequence of repeated calls.

After a timeout, or on an error/stopped notification, read the durable request
first: a terminal reply wins. For an open request check fresh `muxa_status` for
the recipient pane, matching its original agent kind/session and socket; use
`muxa_fleet_status` for the exact returned host/pane_key on Fleet. The request's
`to.state` is a dispatch-time snapshot, not a live health check. A reused pane
is not the original recipient. Missing/offline observations mean unknown
availability, not proof of a provider cap or that the remote process stopped.

Use recorded `rate_limit_scope`, `rate_limit_source`, `rate_limited_until`, state
and attention/error evidence. A utilization percentage alone is not proof that
execution stopped. Do not classify every `error` as quota exhaustion or invent
a reset time. `waiting_input`/`waiting_choice` may require an action; idle alone
is neither completion nor proof of interruption. Known caps, errors, stopped
sessions, repeated unavailability, or reaching the overall wait budget stop
blind reply waits and require a recovery decision.

Record one consolidated update on the original request with observed evidence,
last progress/artifacts, reset time if known, and the next action. Do not send
repeated prompts to the capped agent. If already authorized, continue independent
work, arrange one bounded recheck near a known reset, or use a healthy peer.
A reset timestamp is a recheck hint, not evidence of recovery: verify fresh state
and actual progress before resuming. Do not leave a long blocking tool cell open
until reset or claim to have scheduled a recheck without a real scheduler.

Prevent duplicate execution before reassignment. Cancel a still-queued local
request with `muxa_cancel_message` and verify success. Claimed requests cannot be
cancelled by that tool; error/cap/offline status does not prevent later resumption.
Use supported, authorized stop/handoff controls and verify the old worker cannot
resume overlapping writes, or isolate the replacement in a separate worktree.
Keep the old/new request IDs and artifacts linked in the thread. Never fabricate
a reply as the unavailable peer or treat its failure as a successful review.

If recovery needs a real user decision (wait, change provider, restart, or adjust
scope/cost), send one `target="human"` request using the Human feedback protocol,
with the original `parent_request_id` when supported. Include evidence, impact,
options and recommendation. Reuse its ID while unresolved; do not create an alert
for every timeout. Existing authorization counts; do not require another approval
for an already-authorized recovery. On Fleet preserve the remote endpoint and use
its supported transport; do not pass a remote request ID to local tools.

If the coordinator is itself a recipient and can no longer complete its own work,
reply `blocked` on that incoming request with the unavailable dependency and
handoff evidence. Only do this when ending that attempt; keep it open during an
active human decision. Never submit a terminal reply for someone else's work.
On upgraded daemons, `reason="peer_interrupted"` releases local/Fleet waits while
keeping the work open. Read `interruption.action_request_id` and reuse that human
request instead of creating another; `interruption.decision` carries the recorded
answer, including refusals. The daemon escalates once if the coordinator is also
unavailable or has not acknowledged with an update within two minutes. Recovery
clears the interruption and withdraws unanswered daemon actions. Unknown/missing
identities are not classified as caps; older hosts still need bounded checks.

## Human feedback protocol

Separate agent communication from operator decisions. A pending peer request,
review, retry, progress update, or a human-initiated task is not by itself a
reason to notify the human. Use `muxa_update_request` for progress on the same
request and the existing peer tools for agent work.

When progress actually requires a human's approval, choice, or missing
information, use `muxa_send_message` with `target="human"`, `kind="question"`,
`expects_reply=true`, and `human_action="approval"`, `"choice"`, or
`"information"`. Explain the exact decision, why it is required, available
options, and your recommendation. Reuse existing authorization; never create
an approval request for work the user already authorized.

If this follows a collaboration request you participate in, include its exact
`parent_request_id`; muxa inherits the thread. Otherwise omit the parent.
Preserve work/workspace/run identity when known. Do not invent IDs. Keep the
returned human `request_id` and wait with `muxa_wait_reply`; a timeout is not
an answer, consent, or permission to send a duplicate question. Continue
independent authorized work while waiting, but not work dependent on the answer.

The operator answers in mailbox **Need you** (`e` in watch). Read the structured
reply and its body: `completed` means an answer was submitted, not necessarily
approval. Honor a refusal or cancellation. Resume only the work the answer
permits and eventually reply on the original peer request. Do not mark that
parent `blocked` merely to represent a temporary human wait: blocked is terminal.
Agents must not impersonate the operator console, claim its inbox, or answer
their own human request. Never use terminal input to simulate a human reply.

Check installed tool schemas for `human_action`/`target="human"` support. If
unavailable, ask through the user's existing conversation and report the missing
capability; do not redirect the question to an agent or silently drop it.
The CLI equivalent is `muxa msg send human "decision and context" --human-action
choice --parent REQUEST_ID --json` (omit `--parent` when there is none).
For a remote host, use the installed host's supported message transport and keep
its endpoint with the request ID; do not assume a local ID resolves remotely.

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
