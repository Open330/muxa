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
permission_mode = "default" # default | edit | bypass
additional_dirs = [] # 추가 real path. 예: ["/nfs/home/june"]
timeout_secs = 180
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

`permission_mode = "default"`는 agent CLI의 기본 권한 정책을 유지합니다. `edit`은
sandbox/자동 검토를 유지한 채 workspace 편집을 허용하고, `bypass`는 전체 issue
resolver 같은 무인 작업을 위해 승인과 sandbox를 비활성화합니다. 신뢰하는 prompt와
경로에서만 사용하세요. `additional_dirs`도 agent CLI에 전달됩니다. `cwd` 아래 파일이
외부 경로를 가리키는 symlink라면 real path를 추가해야 합니다. 예를 들어
`/home/june/workspace`가 NFS를 가리키면 `["/nfs/home/june"]`를 사용합니다.

## Collaboration

```toml
[collaboration]
enabled = true
wake = "idle_only" # idle_only | never
max_message_bytes = 16384
```

같은 stable tmux window에 있는 agent 사이의 durable request/reply 기능입니다.
optional `path` 기본값은 `$XDG_DATA_HOME/muxa/collaboration.json`이며 mailbox와
exact-session alias/role을 함께 저장합니다.
`idle_only`는 hook 기반 top-level agent가 Idle일 때만 짧은 request/reply
notification을 입력하며 본문은 mailbox에 둡니다.
[COLLABORATION.ko.md](COLLABORATION.ko.md)를 참고하세요.

## Watch

```toml
[watch]
view = "session"
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]
sort = ["state", "session", "latest"]
hide_paneless = true

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6

[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

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

discovery는 tmux pane을 훑어 알려진 agent CLI(`claude`/`codex`/`gemini`)를
찾아 hook이 오기 전에 레지스트리를 채웁니다. 데몬 시작 시 1회 실행되고 이후
`interval_secs`마다 재스캔하므로, 새 tmux 세션에서 갓 시작한 agent가 첫 hook을
쏘기 전이라도 그 주기 안에 `muxa status`에 뜹니다. `interval_secs = 0`이면
기존 "시작 시 1회"만, `enabled = false`면 discovery를 완전히 끕니다. 재스캔은
reconciler가 이미 호출하는 `tmux list-panes`를 재사용하므로 비용은 무시할
수준입니다.

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

## Zellij

`MUXA_HOST=tmux|zellij`로 host selection을 고정할 수 있습니다. tmux는 full
backend이고, zellij는 CLI baseline 이후 richer support를 계획 중입니다.
자세한 내용은 [ZELLIJ.md](ZELLIJ.md).
