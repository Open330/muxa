# 설정

`muxad`는 `$XDG_CONFIG_HOME/muxa/config.toml`이 있으면 읽습니다. 전체 주석
예시는 `config.example.toml`에서 확인할 수 있습니다.

## Socket

```toml
socket = "/tmp/muxa.sock"
```

CLI는 `MUXA_SOCKET`도 사용합니다. daemon startup 때 tmux environment를
self-heal해서 기존 pane도 현재 socket을 찾을 수 있게 합니다.

## History

```toml
[history]
enabled = true
path = "$XDG_DATA_HOME/muxa/prompts.ndjson"
max_per_pane = 50
max_age_days = 30
```

prompt history는 무제한 warehouse가 아니라 retained window입니다. `muxa recap`과
`muxa stats`의 prompt total은 이 범위를 기준으로 합니다.

## Activity

```toml
[activity]
enabled = true
path = "$XDG_DATA_HOME/muxa/activity.ndjson"
max_age_days = 30
```

activity ledger는 agent state interval, tmux foreground interval, muxa human
interaction interval을 저장합니다. 자세한 기준은 [ACTIVITY.ko.md](ACTIVITY.ko.md).

## Session Activity

```toml
[session_activity]
enabled = true
path = "$XDG_DATA_HOME/muxa/session-activity.json"
interval_secs = 5
```

tmux foreground sampler입니다. 새 activity ledger interval이 쌓이기 전까지
compatibility source이자 stats fallback으로 사용됩니다.

## Ask

```toml
[ask]
enabled = true
agent = "claude"     # claude | codex
cwd = "~"            # headless 프로세스가 실행될 위치. 기본값 $HOME
permission_mode = "bypass" # bypass(기본값) | edit | default
additional_dirs = [] # 추가 real path. 예: ["/nfs/home/june"]
timeout_secs = 1800    # wall-clock 제한 30분
keep = 200           # 보관할 답변 수. 넘으면 오래된 것부터 버립니다
```

`muxa watch`에서 보내는 headless 질의입니다. `a`로 묻고 `A`로 이력을 봅니다.
muxad가 agent를 print 모드로 실행해 답변을 수집하므로 관리할 세션이 없고, 완료
여부가 추측이 아니라 exit code로 정해집니다. agent마다 별도 대화를 유지하며 두
번째 질문부터는 그 대화를 resume해 첫 질문이 지불한 캐시 컨텍스트를 재사용합니다.
패널에서 `n`을 누르면 새 대화를 시작합니다. `path` 기본값은
`$XDG_DATA_HOME/muxa/ask.json`이고 이력과 agent별 thread id를 함께 저장합니다.
daemon이 회원님 계정으로 과금되는 CLI를 띄우는 권한이라 기본은 꺼짐입니다.
[WATCH.ko.md](WATCH.ko.md)를 참고하세요.

`permission_mode = "bypass"`가 기본값입니다. headless agent는 승인 prompt에 응답할
수 없으므로 전체 issue resolver 같은 무인 작업을 위해 승인과 sandbox를
비활성화합니다. 신뢰하는 prompt와 경로에서만 ask를 사용하세요. `edit`은
sandbox/자동 검토를 유지한 채 workspace 편집을 허용하고, `default`는 agent CLI의
기본 권한 정책을 유지합니다. `additional_dirs`도 agent CLI에 전달됩니다. `cwd` 아래
파일이 외부 경로를 가리키는 symlink라면 real path를 추가해야 합니다. 예를 들어
`/home/june/workspace`가 NFS를 가리키면 `["/nfs/home/june"]`를 사용합니다.
`timeout_secs` 기본값은 persistent worker를 준비하는 skill을 고려해 30분입니다.
이 제한에 도달하면 headless agent 프로세스가 종료됩니다. inactivity timeout이 아닌
전체 wall-clock 안전 제한입니다.

TUI를 열지 않아도 동일한 daemon 소유 이력을 사용할 수 있습니다.

```bash
muxa ask --agent codex "현재 구현을 요약해줘"
muxa ask --agent claude --detach --json "배포 계획을 검토해줘"
security find-generic-password -w -s my-codex-key \
  | muxa ask --agent codex --api-key-stdin "이 저장소를 검토해줘"
```

`--api-key-stdin`은 interactive terminal 입력을 거부합니다. 키는 owner-only Unix
socket을 거쳐 선택한 provider의 단 한 번의 child process에만 전달되며 muxa
config/history나 argv에는 저장되지 않습니다. 이 옵션이 없으면 기존 Claude
Code/Codex 로그인 또는 provider 환경을 그대로 사용합니다.

## Collaboration

```toml
[collaboration]
enabled = true
wake = "idle_only" # idle_only | never
wake_payload = "operator_full" # notice | operator_full | full
scope = "window"   # window | host
max_message_bytes = 16384
# path = "$XDG_DATA_HOME/muxa/collaboration.json"
# retention_days = 90 # 생략하면 영구 보존
```

같은 stable tmux window에 있는 agent 사이의 durable request/reply 기능입니다.
과거 optional `path` 기본값은 `$XDG_DATA_HOME/muxa/collaboration.json`입니다.
Muxa는 기존 JSON을 authoritative sibling `collaboration.sqlite3`에 한 번 import하고
JSON은 migration backup으로 유지합니다. `.sqlite`, `.sqlite3`, `.db` path를 설정하면
그 파일을 직접 사용합니다. DB는 mailbox와 exact-session alias/role을 함께 저장합니다.
`retention_days`는 daemon 시작 시 조건을 만족하는 전달 완료 terminal thread를
정리하며 생략하면 모두 보존합니다. 남은 JSON backup은 본문의 중복 사본이고 retention
대상이 아닙니다.
`idle_only`는 hook 기반 top-level agent가 Idle일 때만 입력합니다.
기본값인 `wake_payload = "operator_full"`은 watch/dashboard 같은 operator surface에서
보낸 요청은 본문을 직접 전달하고, agent가 MCP/CLI로 보낸 요청은 mailbox 알림으로
유지합니다. `notice`는 모든 본문을 mailbox에 두며, `full`은 모든 요청을 원자적으로
claim해 직접 전달합니다. reply wake는 모든 모드에서 본문 없는 알림입니다.
`scope = "host"`이면 watch에서 다른 tmux window나 session의 선택된 tracked
agent를 정확한 pane id로 지정할 수 있습니다.
[COLLABORATION.ko.md](COLLABORATION.ko.md)를 참고하세요.

## 메시지 스킬

반복해서 쓰는 prompt 템플릿은 일반 TOML table에 저장하며 watch와 dashboard의
`m` composer, watch의 `a` composer, MCP의 `muxa_call_peer`가 함께 사용합니다.

```toml
[message.skills]
agent-review = "cx alias로 codex pane을 새로 만들고, 우리의 변경사항을 전달해 리뷰해줘"
```

직접 파일을 편집하지 않고도 관리할 수 있습니다.

```bash
muxa skill add agent-review 'cx alias로 codex pane을 새로 만들고, 우리의 변경사항을 전달해 리뷰해줘'
muxa skill list
muxa skill show agent-review
muxa skill remove agent-review
```

초안의 어느 위치에서든 `/`를 누르고 이름이나 본문을 입력해 검색합니다. 방향키나
`Tab`으로 선택하고 `Enter`를 누르면 기존 입력을 지우지 않고 현재 커서 위치에
템플릿이 삽입됩니다. 한 초안에 여러 스킬을 조합할 수도 있습니다. 이때 바로
전송되지 않으므로 내용을 확인·수정한 뒤 `Enter`를 한 번 더 눌러 보냅니다.
`muxa watch`의 팔레트에서는 `F2`로 추가/갱신 form을 열고 `Delete`로 선택한 스킬을
확인 후 삭제할 수 있습니다. 기존 `Ctrl-A`, `Ctrl-D`도 호환 alias로 유지합니다.
multi-line 템플릿은 CLI add 명령의 prompt
자리에 `-`를 넘겨 stdin으로 등록할 수 있습니다.

MCP가 연결된 agent 대화에서는 `/name`으로 같은 템플릿을 `muxa_call_peer`에
선택할 수 있고, 선택적인 body와 context가 템플릿을 바꾸지 않은 채 뒤에
추가됩니다. MCP process는 시작할 때 스킬 table을 읽으므로 스킬을 추가·수정·삭제한
뒤에는 실행 중인 agent를 재시작하세요.

스킬은 prompt 본문만 저장합니다. request kind, collaboration mode, agent, cwd,
timeout, permission scope를 포함하지 않습니다. `m`에서는 현재 선택한 kind/mode가
그대로 적용되고, `a`에서는 daemon의 `[ask]` agent와 `permission_mode`를 포함한 실행
설정이 그대로 적용됩니다. 따라서 스킬 삽입만으로 어느 계약의 권한도 넓어지지
않습니다.

## Watch

```toml
[watch]
view = "work"
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]
sort = ["state", "workspace", "latest"]
hide_paneless = true
collaboration_kind = "question"   # question | review | task | notice
collaboration_mode = "read_only"  # read_only | execute | just_send
collab_layout = "table"            # table | sequence

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6

[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

`m` composer에서 `Tab`이나 `Ctrl-E`로 badge를 바꾸면 watch가
`collaboration_kind`와 `collaboration_mode`를 갱신합니다. 마지막 선택은 composer를
닫거나 watch를 다시 실행한 뒤에도 유지됩니다.
`collab_layout`은 collaboration-history 화면만 제어하며 topology `layout`과
독립적입니다. 실행 중에는 `v`로 전환할 수 있습니다.

TUI 동작, column, sort, keybinding은 [WATCH.ko.md](WATCH.ko.md)에 있습니다.

## UI

```toml
[ui]
theme = "classic"
icons = "unicode"
```

사람이 보는 터미널 출력(`status`, `status-line`, `attend`, `watch`)의 공통
시각 기본값입니다.

- `theme` — 시각 프리셋: `classic`, `oh-my-muxa`, `focus`, `ops`, 또는
  모노크롬 프리셋. 한 번만 적용하려면 `--theme` 플래그로 덮어씁니다.
- `icons` — agent 상태 글리프 세트:
  - `unicode` (기본) — Geometric Shapes 글리프(`●` working, `▶` input,
    `◆` choice, `■` error, `○` idle, `◌` starting, `×` stopped).
  - `ascii` — 단일 문자 폴백(`*` working, `>` input, `?` choice, `!` error,
    `o` idle, `~` starting, `x` stopped). 기본 폰트에 유니코드 글리프가 없거나
    크기가 다른 폴백 폰트로 대체되는 터미널을 위한 옵션.

## Discovery

```toml
[discovery]
enabled = true
interval_secs = 30
```

discovery는 tmux pane을 훑어 알려진 agent CLI(`claude`/`codex`/`gemini`/`agy`)를
찾아 hook이 오기 전에 레지스트리를 채웁니다. 데몬 시작 시 1회 실행되고 이후
`interval_secs`마다 재스캔하므로, 새 tmux 세션에서 갓 시작한 agent가 첫 hook을
쏘기 전이라도 그 주기 안에 `muxa status`에 뜹니다. `interval_secs = 0`이면
기존 "시작 시 1회"만, `enabled = false`면 discovery를 완전히 끕니다. 재스캔은
reconciler가 이미 호출하는 `tmux list-panes`를 재사용하므로 비용은 무시할
수준입니다.

## Daemon

```toml
[daemon]
restart_on_new_binary = true
binary_poll_secs = 30
```

새 muxa를 설치해도 데몬은 저절로 재시작하지 않습니다. 패키지 매니저는 새 빌드를
디스크에 쓰고 `PATH` 위의 링크만 갈아끼우는데, 돌고 있는 프로세스는 열어둔 inode를
그대로 쓰며 옛 로직을 계속 서빙합니다. 서비스 매니저도 개입하지 않습니다 —
`KeepAlive`나 `Restart=always`는 프로세스가 *종료될 때* 반응하는데 아무것도 종료되지
않았기 때문입니다. 그대로 두면 데몬이 몇 주 지난 빌드를 서빙하고, 와이어 포맷이
바뀐 뒤에는 모든 CLI 호출에 `protocol mismatch`로 답하게 됩니다.

그래서 muxad는 자신이 re-exec할 경로를 감시하다가, 그 경로가 두 번 연속으로 다른
파일을 가리키면 새 빌드로 re-exec합니다. 두 번째 확인 폴링은 설치가 끝나지 않은
중간 상태를 새 빌드로 오인하지 않기 위한 것입니다. 데몬은 제자리에서 자신을
교체하므로(pid 동일) launchd·systemd·맨 터미널 어디서든 동일하게 동작하고,
re-exec이 실패해도 옛 이미지가 그대로 살아 있습니다.

업그레이드 순서를 다른 쪽이 관장한다면 — 예를 들어 여러 바이너리를 설치한 뒤 정해진
순서로 재시작하는 배포라면 — `restart_on_new_binary = false`로 끄십시오. 그때는
`muxa daemon restart`로 직접 재시작하면 되고, CLI와 버전이 어긋난 데몬은
`muxa doctor`가 알려줍니다.

## Reconciler

```toml
[reconciler]
enabled = true
interval_secs = 30
stuck_working_timeout_secs = 0
stuck_waiting_timeout_secs = 0
```

stale state가 오래 남는 것을 줄입니다. timeout 값 `0`은 해당 timeout 비활성화입니다.
같은 루프가 pid-liveness 스윕도 돌려, 등록된 백그라운드 task(`muxa register` 참고)는
프로세스가 종료되면 `stopped`로 전환됩니다.

## Fleet

```toml
[fleet]
enabled = true              # outbound SSH host; local은 항상 표시
refresh_secs = 15
keepalive_secs = 10
offline_after_secs = 30
connect_timeout_secs = 10
command_timeout_secs = 10
max_parallel_connects = 6
capture_policy = "selected" # selected | never

[fleet.local.labels]
environment = "development"

[fleet.local.annotations]
"muxa.dev/owner" = "platform"

[fleet.hosts.dev]
ssh = "muxa-devbox"
muxa_path = "muxa"
enabled = true
connect = "auto"            # auto | on_demand
mode = "observe"            # observe | control
# remote_socket = "/run/user/1000/muxa.sock"

[fleet.hosts.dev.labels]
environment = "development"
region = "icn"

[fleet.hosts.dev.annotations]
"muxa.dev/owner" = "platform"
```

Fleet은 controller를 첫 번째 `local` host로 항상 in-process 게시하며 `enabled = false`여도
사용할 수 있습니다. 이 flag는 outbound SSH host만 제어합니다. 활성 remote physical
host마다 persistent OpenSSH stdio relay 하나를 유지합니다.
`offline_after_secs`는 `keepalive_secs`의 두 배 이상이어야 하며 timeout/concurrency 값은
0일 수 없습니다. `capture_policy = "never"`는 control host에서도 pane/window capture를
manager 단계에서 차단합니다.

`ssh`에는 flag가 아닌 OpenSSH destination/Host alias를 씁니다. port, identity,
ProxyJump, host-key 정책은 `~/.ssh/config`에 둡니다. `muxa_path`와 `remote_socket`은
fixed remote command token으로 검증됩니다. label은 Kubernetes-style selector에 쓰고,
annotation은 설명형 value를 허용하지만 같은 namespaced key 문법을 사용합니다.
inventory는 `muxa host add/label/annotate`로 atomic하게 편집하는 것을 권장합니다.
controller metadata는 `muxa host label local`, `muxa host annotate local`로 관리하며
muxad가 제공하는 identity label은 덮어쓸 수 없습니다.
[FLEET.ko.md](FLEET.ko.md)를 참고하세요.

## Dashboard

```toml
[dashboard]
enabled = false
bind = "127.0.0.1:7878"
auth = "token"
token = ""
allow_public = false
```

dashboard는 명시적으로 public binding을 허용하기 전까지 loopback-only입니다.
익명 조회를 공개하되 browser 제어 기능만 PAT로 보호하려면 token과 함께
`auth = "public_read"`를 사용합니다. `auth = "none"`은 조회만 공개하고 모든
제어 요청을 비활성화합니다.
자세한 내용은 [DASHBOARD.md](DASHBOARD.md).

## External Sinks

sink는 opt-in fan-out target입니다. 현재 문서화된 sink는 prompt를 oh-my-prompt로
forward합니다. 자세한 내용은 [SINKS.md](SINKS.md).

## Pane host 선택

`MUXA_HOST=tmux|cmux|rmux|herdr|zellij`로 단일 host를 고정할 수 있습니다.
`MUXA_HOSTS`에는 `MUXA_HOSTS=rmux,tmux`처럼 순서가 있는 host 목록을 지정합니다.
rmux가 tmux 호환 환경변수도 함께 설정하므로 native `RMUX` 환경변수를 먼저
판별합니다. 자세한 내용은 [CMUX.md](CMUX.md), [RMUX.md](RMUX.md), [HERDR.md](HERDR.md),
[ZELLIJ.md](ZELLIJ.md)을 참고하세요.
