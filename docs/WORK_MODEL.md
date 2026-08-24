# Muxa Work domain

The dashboard and execution topology use related but different identities.
Muxa owns the logical work model; tmux supplies an execution surface.

```text
Workspace
└── Work (muxa-owned outcome)
    ├── External issue reference (optional: Linear, GitHub, Jira, ...)
    └── Run (execution attempt)
        └── Agent session
            └── tmux/rmux/herdr/zellij binding
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
  execution-focused surfaces such as `muxa watch`.

## Current tmux compatibility

Managed tmux currently binds one workspace to a session, one active Work run to
a window, and agent sessions to panes. This is a lifecycle policy and storage
adapter, not the logical cardinality: the snapshot and dashboards already allow
multiple Runs under one Work. Existing `@muxa_work_id` metadata and v1 dashboard
records are migrated rather than discarded.

`muxa work up CAL-1234` may use the external display key as the default Work ID
for convenience. The resulting Work and the Linear/GitHub/Jira issue remain
separate objects: the Work owns its local stage and goal, while the external
reference owns provider status and URL.

## 한국어 요약

- `Work`는 Muxa가 소유하는 결과 단위이며 ticket/issue 자체가 아닙니다.
- Linear/GitHub/Jira 항목은 Work에 연결되는 선택적 외부 참조입니다.
- tmux session/window/pane은 Workspace/Work/Agent의 영구 정체성이 아니라 현재
  Run을 실행하기 위한 binding입니다.
- 대시보드는 `Queued / In progress / Review / Done`만 단계로 사용하고,
  `Attention / Blocked / Error`는 별도 신호로 표시합니다.
- 관리되지 않은 일반 window는 Work로 추론하지 않고 `Unlinked executions`로
  분리합니다.
