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

아직 Muxa를 설치하지 않았다면 최신 프리빌트 CLI를 임시로 받아 바로 실행할 수
있습니다. Rust, Git, tmux, muxad가 필요하지 않고 종료할 때 임시 바이너리도
삭제됩니다.

    curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko

이미 설치했다면 다음 명령을 사용합니다.

하나의 전체 화면 시나리오가 가상 기본 shell에서 시작해 tmux session/window/
pane 조작을 가르친 뒤, 화면을 닫지 않고 실제 `muxa watch`와 닮은 화면으로
이어집니다. 모든 장면은 설명용이므로 실제 tmux session을 변경하지
않습니다.

mock은 실제 work view처럼 `WORKSPACE › WORK · DUR · ACT · SUMMARY` 열을 사용하고,
상태를 별도 오른쪽 열에 두지 않습니다. 각 session 이름 왼쪽의 고정 gutter에
상태를 모아 표시합니다. 기본 Unicode 설정에서는 `●` working, `▶`
waiting-input, `◆` choice, `○` idle, `■` error이며 `[ui] icons = "ascii"`
환경에서는 대응하는 ASCII marker가 사용됩니다. 120열 이상에서는 실제
watch처럼 Works와 Inspector를 50/50으로 나눕니다.

    muxa onboard

한국어 locale(`LANG`, `LC_MESSAGES`, `LC_ALL`의 `ko*`)에서는 한글 안내가
자동으로 선택됩니다. 명시적으로 선택하거나 영어로 실행할 수도 있습니다.

    muxa onboard --lang ko
    muxa onboard --lang en

온보딩 도중에는 어느 단계에서든 `F2`로 한글과 영어를 즉시 전환할 수 있습니다.
실제 watch를 재현하는 `WORKSPACE › WORK`, `DUR`, `ACT`, `SUMMARY` label은
그대로 유지하고 안내 dialog, 도움말, footer 설명은 선택한 언어로 표시합니다.
각 단계에서 직접 입력하거나 눌러야 하는 명령과 키는 굵은 노란색으로 표시되어
설명 문장과 한눈에 구분됩니다.

![한글 통합 onboarding: 가상 shell과 tmux 조작에서 Muxa watch workflow로 이어지는 단일 시나리오](demo-onboard.gif)

1/20은 환영 인사로 시작해 Muxa가 왜 tmux 기초부터 안내하는지 설명합니다.
tmux session이 관련 terminal을 하나의 작업 공간으로 유지한다는 의미를 이해한 뒤
`tmux new-session -s muxa-onboarding`을 입력해 연습용 session으로 들어갑니다.
tmux 구간과 watch 구간도 설명을 읽고 Enter만 누르는 방식이 아니라 실제 키를
눌러야 진행됩니다.

- `j`/`↓`: 다음 work로 이동
- `l`/`→`: 선택한 work의 child agent로 진입
- `Alt-T`: state/attention 순으로 정렬
- `o`: pane preview 열기와 닫기
- `?`/`F1`: 전체 단축키 도움말 열기와 닫기
- `n`: new work + agent form 열기, `Esc`로 안전하게 닫기
- `m`: 선택한 peer의 composer 열기, 빈 입력에서 `Backspace`로 닫기
- `M`: mailbox 열기와 닫기
- `q`: 온보딩 완료. 실제 watch에서도 종료 키

shell 명령은 실제 tmux 진입과 재접속에 필요한 `tmux new-session`과
`tmux attach` 두 개만 직접 입력합니다. Muxa의 긴 work/agent 명령은 따라 치지
않으며, `n`으로 form을 열고 위치와 조작 키를 확인한 다음 `Esc`를 누르면 바로
협업 단계로 넘어갑니다.
`Esc`는 실제 watch와 마찬가지로 열려 있는 preview/help/form/composer를 먼저
닫고, modal이 없을 때 tour를 종료합니다.

모든 화면 설명은 보되 단축키 gate를 건너뛰려면 다음을 사용합니다.

    muxa onboard --no-quiz

터미널 상호작용 없이 전체 가이드를 출력할 수도 있습니다.

    muxa onboard --print --lang ko

## 통합 시나리오의 tmux 구간

Muxa workflow로 들어가기 전에 같은 과정 안에서 tmux의 session/window/pane
구조와 기본 조작을 익힙니다. 이 구간도 실제 layout과 lifecycle을 건드리지 않는
fullscreen mock입니다.
현재 설정된 prefix(`Ctrl-b`, `Ctrl-a` 등)를 감지하고 가상 session에 들어간 뒤
prefix만 직접 누르게 합니다. tmux 안에서는 현재 client가 prefix key table로 들어간 것을
확인하자마자 root table로 되돌립니다. 화면이 넘어가기 전에 suffix를 연속해서
누르지 말고 ✓ 확인을 기다리면 live binding은 실행되지 않습니다. 이후 단계는
가상 prefix와 실제 suffix key로 진행하므로 live window 생성, pane 분할,
client detach가 일어나지 않습니다.

환영 dialog에서 연습용 session을 만드는 이유와 이 실습이 실제 tmux 설정을 바꾸지
않는다는 점을 먼저 확인합니다. 이어 가상 기본 shell의 빈 prompt에서
`tmux new-session -s muxa-onboarding`을 입력해 가상 tmux client로 들어갑니다.
실제 키를 다음 순서로 눌러 진행합니다.

- `w`: session/window tree와 session → window → pane 계층
- `c`: 현재 session 안에 새 window를 만들고 그 window의 shell 화면으로 전환
- `%`, `"`: pane 좌우·상하 분할
- `→`: pane focus 이동
- `z`, `z`: pane 확대 후 원래 split layout 복원
- `[`, `q`: copy mode 진입과 종료
- `d`: client를 detach하고 `[detached …]`가 있는 원래 기본 shell로 복귀
- `tmux attach -t muxa-onboarding`, `Enter`: 기본 shell에서 직접 입력해 재연결
- `s`, `q`, `s`: watch 열기, pane 상태 overlay 확인, watch 실습으로 이어가기

`q`로 여는 pane 상태 overlay는 앞에서 `%`와 `"`로 만든 좌우 분할 후 왼쪽 상하
분할 layout을 그대로 사용합니다. 따라서 숫자와 agent 상태가 실제 pane 위치와
일치합니다.

`w`의 tree, `c`의 활성 window, `%`와 `"`의 pane layout은 다음 단계에서도
사라지지 않고 누적됩니다. `s` 화면은 기본 onboarding과 같은 live watch형
header, 왼쪽 state gutter, work tree, 50/50 inspector, footer를 사용합니다.
별도의 tmux 완료 화면이나 Muxa 시작 화면은 없습니다. managed binding을 익히는
11단계 다음 번호에서 같은 watch 화면의 work 이동 실습이 바로 계속되며,
처음부터 마지막까지 하나의 단계 수와 진행률을 사용합니다.

여기서도 `F2`로 한글/영문을 전환하고, `Esc`로 종료하며, `--no-quiz`로 키
gate를 건너뛸 수 있습니다. 정적 안내만 필요하면 다음을 사용합니다.

    muxa onboard --print --lang ko

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

onboarding의 한글 도움말은 watch의 실제 키 구성을 같은 순서로 설명합니다.

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
