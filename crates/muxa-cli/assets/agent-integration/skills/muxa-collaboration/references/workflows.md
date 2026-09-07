# Workflow examples

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

If prerequisites are missing, return `blocked` with the missing prerequisite.
If asked to summarize "the peer's report", retrieve `muxa_peer_report`; do not
ask the peer to repeat work. Long waits keep the same durable request ID even if
the target pane disappears. Remote Fleet requests require an explicit host and
pane and obey the host's observe/control mode.
