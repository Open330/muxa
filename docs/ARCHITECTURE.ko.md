# Architecture

`muxa`는 의도적으로 작게 유지합니다: daemon 하나, CLI 하나, local file, database 없음.

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

## Components

| Component | 역할 |
| --- | --- |
| `muxad` | daemon. registry, IPC server, background task, optional dashboard 소유. |
| `muxa` | status, watch, attend, recap, stats/report, activity query, init, hook CLI. |
| Agent adapters | Claude/Codex/Gemini hook event를 muxa state transition으로 변환. |
| tmux backend | pane/session, pane capture, foreground session activity 조회. |
| Activity ledger | state/tmux/human interval의 append-only duration source. |

## Data Files

| File | 목적 |
| --- | --- |
| `state.json` | daemon restart 후 rehydrate에 쓰는 마지막 snapshot. |
| `prompts.ndjson` | retained prompt audit log. |
| `activity.ndjson` | append-only duration ledger. |
| `session-activity.json` | legacy/compat tmux foreground total. |
| `collaboration.json` | same-window mailbox와 exact-session alias/role snapshot. |

경로는 설정 가능하며 기본값은 `$XDG_DATA_HOME/muxa` 아래입니다.

## Security

- IPC socket은 가능한 경우 owner-only permission으로 harden합니다.
- Dashboard는 기본 loopback-only입니다.
- Public dashboard binding은 명시적 `allow_public`이 필요하며, token 없는
  public API는 추가로 `dashboard.auth = "none"` opt-in이 필요합니다.
- External sink는 opt-in입니다.
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
