# Live TUI

`muxa watch`는 주요 interactive surface입니다. 추적 중인 agent와 일반 tmux
pane을 보여주고, pane attach, live preview, prompt composer를 제공합니다.

TUI 안에 머문 채 prompt 전송, turn abort, live capture 확인까지 하는
session-card console이 필요하면 [`muxa dashboard`](DASHBOARD_CLI.ko.md)를
사용하세요.

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
| `[` / `]` | preview에서 선택 session의 이전 / 다음 agent 보기. |
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

## Preview

`p`를 누르면 선택 pane의 preview가 열립니다. session view에서 선택한 session에
agent pane이 여러 개 있으면 `]`로 다음 agent, `[`로 이전 agent를 볼 수 있습니다.
`Tab`, `Shift+Tab`도 같은 동작입니다. agent가 둘 이상이면 preview title에
`2/3`처럼 현재 위치가 표시됩니다.

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
workload = 8
activity = 6
```

사용 가능한 column key: `pane`, `state`, `kind`, `model`, `ctx`, `cost`,
`limits`, `workload`, `prompt`, `activity`, `session_time`.
기본값에서는 child shell/subagent 작업이 선택된 row의 detail line에만
`tree ◇1 ▸1 +2`처럼 표시됩니다. `workload`를 `columns`에 추가하면 항상 보이는
`TREE` 컬럼으로 렌더링합니다. `◇`는 subagent, `▸`는 shell, `+`는 기타 표시 대상
process를 의미합니다.

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

런타임 정렬 키는 위 preset과 대응하며, 선택한 preset을 `[watch].sort`에 다시
저장합니다. `--sort` flag는 런타임 정렬 키를 누르기 전까지 현재 실행에만 적용되는
override입니다. 기본값은 tmux session으로 묶고, 각 group 안에서 가장 최근
activity가 있는 agent를 위로 올립니다. `activity`와 `act`는 `latest` alias로
계속 동작합니다.

## Detail Row

```toml
[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

사용 가능한 변수: `pane`, `kind`, `state`, `model`, `ctx`, `cost`, `activity`,
`workload`, `last_prompt`, `last_response`, `last_notification`, `cwd`.

표시 가능한 workload가 있으면 선택 row의 detail line은 template보다 먼저
session/name 컬럼에 `tree ...`를 보여줍니다.

긴 detail 내용은 table에 맞게 잘립니다. 더 많은 맥락이 필요하면 preview를
사용하세요.
