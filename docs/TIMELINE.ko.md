# Timeline

`muxa timeline`은 agent session이 work, waiting, error, human interaction,
tmux foreground 사이를 어떻게 오갔는지 보여주는 시각화입니다. `muxa stats`,
`muxa report`와 같은 duration 데이터를 사용합니다.

## 빠른 시작

```bash
muxa timeline --since today
muxa timeline --since today --session main
muxa timeline --since today --exclude-session 'monitor*'
muxa timeline --since 24h --agent codex
muxa timeline --since today --group-by kind
muxa timeline --since today --sort waiting
muxa timeline --view heatmap --since 12w
muxa timeline --day 2026-06-06
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
| `--since` | `today`, `yesterday`, 최근 7일 rolling window인 `week`, 최근 30일 rolling window인 `month`, 저번주 월요일-일요일 구간인 `last-week` / `"last week"`, 이전 달력 월인 `last-month` / `"last month"`, `24h`/`7d`/`4w` 같은 rolling duration, `2026-06-06` 같은 local date, RFC3339 timestamp, `all`. |
| `--day` | 특정 local calendar day shortcut. 예: `--day 2026-06-06`. |
| `--session` | tmux session 이름, tmux session id, pane id. |
| `--exclude-pane` | case-sensitive glob에 맞는 pane id를 제외합니다. 반복하거나 comma-separated로 줄 수 있습니다. |
| `--exclude-session` | case-sensitive glob에 맞는 tmux session name/id를 제외합니다. 반복하거나 comma-separated로 줄 수 있습니다. |
| `--agent` | `codex`, `claude-code`, `gemini-cli`, `opencode`, `unknown`. |
| `--view` | 기본값 `timeline`, 또는 terminal contribution-map summary인 `heatmap`. |
| `--group-by` | 기본값 `session`, 또는 `kind`, `flat`. TUI 전용. |
| `--sort` | 기본값 `latest`, 또는 `name`, `duration`, `working`, `waiting`, `error`, `human`, `foreground`. `dur`, `work`, `wait`, `err`, `tmux` alias도 지원. |
| `--format` | 기본값 `tui`, 또는 `json`. |
| `--theme` | 다른 muxa TUI와 같은 일회성 theme override. |

## Heatmap View

`muxa timeline --view heatmap --since 12w`는 terminal에 compact daily
activity map을 출력합니다. 각 cell은 local calendar day이고, intensity는 agent
work, waiting, error, human interaction, tmux foreground 시간을 기준으로 합니다.
요일 행은 `--since last-week`와 맞게 ISO 스타일 Monday-first 순서를 씁니다.
grid 아래에는 가장 바쁜 날짜가 표시되고, 하루 view에서는 해당 날짜의 top session도
같이 표시됩니다.

## TUI 키

| Key | 동작 |
| --- | --- |
| `j` / `k`, arrows | overview에서는 lane 선택, focus view에서는 interval 선택. |
| `h` / `l`, left/right | 보이는 시간 창을 좌우로 이동. |
| `+` / `-` | zoom in / zoom out. |
| `0` | 최신 view로 이동. |
| `f` | 선택한 `--since` 전체 범위에 맞춤. |
| `g` | grouping 순환: `session` -> `kind` -> `flat`. |
| `s` | sorting 순환: `latest` -> `duration` -> `working` -> `waiting` -> `error` -> `human` -> `foreground` -> `name`. |
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
lane을 묶습니다. lane graph 위에는 daily contribution-map style heatmap이
표시되고, 날짜를 클릭하면 해당 calendar day로 drilldown됩니다. 브라우저에서 계속
켜둘 화면이 필요하면 dashboard를 쓰고, keyboard navigation, focus view, terminal
heatmap, terminal-native JSON export, scope exclusion이 필요하면
`muxa timeline`을 쓰는 것이 좋습니다.
