# Live TUI

`muxa watch`는 주요 interactive surface입니다. 추적 중인 agent와 일반 tmux
pane을 보여주고, pane attach, live preview, prompt composer를 제공합니다.

TUI 안에 머문 채 prompt 전송, turn abort, live capture 확인까지 하는
session-card console이 필요하면 [`muxa dashboard`](DASHBOARD_CLI.ko.md)를
사용하세요.

## 실행

```bash
muxa watch
muxa watch --view session
muxa watch --view pane
muxa watch --include-paneless
```

`view = "session"`은 tmux session 기준으로 묶고, `view = "pane"`은 pane별로
한 줄씩 보여줍니다.

## 주요 키

| Key | Action |
| --- | --- |
| 일반 문자 입력 | session, agent, cwd, model, prompt를 즉시 필터링. |
| `/` | 예약 단축키로 시작하는 검색어까지 입력할 수 있는 명시적 검색 시작. |
| `Backspace` / `Ctrl-W` / `Ctrl-U` | 문자 / 단어 / 전체 검색어 삭제. |
| `j` / `k`, `↑` / `↓` | session 사이 이동. 자식 진입 후에는 agent 사이 이동. |
| `h` / `l`, `←` / `→` | 부모 session으로 복귀 / 첫 번째 자식 agent 선택. |
| `gg` / `G`, `Home` / `End` | 첫 번째 / 마지막 선택 가능 행으로 이동. |
| `Ctrl-U` / `Ctrl-D`, `PageUp` / `PageDown` | 탐색 중 반 페이지 / 한 페이지 이동. |
| `Enter` | 선택한 pane의 prompt composer 열기. 빈 `Enter`는 attach. |
| `o` / `Alt-P` | live preview 열기. |
| `:` | 명령 팔레트 열기. `Tab`은 첫 번째 일치 명령 완성. |
| `r` / `Ctrl-R` / `Alt-R` | 탐색 중 refresh. |
| `?` / `F1` / `Alt-?` | 도움말. |
| `q` / `Ctrl-C` | 탐색 중 종료 / 어디서든 종료. |
| `Alt-I` | 넓은 화면의 상시 inspector toggle. |
| `Alt-E` | 완료·오류·입력 요청 event inbox 열기. |
| `Alt-A` | error/input/choice만 보는 attention filter. |
| `[` / `]` | preview에서 선택 session의 이전 / 다음 agent 보기. |
| `c` | preview content toggle. |
| `f` | popup/fullscreen preview toggle. |
| `Alt-L` | 최신 activity 기준 정렬. |
| `Alt-D` | session duration 기준 정렬. |
| `Alt-S` | session grouping 정렬. |
| `Alt-T` | attention state 우선 정렬. |

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

session view에서는 현재 선택한 session의 자식들이 별도 조작 없이 자동으로
표시됩니다. 이 상태에서 `↑`/`↓`와 탐색 중의 `j`/`k`는 자식을 건너뛰고 session
사이만 이동합니다. `→` 또는 `l`로 자식 선택에 진입한 뒤에는 같은 세로 이동 키로
해당 session의 agent를 고르고, `←` 또는 `h`로 부모 session에 복귀합니다. 다른
session으로 이동하면 이전 session은 접히고 새 session이 펼쳐집니다. pane이
하나뿐인 session은 중복되는 자식 행을 표시하지 않습니다. 선택된 session이나
자식 agent의 기존 `↳ detail` 줄은 그대로 유지되며, process tree 정보가 있으면
같은 detail 줄 높이 안에서 함께 표시됩니다.

## Inspector와 Events

터미널 폭이 120 column 이상이면 선택 pane의 live capture가 오른쪽 inspector에
상시 표시됩니다. `Alt-I`로 끌 수 있으며 좁은 화면에서는 기존 preview popup을
사용합니다.

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

## Prompt Composer

pane이 있는 row에서 `Enter`를 누르면 prompt composer가 열립니다. 내용을 입력한
뒤 `Enter`를 누르면 해당 pane으로 보냅니다. `Esc`는 취소입니다. composer가
비어 있으면 `Enter`는 prompt 전송 대신 pane attach로 동작합니다.

activity logging이 켜져 있으면 prompt input 시간은 `activity.ndjson`에 human
interaction interval로 기록됩니다.

## Preview

`o` 또는 `Alt-P`를 누르면 선택 pane의 preview가 열립니다. session view에서 선택한 session에
agent pane이 여러 개 있으면 `]`로 다음 agent, `[`로 이전 agent를 볼 수 있습니다.
`Tab`, `Shift+Tab`도 같은 동작입니다. agent가 둘 이상이면 preview title에
`2/3`처럼 현재 위치가 표시됩니다.

## tmux Popup Binding

```tmux
bind-key s display-popup -E -w 90% -h 80% "muxa watch"
bind-key D display-popup -E -w 95% -h 90% "muxa dashboard"
```

`prefix+s`는 picker를 열고, `prefix+D`는 현재 선택한 agent의 협업 Dashboard를
엽니다.

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
view = "session"
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6
```

사용 가능한 column key: `pane`, `state`, `state_age`, `kind`, `model`, `ctx`, `cost`,
`limits`, `workload`, `prompt`, `activity`, `session_time`.
기본 `state_age` column은 `▶ WAIT 3m`, `● WORK 42s`처럼 현재 상태와 해당 상태에
머문 시간을 함께 보여줍니다. compact glyph만 필요하면 `state`를 사용하세요.
기본값에서는 child shell/subagent 작업이 선택된 row의 detail line에만
`tree ◇1 ▸1 +2`처럼 표시됩니다. `workload`를 `columns`에 추가하면 항상 보이는
`TREE` 컬럼으로 렌더링합니다. `◇`는 subagent, `▸`는 shell, `+`는 기타 표시 대상
process를 의미합니다.

## Sort

```toml
[watch]
sort = ["state", "session", "latest"]
# sort = ["latest"]
# sort = ["session_time"]
# sort = ["state", "latest"]
# sort = ["session", "pane"]
# sort = ["pane_id"]
```

런타임 정렬 키는 위 preset과 대응하며, 선택한 preset을 `[watch].sort`에 다시
저장합니다. `--sort` flag는 런타임 정렬 키를 누르기 전까지 현재 실행에만 적용되는
override입니다. 기본값은 attention state를 먼저 띄운 뒤 tmux session으로 묶고,
각 group 안에서 가장 최근 activity가 있는 agent를 위로 올립니다. `activity`와
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
