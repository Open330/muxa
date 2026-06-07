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
| `muxa watch` | agent/pane 실시간 TUI. attach와 prompt composer 포함. |
| `muxa attend` | input/choice/error로 가장 오래 막힌 agent로 점프. |
| `muxa stats` / `muxa report` | prompt history, agent 상태 시간, tmux foreground, human thinking 시간 분석. |
| `muxa timeline` | agent 작업/대기/error, human interaction, tmux foreground를 full-screen TUI timeline으로 표시. |
| `muxa activity` | stats/report에 들어간 raw duration ledger 조회. |
| Dashboard | optional loopback HTTP UI + SSE live update + timeline graph. |
| Notifications | agent가 attention을 필요로 할 때 desktop alert. |

## 빠른 시작

필요 조건: Rust 1.88+, tmux 3.x, Unix-like OS.

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

설치 모드, `muxa init` preset, systemd, 수동 hook wiring, rollback은
[docs/INSTALL.ko.md](docs/INSTALL.ko.md)에 정리했습니다.

## 핵심 명령어

| Command | 목적 |
| --- | --- |
| `muxa status` | 추적 중인 agent 테이블. |
| `muxa watch [--view pane\|session]` | live TUI picker/dashboard. |
| `muxa attend [--cycle] [--list]` | attention이 필요한 agent로 focus 또는 list. |
| `muxa status-line [--pane %N]` | tmux status-line 출력. |
| `muxa recap [--pane %N]` | 보관된 disk history에서 최근 prompt 조회. |
| `muxa stats --since today` | day/project/agent/session 단위 summary. |
| `muxa report --since week` | Markdown stats report. |
| `muxa timeline --since today` | session별로 묶은 interactive timeline. `--session main`, `--agent codex`로 필터링하고 `--sort waiting` 정렬이나 `--view heatmap`을 사용할 수 있음. |
| `muxa activity --type agent\|tmux\|human` | raw activity ledger interval 조회. |
| `muxa sync` | tmux pane scan으로 registry backfill. |
| `muxa init` | install/uninstall wizard. |
| `muxad` | daemon process. |

자주 쓰는 stats query:

```bash
muxa stats --since today --group-by session
muxa stats --since yesterday --group-by project
muxa report --since week
muxa timeline --since today --session main
muxa timeline --since today --group-by kind --sort waiting
muxa timeline --view heatmap --since 12w
muxa timeline --day 2026-06-06
muxa activity --since today --type human
```

`--since`는 `today`, `yesterday`, `week`, `24h`/`7d`/`4w` 같은 rolling
duration, `2026-06-06` 같은 local date, RFC3339 timestamp, `all`을 받습니다. `HUMAN`, `THINK`를 포함한
ledger 판정 기준은 [docs/ACTIVITY.ko.md](docs/ACTIVITY.ko.md)에 있습니다.

## 지원 에이전트

| Agent | 상태 | Config |
| --- | --- | --- |
| Claude Code | 지원 | `~/.claude/settings.json` |
| OpenAI Codex | 지원 | `~/.codex/config.toml` |
| Google Gemini CLI | 지원 | `~/.gemini/settings.json` |
| opencode | 예정 | [tracking issue](https://github.com/Open330/muxa/issues/14) |

## 상세 문서

| Topic | Doc |
| --- | --- |
| 설치와 wiring | [docs/INSTALL.ko.md](docs/INSTALL.ko.md) |
| Live TUI와 prompt composer | [docs/WATCH.ko.md](docs/WATCH.ko.md) |
| Stats, report, activity ledger | [docs/ACTIVITY.ko.md](docs/ACTIVITY.ko.md) |
| Timeline TUI와 dashboard graph | [docs/TIMELINE.ko.md](docs/TIMELINE.ko.md) |
| 설정 레퍼런스 | [docs/CONFIGURATION.ko.md](docs/CONFIGURATION.ko.md) |
| Web dashboard | [docs/DASHBOARD.md](docs/DASHBOARD.md) |
| External sinks | [docs/SINKS.md](docs/SINKS.md) |
| Zellij 계획 | [docs/ZELLIJ.md](docs/ZELLIJ.md) |
| Architecture와 개발 | [docs/ARCHITECTURE.ko.md](docs/ARCHITECTURE.ko.md) |

## 개발

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## 라이선스

MIT OR Apache-2.0.
