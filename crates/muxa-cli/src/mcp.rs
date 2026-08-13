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
//! line from stdin, write one response object per line to stdout. Observation
//! and collaboration tools proxy the daemon through [`Client`]; deterministic
//! managed-tmux lifecycle tools invoke same-user tmux locally. The
//! server refuses to start when the daemon socket is unreachable (a clear
//! stderr message + non-zero exit) so an agent never talks to a dead
//! control plane.

use anyhow::{bail, Result};
use muxa::collaboration::{
    AirArtifactReference, CollaborationOrigin, NewRequest, RequestKind, RequestMailbox,
    RequestStatus, RoomContext, WorkMode,
};
use muxa::event::AgentState;
use muxa::ipc::Client;
use muxa::state::{Agent, Transition};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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

/// Sent to MCP hosts during initialization so collaboration is a first-class
/// workflow rather than a capability the model has to infer from tool names.
const MCP_SERVER_INSTRUCTIONS: &str = "muxa is your same-tmux-window peer team control plane. \
    For managed tmux work, treat one session as one workspace/project, one window \
    as one work/ticket, and one pane as one agent. Use muxa_start_agent with a work id instead \
    of delegating tmux setup to another model; use muxa_manage_tmux for lifecycle \
    control and never invent raw tmux commands. \
    At the start of substantial work, call muxa_collaboration_guide (or \
    muxa_room_context) to discover available peer agents. Improve important work \
    with a peer when useful: use review + read_only after implementation and tests \
    for an independent critique; question + read_only for focused analysis; task + \
    execute + narrow paths only for bounded, non-overlapping delegated edits. Keep \
    primary ownership: continue useful work while the peer runs, wait for its \
    structured reply, then verify and integrate the result yourself. Avoid \
    overlapping edits unless separate worktrees isolate them. When notified of an \
    incoming request, call muxa_inbox promptly, honor kind/work_mode/paths, and \
    always finish with muxa_reply using completed, blocked, declined, or failed. \
    When work is already represented by a validated AIR 1 workflow, plan, or trace, \
    attach its typed air_artifacts reference for shared identity and visualization; \
    AIR Workbench remains the validator/editor and muxa never implies conformance.";

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
        "instructions": MCP_SERVER_INSTRUCTIONS,
    })
}

fn air_artifact_reference_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "artifact_id": {
                "type": "string",
                "pattern": "^urn:air:sha256:[0-9a-f]{64}$",
                "description": "AIR content identity from the validated artifact envelope."
            },
            "profile": {
                "type": "string",
                "enum": [
                    "https://open330.github.io/air/profiles/1.0.0/workflow-skill",
                    "https://open330.github.io/air/profiles/1.0.0/plan-native-cli",
                    "https://open330.github.io/air/profiles/1.0.0/trace-native-run",
                    "https://open330.github.io/air/profiles/1.0.0/trace-session-snapshot"
                ]
            },
            "label": {
                "type": "string",
                "minLength": 1,
                "maxLength": 256,
                "description": "Optional display-only label; never artifact authority."
            },
            "locator": {
                "type": "object",
                "properties": {
                    "display": { "type": "string", "minLength": 1, "maxLength": 4096 },
                    "disclosure": { "type": "string", "enum": ["local-only", "redacted"] }
                },
                "required": ["display", "disclosure"],
                "additionalProperties": false,
                "description": "Optional AIR 1 display-only locator. Muxa never opens it automatically."
            }
        },
        "required": ["artifact_id", "profile"],
        "additionalProperties": false
    })
}

/// The control and collaboration tools this server exposes, with JSON-Schema
/// `inputSchema`s.
#[allow(clippy::too_many_lines)] // declarative JSON schemas are clearest kept beside tool names
fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "muxa_status",
            "description": "Focused observation of muxa agents. With no arguments, \
                snapshot every agent plus the managed workspace > work > agent tmux topology. \
                Set pane to avoid loading the whole fleet; \
                optionally include its visible screen and recent prompt history in \
                the same result to save MCP round trips and model context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "string", "description": "Optional exact pane id." },
                    "include_capture": { "type": "boolean", "description": "Include the pane's visible screen; requires pane." },
                    "history_limit": { "type": "integer", "minimum": 0, "maximum": 20, "description": "Recent prompts for this pane. Default 0; requires pane." },
                    "max_capture_lines": { "type": "integer", "minimum": 1, "maximum": 400, "description": "Trim capture to the newest N lines. Default 120." }
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_recent_prompts",
            "description": "Recent prompt-history entries (newest first) from the \
                daemon's audit log. Optionally filter to one pane and cap the count.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "string", "description": "Pane id to filter to (e.g. %12, rmux:%12, or herdr:p1). Omit for all panes." },
                    "limit": { "type": "integer", "minimum": 0, "description": "Max entries. 0 or omitted = all retained." },
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "muxa_start_agent",
            "description": "Create a detached tmux pane, window, or session and \
                start one allowlisted coding agent in it. Use this deterministic \
                tool instead of spending another agent turn on tmux setup. The \
                codex profile expands the local cx behavior to codex --yolo. \
                Returns the exact new pane id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "enum": ["claude", "codex", "gemini", "opencode"] },
                    "placement": { "type": "string", "enum": ["pane", "window", "session"], "description": "Default pane." },
                    "target": { "type": "string", "description": "tmux target for pane/window placement. Defaults to TMUX_PANE." },
                    "cwd": { "type": "string", "description": "Existing working directory. Defaults to the MCP process cwd." },
                    "prompt": { "type": "string", "description": "Optional first task; omit for an empty interactive agent." },
                    "name": { "type": "string", "description": "Optional window/session name." },
                    "workspace": { "type": "string", "description": "Managed workspace/project id. Valid with work; defaults to cwd basename." },
                    "work": { "type": "string", "description": "Managed work/ticket id. Reuses its tmux window or creates it once; conflicts with placement/target/name." },
                    "role": { "type": "string", "description": "Optional pane role such as implementer or reviewer." },
                    "task": { "type": "string", "description": "Optional short pane task label." },
                    "direction": { "type": "string", "enum": ["right", "down"], "description": "Pane split direction. Default right." }
                },
                "required": ["agent"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_manage_tmux",
            "description": "Manage Muxa's tmux lifecycle using workspace=session, work=window, \
                and agent=pane. List/show managed workspaces and work, interrupt an agent turn, \
                or explicitly terminate an agent pane/close a work window or workspace session. \
                Destructive actions require confirm=true and refuse unmanaged targets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list_workspace", "show_workspace", "list_work", "show_work", "interrupt_agent", "terminate_agent", "close_work", "close_workspace"]
                    },
                    "pane": { "type": "string", "description": "Exact pane id for agent actions, for example %42." },
                    "workspace": { "type": "string", "description": "Workspace/project id for workspace actions or to disambiguate work." },
                    "work": { "type": "string", "description": "Work/ticket id for work actions, for example TEST-0001." },
                    "confirm": { "type": "boolean", "description": "Must be true for terminate_agent, close_work, and close_workspace." }
                },
                "required": ["action"],
                "additionalProperties": false
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
                    "pane": { "type": "string", "description": "Target pane id (e.g. %12, rmux:%12, or herdr:p1)." },
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
                    "pane": { "type": "string", "description": "Pane id to capture (e.g. %12, rmux:%12, or herdr:p1)." },
                },
                "required": ["pane"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "muxa_wait_for_change",
            "description": "Block until an agent changes state. Set until=settled \
                to ignore intermediate working transitions and return only when the \
                agent is idle, blocked, errored, or stopped. A focused wait can also \
                include the final pane capture, avoiding polling loops and extra calls.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeout_secs": { "type": "integer", "minimum": 1, "description": "Max seconds to wait. Default 30, max 600." },
                    "pane": { "type": "string", "description": "Only report changes on this pane id. Omit for any pane." },
                    "until": { "type": "string", "enum": ["any", "settled", "idle", "blocked", "stopped"], "description": "Target after at least one state change. Default any; non-any values require pane." },
                    "include_capture": { "type": "boolean", "description": "Include the newest visible pane screen when returning; requires pane." },
                    "max_capture_lines": { "type": "integer", "minimum": 1, "maximum": 400, "description": "Trim included capture to newest N lines. Default 120." }
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "muxa_collaboration_guide",
            "description": "Discover same-window peer agents and get concrete reviewer/subagent workflows. Call near the start of substantial work and again before finalizing important changes when an independent review could improve the result.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_room_context",
            "description": "Identify this agent and list collaboration peers in the same tmux window, plus unread request and reply counts. Use the returned pane, alias, role, and state to choose an appropriate reviewer or delegated subagent.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_set_identity",
            "description": "Replace this exact agent session's room-local alias and roles. Aliases enable @alias routing; roles enable role:<name> routing. An empty call clears identity.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "alias": { "type": "string", "description": "Unique room-local alias, 1-32 slug characters." },
                    "roles": { "type": "array", "maxItems": 8, "items": { "type": "string" }, "description": "Advisory role names used for routing." }
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_send_message",
            "description": "Send a durable request to a same-window peer. Use review + read_only for an independent critique before finalizing important work; question + read_only for focused analysis; task + execute + narrow paths for bounded, non-overlapping delegated edits. Attach typed air_artifacts when a validated AIR workflow, plan, or trace is the shared review context. Continue useful work, wait for the structured reply, and verify the result yourself. Targets: peer (only one peer), pane:%N, @alias, or role:<name> (only one matching role).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "peer, pane:%N, %N, @alias, or role:<name>" },
                    "kind": { "type": "string", "enum": ["question", "review", "task", "notice"] },
                    "body": { "type": "string" },
                    "expects_reply": { "type": "boolean", "description": "Default true except notice." },
                    "work_mode": { "type": "string", "enum": ["read_only", "execute"], "description": "Default read_only." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Advisory path scope for execute work." },
                    "air_artifacts": {
                        "type": "array",
                        "maxItems": 8,
                        "items": air_artifact_reference_schema(),
                        "description": "Typed references to AIR 1 workflow, plan, or trace artifacts. Muxa transports the reference; AIR Workbench validates and visualizes the artifact."
                    }
                },
                "required": ["target", "body"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_inbox",
            "description": "Claim and read pending collaboration requests addressed to this exact agent session. Call promptly when muxa notifies you, honor each request's kind/work_mode/paths contract, and finish it with muxa_reply.",
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
                    "artifacts": { "type": "array", "items": { "type": "string" } },
                    "air_artifacts": {
                        "type": "array",
                        "maxItems": 8,
                        "items": air_artifact_reference_schema(),
                        "description": "Typed AIR 1 artifact references returned to the sender."
                    }
                },
                "required": ["request_id", "status", "body"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_wait_reply",
            "description": "Wait until a peer reviewer/subagent request reaches a terminal status and return its structured reply. Verify findings or edits before integrating them. Default 30 seconds, max 600.",
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
        "muxa_status" => {
            let pane = args.get("pane").and_then(Value::as_str);
            let include_capture = args
                .get("include_capture")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let history_limit = args
                .get("history_limit")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(20) as usize;
            if pane.is_none() && (include_capture || history_limit > 0) {
                return Ok(error_result(
                    "status include_capture/history_limit requires pane",
                ));
            }
            let agents = match pane {
                Some(pane) => client.by_pane(pane).await,
                None => client.snapshot().await,
            };
            let agents = match agents {
                Ok(agents) => agents,
                Err(error) => {
                    return Ok(error_result(&format!("status failed: {error}")));
                }
            };
            let topology_agents = agents.clone();
            let mut payload = json!({ "agents": agents });
            if pane.is_none() {
                match tokio::task::spawn_blocking(move || {
                    let inputs = muxa::active_backends()
                        .into_iter()
                        .map(|backend| {
                            muxa::TopologyInput::new(backend.kind(), backend.list_panes())
                        })
                        .collect();
                    muxa::TopologySnapshot::build(
                        time::OffsetDateTime::now_utc(),
                        inputs,
                        topology_agents,
                    )
                })
                .await
                {
                    Ok(topology) => {
                        payload = json!({ "topology": topology });
                    }
                    Err(error) => payload["topology_error"] = json!(error.to_string()),
                }
            }
            if let Some(pane) = pane {
                if include_capture {
                    let max_lines = args
                        .get("max_capture_lines")
                        .and_then(Value::as_u64)
                        .unwrap_or(120)
                        .clamp(1, 400) as usize;
                    match client.capture(pane).await {
                        Ok(capture) => {
                            payload["capture"] =
                                json!(capture.map(|text| newest_lines(&text, max_lines)));
                        }
                        Err(error) => {
                            payload["capture_error"] = json!(error.to_string());
                        }
                    }
                }
                if history_limit > 0 {
                    match client.recent_prompts(Some(pane), Some(history_limit)).await {
                        Ok(prompts) => payload["prompts"] = json!(prompts),
                        Err(error) => payload["history_error"] = json!(error.to_string()),
                    }
                }
            }
            Ok(json_result(&payload))
        }
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
        "muxa_start_agent" => {
            let Some(agent) = args.get("agent").and_then(Value::as_str) else {
                return Ok(error_result("start_agent requires an agent argument"));
            };
            let agent = match crate::agent_launch::AgentProgram::parse(agent) {
                Ok(agent) => agent,
                Err(error) => return Ok(error_result(&error)),
            };
            let placement = match crate::agent_launch::Placement::parse(
                args.get("placement").and_then(Value::as_str),
            ) {
                Ok(placement) => placement,
                Err(error) => return Ok(error_result(&error)),
            };
            let direction = match crate::agent_launch::SplitDirection::parse(
                args.get("direction").and_then(Value::as_str),
            ) {
                Ok(direction) => direction,
                Err(error) => return Ok(error_result(&error)),
            };
            let request = crate::agent_launch::StartRequest {
                agent,
                placement,
                target: args
                    .get("target")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                cwd: args
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(std::path::PathBuf::from),
                prompt: args
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                name: args.get("name").and_then(Value::as_str).map(str::to_string),
                workspace: args
                    .get("workspace")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                work: args.get("work").and_then(Value::as_str).map(str::to_string),
                role: args.get("role").and_then(Value::as_str).map(str::to_string),
                task: args.get("task").and_then(Value::as_str).map(str::to_string),
                direction,
            };
            Ok(
                match tokio::task::spawn_blocking(move || crate::agent_launch::start(request)).await
                {
                    Ok(Ok(result)) => json_result(&json!(result)),
                    Ok(Err(error)) => error_result(&format!("start_agent failed: {error}")),
                    Err(error) => error_result(&format!("start_agent worker failed: {error}")),
                },
            )
        }
        "muxa_manage_tmux" => {
            let Some(action) = args.get("action").and_then(Value::as_str) else {
                return Ok(error_result("manage_tmux requires an action argument"));
            };
            let action = match crate::tmux_work::ManageAction::parse(action) {
                Ok(action) => action,
                Err(error) => return Ok(error_result(&error)),
            };
            let request = crate::tmux_work::ManageRequest {
                action,
                pane: args.get("pane").and_then(Value::as_str).map(str::to_string),
                workspace: args
                    .get("workspace")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                work: args.get("work").and_then(Value::as_str).map(str::to_string),
                confirm: args
                    .get("confirm")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            Ok(
                match tokio::task::spawn_blocking(move || crate::tmux_work::manage(request)).await {
                    Ok(Ok(result)) => json_result(&json!(result)),
                    Ok(Err(error)) => error_result(&format!("manage_tmux failed: {error}")),
                    Err(error) => error_result(&format!("manage_tmux worker failed: {error}")),
                },
            )
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
        "muxa_collaboration_guide" => {
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(match client.collaboration_context(&origin).await {
                Ok(room) => json_result(&collaboration_guide(room)),
                Err(error) => error_result(&format!("collaboration guide failed: {error}")),
            })
        }
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
        "muxa_set_identity" => {
            let alias = args.get("alias").and_then(Value::as_str);
            let roles = string_array(&args, "roles");
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client
                    .collaboration_set_identity(&origin, alias, &roles)
                    .await
                {
                    Ok(room) => json_result(&json!(room)),
                    Err(error) => error_result(&format!("set_identity failed: {error}")),
                },
            )
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
            let air_artifacts = match parse_air_artifact_references(&args) {
                Ok(references) => references,
                Err(error) => return Ok(error_result(&error)),
            };
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
                air_artifacts,
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
            let air_artifacts = match parse_air_artifact_references(&args) {
                Ok(references) => references,
                Err(error) => return Ok(error_result(&error)),
            };
            let origin = match current_collaboration_origin() {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client
                    .collaboration_reply(
                        &origin,
                        request_id,
                        status,
                        body,
                        &artifacts,
                        &air_artifacts,
                    )
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

fn collaboration_guide(room: RoomContext) -> Value {
    let next_step = match room.peers.len() {
        0 => "No peer is available. Continue locally or run another agent in this tmux window.",
        1 => "One peer is available; target `peer` or its explicit pane id.",
        _ => {
            "Multiple peers are available; choose an explicit pane, @alias, or unique role:<name>."
        }
    };
    json!({
        "purpose": "Use another live agent as an independent reviewer or a bounded delegated subagent to improve important work.",
        "room": room,
        "next_step": next_step,
        "workflows": {
            "reviewer": {
                "when": "After implementation, self-review, and relevant tests; before declaring important work complete.",
                "request": {
                    "kind": "review",
                    "work_mode": "read_only",
                    "body_should_include": [
                        "objective and acceptance criteria",
                        "diff, commit, or artifact to inspect",
                        "tests already run",
                        "specific risks or uncertainties"
                    ]
                },
                "after_sending": "Continue any independent checks, wait with muxa_wait_reply, then verify and address the findings."
            },
            "subagent": {
                "when": "A useful subtask is independently verifiable and does not overlap your active edits.",
                "request": {
                    "kind": "task",
                    "work_mode": "execute",
                    "paths": "Set the narrowest advisory path scope possible.",
                    "body_should_include": [
                        "one bounded deliverable",
                        "constraints and definition of done",
                        "required verification",
                        "expected reply artifacts"
                    ]
                },
                "after_sending": "Retain primary ownership; inspect the peer's edits and rerun relevant verification before integrating."
            },
            "focused_question": {
                "when": "You need independent analysis without edits.",
                "request": { "kind": "question", "work_mode": "read_only" }
            },
            "air_handoff": {
                "when": "The work already has a validated AIR 1 workflow, plan, or trace artifact.",
                "steps": [
                    "Pass artifact_id, exact AIR 1 profile, and optional display-only locator in air_artifacts.",
                    "Use the same artifact identity in review requests and structured replies so watch/dashboard can visualize the handoff.",
                    "Open the source-bearing artifact in AIR Workbench for graph inspection or editing; muxa transports references and does not validate or execute AIR."
                ]
            },
            "incoming_request": {
                "steps": [
                    "Call muxa_inbox promptly to claim and read it.",
                    "Honor kind, work_mode, and paths; read_only never authorizes edits.",
                    "Reply exactly once with completed, blocked, declined, or failed and include useful artifacts."
                ]
            }
        },
        "guardrails": [
            "Do not create overlapping concurrent edits unless separate worktrees isolate them.",
            "Do not treat a peer reply as proof; verify it against the repository and tests.",
            "Do not delegate merely for ceremony or recursively bounce work without a concrete benefit."
        ]
    })
}

fn current_collaboration_origin() -> std::result::Result<CollaborationOrigin, String> {
    let pane = muxa::default_backend()
        .current_pane()
        .or_else(current_process_host_pane)
        .ok_or_else(|| {
            "collaboration could not identify this MCP server's pane; \
             native pane variables are unset and process ancestry did not reach a pane shell. \
             For Codex, add env_vars = [\"RMUX\", \"RMUX_PANE\", \"TMUX\", \"TMUX_PANE\", \"MUXA_SOCKET\"] \
             under [mcp_servers.muxa] and restart Codex"
                .to_string()
        })?;
    let endpoint = match muxa::backend::pane_id_host_kind(&pane) {
        Some(muxa::HostKind::Rmux) => std::env::var("RMUX").ok(),
        Some(muxa::HostKind::Tmux) => std::env::var("TMUX").ok(),
        Some(muxa::HostKind::Zellij | muxa::HostKind::Herdr) | None => None,
    };
    let socket = endpoint.and_then(|value| {
        let path = value.split(',').next()?.trim();
        (!path.is_empty()).then(|| muxa::backend::pane_endpoint_identity(Some(&pane), path))
    });
    // An MCP caller *is* the agent in the pane, so it keeps the pane identity:
    // replies must route back to it and wake it.
    Ok(CollaborationOrigin {
        pane,
        socket,
        console: false,
    })
}

/// Recover the owning pane when an MCP host sanitizes subprocess
/// environment variables. Current Codex releases only forward variables
/// explicitly listed in `mcp_servers.<name>.env_vars`, so an existing muxa
/// registration may launch without the host's native variables even though
/// Codex itself is running in a pane.
///
/// The MCP process is still a descendant of the pane shell. Match the first
/// ancestor whose PID appears in an active backend's pane inventory. Explicit
/// env remains authoritative because it also identifies the endpoint without
/// ambiguity.
fn current_process_host_pane() -> Option<String> {
    let pane_pids = muxa::active_backends()
        .into_iter()
        .flat_map(|backend| backend.pane_pid_map())
        .collect::<HashMap<_, _>>();
    pane_from_ancestry(std::process::id(), &pane_pids, |pid| {
        muxa::adapters::proc_ancestry::parent_pid(pid)
    })
}

fn pane_from_ancestry<F>(
    start_pid: u32,
    pane_pids: &HashMap<u32, String>,
    parent_of: F,
) -> Option<String>
where
    F: Fn(u32) -> Option<u32>,
{
    if let Some(pane) = pane_pids.get(&start_pid) {
        return Some(pane.clone());
    }
    let candidates: HashSet<u32> = pane_pids.keys().copied().collect();
    let pane_shell =
        muxa::adapters::proc_ancestry::ancestor_in_set(start_pid, &candidates, parent_of)?;
    pane_pids.get(&pane_shell).cloned()
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

fn parse_air_artifact_references(
    args: &Value,
) -> std::result::Result<Vec<AirArtifactReference>, String> {
    let Some(value) = args.get("air_artifacts") else {
        return Ok(Vec::new());
    };
    serde_json::from_value(value.clone())
        .map_err(|error| format!("air_artifacts must match the AIR reference schema: {error}"))
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
fn newest_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitUntil {
    Any,
    Settled,
    Idle,
    Blocked,
    Stopped,
}

impl WaitUntil {
    fn parse(value: Option<&str>) -> std::result::Result<Self, &'static str> {
        match value.unwrap_or("any") {
            "any" => Ok(Self::Any),
            "settled" => Ok(Self::Settled),
            "idle" => Ok(Self::Idle),
            "blocked" => Ok(Self::Blocked),
            "stopped" => Ok(Self::Stopped),
            _ => Err("until must be any, settled, idle, blocked, or stopped"),
        }
    }

    fn matches(self, state: AgentState) -> bool {
        match self {
            Self::Any => true,
            Self::Settled => matches!(
                state,
                AgentState::Idle
                    | AgentState::WaitingInput
                    | AgentState::WaitingChoice
                    | AgentState::Error
                    | AgentState::Stopped
            ),
            Self::Idle => state == AgentState::Idle,
            Self::Blocked => matches!(
                state,
                AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
            ),
            Self::Stopped => state == AgentState::Stopped,
        }
    }
}

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
    let until = match WaitUntil::parse(args.get("until").and_then(Value::as_str)) {
        Ok(until) => until,
        Err(error) => return error_result(error),
    };
    let include_capture = args
        .get("include_capture")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if pane.is_none() && (until != WaitUntil::Any || include_capture) {
        return error_result("wait until/include_capture requires pane");
    }
    if until != WaitUntil::Any {
        return wait_for_state(
            client,
            pane.expect("non-any wait has pane"),
            until,
            timeout_secs,
            include_capture,
            capture_line_limit(args),
        )
        .await;
    }

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

    let result = match outcome {
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
    };
    add_wait_capture(
        client,
        pane,
        include_capture,
        capture_line_limit(args),
        result,
    )
    .await
}

/// Snapshot the current `pane → state` map for the reconcile baseline and
/// polls, scoped to one pane when the caller asked for one, else the whole
/// fleet. A daemon error yields an empty map — the reconcile then simply
/// can't detect a change, degrading to the stream/timeout path.
async fn wait_for_state(
    client: &Client,
    pane: &str,
    until: WaitUntil,
    timeout_secs: u64,
    include_capture: bool,
    max_capture_lines: usize,
) -> Value {
    let baseline_agent = current_agent(fetch_agents(client, Some(pane)).await);
    let mut last_state = baseline_agent.as_ref().map(|agent| agent.state);
    let mut changed = false;
    let mut stream = match client.subscribe().await {
        Ok(stream) => stream,
        Err(error) => return error_result(&format!("subscribe failed: {error}")),
    };
    let mut poll = tokio::time::interval(RECONCILE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await;

    let outcome = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            tokio::select! {
                recv = stream.recv() => match recv {
                    Ok(Some(transition))
                        if transition.agent.pane.as_deref() == Some(pane) =>
                    {
                        changed |= transition.from != transition.to;
                        last_state = Some(transition.to);
                        if changed && until.matches(transition.to) {
                            return json_result(&json!({
                                "changed": true,
                                "matched": true,
                                "until": wait_until_label(until),
                                "from": transition.from,
                                "to": transition.to,
                                "agent": &*transition.agent,
                            }));
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        return json_result(&json!({
                            "changed": changed,
                            "matched": false,
                            "reason": "stream closed before target state",
                            "until": wait_until_label(until),
                            "last_state": last_state,
                        }));
                    }
                },
                _ = poll.tick() => {
                    let agent = current_agent(fetch_agents(client, Some(pane)).await);
                    let state = agent.as_ref().map(|agent| agent.state);
                    if state != last_state {
                        changed = true;
                        last_state = state;
                    }
                    if let (true, Some(state), Some(agent)) = (changed, state, agent) {
                        if until.matches(state) {
                            return json_result(&json!({
                                "changed": true,
                                "matched": true,
                                "reconciled": true,
                                "until": wait_until_label(until),
                                "to": state,
                                "agent": agent,
                            }));
                        }
                    }
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        json_result(&json!({
            "changed": changed,
            "matched": false,
            "reason": "timeout",
            "until": wait_until_label(until),
            "last_state": last_state,
            "timeout_secs": timeout_secs,
        }))
    });

    add_wait_capture(
        client,
        Some(pane),
        include_capture,
        max_capture_lines,
        outcome,
    )
    .await
}

fn current_agent(agents: Vec<Agent>) -> Option<Agent> {
    agents
        .into_iter()
        .max_by_key(|agent| (agent.state != AgentState::Stopped, agent.last_activity_at))
}

fn wait_until_label(until: WaitUntil) -> &'static str {
    match until {
        WaitUntil::Any => "any",
        WaitUntil::Settled => "settled",
        WaitUntil::Idle => "idle",
        WaitUntil::Blocked => "blocked",
        WaitUntil::Stopped => "stopped",
    }
}

fn capture_line_limit(args: &Value) -> usize {
    args.get("max_capture_lines")
        .and_then(Value::as_u64)
        .unwrap_or(120)
        .clamp(1, 400) as usize
}

async fn add_wait_capture(
    client: &Client,
    pane: Option<&str>,
    include_capture: bool,
    max_lines: usize,
    result: Value,
) -> Value {
    if !include_capture {
        return result;
    }
    let Some(pane) = pane else {
        return result;
    };
    let Some(text) = result["content"][0]["text"].as_str() else {
        return result;
    };
    let Ok(mut payload) = serde_json::from_str::<Value>(text) else {
        return result;
    };
    match client.capture(pane).await {
        Ok(capture) => {
            payload["capture"] = json!(capture.map(|text| newest_lines(&text, max_lines)));
        }
        Err(error) => payload["capture_error"] = json!(error.to_string()),
    }
    json_result(&payload)
}

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
        let instructions = init["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("start of substantial work"));
        assert!(instructions.contains("one session as one workspace/project"));
        assert!(instructions.contains("one window as one work/ticket"));
        assert!(instructions.contains("muxa_manage_tmux"));
        assert!(instructions.contains("review + read_only"));
        assert!(instructions.contains("task + execute + narrow paths"));
        assert!(instructions.contains("verify and integrate the result yourself"));

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
                "muxa_start_agent",
                "muxa_manage_tmux",
                "muxa_send_prompt",
                "muxa_capture_pane",
                "muxa_wait_for_change",
                "muxa_collaboration_guide",
                "muxa_room_context",
                "muxa_set_identity",
                "muxa_send_message",
                "muxa_inbox",
                "muxa_list_messages",
                "muxa_reply",
                "muxa_wait_reply",
                "muxa_cancel_message",
            ],
        );
        let start_agent = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_start_agent")
            .unwrap();
        assert_eq!(start_agent["inputSchema"]["required"], json!(["agent"]));
        assert_eq!(
            start_agent["inputSchema"]["properties"]["agent"]["enum"],
            json!(["claude", "codex", "gemini", "opencode"])
        );
        assert!(start_agent["inputSchema"]["properties"]["work"].is_object());
        assert!(start_agent["inputSchema"]["properties"]["workspace"].is_object());
        let manage_tmux = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_manage_tmux")
            .unwrap();
        assert_eq!(manage_tmux["inputSchema"]["required"], json!(["action"]));
        assert!(manage_tmux["inputSchema"]["properties"]["workspace"].is_object());
        let guide = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_collaboration_guide")
            .unwrap();
        assert!(guide["description"]
            .as_str()
            .unwrap()
            .contains("reviewer/subagent workflows"));

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[test]
    fn collaboration_guide_surfaces_reviewer_and_subagent_contracts() {
        let participant = |kind: &str, session: &str, pane: &str| {
            json!({
                "agent_kind": kind,
                "agent_session_id": session,
                "pane": pane,
                "socket": "default",
                "room": { "host": "tmux", "socket": "default", "window_id": "@1" },
                "tmux_session_id": "$1",
                "tmux_session_name": "cal-6924",
                "window_name": "agents",
                "state": "idle"
            })
        };
        let room: RoomContext = serde_json::from_value(json!({
            "self": participant("codex", "sender", "%1"),
            "peers": [participant("claude_code", "reviewer", "%2")],
            "unread": 0,
            "unread_replies": 0
        }))
        .unwrap();

        let guide = collaboration_guide(room);
        assert_eq!(guide["room"]["peers"][0]["pane"], "%2");
        assert_eq!(guide["workflows"]["reviewer"]["request"]["kind"], "review");
        assert_eq!(
            guide["workflows"]["reviewer"]["request"]["work_mode"],
            "read_only"
        );
        assert_eq!(
            guide["workflows"]["subagent"]["request"]["work_mode"],
            "execute"
        );
        assert!(guide["guardrails"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule.as_str().unwrap().contains("separate worktrees")));
    }

    #[test]
    fn collaboration_origin_recovers_pane_from_process_ancestry() {
        let pane_pids = HashMap::from([(40, "%659".to_string()), (70, "%700".to_string())]);
        let parents = HashMap::from([(100, 90), (90, 80), (80, 40), (40, 1)]);

        assert_eq!(
            pane_from_ancestry(100, &pane_pids, |pid| parents.get(&pid).copied()),
            Some("%659".to_string())
        );
    }

    #[test]
    fn collaboration_origin_ancestry_fallback_is_bounded_to_known_panes() {
        let pane_pids = HashMap::from([(40, "%659".to_string())]);
        let parents = HashMap::from([(100, 90), (90, 80), (80, 1)]);

        assert_eq!(
            pane_from_ancestry(100, &pane_pids, |pid| parents.get(&pid).copied()),
            None
        );
    }

    #[test]
    fn collaboration_origin_accepts_mcp_process_as_pane_root() {
        let pane_pids = HashMap::from([(100, "%659".to_string())]);

        assert_eq!(
            pane_from_ancestry(100, &pane_pids, |_| None),
            Some("%659".to_string())
        );
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

        // Fleet-wide muxa_status returns the canonical nested topology, even
        // when the runner has no live multiplexer sessions.
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
        let status_payload: Value = serde_json::from_str(text).unwrap();
        assert!(
            status_payload["topology"]["sessions"].is_array(),
            "status text: {text}"
        );
        assert!(
            status_payload.get("agents").is_none(),
            "status text: {text}"
        );

        let focused = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 31, "method": "tools/call",
                "params": {
                    "name": "muxa_status",
                    "arguments": {
                        "pane": "%1",
                        "include_capture": true,
                        "history_limit": 1
                    }
                },
            }),
        )
        .await
        .unwrap();
        let focused_text = focused["result"]["content"][0]["text"].as_str().unwrap();
        assert!(focused_text.contains("\"capture\": \"screen of %1\""));
        assert!(focused_text.contains("\"prompts\""));

        let refused_terminate = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 32, "method": "tools/call",
                "params": {
                    "name": "muxa_manage_tmux",
                    "arguments": {
                        "action": "terminate_agent",
                        "pane": "%1"
                    }
                },
            }),
        )
        .await
        .unwrap();
        assert_eq!(refused_terminate["result"]["isError"], true);
        assert!(refused_terminate["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("confirm=true"));

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

    #[test]
    fn focused_status_capture_keeps_only_newest_lines() {
        assert_eq!(newest_lines("one\ntwo\nthree", 2), "two\nthree");
        assert_eq!(newest_lines("one", 20), "one");
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

    #[test]
    fn settled_wait_matches_only_terminal_turn_states() {
        for state in [
            AgentState::Idle,
            AgentState::WaitingInput,
            AgentState::WaitingChoice,
            AgentState::Error,
            AgentState::Stopped,
        ] {
            assert!(WaitUntil::Settled.matches(state), "{state:?}");
        }
        assert!(!WaitUntil::Settled.matches(AgentState::Starting));
        assert!(!WaitUntil::Settled.matches(AgentState::Working));
        assert!(WaitUntil::Blocked.matches(AgentState::WaitingChoice));
        assert!(!WaitUntil::Blocked.matches(AgentState::Idle));
    }

    #[tokio::test]
    async fn settled_wait_skips_working_and_returns_with_capture() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-settled.sock");
        let (client, _sends, tx, daemon) = spawn_daemon(&sock).await;
        ingest(&client, "%1", "prompt_submitted", json!({ "prompt": "go" })).await;

        let waiting_client = client.clone();
        let waiter = tokio::spawn(async move {
            wait_for_change(
                &waiting_client,
                &json!({
                    "pane": "%1",
                    "until": "settled",
                    "include_capture": true,
                    "timeout_secs": 2
                }),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        ingest(&client, "%1", "turn_stopped", json!({})).await;

        let result = waiter.await.unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"matched\": true"), "{text}");
        assert!(text.contains("\"to\": \"idle\""), "{text}");
        assert!(text.contains("\"capture\": \"screen of %1\""), "{text}");

        tx.send(()).unwrap();
        daemon.await.unwrap();
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
