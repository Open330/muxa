# Runtime 성능 점검

2026-09-26, Linux x86_64 / rustc 1.98.0 기준. daemon 종료, Fleet 갱신·명령 전달,
pipeline readiness·저장, 상태 이벤트 fanout, snapshot 저장 및 협업 조회 경로를 점검했다.
아래 수치는 합성 데이터 또는 격리된 로컬 daemon의 관측값이다.
SSH 네트워크·실제 agent 실행·장시간 운영의 전체 성능 보장은 아니다.

## 반영한 개선

- 모든 IPC 관측 구독은 daemon 종료 상태를 받아 대기 중인 쓰기까지 취소한다.
  이미 접수한 변경 요청은 기존 drain을 유지한다.
- 로컬 Fleet는 협업 revision을 내부 watch 채널로 받는다. 자기 자신의 IPC에
  연결하고 재시도하던 경로를 제거했다. 구독 후 task 시작 전에 발생한 변경도 전달한다.
- 로컬·원격 host 갱신은 대상 host만 복사한다. 기존에는 한 host를 찾기 위해
  전체 Fleet snapshot을 복사·정렬했다.
- pipeline supervisor는 저장소 안에서 준비 여부만 판단한다. 모든 Run의 prompt와
  상태를 복사한 뒤 boolean을 계산하던 작업을 제거했다.
- 포화된 host 명령 큐는 즉시 재시도 오류를 반환한다. 공유 router가 해당 큐에서
  대기해 다른 host 요청과 종료 신호까지 막는 경로를 제거했다.

## 조회 비용 비교

`cargo bench -p muxa --bench runtime_reads`로 재현한다. 벤치마크는 같은 저장소에서
기존 조회식과 새 조회식을 각각 200회 실행한다. 빌드 완료 후 실행 파일을 3회
반복 실행했고, 아래는 각 실행의 평균 중 중앙값이다. p95/p99가 아니다.

Fleet 데이터는 host마다 agent 10개, agent마다 prompt·response 합계 8 KiB다.
Pipeline은 Run마다 prompt 32 KiB이며, 모든 Run을 검사하도록 active claim을 설정한다.
등록·저장 비용은 측정 구간에서 제외한다.

| 조회 | 규모 | 기존 평균 | 개선 후 평균 |
|---|---:|---:|---:|
| 단일 Fleet host 조회 | host 1개 | 2.82 µs | 2.68 µs |
| 단일 Fleet host 조회 | host 10개 | 305.12 µs | 2.72 µs |
| 단일 Fleet host 조회 | host 100개 | 3,996.51 µs | 3.16 µs |
| Pipeline 준비 여부 | Run 1개 | 0.79 µs | 0.07 µs |
| Pipeline 준비 여부 | Run 100개 | 435.18 µs | 4.74 µs |
| Pipeline 준비 여부 | Run 1,000개 | 18,555.72 µs | 62.46 µs |

이 수치는 해당 메모리 조회 구간만 비교한다. Fleet 전체 갱신에는 backend 조회와
직렬화 비교가 남아 있고, pipeline 실행에는 저장과 프로세스 실행 비용이 남아 있다.

## 종료·이벤트 검증

- 수정 후 격리된 debug daemon에서 외부 구독 0·6·60·60·60개를 유지했다.
  SIGTERM부터 process exit까지 각각 31.4·15.3·15.3·31.5·63.5 ms였고
  IPC drain timeout은 없었다. 로컬 Fleet의 자기 자신에 대한 IPC 구독도 없었다.
- 실제 socket의 송신 버퍼를 채운 상태에서도 구독 취소가 완료되는 회귀 시험을 둔다.
  변경 요청 drain, 늦은 stream takeover, 여섯 종류의 구독 종료도 검증한다.
- 기존 `store_apply` release 벤치마크에서 구독자 0·1·2·4·8·16개에 대해
  10,000개 상태 변경을 처리했다. 변경당 producer 비용은 1.11–3.12 µs였으며,
  해당 실행에서는 모든 구독자가 기대한 이벤트 수를 수신했다.
  기존 `Arc<Agent>` fanout은 유지한다.
- snapshot 저장은 이미 변경 알림과 debounce를 사용하며, backend 관찰은
  blocking 작업을 분리하고 있다. 이번 변경에서 이 계약은 바꾸지 않았다.

## 추가 측정이 필요한 부분

성능 문제가 전혀 남지 않았다고 단정하지 않는다. 다음은 코드에서 확인한 비용
후보이며, 이번 조회 벤치마크로 병목 여부나 개선 효과를 확정하지 않았다.

1. PipelineRun은 변경 시 전체 JSON 파일을 직렬화하고 fsync한다. Run 수별 저장
   지연·잠금 대기를 측정한 뒤 영속 operation/attempt 설계와 함께 행 단위 저장을 검토한다.
2. 협업 SQLite 접근은 동기 호출이며, 연결·schema 준비와 busy 대기가 async 요청
   경로에 포함된다. 큰 이력과 외부 DB 잠금 시 IPC 지연을 측정한 뒤 전용 DB worker나
   blocking 실행 경계를 검토한다. 저장 완료 전에 성공을 반환하는 방식으로 바꾸지 않는다.
3. Fleet payload 변경 비교는 JSON 직렬화를 사용한다. 큰 host의 주기 갱신 비용을
   측정한 뒤 구조적 비교나 revision 기반 판정을 검토한다.
4. 느린 backend·원격 명령, IPC idle 연결, 높은 요청률에서 CPU·RSS와 요청 종류별
   p95/p99를 측정해야 한다. 이번 유휴 구독 종료 결과로 이 시나리오까지 보장하지 않는다.

## 머지 전 자체 리뷰

최신 main 통합 과정에서 다음을 확인하고 보완했다.

- main의 기존 `until_server_shutdown`과 idle 연결 종료 처리를 유지했다.
  중복 종료 채널을 추가하지 않고 여섯 구독의 회귀 시험을 기존 경계에 적용했다.
- 구독 시작 응답(ack)도 socket 쓰기에서 대기할 수 있다. ack부터 종료 취소를
  적용했고, 실제 송신 버퍼를 채운 회귀 시험으로 검증한다.
- 기존 로컬 Fleet IPC 재연결은 저장된 답변을 다시 읽도록 최초 invalidation을
  발행했다. 내부 watch 전환도 `mark_changed`로 이 동작을 유지하며, 이미 관측한
  revision에서도 시작 알림이 전달되는지 시험한다.
- 자동 workspace snapshot 등 최신 main의 별도 작업은 보존했다. 변경 범위의
  mutation drain, claim/retry, host 큐 과부하 오류, scoped 조회의 의미를 재검토했다.

위 벤치마크는 최초 개선 브랜치에서 측정한 값이며, main 통합 후 전체 서비스의
성능 측정값으로 확대 해석하지 않는다. 머지 판단은 수정 범위의 결함과 회귀 시험,
전체 테스트 및 CI를 기준으로 한다. 앞서 적은 대용량 저장·DB 잠금 부하 측정은
후속 항목이며, 모든 가능한 성능 문제가 없다는 주장은 하지 않는다.
