# muxa wire protocol

This document describes the wire protocol spoken between
`muxa` (the CLI), third-party adapters, and `muxad` (the daemon).

Current version: **`2`** (defined in `muxa-core/src/event.rs`).

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

#### `recent_prompts`

Return retained prompt-history entries, newest first. `pane` is optional;
when absent, prompts from every retained pane are returned. `limit = 0`
or an absent `limit` means "all entries currently retained in memory",
still bounded by `[history].max_per_pane` and `[history].max_age_days`.

```json
{ "protocol": 2, "kind": "recent_prompts", "pane": "%12", "limit": 10 }
```

Response:

```json
{ "ok": true, "protocol": 2, "prompts": [ <HistoryEntry>, ... ] }
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

#### `hello`

Capability handshake. Optional, but clients SHOULD send it as the first
message on a freshly opened connection. Sending `hello` opts the
connection into *negotiated-protocol* mode: per-message `protocol`
fields become advisory, and the server transparently downgrades enum
variants the client's version doesn't understand. Connections that
never send `hello` keep the legacy strict-match behaviour (any
non-matching `protocol` is rejected).

```json
{ "protocol": 2, "kind": "hello", "client": "muxa/0.5.0" }
```

- `protocol` (required): the version the client wants to speak. Must
  fall in the server's `[min_protocol, max_protocol]` range; out-of-range
  hellos are rejected without pinning the connection.
- `client` (optional, informational): free-form `name/version` tag, used
  by the daemon for log lines.

Response:

```json
{
  "ok": true,
  "protocol": 2,
  "min_protocol": 1,
  "max_protocol": 2,
  "capabilities": ["waiting_choice", "needs_choice", "rate_limited"]
}
```

- `protocol`: the version the server agreed to speak — equals the
  client's requested `protocol`.
- `min_protocol` / `max_protocol`: inclusive range of protocol versions
  the server can serve via the negotiated regime (with on-the-fly
  downgrade where needed).
- `capabilities`: stable string tags advertising semver-additive
  features the server supports. Clients SHOULD feature-gate on these
  rather than comparing `protocol` integers, so adding a new tag is
  always non-breaking.

Capability tags currently advertised:

| tag              | meaning                                                                 |
|------------------|-------------------------------------------------------------------------|
| `waiting_choice` | server emits `AgentState::waiting_choice` (otherwise: `waiting_input`). |
| `needs_choice`   | server emits `NotificationLevel::needs_choice` (otherwise: `needs_input`). |
| `rate_limited`   | server emits the `rate_limited` event type and the `rate_limit_*` fields on `Agent`. |

#### v1-compat downgrade

When a client pins to `protocol: 1` via `hello`, the server rewrites
wire-visible enum variants on the outgoing payload so the v1 client's
deserializer doesn't choke on unknown values:

| v2 value                        | v1 substitute    |
|---------------------------------|------------------|
| `AgentState::waiting_choice`    | `waiting_input`  |
| `NotificationLevel::needs_choice` | `needs_input`  |

The downgrade applies to every response on the connection — including
streaming `Transition`s after a `subscribe` — until the client closes
the socket. It is JSON-tree-aware: only standalone enum string values
are rewritten, never substrings inside other strings (e.g. a
`last_prompt` that happens to contain the word `waiting_choice` is left
unchanged).

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
`"info" | "needs_input" | "needs_choice" | "warning" | "error"`.

`needs_choice` (since protocol v2) is `needs_input`'s menu-style sibling:
it signals the agent is blocked on a numbered selection (e.g., Claude
Code's `AskUserQuestion` / `ExitPlanMode`) rather than free-text input
or a yes/no permission. Adapters that can't distinguish should use
`needs_input`.

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
  "state": "working" | "idle" | "waiting_input" | "waiting_choice" | "error" | "stopped" | "starting",
  "last_prompt": "string | null",
  "last_notification": "string | null",
  "model": "string | null",
  "context_used_pct": 34.0,
  "cost_usd": 0.12,
  "started_at": "2026-04-24T12:00:00Z",
  "last_activity_at": "2026-04-24T12:03:21Z"
}
```

## `HistoryEntry` schema (in `recent_prompts` responses)

Prompt-history entries are a retained audit log, not the live agent row:
many entries can point at the same pane/session.

```json
{
  "v": 1,
  "kind": "claude_code",
  "session_id": "sess-abc",
  "pane": "%12",
  "cwd": "/home/user/proj",
  "prompt": "fix this bug",
  "at": "2026-04-24T12:00:00Z",
  "model": "sonnet"
}
```

`cwd` and `model` are optional. Older `prompts.ndjson` lines may not have
`cwd`; readers should treat it as unknown.

## `ActivityEntry` schema (in `activity.ndjson`)

Append-only duration ledger. Each line is a tagged JSON object with
`v = 1`; readers should skip unknown versions or malformed lines. Timestamps
are RFC 3339 UTC. `duration_secs` is stored redundantly so downstream tools
can aggregate without re-parsing timestamp math.

Closed agent state interval:

```json
{
  "type": "state_transition",
  "v": 1,
  "at": "2026-04-24T12:03:21Z",
  "kind": "codex",
  "session_id": "sess-abc",
  "pane": "%12",
  "cwd": "/home/user/proj",
  "from": "working",
  "to": "waiting_input",
  "state_entered_at": "2026-04-24T12:00:00Z",
  "duration_secs": 201
}
```

Closed tmux foreground interval:

```json
{
  "type": "session_foreground",
  "v": 1,
  "session_id": "$1",
  "session_name": "main",
  "started_at": "2026-04-24T12:00:00Z",
  "ended_at": "2026-04-24T12:30:00Z",
  "duration_secs": 1800
}
```

`pane`, `cwd`, and `state_entered_at` are optional on state transitions.
Foreground intervals are closed when the tmux session detaches or
disappears, so they remain reportable after the tmux session is gone.

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

- Additive **field** changes (new event types, new optional fields) are
  minor and do **not** bump `PROTOCOL_VERSION`. Clients MUST ignore
  unknown fields.
- Adding a new **enum variant** to a wire-visible enum (e.g.,
  `AgentState`, `NotificationLevel`) **does** bump `PROTOCOL_VERSION`,
  because serde's default behavior on an unknown variant value is to
  fail deserialization — an old client receiving a new variant would
  crash rather than ignore. The bump forces a daemon/CLI co-upgrade.
- Removing or renaming fields, changing the meaning of a field, or
  changing wire framing bumps `PROTOCOL_VERSION`. The daemon rejects
  requests whose `protocol` does not match.

### v1 → v2 (2026-05-08)

- Added `AgentState::waiting_choice` — menu-style user block, distinct
  from `waiting_input`.
- Added `NotificationLevel::needs_choice` — routes to `waiting_choice`.
