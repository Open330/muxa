# 설치

README를 짧게 유지하기 위해 자세한 설치와 wiring 절차는 이 문서에 둡니다.

## 필요 조건

- Rust 1.88+
- tmux 3.x
- Unix-like OS
- Claude Code, OpenAI Codex, Google Gemini CLI, Google Antigravity CLI(`agy`) 중 하나

## 설치 전 체험

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh
```

이 경로에는 위 필요 조건이 적용되지 않습니다. 가져온 shell script 자체가 전체
20단계 fullscreen tour입니다. ANSI terminal control로 가상 shell, tmux status line,
window/pane layout과 Muxa watch UI를 계속 유지하면서도 Muxa binary 다운로드, 임시
파일 생성, config 수정, daemon 시작, 실제 tmux session 조작을 전혀 하지 않습니다.
CPU architecture와 관계없이 일반 Unix-like terminal에서 실행할 수 있습니다. tour
flag는 `sh -s --` 뒤에 전달합니다.

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --print
```

Muxa를 설치한 뒤에는 `muxa onboard`로 같은 실습의 native Ratatui 버전을 실행할
수 있습니다. shell tour를 완료하면 원래 terminal로 돌아온 뒤 권장 Homebrew 명령,
직접 다운로드 링크와 설치 안내 링크를 짧게 출력합니다.

## One-Shot 설치

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh
```

스크립트는 `muxa`, `muxad`를 build/install 한 뒤 `muxa init`으로 tmux,
agent hook, optional systemd, optional dashboard를 연결합니다. wizard에
flag를 넘길 때는 `sh -s --`를 사용합니다:

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh -s -- --preset standard --yes
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh -s -- --dry-run
```

## Source에서 설치

```bash
git clone https://github.com/Open330/muxa.git
cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa-cli --locked
muxa init
```

`cargo install`은 `~/.cargo/bin`에 설치합니다. 해당 경로가 `PATH`에 있어야 합니다.

## Pre-Built Binary

[Releases page](https://github.com/Open330/muxa/releases)에서 archive를
받아 `muxa`, `muxad`를 `PATH`에 둔 뒤 실행합니다:

```bash
muxa init
```

현재 release target:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

## `muxa init`

권장 wiring 경로입니다. 사용 가능한 도구를 감지하고, component 선택,
file edit preview, uninstall을 처리합니다.

자주 쓰는 명령:

| 목표 | 명령 |
| --- | --- |
| 대화형 wizard | `muxa init` |
| headless 설치 | `muxa init --preset standard --yes` |
| preview only | `muxa init --dry-run` |
| reverse install | `muxa init --uninstall` |
| component 하나만 | `muxa init --component tmux-popup --yes` |
| preset에서 일부 제외 | `muxa init --preset standard --no muxad-systemd --yes` |

tmux edit은 marker block으로 감쌉니다:

```text
# >>> muxa managed (tmux-popup) >>>
...
# <<< muxa managed (tmux-popup) <<<
```

JSON/TOML agent config는 command-prefix matching으로 muxa hook entry만
제거할 수 있게 처리합니다. write 전에는 `<file>.muxa-backup-<unix_ts>` 백업을 만듭니다.

## Daemon 수동 실행

foreground:

```bash
muxad
```

background:

```bash
muxad &
```

systemd user service:

```bash
mkdir -p ~/.local/share/systemd/user
cp examples/muxad.service ~/.local/share/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now muxad.service
```

## tmux 수동 연결

최소 status line:

```tmux
set -g status-interval 2
set -g status-right "#(muxa status-line --pane #{pane_id}) | %H:%M"
```

optional popup:

```tmux
bind-key s display-popup -B -E -w 100% -h 100% -x 0 -y 0 "muxa watch"
bind-key D display-popup -E -w 95% -h 90% "muxa dashboard"
```

전체 agent 조회는 `prefix+s`를 사용합니다. 협업할 때는 메시지를 보낼 agent
pane을 선택하고 `prefix+D`를 누릅니다.

reload:

```bash
tmux source-file ~/.tmux.conf
```

## Agent 수동 연결

직접 편집보다 `muxa init`이 안전합니다. 수동으로 연결할 때는 기존 user hook을
덮어쓰지 말고 muxa hook command를 append 하세요.

| Agent | Config |
| --- | --- |
| Claude Code | `examples/claude-settings.json`을 `~/.claude/settings.json`에 merge. |
| OpenAI Codex | `crates/muxa/src/adapters/codex.rs`의 module doc에 있는 `[[hooks.*]]` block 추가. |
| Google Gemini CLI | `crates/muxa/src/adapters/gemini.rs`의 hook block을 `~/.gemini/settings.json`에 merge. |
| Google Antigravity CLI | `crates/muxa/src/adapters/antigravity.rs`의 `muxa` block을 `~/.gemini/config/hooks.json`에 추가. Gemini CLI의 `settings.json`이 **아니라** agy 전용 `hooks.json`입니다. |

## 확인

```bash
muxa status
muxa status-line --pane "$TMUX_PANE"
muxa watch
```

## Rollback

```bash
muxa init --uninstall
pkill muxad
tmux source-file ~/.tmux.conf
```

수동 rollback이 필요하면 `.muxa-backup-<unix_ts>` 파일을 복원합니다.
