# Work pipeline — `muxa work up`

`muxa work start`가 명령형 primitive입니다. 한 번 실행하면 agent session 하나가
생깁니다. `muxa work up`은 선언형입니다. 안정적인 Muxa Work id와 선택적인 외부
이슈 키를 별도로 주면 현재 Run을 만들거나 선언 상태로 수렴시킵니다.

```console
$ muxa work up auth-cleanup --external CAL-1234
work auth-cleanup is in workspace callabo via pipeline triad
  cwd      /home/june/worktrees/auth-cleanup (worktree auth-cleanup, created)
  external linear:CAL-1234 Reaper double-reaps a lying pane  [In Progress]
           https://linear.app/rtzr/issue/CAL-1234
  + plan       codex     planner      %12
  + impl       codex     implementer  %13
  + review     claude    reviewer     %14
  layout   main-vertical
```

내부적으로 Workspace와 Work는 지속되는 논리 객체입니다. tmux session은
Workspace를, window는 현재 Run을, pane은 agent session을 바인딩합니다.
([WORK_MODEL.md](WORK_MODEL.md))

## 설정부터

`[ticket]`·`[[route]]`·`[pipeline.*]`는 muxa가 요구하는 가장 구조적인 설정이고
가장 짐작하기 어렵습니다 — 중첩 테이블, 순서 있는 route 배열, 정규식, 플레이스홀더
템플릿. 그래서 이 문서를 보며 손으로 쓰지 않아도 됩니다.

```console
$ muxa work init
◇  Describe the work pipeline you want
│  cal-* 티켓은 codex 기획, codex 구현, claude 리뷰로
```

muxa는 headless agent 한 턴으로 그 문장을 TOML로 바꾸고, **위임하지 않는 부분**을
합니다 — 결과를 파싱하고, 정규식을 전부 컴파일하고, route가 지목한 pipeline이
존재하는지와 나열된 agent가 실제로 뜰 수 있는지 확인하고, 무엇이 바뀔지 출력한
뒤 확인을 받고서야 씁니다. 모델이 없는 키를 만들거나 `vim`을 agent라고 하면 이유를
대고 거부하며 config는 그대로 둡니다. `Config`는 unknown field를 거부하므로,
오타가 디스크에 쓰이면 다음 데몬 시작이 죽습니다.

건드리는 건 `[ticket]`·`[[route]]`·`[pipeline.*]` 셋뿐입니다. `toml_edit`으로 다시
쓰므로 주석과 나머지 섹션은 그대로 살아남습니다.

여기의 모든 경로는 **headless agent 턴 하나를 쓰고, 그 비용은 계정에 과금됩니다.**
muxa는 실행 전에 무엇을 호출하는지 먼저 알리고, `--dry-run`은 파일 쓰기만
건너뜁니다 — 턴은 그대로 돌고 그대로 과금됩니다. 비대화형 호출은 `--yes`로
그 지출을 확인해야 합니다.

```console
muxa work init                                # 대화형으로 묻기
muxa work init --describe "..."               # 비대화형
muxa work init --dry-run                      # 제안만 보고 안 씀
muxa work init --agent codex                  # 다른 resolver 사용
```

손으로 쓰는 게 편하시면 주석 달린 레퍼런스가
[`config.example.toml`](../config.example.toml)에 있고, 아래가 각 부분의 설명입니다.

## 구조

```
Work id ─────────────────────────▶ 지속되는 Work identity
외부 이슈 ─▶ [ticket.source] ─▶ 외부 context + [[route]]
                                             │
                                             ▼
                              workspace + cwd/worktree + pipeline
                                             ▼
                                  원하는 pane vs 실제 pane
                                             ▼
                                        차이만 생성
```

## ticket 조회는 agent에게 위임합니다

muxa는 Linear, Jira, GitHub를 직접 말하지 않고 앞으로도 그럴 계획이 없습니다.
headless agent turn 하나(`claude -p` / `codex exec`, `muxa ask`가 쓰는 그
bridge)를 써서 agent에게 ticket 조회를 시킵니다. 이미 skill, MCP server, `gh`,
환경변수 토큰으로 agent CLI에게 그 방법을 가르쳐 뒀기 때문입니다. provider를
추가하는 일은 release가 아니라 prompt입니다.

```toml
[ticket.source.linear]
match  = '^cal-\d+$'
prompt = '''
linear skill로 Linear issue {{id}}를 조회해서 JSON object 하나만 출력해라.
{"id": "...", "title": "...", "body": "...", "url": "...", "state": "..."}
'''
```

응답은 첫 글자부터 파싱하지 않고 JSON object를 **스캔**합니다. agent는 JSON
주위에 산문을 붙이는 일이 흔하기 때문입니다 — fence, 앞머리 한 문장, 끝에
덧붙이는 제안. 균형 잡힌 마지막 object가 이기므로 prompt에 적어 둔 예시 shape가
실제 답을 이기지 않습니다. 흔한 필드 표기(`body`에 `description`, `id`에
`identifier`, `state`에 `{"name": …}`)도 받아들입니다.

결과는 `[ticket].cache_secs`(기본 15분) 동안 캐시되므로 pane 하나 더 붙이려고
다시 실행해도 turn을 또 쓰지 않습니다. `--refresh`는 캐시를 무시하고,
`--no-ticket`은 조회를 건너뛰고 id만으로 띄웁니다.

## routing은 사용자 것입니다

```toml
[[route]]
match     = '^cal-'
workspace = 'callabo'
pipeline  = 'triad'

[route.worktree]
repo   = '~/workspace/callabo'
branch = '{{id}}'

[[route]]
match    = '.*'
pipeline = 'solo'
```

route는 순서 있는 목록이고 첫 매치가 이깁니다. 구체적인 규칙을 위에, catch-all을
아래에 둡니다. route가 정하는 것은 셋입니다: work가 들어갈 tmux session, agent가
돌 디렉터리, 그리고 그 window를 채울 pipeline.

`[route.worktree]`를 두면 work마다 git worktree가 생깁니다. 한 window의 agent
셋이 같은 checkout에서 서로를 밟지 않게 해 주는 장치입니다. 기본 경로는 repo
**바깥**(`<repo>/../<repo-name>-worktrees/<id>`)입니다. repo 안에 두면 부모의
status와 agent가 돌리는 모든 `find`에 걸리기 때문입니다. 이미 있는 worktree는
재사용하고, 이미 있는 branch는 새로 만들지 않고 checkout합니다.

route 없이도 시작할 수 있습니다. `muxa work up cal-1234 --pipeline triad`는 그
플래그 자체를 routing 결정으로 보고 현재 디렉터리를 씁니다.

## pipeline은 script가 아니라 desired state입니다

```toml
[pipeline.triad]
layout = 'main-vertical'
prompt = '''
{{work}} — {{ticket.title}}
{{ticket.url}}

{{ticket.body}}
'''

[[pipeline.triad.agent]]
alias   = 'plan'
program = 'codex'
role    = 'planner'
prompt  = '너는 기획을 맡는다. 접근안을 먼저 쓰고 코드는 고치지 마라.'

[[pipeline.triad.agent]]
alias   = 'impl'
program = 'codex'
role    = 'implementer'
prompt  = '너는 구현을 맡는다. 기획을 따르고 범위를 바꾸려면 먼저 물어라.'

[[pipeline.triad.agent]]
alias   = 'review'
program = 'claude'
role    = 'reviewer'
prompt  = '너는 리뷰를 맡는다. 구현을 비판하되 직접 고치지 마라.'
```

`alias`가 핵심입니다. desired-vs-actual diff가 이 키로 돌고, pane 자체에
기록되므로(`@muxa_agent_alias`) muxad, CLI 프로세스, pane 안의 agent 재시작을
모두 넘겨 살아남습니다. pipeline 안에서 유일해야 하고, 그 아래 pane이 생긴
뒤에는 바꾸지 않아야 합니다.

pipeline의 `prompt`는 모든 agent에게 필요한 context를 한 번만 적는 자리이고,
각 agent의 `prompt`가 그 뒤에 붙습니다. `role`도 pane에 기록되므로 collaboration
레이어에서 `role:reviewer`로 지목할 수 있습니다.

`layout`은 모든 pane이 생긴 다음에만 적용됩니다. window를 반복해서 split하면
그때그때 active pane이 반으로 쪼개지므로, 도중에 세 번 고치는 것보다 끝에 한 번
정리하는 편이 맞습니다.

## 다시 실행하면 수렴합니다

`muxa work up`을 다시 실행하면 pipeline과 window의 현재 pane을 비교합니다.

- alias에 pane이 없다 → **launch**
- alias에 살아있는 pane이 있다 → **keep**, 건드리지 않음
- alias에 살아있는 pane이 있고 요청을 줬다 → **그 pane에 전송**

```console
$ muxa work up cal-1234           # reviewer pane이 닫혀 있던 상태
  = plan       running                %12
  = impl       running                %13
  + review     claude    reviewer     %21
```

첫 호출은 팀을 세우고, 두 번째는 no-op이고, 무언가 죽은 뒤의 호출은 정확히 그
구멍만 메웁니다.

## 요청: `--body`, `--skill`, `--context`

일감이 *무엇인지*는 요청으로 들어옵니다. `muxa_call_peer`가 받는 방식 그대로입니다.

```console
$ muxa work up cal-1234 \
    --skill review-plan \
    --body "reconciler가 살아있는 pane을 먹는 double reap을 고쳐라" \
    --context "main에서는 테스트 통과함"
```

등록된 `[message.skills]` 항목이 먼저 펼쳐지고, 그다음 body, 그다음 context가
`Invocation context:` 머리말 아래 붙습니다. 합성기는 협업 도구와 **한 벌을
공유**하므로 peer에게 통하는 표현이 pipeline에도 그대로 통합니다. 그 외에는
아무것도 안 붙습니다 — muxa가 대신 transcript나 diff를 끼워 넣지 않습니다.

같은 요청이 **상태에 따라 두 가지로 배달**됩니다.

| pipeline alias | 요청이 가는 곳 |
| --- | --- |
| pane 없음 | 그 agent의 launch prompt |
| 살아있는 pane 있음 | 그 pane에 타이핑 |

```console
$ muxa work up cal-1234 --body "이어가기 전에 main으로 rebase해라"
  » plan       prompted               %12
  » impl       prompted               %13
  » review     prompted               %21
```

"시작"과 "진행 중 개선"이 별도 명령이 아닌 이유가 이것입니다. 둘은 상태가 다른
같은 요청이고, k8s 비유가 실제로 성립하는 지점도 여기입니다. `--prompt`는
`--body`의 다른 표기로 남아 있습니다.

요청 없이 그냥 다시 실행하면 살아있는 agent는 건드리지 않습니다. turn 중인
agent에게 prompt를 밀어 넣는 건 명시적으로 요청받을 만큼 방해가 되는 행동이라서요.

어떤 alias도 주장하지 않는 pane — 직접 띄운 것이거나 지금은 수정된 pipeline이
남긴 것 — 은 **보고만 하고 절대 건드리지 않습니다**.

```console
  ? (no alias) gemini    unclaimed    %30
```

desired state로 수렴시키는 건 유용합니다. 하지만 사람이 연 pane을 desired state
**바깥으로** 치우는 건 orchestration이 불신을 얻는 방식입니다.

## Placeholder

템플릿은 `{{이중}}` 중괄호를 쓰고, muxa가 아는 키만 치환하며 나머지는 그대로
둡니다. resolver prompt가 요청하는 `{"id": "..."}` 형태를 그대로 담을 수 있는
이유이자, 오타가 조용히 빈칸이 되는 대신 prompt에 그대로 드러나는 이유입니다.

| 키 | 값 |
| --- | --- |
| `{{id}}` | 소문자 work id — branch명과 디렉터리용 |
| `{{work}}` | muxa가 저장·표시하는 형태의 work id (`CAL-1234`) |
| `{{workspace}}` | 결정된 workspace/session |
| `{{cwd}}` | 결정된 작업 디렉터리 |
| `{{alias}}`, `{{role}}`, `{{program}}` | 렌더링 중인 agent |
| `{{request}}` | 합성된 `--skill` / `--body` / `--context` |
| `{{ticket.title}}` `{{ticket.body}}` `{{ticket.url}}` `{{ticket.state}}` `{{ticket.id}}` `{{ticket.branch}}` | 조회된 ticket context |

`{{ticket.body}}`는 4000자에서 `…[truncated]` 표시와 함께 잘립니다. launch
prompt는 일의 모양과 URL을 나르고, 나머지는 agent가 직접 읽으면 됩니다.

`{{request}}`만 한 가지가 특별합니다. pipeline의 어떤 템플릿도 이걸 배치하지
않으면 모든 agent의 prompt 맨 앞에 자동으로 붙습니다. `--body`가 생기기 전에
작성된 pipeline이 body를 조용히 삼키면 안 되기 때문입니다. 직접 `{{request}}`를
써서 원하는 위치에 두면 앞에 중복으로 붙지 않습니다.

즉 agent의 launch prompt는 바깥부터 세 겹입니다.

```text
<request>            이번 호출에서 요청한 것
<pipeline.prompt>    이 pipeline의 모든 agent에게 필요한 것 (보통 티켓)
<agent.prompt>       이 agent의 역할
```

## 명령 정리

```console
muxa work init                       # 말로 설명해 설정 쓰기
muxa work up <work> --external <id>  # 외부 이슈 연결 → routing → 없는 것만 생성
muxa work up <id>                    # 호환 경로: <id>를 외부 이슈로도 조회
muxa work up <id> --dry-run          # 계획만 출력, tmux는 건드리지 않음
muxa work up <id> --pipeline triad   # route의 pipeline을 덮어씀
muxa work up <id> --body "..."       # 일감 내용; launch 또는 전달
muxa work up <id> --skill review-plan --context "..."   # muxa_call_peer와 같은 합성기
muxa work up <id> --no-ticket        # 조회 없이 id만으로 실행
muxa work up <id> --refresh          # 캐시된 ticket 무시
muxa work up <id> --json             # 구조화된 plan + 결과
muxa work down <id>                  # window와 그 안의 agent 전부 종료
```

`muxa work down`은 `muxa work close`의 다른 표기입니다. 둘 다 unmanaged window는
건드리지 않고, 같은 work id가 여러 workspace에 있으면 `--workspace`를 요구합니다.

## MCP

같은 기능이 agent에게는 `muxa_start_work`로 노출되고, 인자도 동일합니다.

```text
muxa_start_work {
  "work": "auth-cleanup",
  "external": "CAL-1234",
  "body": "double reap을 고쳐라",
  "skill": "review-plan",
  "dry_run": true
}
```

`muxa_start_agent`를 여러 번 부르는 것보다 이쪽이 낫습니다. 그쪽은 이미 있는
agent와 아직 만들어야 할 agent를 구분하지 못해 수렴 대신 팀을 복제합니다.
[MCP.md](MCP.md) 참고.

## dashboard에서

같은 파이프라인을 work board의 **start work** 컨트롤에서도 실행할 수 있습니다.
`POST /api/work-control/up`으로 갑니다. 제어 토큰 **위에** `[dashboard]
allow_work_start = true`가 더 필요합니다 — dashboard의 다른 쓰기는 이미 떠 있는
프로세스를 조종하지만, 이건 권한을 우회한 프로세스를 새로 만들기 때문입니다.

이 경로에서도 Work와 외부 이슈는 분리됩니다. 외부 title과 provider status는
참조 정보로 연결될 뿐 로컬 Work title이나 workflow stage를 덮어쓰지 않습니다.
board의 로컬 stage(`queued`·`in_progress`·`review`·`done`)는 CLI에도 표시되고,
blocked와 attention은 별도 signal로 남습니다.

```console
$ muxa work list
auth-cleanup  workspace=callabo  session=callabo  window=@7  agents=3  cwd=…  stage=review
```

`stage=auto`는 아무것도 출력하지 않습니다. auto는 "아무도 말한 적 없음"이라,
항상 붙어 있는 칼럼보다 할 말이 있을 때만 나타나는 칼럼이 더 많은 걸 말합니다.
CLI는 그 저장소를 읽기만 하고, 쓰기는 daemon이 소유합니다.
[DASHBOARD.md](DASHBOARD.md) 참고.

## 설정

주석 달린 레퍼런스는 [`config.example.toml`](../config.example.toml)의
`[ticket]`, `[[route]]`, `[pipeline.*]` 섹션에 있습니다.
