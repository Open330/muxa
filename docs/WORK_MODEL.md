# Tend Work domain

The dashboard and execution topology use related but different identities.
Tend owns the logical work model; tmux supplies an execution surface.

```text
Workspace
└── Work (tend-owned outcome)
    ├── External issue reference (optional: Linear, GitHub, Jira, ...)
    └── Run (execution attempt)
        └── Agent session
            └── tmux/cmux/rmux/herdr/zellij/native PTY binding
```

| Entity | Identity | State |
| --- | --- | --- |
| Workspace | `workspace_id` | aggregate counts only |
| Work | `{workspace_id, work_id}` | queued / in progress / review / done |
| External issue | provider + stable ID (display key kept separately) | provider-owned status |
| Run | host + socket + session ID + window ID | starting / running / waiting / idle / failed / completed |
| Agent session | agent runtime session ID | starting / working / waiting / idle / error / stopped |
| Signal | belongs to Work snapshot, not identity | attention / blocked / error |

## Invariants

- A tmux window does not become Work merely because it exists or has an
  issue-shaped name.
- Only managed execution metadata or a durable Work record creates a Work card.
- Closing/recreating a window ends or replaces a Run; it does not rename or
  delete Work.
- External issue status never moves the local board stage automatically.
- Agent waiting/error states produce signals without replacing the local stage.
- Unmanaged windows remain `Unlinked executions` and are inspected through
  execution-focused surfaces such as `tend watch`.

## Backlog

A Work can be planned before anything runs, and its record stays after its
window closes:

```sh
tend work plan web/ENG-1 --title "Fix login" --goal "users land on /home" \
  --prompt "Fix the login redirect" --pipeline pair --priority 1
tend work list --stage queued      # the backlog, priority then age
tend work up ENG-1                 # starts it from the plan; stage → in_progress
tend work show ENG-1               # asked, done (durable Run), decided (result records)
```

`plan` creates a queued record (cwd defaults to the current directory) or
edits only the fields given; an empty value clears a field and
`--no-priority` clears the order. `work up` fills `--workspace`,
`--pipeline`, `--body` and `--external` from the plan when they are not
given, and uses the plan's directory where it would otherwise use the
current one (a route's `cwd` or worktree still wins). `work show` works
without a window: it prints the record, the durable Run's aliases, and every
review item filed on the Work (`tend work done --summary`,
`tend_file_result`) with its disposition.

The records live in tendd (`dashboard-work.json`, shared with the
dashboard's editor, which keeps the plan when it saves metadata) and are
served over IPC as `work_backlog`, `work_edit` and `work_record_show`
(`work_backlog_v1`).

### Acceptance and checks

A Work can say what done means and how tend checks it:

```sh
tend work plan web/ENG-1 --title "Fix login" \
  --accept "login lands on /home" --accept "no new warnings" \
  --check "cargo test -p web" --check "cargo clippy -- -D warnings"
tend work up ENG-2 --check "npm test"        # recorded on the Work it starts
```

`--accept` adds a criterion and `--check` a shell command; repeating a flag
gives the whole list, which replaces the Work's (`--no-acceptance`,
`--no-checks` clear them). `tend_start_work` takes the same lists as
`acceptance` and `checks`, and both refuse a bad list before anything
starts. Checks are pinned to the plan's `cwd` as it was when they were
declared, so a later `cwd` edit does not move them. Before `work up` or
`tend_start_work` launches anything, it re-pins them to the directory it is
about to start in, so a route worktree or `--cwd` is what gets verified; that
re-pin follows the declarer rule below, and when it is refused (an agent
starting the person's Work somewhere else) nothing is launched. A pipeline
registration never moves them. The commands come only from this declaration,
never from repository content. They are stored on the plan (`work_verify_v1`) with who
declared them, and shown by `tend work show`.

tendd records the declarer from the caller it observes: the agent whose pane
the caller runs in, or else the person. The person may always change a
Work's acceptance and checks; an agent may set them where nobody has, or
change its own, but not the person's or another agent's. This keeps the agent
whose work is checked from rewriting its own checks. It is not
authentication: every caller runs as the same user.

tendd runs the checks itself, one after another in the directory they are
pinned to, when an agent reports a result: `tend_file_result` or
`tend work done --summary` on the Work (or on a collaboration request sent
for it), a result recorded as already decided from an agent's pane, and
`tend work done` before it marks the alias done. Whether a result comes
from an agent is what tendd observes about the caller, not what the record
says. Each check gets its own time limit (default 300 s, at most 1,200 s)
and all of them together at most 1,200 s; output is kept to its last 40
lines. Whatever a check started is stopped when it ends; a process that
escapes and keeps the output open fails the check after five seconds rather
than holding it.

- Every check passes: the result record is filed with each check as test
  evidence marked as run by tend (`"verified": true`), ahead of the agent's
  own evidence. An agent's own claim to `verified` is cleared.
- A check fails, times out or cannot start: nothing is filed, the alias
  stays open, and the agent gets `checks_failed` with each check's output to
  fix and report again. The person is not asked.

Every result and every completion runs the checks afresh: a pass from an
earlier call cannot show that what is completed now is what was checked, so
`tend work done --summary` (file, then complete) runs them twice.

### External issues

```sh
tend work link ENG-1 --github Open330/tend#483   # or an issue/PR URL
tend work link ENG-1 --linear ENG-1234
tend work refresh ENG-1                          # or --all
```

`link` reads the issue and stores it on the Work record; `refresh` re-reads
the title and state of every linked GitHub or Linear issue. Both are
read-only toward the tracker and spend no agent turn: GitHub through
`gh api` (your `gh` login), Linear through its GraphQL API with
`LINEAR_API_KEY`. The tracker's state is shown (`tend work show`, the
dashboard) but never moves the Work's own stage. A snapshot read from the
tracker is not overwritten by the older title and state a pane was launched
with, and GitHub owner/repo spellings that differ only in case are one item.
`gh` is pinned to github.com, whatever `GH_HOST` says. tendd stores what the CLI
fetched (`work_external_upsert`, `work_external_v1`) after checking it for
control characters and size.

## Current tmux compatibility

Managed tmux currently binds one workspace to a session, one active Work run to
a window, and agent sessions to panes. This is a lifecycle policy and storage
adapter, not the logical cardinality: the snapshot and dashboards already allow
multiple Runs under one Work. Existing `@tend_work_id` metadata and v1 dashboard
records are migrated rather than discarded.

`tend work up ENG-1234` may use the external display key as the default Work ID
for convenience. The resulting Work and the Linear/GitHub/Jira issue remain
separate objects: the Work owns its local stage and goal, while the external
reference owns provider status and URL.

## 한국어 요약

- `Work`는 Tend가 소유하는 결과 단위이며 ticket/issue 자체가 아닙니다.
- Linear/GitHub/Jira 항목은 Work에 연결되는 선택적 외부 참조입니다.
- tmux session/window/pane은 Workspace/Work/Agent의 영구 정체성이 아니라 현재
  Run을 실행하기 위한 binding입니다.
- 대시보드는 `Queued / In progress / Review / Done`만 단계로 사용하고,
  `Attention / Blocked / Error`는 별도 신호로 표시합니다.
- 관리되지 않은 일반 window는 Work로 추론하지 않고 `Unlinked executions`로
  분리합니다.
