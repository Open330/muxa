# Agent runtime 구조 검토와 통합 개선 계획

기준: muxa `78f3e148ad9434af309205c7a09b27e1c461dc17`에서 시작한 개선 계획.
아래 타입·설정·명령은 구현 완료가 표시되지 않은 한 제안이다.

실행 소유권·영속 명령·실행 식별자를 보강한 뒤 권한 프로필, 완료 검증,
Work 검토 화면, 세션 분기를 같은 실행 계약 위에 추가한다.

## 설계 원칙

- daemon이 실행 생명주기를 소유하고 CLI와 각 화면은 같은 명령 계약을 사용한다.
- 요청, 실행, 재시도를 구분하고 재시작 후 결과를 다시 조회할 수 있게 한다.
- 외부 부작용의 전달 여부가 불확실하면 무조건 재실행하지 않는다.
- provider별 실제 지원 기능과 실행 권한을 명시한다.
- 완료 주장과 검증 결과를 구분하고 검증 대상 코드 상태를 기록한다.
- 기존 tmux 환경, Work 계층, mailbox, Fleet의 독립성을 보존한다.

## 진행 상태

첫 변경에서 pipeline reconciler의 실행 기한·종료·실패 전파를 보강했다.

- worker 실행을 60초, stdout/stderr 합계 64 KiB로 제한한다.
- daemon 종료가 진행 중인 worker와 실패 후 대기를 취소한다.
- 실패 후 최소 5초 대기하며 revision 알림이 대기를 우회하지 않는다.
- CLI는 개별 Run 실패 후 나머지 Run을 처리한 뒤 실패를 반환한다.
- 종료는 CLI worker를 대상으로 하며 tmux에 이미 실행된 agent는 유지한다.
- 실제 자식 프로세스 종료와 동시 claim, 재시작 후 만료 claim의 재획득·pane
  adoption 상태 보존을 회귀 시험으로 확인한다. 실제 tmux 장애 주입은 별도 과제다.

이번 변경만 분리한 checkout에서 `cargo test --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`를 통과했다.

영속 operation/attempt, claim fencing, 공통 실행 service와 이후 기능은 후속 작업이다.

### 성능 개선 순서

1. IPC 구독 종료: 모든 관측 스트림에 종료 상태를 전달하고, 대기 중인 socket
   쓰기까지 취소한다. 일반 변경 요청의 drain은 유지한다. 이 처리는 구현했으며
   IPC drain 소요 시간을 debug 로그의 `elapsed_ms`로 확인할 수 있다.
2. 부하 기준 마련: agent 수·구독 수별 IPC 지연 분포, 유휴 CPU·메모리,
   상태 변경부터 화면 반영까지의 지연, 저장 시간·잠금 대기를 측정한다.
3. 로컬 Fleet 구독은 내부 revision 알림으로 전환했다. Fleet 갱신은 대상 host만
   복사하고, pipeline readiness는 전체 Run 복사 없이 판단한다. PipelineRun 전체
   저장 비용은 영속 operation 설계에 함께 반영할 후속 측정 대상이다.
4. reconcile CLI 실행 비용이 유의미하면 공통 실행 service 전환에서 제거한다.

구독 회귀 시험은 여섯 종류의 실제 IPC 구독을 유지한 종료, 종료 후 늦은 stream
takeover, 읽지 않는 클라이언트의 쓰기 정체를 다룬다. 단순히 전역 drain 제한을
줄이지 않으며, 클라이언트가 송신 쪽을 닫고 알림을 계속 받는 프로토콜도 유지한다.

격리된 debug daemon에서 로컬 Fleet 구독이 연결된 뒤 SIGTERM부터 process exit까지
측정했다. 외부 구독 수 0·6·60·60·60개인 5회 실행에서 각각
15.3·31.4·31.4·15.3·15.3 ms였고, 모두 IPC drain timeout 없이 종료했다.
이는 유휴 구독 종료 시나리오의 관측값이며, 전체 부하 성능이나 p95 보장은 아니다.

추가 점검에서는 한 host의 명령 큐가 포화되면 공유 router까지 대기하던 경로를
제거했다. 해당 요청에 재시도 오류를 반환하며, 다른 host의 요청은 계속 전달한다.
조회 비용 비교는 `cargo bench -p muxa --bench runtime_reads`로 재현할 수 있다.
전체 점검 범위와 남은 측정 항목은 [성능 점검 기록](PERFORMANCE_REVIEW.ko.md)에 정리한다.

## 유지할 구조

- Workspace → Work → Run → Agent session, 외부 이슈와 로컬 Work의 분리.
- 기존 tmux와 다른 pane backend를 관찰·제어하는 방식, daemon 소유 native PTY.
- hook 우선 관찰과 screen fallback. 관찰 상태를 완료 선언으로 추론하지 않는 정책.
- generation 기반 완료 검증, 의존 그래프, 원자적 상태 저장과 pane 재발견.
- Fleet의 host별 상태·권한 분리와 observe 기본값.
- revision 알림 후 scoped snapshot을 다시 읽는 클라이언트 구조.
- SQLite mailbox, request/thread/parent 관계, AIR artifact 참조와 협업 audit.

## 확인한 구조적 문제

### A. 실행 정책의 소유권이 CLI와 daemon 사이에 나뉜다 — 우선 해결

`work_control.rs`는 실제 reconciliation을 CLI가 소유한다고 명시한다.
daemon의 Work 요청은 CLI를 실행하고, CLI는 다시 daemon IPC로 상태를 갱신한다.
daemon의 pipeline reconciler도 `muxa work reconcile --all`을 실행한다.

- 근거: [work_control.rs](../crates/muxa/src/work_control.rs),
  [work_up.rs](../crates/muxa-cli/src/work_up.rs),
  [pipeline_reconciler.rs](../crates/muxad/src/pipeline_reconciler.rs).
- 현재 코드를 재사용한다는 장점은 있다. 그러나 명령의 생명주기, 취소,
  배포된 CLI 버전, 재시도 책임이 여러 계층에 걸친다.
- 기존 background reconciler는 `.output().await`에 명시적 timeout이 없었다.
  첫 변경에서 공통 bounded runner와 종료 시 취소를 적용했다. 실행 정책을
  CLI가 소유하는 구조는 그대로이므로 service 경계 정리는 후속 작업이다.
- 개선: `muxa` library 내부 runtime service가 결정과 실행을 소유하고,
  CLI/MCP/웹/Mac은 같은 명령 API를 사용한다. tmux subprocess 자체는 정상적인
  backend 경계로 남긴다. 첫 단계부터 별도 서비스나 crate를 늘릴 필요는 없다.

### B. operation 기록과 실행 시도 식별자가 충분히 영속적이지 않다 — 우선 해결

`WorkUpManager`의 요청·결과는 메모리 BTreeMap에 있다. `native-work-N` 번호도
daemon 시작마다 1부터 시작하고, 중복 제거는 현재 실행 중인 동일 요청에 한정된다.
daemon 재시작 후 기존 operation을 계속 조회하거나 완료된 요청의 재전송을
판별하는 기반으로 쓰기 어렵다.

`PipelineRunStore`는 WorkIdentity당 PipelineRun 하나를 보관하며 갱신 시 교체한다.
Work snapshot의 execution identity는 host/socket/session/window로 구성된다.
mailbox에 run_id 필드는 있지만, pipeline 실행 전체를 연결하는 독립적 Run/Attempt
이력과 동일한 것은 아니다.

- 근거: [ipc.rs](../crates/muxa/src/ipc.rs)의 `WorkUpManager`,
  [pipeline_run.rs](../crates/muxa/src/pipeline_run.rs)의 `Inner`, `register`,
  [work.rs](../crates/muxa/src/work.rs)의 `ExecutionIdentity`.
- 개선: 영속 `run_id`, 단계별 `attempt_id`, 명령별 `operation_id`를 구분한다.
  generation은 낡은 결과를 거부하는 용도로 유지하고 식별자의 대체물로 쓰지 않는다.
- Work 시작 재호출은 활성 Run으로 수렴한다. 명시적 새 실행은 새 Run,
  같은 단계 재시도는 새 Attempt, pane 재연결은 binding 갱신으로 구분한다.
- 명령은 호출자 범위의 idempotency key와 정규화된 입력 digest를 받는다.
  같은 key/같은 입력은 기존 결과를 반환하고, 다른 입력이면 충돌로 거부한다.
  사용자가 의도적으로 같은 프롬프트를 두 번 보내는 것은 서로 다른 명령이다.

### C. lease 만료와 외부 부작용 사이의 경계가 약하다 — 검증 후 보강

현재 claim은 15초 뒤 재획득할 수 있다. `PipelineClaim`과 `report`에는
별도의 claim token이 없고 generation을 확인한다. 여러 alias를 한꺼번에 claim한
뒤 순서대로 실행한다. 동일 generation에서 이전 실행자가 느리게 살아 있는 동안
예약이 재획득되면, 이전 report를 구별할 수 없는 구조다.

- 근거: [pipeline_run.rs](../crates/muxa/src/pipeline_run.rs)의
  `CLAIM_LEASE_SECONDS`, `claim_ready`, `report`, `claimable`.
- 기존 `recover_unreported_alias`는 생성 후 report 전에 죽은 pane을 재발견한다.
  따라서 중복 방지와 복구가 전혀 없다는 평가는 틀리다.
- 다만 pane 재발견은 동시 실행의 모든 경쟁이나 프롬프트 전달 중복을 증명하지 않는다.
  실제 중복 장애는 미재현이며, 지연·응답 유실을 넣은 시험으로 먼저 확인한다.
- 개선: attempt별 fencing token, bounded executor, 살아 있는 실행의 lease 갱신,
  결과 수락 시 token 비교, 외부 실행 직전 소유권 확인을 적용한다.
- 이미 tmux에 전달된 입력을 fencing token으로 취소할 수는 없다. 전달 여부가
  모호한 프롬프트는 `unknown`으로 기록하고 무조건 재전송하지 않는다.
  같은 프로세스에서 직렬화할 수 있는 실행은 직렬화하고, 재시작 후에는 먼저 관찰한다.

### D. Agent 단계에 검증·권한·결과 증거를 붙일 공통 모델이 부족하다

`PipelineSpec`은 `agents: Vec<AgentSpec>` 중심이다. `done`은 generation과
실행 여부를 확인한 뒤 완료로 기록하며 검증 명령을 실행하지 않는다.
관리형 런처는 주요 provider에 권한 우회 플래그를 기본 추가한다.

- 근거: [work_pipeline_spec.rs](../crates/muxa/src/work_pipeline_spec.rs),
  [pipeline_run.rs](../crates/muxa/src/pipeline_run.rs)의 `done`,
  [agent_launch.rs](../crates/muxa-cli/src/agent_launch.rs)의 `native_launch`.
- 개선: 기존 TOML의 agent 형식은 유지하되 내부에 AgentAttempt와
  VerificationAttempt를 구분한다. 초기에는 agent에 딸린 선택적 검증이면 충분하다.
  범용 workflow DSL은 만들지 않는다.
- `completion_reported`, `verification_passed`, `review_accepted`의 증거를 구분한다.
  pipeline dependency가 요구하는 완료와 Work의 수동 보드 단계는 별개로 유지한다.
- role, 실제 launch access, provider 기능을 분리한다. 자유 CLI options가
  선택한 권한과 충돌하면 실행 전에 거부한다. prompt만으로 read-only를 보장하지 않는다.
- 검증 명령 자체의 cwd·환경·timeout·자식 프로세스 정리 정책도 agent와 별도로 정의한다.

### E. 현재 기록은 풍부하지만 명령부터 결과까지의 연결이 불완전하다

activity, prompts, collaboration audit, pipeline 상태, work operation이 각각 있다.
mailbox에는 이미 thread·parent·run·artifact가 있으므로 새 메시지 저장소는 필요 없다.
부족한 부분은 start → launch → prompt → done → verification → review를 같은
실행 식별자로 조회하고, 어떤 코드 상태에 대한 결과인지 확인하는 계약이다.

- 개선: 새 runtime event에 run/attempt/operation과 원인이 된 request를 연결한다.
  기존 mailbox·AIR artifact를 참조하고 메시지·transcript 본문을 중복 저장하지 않는다.
- event에는 sequence와 시간, 결과에는 명령·도구/설정 버전·입력 digest·코드 fingerprint를 남긴다.
  credential과 민감한 환경 값은 기록하지 않는다.
- 화면 알림은 기존 revision 방식으로 유지한다. 모든 terminal byte를 영속 이벤트로 바꾸지 않는다.
- IPC capability는 이미 있으므로 재발명하지 않는다. 새 runtime API에 한정해
  schema, 오류 코드, fixture, 구버전 호환 테스트를 함께 제공한다.

## 목표 경계

```text
CLI / MCP / Watch / Web / Mac
              │ 같은 명령·조회 계약
              ▼
       muxad RuntimeService
       ├─ 실행 정책 / provider capabilities / 검증
       ├─ 영속 command·run·attempt·event 저장소
       └─ bounded executor / 복구 / 취소
              │
              ├─ tmux 등 pane backend
              ├─ native PTY
              └─ Fleet의 원격 daemon

관찰 hook·screen·backend → 기존 registry → 실행 상태와 대조
mailbox·AIR             → 기존 저장소    → request·artifact 참조
```

결정과 외부 부작용은 같은 DB transaction으로 묶을 수 없다. 먼저 실행 의도를
저장하고, backend 결과를 기록하며, 끊어진 구간은 작업별 관찰·복구 정책으로 해결한다.
재시작만으로 임의 외부 작업의 exactly-once 실행을 보장한다고 주장하지 않는다.

저장소 제안은 새 runtime journal에 한정한 SQLite다. command·attempt·event를
한 transaction으로 갱신하기에 적합하며 이미 사용 중인 의존성이다. state snapshot,
activity ledger, mailbox를 모두 이전하지 않는다. 다른 저장소와의 연동은 durable outbox와
중복 처리 키를 사용하거나 참조로 연결하며, 여러 파일에 걸친 원자성을 가정하지 않는다.

## 기존 계획을 병합한 실행 순서

| 단계 | 범위 | 완료 기준 |
| --- | --- | --- |
| 0. 현재 보장 고정 | 지연 claim, 응답 유실, pane 생성 직후 종료, daemon 재시작의 회귀 시나리오; 해당 reconciler timeout·종료 보강 | 확인된 장애와 설계 위험을 분리하고, 기존 pane adoption·세대 검증을 보존 |
| 1. 실행 기반 | 영속 ID·명령 journal·attempt·fencing; Work 실행 service로 책임 이동; 명령별 취소·복구 정책 | CLI와 daemon 재시도에서 동일 요청 결과 재조회; 낡은 attempt report 거부; UI 종료와 실행 수명 분리 |
| 2. 권한·provider 계약 | 관리형 launch의 capability와 access 매핑; raw options 검증; 기존 설정 호환 정책 | CLI/MCP/Mac/Web이 같은 권한을 실행·표시하고 미지원 조합을 실행 전에 거부 |
| 3. 완료 검증 | 선택적 verify 설정; durable verification attempt와 로그·코드 증거 | 실패하면 후속 단계 차단, 통과하면 한 번만 개방; 오래된 코드·세대 결과 거부 |
| 4. Work 결과 검토 | 변경 파일·diff·검증 결과·기존 mailbox 피드백 연결 | 한 Work에서 변경 검토부터 수정 요청까지 가능; 검증 후 변경은 오래된 결과로 표시 |
| 5. 세션 분기와 재개 | 정확한 provider session ID 기반 fork; parent 관계와 인계 자료 참조 | 원본은 유지, 새 session 계보 보존; 대화 복제와 checkout 격리를 구분 |

권한 단계는 기존 계획보다 앞당긴다. 검증 executor와 이후 자동화까지 같은 실행 정책
위에 올리는 편이 두 번 구현하지 않는 방법이다. 관측용 correlation ID와 API 호환성은
별도 후반 프로젝트가 아니라 1단계부터 각 변경에 포함한다.

## 검증 기능의 수정된 설계

외부 설정은 이전 제안처럼 agent 아래 선택적 `verify`를 두되 내부 실행은 독립 Attempt다.
`work done` → 완료 주장 저장 → 검증 대기/실행 → 통과한 경우 dependency 개방 순서다.
검증이 없는 기존 pipeline은 기존 완료 의미를 유지한다.

검증 대상에는 최소한 repository/worktree, HEAD, tracked/index 변경 및 포함하는
untracked 파일의 digest, command/config digest를 기록한다. `.gitignore` 파일,
외부 서비스와 환경까지 재현 가능하다고 표시하지 않는다. 비 Git 작업은 다른 input
identity를 사용하고 코드 검증 보장 범위를 별도로 표시한다.

같은 checkout이 실행 중 바뀌면 결과를 현재 코드의 성공으로 승격하지 않는다.
실행 전후 fingerprint 비교만으로 중간 변경이 전혀 없었다고 증명할 수는 없다.
초기 strict 검증은 해당 checkout의 muxa 관리 writer를 멈춘 상태 또는 별도 snapshot에서
실행하고, 외부 writer까지 통제하지 못한 live 검증은 그 한계를 결과에 남긴다.
강한 보장이 필요한 snapshot 검증은 원본 dirty 변경과 필요한 untracked 입력을 포함한
정확한 snapshot 생성 규칙을 먼저 정의한다.

timeout·취소·daemon 중단은 성공/테스트 실패와 구분한다. 설정이 허용한 순수·반복 가능한
검증만 자동 재시도하며, 임의 command는 외부 부작용이 없다고 가정하지 않는다.
검증 병렬 수와 로그 보존량을 제한해 agent 실행을 굶기지 않게 한다.

## PR 단위와 전환 정책

1. **복구 경계 고정:** 0단계의 fault injection 회귀 시험과 reconciler 수명 제한.
2. **ID·저장 모델:** Run/Attempt/Operation 스키마, 저장·조회, 이전 JSON import와 fixture.
3. **명령 소유권 이동:** CLI 실행 정책을 library runtime으로 추출하고 daemon이 실행.
   외부 명령 의미는 유지하며 launch/done/status부터 공통 API로 연결한다.
4. **중복·취소·복구:** command idempotency, claim fencing, bounded worker,
   ambiguous delivery와 재시작 복구를 전체 경로로 검증한다.
5. **권한 계약:** provider adapter별 launch profile과 capability, 충돌 options 거부.
6. **검증 수직 구현:** `pair` 한 개에서 구현 → 테스트 → 리뷰를 CLI/daemon으로 완성.
7. **결과 검토 UI:** 먼저 Mac Work 상세에서 전체 흐름을 완성하고 TUI·웹으로 확장.
8. **fork:** 동일 capability·ID·journal·artifact 계약 위에 추가.

각 PR은 작동하는 호환 경로를 유지한다. journal 도입과 service 이동은 함께 검토하되
중간 버전에서도 새 기능을 노출하기 전에 명령 저장과 부작용 실행을 연결해야 한다.
새 capability가 없는 원격 daemon에는 기존에 지원하던 동작만 제공하고 새 보장을
제공하는 것처럼 fallback하지 않는다.

이전 pipeline JSON은 한 번 import하고 원본을 보존한다. 복원할 수 없는 과거 시도
이력을 만들어내지 않으며 import 시점의 legacy snapshot임을 표시한다. 전환 뒤에는
같은 상태를 두 저장소에서 쓰지 않는다. 구버전 rollback은 백업과 구버전 상태의
복구 절차를 사용하고 새 schema를 조용히 덮어쓰지 않는다.

권한이 없는 기존 설정의 의미는 명시적 legacy 동작으로 표시하고 migration 경로를
제공한다. 새 preset에는 권한을 명시한다. 이미 실행 중인 pane의 권한은 설정을
바꿨다고 바뀐 것으로 표시하지 않는다.

## 필수 장애 시나리오

- 동일 command 동시 제출과 완료 응답 유실 후 재전송: 같은 operation과 결과.
- 같은 key에 다른 입력: 충돌 오류. 의도적인 새 요청: 새 operation.
- claim이 15초 이상 지연되고 재획득된 뒤 예전 실행자가 report: 거부.
- pane 생성 후 기록 전 중단: 정확한 기존 pane을 adopt하거나 ambiguous로 표시.
- 프롬프트 전송 후 응답 유실: 수신 확인 없이 자동 중복 전송하지 않음.
- 검증 실행 중 무효화·코드 변경·daemon 종료: 예전 통과로 후속 실행을 열지 않음.
- 한 Work의 느린 executor: 다른 Work의 조회·실행·취소를 무한 차단하지 않음.
- daemon 재시작: operation ID 재사용 없음; 이전 결과·중단 상태 조회 가능.
- native/tmux/Fleet capability 차이, pane ID 재사용: 지원하지 않거나 다른 대상이면 거부.
- CLI/MCP/Web/Mac의 동일 요청: 같은 정책과 결과, 구버전 client에는 명확한 미지원 응답.

## 이번 범위 밖

외부 workflow 서버 도입, 전체 event sourcing 전환, 전 저장소 통합,
완전한 ACP 구현, 동적 plugin marketplace, 자동 병합, 달력 scheduler는 후속 과제다.
공유 checkout의 경로 예약도 검증 대상 고정과 실행 소유권을 먼저 해결한 뒤 평가한다.
예약 자체가 파일 쓰기를 강제로 막는 것은 아니다.

최초 제품 목표는 작은 pair pipeline에서 **실행 → 권한 확인 → 검증 → 리뷰**가
재시작과 재시도에도 추적 가능한 한 흐름으로 작동하는 것이다.
