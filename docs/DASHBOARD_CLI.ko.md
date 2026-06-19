# CLI dashboard

`muxa dashboard`는 session-card 기반 TUI console입니다. 여러 agent session을
tmux에 먼저 attach하지 않고 한 화면에서 확인하고 조작하는 용도입니다.

작고 빠른 picker/table이 필요하면 `muxa watch`를 쓰고, card, inspector, live
terminal capture, prompt composer, session 단위 ACT/WACT total이 필요한
운영 화면은 `muxa dashboard`를 씁니다.

## 실행

```bash
muxa dashboard
muxa dashboard --since today
muxa dashboard --sort attention
muxa dashboard --include-paneless
```

`--since`는 `muxa stats --since`와 같은 time window를 받습니다. 예:
`today`, `24h`, `7d`, local date, RFC3339 timestamp, `all`.

## 키

| Key | Action |
| --- | --- |
| `↑` / `↓` / `←` / `→`, `h` / `j` / `k` / `l` | card 선택 이동. |
| `Tab`, `[` / `]` | 선택 session card 안의 action target 전환. |
| `PageUp` / `PageDown` | live capture history scroll. |
| `G` / `End` | capture를 최신 output으로 이동. |
| `f` | capture fullscreen toggle. |
| `n` | 진단 note가 있을 때 notes popup 열기. |
| `Enter` | 선택 session inspector toggle. |
| `p` | 선택 pane 또는 muxa PTY session에 prompt composer 열기. |
| `c` | 선택 session의 최신 prompt 복사. |
| `R` | 확인 후 선택 pane 또는 PTY session에 Ctrl-C 전송. |
| `K` | 확인 후 선택 pane 또는 PTY session 종료. |
| `o` | 선택 pane/session을 명시적으로 열기. |
| `r` | 즉시 refresh. |
| `?` | 도움말. |
| `q` / `Esc` | 종료. |

`o`만 dashboard를 떠나 attach/open을 수행합니다. prompt 전송, Ctrl-C, 복사,
live capture는 dashboard 안에서 처리합니다.

Pane prompt/abort/terminate action은 현재 tmux pane control이 필요합니다.
zellij card는 계속 표시되고 `o`로 focus할 수 있지만, zellij-safe input
경로가 생기기 전까지 pane write/destructive action은 명시적인 hint와 함께
비활성화됩니다.

## Card와 inspector

Card는 tmux/zellij session, muxa-owned PTY session, detached agent session
단위로 묶입니다. 각 card는 다음을 보여줍니다:

- host type, agent 수, pane 수
- 현재 dominant state
- 최신 activity age와 foreground time
- 선택한 `--since` window의 ACT/WACT
- agent가 제공하는 model/context/cost hint
- 최신 prompt 또는 notification preview

Inspector는 선택 card의 세부 정보와 primary pane 또는 PTY session의 live
capture를 보여줍니다. backend가 capture를 지원하지 않으면 TUI는 실패하지 않고
unavailable 상태로 표시합니다.

multi-pane card에서는 강조된 action target이 `p`, `R`, `K`, `o`, capture의
대상입니다. `Tab`, `[`, `]`로 dashboard 안에서 target을 바꿀 수 있고,
destructive action 확인창은 실행 전 정확한 pane 또는 PTY session을 표시합니다.

## ACT/WACT

Header와 card의 ACT/WACT는 `muxa stats`와 같은 last-touch attribution 코드
경로를 사용합니다. 따라서 각 session의 `WACT`는 항상 해당 `ACT`의 subset으로
유지됩니다. activity ledger를 읽을 수 없어도 dashboard 자체는 열리고 note로
상태를 표시합니다. `n`을 누르면 note 내용을 볼 수 있습니다.
