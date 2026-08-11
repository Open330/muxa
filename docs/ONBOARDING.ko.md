# Muxa 온보딩

Muxa는 다음 작업 단위 tmux workflow에 맞춰 설계되고 최적화되어 있습니다.

- tmux session = 하나의 work 또는 ticket
- tmux pane = 하나의 agent
- tmux window = 화면 배치용 컨테이너
- Muxa = tmux lifecycle, 위치, 상태, 협업 routing 관리
- Agent = 파일, 코드, Git, 테스트, 추론 수행

같은 ticket은 같은 managed session을 재사용합니다. CAL-7041이 이미
존재할 때 CAL-7041-2를 자동으로 만들어 작업을 분리하지 않습니다.
기존 work와 다른 cwd를 지정하면 조용히 재사용하지 않고 오류를 냅니다.

## 온보딩 실행

전체 화면에서 실제 `muxa watch`와 닮은 mock dashboard를 띄우고 단계별
dialog로 work row, agent pane, 상태, inspector, footer 단축키의 위치를
직접 보여줍니다. 이 화면은 설명용이므로 실제 tmux session을 변경하지
않습니다.

mock은 실제 session view처럼 `SESSION · DUR · ACT · SUMMARY` 열을 사용하고,
상태를 별도 오른쪽 열에 두지 않습니다. 각 session 이름 왼쪽의 고정 gutter에
상태를 모아 표시합니다. 기본 Unicode 설정에서는 `●` working, `▶`
waiting-input, `◆` choice, `○` idle, `■` error이며 `[ui] icons = "ascii"`
환경에서는 대응하는 ASCII marker가 사용됩니다. 120열 이상에서는 실제
watch처럼 Sessions와 Inspector를 50/50으로 나눕니다.

    muxa onboard

한국어 locale(`LANG`, `LC_MESSAGES`, `LC_ALL`의 `ko*`)에서는 한글 안내가
자동으로 선택됩니다. 명시적으로 선택하거나 영어로 실행할 수도 있습니다.

    muxa onboard --lang ko
    muxa onboard --lang en

온보딩 도중에는 어느 단계에서든 `F2`로 한글과 영어를 즉시 전환할 수 있습니다.
실제 watch를 재현하는 `SESSION`, `DUR`, `ACT`, `SUMMARY` 같은 UI label은
그대로 유지하고 안내 dialog, 도움말, footer 설명은 선택한 언어로 표시합니다.

![한글 Muxa onboarding: watch mock 위의 위치별 dialog, 실제 상태 icon과 단축키 실습](demo-onboard.gif)

첫 안내만 `Enter`로 시작합니다. 이후 기능 단계는 설명을 읽고 Enter를 누르는
방식이 아니라 실제 watch 키를 눌러야 진행됩니다.

- `j`/`↓`: 다음 session으로 이동
- `l`/`→`: 선택한 session의 child agent로 진입
- `Alt-T`: state/attention 순으로 정렬
- `o`: pane preview 열기와 닫기
- `?`/`F1`: 전체 단축키 도움말 열기와 닫기
- `n`: new work + agent form 열기, `Esc`로 안전하게 닫기
- `m`: 선택한 peer의 composer 열기, 빈 입력에서 `Backspace`로 닫기
- `M`: mailbox 열기와 닫기
- `q`: 온보딩 완료. 실제 watch에서도 종료 키

shell 명령을 그대로 따라 입력하거나 암기하는 단계는 없습니다. `n`으로 form을
열고 위치와 조작 키를 확인한 다음 `Esc`를 누르면 바로 협업 단계로 넘어갑니다.
`Esc`는 실제 watch와 마찬가지로 열려 있는 preview/help/form/composer를 먼저
닫고, modal이 없을 때 tour를 종료합니다.

모든 화면 설명은 보되 단축키 gate를 건너뛰려면 다음을 사용합니다.

    muxa onboard --no-quiz

터미널 상호작용 없이 전체 가이드를 출력할 수도 있습니다.

    muxa onboard --print --lang ko

## tmux 자체를 배우기

Muxa workflow보다 먼저 tmux의 session/window/pane 구조와 기본 조작을
익히고 싶다면 별도 tmux 과정을 실행합니다.

    muxa onboard --tmux --lang ko

이 과정도 실제 tmux를 건드리지 않는 fullscreen mock입니다. 현재 설정된
prefix(`Ctrl-b`, `Ctrl-a` 등)는 읽어서 표시하지만 terminal로 전송하지
않습니다. 화면이 prefix를 가상으로 누른 상태를 만들고 사용자는 suffix key만
입력합니다. 따라서 학습 중에 live window를 만들거나 pane을 분할하고 client를
detach할 위험이 없습니다.

실제 키를 다음 순서로 눌러 진행합니다.

- `w`: session/window tree와 session → window → pane 계층
- `c`: 현재 session 안에 window 생성
- `%`, `"`: pane 좌우·상하 분할
- `→`: pane focus 이동
- `z`, `z`: pane 확대 후 원래 split layout 복원
- `[`, `q`: copy mode 진입과 종료
- `d`: client detach; session과 agent가 계속 실행된다는 의미
- `s`, `q`, `D`: Muxa의 watch, peek, dashboard prefix binding

여기서도 `F2`로 한글/영문을 전환하고, `Esc`로 종료하며, `--no-quiz`로 키
gate를 건너뛸 수 있습니다. 정적 안내만 필요하면 다음을 사용합니다.

    muxa onboard --tmux --print --lang ko

![한글 tmux onboarding: 안전한 mock에서 window, pane 분할과 이동, zoom, copy mode, detach, Muxa prefix binding 실습](demo-tmux-onboard.gif)

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

onboarding의 한글 도움말은 watch의 실제 키 구성을 같은 순서로 설명합니다.

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
