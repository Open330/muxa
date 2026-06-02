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
columns = ["pane", "state", "model", "ctx", "cost", "prompt", "activity"]
sort = ["session", "activity"]
hide_paneless = true

[watch.widths]
prompt = "min:20"
activity = 5

[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

TUI 동작, column, sort, keybinding은 [WATCH.ko.md](WATCH.ko.md)에 있습니다.

## Reconciler

```toml
[reconciler]
enabled = true
interval_secs = 30
stuck_working_timeout_secs = 0
stuck_waiting_timeout_secs = 0
```

stale state가 오래 남는 것을 줄입니다. timeout 값 `0`은 해당 timeout 비활성화입니다.

## Dashboard

```toml
[dashboard]
enabled = false
bind = "127.0.0.1:7878"
token = ""
allow_public = false
```

dashboard는 명시적으로 public binding을 허용하기 전까지 loopback-only입니다.
자세한 내용은 [DASHBOARD.md](DASHBOARD.md).

## External Sinks

sink는 opt-in fan-out target입니다. 현재 문서화된 sink는 prompt를 oh-my-prompt로
forward합니다. 자세한 내용은 [SINKS.md](SINKS.md).

## Zellij

`MUXA_HOST=tmux|zellij`로 host selection을 고정할 수 있습니다. tmux는 full
backend이고, zellij는 CLI baseline 이후 richer support를 계획 중입니다.
자세한 내용은 [ZELLIJ.md](ZELLIJ.md).
