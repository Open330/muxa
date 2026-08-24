# Workspace → work → agent migration

Muxa의 기본 tmux domain model을 다음 계층으로 전환한다. 기존
`session = work` 형식은 읽거나 변환하지 않는다.

| tmux object | Muxa execution binding | lifecycle |
| --- | --- | --- |
| session | workspace 실행 context | 여러 Work Run을 담고 workspace 종료 시 제거 |
| window | Work의 현재 Run | Work 생성·재사용 시 binding하고 종료하면 Run이 끝남 |
| pane | agent 실행 surface | role/task를 가진 Agent session의 현재 binding |

논리 모델은 `Workspace → Work → Run → Agent session`입니다. Linear/GitHub/Jira
issue는 Work에 연결되는 외부 참조이지 Work 자체가 아닙니다. 자세한 invariant는
[WORK_MODEL.md](WORK_MODEL.md)를 참고합니다.

## Implementation checklist

### 1. tmux metadata and lifecycle

- [x] session option을 `@muxa_workspace_id`, `@muxa_workspace_cwd`,
  `@muxa_managed_workspace`로 정의한다.
- [x] window option에 `@muxa_work_id`, `@muxa_work_cwd`,
  `@muxa_managed_work`를 기록한다.
- [x] pane option에는 agent, role, task, workspace ID, work ID를 기록한다.
- [x] workspace/session, work/window, agent/pane을 한 번에 읽는 parser와
  `WorkspaceInfo → WorkInfo → ManagedAgentPane` 결과를 제공한다.
- [x] `close_work`는 exact managed window만 종료하고 `close_workspace`는
  exact managed session을 종료한다.
- [x] unmanaged session/window/pane에 대한 destructive action은 계속 거부한다.

### 2. deterministic launch

- [x] `muxa work start`와 `muxa agent start --work`에 `--workspace`를 추가한다.
- [x] workspace가 없으면 session과 첫 work window를 함께 만들고, workspace만
  있으면 새 work window, work도 있으면 해당 window의 agent pane을 만든다.
- [x] work마다 별도 cwd를 허용해 project session 안의 여러 worktree를 지원한다.
- [x] lower-level `--placement pane|window|session`은 unmanaged surface API로
  유지하되 managed `--work`와 혼합하지 않는다.
- [x] 결과에 workspace, work, session, window, created_workspace,
  created_work를 명시한다.

### 3. CLI and MCP

- [x] `muxa workspace list/show/close` lifecycle을 추가한다.
- [x] `muxa work list/show/close` 출력에 workspace/session/window를 포함한다.
- [x] `muxa_start_agent`가 workspace를 받고 work window를 생성·재사용한다고
  설명한다.
- [x] `muxa_manage_tmux`에 workspace list/show/close action과 workspace 인자를
  추가한다.
- [x] MCP instructions와 examples를 `workspace → work → agent`로 바꾼다.

### 4. watch and collaboration surfaces

- [x] watch session view를 workspace row → work window row → agent pane row로
  구성한다.
- [x] new-work form과 quick spawn은 workspace session 안에 work window를 만들고
  해당 window를 재사용한다.
- [x] preview, attach, message target은 exact agent pane을 유지한다.
- [x] collaboration의 existing same-window room을 work 경계로 설명하고 표시한다.
- [x] dashboard/status에서 session/window identity가 혼동되지 않도록 workspace와
  work label을 구분한다.

### 5. onboarding and documentation

- [x] 1–10단계는 Muxa identity를 미리 주장하지 않고 tmux의 shell, prefix,
  session, window, pane, detach/attach 자체를 설명한다.
- [x] 4/20은 window 생성과 화면 전환만 설명하고 “layout only” 문구를 제거한다.
- [x] Muxa 소개 시점부터 `session = workspace`, `window = work`,
  `pane = agent` mapping을 가르친다.
- [x] README, MCP, watch, onboarding 문서와 CLI help를 새 모델로 통일한다.
- [x] onboarding Tape/GIF를 다시 녹화하고 단계 전환과 hierarchy를 확인한다.

### 6. verification and delivery

- [x] parser, launch planning, lifecycle confirmation, CLI/MCP schema, watch hierarchy,
  onboarding rendering tests를 갱신한다.
- [x] `cargo fmt`, workspace test, Clippy, diff check를 깨끗한 worktree에서 통과한다.
- [x] 관련 파일만 커밋·푸시하고 설치된 `muxa`에서 help/print 출력을 확인한다.
