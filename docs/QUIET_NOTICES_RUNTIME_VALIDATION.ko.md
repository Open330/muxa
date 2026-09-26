# 협업 알림 실환경 검증 — 2026-09-26

## 결과

`0.8.51+quiet.1`을 실제 CLI와 데몬에 설치하고 `/tmp/muxa-1044.sock`에서
실행 중인 데몬을 generation 15 → 16으로 재시작했다.
같은 rmux 서버의 전용 테스트 Work를 사용해 배포 전후를 검증했다.
일반 notice의 자동 터미널 입력을 없애면서 인계·완료 전달은 유지했다.

| 실제 실행 환경에서의 검사 | 결과 |
| --- | --- |
| 변경 전 0.8.51에서 일반 notice 1건 | 실제 수신 터미널에 알림 표시 |
| 변경 후 일반 notice 39건 | 알림 0건, 약 37.79초 동안 터미널 출력 동일 |
| 30초 주기 데몬 재검사 이후 수신함 조회 | 39건 모두 보존·완료 처리 |
| `notify=true` 인계 | 약 0.24초 뒤 관측, 알림 표식 1회 |
| 수신자 working 상태에서 인계 | 2초 관찰 동안 미전달, idle 전환 후 전달 |
| `notify=false` 질문을 조회·완료 처리 | 발신자에게 완료 알림 표식 1회, 응답 본문은 삽입하지 않음 |
| 기존 기능 호환성 | 기존 MCP 도구 27개와 입력 필드 유지, daemon capability 누락 없음 |
| 테스트 정리 | 요청 43건 모두 completed, 전용 Workspace와 두 프로세스 종료 |

테스트 터미널은 Muxa의 `work start`로 만든 전용 rmux Work에서 실행한
`codex app-server` 두 프로세스였다. 모델 turn은 요청하지 않았다.
테스트 하네스가 전용 세션의 상태 이벤트와 durable IPC 요청을 보냈으며,
실제 설치된 daemon → rmux → 터미널 전달을 capture로 확인했다.
이는 모델의 주의 전환 시간이나 전체 업무 처리량을 측정한 시험은 아니다.
단일 인계의 0.24초는 이 실행에서의 관측값이며 지연 SLA가 아니다.
기존 운영 에이전트에게는 테스트 요청을 보내지 않았다.

## 소스와 배포

- 전용 작업 트리: `/home/june/personal/muxa-wt/quiet-notices-runtime`
- 브랜치: `fix/quiet-notices-runtime`
- 기반: `8a56c67` (0.8.51 및 설치 당시 CLI/reload 수정 포함)
- 로컬 빌드 버전: `0.8.51+quiet.1`
- 설치 방법: 각 crate에 대해 `cargo install --path ... --locked --offline --force`
- 실행 반영: `muxa daemon restart`, hello 응답의 버전과 generation으로 검증
- 설치된 협업 스킬과 bootstrap도 부분 갱신했다. 기존 human feedback 및
  peer interruption recovery 문구는 보존했다.

기존 설치 이력의 `/tmp/muxa-reload` 소스가 삭제돼 있었으므로 원본 바이너리의
정확한 빌드 트리는 복원하지 못했다. 설치 당시 0.8.51 커밋을 기반으로
통합하고, 기존 도구·입력·프로토콜 capability를 대조해 호환성을 검증했다.
기존의 0.8.46 작업 트리로 운영 바이너리를 교체하지 않았다.

기존에 열린 MCP 연결은 도구 schema를 캐시할 수 있다. 기본 notice 억제는
데몬에서 즉시 적용된다. 기존 연결에 `notify`가 보이지 않는 경우 필요한
인계는 `muxa msg send TARGET BODY --kind notice --notify true`를 사용하고,
MCP 재연결 시 새 schema를 읽는다. 실행 중인 다른 에이전트는 재시작하지 않았다.

## 검증 및 증거

0.8.51 기반 통합본에서 저장소 59개, 데몬 108개, MCP 37개, CLI 옵션 1개
테스트가 통과했다. Clippy 전체 대상 검사와 rustfmt도 통과했다.
Clippy에 맞춰 기존 복구 테스트의 기본값 초기화를 정리한 후 해당 테스트를
다시 통과시켰다. 이 마지막 수정은 테스트 코드에만 해당한다.

- [배포 전 관측](validation/quiet-notices-runtime/baseline.json)
- [배포 후 관측과 요청 ID](validation/quiet-notices-runtime/after.json)
- [MCP 호환성 검사](validation/quiet-notices-runtime/schema-comparison.json)
- [배포 경로 및 바이너리 SHA-256](validation/quiet-notices-runtime/deployment.json)
- [정리 결과](validation/quiet-notices-runtime/cleanup.json)
- [사용한 하네스 기록](validation/quiet-notices-runtime/probe.py.txt)

하네스 기록은 이번 실행의 소켓·전용 Work·세션 좌표를 담은 감사용 사본이다.
테스트 Workspace는 이미 닫았으므로 그대로 다시 실행하는 재사용 스크립트가 아니다.

## 복구

이전 CLI·데몬·스킬·bootstrap은 다음 경로에 보관했다.

`/home/june/.local/state/muxa/backups/quiet-notices-20260926`

복구가 필요하면 백업 바이너리를 임시 파일로 복사한 뒤 원자적으로 교체하고
`muxa daemon restart`로 반영한다. 실행 파일에 직접 덮어쓰지 않는다.
스킬과 bootstrap도 백업본으로 복원한다. 복구 후 hello의 버전과 generation을
다시 확인한다. 이번 검증에서는 롤백하지 않았고 개선 버전이 실행 중이다.
