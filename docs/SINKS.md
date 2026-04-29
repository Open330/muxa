# Sinks

A *sink* is an opt-in fan-out from the muxa daemon to an external system.
Sinks consume the in-process broadcast channels the daemon already uses
internally (today: prompts; later: state transitions, heartbeats) and
forward selected events to a long-lived destination.

Sinks are **always off by default**. Adding a sink to your config never
sends a byte until you explicitly set `enabled = true` and supply the
required credentials.

## Available sinks

| Name                | Purpose                                              | Section                                  |
| ------------------- | ---------------------------------------------------- | ---------------------------------------- |
| `oh_my_prompt`      | Forward `PromptSubmitted` events to an omp instance. | [`oh-my-prompt`](#oh-my-prompt)          |

## oh-my-prompt

[oh-my-prompt](https://github.com/jiunbae/oh-my-prompt) (omp) is a small
self-hostable service that stores prompts from coding-agent CLIs for
search and analytics. muxa already watches the same three-adapter set
(Claude Code, Codex, Gemini CLI), so the sink ships the prompts it's
already capturing — omp owns history, muxa owns realtime. opencode
integration is deferred — see the README's Agent support table for
status.

### What leaves the box

For every `PromptSubmitted` event the daemon sees, the sink POSTs a
record to your omp endpoint:

| Field            | Always sent? | Source                                                      |
| ---------------- | ------------ | ----------------------------------------------------------- |
| `event_id`       | yes          | `muxa-{session_id}-{unix_ms}` — deterministic, dedups       |
| `created_at`     | yes          | RFC-3339 UTC timestamp from the agent event                 |
| `prompt_text`    | yes          | the prompt as the agent recorded it                         |
| `prompt_length`  | yes          | UTF-8 char count                                            |
| `word_count`     | yes          | whitespace-split                                            |
| `token_estimate` | yes          | `prompt_length / 4` (matches omp's own heuristic)           |
| `source`         | yes          | `claude-code` / `codex` / `gemini` / `opencode`             |
| `cli_name`       | yes          | `claude` / `codex` / `gemini` / `opencode`                  |
| `cli_version`    | yes          | muxa's own version (the CLI version isn't always reachable) |
| `role`           | yes          | always `user` — muxa never sees response text               |
| `session_id`     | yes          | agent-reported session id                                   |
| `cwd`            | when known   | from the agent event                                        |
| `project`        | when known   | basename of `cwd`                                           |
| `model`          | when known   | last `Heartbeat` model for that session                     |

What does NOT leave the box: tool calls, notifications, heartbeats,
session lifecycle, response text, environment variables, file contents,
or any other byte the daemon can see. If the brief above ever changes,
this doc gets the same change in the same PR.

`AgentKind::Unknown` events are dropped — there is no canonical
`source` for them.

### Setup

1. Stand up an omp instance and obtain the `X-User-Token` UUID for the
   account you want to push to. (omp's docs are upstream; muxa never
   touches account creation.)
2. Export the token in the daemon's environment, **not** in your TOML:

   ```sh
   export OMP_SERVER_TOKEN="00000000-0000-0000-0000-000000000000"
   ```

   Choose a different env var name via `token_env` if you prefer.

3. Enable the sink in `~/.config/muxa/config.toml`:

   ```toml
   [sinks.oh_my_prompt]
   enabled            = true
   endpoint           = "https://prompt.example.dev"   # YOUR omp host
   # token_env        = "OMP_SERVER_TOKEN"             # default
   # device_id        = "laptop-01"                    # optional, echoed in payload
   # batch_size       = 50                             # records per HTTP POST
   # flush_interval_ms = 5000                          # time-based flush
   ```

4. Restart `muxad`. You should see `oh-my-prompt sink enabled` in the
   daemon log on startup.

There is intentionally **no default endpoint**. A missing or empty
`endpoint` with `enabled = true` is a startup error.

### Opt out

Either delete the section, or:

```toml
[sinks.oh_my_prompt]
enabled = false
```

The sink task is never spawned. No HTTP client is constructed, no
prompts are buffered, no token is read.

### Failure / robustness

- HTTP 5xx → exponential backoff (500ms → 1s → 2s → 4s → 8s → 16s),
  up to 6 attempts.
- HTTP 429 → honors `Retry-After` (capped at 60s for sanity), then
  retries.
- HTTP 4xx (except 429) → logs and drops the batch (the schema is
  wrong; retrying won't fix it).
- Network errors / timeouts → same retry curve as 5xx.
- HTTP 207 (partial success) → batch is treated as accepted; the
  rejected records are logged and **not** re-sent.

Records are buffered in a 1000-entry ring. When the upstream is
unreachable AND prompts arrive faster than retries clear the queue,
the **oldest** record is dropped with a WARN log — the daemon
intentionally never blocks ingest because the omp endpoint is slow.

### Troubleshooting

- **`401` in the daemon log** — `X-User-Token` is wrong. Double-check
  the env var name (`token_env`) and the value (omp validates the UUID
  format strictly).
- **`429` even at low volume** — your omp instance is rate-limiting
  per-token; the sink will sleep for `Retry-After` seconds and retry.
- **`hostname not found` / TLS errors** — check the `endpoint` URL
  scheme and DNS. The sink uses `rustls`, not the system OpenSSL, so
  hostname mismatches surface as TLS errors rather than connection
  resets.
- **Records are buffered but never sent** — check that `endpoint` is
  reachable from the daemon's network namespace. `curl -fsS
  $endpoint/api/health` is a quick smoke test.
- **`oh-my-prompt ring buffer full; dropped oldest record`** — your
  endpoint has been unreachable long enough that 1000 records piled
  up. Fix the upstream and the sink will recover; older prompts are
  permanently lost.
