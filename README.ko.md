<div align="center">

<img src="assets/logo.svg" alt="muxa" width="260" />

**tmux를 위한 에이전트 CLI 관측 및 오케스트레이션 레이어.**

어떤 에이전트가 작업 중인지, 입력을 기다리는지, 유휴 상태인지 — tmux 상태바나 풀스크린 대시보드에서 한눈에 확인하세요.

[![CI](https://github.com/Open330/muxa/actions/workflows/ci.yml/badge.svg)](https://github.com/Open330/muxa/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-informational)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-pre--alpha-orange)
![tests](https://img.shields.io/badge/tests-92%20green-brightgreen)

[English](README.md) · **한국어**

</div>

---

`muxa`는 tmux 페인 안에서 실행되는 에이전트 CLI들 — **Claude Code, OpenAI
Codex, Google Gemini CLI, opencode** — 을 감시하고, 그 상태를 tmux 상태바, 실시간
TUI 대시보드, 데스크톱 알림, 그리고 가벼운 CLI로 노출해주는 작은 데몬입니다.

tmux를 포크하지 않습니다. tmux와는 tmux CLI를 통해, 각 에이전트와는 그 에이전트
고유의 훅 / 이벤트 시스템을 통해 통신합니다.

<div align="center">
  <img src="docs/demo.gif" alt="muxa demo — status, status-line, watch" width="900" />
</div>

```text
┌─ tmux status-right ──────────────────────────────────────────────────┐
│   ...   │ ⚙ main:2 claude_code │ ! work:1 codex │ · review:0 gemini_cli │
└─────────┼──────────────────────┼────────────────┼──────────────────────┘
          │                      │                └─ 유휴
          │                      └─ 입력 대기 중
          └─ 작업 중
```

> [!IMPORTANT]
> 프리알파 단계입니다. 이벤트 인제스트, 어댑터, 데몬, CLI, 실시간 TUI, 데스크톱
> 알림이 모두 엔드투엔드로 동작하며 92개 테스트가 통과합니다. API는 아직 변경될
> 수 있습니다. opencode 지원은 보류 중입니다.

## 목차

- [에이전트로 빠르게 시작하기](#에이전트로-빠르게-시작하기)
- [기능](#기능)
- [지원 에이전트](#지원-에이전트)
- [설치](#설치)
- [빠른 시작](#빠른-시작)
- [명령어](#명령어)
- [실시간 TUI](#실시간-tui)
- [웹 대시보드](#웹-대시보드)
- [데스크톱 알림](#데스크톱-알림)
- [설정](#설정)
- [아키텍처](#아키텍처)
- [개발](#개발)
- [라이선스](#라이선스)

## 기능

|                          |                                                                                  |
| ------------------------ | -------------------------------------------------------------------------------- |
| **범용 에이전트**         | 데몬 하나, CLI 하나, 어댑터 4개 (Claude · Codex · Gemini · opencode [†]).         |
| **tmux 네이티브**        | `$TMUX_PANE`으로 페인을 식별하고, 출력은 `session:window.pane` 형식으로 라벨링. |
| **무결합**               | tmux나 에이전트 CLI에 어떠한 변경도 가하지 않음 — 기존 훅 시스템만 활용.          |
| **실시간 TUI**            | `muxa watch` — 에이전트가 상단, 그 외 tmux 페인이 하단, 2 Hz 갱신, 컬럼 설정 가능. |
| **웹 대시보드**           | 옵트인 HTTP UI + SSE — 모든 에이전트와 **이 머신의 모든 tmux 페인**을 한 탭에. [`docs/DASHBOARD.md`](docs/DASHBOARD.md) 참고. |
| **데스크톱 알림**         | 옵트인 방식의 libnotify / 네이티브 토스트 — `WaitingInput` / `Error` 전이 시.   |
| **기본값으로 안전**       | 소켓은 `0600` 권한, 대시보드는 두 플래그를 켜기 전까진 루프백 전용, `SIGTERM` 시 드레인, `unsafe_code = forbid`. |
| **버전 관리되는 프로토콜**| 명시적인 `PROTOCOL_VERSION`, 호환되지 않는 클라이언트는 거부.                    |
| **빠름**                  | 인메모리 레지스트리 — DB나 외부 서비스 의존 없음.                                |

<sub>[†] opencode 어댑터는 보류 중입니다 — SSE / 인프로세스 플러그인 기반이라
셸 훅 방식이 아니기 때문입니다.</sub>

## 지원 에이전트

| 에이전트            | 통합 방식                                          | 설정 파일                   |
| ------------------- | -------------------------------------------------- | --------------------------- |
| Claude Code         | ✓ 셸 훅 + status-line Heartbeat                    | `~/.claude/settings.json`   |
| OpenAI Codex        | ✓ 셸 훅 (Claude 프로토콜 클론, 업스트림)           | `~/.codex/config.toml`      |
| Google Gemini CLI   | ✓ 셸 훅 (Claude 호환, 업스트림)                    | `~/.gemini/settings.json`   |
| opencode            | 보류 — SSE 구독 / TS 플러그인 예정                 | —                           |

## 에이전트로 빠르게 시작하기

아래 프롬프트를 tmux 페인에서 동작 중인 AI 코딩 에이전트(Claude Code, Codex,
Gemini CLI 등)에 그대로 붙여넣으세요. `muxa`를 설치하고, 현재 에이전트의 훅
설정을 연결하고, `~/.tmux.conf`까지 손봐서 그 페인이 몇 초 안에 상태를 보고하기
시작합니다.

<div><img src="https://quickstart-for-agents.vercel.app/api/header.svg?theme=claude-code&mascot=thinking&title=install+muxa&lang=Agents" width="100%" /></div>

```text
You're helping install muxa (https://github.com/Open330/muxa), an agent
CLI observability layer for tmux. Requirements: Rust 1.88+, tmux 3.x.

1) Clone and install binaries
   git clone https://github.com/Open330/muxa.git /tmp/muxa
   cargo install --path /tmp/muxa/crates/muxad --locked
   cargo install --path /tmp/muxa/crates/muxa  --locked

2) Start the daemon (foreground is fine; detach with `&` or systemd)
   muxad &

3) Wire the CLI you're running under (detect from $0 / process tree)
   - Claude Code: merge /tmp/muxa/examples/claude-settings.json into
     ~/.claude/settings.json. Do NOT overwrite existing hooks — jq-append
     to each hooks.<event> array.
   - Codex:       append the [[hooks.*]] blocks from
                  crates/muxa-adapters/src/codex.rs module doc to
                  ~/.codex/config.toml.
   - Gemini CLI:  merge the hooks block from
                  crates/muxa-adapters/src/gemini.rs module doc into
                  ~/.gemini/settings.json.

4) Wire tmux (append if not already present)
   set -g status-interval 2
   set -g status-right "#(muxa status-line --pane #{pane_id}) | %H:%M"
   tmux source-file ~/.tmux.conf

5) Verify. The current pane should appear in both outputs below.
   muxa status
   muxa status-line --pane $TMUX_PANE

Rollback: every file edited above was backed up to <file>.muxa-backup-<ts>;
kill muxad with pkill, restore backups, tmux source-file to reload.
```

<div><img src="https://quickstart-for-agents.vercel.app/api/footer.svg?theme=claude-code&tokens=0.4k&model=Any+agent" width="100%" /></div>

직접 설치하고 싶으신가요? 계속 읽어주세요.

## 설치

**Rust 1.88+**, **tmux 3.x**, 그리고 유닉스 계열 OS가 필요합니다.

<details open>
<summary><strong>소스에서 빌드</strong></summary>

```bash
git clone https://github.com/Open330/muxa.git && cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa  --locked
```

`~/.cargo/bin/`에 설치됩니다. 해당 경로가 `PATH`에 포함되어 있는지 확인하세요.

</details>

<details>
<summary><strong>사전 빌드된 바이너리</strong></summary>

자신의 플랫폼에 맞는 아카이브를
[Releases 페이지](https://github.com/Open330/muxa/releases)에서 받아 `muxa`와
`muxad`를 `PATH`에 포함된 디렉터리에 복사하면 됩니다.

빌드 대상 플랫폼:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

</details>

## 빠른 시작

### 1. 데몬 실행

```bash
muxad
```

또는 systemd user 서비스로 실행 — [`examples/muxad.service`](examples/muxad.service)
참고:

```bash
mkdir -p ~/.config/systemd/user
cp examples/muxad.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now muxad.service
```

### 2. 에이전트 연결

Claude Code — [`examples/claude-settings.json`](examples/claude-settings.json)을
`~/.claude/settings.json`에 병합합니다.

<details>
<summary>이미 <code>ccstatusline</code>(또는 다른 statusLine 도구)을 쓰고 계신가요?</summary>

Claude Code는 `statusLine.command`를 하나만 실행하므로, 기본 설정으로는 muxa를
`ccstatusline` 위에 겹쳐 쓸 수 없습니다. `--forward`를 사용해서 status-line
JSON을 기존 도구로 흘려보내세요 — muxa는 Heartbeat(모델, 컨텍스트 %, 비용)를
가로채고, 같은 JSON을 포워딩 대상 명령어에 그대로 파이프하며, 그 stdout과 종료
코드를 수정 없이 통과시킵니다:

```json
"statusLine": {
  "type": "command",
  "command": "muxa hook claude-statusline --forward 'npx -y ccstatusline@latest'",
  "refreshInterval": 5
}
```

전체 예시는
[`examples/claude-settings-with-ccstatusline.json`](examples/claude-settings-with-ccstatusline.json)을
참고하세요. 포워딩 대상 명령은 `/bin/sh -c`로 실행되므로 어떤 셸 한 줄짜리
명령이든 동작합니다. 데몬이 죽어 있더라도 muxa는 평소대로 포워딩을 계속합니다 —
훅 경로는 베스트에포트 방식입니다.

</details>

Codex와 Gemini CLI도 같은 패턴이며, 설정 파일만 다릅니다.
각 어댑터의 모듈 레벨 문서는 `crates/muxa-adapters/src/`에 있습니다.

### 3. tmux 연결

`~/.tmux.conf`에 아래 내용을 추가하거나
[`examples/muxa.tmux.conf`](examples/muxa.tmux.conf)를 `source-file`로 불러오세요:

```tmux
# 우측 상태바에 페인별 에이전트 글리프
set -g status-interval 2
set -g status-right "#(muxa status-line --pane #{pane_id}) | #[fg=white]%H:%M"

# 기본 세션 스위처를 에이전트 인지형 팝업으로 교체.
# 행에서 Enter를 누르면 그 페인에 어태치되고, 종료 시 팝업이 닫힙니다.
bind-key s display-popup -E -w 90% -h 85% "muxa watch"
```

리로드: `tmux source-file ~/.tmux.conf`.

> [!TIP]
> `prefix + s`가 이제 `muxa watch`를 플로팅 팝업으로 띄웁니다 — tmux 내장
> `choose-tree`를 그대로 대체하면서, 실시간 에이전트 상태까지 함께 보여줍니다.
> 기본 윈도우/세션 트리가 필요하면 `prefix + w`로 여전히 열 수 있습니다.

### 4. 동작 확인

```bash
muxa status         # 사람이 읽기 쉬운 테이블
muxa watch          # 실시간 TUI
```

## 명령어

|                                            |                                                                          |
| ------------------------------------------ | ------------------------------------------------------------------------ |
| `muxa status`                              | 추적 중인 모든 에이전트를 사람이 읽기 쉬운 테이블로 출력.                |
| `muxa watch`                               | 풀스크린 실시간 TUI — [실시간 TUI](#실시간-tui) 참고.                    |
| `muxa status-line [--pane %N]`             | tmux `status-right`용 한 줄 출력 — 기본은 `$TMUX_PANE` 스코프.           |
| `muxa recap [--pane %N]`                   | 해당 페인의 마지막 프롬프트를 보여줌.                                    |
| `muxa sync`                                | tmux 페인을 스캔해 레지스트리를 백필 — [Sync](#sync) 참고.               |
| `muxa panes`                               | 디버그용: tmux 페인 목록 덤프.                                            |
| `muxa hook <agent> --event <e>`            | 훅 어댑터 진입점 — 에이전트 CLI가 직접 호출.                             |
| `muxa hook claude-statusline --forward CMD` | Claude의 status-line JSON을 muxa로 받으면서 다운스트림 도구로 포워딩.   |
| `muxad`                                    | 데몬 — 기본적으로 `$XDG_RUNTIME_DIR/muxa.sock`을 리슨.                   |

### Sync

`muxa sync`는 `tmux list-panes`를 훑어 `pane_current_command`를 알려진 에이전트
CLI(`claude`, `codex`, `gemini` / `gemini-cli`)와 매칭하고, 데몬에 합성 에이전트로
등록하도록 요청합니다. 동일한 일회성 패스가 `muxad` 기동 직후에도 자동으로
실행되므로 데몬을 재시작해도 페인에 살아 있는 에이전트가 사라지지 않습니다.
멱등(idempotent)이며, 합성 항목은 실제 훅이 도착하면 그 자리에서 교체됩니다.
`[discovery] enabled = false`로 끌 수 있습니다.

## 실시간 TUI

`muxa watch`는 추적 중인 모든 에이전트를 보여주는 풀스크린 대시보드를 열고
2 Hz로 갱신합니다. [ratatui](https://ratatui.rs)로 렌더링하며, 패닉이 나도
터미널은 깔끔하게 복원됩니다.

가장 좋은 사용법은 tmux 팝업을 통하는 것입니다(위 [tmux 연결](#3-tmux-연결)
참고). 어떤 페인에서든 `prefix + s`를 누르면 → 실시간 대시보드가 팝업으로 뜨고
→ 원하는 행에서 `Enter`를 누르면 → 팝업이 닫히면서 클라이언트가 그 페인으로
전환됩니다. 다시 `prefix + s`를 누르면 원래 자리로 돌아옵니다.

`muxa watch`를 일반 셸에서 직접 실행해도 됩니다 — 기존 tmux 세션에 어태치되며
Enter 동작도 동일합니다. 다만 muxa가 `switch-client` 대신
`tmux attach-session`을 호출합니다.

**키 바인딩**

| 키                    | 동작                                                                  |
| --------------------- | --------------------------------------------------------------------- |
| `↑` / `↓` / `k` / `j` | 선택 커서 이동.                                                        |
| `Enter`               | 선택한 페인에 어태치 (`tmux select-pane` + `switch-client`).           |
| `r`                   | 즉시 리프레시 강제.                                                    |
| `q` / `Esc`           | 종료.                                                                  |
| `Ctrl-C`              | 종료.                                                                  |

## 웹 대시보드

`muxad`에 얹은 read-only HTTP UI. tmux 상태바에 보이는 에이전트 + **이
머신의 모든 tmux 페인** (모든 tmux 서버 통합) 을 한 탭에서, SSE로 실시간
업데이트. 기본값은 OFF, 켜도 loopback 전용이 기본.

```bash
muxad --dashboard      # 이후 http://127.0.0.1:7878/ 접속
```

배포 형태 세 가지 — 누가 봐야 하느냐로 선택:

| 형태                          | 명령                                                                                                                  | URL                                       |
| ----------------------------- | --------------------------------------------------------------------------------------------------------------------- | ----------------------------------------- |
| **루프백, 토큰 없음**         | `muxad --dashboard`                                                                                                   | `http://127.0.0.1:7878/`                  |
| **루프백, 토큰 사용**         | `muxad --dashboard --dashboard-token "$TOK"`                                                                          | `http://127.0.0.1:7878/?token=$TOK`       |
| **LAN / 외부 노출**           | `muxad --dashboard --dashboard-bind 0.0.0.0:7878 --dashboard-token "$TOK" --allow-public`                             | `http://<host>:7878/?token=$TOK`          |

비-루프백 바인딩은 `--allow-public` **그리고** 비어있지 않은
`--dashboard-token` **둘 다** 없으면 startup에서 거절됩니다 — 호스트
바깥으로 인증 없는 소켓을 못 열게 막아둔 안전망입니다. `?token=` 쿼리
파라미터는 페이지 첫 로드에서 캡처되어 `sessionStorage`에 저장되고 URL
바에서는 제거됩니다. 이후 모든 요청에 `Authorization: Bearer …`로 자동
부착됩니다.

토큰 생성: `openssl rand -hex 32`. TLS가 필요하면 nginx/Caddy 같은 리버스
프록시를 앞에 — 의도적으로 v1 범위 밖입니다.

JSON / SSE 엔드포인트 (`/api/*`):

| Method | Path            | 내용                                                                |
| ------ | --------------- | ------------------------------------------------------------------- |
| GET    | `/api/health`   | `{ ok, version, protocol }`                                         |
| GET    | `/api/agents`   | 현재 `Store` 스냅샷                                                 |
| GET    | `/api/panes`    | 모든 readable tmux 소켓의 페인 목록 (TTL 캐시)                     |
| GET    | `/api/events`   | SSE: `snapshot` (초기), `transition` (라이브), `lagged` (드롭)      |

전체 운영 가이드 — 설정 레퍼런스, 보안 모델 설명, "global tmux" 동작
방식, SSE 와이어 포맷 — 은 [`docs/DASHBOARD.md`](docs/DASHBOARD.md).

## 데스크톱 알림

설정에서 옵트인하면 됩니다. `*→WaitingInput`, `*→Error`, 또는
`Working→Stopped`(작업 완료) 전이 시 `muxa`가 네이티브 알림을 띄웁니다 —
Linux는 libnotify, macOS는 NSUserNotification, Windows는 WinRT 토스트.
에이전트가 10분 동안 돌다가 드디어 사용자의 입력을 기다릴 때 유용합니다.

`~/.config/muxa/config.toml`에서 활성화:

```toml
[notifier]
enabled = true
backend = "libnotify"
```

설정 후 데몬을 재시작하세요.

## 설정

`muxad`는 `$XDG_CONFIG_HOME/muxa/config.toml`이 있으면 읽어들입니다. 모든 필드에
적절한 기본값이 있습니다 — 전체 스키마는
[`config.example.toml`](config.example.toml)에서 확인할 수 있습니다. `muxa` CLI도
같은 파일을 읽습니다(주로 `[watch]` 섹션 때문) — `MUXA_CONFIG=…`로 경로를
바꿀 수 있습니다.

### Watch 컬럼

`muxa watch` TUI의 컬럼 구성은 변경 가능합니다. 기본값은 마지막 프롬프트를
앞세우며, `model` / `ctx` / `cost`는 옵트인입니다:

```toml
[watch]
# 표시 순서. 빠진 키는 숨겨집니다.
columns = ["pane", "state", "prompt", "activity"]

[watch.widths]
# 숫자             -> Constraint::Length
# "min:N" 문자열   -> Constraint::Min(N)        (남는 공간을 흡수)
# "pct:N" 문자열   -> Constraint::Percentage(N)
pane     = 22
state    = 14
prompt   = "min:30"
activity = 10
```

사용 가능한 컬럼 키: `pane`, `kind`, `state`, `model`, `ctx`, `cost`,
`prompt`, `activity`. 모르는 키는 경고만 남기고 무시되며, muxa 실행을
막지는 않습니다.

<details>
<summary>환경 변수</summary>

| 변수            | 용도                                                  |
| --------------- | ----------------------------------------------------- |
| `MUXA_SOCKET`   | 유닉스 소켓 경로 오버라이드.                           |
| `MUXA_CONFIG`   | 설정 파일 경로 오버라이드.                             |
| `RUST_LOG`      | 트레이싱 필터. 예: `muxa=debug,tokio=warn`.            |
| `NO_COLOR`      | `muxa status`에서 ANSI 컬러 비활성화.                  |

</details>

## 아키텍처

```text
agent CLIs (Claude, Codex, Gemini)
      │
      │  shell hook runs `muxa hook <agent> --event <e>`
      │  — stdin JSON, ~1 ms per event
      ▼
    muxad  ───  0600 unix socket  ───  muxa CLI
      │                                  │
      ├── in-memory agent registry       └── status / watch TUI / status-line / recap
      ├── transition broadcast ──▶ notifier task (libnotify / native)
      ├── GC task (stopped-agent TTL)
      └── graceful SIGTERM → drain → unlink socket
```

5개 크레이트 워크스페이스:

- `muxa-core`     — 타입, 상태, 설정, 경로, 에러 (I/O 없음)
- `muxa-runtime`  — 유닉스 소켓 IPC 서버/클라이언트 + tmux CLI 래퍼 + 알림 처리
- `muxa-adapters` — `HookAdapter` 트레이트 + claude / codex / gemini 어댑터
- `muxad`         — 데몬 바이너리
- `muxa`          — CLI 바이너리 (`muxa watch` TUI 포함)

와이어 프로토콜 명세는 [`PROTOCOL.md`](PROTOCOL.md)를 참고하세요.

## 개발

```bash
# 빌드 + 테스트 + 린트
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt   --all
```

CI는 `fmt`, `clippy`, `test` (Linux + macOS), MSRV 체크,
`cargo-deny`를 실행합니다. 새로운 에이전트 어댑터를 추가하는 단계별 가이드를
포함한 전체 안내는 [`CONTRIBUTING.md`](CONTRIBUTING.md)에 있습니다.

데모 GIF 재생성 ([`vhs`](https://github.com/charmbracelet/vhs) 필요):

```bash
vhs docs/demo.tape
```

## 라이선스

[MIT](LICENSE-MIT) 또는 [Apache 2.0](LICENSE-APACHE) 듀얼 라이선스 —
원하시는 쪽을 선택하시면 됩니다.
