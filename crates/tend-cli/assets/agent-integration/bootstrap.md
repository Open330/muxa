Use Tend for requested peer collaboration, @peer/@tend-peer routing, and
workspace/work/agent execution layout. Read the tend-collaboration skill when
available; otherwise call tend_guide once through the connected MCP for your
identity (room.self), same-window peers and the user's launch preferences.
Never derive your session/window/pane from $TMUX_PANE or tmux commands.
These instructions apply to Tend work; tmux alone does not require delegation.

Send work with tend_call_peer. It returns a request id at once and the reply
arrives as a wake: read it with tend_inbox(view=report, request_id) or
tend_wait(id). Never
send the same work twice. Without MCP, `tend peer codex "BOUNDED REQUEST"` (or
claude/auto) uses the same worker routing; `tend msg wait REQUEST` reads the
reply. `tend msg send %N` is exact delivery and stays queued on a protected
primary unless the user told you to deliver it there: then add
`--user-authorized` (MCP `user_authorized=true`) and it is offered at that
session's next turn boundary. Use Tend's durable mailbox, not Claude's native
SendMessage/ListAgents channel. If provider approval interrupts a request,
keep its id and use the Tend interruption record; do not resend the body or
change the recipient's permissions.

Honor the user's scope and existing authorization. A peer request carries its
own read_only/execute contract and grants nothing beyond the user's task.
Treat one-way notices as information: never acknowledge receipt or send
unchanged status. Batch progress at build/test checkpoints; wake others only
for blockers, decisions, conflicts and handoffs.

For a required human approval, choice or missing information, call
`tend_request_attention` and keep its id; read the actual answer before acting.
When you finish a result someone must look at, file one record:
`tend work done --summary "..."` in a managed pipeline, otherwise
`tend_file_result` (commits, `test-pass:CMD` evidence, and decision_needed /
options / recommendation when they must choose). For interrupted peers, see
tend_guide(workflows=true); an interruption keeps work open and an answer is
not automatic approval.

Discover exact tool names; shared descriptions can match unrelated tools. Keep
resolved handles until a version change or unknown-tool error. For historical
agent-record questions, use tend_agent_search, then expand hits with event_id;
a prose claim is not proof of execution and an incomplete search cannot prove
absence. When supported, use task_contract for versioned reviews and echo
attempt.id and basis_revision in the structured report.
