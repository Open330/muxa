# 전역 에이전트 연동

`muxa init`이 감지된 Codex와 Claude Code에 공통 협업 스킬, 전역 진입 지침,
사용자 범위 MCP 서버를 연결합니다. `standard`, `full` preset에 포함됩니다.
다른 에이전트의 기존 hook 연동은 유지되며, 이번 전역 지침·스킬 컴포넌트의
대상은 Codex와 Claude Code입니다.

```bash
muxa init --component agent-instructions,agent-skills,agent-mcp --dry-run
muxa init --component agent-instructions,agent-skills,agent-mcp --yes

# 처음 설치할 때: hook, 협업 mailbox, daemon도 함께 설정
muxa init --preset standard --yes
```

설치 후 실행 중인 에이전트를 재시작해야 전역 지침과 MCP 설정을 다시 읽습니다.
세 컴포넌트만 선택하면 협업 mailbox는 켜지지 않으므로, 아직 활성화하지 않았다면
`collaboration`도 선택하거나 `standard` preset을 사용합니다. MCP에는 실행 중인
muxad가 필요합니다.

## 정본과 연결

저장소의 `crates/muxa-cli/assets/agent-integration/` 번들을 바이너리에 포함하고,
설치 시 실제 muxa config 파일 옆에 배치합니다. `--config`로 선택한 경로도
반영합니다.

```text
<muxa 설정 디렉터리>/
├── config.toml
└── agent-integration/
    ├── bootstrap.md
    ├── manifest.json
    └── skills/muxa-collaboration/
        ├── SKILL.md
        └── references/workflows.md
```

macOS 기본 경로는 `~/Library/Application Support/muxa`, Linux에서는
`$XDG_CONFIG_HOME/muxa` 또는 `~/.config/muxa`입니다. 공백이 있는 경로도 지원합니다.

| 컴포넌트 | Codex | Claude Code |
| --- | --- | --- |
| `agent-instructions` | 실제 읽히는 전역 `AGENTS.md` 또는 `AGENTS.override.md`에 관리 블록 추가 | 전역 `CLAUDE.md`에 관리 블록 추가 |
| `agent-skills` | `~/.agents/skills/muxa-collaboration` 심링크 | `~/.claude/skills/muxa-collaboration` 심링크 |
| `agent-mcp` | `config.toml`의 `[mcp_servers.muxa]` 병합 | `~/.claude.json`의 사용자 `mcpServers.muxa` 병합 |

`CODEX_HOME`을 설정하면 Codex 설정·지침 경로를 따릅니다. 개인 공통 스킬 경로는
`~/.agents/skills`입니다. `CLAUDE_CONFIG_DIR`을 설정하면 해당 디렉터리에
Claude 지침·스킬과 `.claude.json`을 둡니다.

전역 지침에는 muxa 협업이 필요할 때 공통 `bootstrap.md`를 읽으라는 짧은 블록만
추가합니다. 기존 지침 파일 전체를 교체하지 않으며 기존 dotfile 심링크도
유지하고 실제 대상 파일을 수정합니다. Codex의 비어 있지 않은 override가
새로 생기면 `init`을 다시 실행해 진입 지침을 옮길 수 있습니다.

## 협업 지침과 사용자 선호

스킬은 Workspace/session, Work의 현재 Run/window, Agent/pane 관계를 설명합니다.
같은 window의 동료 탐색, 읽기 전용 리뷰와 수정 작업의 구분, 담당 파일 분리,
request ID를 통한 결과 추적, inbox 수신과 reply, 최종 검증·통합을 제공합니다.
pane 분할만으로 파일이 격리되지는 않으므로, 동시 수정에는 파일 담당 범위를
나누거나 별도 worktree를 사용합니다.

실시간 상태는 MCP 도구로, 실행 선호는 `muxa_guide`로, 런타임 협업 규약은
`muxa_collaboration_guide`로 확인합니다. 이미 받은 명시적 위임은 같은 범위에서
유효합니다. 리뷰 요청만으로 수정이나 bypass-permission peer 실행 권한이
생기지는 않습니다.

사용자 선호는 `config.toml`의 `[mcp.guide]`에 둡니다. `[message.skills]`는
동료에게 보내는 프롬프트 템플릿이며 이번 에이전트용 `SKILL.md`와 별개입니다.

## 업데이트·진단·제거

재실행해도 지침 블록과 링크는 중복되지 않습니다. 기존 Muxa MCP의 실행 경로,
인자, 환경, 기타 옵션은 유지하며 Codex에 필요한 `env_vars`만 보충합니다.
새 등록에는 선택한 config와 비기본 socket을 반영합니다. 기존 등록의 라우팅을
바꾸려면 해당 항목을 명시적으로 수정해야 합니다.

설치 기록과 이전 MCP 항목은 비공개 권한의 `manifest.json`에 보관하고 파일 수정
전 백업을 만듭니다. 사용자 스킬·다른 MCP 서버와 이름이 충돌하거나 설치된 파일을
사용자가 수정했다면 보존하고 안내합니다. 계획 이후 설정이 바뀌면 적용을 멈추므로
`init`을 다시 실행해 새 내용을 기준으로 계획할 수 있습니다.

```bash
muxa doctor
muxa init --component agent-skills --uninstall
muxa init --component agent-instructions,agent-skills,agent-mcp --uninstall
```

`doctor`는 기록된 파일·심링크, 전역 override에 가려진 지침, MCP 등록 변경을
검사합니다. 클라이언트를 실제 실행하는 검사는 아니므로 재시작 후
`codex mcp list`, `claude mcp list`로 연결도 확인합니다.

제거 시 muxa가 만든 링크와 변경되지 않은 번들 파일만 삭제하고 전역 파일에서는
관리 블록만 뺍니다. MCP 항목이 설치 후 그대로라면 이전 항목을 복원합니다.
수정된 항목은 안내와 함께 보존합니다. 개별 컴포넌트 제거는 나머지 컴포넌트에
영향을 주지 않습니다. 빈 지침 파일·번들 디렉터리·설치 기록·백업은 남을 수 있으며
디렉터리를 재귀 삭제하지 않습니다.
