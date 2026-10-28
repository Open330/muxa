Use Muxa for requested peer collaboration, @peer/@muxa-peer routing, and
workspace/work/agent execution layout. Read the muxa-collaboration skill when
available; otherwise retrieve muxa_collaboration_guide through the connected MCP.
Call muxa_collaboration_guide once for your identity (room.self), same-window
peers, and the user's launch preferences; refresh later with muxa_room_context.
Never derive your session/window/pane from $TMUX_PANE or tmux commands.
These instructions apply to Muxa work;
the presence of tmux alone does not require delegation.
Honor the user's scope and existing authorization. A peer request carries its own
read_only/execute contract; it does not grant authority beyond the user's task.

Treat one-way notices as information, not new reply obligations. Never acknowledge
receipt or send unchanged status. Batch progress at build/test checkpoints and
reserve explicit wake notifications for blockers, decisions, conflicts, and handoffs.

For a required human approval, choice or missing information, use
`muxa_request_attention` and retain its request ID; connected Fleet operator
clients receive it. Read the actual reply before acting; do not request already
authorized routine actions or send duplicate questions.

For interrupted peers or real human decisions, read the recovery workflow in
the collaboration guide (workflows=true). Reuse request IDs; an interruption
keeps work open, and an answer is not automatic approval.

Discover exact tool names; shared descriptions can match unrelated tools.
Retain needed tool handles/schemas; do not rediscover a known tool until its
version/capabilities change or an unknown-tool/argument error occurs. Check guide capabilities before using optional protocols.

For requested historical agent-record investigations, use muxa_agent_search with
the user's natural-language question, then muxa_agent_read on relevant event IDs.
Inspect the interpreted terms/time range and coverage, follow command/result pairs,
and broaden terms or the time/session scope before drawing conclusions. Exclude
the investigating session when it repeats the evidence. A prose claim or quoted
log is not proof of execution, and an incomplete search cannot prove absence.
