# Muxa에서 옮겨 오기

Muxa의 새 이름은 Tend입니다. Tend는 Muxa의 이름을 하나도 읽지 않으므로, 기존 설치는
호스트마다 한 번씩 옮겨야 합니다. `scripts/migrate-from-muxa.py`가 이 일을 합니다.

## 바뀐 것

| Muxa | Tend |
| --- | --- |
| `muxa`, `muxad` | `tend`, `tendd` |
| `brew install open330/tap/muxa` | `brew install open330/tap/tend` |
| `Muxa.app` (`dev.muxa.mac`) | `Tend.app` (`dev.tend.mac`) |
| `MUXA_SOCKET`, `MUXA_CONFIG`, … | `TEND_SOCKET`, `TEND_CONFIG`, … |
| `~/Library/Application Support/muxa`(macOS), `~/.local/share/muxa`, `~/.config/muxa`(Linux) | 같은 위치의 `tend` |
| `/tmp/muxa-<uid>.sock` | `/tmp/tend-<uid>.sock` |
| Claude Code, Codex, Gemini, Antigravity, opencode의 `muxa hook …` | `tend hook …` |
| MCP 서버 `muxa`, 도구 `muxa_*` | MCP 서버 `tend`, 도구 `tend_*` |
| 스킬 `muxa-collaboration` | 스킬 `tend-collaboration` |
| tmux 옵션 `@muxa_*` | `@tend_*` |
| 피어 메시지 `[muxa:req_…]` | `[tend:req_…]` |
| launchd `dev.open330.muxad`, systemd `muxad.service` | `dev.open330.tendd`, `tendd.service` |

피어는 같은 이름을 쓰는 피어하고만 대화합니다. 피어 요청을 다시 쓰기 전에 함대의 모든 호스트를 올리세요.

## 순서

1. Tend 설치: `brew install open330/tap/tend` (Mac 앱은 `brew install --cask open330/tap/tend-app`).
2. 계획 보기. 아직 아무것도 바뀌지 않습니다.

   ```bash
   python3 scripts/migrate-from-muxa.py
   ```

   `~/.claude` 말고 다른 Claude Code 설정 폴더(`CLAUDE_CONFIG_DIR`)를 쓴다면 폴더마다
   `--claude-home DIR`을, `~/.codex` 말고 다른 Codex 설정 폴더(`CODEX_HOME`)를 쓴다면 `--codex-home DIR`을 붙입니다. 바뀌는 줄을 하나씩 보려면 `-v`를 붙입니다.
3. 적용:

   ```bash
   python3 scripts/migrate-from-muxa.py --apply
   ```

4. 새 셸을 열고 에이전트를 다시 시작합니다. 그래야 훅과 MCP 서버가 `tend`를 부릅니다.
   스크립트가 옮긴 Muxa 서비스가 없다고 알리면 `tend init`으로 서비스를 설치합니다.
5. Tend가 잘 돌면 Muxa를 지웁니다: `brew uninstall muxa`(또는 `cargo uninstall muxa-cli muxad`),
   `Muxa.app`은 휴지통으로.

Muxa를 돌리던 호스트마다 실행하세요. Python 3.8 이상이 필요하고, TOML 파일(Tend의 `config.toml`, Codex의
`config.toml`)을 고치려면 3.11 이상이 필요합니다. macOS에 기본으로 든 것처럼 더 오래된 Python에서는 그 두 파일을
고치지 않고 직접 고치라고 알려 줍니다. `brew install python`으로 새 버전을 설치할 수 있습니다.

## 스크립트가 하는 일

- `muxad`를 멈추고, brew 서비스는 `tend`로 다시 시작하거나 방법을 알려 주며, launchd 작업이나 systemd 유닛(복구 작업과 사용자 유닛 폴더 어디에 있든 `.d/` 드롭인 포함)을 Tend 이름으로 옮깁니다.
  에이전트에 맞춰 손본 PATH 같은 환경은 그대로 두고, 이름, 실행 파일, 로그 경로만 바꿉니다.
  Tend가 그 작업이 기대하는 위치에 설치돼 있으면 마지막에 다시 시작합니다.
- Muxa의 데이터, 설정, 캐시 폴더를 `tend`로 옮깁니다. `tend` 폴더가 이미 있으면 둘 다 그대로 두고 알려 줍니다.
- 에이전트 연동 번들(`bootstrap.md`, 협업 스킬)은 이름을 전부 바꿉니다. fleet 설정 키 `muxa_path`는 `tend_path`로 바꿉니다. `config.toml`, 사용자 에이전트 정의,
  다른 도구의 설정에서는 Muxa가 쓴 부분만 바꿉니다: 훅 명령(절대 경로 `/opt/bin/muxa` 포함), MCP 서버 항목,
  관리 블록, `MUXA_*` 변수, `@muxa_*` 옵션, 프롬프트와 권한 규칙 속 MCP 도구 이름(`muxa_call_peer`,
  `mcp__muxa__muxa_status` 등), 옮긴 폴더를 가리키는 경로. 사용자가 넣은 값은 그대로 둡니다:
  `~/code/muxa`의 `cwd`나 저장소, URL, 프로젝트 신뢰 항목.
- Tend 쪽 항목이 이미 있으면 둘 다 그대로 두고 알려 줍니다: `tend`라는 MCP 서버, Codex의 `[mcp_servers.tend]`,
  `tend.ts`, `tend-collaboration` 링크, 키체인의 Tend 키.
- `.claude.json`은 Claude Code의 잠금을 잡고 고치고, 심볼릭 링크로 된 dotfile은 링크를 유지한 채 대상 파일을
  고치며, TOML 파일은 고친 결과를 읽을 수 있는지 확인한 뒤에만 씁니다.
- Claude Code와 Codex의 `tend-collaboration` 스킬 링크를 다시 겁니다.
- 실행 중인 tmux와 rmux 세션, 창, pane의 `@muxa_*` 옵션을 `@tend_*`로 복사해 이미 돌고 있는 Work가 정체성을 유지하게 하고,
  전역 환경의 `MUXA_SOCKET`, `MUXA_CONFIG`, `MUXA_HOST(S)`를 `TEND_*`로 옮깁니다. 그 밖의 `MUXA_*`는 비밀 값일 수 있어 이름만 알려 줍니다.
- macOS: 앱 설정을 `dev.muxa.mac`에서 `dev.tend.mac`으로, Ask 공급자 API 키를
  `dev.muxa.mac.ask-provider` 키체인 서비스에서 `dev.tend.mac.ask-provider`로 복사합니다.
  키는 명령 인자가 아니라 표준 입력으로 `security`에 넘깁니다. 키마다 macOS가 읽기 허락을 한 번 묻습니다.

muxa를 부르는 사용자 launchd 작업, systemd 유닛, crontab 항목은 고치지 않고 마지막에 목록으로 알려 줍니다.

고치는 파일은 먼저 `<파일>.muxa-migrate-<시각>`으로 복사해 둡니다. 성공한 뒤 다시 실행하면 할 일이 없다고 나옵니다.

`--only-files`는 서비스, 실행 중인 tmux, 앱 설정, 키체인을 건너뜁니다. `--skip-tmux`는 실행 중인 tmux만 건너뜁니다.
