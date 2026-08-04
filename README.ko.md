<div align="center">

<img src="assets/logo.svg" alt="muxa" width="260" />

**tmux 안의 AI agent CLI를 관측하고 조작하는 로컬 도구.**

에이전트가 working, waiting, idle, error 중 어디에 있는지 tmux 상태바,
실시간 TUI, 데스크톱 알림, 로컬 리포트에서 확인합니다.

[![CI](https://github.com/Open330/muxa/actions/workflows/ci.yml/badge.svg)](https://github.com/Open330/muxa/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-informational)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-beta-yellow)

[English](README.md) · **한국어**

</div>

---

`muxa`는 terminal multiplexer pane 안에서 실행되는 AI coding agent를 관측하는
작은 daemon + CLI입니다. Claude Code, OpenAI Codex, Google Gemini CLI의 기존
hook/event 시스템을 사용하고, 이를 tmux pane/session과 연결합니다.

tmux나 agent binary를 fork하지 않습니다. tmux는 기본 full backend이고,
zellij는 CLI baseline과 richer plugin 경로를 준비 중입니다.

<div align="center">
  <img src="docs/demo.gif" alt="muxa demo" width="900" />
</div>

> [!IMPORTANT]
> Beta입니다. event ingest, daemon, CLI, live TUI, desktop notification,
> stats/report는 end-to-end로 동작하지만 1.0 전까지 API가 바뀔 수 있습니다.

## 핵심 기능

| Surface | 기능 |
| --- | --- |
| `muxa status-line` | active pane 기준 tmux `status-right` 한 줄 요약. |
| `muxa watch` | agent/pane 실시간 TUI. attach, prompt composer, live preview 포함. |
| `muxa dashboard` | pane 조작과 같은 tmux window의 agent 협업을 제공하는 session-card TUI. |
| `muxa attend` | input/choice/error로 가장 오래 막힌 agent로 점프. |
| `muxa stats` / `muxa report` | prompt history, agent 상태 시간, tmux foreground, human thinking 시간 분석. |
| `muxa timeline` | agent 작업/대기/error, human interaction, tmux foreground를 full-screen TUI timeline으로 표시. |
| `muxa activity` | stats/report에 들어간 raw duration ledger 조회. |
| BarShelf widget (macOS) | active/working/waiting/error agent를 메뉴바 popover에서 요약. |
| Dashboard | optional loopback HTTP UI + SSE live update + timeline graph. |
| Notifications | agent가 attention을 필요로 할 때 desktop alert. |

## 빠른 시작

필요 조건: tmux 3.x(또는 herdr), Unix-like OS.

Homebrew(프리빌트 바이너리, Rust 툴체인 불필요):

```bash
brew install open330/tap/muxa
muxa init
```

또는 원샷 설치 스크립트(소스 빌드, Rust 1.88+ 필요):

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
muxad &
muxa status
muxa watch
```

### Agent 협업은 세 단계입니다

기억할 규칙은 하나입니다. **tmux window 하나가 협업 room 하나**입니다.
Dashboard를 열 때 선택되어 있던 agent가 발신자가 됩니다.

최초 한 번만 `~/.config/muxa/config.toml`에 다음 설정을 넣고 `muxad`를 재시작한
뒤 `muxa init`으로 `prefix+D` popup을 설치합니다.

```toml
[collaboration]
enabled = true
wake = "idle_only"
```

평소에는:

1. 같은 tmux window의 두 pane에서 agent를 각각 실행합니다.
2. 메시지를 보낼 agent pane을 선택하고 `prefix+D`를 누릅니다.
3. `Tab`으로 상대를 고른 뒤 `m`을 누르고 메시지를 작성해 `Enter`로 보냅니다.

일반 shell pane에서 Dashboard를 열면 그 shell은 agent가 아니므로 협업할 수
없습니다. 설정과 응답 흐름은
[docs/COLLABORATION.ko.md](docs/COLLABORATION.ko.md)를 참고하세요.

설치 모드, `muxa init` preset, systemd, 수동 hook wiring, rollback은
[docs/INSTALL.ko.md](docs/INSTALL.ko.md)에 정리했습니다.

## 핵심 명령어

| Command | 목적 |
| --- | --- |
| `muxa status [--json]` | 추적 중인 agent 테이블 또는 desktop integration용 versioned JSON snapshot. |
| `muxa watch [--view pane\|session]` | live TUI picker/dashboard. |
| `muxa dashboard [--since today]` | live capture, prompt composer, abort/terminate action, ACT/WACT total을 보여주는 session-card TUI console. |
| `muxa attend [--cycle] [--list]` | attention이 필요한 agent로 focus 또는 list. |
| `muxa status-line [--pane %N]` | tmux status-line 출력. |
| `muxa recap [--pane %N]` | 보관된 disk history에서 최근 prompt 조회. |
| `muxa peers` / `muxa identity` / `muxa msg` | 같은 tmux window의 agent를 찾고 이름/역할을 지정해 durable request/reply 메시지를 주고받음. |
| `muxa stats --since today` | WACT/ACT/WORK/WAIT 중심 summary. day/project/agent/session 단위로 묶을 수 있고, `--graph`는 WACT 시간 그래프만, `--verbose`는 진단 컬럼을 표시. |
| `muxa report --since week` | day/project/agent/session 모든 breakdown을 ACT/WACT 중심 테이블로 표시 (`--json` / `--markdown`로 export). |
| `muxa timeline --since today` | session별로 묶은 interactive timeline. `--session main`, `--agent codex`로 필터링하고 `--sort waiting` 정렬이나 `--view heatmap`을 사용할 수 있음. |
| `muxa activity --type agent\|tmux\|human` | raw activity ledger interval 조회. |
| `muxa sync` | tmux pane scan으로 registry backfill. |
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
| opencode | 예정 | [tracking issue](https://github.com/Open330/muxa/issues/14) |

**화면 감지(fallback).** 훅이 없는 에이전트는 pane 내용을 TOML 매니페스트로
분류합니다 — `cursor-agent`, `amp`, `copilot`, `aider`, `goose`용 매니페스트가
기본 번들로 제공되며 사용자가 확장 가능. 훅이 있으면 항상 훅이 우선합니다.
[docs/SCREEN_DETECTION.md](docs/SCREEN_DETECTION.md) 참고.

## 호스트

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
