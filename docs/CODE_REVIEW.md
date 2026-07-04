# Code Review

검토일: 2026-07-04

## 요약

이번 패치는 wire protocol 호환성을 깨뜨릴 수 있으며, 여러 줄 붙여넣기를 안전하지 않게 처리합니다. 또한 Pi 이벤트 전달 순서가 뒤바뀔 수 있고, Pi의 응답·비용 집계·타임라인 필터 경로가 완전하지 않습니다.

## 검토 의견

### P1 — 새 wire variant에 맞춰 protocol version 갱신

- 위치: `crates/muxa/src/event.rs:28`
- `PROTOCOL_VERSION`이 3으로 유지되면 기존 v3 클라이언트가 새 daemon을 호환 가능하다고 판단합니다.
- 그러나 snapshot에 `kind: "pi"` 또는 `surface.kind: "cmux"`가 포함되면 역직렬화할 수 없습니다.
- 이 경우 `Client::snapshot`이 빈 vector로 대체하여 모든 agent가 사라진 것처럼 보입니다.
- 새 protocol version과 downgrade 처리를 추가하거나, 최소 지원 version을 올려야 합니다.

### P1 — relay 중 bracketed-paste framing 보존

- 위치: `crates/muxa-cli/src/main.rs:519-521`
- attached child가 bracketed paste를 지원하더라도 crossterm은 `Event::Paste`를 만들기 전에 `CSI 200~` / `CSI 201~` delimiter를 제거합니다.
- 현재처럼 text만 전송하고 LF를 CR로 변환하면 여러 줄 붙여넣기가 일반 Enter 입력처럼 동작하여 각 줄이 즉시 실행될 수 있습니다.
- relay할 때 bracketed-paste 시작·종료 framing을 복원해야 합니다.

### P1 — Pi lifecycle 이벤트 전달 직렬화

- 위치: `crates/muxa-cli/src/init/files/pi.rs:59-61`
- 각 callback이 별도의 detached CLI를 시작하고 즉시 반환하므로 빠르게 연속된 lifecycle 이벤트가 muxad에 도착하는 순서가 바뀔 수 있습니다.
- 늦게 도착한 `agent_end`가 이미 반영된 `session_shutdown` row를 다시 Idle로 만들거나, 늦은 tool-start가 완료된 turn을 Working 상태로 남길 수 있습니다.
- callback 자체는 non-blocking으로 유지하되 이벤트 전달은 직렬화해야 합니다.

### P2 — Pi assistant content block에서 text 추출

- 위치: `crates/muxa-cli/src/init/files/pi.rs:121-122`
- Pi의 `AssistantMessage.content`는 문자열이 아니라 text, thinking, tool-call block 배열입니다.
- 현재 구현에서는 정상 assistant reply도 `undefined`가 되어 `last_response`가 채워지지 않습니다.
- 그 결과 성공한 `TurnStopped`가 기존 Error 상태를 해제하지 못할 수 있습니다.
- 마지막 assistant message에서 text block을 추출해야 합니다.

### P2 — Pi session 누적 비용 전송

- 위치: `crates/muxa-cli/src/init/files/pi.rs:113`
- `turn_end.message.usage.cost.total`은 해당 assistant message 한 건의 비용입니다.
- 반면 `Store::apply_heartbeat`는 `Agent.cost_usd`를 덮어쓰고 UI는 이를 session 누적 비용으로 표시합니다.
- 여러 turn 이후 최신 turn 비용만 보이거나 비용이 감소할 수 있습니다.
- 현재 session branch의 assistant-message 비용을 합산해서 전달해야 합니다.

### P2 — timeline filter parser에 Pi 추가

- 관련 위치: `crates/muxa/src/event.rs:28`
- `AgentKindArg`에 Pi가 없어 `muxa timeline --agent pi`가 거부됩니다.
- `dashboard::server::parse_agent_kind`에도 Pi가 없어 `/api/timeline?agent=pi`가 HTTP 400을 반환합니다.
- 두 parser 모두 Pi를 지원하도록 갱신해야 합니다.

## 완료 조건

- 새 variant를 모르는 구버전 client가 호환 가능한 daemon으로 잘못 인식하지 않는다.
- 여러 줄 paste가 하나의 bracketed-paste 입력으로 child에 전달된다.
- Pi lifecycle 이벤트가 발생 순서대로 muxad에 전달된다.
- 마지막 Pi assistant 응답의 text가 `last_response`에 반영된다.
- Pi 비용이 현재 session branch의 누적 비용으로 표시된다.
- CLI와 dashboard API에서 `pi` timeline filter가 동작한다.
