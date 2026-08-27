# Agent collaboration

`muxa`는 tmux window를 협업 room으로 보고, 그 안의 top-level agent가 durable
request/reply 메시지를 주고받게 할 수 있습니다. tmux는 위치와 범위를 표현하고,
실제 메시지는 `muxad`의 owner-only Unix socket과 local mailbox를 통과합니다.

## 이것만 기억하세요

- tmux window 하나가 협업 room 하나입니다. Muxa managed model에서 이 window는
  현재 Run의 협업 경계이며, 지속되는 Work나 외부 이슈의 identity는 아닙니다.
- MCP나 `muxa msg`로 말하는 **agent**는 자기 pane을 대행하는 발신자입니다. 응답은
  그 pane으로 돌아가 agent를 깨웁니다.
- `muxa watch`는 agent가 아니라 **operator console**입니다. watch를 연 pane에 무엇이
  들어 있든, 발신자는 키보드 앞의 사람입니다.
- agent를 선택하고 `m`으로 보내며 `M`으로 mailbox를 엽니다(`b`는 alias로
  유지됩니다).

즉 agent 간 협업은 같은 window에 agent 둘을 두고 MCP로 주고받는 흐름이고, 사람의
흐름은 그와 별개로 준비가 필요 없습니다. 어디서든 `prefix+s`를 누르고, 행을
가리키고, `m`을 누르면 됩니다. Dashboard는 선택 사항입니다.

### console

`muxa watch`는 고정된 identity를 가지며 자기 pane이 없는 `console`로 보냅니다.
알아둘 결과가 셋 있습니다.

- **watch를 띄운 그 pane을 포함해 모든 행이 대상입니다.** 발신자는 그 pane의
  agent가 아니라 사람이므로 자기 자신에게 보내는 문제가 애초에 없습니다.
- **일반 shell에서도 동작합니다.** 이제 watch를 연 pane이 tracked agent를 담고
  있어야 할 이유가 없습니다.
- **응답은 되돌아오지 않습니다.** console에는 깨울 pane이 없으므로 응답은 수신
  agent의 mailbox에 request와 함께 남습니다. 그 행에 커서를 두고 `M`을 누르면
  읽을 수 있고, `incoming` 탭이 선택한 agent의 mailbox, `sent`가 console이 모든
  대상에게 보낸 기록입니다. claim(`i`)과 응답(`e`)도 수신자의 행위이므로 선택한
  agent를 대행합니다.

console은 watch를 연 window의 room을 빌려 쓰므로 window scope의 peer 선택도 눈앞의
agent로 그대로 해석되지만, identity는 함께 바뀌지 않습니다 — 사람 하나에 `sent`
스레드 하나입니다.

여기서 `from`은 IPC를 실제 호출한 프로세스라는 뜻이 아니라, 요청이 누구의
mailbox 권한으로 생성됐고 응답이 어디로 돌아갈지를 나타내는 represented agent
identity입니다. watch에서 사람이 보낸 요청도 예전에는 이 agent가 직접 보낸 것처럼
보였지만, 이제 request의 `provenance`와 wake 문구가 `watch/MCP/CLI/dashboard`, OS가
확인한 PID/UID, process environment 또는 ancestry로 관찰한 pane과 evidence 종류,
주장한 origin과의 일치 여부를 따로 표시합니다. console 요청도 사람이 어느 pane에서
걸었는지를 함께 남기므로 audit은 그대로 유지되고, 수신자는 wake 문구에서
`from console via muxa watch (caller %N, pid …)`를 봅니다.

## 활성화

협업은 명시적으로 켜야 합니다. 요청이 도착하면 상대 pane에 짧은 prompt를 넣어
깨우기 때문입니다. `muxa init`이 다른 항목과 함께 이 권한을 묻고, `standard`
preset에 포함돼 있습니다.

```bash
muxa init --component collaboration
```

아래 블록을 `config.toml`에 씁니다. 해제는 `muxa init --component collaboration
--uninstall`입니다. 직접 편집해도 동일합니다.

```toml
[collaboration]
enabled = true
wake = "idle_only"
# 선택 사항: 기본값 "notice", 원문 직접 전달은 "full"
wake_payload = "notice"
```

이미 있는 `wake` 값은 덮어쓰지 않습니다. `never`는 "mailbox는 쓰되 내 pane은
건드리지 말라"는 의도적 선택이므로 `muxa init`을 다시 실행해도 유지됩니다.

설정 후 daemon을 재시작합니다.

각 agent에 `muxa mcp`가 등록돼 있어야 합니다. Claude Code와 Codex에는 다음처럼
등록합니다.

```bash
claude mcp add --scope user muxa -- muxa mcp
codex mcp add muxa -- muxa mcp
```

Codex는 허용 목록에 지정한 환경 변수만 stdio MCP process로 전달합니다.
`~/.codex/config.toml`에 생성된 `[mcp_servers.muxa]` table 아래에 다음 줄을
추가하세요.

```toml
env_vars = ["RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE", "MUXA_SOCKET"]
```

이미 실행 중인 agent는 등록된 MCP 목록을 다시 읽도록 종료 후 재실행합니다.
MCP가 연결되면 muxa는 초기 지침에서 같은 window의 agent를 reviewer나 좁은 범위의
subagent로 활용할 수 있음을 알립니다. agent는 필요할 때
`muxa_collaboration_guide`로 같은 지침을 다시 조회할 수 있습니다.

`muxa doctor`가 pane을 synthetic agent로 표시하면 그 agent는 아직 안정적인 session
identity가 없으므로 room participant나 요청 대상에 포함되지 않습니다. agent에서 새
prompt를 한 번 제출해 hook event를 발생시키고, 계속 synthetic이면 agent를 재시작한
뒤 다시 확인하세요. 이는 synthetic session에 요청이 고정된 채 실제 session으로
전환되어 claim할 수 없게 되는 상황을 방지합니다.

기존 `prefix+s` watch 단축키가 있다면 업그레이드 후 추가 단축키가 필요 없습니다.

Claude MCP process는 agent와 같은 pane host의 native 환경 변수를 상속하고,
Codex는 위 `env_vars` 허용 목록을 통해 전달합니다. 기존 default-endpoint Codex
등록에는 muxa가 active backend의 process ancestry를 따라 pane을 복구하는 fallback도 적용합니다.
해석된 값과 daemon의 live agent/pane registry를 대조해 발신자를 결정하므로 tool
argument로 발신 pane을 임의 지정하지 않습니다.

## Room과 주소

- 같은 `(tmux socket, stable window id)`를 공유하는 agent가 한 room입니다.
- agent가 정확히 둘이면 상대를 `peer`로 지정할 수 있습니다.
- 셋 이상이면 `@claude`처럼 handle로, 없으면 `%12` / `pane:%12`로 지정합니다.
- identity를 등록한 agent는 `@reviewer` 또는 `role:rust`처럼 지정할 수 있습니다.
- `scope = "window"`에서는 다른 window의 pane을 거부합니다. `scope = "host"`는
  명시적 `pane:%12` 대상을 다른 window/session까지 넓힙니다.
- 요청은 pane뿐 아니라 현재 agent session에도 고정됩니다. pane을 새 agent가
  재사용해도 이전 요청을 받지 않습니다.

확인:

```bash
muxa peers
muxa peers --json
```

## 기본 handle

agent pane은 아무도 이름을 붙이지 않아도 handle을 하나 받습니다. room에서 그
런타임의 첫 agent가 `@claude`, `@codex`, `@gemini`, `@agy`, `@opencode`가 되고
같은 종류의 두 번째가 `@claude2`가 됩니다. session의 첫 hook 이벤트에서(그리고
`muxa agent start`가 연 pane은 즉시) 부여되며, pane 옵션
`@muxa_agent_alias`에 저장되어 muxad·CLI·agent 재시작보다 오래 남습니다.

이미 이름이 있는 pane은 건드리지 않으므로 pipeline alias나 직접 지정한 이름이
우선합니다. `muxa peek`은 각 pane 헤더에 pane id와 함께 handle을 표시합니다.

## Agent identity

pane이 셋 이상인 room에서는 각 agent가 의미 있는 alias와 role을 등록할 수
있습니다. 등록한 이름은 기본 handle을 덮어씁니다. identity는 pane이 아니라 현재 agent session에 고정되므로 pane을 새
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

# 검증된 AIR plan을 작업 입력으로 함께 전달
muxa msg send peer "이 계획의 위험을 검토해 주세요" --kind review \
  --air-ref '{"artifact_id":"urn:air:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","profile":"https://open330.github.io/air/profiles/1.0.0/plan-native-cli","label":"CAL-6924 plan","locator":{"display":".air/cal-6924-plan.air.json","disclosure":"local-only"}}'

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

tracked agent pane에서 `prefix+s`로 `muxa watch`를 열면 같은 lifecycle을 TUI로
사용할 수 있습니다. `m`은 선택한 room peer에게 요청을 보내고, `b`는 claim 없는
mailbox 이력을 열며, `i`는 inbox를 claim하고 `e`는 응답합니다. `muxa dashboard`는
같은 기능을 더 상세한 workspace-card 화면으로 제공합니다.

composer 안에서 `Ctrl-E`는 전달 방식을 순환합니다. `read-only`와 `execute`는
durable request에 실리는 계약이고, `just send`는 본문을 키스트로크로 pane에 그대로
입력합니다 — request도 응답도 계약도 없습니다. 차이는 화면에 그대로 드러납니다.
계약 모드는 kind·mode 배지를 보여주고, just-send는 `▷ SEND · keystrokes`만
보여줍니다. 키스트로크 위에 QUESTION 배지를 띄우면 존재하지 않는 계약을 주장하는
셈이기 때문입니다.

watch composer는 `? QUESTION`, `◆ REVIEW`, `▶ TASK`, `! NOTICE`를 서로 다른
색상으로 표시합니다. `○ READ-ONLY`는 조사·답변만 위임하고 `● EXECUTE`는 명령과
파일 변경을 허용합니다. 두 mode는 수신 agent에게 전달되는 계약이며 muxa가 직접
작업을 실행하는 스위치가 아닙니다.

## MCP tools

| Tool | 역할 |
| --- | --- |
| `muxa_collaboration_guide` | reviewer/question/subagent/AIR 전달의 권장 계약 조회 |
| `muxa_room_context` | self, same-window peers, unread count 조회 |
| `muxa_call_peer` | 등록 스킬 확장, peer 선택, durable 요청, 선택적 응답 대기, 확인 후 agent 생성 |
| `muxa_peer_report` | 이전 peer 요청의 최신 완료 보고 또는 정확한 request id 조회 |
| `muxa_set_identity` | 현재 agent session의 room-local alias/roles 교체 |
| `muxa_send_message` | durable request 생성 |
| `muxa_inbox` | 현재 agent session의 요청 claim/read |
| `muxa_list_messages` | incoming/sent/all request 상태 조회(미claim) |
| `muxa_reply` | completed/blocked/declined/failed 구조화 응답 |
| `muxa_wait_reply` | event-driven 방식으로 요청의 terminal reply 대기 |
| `muxa_cancel_message` | 아직 queued인 발신 요청 취소 |

## Agent 대화에서 자연스럽게 호출하기

`muxa mcp`에 연결된 Claude나 Codex agent는 `@peer`와 `@muxa-peer`를 Muxa 협업
전용 표현으로 취급합니다. `@codex` 같은 provider, 고유한 `@alias`, `role:name`,
또는 자연어로 동료에게 새 작업을 요청하는 표현은 `muxa_call_peer`로 변환합니다.
`/name`으로 등록 스킬을 함께 지정할 수 있습니다.

```text
@peer 현재 변경사항을 리뷰해줘
@codex /review-plan-feedback commit abc123을 context로 사용해줘
```

“`@peer`의 보고”, “peer 응답”, “peer 지적을 해결해”처럼 기존 결과를 가리키는
표현은 먼저 `muxa_peer_report`로 실제 구조화된 mailbox 응답을 읽습니다. 사용자가
제공했거나 이미 확인된 context에 명시적인 PR 번호나 GitHub PR URL이 없다면
GitHub PR/review 도구를 호출하거나 PR을 추측해서는 안 됩니다. repository나 cwd만
알고 있는 것은 충분한 근거가 아닙니다. Muxa 도구가 보이지 않을 때의 복구 방법은
agent 재시작이며 GitHub를 대체 transport로 사용하지 않습니다.

이 고수준 도구는 아래 mailbox 의미를 유지하면서 model이 여러 저수준 호출을 직접
조립하지 않아도 되게 합니다. 기본값은 `kind=review`,
`work_mode=read_only`이며 정상 peer를 결정적으로 선택하고 구조화된 응답을
기다립니다. execute mode에는 명시적인 task 승인이 필요합니다. 적합한 peer가 없을
때 Muxa는 자동으로 만들지 않고 확인을 요청하며, 사용자가 승인한 뒤에만
`spawn_if_missing=true`로 다시 호출할 수 있습니다. MCP process는 시작할 때 도구와
스킬을 읽으므로 스킬 변경이나 Muxa 업그레이드 뒤에는 기존 agent를 재시작하세요.
승인된 자동 spawn에서는 pane을 만들기 전에 daemon transition stream을 먼저 구독하고,
해당 pane의 agent 등록 이벤트가 왔을 때만 room context를 다시 읽습니다. 기존의
500ms 등록 polling loop는 사용하지 않습니다.

대기는 model이 주도하는 polling loop가 아니라 MCP tool call 하나를 blocking하는
방식입니다. muxad는 durable mailbox의 단조 증가 revision을 구독하고, revision이
바뀌거나 최종 timeout 경계에 도달했을 때만 해당 request를 다시 읽습니다.
새 client가 `collaboration_wait`를 거부하는 구형 daemon에 연결된 경우에는 같은 tool
call 내부에서만 bounded `collaboration_get` 호환 polling을 사용하므로 model turn은
추가되지 않습니다. 최신 daemon은 항상 event-driven 경로를 사용합니다.
`wait=false`로 보냈다면 발신 agent는 독립 작업을 계속할 수 있고, muxad가 reply
revision과 발신 pane의 Idle 전환에 반응해 짧은 알림 prompt를 한 번 전달합니다.
Muxa가 관리하는 peer를 `sleep`, raw `tmux capture-pane`, 반복 status/capture 호출로
모니터링하지 마세요.

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

`wake_payload = "full"`이면 Agent B의 두 단계는 다음처럼 바뀝니다.

```text
Agent B: idle 상태에서 metadata와 원문이 포함된 claimed request prompt 수신
Agent B: inbox 호출 없이 작업 후 muxa_reply(request_id="req_...", ...)
```

## Reviewer와 subagent로 활용하기

agent가 상당한 작업을 시작할 때 권장 순서는 다음과 같습니다.

1. `muxa_collaboration_guide`, `muxa_room_context`로 같은 room의 peer와 계약을
   확인합니다.
2. reviewer에는 `kind=review`, `work_mode=read_only`와 검토 범위를 보냅니다.
3. 구현을 위임할 subagent에는 `kind=task`, `work_mode=execute`와 겹치지 않는 좁은
   path scope를 보냅니다.
4. 발신 agent는 독립적으로 진행하고, 응답을 받은 뒤 결과를 직접 검증해
   통합합니다.

수신 agent는 inbox를 빠르게 claim하고 kind/work mode/path를 지키며, 성공 여부와
관계없이 `muxa_reply`로 terminal 상태를 남겨야 합니다. 두 agent가 같은 파일을
동시에 수정해야 한다면 별도 worktree를 사용하세요.

## AIR artifact 전달과 시각화

request와 reply의 `air_artifacts`에는 AIR 1.0 artifact의 타입이 지정된 참조를 최대
8개까지 첨부할 수 있습니다. `muxa watch`와 `muxa dashboard` mailbox는 첫 참조를
색상 배지로 표시하고, 상세 영역에서 input/output, 짧은 digest, label, 표시용
locator를 보여줍니다.

지원 profile은 AIR 1.0의 정확한 네 profile입니다.

- `https://open330.github.io/air/profiles/1.0.0/workflow-skill` → `AIR WORKFLOW`
- `https://open330.github.io/air/profiles/1.0.0/plan-native-cli` → `AIR PLAN`
- `https://open330.github.io/air/profiles/1.0.0/trace-native-run` → `AIR TRACE`
- `https://open330.github.io/air/profiles/1.0.0/trace-session-snapshot` → `AIR SESSION`

artifact ID는 `urn:air:sha256:` 뒤에 소문자 64자리 SHA-256 digest가 와야 합니다.
locator는 `local-only` 또는 `redacted` disclosure를 가진 표시용 힌트일 뿐이며 파일
접근 권한이나 실행 권한이 아닙니다. muxa는 참조 형식만 검사하고 artifact의 AIR
conformance를 주장하지 않습니다. 검증·편집·그래프 탐색은 AIR Workbench에서
수행하세요. muxa 협업 내용을 위해 새 trace profile을 만들거나 session snapshot에
prompt/message/path/provider 식별자를 넣어서는 안 됩니다.

## 전달 안전성

메시지와 응답 본문은 `$XDG_DATA_HOME/muxa/collaboration.json`에 먼저
저장됩니다. `idle_only` wake는 새 요청뿐 아니라 아직 발신자가 읽지 않은 terminal
응답에도 적용됩니다. 다음 조건을 모두 만족할 때만 pane에 입력합니다.

- hook 기반의 실제 agent session
- state가 `Idle`
- target pane/session이 요청 생성 시점과 동일
- backend가 targeted input을 지원

`Working`, `WaitingInput`, `WaitingChoice`, `Error` 상태에는 입력하지 않습니다.
화면 감지로 생성된 synthetic agent는 안정적인 session identity가 없어 room
participant나 자동 wake 대상이 아닙니다. `wake = "never"`로 설정하면 mailbox는
유지하면서 모든 입력 주입을 끌 수 있습니다.

기본값인 `wake_payload = "notice"`는 짧은 mailbox 알림만 넣고 본문을 terminal에
주입하지 않습니다. 요청을 `muxa_inbox`로 읽는 순간 원자적으로 claim합니다.

`wake_payload = "full"`은 terminal에 입력하기 전에 queued request 하나를
원자적으로 claim하고, request id/source/kind/work mode/paths/AIR reference와 원문을
구조화된 prompt로 직접 전달합니다. 이 request에는 inbox를 다시 호출할 필요가 없어
tool round와 전체 JSON envelope를 줄입니다. idle generation 하나에는 원문 하나만
제출하고, 실제 Idle transition이 온 뒤 다음 요청을 전달합니다. reply 본문은 이
모드에서도 항상 mailbox에만 둡니다.

`full`은 요청 본문을 terminal과 agent prompt history에도 남기므로 민감한 본문을
mailbox에만 두려면 `notice`를 사용하세요. terminal 제어문자가 포함된 본문은
변형하거나 위험하게 paste하지 않고 자동으로 `notice` 경로를 사용합니다. muxad는
prompt text 기록과 별도 Enter 제출 단계를 durable state로 구분합니다. 중단 후
text가 이미 기록된 것이 확실하면 Enter만 재시도하고, 기록 여부가 불확실하면 원문을
다시 넣는 대신 짧은 inbox 복구 알림을 보냅니다. agent가 먼저 inbox를 읽으면 이
자동 복구는 취소됩니다.

모든 collaboration IPC 호출(context, identity, send, inbox, list, reply, get, wait,
cancel)은 `$XDG_DATA_HOME/muxa/collaboration-audit.ndjson`에도 append-only로
기록됩니다. 이 로그는 `0600`이며 message/reply 본문을 중복 저장하지 않고 operation,
request id, 대상, 결과, represented origin/session과 OS-observed caller만 담습니다.
`muxa msg list --json`과 `muxa_list_messages`에서는 각 request의 생성 provenance를 바로
볼 수 있습니다. 업그레이드 전 요청은 `provenance`가 없을 수 있습니다.

origin과 observed caller pane이 다르더라도 기존의 자유로운 대행 권한을 유지하기 위해
전송을 거부하지 않습니다. `mismatched`는 조사 가능한 감사 신호이지 authorization
실패가 아닙니다. 같은 UID의 다른 로컬 프로세스가 IPC를 호출할 수 있다는 기존
owner-only socket 모델도 그대로입니다.

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
