# Timeline

`muxa timeline`은 agent session이 work, waiting, error, human interaction,
tmux foreground 사이를 어떻게 오갔는지 보여주는 시각화입니다. `muxa stats`,
`muxa report`와 같은 duration 데이터를 사용합니다.

## 빠른 시작

```bash
muxa timeline --since today
muxa timeline --since today --session main
muxa timeline --since 24h --agent codex
muxa timeline --since today --group-by kind
muxa timeline --since today --format json
```

기본 overview는 tmux session별로 묶입니다. 각 session group 안에는 다음 lane이
나올 수 있습니다:

| Lane | 의미 |
| --- | --- |
| Agent | working, waiting, error, starting, idle 같은 agent 상태 구간. |
| Human | muxa가 기록한 prompt input, tmux attach 같은 human interaction 구간. |
| tmux | interactive tmux client에서 해당 session이 foreground였던 시간. |

## 옵션

| Option | 값 |
| --- | --- |
| `--since` | `today`, `yesterday`, `week`, `24h`/`7d`/`4w` 같은 rolling duration, RFC3339 timestamp, `all`. |
| `--session` | tmux session 이름, tmux session id, pane id. |
| `--agent` | `codex`, `claude-code`, `gemini-cli`, `opencode`, `unknown`. |
| `--group-by` | 기본값 `session`, 또는 `kind`, `flat`. TUI 전용. |
| `--format` | 기본값 `tui`, 또는 `json`. |
| `--theme` | 다른 muxa TUI와 같은 일회성 theme override. |

## TUI 키

| Key | 동작 |
| --- | --- |
| `j` / `k`, arrows | overview에서는 lane 선택, focus view에서는 interval 선택. |
| `h` / `l`, left/right | 보이는 시간 창을 좌우로 이동. |
| `+` / `-` | zoom in / zoom out. |
| `0` | 최신 view로 이동. |
| `f` | 선택한 `--since` 전체 범위에 맞춤. |
| `g` | grouping 순환: `session` -> `kind` -> `flat`. |
| `tab` / `shift-tab` | 선택한 lane의 interval 순환. |
| `enter` / `o` | overview와 focus view 전환. |
| `r` | activity와 live agent 상태 reload. |
| `?` | help 표시. |
| `q` / `Esc` | 종료. |

선택한 범위가 충분히 길면 TUI는 최신 6시간 view에서 시작합니다. 그래서 처음부터
`h`로 과거 방향 이동이 가능합니다. 이미 최신 끝에 있을 때 `l`을 누르면 footer에
더 이동할 창이 없다는 상태가 표시됩니다.

## 데이터 의미

닫힌 interval은 `activity.ndjson`에서 옵니다. 현재 열려 있는 agent 상태는 daemon의
live snapshot에서 옵니다. 현재 열려 있는 tmux foreground 구간은 session activity
tracking이 켜져 있을 때 `session-activity.json`에서 옵니다.

Agent transition row는 진입한 상태(`to`)가 아니라 떠난 상태(`from`)를 그립니다.
예를 들어 `working -> waiting_input` row는 `state_entered_at`부터 transition
timestamp까지의 `working` span입니다. 이렇게 해야 `muxa stats`의 duration 계산과
일관됩니다.

## Dashboard

Dashboard timeline도 같은 `/api/timeline` 문서를 사용하며 기본적으로 session별로
lane을 묶습니다. 브라우저에서 계속 켜둘 화면이 필요하면 dashboard를 쓰고, keyboard
navigation, focus view, terminal-native JSON export가 필요하면 `muxa timeline`을
쓰는 것이 좋습니다.
