# Architecture

`muxa`는 의도적으로 작게 유지합니다: daemon 하나, CLI 하나, local file, database 없음.

## Managed tmux domain model

Muxa는 작업 단위 tmux 모델에 최적화되어 있습니다.

| tmux 객체 | Domain identity | 불변 조건 |
| --- | --- | --- |
| Session | Workspace/project | managed session 하나가 project의 여러 work window를 담습니다. |
| Window | Work/ticket | work ID와 cwd마다 managed window 하나를 생성하거나 재사용하며 collaboration room이기도 합니다. |
| Pane | Agent | 모든 managed agent는 해당 work window 안의 독립 pane에서 실행됩니다. |

CLI, `muxa watch`, daemon registry, `muxa mcp`가 모두 이 매핑을 공유합니다.
같은 workspace의 같은 work는 기존 window를 재사용하거나 cwd가 다르면 실패해야
합니다. agent 추가는 pane 추가, work 종료는 window 종료, workspace 종료는
session 종료로 이어지며 파괴적 제어는 unmanaged target을 거부합니다.

## Flow

```text
agent hook/status event
        |
        v
      muxa hook  ---- unix socket ---->  muxad in-memory registry
                                             |
                                             +--> state.json
                                             +--> prompts.ndjson
                                             +--> activity.ndjson
                                             +--> collaboration.json
                                             +--> notifications / sinks
                                             +--> dashboard SSE
```

CLI는 live state를 socket으로 읽고, history/reporting view는 retained local
file도 함께 사용합니다.

controller daemon은 항상 별도의 physical-node plane을 만들고 in-process Store/backend에서
자신의 `local` node를 게시합니다. `[fleet]`을 켜면 remote host별 task가 remote user의
local muxad로 향하는 OpenSSH stdio relay 하나를 소유하고 자신의
`FleetStore` entry만 갱신합니다. remote agent는 local `Store`에 넣지 않으므로 local
reconcile, pane-id 재사용, GC가 다른 node의 truth를 손상시키지 않습니다.
[FLEET.ko.md](FLEET.ko.md)를 참고하세요.
local-only Fleet watch는 native watch runtime을 그대로 사용합니다. multi-node path는
IPC로 `FleetStore` invalidation을 구독해 coalesce하고 revision이 바뀐 host topology만
재구성하며, 느린 full-snapshot reconcile poll을 함께 유지합니다.

## Components

| Component | 역할 |
| --- | --- |
| `muxad` | daemon. registry, IPC server, background task, optional dashboard 소유. |
| `muxa` | status, watch, attend, recap, stats/report, activity query, init, hook CLI. |
| Agent adapters | Claude/Codex/Gemini hook event를 muxa state transition으로 변환. |
| tmux backend | pane/session, pane capture, foreground session activity 조회. |
| Activity ledger | state/tmux/human interval의 append-only duration source. |
| FleetManager | always-present local adapter, 독립 SSH relay state machine, node identity/권한, revision reconcile, host별 cache. |

## Data Files

| File | 목적 |
| --- | --- |
| `state.json` | daemon restart 후 rehydrate에 쓰는 마지막 snapshot. |
| `prompts.ndjson` | retained prompt audit log. |
| `activity.ndjson` | append-only duration ledger. |
| `session-activity.json` | legacy/compat tmux foreground total. |
| `collaboration.json` | same-window mailbox와 exact-session alias/role snapshot. |
| `host-id` | Fleet handshake에 쓰는 owner-only stable physical-node UUID. |

경로는 설정 가능하며 기본값은 `$XDG_DATA_HOME/muxa` 아래입니다.

## Security

- IPC socket은 가능한 경우 owner-only permission으로 harden합니다.
- Dashboard는 기본 loopback-only입니다.
- Public dashboard binding은 명시적 `allow_public`이 필요합니다.
  `public_read`는 익명 조회를 허용하되 변경에는 PAT를 요구하고, `none`은
  조회만 허용하며 변경 기능을 비활성화합니다.
- External sink는 opt-in입니다.
- Fleet은 fixed SSH command token을 사용하고 forwarding을 끄며 exact global pane identity를
  검증합니다. host는 observe-only가 기본이고 remote network listener를 열지 않습니다.
- Rust `unsafe`는 금지되어 있습니다.

## Shutdown

`SIGTERM`/`SIGINT`는 먼저 IPC server와 일반 background producer를 중단합니다.
in-flight handler와 producer가 drain된 뒤 activity transition subscriber,
prompt/activity writer, 마지막으로 `state.json` snapshot을 순서대로 flush합니다.
따라서 종료 전 commit된 event가 ledger와 snapshot에서 서로 어긋나지 않습니다.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

자주 쓰는 focused check:

```bash
cargo test -p muxa-cli -- --nocapture
cargo test -p muxa activity::tests -- --nocapture
cargo check -p muxa-cli
```
