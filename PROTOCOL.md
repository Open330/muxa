# muxa wire protocol

This document describes the wire protocol spoken between
`muxa` (the CLI), third-party adapters, and `muxad` (the daemon).

Current version: **`1`** (defined in `muxa-core/src/event.rs`).

## Stability

Pre-1.0 (current): the wire protocol may change between **minor** releases
(0.x → 0.x+1). Adapters should pin a specific muxa version while we iterate
on the schema; the constants below (`PROTOCOL_VERSION`,
`HISTORY_SCHEMA_VERSION`, `STATE_SCHEMA_VERSION`) are negotiation tokens, not
stable API.

Post-1.0: the protocol is stable within a major version; breaking changes bump
`PROTOCOL_VERSION`'s major component and the crate's major version.

---

## Transport

- Unix-domain stream socket.
- Default path: `$XDG_RUNTIME_DIR/muxa.sock`, falling back to
  `/tmp/muxa-<uid>.sock`.
- Permissions: `0600` (owner-only). The daemon `chmod`s after bind.
- Encoding: **line-delimited JSON** (one UTF-8 JSON object per line, `\n`
  terminated). `serde_json` does not emit raw newlines inside values, so
  embedded newlines in string fields are escaped per RFC 8259.

Each connection is full-duplex: a client may issue multiple
request/response pairs over the same stream.

## Request

```json
{ "protocol": 1, "kind": "<method>", ...method-specific fields }
```

- `protocol` (required): integer. Must equal the server's
  `PROTOCOL_VERSION`. Mismatches produce `{"ok":false,"error":"protocol mismatch: …"}`.
- `kind` (required): string discriminant — see methods below.

### Methods

#### `ingest`

Fire an `AgentEvent` into the daemon's registry. Used by adapters.

```json
{ "protocol": 1, "kind": "ingest", "event": <AgentEvent> }
```

Response: `{ "ok": true, "protocol": 1 }`.

#### `snapshot`

Return all currently-tracked agents.

```json
{ "protocol": 1, "kind": "snapshot" }
```

Response:

```json
{ "ok": true, "protocol": 1, "agents": [ <Agent>, ... ] }
```

#### `by_pane`

Return agents currently associated with a tmux pane id.

```json
{ "protocol": 1, "kind": "by_pane", "pane": "%12" }
```

#### `by_session`

Return the agent with the given session id (as a single-element array,
or empty).

```json
{ "protocol": 1, "kind": "by_session", "session_id": "sess-abc" }
```

#### `health`

Liveness probe.

```json
{ "protocol": 1, "kind": "health" }
```

Response:

```json
{ "ok": true, "protocol": 1,
  "health": { "version": "0.0.1", "protocol": 1 } }
```

---

## `AgentEvent` schema

Tagged union. `type` field is the discriminant.

| `type`                | Fields                                                            | Meaning                                  |
|-----------------------|-------------------------------------------------------------------|------------------------------------------|
| `started`             | `id`, `at`                                                        | Session opened                           |
| `prompt_submitted`    | `id`, `prompt`, `at`                                              | User submitted a prompt (truncated ≤4KB) |
| `tool_started`        | `id`, `tool`, `at`                                                | Tool invocation began                    |
| `tool_completed`      | `id`, `tool`, `success`, `at`                                     | Tool invocation finished                 |
| `notification_fired`  | `id`, `level`, `message`, `at`                                    | Agent needs attention                    |
| `turn_stopped`        | `id`, `at`                                                        | Agent finished responding                |
| `session_ended`       | `id`, `at`                                                        | Session terminated                       |
| `heartbeat`           | `id`, `model?`, `context_used_pct?`, `cost_usd?`, `at`            | Periodic metadata refresh                |

`id`:
```json
{ "kind": "claude_code" | "codex" | "gemini_cli" | "opencode" | "unknown",
  "session_id": "string",
  "pane": "string | null",
  "cwd": "string | null" }
```

`level`:
`"info" | "needs_input" | "warning" | "error"`.

`at`: RFC 3339 UTC timestamp (`"2026-04-24T12:00:00Z"`).

---

## `Agent` schema (in responses)

Same fields as stored in the registry:

```json
{
  "kind": "claude_code",
  "session_id": "sess-abc",
  "pane": "%12",
  "cwd": "/home/user/proj",
  "state": "working" | "idle" | "waiting_input" | "error" | "stopped" | "starting",
  "last_prompt": "string | null",
  "last_notification": "string | null",
  "model": "string | null",
  "context_used_pct": 34.0,
  "cost_usd": 0.12,
  "started_at": "2026-04-24T12:00:00Z",
  "last_activity_at": "2026-04-24T12:03:21Z"
}
```

---

## Semantics

- **At-least-once delivery.** Adapters should retry `ingest` on transient
  socket errors. The daemon applies events idempotently per-field; no
  deduplication by event id (yet).
- **Ordering per session.** Adapters SHOULD send events in causal order
  for a given `session_id`. Out-of-order events produce incorrect state.
- **Late-arriving identity.** `pane` and `cwd` may be `null` on the first
  event; the daemon fills them in from subsequent events.
- **GC.** Agents in state `stopped` for more than 60 minutes are evicted
  by the daemon's GC sweeper (interval: 60s). The thresholds are
  hardcoded — they live as `const` in `muxad`, not in config.

---

## Versioning policy

- Additive changes (new event types, new optional fields) are minor and
  do **not** bump `PROTOCOL_VERSION`. Clients MUST ignore unknown fields.
- Removing or renaming fields, changing the meaning of a field, or
  changing wire framing bumps `PROTOCOL_VERSION`. The daemon rejects
  requests whose `protocol` does not match.
