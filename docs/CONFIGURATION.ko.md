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

## Watch

```toml
[watch]
view = "session"
columns = ["pane", "state", "model", "ctx", "cost", "workload", "prompt", "activity"]
sort = ["session", "latest"]
hide_paneless = true

[watch.widths]
prompt = "min:20"
workload = 14
activity = 5

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
token 없는 public API는 추가로 `auth = "none"`을 명시해야 합니다.
자세한 내용은 [DASHBOARD.md](DASHBOARD.md).

## External Sinks

sink는 opt-in fan-out target입니다. 현재 문서화된 sink는 prompt를 oh-my-prompt로
forward합니다. 자세한 내용은 [SINKS.md](SINKS.md).

## Zellij

`MUXA_HOST=tmux|zellij`로 host selection을 고정할 수 있습니다. tmux는 full
backend이고, zellij는 CLI baseline 이후 richer support를 계획 중입니다.
자세한 내용은 [ZELLIJ.md](ZELLIJ.md).
