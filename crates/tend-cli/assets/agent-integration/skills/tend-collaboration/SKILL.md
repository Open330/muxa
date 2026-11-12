---
name: tend-collaboration
description: Coordinate coding agents through Tend MCP and its tmux workspace, work, and pane model. Use for Tend peer requests, @peer or @tend-peer routing, and Tend multi-agent execution layout.
---

# Tend collaboration

Delegate bounded work through Tend and keep ownership of the final result.

## Start

Call `tend_guide` once. It returns your identity (`room.self`: pane, tmux
session, window), same-window peers, the user's launch preferences and the
guardrails. Refresh later with `tend_guide(topic=room)` only when peers may
have changed. Take your location from `self`, never from `$TMUX_PANE` or tmux
commands, which can name another pane. Discover exact tool names (one ending in
`__tend_inbox`, not matches in descriptions), keep the handles you resolved,
and refresh only after a version change or an unknown-tool error.

For an independent review use the opposite provider (Codex picks Claude,
Claude picks Codex) unless the user named one.

## Layout

A managed tmux session is a Workspace, a window is a Work's current Run, a
pane is one agent. Keep one Work's collaborators in its window. Use
`tend_start_work` for a configured pipeline, `tend_start_agent` for one agent,
`tend_manage_tmux` for lifecycle. Panes share files: give peers disjoint paths
or separate worktrees.

## Send work

1. `tend_call_peer` with the objective, context, acceptance criteria, paths
   and the evidence you expect back. Review and question are read-only;
   editing needs `intent=task` and `execute=true` within the user's
   authorization. `target` picks a peer, provider, `%N`, `@alias` or `role:`;
   Tend reuses or creates a worker with configured permissions.
2. It returns at once with a `request_id`. Keep working; the reply arrives as
   a wake. Read it with `tend_inbox(view=report, request_id=ID)` or `tend_wait(id=ID)`
   (30-60 s foreground waits, never repeated 1-second probes). Never send the
   same work twice; a timeout is not failure.
3. Verify the reply against the files and checks. A reply is not proof.

Exact mailbox delivery (`exact=true`, or `intent=notice` for one-way
information) queues on a protected primary instead of rerouting to a worker.
Human sessions are protected primaries. When the user told you to deliver a
request to their session, add `user_authorized=true`: it is offered at that
session's next turn boundary, never typed into its terminal. On the receiving
side such an offer is not an instruction until you claim it with
`tend_inbox(request_id)`.

## Receive work

Claim with `tend_inbox` (the wake names the `request_id`). Honor
kind/work_mode/paths. Only when `expects_reply=true`, finish with exactly one
`tend_reply` (`completed`, `blocked`, `declined` or `failed`) with artifacts
and verification. Never acknowledge a one-way notice. Progress goes on the
same request with `tend_update_request`, at real checkpoints only.

If a reminder says you claimed a request and ended your turn without
replying, reply now or post progress; do not redo the work.

## Finishing

File one result record the person can decide from without reading your pane:
`tend work done --summary ...` in a managed pipeline, otherwise
`tend_file_result` (summary, commits, `test-pass:CMD`/`test-fail:CMD`, and
`decision_needed`/`options`/`recommendation` when they must choose).

## Reading the mailbox

`tend_inbox(view=incoming|sent|all)` lists without claiming (20 metadata rows
by default; `request_id` + `include_body=true` for one instruction,
`after_update` for progress). `tend_inbox(view=report)` returns the newest
peer reply. Delivery and `final_reply_received` are receipts, not success;
`reported_status` is the recipient's word. Never judge status from words in a
body.

## Notices and wakes

One-way notices do not wake anyone unless `notify=true`; use it only for a
blocker, decision, edit-scope conflict or handoff. Replies already wake the
sender. When you give up on your own request, `tend_update_request(action=
withdraw_wake, request_id=ID)`; cancel a still-queued one with
`action=cancel`. Never update a finished request; send a new one with
`parent_request_id`. Report outcomes and decisions to the user, not every
message.

## Interrupted peers and human decisions

A `peer_interrupted` result keeps the work open: read `interruption`, reuse
its `action_request_id`, do not resend. Reasons include provider approval,
caps, a lost binding, `reply_overdue` (claimed, reminded, still silent) and
`wake_unsubmitted` (the wake never reached a turn). Check fresh recipient
state once at a checkpoint; reassign only after a verified stop or in an
isolated worktree. `tend_guide(workflows=true)` has recovery examples.

For a real approval, choice or missing information, call
`tend_request_attention` (body, options, recommendation, evidence) and keep
its id. Its answer arrives as a wake. Timeout is never consent; an answer
may be a refusal. Never answer a human request as an agent.

## Authority

Existing user authorization stays valid within its scope; do not ask again.
A review grants no edit authority. Never add bypass flags to get past a
permission prompt.

## Fleet

Add `host` to reach a control-mode Fleet host: `tend_status(host)`,
`tend_call_peer(host, pane)`, `tend_send_prompt(host, pane)`. Dispatch Work
with `tend_start_work(commit, dispatch_id, ...)`, reuse the dispatch id after
an uncertain delivery, and read it with `tend_wait(id)`; launched means
started, not done.

## Without MCP

Report that setup or a restart is needed (`tend doctor`). `tend peer codex
"BOUNDED REQUEST"` (or `claude`, a pane, `auto`) uses the same worker routing
and returns a request id; `tend msg wait REQUEST --body` reads the reply.
`tend msg send %N` is exact delivery and can stay queued on a primary. Keep
CLI exit status and stderr; never parse JSON through `echo`.

Examples: [references/workflows.md](references/workflows.md). Tend's
`[message.skills]` are outgoing prompt templates, distinct from this skill.
