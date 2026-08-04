# Agent collaboration

`muxa`는 tmux window를 협업 room으로 보고, 그 안의 top-level agent가 durable
request/reply 메시지를 주고받게 할 수 있습니다. tmux는 위치와 범위를 표현하고,
실제 메시지는 `muxad`의 owner-only Unix socket과 local mailbox를 통과합니다.

## 활성화

`~/.config/muxa/config.toml`에 다음을 추가하고 daemon을 재시작합니다.

```toml
[collaboration]
enabled = true
wake = "idle_only"
```

각 agent에 `muxa mcp`가 등록돼 있어야 합니다. Claude Code 예시는 다음과
같습니다.

```bash
claude mcp add muxa -- muxa mcp
```

MCP process는 agent와 같은 pane의 `TMUX_PANE`/`TMUX` 환경을 상속받습니다.
이 값과 daemon의 live agent/pane registry를 대조해 발신자를 결정하므로 tool
argument로 발신 pane을 임의 지정하지 않습니다.

## Room과 주소

- 같은 `(tmux socket, stable window id)`를 공유하는 agent가 한 room입니다.
- agent가 정확히 둘이면 상대를 `peer`로 지정할 수 있습니다.
- 셋 이상이면 `%12` 또는 `pane:%12`처럼 pane을 명시합니다.
- identity를 등록한 agent는 `@reviewer` 또는 `role:rust`처럼 지정할 수 있습니다.
- 다른 window의 pane은 명시해도 거부됩니다.
- 요청은 pane뿐 아니라 현재 agent session에도 고정됩니다. pane을 새 agent가
  재사용해도 이전 요청을 받지 않습니다.

확인:

```bash
muxa peers
muxa peers --json
```

## Agent identity

pane이 셋 이상인 room에서는 각 agent가 의미 있는 alias와 role을 등록할 수
있습니다. identity는 pane이 아니라 현재 agent session에 고정되므로 pane을 새
agent가 재사용해도 이전 이름이나 역할을 상속하지 않습니다.

```bash
muxa identity set --alias reviewer --role review --role rust
muxa identity show

muxa msg send @reviewer "auth 변경을 검토해 주세요" --kind review
muxa msg send role:rust "이 lifetime 오류의 원인을 찾아 주세요"

muxa identity clear
```

alias는 live peer 사이에서 room-local unique이며 32자 이하 slug입니다. role은
여러 agent가 공유할 수 있지만 `role:<name>`과 일치하는 peer가 둘 이상이면
오배송을 피하기 위해 요청을 거부합니다. 이 경우 `@alias`나 pane을 명시하세요.

## CLI

```bash
# 질문/리뷰는 read-only가 기본
muxa msg send peer "auth 변경의 race 가능성을 검토해 주세요" --kind review

# 수정 권한과 advisory path scope를 명시한 작업
muxa msg send pane:%18 "테스트를 보강해 주세요" \
  --kind task --execute --path 'crates/auth/**'

# 수신 agent
muxa msg inbox
muxa msg reply req_... "검토 완료: ..." --status completed

# 발신 agent
muxa msg wait req_... --timeout-secs 300

# claim하지 않고 request/reply lifecycle 확인
muxa msg list --mailbox sent
muxa msg list --mailbox incoming --json

# 아직 상대가 claim하지 않은 요청 취소
muxa msg cancel req_...
```

tracked agent pane 안에서 `muxa dashboard`를 실행하면 같은 lifecycle을 TUI로
사용할 수 있습니다. `m`은 선택한 room peer에게 요청을 보내고, `b`는 claim 없는
mailbox 이력을 열며, `i`는 inbox를 claim합니다. Mailbox에서는 `e`로 응답하고
`x`로 아직 queued인 발신 요청을 취소할 수 있습니다.

## MCP tools

| Tool | 역할 |
| --- | --- |
| `muxa_room_context` | self, same-window peers, unread count 조회 |
| `muxa_set_identity` | 현재 agent session의 room-local alias/roles 교체 |
| `muxa_send_message` | durable request 생성 |
| `muxa_inbox` | 현재 agent session의 요청 claim/read |
| `muxa_list_messages` | incoming/sent/all request 상태 조회(미claim) |
| `muxa_reply` | completed/blocked/declined/failed 구조화 응답 |
| `muxa_wait_reply` | 요청의 terminal reply 대기 |
| `muxa_cancel_message` | 아직 queued인 발신 요청 취소 |

일반적인 흐름:

```text
Agent A: muxa_room_context
Agent A: muxa_set_identity(alias="implementer", roles=["rust"])
Agent A: muxa_send_message(target="peer", kind="review", ...)
Agent A: muxa_wait_reply(request_id="req_...", timeout_secs=300)

Agent B: idle 상태에서 짧은 mailbox wake prompt 수신
Agent B: muxa_inbox
Agent B: muxa_reply(request_id="req_...", status="completed", ...)

Agent A: wait 중이 아니고 Idle이면 짧은 reply wake prompt 수신
Agent A: muxa_wait_reply(request_id="req_...")
```

## 전달 안전성

메시지와 응답 본문은 `$XDG_DATA_HOME/muxa/collaboration.json`에 먼저
저장됩니다. `idle_only` wake는 새 요청뿐 아니라 아직 발신자가 읽지 않은 terminal
응답에도 적용됩니다. 다음 조건을 모두 만족할 때만 짧은 notification prompt를
pane에 넣으며 본문은 terminal에 주입하지 않습니다.

- hook 기반의 실제 agent session
- state가 `Idle`
- target pane/session이 요청 생성 시점과 동일
- backend가 targeted input을 지원

`Working`, `WaitingInput`, `WaitingChoice`, `Error` 상태에는 입력하지 않습니다.
화면 감지로 생성된 synthetic agent도 자동 wake 대상이 아닙니다. `wake =
"never"`로 설정하면 mailbox는 유지하면서 모든 입력 주입을 끌 수 있습니다.

요청을 `muxa_inbox`로 읽는 순간 원자적으로 claim합니다. wake prompt가 중복돼도
동일 request id의 작업을 새 요청으로 만들지 않습니다.

발신자가 `muxa_wait_reply`/`muxa msg wait`로 terminal 응답을 읽으면 이를 확인한
것으로 기록해 이후 reply wake를 생략합니다. 먼저 reply wake가 전달된 경우에도
본문은 mailbox에 그대로 남아 있어 `wait` 또는 `list`로 조회할 수 있습니다.

## Request lifecycle

`muxa msg list`와 `muxa_list_messages`는 메시지를 claim하지 않고 현재 agent
session의 incoming/sent/all 이력을 보여줍니다. `room_context`는 새 incoming
request 수와 아직 확인하지 않은 reply 수를 각각 반환합니다.

발신자는 요청이 `queued`인 동안만 취소할 수 있습니다. 수신자가 inbox를 읽어
`claimed`가 된 뒤에는 이미 작업이 시작됐을 수 있으므로 취소를 거부합니다.

## 작업공간 규칙

`question`과 `review`는 read-only 계약이 기본입니다. shared worktree를 수정하게
하려면 `work_mode = execute`와 path scope를 명시하세요. 현재 path scope는 agent
간 협업 계약이며 OS sandbox는 아닙니다. 서로 다른 agent가 동시에 같은 파일을
수정해야 하는 작업에는 별도 git worktree를 권장합니다.
