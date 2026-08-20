# Muxa Fleet

Muxa Fleet은 기존 session/window/pane 구조 위에 physical host 계층을 더하는
중앙 control plane입니다.

```text
local muxad
  FleetManager
    ├─ in-process      ─ host local ─ session ─ window ─ pane(agent)
    ├─ SSH stdio relay ─ host dev ─ session ─ window ─ pane(agent)
    ├─ SSH stdio relay ─ host gpu ─ session ─ window ─ pane(agent)
    └─ host별 cache    ─ host prod ─ offline 중 last known snapshot
```

controller는 항상 첫 번째 `local` host로 in-process 게시됩니다. 설정된 remote
host마다 독립적인 OpenSSH process 하나를 유지합니다. 원격에서는
`muxa relay --stdio`가 실행되어 같은 사용자의 owner-only muxad Unix socket과만
통신합니다. 원격 TCP port를 열거나 agent socket을 forwarding하지 않으며 terminal
내용도 상시 복제하지 않습니다.

여기서 Fleet host는 물리 node입니다. 기존 문서에서 사용하던 tmux/rmux/herdr/
zellij “pane host”는 각 node 내부의 backend kind로 그대로 유지됩니다.

## 준비

- controller와 모든 remote host에 같은 Muxa version을 설치합니다.
- 각 remote host에서 같은 Unix user로 muxad를 실행합니다.
- non-interactive OpenSSH 인증과 host-key trust를 먼저 확인합니다.
- user, port, key, ProxyJump 등 SSH 정책은 `~/.ssh/config`에 둡니다.

```sshconfig
Host muxa-devbox
  HostName devbox.example.com
  User june
  IdentityFile ~/.ssh/id_ed25519
  IdentitiesOnly yes
```

등록 전 transport를 확인할 수 있습니다.

```bash
ssh -T -o BatchMode=yes muxa-devbox muxa relay --stdio
```

JSON hello 한 줄이 출력된 뒤 요청을 기다리면 정상입니다. 수동 확인에서는
`Ctrl-C`로 종료합니다.

## Inventory와 Kubernetes-style metadata

controller node는 별도 설정 없이 바로 사용할 수 있습니다.

```bash
muxa fleet status
muxa host show local
muxa host doctor local
muxa host label local environment=development
muxa host annotate local muxa.dev/owner=platform
```

`local`도 remote node와 같은 stable NodeId와 topology를 가지지만 SSH를 사용하지 않고
항상 connected/control 상태입니다. add/remove/disable/disconnect할 수 없으며 사용자
metadata는 `[fleet.local]`에 저장됩니다. `muxa.io/local`, `muxa.io/transport`,
`kubernetes.io/{hostname,os,arch}`는 muxad가 관리하는 사실 label이므로 selector에는
쓸 수 있지만 사용자가 덮어쓸 수 없습니다.

CLI를 사용하면 TOML을 검증하고 atomic하게 변경합니다.

```bash
muxa host add dev muxa-devbox \
  --label environment=development \
  --label region=icn \
  --annotation muxa.dev/owner=platform \
  --mode observe

muxa host doctor dev
muxa host list
muxa host show dev
```

첫 remote host 명령은 outbound SSH Fleet 연결을 활성화합니다. `[fleet] enabled = false`여도
local node는 계속 보입니다. 변경 후에는 연결 목록과 권한 정책이
함께 다시 로드되도록 muxad self-restart를 요청합니다. daemon에 연결할 수 없으면
config 변경은 보존하고 수동 restart 안내를 출력합니다.

label은 Kubernetes key/value 규칙을 따르는 selection metadata입니다.

```bash
muxa host label dev tier=worker accelerator=gpu
muxa host label dev tier=worker --overwrite
muxa host label dev accelerator-              # 제거
muxa host tag dev region=icn                   # label의 visible alias
```

annotation key도 namespaced key 규칙을 따르지만 value에는 설명이나 URL을 넣을 수
있습니다.

```bash
muxa host annotate dev muxa.dev/runbook=https://example.invalid/dev
```

controller alias는 사람이 읽는 config identity입니다. relay는 별도로
`$XDG_DATA_HOME/muxa/host-id`에 owner-only stable UUID를 만듭니다. SSH alias,
hostname, inventory alias를 바꿔도 node identity는 유지됩니다. 동일 UUID를 보고하는
두 alias가 동시에 연결되면 한 물리 장비를 중복 제어하지 않도록 거부합니다.

지원 selector는 다음과 같습니다.

- `environment=production`, `environment==production`
- `environment!=production`
- `region in (icn,nrt)`, `region notin (iad,sfo)`
- `accelerator`(key 존재), `!accelerator`(key 없음)
- comma는 AND: `environment=production,region in (icn,nrt)`

## 권한과 연결 정책

각 host는 두 정책을 독립적으로 가집니다.

local node는 항상 connected/control이며, 아래 설정은 remote inventory에 적용됩니다.

- `mode = "observe"`: snapshot과 on-demand capture는 허용하지만 prompt 전송은
  거부합니다. 기본값입니다.
- `mode = "control"`: exact-pane prompt 전송까지 허용합니다.
- `connect = "auto"`: bounded exponential backoff로 persistent relay를 유지합니다.
- `connect = "on_demand"`: `muxa fleet connect` 전까지 연결하지 않습니다.

`muxa host disable`은 metadata를 남기고 연결만 막으며, `muxa host enable`로 다시
활성화합니다.

## 사용

```bash
muxa fleet status
muxa fleet status -l 'environment=production,region in (icn,nrt)'
muxa fleet status --json
muxa fleet watch
muxa watch --fleet --selector 'accelerator=gpu'

muxa fleet panes local
muxa fleet capture local '%12'
muxa fleet send local '%12' '현재 결과를 요약해주세요.'
muxa fleet attach local '%12'

muxa fleet connect dev
muxa fleet disconnect dev
muxa fleet refresh dev
muxa fleet panes dev
muxa fleet capture dev '%12'
muxa fleet send dev '%12' '현재 결과를 요약해주세요.'
muxa fleet attach dev '%12'
```

bare pane id나 `session/window/pane` path는 해당 물리 host의 모든 backend endpoint에서
유일할 때만 허용합니다. display path도 모호하면 `muxa fleet panes HOST --json`이
capture/send/attach와 MCP 도구에 그대로 전달할 수 있는 complete `PaneKey` JSON을
출력합니다. 내부 명령은 완전한 node/backend/session/window/pane identity를 전달하고,
relay가 control 직전에 fresh pane list로 다시 확인합니다.
local adapter도 같은 exact key 검증을 in-process로 수행하며 attach 시 SSH를 열지 않고
직접 이동합니다.

Fleet TUI 동작:

- Up/Down은 보이는 모든 structural node를 순회합니다.
- `j`/`k`는 singleton session/window chain에서도 actionable pane 사이를 바로
  이동합니다.
- `h`/`l` 또는 Left/Right는 접기/내리기, Space는 parent toggle입니다.
- `/` 검색, `a` attention-only, `r` refresh, `c` remote host 연결/해제입니다.
  `local`에서는 항상 연결됐다는 안내만 표시합니다.
- `p` pane capture, `m` exact-pane prompt composer, Enter는 `local`이면 직접 이동하고
  remote이면 SSH attach하며, `?`는 help입니다.

session inspector는 window와 하위 pane을 함께 roll-up합니다. window를 선택하면
필요할 때만 pane을 capture하고 실제 tmux split geometry로 mosaic를 렌더링합니다.
접힌 window나 선택하지 않은 window에는 capture를 수행하지 않습니다.

## 상태, 일관성, 성능

host 상태는 `disabled`, `connecting`, `online`, `degraded`, `offline`,
`auth_failed`, `version_skew`입니다. offline에서도 last good snapshot을 유지합니다.
새 handshake가 성공하면 예전 remote identity를 먼저 지우고 새 full snapshot을
받습니다.

snapshot/transition에는 monotonic revision이 있습니다. gap은 degraded로 표시하고
reconcile하며, subscription을 잃으면 relay를 재연결합니다. keepalive는 조용히 끊긴
transport를 감지합니다. host마다 task/state/backoff가 독립적이므로 느린 host 하나가
전체를 막지 않고 `max_parallel_connects`가 동시 SSH handshake를 제한합니다.

local adapter는 Store transition을 직접 구독하고 `refresh_secs`마다 backend topology를
갱신합니다. backend scan은 async IPC executor가 아닌 blocking worker에서 실행되며,
별도의 revisioned Fleet snapshot을 유지하므로 selector/UI가 authoritative local Store를
복제하거나 변경하지 않습니다.

central cache에는 agent/topology metadata만 저장합니다. terminal capture는 선택 시에만
요청하고 크기를 제한하며 control sequence를 제거합니다. window capture의 병렬성과
payload도 제한됩니다. prompt는 remote shell command에 삽입하지 않고 stdin frame으로만
보내며 manager audit log에 body를 남기지 않습니다.

mutation은 자동 retry하지 않습니다. 결과는 text delivery와 Enter submit 여부를 따로
알려 partial acknowledgement 뒤 prompt를 중복 전송하지 않게 합니다.

## 다른 interface

`muxa mcp`는 `muxa_fleet_status`, `muxa_fleet_capture`,
`muxa_fleet_send_prompt`를 제공합니다. control 도구는 host와 pane을 명시해야 합니다.
remote는 observe/control 검사를 적용하고 `local`은 owner-only socket control입니다.

web dashboard를 켜면 read API에 `GET /api/fleet?selector=...`가 추가됩니다.
`POST /api/fleet/{host}/command`는 serialized `FleetOperation`을 받아 PAT를 요구합니다.
dashboard `auth = "none"`에서는 기존 정책대로 모든 write가 비활성화됩니다.

durable Muxa collaboration mailbox는 이번 버전에서 physical node local입니다. Fleet
prompt로 remote agent에게 작업을 요청할 수는 있지만 cross-host `@peer`가 durable reply를
보장한다고 가장하지 않습니다. 향후 hub transport는 현재 node/pane identity를 바꾸지
않고 추가할 수 있습니다.

## 보안 checklist

- controller dashboard는 의도적으로 PAT/public bind를 구성한 경우가 아니면
  loopback-only로 유지합니다.
- OpenSSH `Host` alias와 `known_hosts`를 검토합니다. Fleet은 `BatchMode=yes`를 쓰며
  host-key 검사를 끄지 않습니다.
- Fleet은 `ClearAllForwardings=yes`를 강제하고 agent forwarding을 요청하지 않습니다.
- 새 host는 observe로 시작하고 필요한 node만 control로 승격합니다.
- controller의 muxad socket, config, SSH key 접근은 shell-equivalent 권한으로
  취급합니다.
- label/annotation에 secret을 넣지 않습니다. inspector에 표시됩니다.

전체 설정은 [CONFIGURATION.ko.md](CONFIGURATION.ko.md), 같은 물리 node 안에서 여러
backend를 관측하는 기능은 [MULTI_HOST.md](MULTI_HOST.md)를 참고하세요.
