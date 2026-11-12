# MCP surface: one tool per job

Status: target surface for the mcp-slim stream (2026-11-12). Implemented in
stages; each PR states the `tools/list` byte count before and after
(`cargo test -p tend-cli --bin tend mcp_schema_bytes -- --ignored --nocapture`).

## Problem

`tend mcp` listed 36 tools and 52,864 bytes of schema (about 13K tokens) that
every MCP session pays for. Several jobs had two to six tools:

- text in front of an agent: `send_prompt`, `fleet_send_prompt`,
  `send_message`, `call_peer`, `fleet_call_peer`, `dispatch_work`;
- waiting: `wait_reply`, `fleet_wait_reply`, `wait_for_change`,
  `dispatch_status`;
- reading the mailbox: `inbox`, `list_messages`, `peer_report`;
- orientation: `guide`, `collaboration_guide`, `room_context`;
- observation: `status`, `agent_find`, `capture_pane`, `fleet_status`,
  `fleet_capture`, `recent_prompts`.

`call_peer`, `send_message` and `fleet_call_peer` each carried about 25
parameters, including ids the daemon already derives from the caller
(`thread_id`, `workspace_id`, `work_id`, `run_id`).

## Target: 16 listed tools

| Tool | One job | Absorbs (still accepted, not listed) |
| --- | --- | --- |
| `tend_guide` | Orientation: identity, peers, guardrails, launch preferences. `topic`: `collaboration` (default), `room`, `launch` | `collaboration_guide`, `room_context`, old `guide` |
| `tend_status` | Observe agents: summary, one `pane` (with capture/history), or `query` to find where work is; `host` for a Fleet host | `agent_find`, `capture_pane`, `recent_prompts`, `fleet_status`, `fleet_capture` |
| `tend_agent_search` | Historical transcript evidence; `event_ids` expands hits | `agent_read` |
| `tend_wait` | Wait on one handle: a request id (`req_…`, `dispatch-<uuid>`) for its reply, a swap job (`swap-<uuid>`) until it ends, a dispatch UUID (its current status, returned at once), or a `pane` state change. The handle's kind comes from its id; `host` only says where it runs | `wait_reply`, `fleet_wait_reply`, `wait_for_change`, `dispatch_status` |
| `tend_call_peer` | Every durable request to an agent: auto-selected peer, exact `target`, one-way `intent=notice`; `host` + `pane` for Fleet | `send_message`, `fleet_call_peer` |
| `tend_request_attention` | A decision only a person can make | — |
| `tend_inbox` | Read the mailbox. Without `view` it claims incoming work (one `request_id`, or the queue). With `view=incoming|sent|all|report` it only reads (history, one instruction or its progress, the newest peer reply) and never accepts work or changes its authority | `list_messages`, `peer_report` |
| `tend_update_request` | Act on an open request: progress `body`, or `action=cancel|withdraw_wake` | `fleet_update_request`, `cancel_message`, `withdraw_reply_wake` |
| `tend_reply` | Finish a claimed request | — |
| `tend_file_result` | File a result record | — |
| `tend_send_prompt` | Type keystrokes into a pane (gated on Idle); `host` for Fleet | `fleet_send_prompt` |
| `tend_start_agent` | Start one agent in a pane/window/session | — |
| `tend_start_work` | Start managed Work; `host` + `commit` dispatches through the Fleet coordinator | `dispatch_work` |
| `tend_manage_tmux` | Rename/terminate/close managed objects | — |
| `tend_swap_account` | Move agents to another account (optional module) | — |
| `tend_set_identity` | Set this session's alias and roles | — |

Rules:

1. **Local and Fleet twins merge behind an optional `host`.** Omitted or
   `local` means this daemon. A Fleet reply handle (`pane_key`) returned by
   a call is remembered by the MCP process, so `tend_wait(id)` needs only
   the id; an explicit `host`/`pane_key` still works across processes.
2. **Daemon-derived ids leave the schemas.** `thread_id`, `workspace_id`,
   `work_id` and `run_id` are stamped from the caller's pane; the handlers
   still accept them, they are just not advertised. `parent_request_id`
   stays: it is a causal fact only the caller knows. Handles stay too:
   receipts keep returning the full `host`/`pane_key`/`request_id`, so a
   restarted MCP process can still wait or update without resending work.
3. **No foreground default wait.** `tend_call_peer` defaults to
   `wait=false`; the reply arrives as a wake and is read with
   `tend_inbox(request_id)` or `tend_wait(id)`.
4. **Compatibility.** Every retired name keeps working for already-running
   agents for at least one release. It is not listed; its result carries a
   `replaced_by` note naming the tool to use. Behaviour behind both names is
   the same handler.
5. **No semantic change.** Mailbox semantics and delivery transport do not
   change in this stream; the new names route to the existing handlers.

## Stages

1. This document, the compatibility layer, `tend_wait`, `tend_guide`.
2. Observation and control merges (`tend_status`, `tend_send_prompt host`,
   `tend_start_work host`, `tend_agent_search event_ids`).
3. Messaging merges (`tend_call_peer`, `tend_inbox`, `tend_update_request`),
   schema trimming, `wait=false` default.
4. Installed skill, guide and bootstrap text shrunk to this surface.
5. `tend_send_prompt host=...` gated like local sends once relays negotiate
   `send_prompt_state_gate_v1`.
