# Live TUI

`muxa watch`는 주요 interactive surface입니다. 추적 중인 agent와 일반 tmux
pane을 보여주고, pane attach, live preview, 같은 window의
agent 협업을 제공합니다.

TUI 안에 머문 채 prompt 전송, turn abort, live capture 확인까지 하는
workspace-card console이 필요하면 [`muxa dashboard`](DASHBOARD_CLI.ko.md)를
사용하세요.

## 실행

```bash
muxa watch
muxa watch --view work
muxa watch --view pane
muxa watch --include-paneless
```

`view = "work"`는 tmux window 기준으로 묶고 parent를 `workspace › work`로
표시하는 실행 topology view입니다. session은 Workspace를, window는 현재 Run을,
child pane은 agent session을 바인딩합니다. `view = "pane"`은 pane별로 한 줄씩
보여줍니다. 지속되는 Work와 외부 이슈 상태는 이 tree가 아니라 `muxa dashboard`가
담당합니다.

pane 보기에서 실제 window가 하나뿐인 session은 `session › window` 한 행으로
압축하고 pane을 바로 아래에 표시합니다. 압축 행의 선택 의미는 session으로
유지되므로 `n`, rename, close는 계속 session을 대상으로 합니다. 정확한 window
단위 동작은 window 보기로 전환하면 됩니다. window가 여러 개인 session은 기존의
전체 계층을 유지하며, 검색·attention filter 중에도 일치 결과가 원래부터 단일
window였던 것처럼 보이지 않도록 전체 session → window → pane ancestry를 표시합니다.

## 주요 키

| Key | Action |
| --- | --- |
| `/` | 검색 시작. 검색으로 들어가는 유일한 키이며, 이후 입력한 문자가 workspace, work, agent, cwd, model, prompt를 필터링합니다. 검색 중에는 예약 단축키도 평범한 문자입니다. |
| `Backspace` / `Ctrl-W` / `Ctrl-U` | 문자 / 단어 / 전체 검색어 삭제. |
| `j` / `k`, `↑` / `↓` | work 사이 이동. 자식 진입 후에는 agent 사이 이동. |
| `h` / `l`, `←` / `→` | 부모 work로 복귀 / 첫 번째 자식 agent 선택. |
| `gg` / `G`, `Home` / `End` | 첫 번째 / 마지막 선택 가능 행으로 이동. |
| `Ctrl-U` / `Ctrl-D`, `PageUp` / `PageDown` | 탐색 중 반 페이지 / 한 페이지 이동. |
| `Enter` | 선택한 pane에 바로 attach. |
| `C` | 선택한 행이 속한 session에 shell window를 만들고 바로 이동. attach 후 `prefix + c`를 누르는 것과 같습니다. |
| `n` | workspace session과 work window를 생성/재사용하고 agent pane 추가. |
| `w` | pipeline 실행: work id를 입력하면 `muxa work up`을 실행하는 window로 넘어갑니다. |
| `R` / `:rename` | 선택한 tmux session/window 이름 또는 pane title 변경. |
| `\|` | list/inspector 분할 순환: 50/50 → 70/30 → 30/70. |
| `a` / `A` | 설정한 agent에게 headless 질의 / 답변 이력 보기. |
| `m` / `M` | resolve된 agent에게 request 보내기 / 선택 topology scope 이력 열기. |
| `b` | `M`의 이전 alias. mailbox 안에서 `i`는 claim, `e`는 reply. |
| `v` | collaboration 화면에서 table / 시간순 sequence 전환. |
| `o` / `Alt-P` | live preview 열기. |
| `:` | 명령 팔레트 열기. `Tab`은 첫 번째 일치 명령 완성. |
| `r` / `Ctrl-R` / `Alt-R` | 탐색 중 refresh. |
| `?` / `F1` / `Alt-?` | 도움말. |
| `q` / `Ctrl-C` | 탐색 중 종료 / 어디서든 종료. |
| `Alt-I` | 넓은 화면의 상시 inspector toggle. |
| `Alt-E` | 완료·오류·입력 요청 event inbox 열기. |
| `Alt-A` | error/input/choice만 보는 attention filter. |
| `[` / `]` | preview에서 선택 work의 이전 / 다음 agent 보기. |
| `c` | preview 안에서 content toggle. |
| `f` | popup/fullscreen preview toggle. |
| `Alt-L` | 최신 activity 기준 정렬. |
| `Alt-D` | workspace duration 기준 정렬. |
| `Alt-S` | workspace grouping 정렬. |
| `Alt-T` | attention state 우선 정렬. |

### macOS에서 `Alt`가 안 먹을 때

macOS는 터미널이 따로 지정하지 않는 한 Option을 조합(compose) 키로 처리합니다.
그래서 `Alt-I`는 `ˆ`, `Alt-E`는 `´`로 입력되고 keystroke 자체가 watch까지
오지 않습니다. 터미널과 활성 키보드 레이아웃에 따라 다른데, Ghostty의 경우
U.S. Standard / U.S. International 레이아웃에서만 기본으로 Alt가 켜집니다.

- **Ghostty** — `~/Library/Application Support/com.mitchellh.ghostty/config`에
  `macos-option-as-alt = left` (macOS에서는 이 경로가
  `~/.config/ghostty/config`를 덮어씁니다). 저장 후 `cmd+shift+,`로 reload.
  `left`면 오른쪽 Option은 특수문자 입력용으로 남고, `true`면 양쪽 다 Alt가
  됩니다.
- **iTerm2** — Settings → Profiles → Keys → *Left Option key* → **Esc+**.
- **Terminal.app** — Settings → Profiles → Keyboard → *Use Option as Meta key*.

확인 방법: `cat -v` 실행 후 `Alt-I`. `^[i`가 찍히면 정상, `ˆ`면 아직 안 된 것.

모든 `Alt` 바인딩은 command palette에 `Alt` 없는 대체 경로가 있습니다
(`:inspector`, `:events`, `:preview` 등). 터미널 설정과 무관하게 동작합니다.

## 검색과 Attention

table에서 일반 문자를 입력하면 즉시 대소문자 구분 없는 필터가 적용됩니다. 검색어가
비어 있을 때 `hjkl`, `q`, `r`, `o`, `g` 등은 탐색 단축키입니다. 예약되지 않은
문자로 검색을 시작한 뒤에는 이 키들도 일반 검색 문자로 입력됩니다. 검색어 자체가
예약 단축키로 시작해야 한다면 `/`를 먼저 누릅니다. 이 명시적 검색에서는
Backspace로 빈 문자열이 되어도 검색 입력이 유지됩니다. `Ctrl-W`는 단어를 지우고,
`Ctrl-U` 또는 `Esc`는 검색을 비우고 탐색으로 돌아갑니다.
검색어가 없을 때 `Esc`는 attention filter를 먼저 해제하고, 다음 `Esc`에서
종료합니다.

`Alt-A`는 `waiting_input`, `waiting_choice`, `error` agent만 남깁니다. 검색어와
attention filter는 함께 적용할 수 있습니다.

work view에서는 현재 선택한 work window의 child agent가 별도 조작 없이 자동으로
표시됩니다. 이 상태에서 `↑`/`↓`와 탐색 중의 `j`/`k`는 자식을 건너뛰고 work
사이만 이동합니다. `→` 또는 `l`로 자식 선택에 진입한 뒤에는 같은 세로 이동 키로
해당 work의 agent를 고르고, `←` 또는 `h`로 부모 work에 복귀합니다. 다른
work로 이동하면 이전 work는 접히고 새 work가 펼쳐집니다. pane이
하나뿐인 work는 중복되는 자식 행을 표시하지 않습니다. 선택된 work나
자식 agent의 기존 `↳ detail` 줄은 그대로 유지되며, process tree 정보가 있으면
같은 detail 줄 높이 안에서 함께 표시됩니다.

## Inspector와 Events

터미널 폭이 120 column 이상이면 선택 pane의 live capture가 오른쪽 inspector에
상시 표시됩니다. `Alt-I`로 끌 수 있으며 좁은 화면에서는 기존 preview popup을
사용합니다.

여기서 120은 터미널 폭이 아니라 `muxa watch`가 실제로 받는 폭입니다. inset
`display-popup`은 inset과 테두리를 모두 깎기 때문에, 134 column 터미널에서
`-w 90%` popup의 내부 폭은 118에 그칩니다. 기본 `prefix + s` 바인딩이
테두리 없는 전체 화면(`-B -w 100% -h 100%`)인 이유입니다.

working agent의 완료, error 진입, input/choice 대기는 watch 실행 중 event inbox에
최대 50개까지 남습니다. header의 `◆ N new`가 아직 확인하지 않은 수이고 `Alt-E`로
inbox를 열면 확인 처리됩니다.

## 명령 팔레트

탐색 중 `:`를 누르면 명령 팔레트가 열립니다. 명령을 입력하고 `Enter`로 실행하며,
`Tab`은 첫 번째 일치 항목을 완성하고 `Esc`는 취소합니다. `refresh`, `preview`,
`copy`, `attention`, `events`, `inspector`, `sort latest|duration|session|state`,
`view pane|session|swarm`, `layout tree|swarm|work`, `layout table|sequence`,
`screen topology|collab`, `help`, `quit`를 지원합니다. table/sequence 명령은
collaboration 화면만 바꾸고 topology layout은 그대로 둡니다. `kill`과 `abort`는 기존과
동일하게 확인 popup을 거칩니다. `view` 변경은 cached snapshot에 즉시 반영되며
현재 watch process의 이후 refresh에도 유지됩니다.


## work 레이아웃

`W`를 누르면 트리 대신 Work를 한 줄씩 보여주는 평면 테이블로 바뀝니다.
`muxa work list`와 컬럼이 그대로 일치하고(WORK, WORKSPACE, GEN, ALIASES, DONE,
CWD), CLI 테이블이 보여줄 수 없는 실시간 state 게이지가 앞에 붙습니다. `W`를 다시
누르면 직전 레이아웃으로 돌아가므로 쓰던 swarm이 조용히 버려지지 않습니다.
팔레트의 `layout work`, `--layout work`, `[watch] layout = "work"`로도 갑니다.

행은 실제로 window 노드입니다. 그래서 attach·preview·composer·kill이 트리에서와
똑같이 Work를 대상으로 동작하고, 레이아웃을 바꿔도 커서가 그대로 이어집니다.
접히는 것은 session과 pane 층뿐입니다 — workspace는 여기서 컬럼이고, Work의
pane들은 alias 상태로 요약되기 때문입니다. 이동은 트리의 형제 그룹이 아니라 목록
전체를 훑습니다. 평면 테이블에는 이동할 조상 구조가 없기 때문입니다.

## 화면(Screen)

`muxa watch`는 두 가지 목록 중 하나를 보여주며 `Alt-1` / `Alt-2`, 팔레트의
`screen topology|collab`, `--screen`, `[watch] screen`으로 전환합니다.

`topology`는 session → window → pane 트리이고 `view`·`layout`이 설명하는 모든
것이 여기 적용됩니다. `collab`은 대신 협업 *request*를 나열합니다 — 커서가 놓인
방만 보여주는 `M`과 달리 daemon이 가진 모든 room이 대상입니다. 행이 무엇이냐가
다르기 때문에 `layout`의 값이 아니라 별도 화면입니다.

`view` 축은 그대로 이어집니다: `session`은 tmux session별, `window`는 room별로
묶고 `pane`은 평면 목록입니다. request는 그것이 제기된 room 아래에 묶이므로 어느
묶음에서도 작업이 일어난 위치를 알 수 있습니다. 타이핑하면 양쪽 끝의 alias,
`session:window`, 메시지, kind, status를 대상으로 필터링합니다. `Enter`는 상대
pane으로 이동합니다 — request는 pane보다 오래 남으므로 상대 pane이 사라졌으면
아무 일도 안 하는 대신 그 사실을 알려줍니다.

행에는 발췌만 들어가므로 선택된 request는 표 아래 detail pane에도 펼쳐집니다 —
양쪽 끝과 각자의 room, 어떤 kind·전달 모드로 보냈는지, 본문 전문, 그리고 답장이
왔다면 답장까지. 이 pane은 7행을 쓰므로 16행보다 짧은 터미널에서는 목록을 지키고
본문은 `M`에 맡깁니다.

기본 `table`은 최신순입니다. `v`, `:layout sequence`,
`--collab-layout sequence`, `[watch] collab_layout = "sequence"`를 사용하면 같은
필터 결과를 시간순 participant lifeline으로 그립니다. request는 발신자에서 수신자로,
reply는 점선 역방향 화살표로 표시됩니다. 표현 방식만 바뀌며 필터·선택·attach와
durable history는 table과 같습니다.

자기 mailbox 너머를 보는 것은 operator console 작업이라, 이 화면은 CLI와 같은
트리에서 빌드된 daemon을 요구합니다. 구버전 daemon은 caller 범위 결과로 조용히
답하는 대신 그 사실을 보고합니다.

## Ask

`a`는 컴포저 제목에 표시된 agent에게 보낼 headless 질의를 작성합니다. `Tab`으로
claude ↔ codex를 바꾸고, `Ctrl-V`로 붙여넣으며, 초안 어느 위치에서든 `/`로 공용
스킬 팔레트를 열어 현재 커서에 삽입하고, `Enter`로 보냅니다. muxad가 agent를
print 모드로 실행해 답변을 수집하므로 pane에 입력하지 않고 관리할 세션도 없습니다.
`Esc`로 취소하며, 입력이 이미 비어 있을 때는 `Backspace`로도 닫을 수 있습니다.

`A`는 이력을 엽니다. `j`/`k` 선택, `|` 상세 영역 확대, `Tab` agent 필터(all →
claude → codex), `n` 새 대화. `n` 전까지는 하나의 대화라 질문마다 직전 대화를
resume하며, 두 번째부터는 첫 질문이 지불한 캐시 컨텍스트를 재사용합니다. 대화는
agent별로 분리돼 있어 되돌아오면 그 대화가 이어집니다.

실행 주체는 daemon입니다. 팝업을 닫아도 답변이 이력에 도착하고, 이력은
`$XDG_DATA_HOME/muxa/ask.json`에 남아 재시작 후에도 조회됩니다. `[ask] enabled =
true`가 필요합니다 — [CONFIGURATION.ko.md](CONFIGURATION.ko.md) 참고.

Ask 이력 안에서 `n`은 이력을 지우지 않고 새 대화를 시작합니다. `D`는 확인창을 연
뒤 모든 agent filter의 완료 이력을 지웁니다. 소문자 `d`는 선택한 완료 이력 하나만
확인 후 지웁니다. 두 동작 모두 실행 중인 ask와 conversation id를 보존합니다.

Ask는 headless session이 승인 prompt에 응답할 수 없고 무인 skill 실행을 목적으로
하므로 `[ask].permission_mode = "bypass"`가 기본값입니다. 파일 편집·명령 실행·배포가
확인 없이 가능하므로 신뢰하는 prompt만 보내세요. 더 엄격한 agent 제어가 필요하면
`edit` 또는 `default`를 선택할 수 있습니다. 또한 `cwd` 밖의 symlink target은
`[ask].additional_dirs`에 real path를 추가해야 합니다.

삽입된 스킬은 질문 본문일 뿐입니다. 선택한 agent, `permission_mode`, cwd,
additional directories, timeout은 바꾸지 않으며 daemon이 가진 기존 `[ask]` 계약을
그대로 사용합니다.

## Agent 협업

어디서든 `prefix+s`로 watch를 열고, agent를 선택한 뒤 `m`을 누릅니다. watch는
**operator console**로서 보냅니다 — 발신자는 watch를 연 pane에 들어 있던 agent가
아니라 키보드 앞의 사람입니다. 따라서 watch를 띄운 그 pane도 다른 행과 똑같은
수신 대상이고, 일반 shell pane에서 열어도 문제없습니다. room에 peer가 하나뿐이면
watch가 자동으로 선택합니다. composer에서 `Tab`은 request kind를 바꾸고 `Enter`는
전송합니다. `Esc`로 취소하며, 입력이 이미 비어 있을 때는 `Backspace`로도 닫을 수
있습니다. 마지막 kind와 mode는 즉시 저장되어 다음 `m`과 watch 재실행 후에도
복원됩니다.

`m` composer의 어느 위치에서든 `/`를 누르면 재사용 메시지 스킬 목록이 열립니다.
이름이나 본문을 입력해 검색하고, 방향키 또는 `Tab`으로 선택한 뒤 `Enter`를 누르면
기존 입력을 지우지 않고 현재 커서 위치에 템플릿이 삽입됩니다. 인접한 내용과는
문단으로 구분되며 같은 초안에 여러 스킬을 넣을 수도 있습니다. 선택만으로는
전송되지 않습니다. 확장된 내용을 확인·수정한 뒤 `Enter`를 한 번 더 눌러 보냅니다.
`muxa skill add <name> <prompt>` 또는 설정의
`[message.skills]`로 템플릿을 등록할 수 있습니다. watch의 두 팔레트 모두에서
`F2`는 추가/갱신 form을 열고, `Delete`는 선택한 스킬을 확인 후 삭제합니다. 기존
`Ctrl-A`, `Ctrl-D`도 호환 alias로 유지합니다.

스킬은 request kind나 send mode의 범위를 벗어나지 않습니다. 본문 텍스트만
담으며, `Tab`으로 고른 kind와 `Ctrl-E`로 고른 mode는 스킬을 삽입해도 바뀌지
않습니다. 확장된 본문을 두 번째 `Enter`로 명시적으로 보낼 때 기존 계약이
적용됩니다.

console에는 자기 pane이 없으므로 응답은 발신자에게 되돌아오지 않고 **수신 agent의
mailbox**에 request와 함께 남습니다. `M`은 커서의 topology level을 따릅니다. pane이면
`incoming`은 그 agent의 것, `sent`는 console이 모든 대상에게 보낸 기록이며
claim(`i`)·응답(`e`)·새 메시지(`m`)도 그 agent를 대행합니다. window이면 room 전체를,
session이면 모든 room을 window별로 묶어 보여줍니다. window/session 이력은 read-only라
claim·reply·새 메시지는 pane을 선택한 뒤 사용할 수 있습니다.

`[collaboration].scope = "host"`이면 선택한 session, window, pane이 watch를 연
window의 유일한 peer보다 우선합니다. `l`로 하위 pane까지 내려가지 않아도 parent
node에서 바로 `m`을 사용할 수 있습니다. window는 pane index가 가장 낮은 live
tracked agent를, session은 window index와 pane index가 낮은 순서의 agent를 자동
대상으로 삼습니다. composer 제목에는 전송 전에 실제 agent와 pane이 표시됩니다.
“`cx` alias로 새 pane에서 codex를 시작한 뒤 변경사항을
리뷰해줘” 같은 요청은 `TASK`와 `EXECUTE`를 선택해 보내면 raw keystroke가 아니라
실행 권한이 명시된 작업 계약으로 전달됩니다.

`Ctrl-E`는 전송 방식을 순환합니다. `read-only`와 `execute`는 request에 실리는
계약이고, `just send`는 본문을 계약도 응답도 없는 키스트로크로 pane에 그대로
입력합니다 — `Enter`가 바로 attach하게 된 지금, 일반 prompt는 이 모드로 보냅니다.

- `? QUESTION`(청록): 답을 요청합니다.
- `◆ REVIEW`(자홍): 코드나 판단의 검토 결과를 요청합니다.
- `▶ TASK`(노랑): 구체적인 작업을 위임합니다.
- `! NOTICE`(파랑): 회신이 필요 없는 알림입니다.

`○ READ-ONLY`(초록)는 조사·답변만 허용하고 변경은 위임하지 않는 계약입니다.
`Ctrl-E`로 바꾸는 `● EXECUTE`(빨강)는 실행과 파일 변경을 명시적으로 허용합니다.
이는 muxa가 즉시 명령을 실행한다는 뜻이 아니라, 수신 agent가 inbox에서 읽는 작업
계약입니다. watch는 별도 path scope 입력을 제공하지 않으므로 execute 요청에는
메시지 본문에 수정 범위를 함께 적는 것이 좋습니다.

`M`은 이력을 엽니다(`b`도 alias로 유지됩니다). pane mailbox에서는 `m`으로 새
메시지를 작성하고 `M`으로 닫습니다. `Tab`으로 mailbox를 전환하고 `j`/`k`로 요청을
선택하며 `i`로 pending 요청을 claim하고 `e`로 응답합니다. window/session aggregate는
하나의 결합 stream이므로 `Tab`, `m`, `i`, `e`가 비활성화됩니다.
console이 발신자이므로 일반 shell pane에서 watch를 열어도 협업이 그대로
동작합니다.

AIR artifact 참조가 첨부된 request는 mailbox에서 profile별 색상 배지로
표시됩니다. `AIR WORKFLOW`는 파랑, `AIR PLAN`은 자홍, `AIR TRACE`는 청록,
`AIR SESSION`은 밝은 청록입니다. 선택 상세에는 참조가 작업 입력인지 응답
출력인지와 짧은 digest, label, 표시용 locator가 함께 나옵니다.

## Preview

`o` 또는 `Alt-P`를 누르면 선택 pane의 preview가 열립니다. work view에서 선택한 work window에
agent pane이 여러 개 있으면 `]`로 다음 agent, `[`로 이전 agent를 볼 수 있습니다.
`Tab`, `Shift+Tab`도 같은 동작입니다. agent가 둘 이상이면 preview title에
`2/3`처럼 현재 위치가 표시됩니다.

## tmux Popup Binding

```tmux
bind-key s display-popup -B -E -w 100% -h 100% -x 0 -y 0 "muxa watch"
bind-key D display-popup -E -w 95% -h 90% "muxa dashboard"
```

`prefix+s`가 관측과 협업의 기본 진입점입니다. `prefix+D`는 더 상세한 Dashboard를
바로 여는 선택 단축키입니다.

## 안정적인 tmux 이름

`muxa init`은 `tmux-window-names` 컴포넌트를 기본으로 켭니다. tmux의 process 기반
`automatic-rename`을 꺼서 Work window가 `node`나 `claude`로 덮어써지지 않게 하고,
익숙한 `prefix + ,` prompt를 `muxa window rename`으로 연결합니다. window 이름의
공백은 `-`로 정규화하며 같은 session 안의 중복 이름은 거부합니다. 특정 window만
동적 이름으로 되돌리려면 `muxa window rename --auto`를 실행합니다.

## 한 workspace를 터미널 두 개로 보기

tmux session은 current window를 하나만 가지며, attach된 모든 client가 그 window를
봅니다. 따라서 같은 session에 두 터미널을 붙이면 서로 다른 Work window에 머무를 수
없습니다. muxa의 제약이 아니라 tmux의 모델이고, 이를 바꾸는 유일한 수단이
*session group*입니다 — window 목록은 공유하되 group 안의 각 session이 자기
current window를 따로 가집니다.

`muxa init`이 이를 `tmux-auto-view` 컴포넌트로 설치하며 기본으로 켜집니다. 도착한
client에게 자기 view를 주는 훅 두 개를 겁니다.

```tmux
set-hook -g 'client-attached[9000]' "if -F '#{&&:#{>:#{session_attached},1},#{==:#{@no_auto_view},}}' 'run-shell \"muxa workspace view --client #{client_name}\"'"
set-hook -g 'client-session-changed[9000]' "…같은 내용…"
```

전용 hook array slot을 쓰므로 config를 다시 source해도 중복되지 않고, 사용자가 같은
이벤트에 설치한 다른 hook도 덮어쓰지 않습니다.

**훅이 두 개여야 합니다.** `client-attached`는 `tmux attach`로 붙는 터미널을,
`client-session-changed`는 `switch-client`를 덮습니다 — 후자가 watch의 `Enter`가
하는 일이고, 컴포넌트 설치 이전부터 열려 있던 터미널이 지나는 경로입니다. tmux
3.4에서 실측: attach 훅만 있으면 watch에서 점프할 때 두 터미널이 다시 한 session에
묶이고 그때부터 서로를 따라다닙니다.

터미널이 하나면 아무 일도 일어나지 않습니다. `muxa workspace view`는 자기가 그
session의 유일한 client이면 no-op이므로 단일 터미널 workspace에 여분의 session이
생기지 않고, 재그룹하는 터미널은 자기 view를 재사용해 점프마다 session이 쌓이지
않습니다.

이미 붙어 있는 터미널에는 직접 실행할 수도 있습니다.

```sh
muxa workspace view
```

view 이름은 `<session>~view~<pid>`라 원본 session 옆에 정렬됩니다. tmux가 session
이름을 prefix로 매칭하는데도 안전한 이유는 **정확 일치가 우선**하기 때문입니다 —
`callabo`, `callabo-set`, `callabo~view~1734560`이 모두 있어도 `-t callabo`는
`callabo`로 해석됩니다. detach하면 사라지므로 view가 쌓이지 않습니다.

window 크기를 session에 붙은 가장 작은 client가 아니라 실제로 그 window를 보고
있는 터미널 기준으로 잡으려면 window 단위 sizing을 함께 켭니다.

```tmux
set -g window-size smallest
setw -g aggressive-resize on
```

두 줄 모두 필요하며 `aggressive-resize`만으로는 아무 효과가 없습니다 — 이 옵션은
`window-size`가 `smallest` 또는 `largest`인 window에만 적용되는데 tmux 3.x의
기본값은 `latest`입니다. tmux 3.4에서 200x50과 80x24 client를 각각 다른 window에
두고 실측한 결과:

| 설정 | 200x50 client가 보는 window | 80x24 client가 보는 window |
| --- | --- | --- |
| `window-size latest` (기본값) | 80x23 | 80x23 |
| `smallest` + `aggressive-resize on` | **200x49** | 80x23 |
| `smallest` + `aggressive-resize off` | 80x23 | 80x23 |

`smallest` 단독은 전형적인 함정입니다 — 어디든 작은 client가 붙어 있으면 내
window가 쪼그라듭니다. `aggressive-resize`가 "아무 client"를 "이 window를 current로
갖는 client"로 좁혀 주고, 이것이 view 분리와 정확히 맞물립니다. 터미널이 하나뿐이면
smallest가 곧 그 터미널이라 단일 터미널 사용에는 변화가 없습니다.

watch에서 `Enter`로 이동할 때는 대상 window를 `<session_id>:<window_id>`로
지정하므로, 요청한 터미널만 움직이고 group의 다른 session은 보고 있던 window를
유지합니다.

반대로 두 터미널이 *일부러* 같은 화면을 봐야 한다면(페어링, 화면 공유):

```sh
tmux set-option -t <session> @no_auto_view 1
```

## macOS 메뉴바 (BarShelf)

[BarShelf](https://github.com/Open330/barshelf)에 포함된 `muxa Watch` widget을
사용하면 같은 agent 상태를 메뉴바 popover에서 compact하게 확인할 수 있습니다.
latest activity 기준 상위 5개 agent를 `NAME / ST / ACT / LAST PROMPT` layout으로
표시합니다. popover가 열려 있을 때 5초마다 갱신하고 background에서는 polling하지
않습니다.

BarShelf gallery에서 설치하거나 다음 명령을 사용하세요.

```bash
barshelf install https://github.com/Open330/barshelf/tree/master/widgets/muxa-watch
```

Deno와 versioned snapshot command를 지원하는 `muxa`가 필요합니다. 기본 설치
경로/소켓이 아닌 경우 `MUXA_BIN` 또는 widget의 custom socket setting을 지정하세요.

```bash
muxa status --json
```

## Columns

`[watch]`에서 설정합니다:

```toml
[watch]
view = "work"
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6
```

사용 가능한 column key: `pane`, `state`, `state_age`, `kind`, `model`, `ctx`, `cost`,
`limits`, `workload`, `prompt`, `activity`, `workspace_time`.
기본 `state_age` column은 `▶ WAIT 3m`, `● WORK 42s`처럼 현재 상태와 해당 상태에
머문 시간을 함께 보여줍니다. compact glyph만 필요하면 `state`를 사용하세요.
기본값에서는 child shell/subagent 작업이 선택된 row의 detail line에만
`tree ◇1 ▸1 +2`처럼 표시됩니다. `workload`를 `columns`에 추가하면 항상 보이는
`TREE` 컬럼으로 렌더링합니다. `◇`는 subagent, `▸`는 shell, `+`는 기타 표시 대상
process를 의미합니다.

## Sort

```toml
[watch]
sort = ["state", "workspace", "latest"]
# sort = ["latest"]
# sort = ["workspace_time"]
# sort = ["state", "latest"]
# sort = ["workspace", "pane"]
# sort = ["pane_id"]
```

런타임 정렬 키는 위 preset과 대응하며, 선택한 preset을 `[watch].sort`에 다시
저장합니다. `--sort` flag는 런타임 정렬 키를 누르기 전까지 현재 실행에만 적용되는
override입니다. 기본값은 attention state를 먼저 띄운 뒤 workspace로 묶고,
각 group 안에서 가장 최근 activity가 있는 work를 위로 올립니다. `activity`와
`act`는 `latest` alias로 계속 동작합니다.

## Detail Row

```toml
[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

사용 가능한 변수: `pane`, `kind`, `state`, `model`, `ctx`, `cost`, `activity`,
`workload`, `last_prompt`, `last_response`, `last_notification`, `cwd`.

표시 가능한 workload가 있으면 선택 row의 detail line은 template보다 먼저
session/name 컬럼에 `tree ...`를 보여줍니다.

긴 detail 내용은 table에 맞게 잘립니다. 더 많은 맥락이 필요하면 preview를
사용하세요.
