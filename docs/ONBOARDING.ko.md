# Muxa 온보딩

Muxa는 다음 작업 단위 tmux workflow에 맞춰 설계되고 최적화되어 있습니다.

- tmux session = 하나의 workspace 또는 project
- tmux window = 하나의 work 또는 ticket
- tmux pane = 하나의 agent
- Muxa = tmux lifecycle, 위치, 상태, 협업 routing 관리
- Agent = 파일, 코드, Git, 테스트, 추론 수행

같은 workspace의 같은 ticket은 같은 managed window를 재사용합니다. 기존
work와 다른 cwd를 지정하면 조용히 재사용하지 않고 오류를 냅니다. 한 workspace
session에는 서로 다른 cwd/worktree를 가진 여러 work window가 공존할 수 있습니다.

## 온보딩 실행

`muxa onboard`의 기본 동작은 실제 tmux와 Muxa를 사용하는 16단계 live tour입니다.
개인 환경과 격리된 전용 tmux server, `muxad`, config, data directory, mailbox를
만들고 모든 종료 경로에서 sandbox를 삭제합니다. 기존 tmux 안에서 실행하면 prefix가
모호해지므로 거부합니다. detach한 terminal이나 tmux 밖 terminal에서 실행하세요.

    muxa onboard

한국어 locale(`LANG`, `LC_MESSAGES`, `LC_ALL`의 `ko*`)에서는 한글 안내를 자동으로
선택합니다. 명시적으로 선택하거나 영어로 실행할 수도 있습니다. tour 중 `F2`를 누르면
언어가 바뀝니다.

    muxa onboard --lang ko
    muxa onboard --lang en

tour는 keypress를 가로채지 않습니다. 각 단계가 요구한 실제 tmux 또는 Muxa state를
polling하고, 그 상태가 확인되면 다음 단계로 넘어갑니다. 한 단계에서 45초 이상
막히면 `F12` skip을 표시합니다. `--no-quiz`는 step을 없애는 대신 처음부터 `F12`를
표시하며, skip이 필요한 sandbox state도 일관되게 만들어 줍니다.

    muxa onboard --no-quiz

## 16단계 live workflow

`muxa onboard --print --lang ko`가 같은 목록을 출력합니다. 그 출력은 tour의
단계 정의에서 직접 생성되므로, 아래 요약과 달라지면 아래가 낡은 것입니다.

1. `tmux new-session -s muxa-onboarding` — session은 당신 없이도 도는 작업 공간.
2. `Ctrl-b`, `c` — window 하나가 Work 하나.
3. `Ctrl-b`, `s`로 tree를 보고 `q`로 닫습니다.
4. `Ctrl-b`, `d`로 detach. client만 나가고 작업은 계속됩니다.
5. `tmux ls`로 session이 남아 있음을 직접 확인합니다.
6. `tmux attach -t muxa-onboarding`으로 다시 들어갑니다.
7. `Ctrl-b`, `%`로 window를 나눕니다 (`"`는 상하). pane 하나가 agent 하나.
8. `Enter` — tour가 연습용 agent 둘을 띄웁니다. 실제 CLI는 실행되지 않습니다.
9. `muxa watch` — 진입점. session·window·agent를 살아있는 채로 봅니다.
10. watch 안에서 `j`/`k` 이동, `h`/`l` 접기, `Enter`로 pane 진입, `?`로 키 목록.
    둘러본 뒤 `q`로 나옵니다.
11. `muxa attend` — 가장 오래 막힌 agent로 이동합니다.
12. `Ctrl-b`, `;`로 직전 pane으로 복귀 (`Ctrl-b o`는 순환).
13. `muxa msg send @claude "어디까지 됐나요?"` — attach 없이 질문합니다.
14. `muxa msg list` — 보낸 것과 돌아온 답을 확인합니다.
15. `muxa msg inbox` — codex가 보낸 요청을 가져옵니다.
16. `Ctrl-b`, `d`로 마칩니다. tour가 전용 server와 sandbox를 삭제합니다.

핵심 mapping은 `session = workspace`, `window = Work`, `pane = agent`입니다.
터미널 상호작용 없이 같은 순서의 written guide만 출력할 수도 있습니다.

    muxa onboard --print --lang ko

## 설치 없이 실행하기

`scripts/onboard.sh`는 지원되는 release archive를 임시 디렉터리로 받아 SHA-256을
검증한 뒤 진짜 `muxa onboard`를 실행하고, 끝나면 지웁니다. 설치가 아니라 일시적인
download이므로 daemon, config, PATH 항목이 남지 않습니다.

    curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko

launcher에는 network, 지원되는 release 플랫폼, checksum 도구, `tar`, tmux가
필요합니다. release를 내려받아 검증할 수 없으면 다른 tour로 넘어가지 않고 명확한
오류로 끝납니다. tmux를 사용할 수 없으면 설치된 binary의 `muxa onboard --print`를
사용하세요.

설치 직후에는 다음 순서가 권장됩니다.

    muxa init
    muxa doctor
    muxa onboard
    muxa watch

tmux 설정을 설치했다면 prefix+s로 watch를 열 수 있습니다.

## 기본 work 흐름

muxa-onboarding을 첫 agent와 함께 시작합니다.

    muxa work start muxa-onboarding \
      --workspace muxa \
      --cwd /path/to/repo \
      --agent codex \
      --role implementer \
      --prompt "Implement muxa-onboarding"

같은 work에 reviewer pane을 추가합니다.

    muxa agent start \
      --workspace muxa \
      --work muxa-onboarding \
      --agent claude \
      --role reviewer \
      --prompt "Review the current changes and report findings"

work와 agent pane을 조회합니다.

    muxa workspace list
    muxa work list --workspace muxa
    muxa work show muxa-onboarding --workspace muxa

현재 turn만 중단할 때는 interrupt를 사용합니다.

    muxa agent control --pane %42 --action interrupt

agent pane, work window 또는 workspace session을 닫는 동작은 확인을 요구합니다.

    muxa agent control --pane %42 --action terminate
    muxa work close muxa-onboarding --workspace muxa
    muxa workspace close muxa

Muxa가 managed metadata를 기록하지 않은 pane, window, session은 종료하지
않습니다.

## muxa watch

muxa watch는 work 중심 화면을 기본으로 사용합니다. `workspace › work` parent
row는 tmux window를, 펼친 child row는 agent pane을 나타냅니다.

주요 단축키:

- Enter: 선택한 pane으로 이동
- n: 새 work와 첫 agent 생성, 또는 기존 work에 agent 추가
- m: 선택한 agent에 collaboration 메시지 작성
- M: mailbox 열기
- a: headless ask
- A: ask history
- o 또는 Alt-P: pane preview
- Alt-I: inspector
- Alt-E: event inbox
- Alt-A: attention 상태만 보기
- Alt-K: 선택한 managed pane 종료 확인
- Alt-X: 현재 turn 중단 확인
- Alt-S/L/D/T: workspace, latest, duration, state 정렬
- ? 또는 F1: 전체 단축키 도움말

onboarding의 watch 단계도 이 실제 TUI를 그대로 실행합니다.

## Agent가 Muxa MCP를 사용하는 패턴

별도 tmux MCP 서버를 추가하지 않고 기존 muxa mcp를 사용합니다.

    muxa_start_agent(
      workspace="muxa",
      work="muxa-onboarding",
      agent="codex",
      role="reviewer",
      prompt="Review the current changes"
    )

중간 transition마다 polling하지 않고 settled 상태까지 기다리며 마지막
화면을 함께 받습니다.

    muxa_wait_for_change(
      pane="%42",
      until="settled",
      include_capture=true
    )

특정 pane의 상태, 화면, 최근 prompt를 한 번에 확인할 수 있습니다.

    muxa_status(
      pane="%42",
      include_capture=true,
      history_limit=1
    )

tmux lifecycle 제어는 하나의 muxa_manage_tmux 도구로 묶여 있습니다.

    muxa_manage_tmux(action="interrupt_agent", pane="%42")
    muxa_manage_tmux(action="terminate_agent", pane="%42", confirm=true)
    muxa_manage_tmux(action="close_work", workspace="muxa", work="muxa-onboarding", confirm=true)
    muxa_manage_tmux(action="close_workspace", workspace="muxa", confirm=true)

## 경계

Muxa는 범용 shell이나 임의 tmux command를 제공하지 않습니다. 파일
수정, Git, 테스트 실행은 agent가 담당합니다. Muxa는 workspace/session,
work/window, agent/pane을 안전하고 반복 가능한 방식으로 생성·관찰·제어하는 control
plane에 집중합니다.
