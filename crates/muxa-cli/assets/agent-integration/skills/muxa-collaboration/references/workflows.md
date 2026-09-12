# Workflow examples

## Human decision during peer work

While handling `req_review`, a reviewer discovers two mutually exclusive product
requirements that the authorized task does not resolve. Send:

```json
{
  "target": "human",
  "kind": "question",
  "expects_reply": true,
  "human_action": "choice",
  "parent_request_id": "req_review",
  "body": "Choose A (preserve compatibility) or B (remove the legacy API). This changes the public contract, so I need your choice. Recommend A for this release."
}
```

Use `muxa_send_message`, retain its returned ID (for example `req_decision`),
then call `muxa_wait_reply(request_id="req_decision")`. Keep the original review
open; continue independent checks. On timeout, wait on `req_decision` again.
When the operator answers, inspect both status and body, implement only what
the answer authorizes, then complete `req_review` with the verified result.
These IDs are illustrative: always use actual returned IDs.

Do not use `muxa_call_peer(target="human")`, `pane:console`, or `console=true`
to create or answer an operator decision. A failed review, missing peer, or
routine permission already granted by the user does not justify this flow.
`approval` and `information` use the same protocol with their corresponding
`human_action`. The dashboard/app shows the linked request and return reply;
orange/Need you identifies an unresolved human request, not agent activity.

## Independent review

For "@peer review the current changes", retrieve room context, then call
`muxa_call_peer(target="auto", intent="review", body="Review the current diff for
correctness. Acceptance criteria: ...; checks already run: ...; focus: ...",
wait=false)`. Include the exact repository/worktree and commit or diff context.
Continue independent checks, read the reply using the returned request ID, and
verify the findings. This does not require creating a PR or a new agent.

## Parallel implementation

For an authorized implementation with peer delegation, assign one bounded
deliverable per worker and record file ownership in each request. For example,
delegate API regression coverage under `tests/api/` while implementing the
handler locally. Use `intent="task"`, `execute=true`, and the narrow `paths`
scope. Paths are advisory and must also be explained in the body.

When multiple workers need to edit the same files, prepare separate worktrees
and launch each agent with the intended working directory. A shared Work window
still supplies the collaboration room. Integrate commits or patches explicitly;
do not assume that successful replies imply the shared checkout is updated.

An independent outcome belongs to another Work window in the same Workspace;
an unrelated project belongs to another Workspace. Prefer managed launch tools
so identity and lifecycle metadata stay consistent.

## Incoming work and follow-up

On a Muxa notification, call `muxa_inbox` and read the complete request. A
read-only review returns findings without changing files. An execute task edits
only within its delegated scope and reports the checks run. Use `muxa_reply`
once with the original request ID, terminal status, result, and artifacts.

If a missing prerequisite ends this work attempt, return `blocked` and name it.
While awaiting a human decision within an active attempt, keep the request open
and use the Human decision protocol above instead of a terminal `blocked` reply.
If asked to summarize "the peer's report", retrieve `muxa_peer_report`; do not
ask the peer to repeat work. Long waits keep the same durable request ID even if
the target pane disappears. Remote Fleet requests require an explicit host and
pane and obey the host's observe/control mode.
