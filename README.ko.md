<div align="center">

<img src="assets/logo.svg" alt="muxa" width="260" />

**tmux를 작업 단위로 구성하는 AI agent 관측·오케스트레이션 도구.**

에이전트가 working, waiting, idle, error 중 어디에 있는지 tmux 상태바,
실시간 TUI, 데스크톱 알림, 로컬 리포트에서 확인합니다.

[![CI](https://github.com/Open330/muxa/actions/workflows/ci.yml/badge.svg)](https://github.com/Open330/muxa/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.89-informational)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-beta-yellow)

[English](README.md) · **한국어**

</div>

---

## README를 읽기 전에 Muxa 온보딩부터 체험해 보세요

설치 없이 전체 화면 tour를 바로 체험하세요. script가 release 바이너리를 임시
디렉터리로 받아 checksum을 검증한 뒤 진짜 `muxa onboard`를 실행하고 끝나면
지웁니다. 지원되는 release 플랫폼과 network가 필요하고 live tour에는 tmux도
필요합니다. non-interactive 가이드는 `muxa onboard --print`로 볼 수 있습니다.

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko
```

<div align="center">
  <img src="docs/demo.gif" alt="muxa watch에서 여러 agent의 상태를 한눈에 확인하고 필요한 agent로 이동하는 모습" width="900" />
</div>

`muxa`는 terminal multiplexer pane 안에서 실행되는 AI coding agent를 관측하는
작은 daemon + CLI입니다. Claude Code, OpenAI Codex, Google Gemini CLI와 그
후속인 Antigravity CLI(`agy`)의 기존
hook/event 시스템을 사용하고, 이를 tmux pane/session과 연결합니다.

tmux나 agent binary를 fork하지 않습니다. tmux는 기본 full backend이고,
zellij는 CLI baseline과 richer plugin 경로를 준비 중입니다.

## 작업 단위 tmux workflow에 최적화

Muxa는 tmux를 단순한 terminal pane 모음이 아니라 지속적인 작업 실행 모델로
사용합니다.

| tmux 객체 | Muxa에서의 의미 | 사용 방식 |
| --- | --- | --- |
| **session** | workspace 실행 context | 한 workspace의 활성 Run window를 담습니다. |
| **window** | Work의 활성 Run | 안정적인 Work identity에 연결되며 window 종료는 Work 삭제가 아니라 Run 종료입니다. |
| **pane** | agent 실행 surface | implementer, reviewer, helper Agent session을 Run에 연결합니다. |

권장 workflow도 이 모델을 그대로 따릅니다.

1. work ID를 시작하면 Muxa가 workspace session을 생성하거나 재사용하고 work
   window와 첫 agent pane을 만듭니다.
2. 같은 Work Run에 agent pane을 추가하며 다른 Work는 sibling Run window를 사용합니다.
3. `muxa watch`에서 상태 확인, preview, 메시지, 제어를 수행하거나 agent가
   `muxa mcp`를 통해 같은 정책으로 다른 agent를 관리합니다.
4. 작업이 끝나면 agent pane, work window 또는 workspace session을 명시적으로
   닫습니다. Muxa는 unmanaged tmux 객체를 종료하지 않습니다.

Linear/GitHub/Jira issue는 Work에 연결되는 선택적 외부 참조이며 Work 자체나 로컬
단계가 아닙니다. [Work domain model](docs/WORK_MODEL.md)을 참고하세요. 요약하면
**Workspace → Work → Run → Agent session**이며 tmux는 현재 실행 binding입니다.
`muxa onboard`는 전용 일회용 tmux server, daemon, mailbox를 만든 뒤 실제
`tmux new-session`부터 계층, detach/attach, sandbox agent, Muxa watch·attend·message
workflow까지 하나의 시나리오로 익히게 합니다. 기존 tmux server는 건드리지 않고
종료할 때 sandbox 전체를 삭제합니다.

> [!IMPORTANT]
> Beta입니다. event ingest, daemon, CLI, live TUI, desktop notification,
> stats/report는 end-to-end로 동작하지만 1.0 전까지 API가 바뀔 수 있습니다.

## 핵심 기능

| Surface | 기능 |
| --- | --- |
| `muxa status-line` | active pane 기준 tmux `status-right` 한 줄 요약. |
| `muxa peek` | `prefix + q` 오버레이: 각 pane의 실제 화면을 dim 배경으로 깔고 그 위에 handle(`@claude`)·tmux pane id와 agent의 상태·요약·최근 프롬프트/응답과 마지막 프롬프트 시각을 얹으며, 가장 최근에 프롬프트를 보낸 pane은 따로 표시함. 숫자 키로 이동. |
| `muxa watch` | agent/pane 관측, prompt, live preview, hierarchy-aware mailbox와 table/sequence 협업을 제공하는 기본 TUI. |
| `muxa dashboard` | Work 중심 TUI. `P`는 선택 Work의 모든 live agent에 prompt를 보내고 `A`는 모두 중단. |
| `muxa attend` | input/choice/error로 가장 오래 막힌 agent로 점프. |
| `muxa stats` / `muxa report` | prompt history, agent 상태 시간, tmux foreground, human thinking 시간 분석. |
| `muxa timeline` | agent 작업/대기/error, human interaction, tmux foreground를 full-screen TUI timeline으로 표시. |
| `muxa activity` | stats/report에 들어간 raw duration ledger 조회. |
| BarShelf widget (macOS) | active/working/waiting/error agent를 메뉴바 popover에서 요약. |
| Dashboard | optional loopback HTTP UI + SSE live update + timeline 및 collaboration node-edge/sequence graph. |
| Notifications | agent가 attention을 필요로 할 때 desktop alert. |

## Muxa 설치

체험 후 계속 사용하기로 했다면 아래 방법 중 하나로 설치합니다.

필요 조건: tmux 3.x(또는 herdr), Unix-like OS.

Homebrew(프리빌트 바이너리, Rust 툴체인 불필요):

```bash
brew install open330/tap/muxa
muxa init
```

또는 원샷 설치 스크립트(소스 빌드, Rust 1.89+ 필요):

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh
```

source에서 설치:

```bash
git clone https://github.com/Open330/muxa.git
cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa-cli --locked
muxa init
```

확인:

```bash
muxa daemon start
muxa daemon status
muxa status
muxa watch
```

### `muxa watch`에서 바로 협업합니다

기억할 규칙은 하나입니다. **tmux window 하나가 협업 room 하나**입니다.
watch/dashboard에서 사람이 보낸 메시지는 operator console이 발신자가 되고, 선택한
agent가 수신자가 됩니다. agent가 MCP나 CLI로 보낸 요청은 해당 agent가 발신자입니다.

최초 한 번만 `~/.config/muxa/config.toml`에 다음 설정을 넣고 `muxad`를 재시작한
뒤 `muxa init`으로 `prefix+s` watch popup을 설치합니다.

```toml
[collaboration]
enabled = true
wake = "idle_only"
# 기본값: operator 본문은 직접 전달하고 agent 발신 본문은 mailbox에 둡니다.
wake_payload = "operator_full"
```

두 agent가 메시지를 직접 읽고 답할 수 있도록 MCP도 한 번 등록한 뒤 실행 중인
agent를 다시 시작합니다.

```bash
claude mcp add --scope user muxa -- muxa mcp
codex mcp add muxa -- muxa mcp
```

연결된 agent에는 같은 room의 peer를 read-only reviewer 또는 좁은 범위의 실행
subagent로 활용하라는 협업 지침이 자동으로 노출됩니다. 요청과 응답에는 검증된
AIR 1.0 artifact 참조를 첨부할 수 있고, watch mailbox가 profile별 색상 배지로
표시합니다.

연결된 Claude 또는 Codex 대화 안에서 바로 동료를 호출할 수도 있습니다. agent가
다음 표현을 Muxa의 durable peer-call 도구로 변환합니다.

```text
@peer 현재 변경사항을 리뷰해줘
@codex /review-plan-feedback commit abc123을 context로 사용해줘
@peer의 보고를 요약하고 타당한 조언만 반영해줘
```

`@peer`와 `@muxa-peer`는 Muxa 협업 전용 표현입니다. 새 요청은 peer-call 도구로,
기존 peer 보고는 durable mailbox 보고 도구로 연결되며 명시적인 PR 참조 없이는
GitHub PR을 의미하지 않습니다. `@peer`는 같은 window의 정상 agent를 결정적으로
선택하고, `@claude`, `@codex`,
`@gemini`, `@alias`, `role:name`으로 대상을 좁힐 수 있습니다. 기본 계약은
`REVIEW · READ-ONLY`입니다. 변경 실행은 명시적인 task 승인, 새 agent pane 생성은
별도 확인이 있어야 합니다. Muxa를 업그레이드하거나 등록된 스킬을 바꾼 뒤에는
실행 중이던 agent를 재시작해야 MCP process가 새 도구와 템플릿을 읽습니다.

`muxa doctor`에서 synthetic으로 표시되는 agent는 안정적인 session identity가
생길 때까지 협업 대상에서 제외됩니다. 새 prompt로 hook을 발생시키거나 agent를
재시작한 뒤 다시 확인하세요.

평소에는:

1. 같은 tmux window의 두 pane에서 agent를 각각 실행합니다.
2. 메시지를 보낼 agent pane을 선택하고 `prefix+s`를 누릅니다.
3. watch에서 상대 agent를 선택하고 `m`을 눌러 메시지를 보냅니다. 받은 메시지와
   응답은 `M` mailbox에서 확인합니다(`b`는 alias로 유지됩니다).

window에서 `M`을 누르면 room 전체를, session에서는 모든 window를 window별로 묶은
read-only 이력을 봅니다. collaboration 화면의 `v`는 최신순 table과 시간순 sequence를
전환하며 `muxa watch --screen collab --collab-layout sequence`로 바로 열 수 있습니다.
Web dashboard에는 room을 넘나드는 node-edge graph, sequence drill-down, filter, cursor
pagination이 있습니다. Durable history는 `collaboration.sqlite3`에 index되고 기존
`collaboration.json`은 한 번 import한 뒤 직접 archive/remove할 때까지 migration
backup이자 중복 사본으로 남습니다.

일반 shell pane에서 watch를 열어도 operator console이 발신자이므로 메시지를 보낼 수
있습니다. 설정과 응답 흐름은
[docs/COLLABORATION.ko.md](docs/COLLABORATION.ko.md)를 참고하세요.

설치 모드, `muxa init` preset, systemd, 수동 hook wiring, rollback은
[docs/INSTALL.ko.md](docs/INSTALL.ko.md)에 정리했습니다.

## 핵심 명령어

기본 tmux 운영 정책은 workspace 실행 context를 session에, Work의 활성 Run을
window에, Agent session을 pane에 연결합니다. `muxa onboard`는 이 mapping을 전용
sandbox에서 실행하는 16단계 live tour입니다. 실제 session과 window를 만들고 tree를
확인하며 detach/attach를 거친 뒤 pane을 나누고 scripted agent를 시작합니다. 이어
실제 `muxa watch`, `muxa attend`, `muxa msg`를 실제 sandbox mailbox에 사용합니다.
narration은 tmux와 Muxa state를 관찰해 진행하며 key를 가로채지 않습니다. 한국어
locale에서는 한글을 자동 선택하고 `--lang ko`로 명시하거나 도중 `F2`로 전환할 수
있습니다. `--print`는 tmux를 시작하지 않고 같은 16단계 written guide를 출력합니다.

| Command | 목적 |
| --- | --- |
| `muxa status [--json]` | 추적 중인 agent 테이블 또는 desktop integration용 versioned JSON snapshot. |
| `muxa watch [--view session\|window\|pane]` | workspace → work → agent live TUI picker/dashboard. |
| `muxa dashboard [--since today]` | Run capture와 agent별/Work 일괄 prompt·abort, ACT/WACT total을 보여주는 Work-card TUI. |
| `muxa attend [--cycle] [--list]` | attention이 필요한 agent로 focus 또는 list. |
| `muxa status-line [--pane %N]` | tmux status-line 출력. |
| `muxa peek [--plain]` | 현재 tmux window의 pane별 오버레이. `--plain`은 텍스트로 출력. |
| `muxa recap [--pane %N]` | 보관된 disk history에서 최근 prompt 조회. |
| `muxa peers` / `muxa identity` / `muxa msg` | 같은 tmux window의 agent를 찾고 이름/역할을 지정해 durable request/reply 메시지를 주고받음. |
| `muxa skill add/list/show/remove` | watch/dashboard 메시지, watch ask, MCP peer call에서 사용할 `/` prompt 템플릿 관리. |
| `muxa host add/list/label/annotate/doctor` | physical SSH node와 Kubernetes-style label/annotation 관리. |
| `muxa fleet status/watch/capture/send/attach` | 중앙 host → session → window → pane(agent) 관측/제어. |
| `muxa stats --since today` | WACT/ACT/WORK/WAIT 중심 summary. day/project/agent/session 단위로 묶을 수 있고, `--graph`는 WACT 시간 그래프만, `--verbose`는 진단 컬럼을 표시. |
| `muxa report --since week` | day/project/agent/session 모든 breakdown을 ACT/WACT 중심 테이블로 표시 (`--json` / `--markdown`로 export). |
| `muxa timeline --since today` | session별로 묶은 interactive timeline. `--session main`, `--agent codex`로 필터링하고 `--sort waiting` 정렬이나 `--view heatmap`을 사용할 수 있음. |
| `muxa activity --type agent\|tmux\|human` | raw activity ledger interval 조회. |
| `muxa sync` | tmux pane scan으로 registry backfill. |
| `muxa agent start --agent codex [--host auto\|native\|tmux]` | allowlist agent 실행. `auto`는 실제 tmux 셸에서는 tmux를, 일반 터미널에서는 muxa-owned PTY를 사용. |
| `muxa work start muxa-onboarding --workspace muxa --agent codex ...` | workspace session과 work window를 만들거나 재사용하고 agent pane을 추가. |
| `muxa workspace list/show/close` | workspace/project session을 조회하거나 명시적으로 종료. |
| `muxa work list/show/close [--workspace muxa]` | Work와 현재 Run binding을 조회하거나 Run window를 명시적으로 종료. |
| `muxa agent start --host tmux --workspace muxa --work muxa-onboarding ...` | allowlist agent pane을 managed tmux Work window에 추가. MCP에서는 `muxa_start_agent`로 제공. |
| `muxa agent control (--pane %N\|--session pty-N) --action interrupt` | managed tmux pane 또는 muxa-owned PTY agent session 하나를 중단하거나 명시적으로 종료. |
| `muxa onboard [--tour live] [--lang auto\|en\|ko]` | 전용 sandbox에서 실제 tmux, watch, attend, mailbox를 다루는 16단계 tour. 기존 tmux 안에서는 실행을 거부합니다. `F2` 언어 전환, `--no-quiz`의 즉시 `F12` 제공, `--print` guide를 지원합니다. |
| `muxa mcp` | coding agent가 상태 확인, 메시지, pane capture, 변경 대기, tmux lifecycle을 Muxa를 통해 수행하는 MCP stdio server. [docs/MCP.md](docs/MCP.md) 참고. |
| `muxa init` | install/uninstall wizard. |
| `muxad` | daemon process. |

자주 쓰는 stats query:

```bash
muxa stats --since today --group-by session
muxa stats --since yesterday --group-by project
muxa report --since last-week
muxa timeline --since today --session main
muxa timeline --since today --exclude-session 'monitor*'
muxa stats --since month --exclude-pane '%42' --exclude-session 'monitor*'
muxa timeline --since today --group-by kind --sort waiting
muxa timeline --view heatmap --since 12w
muxa timeline --day 2026-06-06
muxa activity --since today --type human
```

`--since`는 `today`, `yesterday`, 최근 7일 rolling window인 `week`, 최근
30일 rolling window인 `month`, 저번주 월요일-일요일 구간인 `last-week` /
`"last week"`, 이전 달력 월인 `last-month` / `"last month"`, `24h`/`7d`/`4w`
같은 rolling duration, `2026-06-06` 같은 local date, RFC3339 timestamp,
`all`을 받습니다. `HUMAN`, `THINK`, `ACT`를 포함한
ledger 판정 기준은 [docs/ACTIVITY.ko.md](docs/ACTIVITY.ko.md)에 있습니다.

계속 켜둔 monitoring scope는 `muxa stats`, `muxa report`, `muxa timeline`에서
`--exclude-pane`, `--exclude-session`으로 제외할 수 있습니다. 패턴은
case-sensitive이고 `*`, `?` wildcard를 지원합니다. 예:
`--exclude-session 'monitor*'`.

## 지원 에이전트

**훅 기반(authoritative).** 기존 훅/이벤트 시스템에 연결해 정확한 상태
전이를 받습니다:

| Agent | 상태 | Config |
| --- | --- | --- |
| Claude Code | 지원 | `~/.claude/settings.json` |
| OpenAI Codex | 지원 | `~/.codex/config.toml` |
| Google Gemini CLI | 지원 | `~/.gemini/settings.json` |
| Google Antigravity CLI (`agy`) | 지원 | `~/.gemini/config/hooks.json` — [문서](docs/ANTIGRAVITY.md) |
| opencode | 예정 | [tracking issue](https://github.com/Open330/muxa/issues/14) |

**화면 감지(fallback).** 훅이 없는 에이전트는 pane 내용을 TOML 매니페스트로
분류합니다 — `agy`, `cursor-agent`, `amp`, `copilot`, `aider`, `goose`용
매니페스트가 기본 번들로 제공되며 사용자가 확장 가능. 훅이 있으면 훅이 우선하되
예외가 하나 있습니다: `agy`는 승인 프롬프트에 대한 훅이 없어서 그 신호만은
화면 감지가 계속 담당합니다.
[docs/SCREEN_DETECTION.md](docs/SCREEN_DETECTION.md) 참고.

## Fleet host

로컬 muxad를 현재 장비와 여러 SSH host의 중앙 controller로 사용할 수 있습니다.
controller 장비는 별도 Fleet 설정 없이 즉시 `local` node로 나타납니다. 각 physical node는
stable UUID와 label/annotation metadata를 가지며 controller는 node마다 persistent outbound
SSH stdio relay와 독립적인 last-known cache를 유지합니다. 기본값은 `observe`이고
`control`은 host별로 명시해야 합니다. 원격 TCP listener는 열지 않습니다.

```bash
muxa fleet status                         # local은 이미 표시됨
muxa fleet status -L environment,region   # 필요한 label column만 추가
muxa fleet status -o wide                 # 공간에 따라 hostname/version/latency
muxa host label local environment=development
muxa host add dev muxa-devbox --label environment=development --mode observe
muxa host doctor dev
muxa fleet watch
# 같은 진입점: muxa watch --fleet
# `muxa init`은 이 화면을 tmux prefix+S에 연결함(local watch는 prefix+s 유지)
# local 하나뿐이면 불필요한 host row 없이 완전한 native watch 사용
```

selector, TUI key, 보안/성능, MCP/dashboard API는
[docs/FLEET.ko.md](docs/FLEET.ko.md)를 참고하세요.

## Pane backend

muxa는 여러 터미널 멀티플렉서 백엔드에서 에이전트를 관측하며, 여러 호스트를
동시에 볼 수 있습니다(tmux→herdr 마이그레이션 등):

| 호스트 | 상태 | 비고 |
| --- | --- | --- |
| tmux | 전체 | 기본 백엔드. |
| [herdr](https://herdr.dev) | 전체 | herdr 소켓 API 경유; [docs/HERDR.md](docs/HERDR.md). |
| zellij | CLI 기본 | 플러그인 경로 예정; [docs/ZELLIJ.md](docs/ZELLIJ.md). |

여러 호스트 동시 관측은 [docs/MULTI_HOST.md](docs/MULTI_HOST.md) 참고.

## 상세 문서

| Topic | Doc |
| --- | --- |
| 설치와 wiring | [docs/INSTALL.ko.md](docs/INSTALL.ko.md) |
| 온보딩과 work/agent 운영 정책 | [docs/ONBOARDING.ko.md](docs/ONBOARDING.ko.md) |
| MCP control plane (`muxa mcp`) | [docs/MCP.md](docs/MCP.md) |
| Physical SSH fleet | [docs/FLEET.ko.md](docs/FLEET.ko.md) |
| Live TUI와 prompt composer | [docs/WATCH.ko.md](docs/WATCH.ko.md) |
| CLI dashboard | [docs/DASHBOARD_CLI.ko.md](docs/DASHBOARD_CLI.ko.md) |
| Stats, report, activity ledger | [docs/ACTIVITY.ko.md](docs/ACTIVITY.ko.md) |
| Timeline TUI와 dashboard graph | [docs/TIMELINE.ko.md](docs/TIMELINE.ko.md) |
| 설정 레퍼런스 | [docs/CONFIGURATION.ko.md](docs/CONFIGURATION.ko.md) |
| Web dashboard | [docs/DASHBOARD.md](docs/DASHBOARD.md) |
| External sinks | [docs/SINKS.md](docs/SINKS.md) |
| Zellij 계획 | [docs/ZELLIJ.md](docs/ZELLIJ.md) |
| Architecture와 개발 | [docs/ARCHITECTURE.ko.md](docs/ARCHITECTURE.ko.md) |
| Agent 간 협업 | [docs/COLLABORATION.ko.md](docs/COLLABORATION.ko.md) |

## 개발

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## 라이선스

MIT OR Apache-2.0.
