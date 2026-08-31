# CLI dashboard

`muxa dashboard`는 Work-card 기반 TUI console입니다. Web dashboard와 같은
canonical `WorkSnapshot`을 사용하므로 로컬 Work 단계, 외부 issue 상태, Run 상태,
Agent 상태, Attention/Error 신호를 구분합니다. 일반 tmux session/window는 card로
만들지 않고 unlinked execution 수만 note로 알리며 topology 확인은 `muxa watch`로
연결합니다.

작고 빠른 picker/table이 필요하면 `muxa watch`를 쓰고, card, inspector, live
Run capture, prompt composer, Work 일괄 action, ACT/WACT total이 필요한
운영 화면은 `muxa dashboard`를 씁니다.
tracked tmux agent pane 안에서 실행하면 현재 agent의 collaboration room
console 역할도 함께 수행합니다.

## 실행

조회만 할 때는 어디서든 `muxa dashboard`를 실행할 수 있습니다. 협업할 때는
명령을 직접 입력하지 말고, 메시지를 보낼 agent pane을 선택한 뒤 `prefix+D`를
누릅니다. 이 popup 단축키는 `muxa init`이 설치합니다.

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
| `Tab`, `[` / `]` | 선택 Work card 안의 execution target 전환. |
| `PageUp` / `PageDown` | live capture history scroll. |
| `G` / `End` | capture를 최신 output으로 이동. |
| `f` | capture fullscreen toggle. |
| `n` | 진단 note가 있을 때 notes popup 열기. |
| `Enter` | 선택 Work inspector toggle. |
| `p` | 선택 pane 또는 muxa PTY session에 prompt composer 열기. |
| `P` | 하나의 prompt를 선택 Work의 모든 live agent에 전송. |
| `m` | 선택한 same-room agent에게 구조화 요청 작성. 초안 어디서든 `/`를 눌러 현재 커서 위치에 등록한 메시지 스킬 삽입. |
| `b` | request를 claim하지 않고 incoming/sent collaboration mailbox 열기. |
| `i` | pending collaboration request를 claim하고 incoming mailbox 열기. |
| `c` | 선택 Work의 최신 prompt 복사. |
| `R` | 확인 후 선택 pane 또는 PTY session에 Ctrl-C 전송. |
| `A` | 확인 후 선택 Work의 모든 live agent에 Ctrl-C 전송. |
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

Card는 논리 `{workspace_id, work_id}` 단위로 묶입니다. 각 card는 다음을 보여줍니다:

- 로컬 Work 단계와 Attention/Blocked/Error 신호
- 외부 provider/display key/status 또는 `local work`
- Run, agent, pane 수와 dominant Agent runtime 상태
- 최신 activity age와 foreground time
- 선택한 `--since` window의 ACT/WACT
- agent가 제공하는 model/context/cost hint
- 최신 prompt 또는 notification preview

Inspector는 선택 Work의 Run/Agent 정보와 선택 execution target의 live capture를
보여줍니다. backend가 capture를 지원하지 않으면 TUI는 실패하지 않고
unavailable 상태로 표시합니다.

multi-agent Work에서는 강조된 target이 `p`, `R`, `K`, `o`, capture의 대상입니다.
`P`, `A`는 cursor와 무관하게 Work의 모든 live agent를 대상으로 합니다. 확인창은
exact-target action인지 Work-wide action인지 실행 전에 명시합니다.

## Collaboration room

평소 사용 순서는 세 단계입니다.

1. 어디서든 `prefix+D`를 누릅니다.
2. `Tab`으로 같은 window의 agent를 고릅니다.
3. `m`을 눌러 메시지를 보냅니다.

window 하나가 room 하나이고, dashboard는 **operator console**로서 보냅니다 —
발신자는 dashboard를 연 pane에 들어 있던 agent가 아니라 키보드 앞의 사람입니다.
그래서 그 pane의 agent도 다른 행과 똑같은 수신 대상이고, 일반 shell pane에서
열어도 agent pane에서 연 것과 똑같이 메시지를 보낼 수 있습니다.

console에는 자기 pane이 없으므로 응답은 발신자에게 되돌아오지 않고 **수신
agent의 mailbox**에 request와 함께 남습니다. `b`는 커서가 놓인 card의 mailbox를
보여주며 `incoming`은 그 agent의 것, `sent`는 console이 모든 대상에게 보낸
기록입니다. claim(`i`)과 응답(`e`)은 수신자의 행위이므로 그 agent를 대행합니다.

Header와 inspector에는 현재 room, 호출 agent의 alias, room participant의 role,
읽지 않은 request/reply 수가 표시됩니다. `Tab`, `[`, `]`로 peer pane을 선택한
뒤 `m`을 누르면 그 agent session에 고정된 durable request를 작성합니다.
message composer에서는 다음 키를 사용합니다.

- `Tab`: `question`, `review`, `task`, `notice` 전환
- `Ctrl-E`: 명시적인 `read-only` / `execute` 작업 계약 전환
- `Enter`: 전송, `Esc`: dashboard로 복귀

`b`는 claim 없이 incoming/sent 이력을 보여줍니다. Mailbox 안에서는 `Tab`으로
mailbox를 바꾸고 화살표로 request를 선택합니다. `i`는 pending incoming 작업을
원자적으로 claim하고, `e`는 claimed request에 응답하며, `x`는 아직 queued인
발신 request의 취소 확인창을 엽니다. reply composer의 `Tab`은 `completed`,
`blocked`, `declined`, `failed`를 전환합니다.

이 TUI mailbox는 계속 선택 agent 범위입니다. window/session 전체를 read-only로
보려면 `muxa watch`의 해당 topology 행에서 `M`을 누르고, room을 넘나드는 node-edge
graph와 시간순 sequence를 탐색하려면 Web dashboard의 collaboration panel을 사용하세요.

window에 agent가 하나도 없다면 같은 window의 새 pane에서 하나 실행합니다 —
`muxa watch`와 달리 dashboard의 사정권은 여전히 room이라
`[collaboration].scope = "host"`를 따라 다른 window로 넘어가지 않습니다. 협업
불가 안내가 나오면 `[collaboration].enabled` 설정과 muxad 재시작 여부를
확인하세요. 내부적으로 muxad가 사람이 호출한 pane을 provenance로 남기고
CLI/MCP와 같은 same-window 경계를 유지합니다.

## ACT/WACT

Header와 card의 ACT/WACT는 `muxa stats`와 같은 last-touch attribution 코드
경로를 사용합니다. 따라서 각 session의 `WACT`는 항상 해당 `ACT`의 subset으로
유지됩니다. activity ledger를 읽을 수 없어도 dashboard 자체는 열리고 note로
상태를 표시합니다. `n`을 누르면 note 내용을 볼 수 있습니다. 1초 간격 automatic
refresh는 action/mailbox hint를 가리지 않도록 조용히 처리하며, 사용자가 `r`을
누른 explicit refresh만 `refreshing` / `refreshed` 상태를 표시합니다.
