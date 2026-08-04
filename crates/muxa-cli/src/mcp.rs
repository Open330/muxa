//! `muxa mcp` — a Model Context Protocol stdio server that lets a coding
//! agent orchestrate muxa: ask what other agents are doing, send them
//! prompts, capture their panes, and block until an agent changes state.
//!
//! ## Why hand-rolled, not `rmcp`
//!
//! The official Rust MCP SDK (`rmcp`) pulls a substantial dependency tree
//! (schemars, an async runtime layer, proc-macro codegen) that has to clear
//! MSRV 1.88 *and* the workspace's `cargo-deny` license/advisory policy. A
//! **tools-only** server needs just three request methods — `initialize`,
//! `tools/list`, `tools/call` — plus `ping` and the `initialized`
//! notification. That subset of the JSON-RPC 2.0 wire protocol is tiny and
//! stable, so we implement it directly over the crate's existing
//! `serde_json` + `tokio`, adding **no new dependencies**. If the tool
//! surface ever grows to need resources/prompts/sampling, revisit `rmcp`.
//!
//! ## Transport
//!
//! Newline-delimited JSON-RPC 2.0 over stdio: read one request object per
//! line from stdin, write one response object per line to stdout. Every
//! tool proxies the daemon over the existing unix-socket [`Client`]. The
//! server refuses to start when the daemon socket is unreachable (a clear
//! stderr message + non-zero exit) so an agent never talks to a dead
//! control plane.

use anyhow::{bail, Result};
use muxa::collaboration::{
    CollaborationOrigin, NewRequest, RequestKind, RequestMailbox, RequestStatus, WorkMode,
};
use muxa::event::AgentState;
use muxa::ipc::Client;
use muxa::state::{Agent, Transition};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// MCP protocol revision we speak. `2024-11-05` is the widely-deployed
/// revision Claude Code and other hosts negotiate; a tools-only server is
/// wire-compatible across the later revisions too.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Deadline for the startup liveness probe against the daemon socket.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// Default `muxa_wait_for_change` timeout when the caller omits one.
const DEFAULT_WAIT_SECS: u64 = 30;
/// Hard ceiling on `muxa_wait_for_change` so a caller can't pin the stdio
/// loop forever on a typo'd huge timeout.
const MAX_WAIT_SECS: u64 = 600;

/// How often `muxa_wait_for_change` reconciles against a fresh daemon
/// snapshot while blocking on the transition stream. A broadcast lag on the
/// daemon can silently drop the exact transition we're waiting for (the
/// daemon emits a `{"event":"lagged"}` marker, but the shared
/// `TransitionStream` consumer skips it and this consumer has no other lag
/// signal), so we poll on this cadence and compare the target pane's current
/// state against a baseline captured at entry. Small enough to stay
/// responsive, large enough that even a 600 s wait costs only a few hundred
/// cheap snapshot round-trips.
const RECONCILE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Run the MCP stdio server until stdin closes. Refuses to start if the
/// daemon socket is unreachable.
pub async fn run(client: Client) -> Result<()> {
    // Refuse to start against an absent/dead daemon: a control plane the
    // tools can't reach is worse than a clear failure.
    if let Err(e) = client.snapshot_with_timeout(PROBE_TIMEOUT).await {
        bail!(
            "muxa daemon is not reachable ({e}). Start `muxad` (or check MUXA_SOCKET) \
             before launching `muxa mcp`.",
        );
    }

    serve(
        client,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

/// Read newline-delimited JSON-RPC requests from `reader` and write one
/// response line per request to `writer`, **dispatching each request on its
/// own task**. This is the crux of the non-blocking transport: a
/// long-running tool (`muxa_wait_for_change`, up to 600 s) must never wedge
/// unrelated traffic, so a `ping` / `tools/list` issued while a wait is
/// outstanding is answered immediately rather than queued behind it.
///
/// Responses may therefore interleave in time; that is expected for
/// concurrent JSON-RPC requests, and the `id` echoed on each response lets
/// the client correlate. Line framing is still strict: the shared `writer`
/// mutex is held across the whole `write_all` + newline, so two concurrent
/// responses can never splice mid-line.
///
/// `Client` is a cheap handle (a socket path) and every tool opens its own
/// short-lived daemon connection per call, so cloning it per task shares no
/// serial connection between dispatches.
///
/// Generic over the transport so tests can drive it through an in-memory
/// duplex pipe instead of the real stdio handles.
async fn serve<R, W>(client: Client, reader: R, writer: W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = reader.lines();
    let writer = Arc::new(Mutex::new(writer));
    // In-flight dispatch tasks, tracked so we can drain them when stdin
    // closes rather than dropping a response mid-flight.
    let mut tasks: JoinSet<()> = JoinSet::new();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let client = client.clone();
        let writer = Arc::clone(&writer);
        tasks.spawn(async move {
            let response = match serde_json::from_str::<Value>(&line) {
                Ok(req) => dispatch(&client, &req).await,
                // A line that isn't valid JSON at all: -32700, id unknown.
                Err(e) => Some(error_response(
                    &Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                )),
            };
            // Notifications (and a parse error on what was meant to be a
            // notification) produce no response; only write when there is one.
            if let Some(resp) = response {
                if let Err(e) = write_response(&writer, &resp).await {
                    // stdout is gone (host closed the pipe): nothing left to do
                    // for this response. The read loop will see EOF and exit.
                    tracing::debug!(error = %e, "mcp: failed to write response");
                }
            }
        });
        // Reap finished tasks opportunistically so the set can't grow without
        // bound under steady traffic.
        while tasks.try_join_next().is_some() {}
    }

    // stdin closed: let any in-flight dispatches finish writing before exit.
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Write one response object followed by a newline, holding the shared
/// writer lock across the entire write so concurrent responses never
/// interleave mid-line.
async fn write_response<W>(writer: &Arc<Mutex<W>>, resp: &Value) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(resp).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    let mut guard = writer.lock().await;
    guard.write_all(&bytes).await?;
    guard.flush().await
}

/// Handle one parsed JSON-RPC message. Returns `Some(response)` for
/// anything that warrants a reply and `None` for notifications (a request
/// object with no `id`), which JSON-RPC forbids answering.
///
/// Framing robustness (JSON-RPC 2.0 + MCP `2024-11-05`) — no valid JSON is
/// ever silently dropped:
/// - a **batch array** is rejected with a single `-32600` error (muxa does
///   not implement batching; documented in `docs/MCP.md`);
/// - a **bare value** (number / string / bool / null) is not a request
///   object and carries no `id`, so `-32600` with `id: null`;
/// - an **object** is routed by [`dispatch_object`], which still returns a
///   proper error for a missing/invalid `jsonrpc` or `method` whenever an
///   `id` is present to address the reply to.
async fn dispatch(client: &Client, req: &Value) -> Option<Value> {
    match req {
        Value::Object(_) => dispatch_object(client, req).await,
        Value::Array(_) => Some(error_response(
            &Value::Null,
            -32600,
            "batch requests are not supported; send one JSON-RPC object per line",
        )),
        // Valid JSON, but not a JSON-RPC request object — and no id to
        // address a reply to.
        _ => Some(error_response(
            &Value::Null,
            -32600,
            "invalid request: expected a JSON-RPC object",
        )),
    }
}

/// Route a JSON-RPC **object**: split notifications (no `id`) from requests,
/// validate the envelope, then dispatch by `method`.
async fn dispatch_object(client: &Client, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();

    // No `id` key at all → notification: never answered (even if malformed,
    // there's nothing to address a reply to). The only one we expect is
    // `notifications/initialized`.
    let id = id?;

    // From here the message has an `id`, so any envelope problem yields a
    // proper error response addressed to it rather than a silent drop.
    if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error_response(
            &id,
            -32600,
            "invalid request: \"jsonrpc\" must be \"2.0\"",
        ));
    }
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        return Some(error_response(
            &id,
            -32600,
            "invalid request: missing or non-string \"method\"",
        ));
    };

    let response = match method {
        "initialize" => success(&id, initialize_result()),
        "ping" => success(&id, json!({})),
        "tools/list" => success(&id, json!({ "tools": tool_definitions() })),
        "tools/call" => match call_tool(client, req.get("params")).await {
            Ok(result) => success(&id, result),
            // A malformed `tools/call` (missing name / bad args) is a
            // protocol error; a tool that *ran* but failed reports
            // `isError` inside a normal result (see `call_tool`).
            Err(e) => error_response(&id, -32602, &format!("invalid tool call: {e}")),
        },
        other => error_response(&id, -32601, &format!("method not found: {other}")),
    };
    Some(response)
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "muxa",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "muxa control plane. Use muxa_room_context to discover \
            same-window peers, muxa_send_message/muxa_inbox/muxa_reply for durable \
            peer collaboration, muxa_list_messages for lifecycle visibility, and \
            muxa_wait_reply for a structured result. Use \
            muxa_status to see what agents \
            are doing, muxa_send_prompt to drive one, muxa_capture_pane to read \
            its screen, and muxa_wait_for_change to block until an agent changes \
            state.",
    })
}

/// The control and collaboration tools this server exposes, with JSON-Schema
/// `inputSchema`s.
#[allow(clippy::too_many_lines)] // declarative JSON schemas are clearest kept beside tool names
fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "muxa_status",
            "description": "Snapshot of every agent muxa tracks: state \
                (working/idle/waiting_input/…), pane id, session, model, last \
                prompt, and last notification. Call this first to see what other \
                agents are doing.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_recent_prompts",
            "description": "Recent prompt-history entries (newest first) from the \
                daemon's audit log. Optionally filter to one pane and cap the count.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "string", "description": "Pane id to filter to (e.g. %12 or herdr:p1). Omit for all panes." },
                    "limit": { "type": "integer", "minimum": 0, "description": "Max entries. 0 or omitted = all retained." },
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "muxa_send_prompt",
            "description": "Inject text into an agent's pane as keystrokes. With \
                submit=true (the default), a trailing Enter commits the line so the \
                agent starts working. This is a control action against another \
                agent — use deliberately.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "string", "description": "Target pane id (e.g. %12 or herdr:p1)." },
                    "text": { "type": "string", "description": "Literal text to type into the pane." },
                    "submit": { "type": "boolean", "description": "Press Enter after the text. Default true." },
                },
                "required": ["pane", "text"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "muxa_capture_pane",
            "description": "Capture the visible contents of a pane so you can read \
                what an agent is showing right now.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "string", "description": "Pane id to capture (e.g. %12 or herdr:p1)." },
                },
                "required": ["pane"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "muxa_wait_for_change",
            "description": "Block until an agent's state changes (or a timeout), \
                then return the transition (from/to state, agent, pane). Optionally \
                wait only for changes on a specific pane. Use after muxa_send_prompt \
                to know when the agent finished or needs input.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeout_secs": { "type": "integer", "minimum": 1, "description": "Max seconds to wait. Default 30, max 600." },
                    "pane": { "type": "string", "description": "Only report changes on this pane id. Omit for any pane." },
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "muxa_room_context",
            "description": "Identify this agent and list collaboration peers in the same tmux window, plus unread request and reply counts. Call this before addressing a peer.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_send_message",
            "description": "Send a durable request to a same-window peer. Use target=peer when exactly one other agent is present, or pane:%N for an explicit peer. review/question are read-only by default.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "peer, pane:%N, or %N" },
                    "kind": { "type": "string", "enum": ["question", "review", "task", "notice"] },
                    "body": { "type": "string" },
                    "expects_reply": { "type": "boolean", "description": "Default true except notice." },
                    "work_mode": { "type": "string", "enum": ["read_only", "execute"], "description": "Default read_only." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Advisory path scope for execute work." }
                },
                "required": ["target", "body"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_inbox",
            "description": "Claim and read pending collaboration requests addressed to this exact agent session.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_list_messages",
            "description": "List incoming, sent, or all collaboration requests for this exact agent session without claiming them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mailbox": { "type": "string", "enum": ["incoming", "sent", "all"], "description": "Default all." }
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_reply",
            "description": "Finish a claimed request with a structured response. The response returns directly to the sender; do not type into its pane.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": { "type": "string" },
                    "status": { "type": "string", "enum": ["completed", "blocked", "declined", "failed"] },
                    "body": { "type": "string" },
                    "artifacts": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["request_id", "status", "body"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_wait_reply",
            "description": "Wait until a collaboration request reaches a terminal status, returning its structured reply. Default 30 seconds, max 600.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": { "type": "string" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 }
                },
                "required": ["request_id"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_cancel_message",
            "description": "Cancel a sent request only while it is still queued. A request already claimed by its recipient cannot be cancelled.",
            "inputSchema": {
                "type": "object",
                "properties": { "request_id": { "type": "string" } },
                "required": ["request_id"],
                "additionalProperties": false
            },
        }),
    ]
}

/// Execute a `tools/call`. Returns `Err` only for protocol-level problems
/// (missing tool name, malformed params); a tool that runs but fails to do
/// its job returns an `isError` result so the calling model sees the
/// message rather than a transport fault.
#[allow(clippy::too_many_lines)] // one explicit MCP tool dispatch table
async fn call_tool(client: &Client, params: Option<&Value>) -> Result<Value> {
    let params = params.ok_or_else(|| anyhow::anyhow!("missing params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match name {
        "muxa_status" => Ok(match client.snapshot().await {
            Ok(agents) => json_result(&json!({ "agents": agents })),
            Err(e) => error_result(&format!("status failed: {e}")),
        }),
        "muxa_recent_prompts" => {
            let pane = args.get("pane").and_then(Value::as_str);
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok());
            Ok(match client.recent_prompts(pane, limit).await {
                Ok(prompts) => json_result(&json!({ "prompts": prompts })),
                Err(e) => error_result(&format!("recent_prompts failed: {e}")),
            })
        }
        "muxa_send_prompt" => {
            let Some(pane) = args.get("pane").and_then(Value::as_str) else {
                return Ok(error_result("send_prompt requires a `pane` argument"));
            };
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return Ok(error_result("send_prompt requires a `text` argument"));
            };
            // Submit defaults to true — the common case is "prompt and run".
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(true);
            Ok(match client.send_prompt(pane, text, submit).await {
                // The daemon distinguishes "text injected" (`sent`) from
                // "Enter delivered" (`submitted`); surface the latter so a
                // caller whose submit was requested but not delivered learns
                // the text already landed and must not blindly resend it.
                Ok(outcome) => {
                    render_send_result(text.len(), pane, submit, Some(outcome.submitted))
                }
                Err(e) => error_result(&format!("send_prompt failed: {e}")),
            })
        }
        "muxa_capture_pane" => {
            let Some(pane) = args.get("pane").and_then(Value::as_str) else {
                return Ok(error_result("capture_pane requires a `pane` argument"));
            };
            Ok(match client.capture(pane).await {
                Ok(Some(text)) => text_result(&text),
                Ok(None) => error_result(&format!(
                    "no capture for {pane} (pane gone or backend can't capture)",
                )),
                Err(e) => error_result(&format!("capture failed: {e}")),
            })
        }
        "muxa_wait_for_change" => Ok(wait_for_change(client, &args).await),
        "muxa_room_context" => {
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(match client.collaboration_context(&origin).await {
                Ok(room) => json_result(&json!(room)),
                Err(error) => error_result(&format!("room context failed: {error}")),
            })
        }
        "muxa_send_message" => {
            let Some(target) = args.get("target").and_then(Value::as_str) else {
                return Ok(error_result("send_message requires a `target` argument"));
            };
            let Some(body) = args.get("body").and_then(Value::as_str) else {
                return Ok(error_result("send_message requires a `body` argument"));
            };
            let kind = match parse_request_kind(args.get("kind").and_then(Value::as_str)) {
                Ok(kind) => kind,
                Err(error) => return Ok(error_result(error)),
            };
            let work_mode = match parse_work_mode(args.get("work_mode").and_then(Value::as_str)) {
                Ok(mode) => mode,
                Err(error) => return Ok(error_result(error)),
            };
            let paths = string_array(&args, "paths");
            let expects_reply = args
                .get("expects_reply")
                .and_then(Value::as_bool)
                .unwrap_or(kind != RequestKind::Notice);
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            let request = NewRequest {
                kind,
                body: body.to_string(),
                expects_reply,
                work_mode,
                paths,
            };
            Ok(
                match client.collaboration_send(&origin, target, &request).await {
                    Ok(request) => json_result(&json!(request)),
                    Err(error) => error_result(&format!("send_message failed: {error}")),
                },
            )
        }
        "muxa_inbox" => {
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(match client.collaboration_inbox(&origin).await {
                Ok(requests) => json_result(&json!({ "requests": requests })),
                Err(error) => error_result(&format!("inbox failed: {error}")),
            })
        }
        "muxa_list_messages" => {
            let mailbox = match parse_mailbox(args.get("mailbox").and_then(Value::as_str)) {
                Ok(mailbox) => mailbox,
                Err(error) => return Ok(error_result(error)),
            };
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(match client.collaboration_list(&origin, mailbox).await {
                Ok(requests) => json_result(&json!({ "requests": requests })),
                Err(error) => error_result(&format!("list_messages failed: {error}")),
            })
        }
        "muxa_reply" => {
            let Some(request_id) = args.get("request_id").and_then(Value::as_str) else {
                return Ok(error_result("reply requires a `request_id` argument"));
            };
            let Some(body) = args.get("body").and_then(Value::as_str) else {
                return Ok(error_result("reply requires a `body` argument"));
            };
            let status = match parse_reply_status(args.get("status").and_then(Value::as_str)) {
                Ok(status) => status,
                Err(error) => return Ok(error_result(error)),
            };
            let artifacts = string_array(&args, "artifacts");
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client
                    .collaboration_reply(&origin, request_id, status, body, &artifacts)
                    .await
                {
                    Ok(request) => json_result(&json!(request)),
                    Err(error) => error_result(&format!("reply failed: {error}")),
                },
            )
        }
        "muxa_wait_reply" => Ok(wait_for_reply(client, &args).await),
        "muxa_cancel_message" => {
            let Some(request_id) = args.get("request_id").and_then(Value::as_str) else {
                return Ok(error_result(
                    "cancel_message requires a `request_id` argument",
                ));
            };
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client.collaboration_cancel(&origin, request_id).await {
                    Ok(request) => json_result(&json!(request)),
                    Err(error) => error_result(&format!("cancel_message failed: {error}")),
                },
            )
        }
        other => Ok(error_result(&format!("unknown tool: {other}"))),
    }
}

fn current_collaboration_origin() -> std::result::Result<CollaborationOrigin, String> {
    let pane = std::env::var("TMUX_PANE").map_err(|_| {
        "collaboration requires this MCP server to run inside a tmux pane".to_string()
    })?;
    let socket = std::env::var("TMUX").ok().and_then(|value| {
        let path = value.split(',').next()?.trim();
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
    });
    Ok(CollaborationOrigin { pane, socket })
}

fn parse_request_kind(value: Option<&str>) -> std::result::Result<RequestKind, &'static str> {
    match value.unwrap_or("question") {
        "question" => Ok(RequestKind::Question),
        "review" => Ok(RequestKind::Review),
        "task" => Ok(RequestKind::Task),
        "notice" => Ok(RequestKind::Notice),
        _ => Err("kind must be question, review, task, or notice"),
    }
}

fn parse_work_mode(value: Option<&str>) -> std::result::Result<WorkMode, &'static str> {
    match value.unwrap_or("read_only") {
        "read_only" => Ok(WorkMode::ReadOnly),
        "execute" => Ok(WorkMode::Execute),
        _ => Err("work_mode must be read_only or execute"),
    }
}

fn parse_reply_status(value: Option<&str>) -> std::result::Result<RequestStatus, &'static str> {
    match value {
        Some("completed") => Ok(RequestStatus::Completed),
        Some("blocked") => Ok(RequestStatus::Blocked),
        Some("declined") => Ok(RequestStatus::Declined),
        Some("failed") => Ok(RequestStatus::Failed),
        _ => Err("status must be completed, blocked, declined, or failed"),
    }
}

fn parse_mailbox(value: Option<&str>) -> std::result::Result<RequestMailbox, &'static str> {
    match value.unwrap_or("all") {
        "incoming" => Ok(RequestMailbox::Incoming),
        "sent" => Ok(RequestMailbox::Sent),
        "all" => Ok(RequestMailbox::All),
        _ => Err("mailbox must be incoming, sent, or all"),
    }
}

fn string_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

async fn wait_for_reply(client: &Client, args: &Value) -> Value {
    let Some(request_id) = args.get("request_id").and_then(Value::as_str) else {
        return error_result("wait_reply requires a `request_id` argument");
    };
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WAIT_SECS)
        .min(MAX_WAIT_SECS);
    let origin = match current_collaboration_origin() {
        Ok(origin) => origin,
        Err(error) => return error_result(&error),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match client.collaboration_get(&origin, request_id).await {
            Ok(request) if request.status.is_terminal() => return json_result(&json!(request)),
            Ok(_) => {}
            Err(error) => return error_result(&format!("wait_reply failed: {error}")),
        }
        if tokio::time::Instant::now() >= deadline {
            return json_result(&json!({
                "completed": false,
                "reason": "timeout",
                "request_id": request_id,
            }));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Render the `muxa_send_prompt` result, defensively tolerating both the
/// legacy contract (success = text sent, and Enter pressed if requested) and
/// the newer daemon shape that reports Enter delivery separately.
///
/// `submitted` is what the daemon actually reported about the Enter keystroke:
/// - `None` — the daemon didn't say (older client method that returns `()`):
///   fall back to the caller's `submit` intent.
/// - `Some(true)` — Enter was delivered; the line is committed.
/// - `Some(false)` — the text landed but Enter did **not**; surfaced plainly
///   so the caller doesn't assume the line was committed.
fn render_send_result(text_len: usize, pane: &str, submit: bool, submitted: Option<bool>) -> Value {
    match submitted {
        Some(false) if submit => text_result(&format!(
            "sent {text_len} chars to {pane}, but the line was NOT submitted \
             (Enter not delivered) — the text is typed but not yet committed",
        )),
        _ => {
            // No daemon signal → trust the requested `submit`; an explicit
            // `Some(true)` confirms it.
            let committed = submitted.unwrap_or(submit);
            text_result(&format!(
                "sent {text_len} chars to {pane}{}",
                if committed { " and submitted" } else { "" },
            ))
        }
    }
}

/// Terminal outcome of a `muxa_wait_for_change` race, before it's rendered
/// into a tool result.
enum WaitOutcome {
    /// A live transition arrived on the stream and matched the pane filter.
    Observed(Transition),
    /// A reconcile poll (or a post-lag / post-timeout reconcile) detected the
    /// target pane's state had moved. Carries the ready-made result value.
    Reconciled(Value),
    /// The daemon closed the stream before any matching change, and a
    /// reconcile found nothing.
    Closed,
}

/// `muxa_wait_for_change`: block until a matching state transition is
/// observed, or a reconciled post-lag state match is detected, or the
/// timeout elapses. **Returns on the first observed change OR a reconciled
/// post-lag state match.**
///
/// Two signals race under one deadline:
/// 1. the daemon's transition **stream** (push, ~1 ms latency); and
/// 2. a periodic snapshot **reconcile** ([`RECONCILE_POLL_INTERVAL`]) that
///    compares the target pane's current state against a baseline captured
///    at entry.
///
/// The reconcile exists because a broadcast **lag** on the daemon can drop
/// the exact transition we're waiting for: the daemon emits a
/// `{"event":"lagged"}` marker, but the shared `TransitionStream` consumer
/// skips it, so a matching change dropped during lag would otherwise be
/// misreported as a timeout. Polling the snapshot closes that hole without
/// depending on the stream surfacing the lag to this consumer.
async fn wait_for_change(client: &Client, args: &Value) -> Value {
    let pane = args.get("pane").and_then(Value::as_str);
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WAIT_SECS)
        .clamp(1, MAX_WAIT_SECS);

    // Baseline pane→state map so a reconcile can tell whether the target moved
    // even if the stream never delivered the transition.
    let baseline = pane_states(client, pane).await;

    let mut stream = match client.subscribe().await {
        Ok(s) => s,
        Err(e) => return error_result(&format!("subscribe failed: {e}")),
    };

    let mut poll = tokio::time::interval(RECONCILE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await; // consume the immediate first tick

    let deadline = Duration::from_secs(timeout_secs);
    let outcome = tokio::time::timeout(deadline, async {
        loop {
            tokio::select! {
                recv = stream.recv() => match recv {
                    Ok(Some(t)) => {
                        if pane.is_none_or(|p| t.agent.pane.as_deref() == Some(p)) {
                            return WaitOutcome::Observed(t);
                        }
                    }
                    // Stream closed (daemon shutdown) or a read error. Try one
                    // reconcile before giving up — the change may already have
                    // landed — otherwise report the closed stream.
                    Ok(None) | Err(_) => {
                        return match reconcile(client, pane, &baseline).await {
                            Some(v) => WaitOutcome::Reconciled(v),
                            None => WaitOutcome::Closed,
                        };
                    }
                },
                _ = poll.tick() => {
                    if let Some(v) = reconcile(client, pane, &baseline).await {
                        return WaitOutcome::Reconciled(v);
                    }
                }
            }
        }
    })
    .await;

    match outcome {
        Ok(WaitOutcome::Observed(t)) => json_result(&json!({
            "changed": true,
            "from": t.from,
            "to": t.to,
            "agent": &*t.agent,
        })),
        Ok(WaitOutcome::Reconciled(v)) => v,
        Ok(WaitOutcome::Closed) => json_result(&json!({
            "changed": false,
            "reason": "stream closed before a matching change",
        })),
        // Deadline hit. One last reconcile guards the narrow window where a
        // transition was dropped between the final poll tick and the timeout.
        Err(_) => reconcile(client, pane, &baseline).await.unwrap_or_else(|| {
            json_result(&json!({
                "changed": false,
                "reason": "timeout",
                "timeout_secs": timeout_secs,
            }))
        }),
    }
}

/// Snapshot the current `pane → state` map for the reconcile baseline and
/// polls, scoped to one pane when the caller asked for one, else the whole
/// fleet. A daemon error yields an empty map — the reconcile then simply
/// can't detect a change, degrading to the stream/timeout path.
async fn pane_states(client: &Client, pane: Option<&str>) -> HashMap<String, AgentState> {
    fetch_agents(client, pane)
        .await
        .into_iter()
        .filter_map(|a| a.pane.clone().map(|p| (p, a.state)))
        .collect()
}

/// Compare the target pane(s)' current state against `baseline`; if any has
/// moved — or a matching pane appeared that wasn't there before — return a
/// synthetic transition result flagged `reconciled` so the caller can tell it
/// came from a snapshot rather than the live stream. `None` when nothing
/// changed.
async fn reconcile(
    client: &Client,
    pane: Option<&str>,
    baseline: &HashMap<String, AgentState>,
) -> Option<Value> {
    for agent in fetch_agents(client, pane).await {
        let Some(p) = agent.pane.clone() else {
            continue;
        };
        if pane.is_some_and(|want| want != p) {
            continue;
        }
        let previous = baseline.get(&p).copied();
        if previous != Some(agent.state) {
            return Some(json_result(&json!({
                "changed": true,
                "reconciled": true,
                // `null` when the pane is newly observed (no baseline entry).
                "from": previous,
                "to": agent.state,
                "agent": agent,
            })));
        }
    }
    None
}

/// Fetch the agents relevant to a wait: just the target pane's rows when
/// scoped, else the whole snapshot. Daemon errors degrade to an empty list.
async fn fetch_agents(client: &Client, pane: Option<&str>) -> Vec<Agent> {
    match pane {
        Some(p) => client.by_pane(p).await,
        None => client.snapshot().await,
    }
    .unwrap_or_default()
}

// --- JSON-RPC / MCP result helpers --------------------------------------

fn success(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A tools/call result wrapping plain text.
fn text_result(text: &str) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ] })
}

/// A tools/call result wrapping a JSON value, serialized as pretty text
/// (MCP content is text; the model reads the JSON).
fn json_result(value: &Value) -> Value {
    let text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| "{\"error\":\"serialize failed\"}".to_string());
    text_result(&text)
}

/// A tools/call result flagged `isError` so the model sees the failure
/// message instead of a silent empty result.
fn error_result(message: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": message } ],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::backend::{BackendCaps, HostKind, PaneBackend, SharedBackend};
    use muxa::event::AgentEvent;
    use muxa::ipc::Server;
    use muxa::state::Store;
    use muxa::tmux::PaneInfo;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::sync::broadcast;

    /// Fake backend that always accepts `send_text` and records the calls,
    /// so the MCP → IPC → backend path can be exercised in-process.
    struct FakeBackend {
        sends: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl PaneBackend for FakeBackend {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            Vec::new()
        }
        fn resolve_pane(&self, _: &str) -> Option<PaneInfo> {
            None
        }
        fn capture_pane(&self, pane_id: &str) -> Option<String> {
            Some(format!("screen of {pane_id}"))
        }
        fn pane_pid_map(&self) -> std::collections::HashMap<u32, String> {
            std::collections::HashMap::new()
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _: &str) -> bool {
            false
        }
        fn send_text(&self, pane_id: &str, text: &str) -> bool {
            self.sends
                .lock()
                .unwrap()
                .push((pane_id.to_string(), text.to_string()));
            true
        }
        fn caps(&self) -> BackendCaps {
            BackendCaps::default()
        }
    }

    async fn wait_for_socket(sock: &Path) {
        for _ in 0..50 {
            if sock.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Spin an in-process daemon with a recording backend; returns the
    /// client, the send-log, and a shutdown handle.
    async fn spawn_daemon(
        sock: &Path,
    ) -> (
        Client,
        Arc<Mutex<Vec<(String, String)>>>,
        broadcast::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: SharedBackend = Arc::new(FakeBackend {
            sends: sends.clone(),
        });
        let server = Server::new(sock.to_path_buf(), Store::shared()).with_backends(vec![backend]);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(sock).await;
        (Client::new(sock.to_path_buf()), sends, tx, handle)
    }

    #[tokio::test]
    async fn initialize_and_tools_list() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-init.sock");
        let (client, _sends, tx, handle) = spawn_daemon(&sock).await;

        let init = dispatch(
            &client,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
        )
        .await
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "muxa");
        assert_eq!(init["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert!(init["result"]["capabilities"]["tools"].is_object());

        let list = dispatch(
            &client,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await
        .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "muxa_status",
                "muxa_recent_prompts",
                "muxa_send_prompt",
                "muxa_capture_pane",
                "muxa_wait_for_change",
                "muxa_room_context",
                "muxa_send_message",
                "muxa_inbox",
                "muxa_list_messages",
                "muxa_reply",
                "muxa_wait_reply",
                "muxa_cancel_message",
            ],
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// A notification (no `id`) yields no response.
    #[tokio::test]
    async fn notification_produces_no_response() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-notif.sock");
        let (client, _sends, tx, handle) = spawn_daemon(&sock).await;

        let resp = dispatch(
            &client,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await;
        assert!(resp.is_none());

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn tools_call_status_and_send_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-call.sock");
        let (client, sends, tx, handle) = spawn_daemon(&sock).await;

        // muxa_status returns a (possibly empty) agents payload as text.
        let status = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "muxa_status", "arguments": {} },
            }),
        )
        .await
        .unwrap();
        let text = status["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"agents\""), "status text: {text}");

        // muxa_send_prompt routes through to the backend with a submit CR.
        let send = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {
                    "name": "muxa_send_prompt",
                    "arguments": { "pane": "%1", "text": "hello", "submit": true },
                },
            }),
        )
        .await
        .unwrap();
        assert!(send["result"]["isError"].as_bool() != Some(true), "{send}");
        assert_eq!(
            sends.lock().unwrap().clone(),
            vec![
                ("%1".to_string(), "hello".to_string()),
                ("%1".to_string(), "\r".to_string()),
            ],
        );

        // muxa_capture_pane returns the backend's screen text.
        let cap = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": { "name": "muxa_capture_pane", "arguments": { "pane": "%1" } },
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            cap["result"]["content"][0]["text"].as_str().unwrap(),
            "screen of %1",
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn unknown_method_is_jsonrpc_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-unknown.sock");
        let (client, _sends, tx, handle) = spawn_daemon(&sock).await;

        let resp = dispatch(
            &client,
            &json!({ "jsonrpc": "2.0", "id": 9, "method": "does/not/exist" }),
        )
        .await
        .unwrap();
        assert_eq!(resp["error"]["code"], -32601);

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn wait_for_change_times_out_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-wait.sock");
        let (client, _sends, tx, handle) = spawn_daemon(&sock).await;

        let resp = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {
                    "name": "muxa_wait_for_change",
                    "arguments": { "timeout_secs": 1 },
                },
            }),
        )
        .await
        .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"changed\": false"), "{text}");
        assert!(text.contains("timeout"), "{text}");

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Ingest one synthetic hook event so a pane appears in the store with a
    /// derived state (`prompt_submitted` → Working, `turn_stopped` → Idle).
    async fn ingest(client: &Client, pane: &str, ty: &str, extra: Value) {
        let mut ev = json!({
            "type": ty,
            "id": { "kind": "claude_code", "session_id": pane, "pane": pane, "cwd": null },
            "at": "2026-07-20T00:00:00Z",
        });
        let (Value::Object(fields), Value::Object(more)) = (&mut ev, extra) else {
            unreachable!("event and extra are objects");
        };
        for (k, v) in more {
            fields.insert(k, v);
        }
        let event: AgentEvent = serde_json::from_value(ev).unwrap();
        client.ingest(&event).await.unwrap();
    }

    /// `render_send_result` tolerates the unknown/legacy shape and the newer
    /// `{sent, submitted}` daemon signal, surfacing "not submitted" plainly.
    #[test]
    fn render_send_result_covers_all_submit_shapes() {
        let text = |v: &Value| v["content"][0]["text"].as_str().unwrap().to_string();

        // Unknown submit status (client returns `()` today) → trust intent.
        assert!(text(&render_send_result(5, "%1", true, None)).contains("and submitted"));
        assert!(!text(&render_send_result(5, "%1", false, None)).contains("submitted"));
        // Daemon confirms Enter landed.
        assert!(text(&render_send_result(5, "%1", true, Some(true))).contains("and submitted"));
        // Daemon says text landed but Enter did NOT — surfaced to the caller.
        let not_submitted = text(&render_send_result(5, "%1", true, Some(false)));
        assert!(not_submitted.contains("NOT submitted"), "{not_submitted}");
    }

    /// Framing robustness: batch arrays, bare values, and malformed envelopes
    /// all yield a proper error (or a silent drop only for a true
    /// notification) instead of vanishing.
    #[tokio::test]
    async fn framing_rejects_non_object_and_bad_envelope() {
        let client = Client::new(std::path::PathBuf::from("/nonexistent/muxa.sock"));

        // Batch array → single -32600, id null (batching unsupported).
        let batch = dispatch(&client, &json!([{"jsonrpc":"2.0","id":1,"method":"ping"}]))
            .await
            .unwrap();
        assert_eq!(batch["error"]["code"], -32600);
        assert!(batch["id"].is_null());

        // Bare value → -32600, id null.
        let bare = dispatch(&client, &json!(42)).await.unwrap();
        assert_eq!(bare["error"]["code"], -32600);
        assert!(bare["id"].is_null());

        // Object with an id but a bad `jsonrpc` → -32600 addressed to the id.
        let bad = dispatch(&client, &json!({"id":7,"method":"ping"}))
            .await
            .unwrap();
        assert_eq!(bad["error"]["code"], -32600);
        assert_eq!(bad["id"], 7);

        // Object with an id but no method → -32600 addressed to the id.
        let no_method = dispatch(&client, &json!({"jsonrpc":"2.0","id":8}))
            .await
            .unwrap();
        assert_eq!(no_method["error"]["code"], -32600);
        assert_eq!(no_method["id"], 8);

        // Object with no id → notification: no response even though malformed.
        assert!(dispatch(&client, &json!({"foo":"bar"})).await.is_none());
    }

    /// A raw line that isn't JSON at all yields a `-32700` parse error with a
    /// null id, and never crashes the server loop.
    #[tokio::test]
    async fn serve_reports_parse_error_for_garbage_line() {
        let client = Client::new(std::path::PathBuf::from("/nonexistent/muxa.sock"));
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, client_out) = tokio::io::duplex(4096);
        let handle = tokio::spawn(async move {
            serve(client, BufReader::new(server_in), server_out)
                .await
                .unwrap();
        });
        let mut client_out = BufReader::new(client_out);

        client_in.write_all(b"this is not json\n").await.unwrap();
        client_in.flush().await.unwrap();

        let mut line = String::new();
        client_out.read_line(&mut line).await.unwrap();
        let resp: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(resp["error"]["code"], -32700);
        assert!(resp["id"].is_null());

        drop(client_in);
        handle.await.unwrap();
    }

    /// The core non-blocking property: a slow `muxa_wait_for_change` in flight
    /// must not delay an unrelated `ping` — the ping response comes back
    /// promptly, well before the wait's timeout.
    #[tokio::test]
    async fn slow_wait_does_not_block_ping() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-concurrent.sock");
        let (client, _sends, tx, daemon) = spawn_daemon(&sock).await;

        let (mut client_in, server_in) = tokio::io::duplex(8192);
        let (server_out, client_out) = tokio::io::duplex(8192);
        let serve_handle = tokio::spawn(async move {
            serve(client, BufReader::new(server_in), server_out)
                .await
                .unwrap();
        });
        let mut client_out = BufReader::new(client_out);

        // Slow wait (2 s, empty fleet → will time out) then an immediate ping.
        client_in
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":\
                  {\"name\":\"muxa_wait_for_change\",\"arguments\":{\"timeout_secs\":2}}}\n",
            )
            .await
            .unwrap();
        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        client_in.flush().await.unwrap();

        // First response back must be the ping (id 2), and quickly.
        let start = Instant::now();
        let mut first = String::new();
        client_out.read_line(&mut first).await.unwrap();
        let first: Value = serde_json::from_str(first.trim()).unwrap();
        assert_eq!(
            first["id"], 2,
            "ping should return before the slow wait; got {first}"
        );
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "ping was blocked by the outstanding wait ({:?})",
            start.elapsed(),
        );

        // The wait's own response (id 1) lands later, timing out.
        let mut second = String::new();
        client_out.read_line(&mut second).await.unwrap();
        let second: Value = serde_json::from_str(second.trim()).unwrap();
        assert_eq!(second["id"], 1);
        assert!(second["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("timeout"));

        drop(client_in);
        serve_handle.await.unwrap();
        tx.send(()).unwrap();
        daemon.await.unwrap();
    }

    /// `wait_for_change`'s reconcile catches a state move that the transition
    /// stream might have dropped during a broadcast lag: baseline is captured,
    /// the pane moves, and a snapshot comparison surfaces the change.
    #[tokio::test]
    async fn reconcile_detects_state_change_missed_by_stream() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-reconcile.sock");
        let (client, _sends, tx, daemon) = spawn_daemon(&sock).await;

        // Put %1 into Working via a prompt, then baseline it.
        ingest(&client, "%1", "prompt_submitted", json!({ "prompt": "go" })).await;
        let baseline = pane_states(&client, Some("%1")).await;
        assert_eq!(baseline.get("%1").copied(), Some(AgentState::Working));

        // No movement yet → reconcile reports nothing.
        assert!(reconcile(&client, Some("%1"), &baseline).await.is_none());

        // End the turn → Idle. Even if the stream had dropped this transition,
        // the reconcile compares against the baseline and catches it.
        ingest(&client, "%1", "turn_stopped", json!({})).await;
        let v = reconcile(&client, Some("%1"), &baseline)
            .await
            .expect("reconcile should detect the state move");
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"reconciled\": true"), "{text}");
        assert!(text.contains("\"from\": \"working\""), "{text}");
        assert!(text.contains("\"to\": \"idle\""), "{text}");

        tx.send(()).unwrap();
        daemon.await.unwrap();
    }
}
