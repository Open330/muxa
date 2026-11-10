Use Tend for requested peer collaboration, @peer/@tend-peer routing, and
workspace/work/agent execution layout. Read the tend-collaboration skill when
available; otherwise retrieve tend_collaboration_guide through the connected MCP.
Call tend_collaboration_guide once for your identity (room.self), same-window
peers, and the user's launch preferences; refresh later with tend_room_context.
If MCP is unavailable, use `tend peer codex "BOUNDED REQUEST"` (or claude/auto)
for new peer work; it reuses or creates a dedicated worker. `tend msg send %N`
is exact mailbox delivery and stays queued on a protected primary, unless the
user told you to deliver it there: then add `--user-authorized` (MCP
`user_authorized=true`) and it is handed over at that session's next turn
boundary without anyone accepting it. Keep the
request id; use `tend msg wait REQUEST` for its reply, not a duplicate request.
For Tend coordination, use Tend's durable mailbox rather than Claude's native
SendMessage/ListAgents channel. Native cross-session acceptance is not a Tend
delivery receipt. If provider approval interrupts an accepted request, keep its
request ID and use the Tend interruption/recovery record; do not resend the body,
change the recipient's permissions, or treat a held/refused message as completed.
Never derive your session/window/pane from $TMUX_PANE or tmux commands.
These instructions apply to Tend work;
the presence of tmux alone does not require delegation.
Honor the user's scope and existing authorization. A peer request carries its own
read_only/execute contract; it does not grant authority beyond the user's task.

Treat one-way notices as information, not new reply obligations. Never acknowledge
receipt or send unchanged status. Batch progress at build/test checkpoints and
reserve explicit wake notifications for blockers, decisions, conflicts, and handoffs.

For a required human approval, choice or missing information, use
`tend_request_attention` and retain its request ID; connected Fleet operator
clients receive it. Read the actual reply before acting; do not request already
authorized routine actions or send duplicate questions.

For interrupted peers or real human decisions, read the recovery workflow in
the collaboration guide (workflows=true). Reuse request IDs; an interruption
keeps work open, and an answer is not automatic approval.

Discover exact tool names; shared descriptions can match unrelated tools.
Retain needed tool handles/schemas; do not rediscover a known tool until its
version/capabilities change or an unknown-tool/argument error occurs. Check guide capabilities before using optional protocols.

For requested historical agent-record investigations, use tend_agent_search with
the user's natural-language question, then tend_agent_read on relevant event IDs.
Inspect the interpreted terms/time range and coverage, follow command/result pairs,
and broaden terms or the time/session scope before drawing conclusions. Exclude
the investigating session when it repeats the evidence. A prose claim or quoted
log is not proof of execution, and an incomplete search cannot prove absence.

When supported, use task_contract for versioned reviews, then echo the received
attempt.id and basis_revision in a structured report with criterion evidence.
Reported checks are not independently verified. Supersede an outdated read-only
request explicitly; never use supersession to take over a running editor.
