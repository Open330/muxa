# Workflow examples

All ids below are illustrative; always use the ids Tend returns.

## Independent review

Codex reviews with Claude and Claude with Codex unless the user chose a
provider. For "@peer review the current changes", call
`tend_call_peer(target="auto", intent="review", body="Review the diff at
<worktree> <commit> for correctness. Acceptance criteria: ...; checks run: ...;
focus: ...")`. It returns `req_r` at once. Continue independent checks; when
the reply wakes you, read it with `tend_inbox(view="report", request_id="req_r")` and verify
the findings against the code. No PR or new agent is required.

## Parallel implementation

Give each worker one bounded deliverable and its file ownership, e.g. API
regression tests under `tests/api/` while you implement the handler:
`intent="task"`, `execute=true`, `paths=["tests/api/"]`, and the scope stated in
the body too (paths are advisory). When workers must edit the same files, give
each its own worktree and cwd; integrate commits explicitly. A successful
reply does not mean your checkout changed.

## Human decision during peer work

While handling `req_review`, a reviewer finds two mutually exclusive
requirements the task does not resolve:

```json
{"body": "Choose A (keep compatibility) or B (remove the legacy API). It changes the public contract. Recommend A for this release.",
 "action": "choice", "options": ["A", "B"], "recommendation": "A",
 "parent_request_id": "req_review"}
```

Send it with `tend_request_attention`, keep `req_decision`, continue
independent checks, and read the answer when it wakes you
(`tend_wait(id="req_decision")` if you must block). Implement only what the
answer authorizes, then finish `req_review`. A failed review, a missing peer
or an already-granted permission does not justify this flow. CLI:
`tend msg attention "..." --action choice`.

## Incoming work

On a Tend wake, `tend_inbox(request_id=ID)` unless the full request was already
delivered. A review returns findings without editing; a task edits only its
scope and reports the checks it ran. With `expects_reply=true`, finish once
with `tend_reply`. Absorb one-way notices silently. If a missing prerequisite
ends the attempt, reply `blocked` and name it; while a human decision is
pending, keep the request open instead. For "the peer's report", read
`tend_inbox(view=report)`; do not ask the peer to repeat the work.

## Interrupted peer

`tend_wait(id="req_r", timeout_secs=60)` timed out, or returned
`peer_interrupted`. Check the durable record and fresh state of req_r's exact
recipient once. If it is capped, record the scope, reset and last artifacts
with `tend_update_request(request_id="req_r", body=...)`. Use an authorized
recovery when one exists; otherwise one `tend_request_attention` with
`parent_request_id="req_r"` ("wait for the reset / move to a healthy
provider"). Keep req_r open; a claimed request needs a verified stop or an
isolated worktree before anyone else takes it. A Fleet request keeps its
returned host and pane_key.
