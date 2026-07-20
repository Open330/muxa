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
{ "protocol": 2, "kind": "hello", "client": "muxa/0.6.0" }
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

## Control methods

These methods let a client **drive** agents rather than only observe them.
They back muxa's control plane — the `muxa mcp` MCP server exposes each as a
tool so a coding agent can orchestrate the others (see `docs/MCP.md`).

**Safety.** `send_prompt` injects keystrokes into another agent's pane — a
control action, not a read. The IPC socket is already owner-only (`0600`,
chmod'd after bind — see [Transport](#transport)), so only the daemon's own
user can invoke it; there is no network exposure and no additional
authentication layer. Treat socket access as equivalent to shell access for
that user.

**Pre-1.0 evolution.** These methods are additive (they do not bump
`PROTOCOL_VERSION`). Like the rest of the pre-1.0 surface they may change
shape between minor releases; pin a muxa version if you build on them.

#### `send_prompt`

Inject `text` into `pane` as literal keystrokes. The daemon resolves the
backend from the pane-id namespace (`%…` → tmux, `herdr:…` → herdr, …),
falling back to the primary backend **only for ids it can't classify**. When
`submit` is true a trailing carriage return is sent as a **second, separate**
injection so the agent's current line is committed (byte-identical to tmux
`send-keys Enter` and to writing a CR to a herdr pane's pty).

**Server targeting.** A tmux pane id like `%5` exists on *every* running tmux
server, so the daemon pins the injection to the specific server the pane's
agent row was recorded on (its `tmux_socket`), passing `tmux -S <socket>`. An
untracked pane, or a host without a per-server socket concept (herdr), falls
back to the env-scoped default. This is what makes `send_prompt` land on the
right pane in a multi-server setup.

```json
{ "protocol": 3, "kind": "send_prompt", "pane": "%12", "text": "run the tests", "submit": true }
```

Response (text landed): `{ "ok": true, "protocol": 3, "sent": true, "submitted": true }`.

The text-send and the submit CR are two **non-atomic** injections, reported
distinctly so a caller can act on a partial failure without double-injecting:

- `sent` — the text landed. When `ok` is `true`, `sent` is `true`; a caller
  **MUST NOT resend the text** even if `submitted` is `false`.
- `submitted` — the submit CR landed and committed the line. `false` when
  `submit:false` was requested (nothing to submit), or — with `submit:true` —
  a **partial failure**: the text is typed but not committed, so retry the
  *submit alone* (never the whole prompt).

The submit CR is attempted **only** when the text send succeeded — a text-send
failure never masquerades as a submit failure, and a submit failure never
looks like a total failure (which would drive a double-inject retry). A total
text-send failure is an `ok:false` error (`"send_text failed: …"`); nothing
landed, so the whole send is safe to retry.

Refused with a **structured error** (never a panic), and with no injection
attempted, in two cases:

- The pane classifies to a KNOWN namespace whose backend is **not observed**
  by this daemon — routing it to another host would inject into the wrong
  pane, so it is refused rather than falling back:

  ```json
  { "ok": false, "protocol": 3, "error": "namespace unavailable: no active herdr backend for pane herdr:3" }
  ```

- The resolving backend lacks the `send_text` capability — e.g. zellij, whose
  CLI `write-chars` only reaches the focused pane and so can't safely target
  an arbitrary pane id:

  ```json
  { "ok": false, "protocol": 3, "error": "backend zellij does not support send_text (pane zellij:3)" }
  ```

`text` is sent literally, so it is never reinterpreted as a key name. On tmux,
simple single-line text uses `send-keys -l -- <text>` (the `--` keeps text
that *begins* with `-` from being parsed as a flag — MCP forwards arbitrary
model text). Text with an embedded **newline** or a **trailing `;`** is sent
via a bracketed paste (`load-buffer` + `paste-buffer -p`) instead: a raw
`send-keys -l` would replay each newline as an Enter (submitting a multi-line
prompt line-by-line) and tmux would eat a trailing `;` as a command separator.
The paste path is best-effort for submit semantics — a paste-aware target
(Claude Code, a modern readline shell) inserts the block without executing
intermediate newlines, but a non-paste-aware target may still run them
line-by-line. On herdr, `pane.send_text` delivers the whole block (newlines
included) literally over the socket, with no per-line submit.

#### `capture`

Capture the visible contents of `pane` via the namespace-resolved backend.
Like `send_prompt`, the capture is pinned to the specific server the pane's
agent row was recorded on (its `tmux_socket`) so a shared pane id reads the
right screen, and it is refused with the same `namespace unavailable` error
when the pane classifies to a known-but-unobserved namespace (capturing via
the wrong backend would read a different host's screen).

```json
{ "protocol": 3, "kind": "capture", "pane": "%12" }
```

Response: `{ "ok": true, "protocol": 3, "capture": "<visible pane text>" }`.
`capture` is `null` when the pane is gone or the backend can't capture
(best-effort — never an error). A `namespace unavailable` refusal is an
`ok:false` error, distinct from a `null` capture.

#### `subscribe`

Long-lived push stream. The server replies with a one-shot `ok` ack, then
writes one JSON-encoded [`Transition`](#transition-schema-in-subscribe-stream)
per state change (newline-delimited) until the client closes the socket. The
stream is the same broadcast the daemon's notifier and activity ledger
consume, so subscribers see every committed transition.

```json
{ "protocol": 3, "kind": "subscribe", "lagged_markers": true }
```

`lagged_markers` (optional, default `false`) opts the connection into the
lagged-marker control frame described below. It defaults **off** so a
pre-marker client — whose `Transition` parser would choke on the frame and
abandon push mode — keeps the historical behavior: on overflow the server
silently continues and the client reconciles via its fallback `snapshot` poll.
muxa's own client (used by `muxa watch` and `muxa mcp`) sends
`lagged_markers: true`, because its `TransitionStream` reader understands the
frame.

Two non-`Transition` control frames are interleaved on the stream and MUST be
tolerated by readers:

- **Keepalive** — a bare empty line the daemon writes on an idle stream to
  detect a dead client (broken pipe). Skip it. Always emitted.
- **Lagged marker** — `{"event":"lagged","dropped":N}` when the client
  consumed too slowly and the broadcast buffer overflowed, dropping `N`
  transitions. **Emitted only to connections that sent
  `lagged_markers: true`.** The stream continues; the client should reconcile
  the gap with a fresh `snapshot`. muxa's own `TransitionStream` reader skips
  this frame automatically.

Used by `muxa watch` (to replace 500 ms polling with push updates) and by
`muxa mcp`'s `muxa_wait_for_change` tool.

##### `Transition` schema (in `subscribe` stream)

```json
{
  "from": "idle",
  "to": "working",
  "agent": { <Agent>, ... }
}
```

`from`/`to` are `AgentState` values; `agent` is the post-transition
[`Agent`](#agent-schema-in-responses) row.

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
  "tmux_socket": "string | absent",
  "cwd": "string | null" }
```

`tmux_socket` (additive, optional): absolute path of the tmux server
socket `pane` belongs to — the first comma-separated field of `$TMUX` at
hook time. Pane ids are only unique per server, so this disambiguates
panes on non-default servers (e.g. a dedicated `tmux -L amux` server).
Adapters that predate the field simply never send it.

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
  "tmux_socket": "amux",
  "tmux_session": "amux-spike",
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

`tmux_socket` and `tmux_session` are additive and optional. `tmux_socket`
is the short socket name (the socket file's basename, e.g. `default` or
`amux`) — from the adapter's `$TMUX` or backfilled by the daemon's
reconciler. `tmux_session` is backfilled by the reconciler's multi-socket
pane scan each tick; it is absent until the first tick after the agent's
pane is seen.

## `HistoryEntry` schema (in `recent_prompts` responses)

Prompt-history entries are a retained audit log, not the live agent row:
many entries can point at the same pane/session.

```json
{
  "v": 1,
  "kind": "claude_code",
  "session_id": "sess-abc",
  "pane": "%12",
  "tmux_session": "main",
  "cwd": "/home/user/proj",
  "prompt": "fix this bug",
  "at": "2026-04-24T12:00:00Z",
  "model": "sonnet"
}
```

`tmux_session`, `cwd`, and `model` are optional. Older `prompts.ndjson`
lines may not have `tmux_session` or `cwd`; readers should treat them as
unknown.

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
  "session_name": "main",
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

`pane`, `session_name`, `cwd`, and `state_entered_at` are optional on
state transitions. Foreground intervals are closed when the tmux session
detaches or disappears, so they remain reportable after the tmux session
is gone.

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
