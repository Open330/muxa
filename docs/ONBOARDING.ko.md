# Muxa 온보딩

Muxa의 기본 운영 모델은 단순합니다.

- tmux session = 하나의 work 또는 ticket
- tmux pane = 하나의 agent
- tmux window = 화면 배치용 컨테이너
- Muxa = tmux lifecycle, 위치, 상태, 협업 routing 관리
- Agent = 파일, 코드, Git, 테스트, 추론 수행

같은 ticket은 같은 managed session을 재사용합니다. CAL-7041이 이미
존재할 때 CAL-7041-2를 자동으로 만들어 작업을 분리하지 않습니다.
기존 work와 다른 cwd를 지정하면 조용히 재사용하지 않고 오류를 냅니다.

## 온보딩 실행

대화형 설명과 짧은 확인 문제를 실행합니다.

    muxa onboard

터미널 상호작용 없이 전체 가이드를 출력할 수도 있습니다.

    muxa onboard --print

설치 직후에는 다음 순서가 권장됩니다.

    muxa init
    muxa doctor
    muxa onboard
    muxa watch

tmux 설정을 설치했다면 prefix+s로 watch를 열 수 있습니다.

## 기본 work 흐름

CAL-7041을 첫 agent와 함께 시작합니다.

    muxa work start CAL-7041 \
      --cwd /path/to/repo \
      --agent codex \
      --role implementer \
      --prompt "Implement CAL-7041"

같은 work에 reviewer pane을 추가합니다.

    muxa agent start \
      --work CAL-7041 \
      --agent claude \
      --role reviewer \
      --prompt "Review the current changes and report findings"

work와 agent pane을 조회합니다.

    muxa work list
    muxa work show CAL-7041

현재 turn만 중단할 때는 interrupt를 사용합니다.

    muxa agent control --pane %42 --action interrupt

agent pane 또는 work 전체를 닫는 동작은 확인을 요구합니다.

    muxa agent control --pane %42 --action terminate
    muxa work close CAL-7041

Muxa가 managed metadata를 기록하지 않은 pane과 session은 종료하지
않습니다.

## muxa watch

muxa watch는 session 중심 화면을 기본으로 사용합니다. session row는
work를 나타내고, 펼친 child row는 agent pane을 나타냅니다.

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
- Alt-S/L/D/T: session, latest, duration, state 정렬
- ? 또는 F1: 전체 단축키 도움말

onboarding은 watch의 실제 도움말 데이터를 사용하므로 단축키 설명과
구현이 서로 어긋나지 않습니다.

## Agent가 Muxa MCP를 사용하는 패턴

별도 tmux MCP 서버를 추가하지 않고 기존 muxa mcp를 사용합니다.

    muxa_start_agent(
      work="CAL-7041",
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
    muxa_manage_tmux(action="close_work", work="CAL-7041", confirm=true)

## 경계

Muxa는 범용 shell이나 임의 tmux command를 제공하지 않습니다. 파일
수정, Git, 테스트 실행은 agent가 담당합니다. Muxa는 work/session과
agent/pane을 안전하고 반복 가능한 방식으로 생성·관찰·제어하는 control
plane에 집중합니다.
