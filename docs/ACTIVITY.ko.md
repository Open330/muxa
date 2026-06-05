# Activity Ledger

`muxa`의 duration 데이터는 `activity.ndjson`에 저장됩니다. 이 파일은
`muxa stats`, `muxa report`, raw 조회 명령인 `muxa activity`의 기준입니다.

## 빠른 조회

```bash
muxa stats --since today
muxa stats --since yesterday --group-by session
muxa stats --since week --group-by project
muxa report --since week
muxa timeline --since today --session main
muxa activity --since today --type human
muxa activity --since today --type agent --format json
```

`--since` 는 다음 값을 받습니다:

- `today`: 로컬 날짜 기준 오늘 00:00부터 현재까지.
- `yesterday`: 로컬 날짜 기준 어제 00:00부터 오늘 00:00 전까지.
- `week`: 현재 시각 기준 최근 7일.
- `24h`, `7d`, `4w`: rolling duration.
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

`--sort` 는 다음 값을 받습니다: `prompts`, `work`, `wait`, `err`, `tmux`,
`human`, `think`, `block`, `tok`, `words`, `sess`, `agents`, `last`, `name`.

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
muxa timeline --since 24h --agent codex
muxa timeline --since today --group-by kind --sort waiting
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
