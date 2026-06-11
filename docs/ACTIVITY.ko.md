# Activity Ledger

`muxa`의 duration 데이터는 `activity.ndjson`에 저장됩니다. 이 파일은
`muxa stats`, `muxa report`, raw 조회 명령인 `muxa activity`의 기준입니다.

## 빠른 조회

```bash
muxa stats --since today
muxa stats --since yesterday --group-by session
muxa stats --since week --group-by project
muxa stats --since month --exclude-session 'monitor*'
muxa report --since week
muxa report --since last-month --exclude-pane '%42'
muxa timeline --since today --session main
muxa activity --since today --type human
muxa activity --since today --type agent --format json
```

`--since` 는 다음 값을 받습니다:

- `today`: 로컬 날짜 기준 오늘 00:00부터 현재까지.
- `yesterday`: 로컬 날짜 기준 어제 00:00부터 오늘 00:00 전까지.
- `week`: 현재 시각 기준 최근 7일.
- `month`: 현재 시각 기준 최근 30일.
- `last-week`, `"last week"`: 로컬 날짜 기준 저번주 월요일 00:00부터 이번주 월요일 00:00 전까지.
- `last-month`, `"last month"`: 로컬 날짜 기준 이전 달 1일 00:00부터 이번 달 1일 00:00 전까지.
- `24h`, `7d`, `4w`, `30d`: rolling duration.
- `YYYY-MM-DD`: 로컬 날짜 기준 해당 하루.
- RFC3339 timestamp: 해당 시각 이후 전체.
- `all`: 보관 중인 모든 ledger entry.

## 정렬

`muxa stats`의 행은 기본적으로 prompt 수 기준으로 정렬됩니다. `--sort`로 임의의
컬럼을 기준으로 정렬하고 `--reverse`로 방향을 뒤집을 수 있습니다. 숫자 컬럼은
기본 내림차순(큰 값 먼저), `name`은 기본 오름차순입니다.

```bash
muxa stats --since today --sort wait              # 대기 시간이 긴 순
muxa stats --since today --sort work --reverse    # 작업 시간이 짧은 순
muxa stats --since today --group-by session --sort name
```

계속 켜둔 monitoring scope는 `--exclude-pane`, `--exclude-session`으로 rows와
totals에서 모두 제외할 수 있습니다. 값은 반복하거나 comma-separated로 줄 수
있고, 패턴은 case-sensitive이며 `*`, `?` wildcard를 지원합니다. 예:
`--exclude-session 'monitor*'`.

`--sort` 는 다음 값을 받습니다: `prompts`, `work`, `wait`, `err`, `tmux`,
`human`, `think`, `active`, `block`, `tok`, `words`, `sess`, `agents`, `last`,
`name`.

`ACTIVE`(`ACT` column, JSON의 `active` / `active_secs`)는 실제로 몰입해
다룬 human time 추정값입니다. submitted prompt 주변 window, tmux input tick
(keypress/scroll), agent가 human 답변을 기다리는 동안의 thinking time을
합칩니다. 다만 prompt/input padding은 같은 session/pane의 `HUMAN` presence
(tmux foreground, prompt input, attach 등)와 겹치는 구간으로 자릅니다. 그래서
padding 때문에 한 session의 `ACT`가 관측된 foreground/interaction 시간을
넘어가지 않습니다. 여러 session의 window가 겹치면 가장 최근 touch session에
귀속해서 중복 집계하지 않습니다.

표 마지막에는 `TOTAL` 푸터 행이 붙습니다. 모든 그룹의 총합을 담으며,
`--limit`로 위쪽 행이 잘려도 전체 데이터를 반영합니다.

## Timeline

`muxa timeline`은 같은 duration 데이터를 interactive TUI로 표시합니다.
overview는 기본적으로 session별로 묶고, 각 session 아래에 agent/human/tmux
foreground lane을 보여줍니다. focus view는 선택한 lane을 timestamped interval
목록처럼 따라갑니다.

```bash
muxa timeline --since today
muxa timeline --since today --session main
muxa timeline --since today --exclude-session 'monitor*'
muxa timeline --since 24h --agent codex
muxa timeline --since today --group-by kind --sort waiting
muxa timeline --view heatmap --since 12w
muxa timeline --day 2026-06-06
muxa timeline --since today --format json
```

agent transition row는 진입한 상태(`to`)가 아니라 떠난 상태(`from`)를
그립니다. 예를 들어 `working -> waiting_input` row는 `state_entered_at`부터
transition timestamp까지의 `working` 구간입니다.

TUI keybinding, grouping mode, dashboard 동작, JSON export 설명은
[docs/TIMELINE.ko.md](TIMELINE.ko.md)에 따로 정리했습니다.

## Ledger 타입

`muxa activity --type ...` 은 raw ledger 를 타입별로 필터링합니다:

| Type | 의미 |
| ---- | ---- |
| `agent` | working, waiting, error 같은 agent 상태 transition interval. |
| `tmux` | interactive tmux client 에서 관측된 닫힌 foreground interval. |
| `human` | muxa 자체가 기록한 human interaction interval. |

기존 호환성을 위해 `--type state`는 숨겨진 alias 로 남아 있으며
`--type agent`와 같습니다.

## Stats 컬럼

| Column | 기준 |
| ------ | ---- |
| `WORK` | agent working 상태에 머문 시간. |
| `WAIT` | input/choice 를 기다린 시간. |
| `ERR` | error 상태에 머문 시간. agent 가 quota/rate-limit 류 block 을 error 로 보고하면 여기에 포함됩니다. |
| `TMUX` | interactive tmux client 에서 해당 session 이 foreground였던 시간. |
| `HUMAN` | tmux foreground 시간과 muxa human interaction interval 의 union. |
| `THINK` | attention 상태와 human presence 가 겹친 시간. |
| `BLOCK` | Waiting/Error attention 상태로 진입한 횟수. |

`THINK`는 `HUMAN`보다 의도적으로 좁습니다. agent 가 attention 을 필요로 하는
상태(`WaitingInput`, `WaitingChoice`, `Error`)이고, 동시에 human presence 가
있을 때만 계산합니다. 여기서 human presence 는 tmux foreground, muxa prompt
input, tmux attach 입니다. 단순히 `muxa watch`를 열어둔 시간은 `HUMAN`에는
들어가지만 `THINK`에는 들어가지 않습니다. 대시보드를 열어둔 시간이 실제
사고 시간이라고 단정하기 어렵기 때문입니다.

## Retention

`activity.ndjson`는 append-only 파일이며 `[activity]` 설정에 따라 보관됩니다.
기존 `session-activity.json` 누적값은 activity ledger 에 foreground interval 이
생기기 전까지 legacy fallback 으로만 사용됩니다.
