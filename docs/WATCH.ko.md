# Live TUI

`muxa watch`는 주요 interactive surface입니다. 추적 중인 agent와 일반 tmux
pane을 보여주고, pane attach, live preview, prompt composer를 제공합니다.

## 실행

```bash
muxa watch
muxa watch --view session
muxa watch --view pane
muxa watch --include-paneless
```

`view = "session"`은 tmux session 기준으로 묶고, `view = "pane"`은 pane별로
한 줄씩 보여줍니다.

## 주요 키

| Key | Action |
| --- | --- |
| `Enter` | 선택한 pane의 prompt composer 열기. 빈 `Enter`는 attach. |
| `Esc` / `q` | 종료 또는 popup 닫기. |
| `p` | live preview 열기. |
| `c` | preview content toggle. |
| `f` | popup/fullscreen preview toggle. |
| `?` | 도움말. |
| `l` / `a` | 최신 activity 기준 정렬. |
| `d` | session duration 기준 정렬. |
| `s` | session grouping 정렬. |
| `t` | attention state 우선 정렬. |
| `r` | refresh. |

## Prompt Composer

pane이 있는 row에서 `Enter`를 누르면 prompt composer가 열립니다. 내용을 입력한
뒤 `Enter`를 누르면 해당 pane으로 보냅니다. `Esc`는 취소입니다. composer가
비어 있으면 `Enter`는 prompt 전송 대신 pane attach로 동작합니다.

activity logging이 켜져 있으면 prompt input 시간은 `activity.ndjson`에 human
interaction interval로 기록됩니다.

## tmux Popup Binding

```tmux
bind-key s display-popup -E -w 90% -h 80% "muxa watch"
```

## Columns

`[watch]`에서 설정합니다:

```toml
[watch]
view = "session"
columns = ["pane", "state", "model", "ctx", "cost", "prompt", "activity"]

[watch.widths]
prompt = "min:20"
activity = 5
```

사용 가능한 column key: `pane`, `pane_id`, `state`, `kind`, `model`, `ctx`,
`cost`, `limits`, `prompt`, `activity`, `session_time`.

## Sort

```toml
[watch]
sort = ["session", "latest"]
# sort = ["latest"]
# sort = ["session_time"]
# sort = ["state", "latest"]
# sort = ["session", "pane"]
# sort = ["pane_id"]
```

기본값은 tmux session으로 묶고, 각 group 안에서 가장 최근 activity가 있는
agent를 위로 올립니다. `activity`와 `act`는 `latest` alias로 계속 동작합니다.

## Detail Row

```toml
[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

사용 가능한 변수: `pane`, `kind`, `state`, `model`, `ctx`, `cost`, `activity`,
`last_prompt`, `last_response`, `last_notification`, `cwd`.

긴 detail 내용은 table에 맞게 잘립니다. 더 많은 맥락이 필요하면 preview를
사용하세요.
