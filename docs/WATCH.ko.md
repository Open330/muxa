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
표시합니다. session 하나는 workspace/project, window 하나는 work/ticket,
child pane 하나는 agent입니다. `view = "pane"`은 pane별로 한 줄씩 보여줍니다.

## 주요 키

| Key | Action |
| --- | --- |
| 일반 문자 입력 | workspace, work, agent, cwd, model, prompt를 즉시 필터링. |
| `/` | 예약 단축키로 시작하는 검색어까지 입력할 수 있는 명시적 검색 시작. |
| `Backspace` / `Ctrl-W` / `Ctrl-U` | 문자 / 단어 / 전체 검색어 삭제. |
| `j` / `k`, `↑` / `↓` | work 사이 이동. 자식 진입 후에는 agent 사이 이동. |
| `h` / `l`, `←` / `→` | 부모 work로 복귀 / 첫 번째 자식 agent 선택. |
| `gg` / `G`, `Home` / `End` | 첫 번째 / 마지막 선택 가능 행으로 이동. |
| `Ctrl-U` / `Ctrl-D`, `PageUp` / `PageDown` | 탐색 중 반 페이지 / 한 페이지 이동. |
| `Enter` | 선택한 pane에 바로 attach. |
| `n` | workspace session과 work window를 생성/재사용하고 agent pane 추가. |
| `\|` | list/inspector 분할 순환: 50/50 → 70/30 → 30/70. |
| `a` / `A` | 설정한 agent에게 headless 질의 / 답변 이력 보기. |
| `m` / `M` | 선택한 agent에게 request 보내기 / incoming·sent mailbox 열기. |
| `b` | `M`의 이전 alias. mailbox 안에서 `i`는 claim, `e`는 reply. |
| `o` / `Alt-P` | live preview 열기. |
| `:` | 명령 팔레트 열기. `Tab`은 첫 번째 일치 명령 완성. |
| `r` / `Ctrl-R` / `Alt-R` | 탐색 중 refresh. |
| `?` / `F1` / `Alt-?` | 도움말. |
| `q` / `Ctrl-C` | 탐색 중 종료 / 어디서든 종료. |
| `Alt-I` | 넓은 화면의 상시 inspector toggle. |
| `Alt-E` | 완료·오류·입력 요청 event inbox 열기. |
| `Alt-A` | error/input/choice만 보는 attention filter. |
| `[` / `]` | preview에서 선택 work의 이전 / 다음 agent 보기. |
| `c` | preview content toggle. |
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
`view pane|session|swarm`, `help`, `quit`를 지원합니다. `kill`과 `abort`는 기존과
동일하게 확인 popup을 거칩니다. `view` 변경은 cached snapshot에 즉시 반영되며
현재 watch process의 이후 refresh에도 유지됩니다.


## Ask

`a`는 컴포저 제목에 표시된 agent에게 보낼 headless 질의를 작성합니다. `Tab`으로
claude ↔ codex를 바꾸고, `Ctrl-V`로 붙여넣고, `Enter`로 보냅니다. muxad가 agent를
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

## Agent 협업

메시지를 보낼 agent pane을 선택하고 `prefix+s`로 watch를 엽니다. 같은 tmux
window의 상대 agent를 선택한 뒤 `m`을 누릅니다. room에 peer가 하나뿐이면 watch가
자동으로 선택합니다. composer에서 `Tab`은 request kind를 바꾸고 `Enter`는
전송합니다. `Esc`로 취소하며, 입력이 이미 비어 있을 때는 `Backspace`로도 닫을 수
있습니다. 마지막 kind와 mode는 즉시 저장되어 다음 `m`과 watch 재실행 후에도
복원됩니다.

`[collaboration].scope = "host"`이면 선택한 agent가 watch를 연 window의 유일한
peer보다 우선합니다. origin 자신을 제외한 agent가 하나인 접힌 session도 바로
지정할 수 있으므로 해당 session을 선택하고 `m`을 누르면 펼치지 않아도 그 agent가
대상이 됩니다. “`cx` alias로 새 pane에서 codex를 시작한 뒤 변경사항을
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

`M`은 incoming/sent mailbox를 엽니다(`b`도 alias로 유지됩니다). mailbox 안에서
`m`은 새 메시지를 작성하고 `M`은 mailbox를 닫습니다. `Tab`으로 mailbox를 전환하고
`j`/`k`로 request를 선택하며 `i`로 incoming 작업을 claim하고 `e`로 응답합니다. 일반
shell에서 watch를 열었다면 조회는 가능하지만, 협업하려면 agent pane에서
`prefix+s`로 다시 열라는 안내가 표시됩니다.

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
