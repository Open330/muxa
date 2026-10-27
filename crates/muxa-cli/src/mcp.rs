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
//! server starts even when the daemon is unreachable (with a stderr
//! warning): agent hosts never respawn a server that exited, so tools
//! report the daemon's error until it is back instead. When its binary is
//! upgraded, an idle server re-executes itself on the same stdio
//! ([`UpgradeWatch`]).

use anyhow::Result;
use muxa::collaboration::{
    AirArtifactReference, CollaborationOrigin, CollaborationRequest, NewRequest, Participant,
    RequestKind, RequestMailbox, RequestStatus, RoomContext, WorkMode,
};
use muxa::event::{AgentKind, AgentState};
use muxa::ipc::{Client, SendPromptOutcome, TransitionStream};
use muxa::state::{Agent, Transition};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
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

/// How long a spawned peer is given to register before the request is queued
/// against its pane instead. Agents that register on boot (claude fires
/// `SessionStart` within a second) resolve inside this window and keep the
/// ordinary session-pinned path; the rest cost one grace period, not a
/// failure — codex cannot register until a prompt reaches it.
const DEFAULT_SPAWN_GRACE_SECS: u64 = 10;
/// Hard ceiling on `muxa_wait_for_change` so a caller can't pin the stdio
/// loop forever on a typo'd huge timeout.
const MAX_WAIT_SECS: u64 = 600;
/// Compatibility polling is used only with controllers lacking shared reply waits.
const FLEET_REPLY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Sent to MCP hosts during initialization so collaboration is a first-class
/// workflow rather than a capability the model has to infer from tool names.
const MCP_SERVER_INSTRUCTIONS: &str =
    "Read muxa_collaboration_guide once. Delivery is not task success; reported_status is peer-declared.";

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

/// Run the MCP stdio server until stdin closes.
///
/// It starts even when muxad is unreachable. Agent hosts launch MCP servers
/// once and do not retry: refusing to start because the daemon was
/// restarting (or, once, because its full snapshot outgrew an IPC frame)
/// left that agent without muxa tools for the rest of its life. Each tool
/// call opens its own daemon connection, so calls fail with the daemon's
/// error while it is down and work again when it is back.
pub async fn run(client: Client, config: muxa::config::Config) -> Result<()> {
    if let Err(e) = client.hello(PROBE_TIMEOUT).await {
        eprintln!(
            "muxa mcp: daemon is not reachable ({e}); serving anyway. Tools report the \
             error until `muxad` is back (check MUXA_SOCKET)."
        );
    }
    let mut stdout = tokio::io::stdout();
    if std::env::var_os(REEXEC_ENV).is_some() {
        // Re-executed after an upgrade: the tool set may have changed.
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" });
        let mut bytes = serde_json::to_vec(&note).unwrap_or_default();
        bytes.push(b'\n');
        stdout.write_all(&bytes).await?;
        stdout.flush().await?;
    }
    serve_with_config(
        client,
        BufReader::new(tokio::io::stdin()),
        stdout,
        Arc::new(config),
        UpgradeWatch::for_this_process(),
    )
    .await
}

/// How often an idle server checks whether its binary was upgraded.
const UPGRADE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Set on a server re-executed by [`UpgradeWatch`].
const REEXEC_ENV: &str = "MUXA_MCP_REEXEC";

/// What the launch path resolves to, and that file's size and mtime: an
/// upgrade either repoints the path (per-commit builds behind a symlink)
/// or rewrites the file in place (`cargo install`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryStamp {
    target: std::path::PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl BinaryStamp {
    fn of(path: &std::path::Path) -> Option<Self> {
        let target = std::fs::canonicalize(path).ok()?;
        let metadata = std::fs::metadata(&target).ok()?;
        Some(Self {
            target,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

/// Re-executes the server in place when its binary is upgraded, so agents
/// keep muxa tools current without restarting. Agent hosts spawn `muxa
/// mcp` once per session and never respawn it; before this, every upgrade
/// left each agent's tools on the old build until the agent restarted.
/// The server keeps no session state (each call opens its own daemon
/// connection), so the new process can take over the same stdio.
struct UpgradeWatch {
    /// The path the server was launched from (`muxa` on PATH), not the
    /// build it resolved to: that is what an upgrade repoints.
    path: std::path::PathBuf,
    started: BinaryStamp,
    reexec: Box<dyn FnMut(&std::path::Path) + Send>,
}

impl UpgradeWatch {
    #[cfg(unix)]
    fn for_this_process() -> Option<Self> {
        let path = launch_path()?;
        let started = BinaryStamp::of(&path)?;
        Some(Self {
            path,
            started,
            reexec: Box::new(reexec_in_place),
        })
    }

    #[cfg(not(unix))]
    fn for_this_process() -> Option<Self> {
        None
    }

    fn replaced(&self) -> bool {
        BinaryStamp::of(&self.path).is_some_and(|now| now != self.started)
    }
}

/// `argv[0]` as a path: absolute as given, relative to the working
/// directory when it has a directory part, else looked up on PATH.
#[cfg(unix)]
fn launch_path() -> Option<std::path::PathBuf> {
    let argv0 = std::env::args_os().next()?;
    let path = std::path::PathBuf::from(&argv0);
    if path.is_absolute() {
        Some(path)
    } else if path.components().count() > 1 {
        std::env::current_dir().ok().map(|dir| dir.join(path))
    } else {
        which::which(&argv0).ok()
    }
}

/// Replace this process with `path`, same arguments and stdio. Returns
/// only if the exec failed, after which the old server keeps serving.
#[cfg(unix)]
fn reexec_in_place(path: &std::path::Path) {
    use std::os::unix::process::CommandExt as _;
    let error = std::process::Command::new(path)
        .args(std::env::args_os().skip(1))
        .env(REEXEC_ENV, "1")
        .exec();
    tracing::warn!(%error, path = %path.display(), "mcp: could not re-exec the upgraded binary; keeping this one");
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
#[cfg(test)]
async fn serve<R, W>(client: Client, reader: BufReader<R>, writer: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_with_config(
        client,
        reader,
        writer,
        Arc::new(muxa::config::Config::default()),
        None,
    )
    .await
}

async fn serve_with_config<R, W>(
    client: Client,
    reader: BufReader<R>,
    writer: W,
    config: Arc<muxa::config::Config>,
    mut upgrade: Option<UpgradeWatch>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = reader.lines();
    let writer = Arc::new(Mutex::new(writer));
    // In-flight dispatch tasks, tracked so we can drain them when stdin
    // closes rather than dropping a response mid-flight.
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut upgrade_poll = tokio::time::interval(UPGRADE_POLL_INTERVAL);
    upgrade_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // `next_line` is cancel-safe, so polling for an upgrade beside it
        // never loses part of a request.
        let line = tokio::select! {
            line = lines.next_line() => line?,
            _ = upgrade_poll.tick(), if upgrade.is_some() => {
                while tasks.try_join_next().is_some() {}
                // Only between requests: nothing in flight, nothing read but
                // unparsed, every response already flushed.
                let idle = tasks.is_empty() && lines.get_ref().buffer().is_empty();
                if let Some(watch) = upgrade.as_mut().filter(|watch| idle && watch.replaced()) {
                    let path = watch.path.clone();
                    (watch.reexec)(&path);
                    // Still here: the exec failed (or a test hook returned).
                    upgrade = None;
                }
                continue;
            }
        };
        let Some(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let client = client.clone();
        let writer = Arc::clone(&writer);
        let config = Arc::clone(&config);
        tasks.spawn(async move {
            let response = match serde_json::from_str::<Value>(&line) {
                Ok(req) => dispatch_with_config(&client, &req, &config).await,
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
#[cfg(test)]
async fn dispatch(client: &Client, req: &Value) -> Option<Value> {
    dispatch_with_config(client, req, &muxa::config::Config::default()).await
}

async fn dispatch_with_config(
    client: &Client,
    req: &Value,
    config: &muxa::config::Config,
) -> Option<Value> {
    match req {
        Value::Object(_) => dispatch_object(client, req, config).await,
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
async fn dispatch_object(
    client: &Client,
    req: &Value,
    config: &muxa::config::Config,
) -> Option<Value> {
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
        "initialize" => success(&id, initialize_result(config)),
        "ping" => success(&id, json!({})),
        "tools/list" => success(&id, json!({ "tools": tool_definitions(config) })),
        "tools/call" => match CALLER_SESSION
            .scope(
                caller_session_hint(req.get("params")),
                call_tool(client, req.get("params"), config),
            )
            .await
        {
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

fn initialize_result(_config: &muxa::config::Config) -> Value {
    let instructions = MCP_SERVER_INSTRUCTIONS;
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": true } },
        "serverInfo": {
            "name": "muxa",
            "version": crate::mcp_revision::server_version(),
        },
        "instructions": instructions,
    })
}

/// The provider arguments a launch of `program` would actually carry,
/// rendered for the guide. Reads through [`muxa::config::Config::launch_options`]
/// rather than `[mcp.guide].options` directly: with `[agent.<program>]`
/// configured, quoting the guide's own list would advertise arguments the
/// launch is not going to use.
fn guide_options_for(config: &muxa::config::Config, program: &str) -> Vec<String> {
    config.launch_options(program, None)
}

fn launch_guide_summary(config: &muxa::config::Config) -> String {
    let guide = &config.mcp.guide;
    let agent = guide.agent.as_deref().unwrap_or("caller must choose");
    let options = match guide.agent.as_deref() {
        Some(program) => {
            let resolved = guide_options_for(config, program);
            if resolved.is_empty() {
                "none".to_string()
            } else {
                serde_json::to_string(&resolved).unwrap_or_else(|_| "[]".into())
            }
        }
        None => "per agent; see launch_defaults.agent_options".to_string(),
    };
    format!(
        "User launch preferences (use when the request is silent): placement={}, agent={}, \
         additional agent options={}, pane direction={}. Managed Work keeps its required \
         workspace-session/work-window/agent-pane layout, and collaboration peer spawning \
         stays in the current window.",
        guide.placement.as_str(),
        agent,
        options,
        guide.direction.as_str()
    )
}

fn launch_guide_value(config: &muxa::config::Config) -> Value {
    let guide = &config.mcp.guide;
    // Every provider muxa can launch, so a caller choosing a non-default
    // agent still learns what that one will be started with.
    let agent_options: serde_json::Map<String, Value> =
        ["claude", "codex", "gemini", "agy", "opencode"]
            .into_iter()
            .map(|program| {
                (
                    program.to_string(),
                    json!(guide_options_for(config, program)),
                )
            })
            .collect();
    json!({
        "launch_defaults": {
            "placement": guide.placement,
            "agent": guide.agent,
            "options": guide.agent.as_deref().map_or_else(Vec::new, |program| guide_options_for(config, program)),
            "agent_options": agent_options,
            "direction": guide.direction,
        },
        "instructions": guide.instructions,
        "semantics": {
            "placement": "Applied to unmanaged muxa_start_agent calls when placement is omitted.",
            "agent": "Applied when agent is omitted. Without a configured value, callers must choose an agent.",
            "options": "Additional arguments after Muxa's built-in provider profile and before the initial prompt, for launch_defaults.agent. Explicit tool options replace these defaults.",
            "agent_options": "The same, per provider, from [agent.<program>] in muxa's config. Use the entry for the agent you are launching.",
            "managed_work": "Managed Work always uses workspace=session, current Run=window, and agent=pane.",
            "peer_spawn": "Collaboration peer spawning always creates a pane in the current window; agent/options/direction preferences still apply when compatible."
        }
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
fn tool_definitions(config: &muxa::config::Config) -> Vec<Value> {
    let start_required = if config.mcp.guide.agent.is_some() {
        json!([])
    } else {
        json!(["agent"])
    };
    let start_description = format!(
        "Create a detached tmux pane, window, or session and start one allowlisted coding agent in it. \
         Use this deterministic tool instead of spending another agent turn on tmux setup. The codex \
         profile expands the local cx behavior to codex --yolo. Returns the exact new pane id. {}",
        launch_guide_summary(config)
    );
    vec![
        json!({
            "name": "muxa_status",
            "description": "Focused observation of muxa agents. With no arguments, \
                return a compact fleet summary; set full=true only when the complete managed \
                workspace > work > agent topology is necessary. Set pane for one agent; \
                optionally include its visible screen and recent prompt history in \
                the same result to save MCP round trips and model context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "string", "description": "Optional exact pane id." },
                    "full": { "type": "boolean", "description": "Return the complete topology when pane is omitted. Default false." },
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
                    "limit": { "type": "integer", "minimum": 0, "description": "Default 20; normal range 1..100. Larger limits or 0 require full=true." },
                    "full": { "type": "boolean", "description": "Explicit full-history read; omitted limit or limit=0 returns all retained entries. Default false." },
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "muxa_guide",
            "description": "Return the user's configured Muxa orchestration preferences: pane/window/session placement, default agent, additional CLI options, split direction, and free-form guidance. Consult it before choosing a surface when the user did not specify one.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_start_agent",
            "description": start_description,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "purpose": {"type":"string","enum":["primary","worker"],"description":"Default primary protects human work. Explicit worker accepts peer requests using configured provider defaults."},
                    "agent": { "type": "string", "enum": ["claude", "codex", "gemini", "agy", "opencode"], "description": "Agent provider. May be omitted when mcp.guide.agent is configured." },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Additional provider CLI arguments. Overrides mcp.guide.options when present; Muxa shell-quotes every argument." },
                    "placement": { "type": "string", "enum": ["pane", "window", "session"], "description": "Defaults to mcp.guide.placement (pane when unconfigured). Managed work always uses its workspace window." },
                    "target": { "type": "string", "description": "Target for pane/window placement. Use the exact room pane including rmux:; defaults to the identified caller pane." },
                    "cwd": { "type": "string", "description": "Existing working directory. Defaults to the MCP process cwd." },
                    "prompt": { "type": "string", "description": "Optional first task; omit for an empty interactive agent." },
                    "name": { "type": "string", "description": "Optional window/session name." },
                    "workspace": { "type": "string", "description": "Managed workspace/project id. Valid with work; defaults to cwd basename." },
                    "work": { "type": "string", "description": "Managed Work id. Reuses the current Run window or creates it once; conflicts with placement/target/name." },
                    "role": { "type": "string", "description": "Optional pane role such as implementer or reviewer." },
                    "alias": { "type": "string", "description": "Stable per-work name for this pane. `muxa work up` diffs on it to tell an agent it already started from one it still has to." },
                    "task": { "type": "string", "description": "Optional short pane task label." },
                    "direction": { "type": "string", "enum": ["right", "down"], "description": "Pane split direction. Defaults to mcp.guide.direction (right when unconfigured)." }
                },
                "required": start_required,
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_start_work",
            "description": "Bring one Work's current Run to the state its configured pipeline declares: \
                optionally resolve and link an external issue, route the Work to a workspace and directory, and create whichever agent panes \
                are missing. This is the deterministic way to stand up a whole team (planner, implementer, \
                reviewer) for a Work — prefer it over several muxa_start_agent calls, which cannot tell an \
                agent that already exists from one that still has to be created. Re-running converges instead \
                of duplicating: existing panes are kept, and a body/skill/context is typed into them rather \
                than launched again, which is also how you steer work already in flight. Panes no pipeline \
                alias claims are reported and never touched. Requires [[route]] and [pipeline.*] config, or an \
                explicit pipeline. Use dry_run=true first when you are unsure what it would create. \
                CLI equivalent: `muxa work up`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "work": { "type": "string", "description": "Stable Muxa Work id. One managed tmux window per id." },
                    "external": { "type": "string", "description": "Optional external issue key to resolve and link, for example CAL-1234." },
                    "pipeline": { "type": "string", "description": "Pipeline name from [pipeline.*]. Defaults to the matching route's; required when no route matches." },
                    "workspace": { "type": "string", "description": "Workspace/project session. Defaults to the route's, then the cwd basename." },
                    "cwd": { "type": "string", "description": "Work directory. Overrides the route; refused when the route creates a git worktree." },
                    "body": { "type": "string", "description": "What the work is. Launched agents get it in their first prompt; agents already running get it typed into their pane." },
                    "skill": { "type": "string", "description": "Registered Muxa message skill name, with or without leading /. Expanded ahead of body, exactly as in muxa_call_peer." },
                    "context": { "type": "string", "description": "Optional bounded context appended after the skill/body. Muxa never attaches transcripts or diffs implicitly." },
                    "no_ticket": { "type": "boolean", "description": "Skip ticket lookup and use the work id alone. Default false." },
                    "refresh": { "type": "boolean", "description": "Ignore the cached ticket and ask the resolver again. Default false." },
                    "dry_run": { "type": "boolean", "description": "Return the plan without creating panes or sending prompts. Default false." }
                },
                "required": ["work"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_swap_account",
            "description": "Move Claude/Codex agents onto another account and resume each conversation in its own pane: \
                stop the agent, relaunch it on the account with the same session, answer startup dialogs, and send a \
                follow-up so a cut-off turn carries on. Default targets are agents stopped by a usage cap. Accounts come \
                from [[accounts.<provider>]] on that host, or aas. Run on this host or a control-mode Fleet host. \
                Use dry_run=true to see the plan first. CLI equivalent: `muxa swap`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Fleet host alias. Omit (or \"local\") for this host." },
                    "targets": { "type": "array", "items": { "type": "string" }, "description": "Panes (%5, rmux:%5) or provider session ids. Omit for every capped agent." },
                    "all": { "type": "boolean", "description": "Every Claude/Codex agent, not only capped ones." },
                    "provider": { "type": "string", "enum": ["claude", "codex"] },
                    "to": { "type": "string", "description": "Account name, or auto (default)." },
                    "spread": { "type": "boolean", "description": "Round-robin agents over every account with quota left." },
                    "force": { "type": "boolean", "description": "Also stop agents that are mid-turn." },
                    "continue": { "type": "string", "description": "Follow-up prompt for every swapped agent." },
                    "no_continue": { "type": "boolean", "description": "Send no follow-up prompt." },
                    "dry_run": { "type": "boolean", "description": "Return the plan without touching any agent." }
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_manage_tmux",
            "description": "Manage Muxa's tmux bindings: session=Workspace, window=current Run, \
                and pane=agent session. List/show managed Workspaces and Work, interrupt an agent turn, \
                or explicitly terminate an agent pane/close a work window or workspace session. \
                Destructive actions require confirm=true and refuse unmanaged targets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list_workspace", "show_workspace", "list_work", "show_work", "interrupt_agent", "terminate_agent", "close_work", "close_workspace"]
                    },
                    "pane": { "type": "string", "description": "Exact pane id from room context for agent actions, for example rmux:%42 or %42. The server must resolve uniquely." },
                    "workspace": { "type": "string", "description": "Workspace/project id for workspace actions or to disambiguate work." },
                    "work": { "type": "string", "description": "Muxa Work id for Work actions, for example TEST-0001." },
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
            "name": "muxa_fleet_status",
            "description": "Read compact cached physical-host Fleet health, including the local controller. Full topology requires full=true. Optionally filter hosts with a Kubernetes-style label selector. This does not open a new SSH connection per call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "Optional label selector, for example environment=production,region in (icn,nrt)." },
                    "full": { "type": "boolean", "description": "Return complete cached host snapshots and topology. Default false: host health/counts and at most 20 active agents per host, without prompts." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muxa_fleet_capture",
            "description": "Capture one exact pane from a named Fleet host, including local. The host must have a live cached topology; pane accepts a backend pane id or session/window/pane path and ambiguity is rejected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Fleet host alias; local always names the controller node." },
                    "pane": { "type": "string", "description": "Pane id such as %12 or session/window/%12." }
                },
                "required": ["host", "pane"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muxa_fleet_send_prompt",
            "description": "Send literal text to one exact pane on a named Fleet host. Local is always control-capable; remote hosts must use control mode. The daemon never retries this mutation. For ongoing agent work use muxa_fleet_update_request; this tool has no request lifecycle or completion acknowledgement.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Fleet host alias; local always names the controller node." },
                    "pane": { "type": "string", "description": "Pane id such as %12 or session/window/%12." },
                    "text": { "type": "string", "description": "Literal prompt text." },
                    "submit": { "type": "boolean", "description": "Press Enter after delivery. Default true." }
                },
                "required": ["host", "pane", "text"],
                "additionalProperties": false
            }
        }),
        json!({
            "name":"muxa_dispatch_work",
            "description":"Delegate authorized Work through the shared Fleet coordinator. Resolves node selectors and personalized paths, prepares an isolated worktree at an exact commit, and launches the configured pipeline. Reuse dispatch_id on retries. plan=true is read-only; launched means started, not completed.",
            "inputSchema":{"type":"object","required":["dispatch_id","workspace","work","commit","body"],"properties":{
                "dispatch_id":{"type":"string"},"workspace":{"type":"string"},"work":{"type":"string"},"commit":{"type":"string"},"body":{"type":"string"},"selector":{"type":"string"},"host":{"type":"string"},"plan":{"type":"boolean","default":false}
            }}
        }),
        json!({
            "name":"muxa_dispatch_status","description":"Read the durable Fleet dispatch by UUID through the shared coordinator. Does not repeat execution.",
            "inputSchema":{"type":"object","required":["dispatch_id"],"properties":{"dispatch_id":{"type":"string"}}}
        }),
        json!({
            "name": "muxa_fleet_call_peer",
            "description": "Send one durable structured collaboration request to one exact agent pane on a named Fleet host and optionally wait for its reply. The host and pane are always explicit; this tool never auto-selects or spawns a remote agent. Remote hosts must be configured mode='control' and advertise collaboration reply support. Defaults to review + read_only. Set execute=true only after explicit user authorization. If the bounded wait expires, continue with muxa_fleet_wait_reply and the returned pane_key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dispatch_id": {"type":"string", "description":"UUID for this dispatch. Reuse unchanged on any uncertain-delivery retry."},
                    "delegation_parent": {"type":"object", "required":["node_id","request_id"], "properties":{"node_id":{"type":"string"},"request_id":{"type":"string"}}},
                    "host": { "type": "string", "description": "Exact Fleet host alias. The remote host must be explicitly configured mode='control'." },
                    "pane": { "type": "string", "description": "Exact or unambiguous live pane id/path, for example %12 or session/window/%12." },
                    "intent": { "type": "string", "enum": ["review", "question", "task"], "description": "Default review." },
                    "body": { "type": "string", "description": "Request-specific instruction. Optional when skill is supplied." },
                    "skill": { "type": "string", "description": "Registered local Muxa message skill name, with or without leading /. Its prompt is expanded before sending." },
                    "context": { "type": "string", "description": "Optional bounded context appended after the skill/body. Muxa never attaches transcripts or diffs implicitly." },
                    "execute": { "type": "boolean", "description": "Default false. Requires intent=task, explicit user authorization, and a control-mode host." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Advisory path scope, especially for execute work." },
                    "wait": { "type": "boolean", "description": "Wait for the structured reply in this call. Default true." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "description": "Reply wait timeout. Default 300." },
                    "thread_id": { "type": "string", "description": "Stable id shared by one causal conversation. Omit for a root request; muxa assigns its request id." },
                    "parent_request_id": { "type": "string", "description": "Exact prior request that caused this follow-up." },
                    "workspace_id": { "type": "string", "description": "Stable workspace/project id used to group collaboration history." },
                    "work_id": { "type": "string", "description": "Durable Work id within the workspace, used to group requests across runs." },
                    "run_id": { "type": "string", "description": "Specific execution/run id for this request." },
                    "artifacts": { "type": "array", "items": { "type": "string" }, "description": "Generic artifact identifiers or locators attached to the request." },
                    "links": { "type": "array", "items": { "type": "string" }, "description": "Generic relationship or URL locators associated with the request." },
                    "air_artifacts": {
                        "type": "array",
                        "maxItems": 8,
                        "items": air_artifact_reference_schema(),
                        "description": "Optional validated AIR artifact references carried with the request."
                    }
                },
                "required": ["host", "pane"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muxa_update_request",
            "description": "Append verified guidance or a progress report to an existing nonterminal request without completing it. Use this instead of sending a new message to pane:console or injecting a prompt. Only existing participants may update; scope and execution authority stay unchanged. Read updates through muxa_list_messages and finish with one muxa_reply.",
            "inputSchema": {"type":"object", "properties": {
                "request_id":{"type":"string"}, "body":{"type":"string","description":"One consolidated update, at most 8192 UTF-8 bytes. Link larger artifacts."}
            }, "required":["request_id","body"], "additionalProperties":false}
        }),
        json!({
            "name": "muxa_fleet_update_request",
            "description": "Append consolidated guidance to an existing Fleet request using its exact returned host/pane_key/request_id. Does not send another task or inject terminal text. Existing authorized scope is unchanged. The recipient reads updates on the same request; terminal completion still uses muxa_reply.",
            "inputSchema": {"type":"object", "properties": {
                "host":{"type":"string"}, "pane_key":{"type":"object"},
                "request_id":{"type":"string"}, "body":{"type":"string","description":"Verified guidance within the original scope, at most 8192 UTF-8 bytes."}
            }, "required":["host","pane_key","request_id","body"], "additionalProperties":false}
        }),
        json!({
            "name": "muxa_fleet_wait_reply",
            "description": "Continue waiting for the structured reply to a prior muxa_fleet_call_peer request. Pass the exact host, pane_key, and request_id returned by that call. The durable remote request remains readable even if the target pane has since exited. Remote hosts must remain explicitly configured mode='control'. Prefer completion wakes/checkpoints; use 30-60 seconds for a foreground wait, not repeated 1-second probes. Check fresh recipient/host health after timeout; follow peer_interruption in muxa_collaboration_guide.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Exact Fleet host alias used for the original call." },
                    "pane_key": {
                        "type": "object",
                        "description": "Exact collision-free PaneKey object returned by muxa_fleet_call_peer; do not reconstruct it from a pane id."
                    },
                    "request_id": { "type": "string", "description": "Exact collaboration request id returned by muxa_fleet_call_peer." },
                    "after_update": {"type":"integer", "minimum":0, "description":"Optionally return when a progress/guidance sequence newer than this value is available, without waiting for terminal completion. Start at 0; resume with next_after_update."},
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": DEFAULT_WAIT_SECS, "description": "Reply wait timeout. Default 30 seconds." }
                },
                "required": ["host", "pane_key", "request_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muxa_wait_for_change",
            "description": "Block until an agent changes state. Set until=settled \
                to ignore intermediate working transitions and return only when the \
                agent is idle, blocked, errored, or stopped. A focused wait can also \
                include the final pane capture, avoiding polling loops and extra calls. \
                If a Codex host yields a background cell, resume that same cell with \
                yield_time_ms=60000 instead of starting another Muxa wait.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "description": "Max seconds to wait. Default 30." },
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
            "description": "Return room.self identity, peers, launch preferences and messaging guardrails. Call once; refresh peers with muxa_room_context. Set workflows=true only for detailed examples.",
            "inputSchema": { "type": "object", "properties": {"workflows":{"type":"boolean", "description":"Include detailed workflow examples. Default false."}}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_room_context",
            "description": "Identify this agent (self) and list collaboration peers in the same tmux window, plus unread request and reply counts. A lightweight refresh after muxa_collaboration_guide; use the returned pane, alias, role, and state to choose a reviewer or delegated subagent.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_call_peer",
            "description": "Create a new natural colleague request for Muxa-reserved @peer/@muxa-peer routing. Deterministically selects a healthy same-window worker (or an explicit provider, @alias, role, or pane), optionally expands a registered Muxa /skill, sends one durable request, and optionally waits for its structured reply. Use muxa_peer_report instead when the user asks about an existing peer report, reply, findings, or conversation. Defaults to review + read_only. Set execute=true only after explicit user authorization. Generic work reuses a worker or automatically creates one using configured provider permissions. Set context_bound=true only when the exact target conversation is required. Automatic creation can be disabled in collaboration settings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Default auto. Accepts auto/peer, codex/claude/gemini/agy/opencode (with or without @), pane:%N/%N, @alias, role:<name>, or a workspace/work/role[@alias] path." },
                    "intent": { "type": "string", "enum": ["review", "question", "task"], "description": "Default review." },
                    "body": { "type": "string", "description": "Request-specific instruction. Optional when skill is supplied." },
                    "skill": { "type": "string", "description": "Registered Muxa message skill name, with or without leading /. Its prompt is expanded before sending." },
                    "context": { "type": "string", "description": "Optional bounded context appended after the skill/body. Muxa never attaches transcripts or diffs implicitly." },
                    "execute": { "type": "boolean", "description": "Default false. Requires intent=task and explicit user authorization." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Advisory path scope, especially for execute work." },
                    "wait": { "type": "boolean", "description": "Wait for the structured reply in this call. Default true." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "description": "Reply wait timeout. Default 300." },
                    "spawn_if_missing": { "type": "boolean", "description": "Override collaboration.auto_spawn_workers (default true). Reuse an existing worker first; new peers use configured provider permissions unless worker_bypass_permissions is explicitly enabled." },
                    "context_bound": {"type":"boolean","description":"Default false routes generic work to a dedicated worker, including when an exact target is primary. True requires that exact session's conversation and keeps primary requests queued for acceptance."},
                    "spawn_agent": { "type": "string", "enum": ["claude", "codex", "gemini", "agy", "opencode"], "description": "Provider to create when spawn_if_missing=true. Defaults to the current agent's opposite provider." },
                    "spawn_timeout_secs": { "type": "integer", "minimum": 1, "maximum": 60, "description": "Grace period for the new pane to register as a collaboration peer before the request is queued against the pane itself. Default 10. Never poll the pane instead of raising this." },
                    "thread_id": { "type": "string", "description": "Stable id shared by one causal conversation. Omit for a root request; muxa assigns its request id. When parent_request_id is set, this must match the parent's canonical thread." },
                    "parent_request_id": { "type": "string", "description": "Exact prior request that caused this follow-up. Muxa validates the parent and inherits its canonical thread_id when thread_id is omitted." },
                    "workspace_id": { "type": "string", "description": "Stable workspace/project id used to group collaboration history." },
                    "work_id": { "type": "string", "description": "Durable Work id within the workspace, used to group requests across runs." },
                    "run_id": { "type": "string", "description": "Specific execution/run id for this request." },
                    "artifacts": { "type": "array", "items": { "type": "string" }, "description": "Generic artifact identifiers or locators attached to the request." },
                    "links": { "type": "array", "items": { "type": "string" }, "description": "Generic relationship or URL locators associated with the request." },
                    "withdraw_reply_wakes": { "type": "array", "maxItems": 16, "items": { "type": "string" }, "description": "Your own earlier request ids this request replaces (for example one you gave up on after it was blocked). Their late replies will not wake you; their work, replies and muxa_wait_reply results are unchanged. Applied atomically with this send." },
                    "air_artifacts": {
                        "type": "array",
                        "maxItems": 8,
                        "items": air_artifact_reference_schema(),
                        "description": "Optional validated AIR artifact references carried with the request."
                    }
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "muxa_peer_report",
            "description": "Read the newest received Muxa peer report/reply sent to this agent, or retrieve an exact request_id. Use this first for possessive requests such as '@peer's report', 'peer findings', 'the peer conversation', or 'resolve the peer feedback'. This is Muxa mailbox data, not a GitHub PR review; never infer a PR number or review thread from it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Optional report author filter: peer/auto/@muxa-peer, provider, pane id, @alias, or role:<name>. Default newest report from any peer." },
                    "request_id": { "type": "string", "description": "Retrieve one exact prior Muxa request/reply instead of selecting the newest report." },
                    "full": { "type": "boolean", "description": "Return the legacy full request record. Default false: only receipt and reply." }
                },
                "additionalProperties": false
            },
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
            "name": "muxa_request_attention",
            "description": "Request a required human approval, choice, or missing information. Creates one durable operator request on this host, visible in every connected Fleet client including the Muxa Mac app. Explain what decision is needed, why, options and recommendation. Reuse the returned request ID and wait via muxa_wait_reply; never treat timeout or delivery as approval. Use only when a person is required, not for routine progress or peer work.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "body": { "type": "string" },
                    "action": { "type": "string", "enum": ["approval", "choice", "information"], "default": "information" },
                    "dispatch_id": { "type": "string", "description": "A UUID. Reuse for retries of the same decision to avoid duplicate requests." },
                    "parent_request_id": { "type": "string" },
                    "thread_id": { "type": "string" },
                    "workspace_id": { "type": "string" },
                    "work_id": { "type": "string" },
                    "run_id": { "type": "string" }
                },
                "required": ["body"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muxa_send_message",
            "description": "Send a durable request to a same-window peer. Use review + read_only for an independent critique before finalizing important work; question + read_only for focused analysis; task + execute + narrow paths for bounded, non-overlapping delegated edits. Attach typed air_artifacts when a validated AIR workflow, plan, or trace is the shared review context. Continue useful work, wait for the structured reply, and verify the result yourself. Targets: peer (only one peer), pane:%N, @alias, or role:<name> (only one matching role).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "peer, pane:%N, %N, @alias, role:<name>, a workspace/work/role[@alias] path (follows a replaced holder until delivered), or human for an operator decision" },
                    "kind": { "type": "string", "enum": ["question", "review", "task", "notice"] },
                    "body": { "type": "string" },
                    "expects_reply": { "type": "boolean", "description": "Default true except notice. Independent of notify." },
                    "notify": { "type": "boolean", "description": "Override recipient wake: one-way notices are quiet by default; other requests wake idle recipients. Set true only for blockers, decisions, or handoffs; false records without waking. Does not interrupt busy agents or override global wake=never." },
                    "human_action": { "type": "string", "enum": ["approval", "choice", "information"], "description": "Use target human only when an operator decision is required. Explain the question, choices and recommendation in body. Internal agent requests must omit this." },
                    "work_mode": { "type": "string", "enum": ["read_only", "execute"], "description": "Default read_only." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Advisory path scope for execute work." },
                    "thread_id": { "type": "string", "description": "Stable id shared by one causal conversation. Omit for a root request; muxa assigns its request id. When parent_request_id is set, this must match the parent's canonical thread." },
                    "parent_request_id": { "type": "string", "description": "Exact prior request that caused this follow-up. Muxa validates the parent and inherits its canonical thread_id when thread_id is omitted." },
                    "workspace_id": { "type": "string", "description": "Stable workspace/project id used to group collaboration history." },
                    "work_id": { "type": "string", "description": "Durable Work id within the workspace, used to group requests across runs." },
                    "run_id": { "type": "string", "description": "Specific execution/run id for this request." },
                    "artifacts": { "type": "array", "items": { "type": "string" }, "description": "Generic artifact identifiers or locators attached to the request." },
                    "links": { "type": "array", "items": { "type": "string" }, "description": "Generic relationship or URL locators associated with the request." },
                    "withdraw_reply_wakes": { "type": "array", "maxItems": 16, "items": { "type": "string" }, "description": "Your own earlier request ids this request replaces (for example one you gave up on after it was blocked). Their late replies will not wake you; their work, replies and muxa_wait_reply results are unchanged. Applied atomically with this send." },
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
            "description": "Read worker inbox requests, or explicitly accept one request_id on a protected primary session. Primary sessions do not auto-claim queued peer work. Instructions are included; full=true is for diagnostics. Honor kind/work_mode/paths; reply only when requested.",
            "inputSchema": { "type": "object", "properties": {"request_id":{"type":"string","description":"Explicitly accept this one inbound request, including on a protected primary. Omit for normal worker inbox."},"full": {"type":"boolean", "description":"Return legacy full records. Default false."}}, "additionalProperties": false },
        }),
        json!({
            "name": "muxa_list_messages",
            "description": "List incoming, sent, or all Muxa collaboration requests for this exact agent session without claiming them. Default: newest 20 metadata-only records. Use request_id + include_body for instructions, after_update for progress, or muxa_peer_report for replies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mailbox": { "type": "string", "enum": ["incoming", "sent", "all"], "description": "Default all." },
                    "request_id": {"type":"string", "description":"Read one exact request without claiming it."},
                    "limit": {"type":"integer", "minimum":1, "maximum":100, "description":"Newest records to return. Default 20."},
                    "offset": {"type":"integer", "minimum":0, "description":"Skip this many records. Default 0."},
                    "include_body": {"type":"boolean", "description":"Include instructions and attachments for an exact request_id; bulk reads require full=true. Default false."},
                    "after_update": {"type":"integer", "minimum":0, "description":"Include only progress after this sequence; use with request_id."},
                    "full": {"type":"boolean", "description":"Return legacy full records. Default false."}
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
            "description": "Wait for a final reply or explicit cancellation/expiry. final_reply_received records receipt; reported_status is declared by the recipient. Timeout leaves work unchanged. An explicit peer_interrupted result keeps the work open; consult the opt-in recovery workflow. Prefer completion wakes or checkpoints over repeated 1-second probes. Default 30 seconds, max 600.",
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
            "name": "muxa_withdraw_reply_wake",
            "description": "Stop being woken for the replies to your own earlier requests that you no longer need, such as ones you gave up on or replaced. Works whatever their status. Only notification interest is withdrawn: the recipient's work, the reply, and muxa_wait_reply results are unchanged. Use muxa_cancel_message to stop queued work. When sending the replacement, prefer withdraw_reply_wakes on that send instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_ids": { "type": "array", "minItems": 1, "maxItems": 16, "items": { "type": "string" } }
                },
                "required": ["request_ids"],
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
async fn call_tool(
    client: &Client,
    params: Option<&Value>,
    config: &muxa::config::Config,
) -> Result<Value> {
    let params = params.ok_or_else(|| anyhow::anyhow!("missing params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
    let mut args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let name = if name == "muxa_request_attention" {
        let Some(object) = args.as_object_mut() else {
            return Ok(error_result("attention arguments must be an object"));
        };
        let action = object
            .remove("action")
            .unwrap_or_else(|| json!("information"));
        object.insert("human_action".into(), action);
        object.insert("target".into(), json!("human"));
        object.insert("kind".into(), json!("question"));
        object.insert("expects_reply".into(), json!(true));
        object.insert("notify".into(), json!(true));
        object.insert("work_mode".into(), json!("read_only"));
        "muxa_send_message"
    } else {
        name
    };

    match name {
        "muxa_status" => {
            let pane = args.get("pane").and_then(Value::as_str);
            let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
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
                        payload = if full {
                            json!({ "topology": topology })
                        } else {
                            topology_summary(&topology)
                        };
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
        "muxa_recent_prompts" => Ok(
            match crate::mcp_output::recent_prompts(client, &args).await {
                Ok(value) => json_result(&value),
                Err(error) => error_result(&format!("recent_prompts failed: {error}")),
            },
        ),
        "muxa_guide" => Ok(json_result(&launch_guide_value(config))),
        "muxa_start_agent" => {
            let explicit_agent = args.get("agent").and_then(Value::as_str);
            let Some(agent) = explicit_agent.or(config.mcp.guide.agent.as_deref()) else {
                return Ok(error_result(
                    "start_agent requires an agent argument when mcp.guide.agent is not configured",
                ));
            };
            let agent = match crate::agent_launch::AgentProgram::parse(agent) {
                Ok(agent) => agent,
                Err(error) => return Ok(error_result(&error)),
            };
            let placement_value = args.get("placement").and_then(Value::as_str).or_else(|| {
                args.get("work")
                    .and_then(Value::as_str)
                    .is_none()
                    .then_some(config.mcp.guide.placement.as_str())
            });
            let placement = match crate::agent_launch::Placement::parse(placement_value) {
                Ok(placement) => placement,
                Err(error) => return Ok(error_result(&error)),
            };
            let direction_value = args
                .get("direction")
                .and_then(Value::as_str)
                .or(Some(config.mcp.guide.direction.as_str()));
            let direction = match crate::agent_launch::SplitDirection::parse(direction_value) {
                Ok(direction) => direction,
                Err(error) => return Ok(error_result(&error)),
            };
            let options = match launch_options(&args, config, agent) {
                Ok(options) => options,
                Err(error) => return Ok(error_result(&error)),
            };
            let reported_options = options.clone();
            let defaults_applied = json!({
                "agent": explicit_agent.is_none(),
                "placement": args.get("placement").is_none() && args.get("work").is_none(),
                "options": args.get("options").is_none() && !reported_options.is_empty(),
                "direction": args.get("direction").is_none(),
            });
            let (endpoint, target) =
                match lifecycle_target(client, args.get("target").and_then(Value::as_str)).await {
                    Ok(value) => value,
                    Err(error) => return Ok(error_result(&format!("start_agent failed: {error}"))),
                };
            let purpose = match args
                .get("purpose")
                .and_then(Value::as_str)
                .unwrap_or("primary")
                .parse::<muxa::collaboration::LaunchPurpose>()
            {
                Ok(purpose) => purpose,
                Err(error) => return Ok(error_result(&error)),
            };
            let creator = match resolve_collaboration_origin(client).await {
                Ok(origin) => client
                    .collaboration_context(&origin)
                    .await
                    .ok()
                    .filter(|room| !room.current.console)
                    .map(|room| (&room.current).into()),
                Err(_) => None,
            };
            let request = crate::agent_launch::StartRequest {
                creator,
                worker_spawn: None,
                bypass_permissions: true,
                purpose,
                socket: client.socket().to_path_buf(),
                agent,
                options,
                placement,
                target: if args.get("work").is_some() && args.get("target").is_none() {
                    None
                } else {
                    target
                },
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
                alias: args
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                task: args.get("task").and_then(Value::as_str).map(str::to_string),
                generation: None,
                direction,
                account: config.account_route(agent.label()),
            };
            let is_rmux = endpoint
                .as_ref()
                .is_some_and(|e| e.host == muxa::HostKind::Rmux);
            Ok(
                match tokio::task::spawn_blocking(move || {
                    crate::mux_control::with_endpoint(endpoint, || {
                        crate::agent_launch::start(request)
                    })
                })
                .await
                {
                    Ok(Ok(result)) => {
                        let mut result = json!(result);
                        if is_rmux {
                            result["host"] = json!("rmux");
                            result["pane"] = json!(format!(
                                "rmux:{}",
                                result["pane"].as_str().unwrap_or_default()
                            ));
                        }
                        result["options"] = json!(reported_options);
                        result["defaults_applied"] = defaults_applied;
                        json_result(&result)
                    }
                    Ok(Err(error)) => error_result(&format!("start_agent failed: {error}")),
                    Err(error) => error_result(&format!("start_agent worker failed: {error}")),
                },
            )
        }
        "muxa_manage_tmux" => {
            use crate::tmux_work::ManageAction;
            let Some(action) = args.get("action").and_then(Value::as_str) else {
                return Ok(error_result("manage_tmux requires an action argument"));
            };
            let action = match crate::tmux_work::ManageAction::parse(action) {
                Ok(action) => action,
                Err(error) => return Ok(error_result(&error)),
            };
            if matches!(
                action,
                ManageAction::TerminateAgent
                    | ManageAction::CloseWork
                    | ManageAction::CloseWorkspace
            ) && !args
                .get("confirm")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(error_result(
                    "manage_tmux destructive action requires confirm=true",
                ));
            }
            if matches!(
                action,
                ManageAction::TerminateAgent | ManageAction::InterruptAgent
            ) && args.get("pane").and_then(Value::as_str).is_none()
            {
                return Ok(error_result("manage_tmux agent action requires pane"));
            }
            let (endpoint, pane) =
                match lifecycle_target(client, args.get("pane").and_then(Value::as_str)).await {
                    Ok(value) => value,
                    Err(error) => return Ok(error_result(&format!("manage_tmux failed: {error}"))),
                };
            let request = crate::tmux_work::ManageRequest {
                action,
                pane,
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
            let reported_pane = args.get("pane").cloned();
            Ok(
                match tokio::task::spawn_blocking(move || {
                    crate::mux_control::with_endpoint(endpoint, || {
                        crate::tmux_work::manage(request)
                    })
                })
                .await
                {
                    Ok(Ok(result)) => {
                        let mut result = json!(result);
                        if result.get("pane").is_some() {
                            if let Some(pane) = reported_pane {
                                result["pane"] = pane;
                            }
                        }
                        json_result(&result)
                    }
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
            Ok(
                match send_prompt_with_local_cmux(client, pane, text, submit).await {
                    // The daemon distinguishes "text injected" (`sent`) from
                    // "Enter delivered" (`submitted`); surface the latter so a
                    // caller whose submit was requested but not delivered learns
                    // the text already landed and must not blindly resend it.
                    Ok(outcome) => render_send_result(
                        text.len(),
                        pane,
                        submit,
                        Some(outcome.submitted),
                        outcome.submission_confirmed,
                    ),
                    Err(e) => error_result(&format!("send_prompt failed: {e}")),
                },
            )
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
        "muxa_fleet_status" => {
            let selector = args.get("selector").and_then(Value::as_str);
            Ok(match client.fleet_snapshot(selector).await {
                Ok(snapshot) => json_result(&crate::mcp_output::fleet_view(
                    &snapshot,
                    args.get("full").and_then(Value::as_bool).unwrap_or(false),
                )),
                Err(error) => error_result(&format!("fleet status failed: {error}")),
            })
        }
        "muxa_fleet_capture" => {
            let Some(host) = args.get("host").and_then(Value::as_str) else {
                return Ok(error_result("fleet_capture requires a `host` argument"));
            };
            let Some(pane) = args.get("pane").and_then(Value::as_str) else {
                return Ok(error_result("fleet_capture requires a `pane` argument"));
            };
            let target = match crate::fleet_cli::resolve_pane(client, host, pane).await {
                Ok(target) => target,
                Err(error) => return Ok(error_result(&format!("fleet capture failed: {error}"))),
            };
            Ok(
                match client
                    .fleet_execute(host, &muxa::FleetOperation::Capture { pane: target })
                    .await
                {
                    Ok(result) => match result.capture {
                        Some(capture) => text_result(&capture),
                        None => error_result("fleet capture returned no pane contents"),
                    },
                    Err(error) => error_result(&format!("fleet capture failed: {error}")),
                },
            )
        }
        "muxa_fleet_send_prompt" => {
            let Some(host) = args.get("host").and_then(Value::as_str) else {
                return Ok(error_result("fleet_send_prompt requires a `host` argument"));
            };
            let Some(pane) = args.get("pane").and_then(Value::as_str) else {
                return Ok(error_result("fleet_send_prompt requires a `pane` argument"));
            };
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return Ok(error_result("fleet_send_prompt requires a `text` argument"));
            };
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(true);
            let target = match crate::fleet_cli::resolve_pane(client, host, pane).await {
                Ok(target) => target,
                Err(error) => {
                    return Ok(error_result(&format!("fleet send prompt failed: {error}")));
                }
            };
            Ok(
                match client
                    .fleet_execute(
                        host,
                        &muxa::FleetOperation::SendPrompt {
                            pane: target,
                            text: text.to_string(),
                            submit,
                        },
                    )
                    .await
                {
                    Ok(result) => json_result(&json!({
                        "host": host,
                        "pane": pane,
                        "send": result.send,
                    })),
                    Err(error) => error_result(&format!("fleet send prompt failed: {error}")),
                },
            )
        }
        "muxa_dispatch_work" => {
            let mut command = vec!["work".to_string(), "dispatch".to_string()];
            if args.get("plan").and_then(Value::as_bool).unwrap_or(false) {
                command.push("--plan".into());
            }
            let mut request = args.clone();
            if let Some(object) = request.as_object_mut() {
                object.remove("plan");
            }
            match client
                .work_command(None, &command, Some(&request.to_string()))
                .await
            {
                Ok(output) if output.exit_code == 0 => {
                    match serde_json::from_str::<Value>(&output.stdout) {
                        Ok(value) => Ok(json_result(&value)),
                        Err(e) => Ok(error_result(&e.to_string())),
                    }
                }
                Ok(output) => Ok(error_result(&output.stderr)),
                Err(e) => Ok(json_result(
                    &json!({"state":"unknown","dispatch_id":args["dispatch_id"],"error":e.to_string(),"next_step":"read muxa_dispatch_status with this ID; do not create a replacement"}),
                )),
            }
        }
        "muxa_dispatch_status" => {
            let Some(id) = args.get("dispatch_id").and_then(Value::as_str) else {
                return Ok(error_result("dispatch_id required"));
            };
            match client
                .work_command(
                    None,
                    &["work".into(), "dispatch-status".into(), id.into()],
                    None,
                )
                .await
            {
                Ok(output) if output.exit_code == 0 => {
                    match serde_json::from_str::<Value>(&output.stdout) {
                        Ok(v) => Ok(json_result(&v)),
                        Err(e) => Ok(error_result(&e.to_string())),
                    }
                }
                Ok(output) => Ok(error_result(&output.stderr)),
                Err(e) => Ok(error_result(&e.to_string())),
            }
        }
        "muxa_fleet_call_peer" => Ok(fleet_call_peer(client, &args, &config.message.skills).await),
        "muxa_fleet_wait_reply" => Ok(fleet_wait_reply(client, &args).await),
        "muxa_wait_for_change" => Ok(wait_for_change(client, &args).await),
        "muxa_collaboration_guide" => {
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(match client.collaboration_context(&origin).await {
                Ok(room) => json_result(&collaboration_guide(
                    room,
                    config,
                    args.get("workflows")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    crate::mcp_output::human_actions_supported(client).await,
                )),
                Err(error) => error_result(&format!("collaboration guide failed: {error}")),
            })
        }
        "muxa_room_context" => {
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(match client.collaboration_context(&origin).await {
                Ok(room) => json_result(&json!(room)),
                Err(error) => error_result(&format!("room context failed: {error}")),
            })
        }
        "muxa_call_peer" => Ok(call_peer(client, &args, config).await),
        "muxa_start_work" => Ok(start_work(client, &args, config).await),
        "muxa_swap_account" => Ok(swap_account(client, &args).await),
        "muxa_peer_report" => Ok(peer_report(client, &args).await),
        "muxa_set_identity" => {
            let alias = args.get("alias").and_then(Value::as_str);
            let roles = string_array(&args, "roles");
            let origin = match resolve_collaboration_origin(client).await {
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
            if target.eq_ignore_ascii_case("human") {
                if let Err(error) = crate::mcp_output::require_human_actions(client).await {
                    return Ok(error_result(&error));
                }
            }
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
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            let request = match new_request_from_args(
                &args,
                kind,
                body.to_string(),
                expects_reply,
                work_mode,
                paths,
                air_artifacts,
            ) {
                Ok(request) => request,
                Err(error) => return Ok(error_result(&error)),
            };
            if let Err(error) = crate::mcp_output::require_reply_wake_withdraw(
                client,
                &request.withdraw_reply_wakes,
            )
            .await
            {
                return Ok(error_result(&error));
            }
            Ok(
                match client.collaboration_send(&origin, target, &request).await {
                    Ok(request) => json_result(&request_ack(&request)),
                    Err(error) => error_result(&format!("send_message failed: {error}")),
                },
            )
        }
        "muxa_inbox" => {
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            let inbox = if let Some(value) = args.get("request_id") {
                let Some(id) = value.as_str().filter(|id| !id.trim().is_empty()) else {
                    return Ok(error_result("request_id must be a nonempty string"));
                };
                if let Err(error) = crate::require_capability(
                    client,
                    muxa::collaboration::HUMAN_PRIORITY_CAPABILITY,
                    "explicit inbox acceptance",
                )
                .await
                {
                    return Ok(error_result(&error.to_string()));
                }
                client.collaboration_claim(&origin, id).await
            } else {
                client.collaboration_inbox(&origin).await
            };
            Ok(match inbox {
                Ok(requests) => json_result(&json!({ "requests": requests.iter().map(|request| {
                    if args.get("full").and_then(Value::as_bool).unwrap_or(false) { json!(request) }
                    else { crate::collaboration_output::message(request, true) }
                }).collect::<Vec<_>>() })),
                Err(error) => error_result(&format!("inbox failed: {error}")),
            })
        }
        "muxa_list_messages" => {
            if let Err(error) = crate::mcp_output::validate_message_read(&args) {
                return Ok(error_result(error));
            }
            let mailbox = match parse_mailbox(args.get("mailbox").and_then(Value::as_str)) {
                Ok(mailbox) => mailbox,
                Err(error) => return Ok(error_result(error)),
            };
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            if let Some(id) = args.get("request_id").and_then(Value::as_str) {
                // Incremental reads omit the reply: never acknowledge it implicitly.
                let request = if args.get("after_update").is_some()
                    && args.get("full").and_then(Value::as_bool) != Some(true)
                {
                    client
                        .collaboration_peek_view(
                            &origin,
                            id,
                            muxa::collaboration::CollaborationReadView::Progress {
                                after_update: args
                                    .get("after_update")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0),
                            },
                        )
                        .await
                } else {
                    client
                        .collaboration_get_view(
                            &origin,
                            id,
                            if args.get("full").and_then(Value::as_bool) == Some(true)
                                || args.get("include_body").and_then(Value::as_bool) == Some(true)
                            {
                                muxa::collaboration::CollaborationReadView::Full
                            } else {
                                muxa::collaboration::CollaborationReadView::Outcome
                            },
                        )
                        .await
                };
                return Ok(match request {
                    Ok(request) => json_result(&message_view(&request, &args)),
                    Err(error) => error_result(&format!("list_messages failed: {error}")),
                });
            }
            let offset = usize::try_from(args.get("offset").and_then(Value::as_u64).unwrap_or(0))
                .unwrap_or(usize::MAX);
            let limit = usize::try_from(
                args.get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .clamp(1, 100),
            )
            .unwrap_or(100);
            let query = muxa::collaboration::CollaborationQuery {
                limit: Some(limit),
                ..Default::default()
            };
            let metadata_only = args.get("full").and_then(Value::as_bool) != Some(true);
            Ok(
                match client
                    .collaboration_query(
                        &origin,
                        mailbox,
                        muxa::collaboration::MailboxScope::Caller,
                        &query,
                        offset,
                        metadata_only,
                    )
                    .await
                {
                    Ok(page) => json_result(
                        &json!({"requests": page.requests.iter().map(|request| message_view(request, &args)).collect::<Vec<_>>(),
                    "has_more":page.has_more,"next_offset":page.has_more.then(|| offset.saturating_add(limit))}),
                    ),
                    Err(error) => error_result(&format!("list_messages failed: {error}")),
                },
            )
        }
        "muxa_update_request" | "muxa_fleet_update_request" => {
            let Some(request_id) = args.get("request_id").and_then(Value::as_str) else {
                return Ok(error_result("request_id is required"));
            };
            let Some(body) = args.get("body").and_then(Value::as_str) else {
                return Ok(error_result("body is required"));
            };
            if body.trim().is_empty() || body.len() > 8192 {
                return Ok(error_result("body must contain 1..8192 UTF-8 bytes"));
            }
            if name == "muxa_fleet_update_request" {
                let Some(host) = args.get("host").and_then(Value::as_str) else {
                    return Ok(error_result("host is required"));
                };
                let pane = match serde_json::from_value::<muxa::PaneKey>(
                    args.get("pane_key").cloned().unwrap_or(Value::Null),
                ) {
                    Ok(pane) => pane,
                    Err(error) => return Ok(error_result(&format!("invalid pane_key: {error}"))),
                };
                return Ok(
                    match client
                        .fleet_execute(
                            host,
                            &muxa::FleetOperation::CollaborationUpdate {
                                pane,
                                request_id: request_id.into(),
                                body: body.into(),
                            },
                        )
                        .await
                    {
                        Ok(result) => match result.collaboration_request {
                            Some(request) => json_result(
                                &crate::collaboration_output::mutation_receipt(&request),
                            ),
                            None => error_result("update returned no request"),
                        },
                        Err(error) => error_result(&error.to_string()),
                    },
                );
            }
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client
                    .collaboration_update_view(
                        &origin,
                        request_id,
                        body,
                        muxa::collaboration::CollaborationReadView::Metadata,
                    )
                    .await
                {
                    Ok(request) => {
                        json_result(&crate::collaboration_output::mutation_receipt(&request))
                    }
                    Err(error) => error_result(&error.to_string()),
                },
            )
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
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client
                    .collaboration_reply_view(
                        &origin,
                        request_id,
                        status,
                        body,
                        &artifacts,
                        &air_artifacts,
                        muxa::collaboration::CollaborationReadView::Metadata,
                    )
                    .await
                {
                    Ok(request) => {
                        json_result(&crate::collaboration_output::mutation_receipt(&request))
                    }
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
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client.collaboration_cancel(&origin, request_id).await {
                    Ok(request) => {
                        json_result(&crate::collaboration_output::mutation_receipt(&request))
                    }
                    Err(error) => error_result(&format!("cancel_message failed: {error}")),
                },
            )
        }
        "muxa_withdraw_reply_wake" => {
            let request_ids = match request_id_array(&args, "request_ids") {
                Ok(Some(ids)) if !ids.is_empty() => ids,
                Ok(_) => {
                    return Ok(error_result(
                        "withdraw_reply_wake requires a non-empty `request_ids` array",
                    ))
                }
                Err(error) => return Ok(error_result(&error)),
            };
            if let Err(error) =
                crate::mcp_output::require_reply_wake_withdraw(client, &request_ids).await
            {
                return Ok(error_result(&error));
            }
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client
                    .collaboration_withdraw_reply_wakes(&origin, &request_ids)
                    .await
                {
                    Ok(requests) => json_result(&json!({
                        "withdrawn": requests
                            .iter()
                            .map(crate::collaboration_output::mutation_receipt)
                            .collect::<Vec<_>>(),
                    })),
                    Err(error) => error_result(&format!("withdraw_reply_wake failed: {error}")),
                },
            )
        }
        other => Ok(error_result(&format!("unknown tool: {other}"))),
    }
}

/// Resolve namespace and server together before entering synchronous lifecycle
/// code. A namespaced pane must never fall back onto a different ambient host.
async fn lifecycle_target(
    client: &Client,
    target: Option<&str>,
) -> Result<(Option<muxa::BackendEndpoint>, Option<String>), String> {
    let resolved_origin = resolve_collaboration_origin(client).await;
    if target.is_none_or(|t| muxa::backend::pane_id_host_kind(t).is_none())
        && pane_env_is_borrowed()
    {
        let origin = resolved_origin.as_ref().map_err(Clone::clone)?;
        if origin.pane.is_empty() || origin.socket.is_none() {
            return Err("lifecycle caller is not bound to a server yet; refresh room context before retrying".into());
        }
    }
    let origin = resolved_origin.ok();
    let target = target.map(str::to_string).or_else(|| {
        origin
            .as_ref()
            .map(|o| o.pane.clone())
            .filter(|p| !p.is_empty())
    });
    let Some(pane) = target.as_deref() else {
        return Ok((None, None));
    };
    let Some(host) = muxa::backend::pane_id_host_kind(pane) else {
        // Session names belong to the caller's server, not the app-server's.
        let endpoint = origin.and_then(|o| {
            let host = muxa::backend::pane_id_host_kind(&o.pane)?;
            crate::mux_control::supported(host).then_some(())?;
            Some(muxa::BackendEndpoint {
                host,
                socket: o.socket?,
            })
        });
        return Ok((endpoint, target));
    };
    if !crate::mux_control::supported(host) {
        return Err(format!("{host} does not support this lifecycle operation"));
    }
    let agents = client
        .by_pane(pane)
        .await
        .map_err(|e| format!("resolve lifecycle endpoint: {e}"))?;
    let mut endpoints: Vec<_> = agents
        .iter()
        .filter_map(|a| a.tmux_socket.clone())
        .collect();
    if let Some(origin) = origin.filter(|o| o.pane == pane) {
        if let Some(socket) = origin.socket {
            endpoints.push(socket);
        }
    }
    let endpoint = select_lifecycle_endpoint(host, endpoints)?;
    Ok((
        Some(endpoint),
        Some(pane.strip_prefix("rmux:").unwrap_or(pane).to_string()),
    ))
}

fn select_lifecycle_endpoint(
    host: muxa::HostKind,
    mut sockets: Vec<String>,
) -> Result<muxa::BackendEndpoint, String> {
    sockets.retain(|s| !s.trim().is_empty());
    sockets.sort();
    sockets.dedup();
    if sockets.len() != 1 {
        return Err("cannot resolve a unique lifecycle server for this pane; refresh room context and use an exact tracked target".into());
    }
    Ok(muxa::BackendEndpoint {
        host,
        socket: sockets.remove(0),
    })
}

/// Keep the default fleet view useful for routing without embedding every
/// agent's prompts, responses, workload tree, and backend metadata. Callers
/// can request `full=true` when they genuinely need the canonical topology.
fn topology_summary(topology: &muxa::TopologySnapshot) -> Value {
    let mut agents = Vec::new();
    let mut states = [0_usize; 7];
    let mut window_count = 0_usize;
    let mut pane_count = 0_usize;

    for session in &topology.sessions {
        window_count += session.windows.len();
        for window in &session.windows {
            pane_count += window.panes.len();
            for pane in &window.panes {
                if let Some(agent) = &pane.agent {
                    states[state_index(agent.state)] += 1;
                    let endpoint = &pane.key.window.session.endpoint;
                    agents.push(agent_status_summary(
                        agent,
                        agent.pane.as_deref().or(Some(&pane.key.pane_id)),
                        Some(&session.name),
                        Some(&window.name),
                        Some(endpoint.host),
                        Some(&endpoint.socket),
                    ));
                }
            }
        }
    }

    let unassigned_agents = topology
        .unassigned_agents
        .iter()
        .map(|agent| {
            states[state_index(agent.state)] += 1;
            let host = agent
                .pane
                .as_deref()
                .and_then(muxa::backend::pane_id_host_kind);
            agent_status_summary(
                agent,
                agent.pane.as_deref(),
                None,
                None,
                host,
                agent.tmux_socket.as_deref(),
            )
        })
        .collect::<Vec<_>>();

    json!({
        "summary": {
            "sessions": topology.sessions.len(),
            "windows": window_count,
            "panes": pane_count,
            "agents": agents.len() + unassigned_agents.len(),
            "states": {
                "starting": states[0],
                "working": states[1],
                "idle": states[2],
                "waiting_input": states[3],
                "waiting_choice": states[4],
                "error": states[5],
                "stopped": states[6],
            },
            "unmapped_panes": topology.unmapped_panes.len(),
            "unassigned_agents": unassigned_agents.len(),
        },
        "agents": agents,
        "unassigned_agents": unassigned_agents,
        "next_step": "Use muxa_status with pane for details, or full=true only for the complete topology."
    })
}

fn state_index(state: AgentState) -> usize {
    match state {
        AgentState::Starting => 0,
        AgentState::Working => 1,
        AgentState::Idle => 2,
        AgentState::WaitingInput => 3,
        AgentState::WaitingChoice => 4,
        AgentState::Error => 5,
        AgentState::Stopped => 6,
    }
}

fn agent_status_summary(
    agent: &Agent,
    pane: Option<&str>,
    session: Option<&str>,
    window: Option<&str>,
    host: Option<muxa::HostKind>,
    socket: Option<&str>,
) -> Value {
    let mut value = json!({
        "pane": pane,
        "kind": agent.kind,
        "state": agent.state,
        "last_activity_at": agent.last_activity_at,
    });
    if let Some(session) = session {
        value["session"] = json!(session);
    }
    if let Some(window) = window {
        value["window"] = json!(window);
    }
    if let Some(host) = host {
        value["host"] = json!(host);
    }
    if let Some(socket) = socket {
        value["socket"] = json!(socket);
    }
    if let Some(cwd) = &agent.cwd {
        value["cwd"] = json!(cwd);
    }
    if let Some(model) = &agent.model {
        value["model"] = json!(model);
    }
    if let Some(title) = &agent.ai_title {
        value["title"] = json!(crate::truncate_cell(title, 160));
    }
    if let Some(notification) = &agent.last_notification {
        value["attention"] = json!(crate::truncate_cell(notification, 160));
    }
    if let Some(context_used_pct) = agent.context_used_pct {
        value["context_used_pct"] = json!(context_used_pct);
    }
    value
}

#[allow(clippy::too_many_lines)] // declarative guide keeps detailed recovery workflows opt-in
fn collaboration_guide(
    room: RoomContext,
    config: &muxa::config::Config,
    include_workflows: bool,
    human_actions: bool,
) -> Value {
    let next_step = match room.peers.len() {
        0 => "No peer is available. Continue locally or run another agent in this tmux window.",
        1 => "One peer is available; target `peer` or its explicit pane id.",
        _ => {
            "Multiple peers are available; choose an explicit pane, @alias, or unique role:<name>."
        }
    };
    let mut guide = json!({
        "capabilities": {"human_actions":human_actions},
        "purpose": "Use another live agent as an independent reviewer or a bounded delegated subagent to improve important work.",
        "room": room,
        "user_launch_preferences": launch_guide_value(config),
        "message_skills": config.message.skills.keys().collect::<Vec<_>>(),
        "transport_contract": {
            "discovery": "Discover matching names only, then read one exact selected schema; never print all matching descriptions. Retain tool handles/schema for the session. Do not print ALL_TOOLS again for a known tool; refresh only after a version/capability change or unknown-tool/argument error.",
            "bounded_reads": "Prompt history defaults to 20 entries. Fleet status is a summary; full=true explicitly requests large data. Inbox already includes task instructions. include_body requires an exact request_id unless full=true.",
            "final_reply_received": "Only reports that a recipient submitted a final reply; never proves success.",
            "reported_status": "Recipient-declared completed/blocked/declined/failed. Muxa does not judge the work.",
            "timeout": "The wait ended; work is unchanged. Prefer completion wakes or substantive checkpoints. If waiting in the foreground, use 30-60 seconds; a 1-second probe is for an expected result, not a polling loop. Do not renew waits blindly.",
            "progress": "Use update_request (CLI: msg update REQUEST BODY) for consolidated guidance/progress on the same request; reply once at the end. No receipt acknowledgements or unchanged status messages.",
            "abandon": "When you give up on or replace your own request, name it in withdraw_reply_wakes on the replacement (CLI: msg send --withdraw REQUEST), or call muxa_withdraw_reply_wake. Its late reply then stays readable but does not wake you. Nothing else changes.",
            "reads": "Reports omit original requests. Lists default to 20 metadata records; request_id + include_body reads instructions, full=true opts into legacy records.",
            "cli": "muxa msg wait REQUEST --body [--output FILE] prints the reply directly. Keep exit status/stderr; do not hide errors with grep or retry sends after empty filtered output. --notify means true; --notify false is quiet. Never judge completion from body keywords.",
            "authority": "execute=true and bypass-permission spawning require existing explicit authorization. Preserve request identity on retry; a timeout never authorizes duplicate work.",
            "fleet": "Use exact returned host/pane_key/request_id. Observe hosts cannot execute. Resume yielded host tool cells instead of duplicating waits."
        },
        "next_step": next_step,
        "workflows": {
            "peer_interruption": {
                "wait_slice_secs": 60,
                "owner": "The coordinator owns recovery; a capped peer may never send a reply. Set an overall wait budget; never renew timed-out waits blindly.",
                "health": "Read the durable reply first, then fresh muxa_status for the exact recipient agent/session/socket or muxa_fleet_status for the returned host/pane_key. Request to.state is stale. Missing/offline is unknown availability, not proof of a cap or process exit.",
                "evidence": ["state", "rate_limit_scope", "rate_limit_source", "rate_limited_until", "last_notification"],
                "trigger": "Known cap/error/stopped, repeated unavailability, or exhausted overall budget requires a recovery decision. Utilization alone is insufficient; reset time is only a recheck hint.",
                "recovery": "Record one update with evidence/artifacts. Use authorized independent work, one bounded reset recheck, or a healthy peer. Verify current health and progress; do not repeatedly prompt the capped agent.",
                "handoff": "Cancel queued local requests with muxa_cancel_message and verify success. Claimed requests need supported authorized stop/handoff or isolated replacement work; a cap/offline state does not prevent resumption. Preserve request/thread identities and never impersonate the peer.",
                "human": "Only if a real choice/approval/information is needed, follow human_feedback with one linked request; reuse its ID and existing authorization. Preserve remote endpoint identity.",
                "completion": "Do not fabricate the peer reply. If ending your own incoming attempt, reply blocked with dependency/handoff evidence; keep it open during an active human decision.",
                "limitation": "Upgraded daemons return peer_interrupted without completing work. A running coordinator handles recovery first; if unavailable or unacknowledged for two minutes the daemon creates one durable human action. Reuse interruption.action_request_id; inspect interruption.decision without interpreting completion as consent. Older hosts still need bounded health checks."
            },
            "human_feedback": {
                "when": "Only when an actual operator approval, choice, or missing information is required; never for routine peer traffic or already-authorized work.",
                "tool": "muxa_send_message",
                "request": { "target": "human", "kind": "question", "expects_reply": true, "human_action": "choice" },
                "actions": ["approval", "choice", "information"],
                "context": "Include the exact decision, why required, options and recommendation. Include parent_request_id only for an existing request you participate in; inherit its thread and preserve known work identity.",
                "wait": "Retain the returned request_id and use muxa_wait_reply. Continue independent work; never treat a timeout as consent or create duplicate questions. Keep the original peer request open while waiting.",
                "answer": "The human answers in Need you (watch: e). Read status and body; completed is not necessarily approval. Honor refusal/cancellation and complete the original peer request after authorized work.",
                "identity": "Never use console=true or claim/answer the operator inbox as an agent.",
                "fallback": "If the installed schema lacks human_action, ask in the existing user conversation. Do not silently drop the question or redirect it to a peer."
            },
            "reviewer": {
                "when": "After implementation, self-review, and relevant tests; before declaring important work complete.",
                "provider": "For independent review, Codex chooses Claude and Claude chooses Codex unless the user explicitly selects another provider.",
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
            "ongoing_fleet_work": {
                "steps": [
                    "Consolidate verified guidance with muxa_fleet_update_request on the returned host/pane_key/request_id; do not inject repeated task prompts.",
                    "Report nonterminal progress with muxa_update_request on the same request, never by addressing pane:console.",
                    "At agreed checkpoints read muxa_list_messages; updates do not interrupt terminal input or widen execution authority.",
                    "Wait with after_update=0 for progress, then resume with next_after_update; finish with exactly one terminal reply."
                ]
            },
            "incoming_request": {
                "steps": [
                    "Call muxa_inbox to claim/read unless the complete request is already claimed and delivered in the prompt.",
                    "Honor kind, work_mode, and paths; read_only never authorizes edits.",
                    "Only when expects_reply=true, reply exactly once with completed, blocked, declined, or failed and useful artifacts. Never acknowledge one-way notices."
                ]
            }
        },
        "guardrails": [
            "Use room.self for identity; never derive it from TMUX_PANE or raw tmux commands, which may belong to a shared app-server.",
            "Use Muxa lifecycle tools instead of raw tmux; name Fleet host/pane explicitly and respect observe mode.",
            "Never infer GitHub/PR work without an explicit PR number or URL.",
            "One-way notices need no acknowledgement; notify=true is for blockers, decisions, conflicts or handoffs. Batch progress at checkpoints.",
            "For review use the opposite provider: Codex to Claude, Claude to Codex; an explicit user selection takes precedence.",
            "Human sessions default to primary and are protected from peer injection. High-level calls reuse or create workers, including when a primary is named. Set context_bound=true only for its exact conversation; those primary requests stay queued. Accept a specific request with muxa_inbox(request_id) only when the human task permits it. Spawned peers use worker purpose with configured provider defaults, not a cloned conversation.",
            "Claim requests with muxa_inbox unless already claimed and delivered in full. Honor kind/work_mode/paths; read_only never authorizes edits. Reply once only when expects_reply=true; progress uses update_request.",
            "Do not create overlapping concurrent edits unless separate worktrees isolate them.",
            "Do not treat a peer reply as proof; verify it against the repository and tests.",
            "Do not delegate merely for ceremony or recursively bounce work without a concrete benefit."
        ]
    });
    if !human_actions {
        guide["workflows"]
            .as_object_mut()
            .unwrap()
            .remove("human_feedback");
        guide["workflows"]["peer_interruption"]["limitation"] = json!("The running daemon has not advertised human-action support. Use bounded health checks and the existing user conversation for required decisions.");
        guide["workflows"]["peer_interruption"]["human"] = json!("Human-action support is unverified on the running daemon. Ask through the existing user conversation; do not retry peer aliases.");
    }
    if !include_workflows {
        guide.as_object_mut().unwrap().remove("workflows");
    }
    guide
}

#[derive(Debug, Clone)]
struct PeerSelection {
    /// The registered peer, when one exists. `None` for a pane muxa just
    /// launched whose agent has not registered a session yet — the request is
    /// addressed to the pane and the daemon adopts it onto the session that
    /// registers there.
    peer: Option<Participant>,
    pane: String,
    reason: String,
}

impl PeerSelection {
    fn registered(peer: Participant, reason: String) -> Self {
        Self {
            pane: peer.pane.clone(),
            peer: Some(peer),
            reason,
        }
    }

    fn pending(pane: String, reason: String) -> Self {
        Self {
            peer: None,
            pane,
            reason,
        }
    }
}

/// Read a prior peer's structured response without asking it to do new work.
/// Listing is non-claiming; the final exact get records that the sender read
/// the reply, matching `muxa_wait_reply` semantics.
async fn peer_report(client: &Client, args: &Value) -> Value {
    let origin = match resolve_collaboration_origin(client).await {
        Ok(origin) => origin,
        Err(error) => return error_result(&error),
    };
    let view = if args.get("full").and_then(Value::as_bool).unwrap_or(false) {
        muxa::collaboration::CollaborationReadView::Full
    } else {
        muxa::collaboration::CollaborationReadView::Outcome
    };
    if let Some(request_id) = args.get("request_id").and_then(Value::as_str) {
        return match client
            .collaboration_get_view(&origin, request_id, view)
            .await
        {
            Ok(request) if request.reply.is_some() => json_result(&json!({
                "selection_reason": "explicit request_id",
                if args.get("full").and_then(Value::as_bool).unwrap_or(false) { "request" } else { "result" }: report_view(&request, args),
            })),
            Ok(_) => error_result(&format!(
                "Muxa request {request_id:?} has no final reply yet"
            )),
            Err(error) => error_result(&format!("peer_report failed: {error}")),
        };
    }

    let target = args.get("target").and_then(Value::as_str).unwrap_or("peer");
    let selected = match newest_peer_report(client, &origin, target).await {
        Ok(request) => request,
        Err(error) => return error_result(&format!("peer_report list failed: {error}")),
    };
    let Some(selected) = selected else {
        return error_result(&format!(
            "no received Muxa peer report matches {target:?}; use muxa_call_peer only if the user wants a new request"
        ));
    };
    match client
        .collaboration_get_view(&origin, &selected.id, view)
        .await
    {
        Ok(request) => json_result(&json!({
            "selection_reason": format!("newest received report matching {target:?}"),
            if args.get("full").and_then(Value::as_bool).unwrap_or(false) { "request" } else { "result" }: report_view(&request, args),
        })),
        Err(error) => error_result(&format!("peer_report failed: {error}")),
    }
}

async fn newest_peer_report(
    client: &Client,
    origin: &CollaborationOrigin,
    target: &str,
) -> Result<Option<CollaborationRequest>, String> {
    let mut query = muxa::collaboration::CollaborationQuery {
        limit: Some(20),
        ..Default::default()
    };
    loop {
        let result = client
            .collaboration_query_result(
                origin,
                RequestMailbox::Sent,
                muxa::collaboration::MailboxScope::Caller,
                &query,
                0,
                muxa::collaboration::CollaborationReadView::Metadata,
                false,
            )
            .await
            .map_err(|error| error.to_string())?;
        if let Some(legacy) = result.legacy_requests {
            let selected = legacy
                .into_iter()
                .filter(|request| {
                    request.reply.is_some() && peer_report_target_matches(&request.to, target)
                })
                .max_by_key(|request| request.created_at);
            return Ok(selected.map(|mut request| {
                request.omit_list_bodies();
                request
            }));
        }
        let page = result.page;
        if let Some(request) = page.requests.into_iter().find(|request| {
            request.reply.is_some() && peer_report_target_matches(&request.to, target)
        }) {
            return Ok(Some(request));
        }
        if !page.has_more {
            return Ok(None);
        }
        query.cursor = Some(
            page.next_cursor
                .ok_or("report page omitted its continuation cursor")?,
        );
    }
}

fn peer_report_target_matches(peer: &Participant, target: &str) -> bool {
    let target = target.trim();
    if target.is_empty()
        || target.eq_ignore_ascii_case("auto")
        || target.eq_ignore_ascii_case("peer")
        || target.eq_ignore_ascii_case("@peer")
        || target.eq_ignore_ascii_case("@muxa-peer")
    {
        return true;
    }
    let pane_target = target.strip_prefix("pane:").unwrap_or(target);
    if peer.pane == pane_target {
        return true;
    }
    if let Some(kind) = provider_kind(target) {
        return peer.agent_kind == kind;
    }
    if let Some(role) = target.strip_prefix("role:") {
        return peer
            .roles
            .iter()
            .any(|value| value.eq_ignore_ascii_case(role));
    }
    let alias = target
        .strip_prefix('@')
        .or_else(|| target.strip_prefix("alias:"))
        .unwrap_or(target);
    peer.alias
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(alias))
}

fn peer_worker_route(
    room: &RoomContext,
    target: &str,
    args: &Value,
) -> std::result::Result<
    (
        Option<PeerSelection>,
        Option<crate::agent_launch::AgentProgram>,
    ),
    String,
> {
    let mut selection = select_peer(room, target)?;
    let context_bound = args
        .get("context_bound")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut worker_program = None;
    if !context_bound {
        if let Some(primary) = selection
            .as_ref()
            .and_then(|selected| selected.peer.as_ref())
            .filter(|peer| !peer.accepts_peer_work())
        {
            let program = agent_program_for_kind(primary.agent_kind);
            selection = select_peer(room, program.label())?;
            if let Some(worker) = selection.as_mut() {
                worker.reason = format!("worker for {target}: {}", worker.reason);
            }
            worker_program = Some(program);
        }
    } else if selection.is_none() {
        return Err(
            "context_bound requires an existing exact pane or alias; no request was created".into(),
        );
    }
    Ok((selection, worker_program))
}

/// Whether `target` names a collaboration slot (`@alias`, `alias:`,
/// `role:`, or a `workspace/work/role` path) rather than a pane, a provider
/// (`@codex`) or the reserved `@peer` routing words.
fn names_a_slot(target: &str) -> bool {
    let target = target.trim();
    if target.starts_with("role:") || muxa::collaboration::parse_slot_path(target).is_some() {
        return true;
    }
    let Some(alias) = target
        .strip_prefix('@')
        .or_else(|| target.strip_prefix("alias:"))
    else {
        return false;
    };
    !matches!(alias, "peer" | "muxa-peer")
        && crate::agent_launch::AgentProgram::parse(alias).is_err()
}

/// Send a peer call by its logical target when it named a slot, falling
/// back to the locally selected pane only when the daemon refused the slot
/// outright (for example a role two peers share).
async fn send_peer_call(
    client: &Client,
    origin: &CollaborationOrigin,
    target: &str,
    logical: bool,
    pane_selector: Option<&str>,
    request: &NewRequest,
) -> Result<muxa::collaboration::CollaborationRequest, muxa::ipc::RuntimeError> {
    if logical {
        match client.collaboration_send(origin, target, request).await {
            Ok(sent) => return Ok(sent),
            // Only an explicit refusal proves nothing was stored. After a
            // lost or garbled response the request may exist, and a second
            // send would duplicate the work.
            Err(muxa::ipc::RuntimeError::Daemon(_)) if pane_selector.is_some() => {}
            Err(error) => return Err(error),
        }
    }
    let selector = pane_selector.unwrap_or(target);
    client.collaboration_send(origin, selector, request).await
}

/// One high-level colleague call behind the conversational `@peer` protocol.
/// The model still owns understanding the user's sentence; muxa owns every
/// deterministic and security-sensitive step after that: prompt expansion,
/// target selection, contract construction, optional explicit spawn, durable
/// delivery, and reply correlation.
#[allow(clippy::too_many_lines)] // keep the security-sensitive call sequence linear and auditable
async fn call_peer(client: &Client, args: &Value, config: &muxa::config::Config) -> Value {
    for key in ["context_bound", "spawn_if_missing"] {
        if args.get(key).is_some_and(|value| !value.is_boolean()) {
            return error_result(&format!("{key} must be a boolean"));
        }
    }
    let (body, expanded_skill) = match peer_call_body(args, &config.message.skills) {
        Ok(body) => body,
        Err(error) => return error_result(&error),
    };
    let (kind, work_mode) = match peer_call_contract(args) {
        Ok(contract) => contract,
        Err(error) => return error_result(error),
    };
    let air_artifacts = match parse_air_artifact_references(args) {
        Ok(references) => references,
        Err(error) => return error_result(&error),
    };
    let paths = string_array(args, "paths");
    let request =
        match new_request_from_args(args, kind, body, true, work_mode, paths, air_artifacts) {
            Ok(request) => request,
            Err(error) => return error_result(&error),
        };
    if let Err(error) =
        crate::mcp_output::require_reply_wake_withdraw(client, &request.withdraw_reply_wakes).await
    {
        return error_result(&error);
    }
    let origin = match resolve_collaboration_origin(client).await {
        Ok(origin) => origin,
        Err(error) => return error_result(&error),
    };
    let room = match client.collaboration_context(&origin).await {
        Ok(room) => room,
        Err(error) => return error_result(&format!("call_peer context failed: {error}")),
    };
    let target = args.get("target").and_then(Value::as_str).unwrap_or("auto");
    let target = review_provider_target(target, kind, room.current.agent_kind);
    // A `workspace/work/role` path names a slot the daemon resolves across
    // windows; there is no local peer to select or worker to spawn for it.
    let path_target = muxa::collaboration::parse_slot_path(target).is_some();
    let context_bound = args
        .get("context_bound")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if context_bound && path_target {
        return error_result(
            "context_bound needs the exact session that holds the conversation; name its pane or @alias, not a workspace/work/role path",
        );
    }
    let (mut selection, worker_program) = if path_target {
        (None, None)
    } else {
        match peer_worker_route(&room, target, args) {
            Ok(route) => route,
            Err(error) => return error_result(&error),
        }
    };
    let mut spawned = None;

    if selection.is_none() && !path_target {
        let spawn_target = if let Some(program) = worker_program {
            program.label()
        } else {
            target
        };
        let suggested =
            match peer_spawn_program(args, &room.current, spawn_target, &config.mcp.guide) {
                Ok(program) => program,
                Err(error) => return error_result(&error),
            };
        if !args
            .get("spawn_if_missing")
            .and_then(Value::as_bool)
            .unwrap_or(config.collaboration.auto_spawn_workers)
        {
            return json_result(&json!({
                "sent": false,
                "reason": "worker_spawn_disabled",
                "message": format!(
                    "No eligible {target:?} worker is available and automatic creation is disabled. Enable collaboration.auto_spawn_workers or explicitly allow this spawn."
                ),
                "suggested_call": {
                    "target": target,
                    "spawn_if_missing": true,
                    "spawn_agent": suggested.label(),
                },
                "available_peers": room.peers,
            }));
        }
        let capacity = match acquire_peer_capacity(client, &origin, args, target, suggested).await {
            Ok(capacity) => capacity,
            Err(error) => return error_result(&error),
        };
        match capacity {
            muxa::collaboration::WorkerSpawnAcquisition::Draining {
                drain_id,
                expires_at,
            } => {
                return json_result(
                    &json!({"sent":false,"reason":"worker_draining","drain_id":drain_id,"expires_at":expires_at,"next_step":"Retry after the worker drain completes; no replacement was spawned."}),
                );
            }
            muxa::collaboration::WorkerSpawnAcquisition::Reuse { peer }
            | muxa::collaboration::WorkerSpawnAcquisition::Pending {
                peer: Some(peer), ..
            } => {
                selection = Some(PeerSelection::registered(
                    peer,
                    "daemon worker capacity reuse".into(),
                ));
            }
            muxa::collaboration::WorkerSpawnAcquisition::Pending {
                launch_id,
                phase,
                pane,
                socket,
                detail,
                peer: None,
            } => {
                return json_result(&json!({
                    "sent":false, "reason":"launch_pending", "launch_id":launch_id, "phase":phase,
                    "pane":pane, "socket":socket, "detail":detail,
                    "next_step":"The existing launch owns this capacity. Reconcile it, or explicitly release it after stopping its launcher; no replacement was spawned.",
                }));
            }
            muxa::collaboration::WorkerSpawnAcquisition::LaunchToken { ticket } => {
                match spawn_peer(
                    client,
                    &room,
                    args,
                    target,
                    suggested,
                    config,
                    ticket.clone(),
                )
                .await
                {
                    Ok((new_selection, start_result)) => {
                        selection = Some(new_selection);
                        spawned = Some(start_result);
                    }
                    Err(error) => {
                        let _ = client
                            .collaboration_worker_spawn(
                                &ticket,
                                &muxa::collaboration::WorkerSpawnAction::Uncertain {
                                    detail: error.clone(),
                                },
                            )
                            .await;
                        let phase =
                            client
                                .collaboration_worker_spawns()
                                .await
                                .ok()
                                .and_then(|records| {
                                    records
                                        .into_iter()
                                        .find(|r| r.launch_id == ticket.launch_id)
                                        .map(|r| r.phase)
                                });
                        let failed = phase == Some(muxa::collaboration::WorkerSpawnPhase::Failed);
                        return json_result(&json!({
                            "sent":false, "reason":if failed { "worker_spawn_failed" } else { "launch_pending" }, "launch_id":ticket.launch_id,
                            "phase":phase, "error":error, "next_step":if failed { "Reservation closed; correct the reported failure and retry." } else { "Reconcile or explicitly release this launch before creating another worker." },
                        }));
                    }
                }
            }
        }
    }

    let (selected_peer, selection_reason, pane_selector) = match selection {
        Some(selection) => (
            selection.peer,
            selection.reason,
            Some(format!("pane:{}", selection.pane)),
        ),
        None => (None, format!("logical slot {target}"), None),
    };
    // Send a named slot (path, `@alias`, `role:`) as itself so the daemon
    // records it and the request can follow a replaced holder. A provider or
    // worker substitution was a local choice, so it is sent by pane, and a
    // context-bound call must stay with the exact session it selected.
    let logical =
        !context_bound && (path_target || (worker_program.is_none() && names_a_slot(target)));
    let sent = match send_peer_call(
        client,
        &origin,
        target,
        logical,
        pane_selector.as_deref(),
        &request,
    )
    .await
    {
        Ok(request) => request,
        Err(error) => return error_result(&format!("call_peer send failed: {error}")),
    };
    let peer_pending = muxa::collaboration::is_pending_session(&sent.to.agent_session_id);
    let mut common = json!({
        "sent": true,
        "selected_peer": selected_peer.unwrap_or_else(|| sent.to.clone()),
        "selection_reason": selection_reason,
        "expanded_skill": expanded_skill,
        "spawned": spawned,
        "request_id": sent.id,
        "thread_id": sent.thread_id,
        "parent_request_id": sent.parent_request_id,
        "workspace_id": sent.workspace_id,
        "work_id": sent.work_id,
        "run_id": sent.run_id,
        "kind": kind,
        "work_mode": work_mode,
    });
    if !sent.from.console && !sent.to.accepts_peer_work() {
        common["final_reply_received"] = json!(false);
        common["reason"] = json!("deferred_primary");
        common["next_step"] = json!("Target is a protected human session. Use a worker; explicit by-id acceptance remains available on the target.");
        return json_result(&common);
    }
    if peer_pending {
        common["peer_pending"] = json!(true);
        common["delivery"] = json!(format!(
            "pane {} has no registered agent session yet; muxad delivers this request as soon as \
             that pane reports idle, which needs muxa to see an agent process there. Wait on \
             request_id {} with muxa_wait_reply — never watch the pane with capture-pane or \
             sleep. If the reply times out, check `muxa status` for a row on the pane.",
            sent.to.pane, sent.id
        ));
    }
    if !args.get("wait").and_then(Value::as_bool).unwrap_or(true) {
        let mut payload = common;
        payload["final_reply_received"] = json!(false);
        payload["reason"] = json!("sent_without_waiting");
        return json_result(&payload);
    }

    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(300)
        .clamp(1, MAX_WAIT_SECS);
    match await_collaboration_reply(client, &origin, &sent.id, timeout_secs).await {
        Ok(Some(request)) => {
            let mut payload = common;
            payload
                .as_object_mut()
                .unwrap()
                .extend(request_outcome(&request).as_object().unwrap().clone());
            json_result(&payload)
        }
        Ok(None) => {
            let mut payload = common;
            payload["final_reply_received"] = json!(false);
            payload["reason"] = json!("timeout");
            payload["timeout_secs"] = json!(timeout_secs);
            payload["delivery_state"] =
                crate::collaboration_output::receipt(&sent)["delivery_state"].clone();
            payload["next_step"] = json!(format!(
                "the request remains durable for {}; call muxa_wait_reply with request_id {} to keep \
                 waiting, or muxa_peer_report later. Do not poll the pane.",
                sent.to.pane, sent.id
            ));
            json_result(&payload)
        }
        Err(error) => error_result(&format!("call_peer wait failed: {error}")),
    }
}

/// Durable sender receipt written before any remote effect. A restarted caller
/// can recover the deterministic request ID without inventing another task.
fn persist_peer_dispatch(
    id: &str,
    host: &str,
    node_id: &muxa::NodeId,
    pane: &muxa::PaneKey,
    request: &NewRequest,
) -> anyhow::Result<()> {
    use crate::owner_only::OwnerOnly;
    use std::io::Write;
    let root =
        muxa::paths::default_node_id_file().ok_or_else(|| anyhow::anyhow!("no data directory"))?;
    let dir = root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no data parent"))?
        .join("dispatches/peer-outbox");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{id}.json"));
    let mut contract = serde_json::to_value(request)?;
    // Agent state/cwd changes between retries are advisory, not a new task.
    contract.as_object_mut().unwrap().remove("initiator");
    let value = json!({"host":host,"node_id":node_id,"pane_key":pane,"request":contract,"request_id":format!("dispatch-{id}")});
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .owner_only(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(&serde_json::to_vec(&value)?)?;
            file.sync_all()?;
            std::fs::File::open(dir)?.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing: Value = serde_json::from_slice(&std::fs::read(path)?)?;
            if existing != value {
                anyhow::bail!("dispatch_id already journals a different target or payload");
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Durable colleague call across a named physical Fleet host.
///
/// Unlike the same-window call surface, every routing dimension is explicit:
/// no peer selection, remote pane creation, or terminal-output inference is
/// performed here. The target host's control mode remains the daemon's
/// authoritative authorization check on both send and reply retrieval.
#[allow(clippy::too_many_lines)] // keep remote authorization, send, and wait correlation auditable
async fn fleet_call_peer(
    client: &Client,
    args: &Value,
    message_skills: &BTreeMap<String, String>,
) -> Value {
    let Some(host) = args
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty())
    else {
        return error_result("fleet_call_peer requires a non-empty `host` argument");
    };
    let Some(pane) = args
        .get("pane")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|pane| !pane.is_empty())
    else {
        return error_result("fleet_call_peer requires a non-empty `pane` argument");
    };
    let (body, expanded_skill) = match peer_call_body(args, message_skills) {
        Ok(body) => body,
        Err(error) => return error_result(&error),
    };
    let (kind, work_mode) = match peer_call_contract(args) {
        Ok(contract) => contract,
        Err(error) => return error_result(error),
    };
    let air_artifacts = match parse_air_artifact_references(args) {
        Ok(references) => references,
        Err(error) => return error_result(&error),
    };
    let paths = string_array(args, "paths");
    let mut request =
        match new_request_from_args(args, kind, body, true, work_mode, paths, air_artifacts) {
            Ok(request) => request,
            Err(error) => return error_result(&error),
        };
    if !request.withdraw_reply_wakes.is_empty() {
        // The withdrawn requests live in this host's mailbox and the new one
        // in the remote host's; there is no transaction spanning both.
        return error_result(
            "fleet_call_peer cannot withdraw reply wakes; call muxa_withdraw_reply_wake on the host that holds those requests",
        );
    }

    if let Ok(origin) = resolve_collaboration_origin(client).await {
        if let Ok(room) = client.collaboration_context(&origin).await {
            request.initiator = Some(Box::new(room.current));
        }
    }
    if let Err(error) = ensure_fleet_collaboration_ready(client, host).await {
        return error_result(&format!("fleet_call_peer refused: {error}"));
    }
    let pane_key = match crate::fleet_cli::resolve_pane(client, host, pane).await {
        Ok(target) => target,
        Err(error) => return error_result(&format!("fleet_call_peer failed: {error}")),
    };
    let dispatch_id = match request.dispatch_id.as_deref() {
        Some(id) => match uuid::Uuid::parse_str(id) {
            Ok(id) => id.to_string(),
            Err(_) => return error_result("dispatch_id must be a UUID"),
        },
        None => uuid::Uuid::new_v4().to_string(),
    };
    request.dispatch_id = Some(dispatch_id.clone());
    let snapshot = match client.fleet_snapshot(None).await {
        Ok(snapshot) => snapshot,
        Err(error) => return error_result(&error.to_string()),
    };
    request.source_node_id = snapshot
        .hosts
        .iter()
        .find(|h| h.local)
        .and_then(|h| h.node_id.clone());
    if !snapshot.hosts.iter().any(|h| {
        h.alias == host
            && h.capabilities
                .iter()
                .any(|c| c == "collaboration_dispatch_v1")
    }) {
        return error_result("target must advertise collaboration_dispatch_v1; upgrade its muxa and muxad before durable dispatch");
    }
    let Some(node_id) = snapshot
        .hosts
        .iter()
        .find(|h| h.alias == host)
        .and_then(|h| h.node_id.as_ref())
    else {
        return error_result("target NodeId unavailable; refusing an unpinned peer dispatch");
    };
    if let Err(error) = persist_peer_dispatch(&dispatch_id, host, node_id, &pane_key, &request) {
        return error_result(&format!("cannot journal dispatch before sending: {error}"));
    }
    let result = match client
        .fleet_execute(
            host,
            &muxa::FleetOperation::CollaborationSend {
                pane: pane_key.clone(),
                request: Box::new(request),
            },
        )
        .await
    {
        Ok(result) => result,
        Err(error) => {
            return json_result(&json!({
                "sent": null, "final_reply_received": false, "reason": "delivery_unknown",
                "dispatch_id": dispatch_id, "request_id": format!("dispatch-{dispatch_id}"),
                "host": host, "pane_key": pane_key, "error": error.to_string(),
                "next_step": "Look up this exact request first. Retry only with the same dispatch_id and unchanged payload; never create a replacement automatically."
            }))
        }
    };
    let Some(sent) = result.collaboration_request.map(|request| *request) else {
        return error_result("fleet_call_peer send returned no collaboration request");
    };
    let common = json!({
        "dispatch_id": dispatch_id,
        "sent": true,
        "host": host,
        "pane_key": pane_key,
        "selected_peer": sent.to,
        "initiator": sent.initiator,
        "progress_route": {"tool":"muxa_update_request", "request_id":sent.id},
        "guidance_route": {"tool":"muxa_fleet_update_request", "host":host, "pane_key":pane_key, "request_id":sent.id},
        "expanded_skill": expanded_skill,
        "request_id": sent.id,
        "thread_id": sent.thread_id,
        "parent_request_id": sent.parent_request_id,
        "workspace_id": sent.workspace_id,
        "work_id": sent.work_id,
        "run_id": sent.run_id,
        "kind": kind,
        "work_mode": work_mode,
    });
    if !args.get("wait").and_then(Value::as_bool).unwrap_or(true) {
        let mut payload = common;
        payload["final_reply_received"] = json!(false);
        payload["reason"] = json!("sent_without_waiting");
        return json_result(&payload);
    }

    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(300)
        .clamp(1, MAX_WAIT_SECS);
    match await_fleet_collaboration_reply(client, host, &pane_key, &sent.id, timeout_secs, None)
        .await
    {
        Ok(Some(request)) if request.peer_interrupted() && !request.status.is_terminal() => {
            let mut payload = request_outcome(&request);
            payload["host"] = json!(host);
            payload["pane_key"] = json!(pane_key);
            json_result(&payload)
        }
        Ok(Some(request)) if request.status.is_terminal() => {
            let mut payload = common;
            payload
                .as_object_mut()
                .unwrap()
                .extend(request_outcome(&request).as_object().unwrap().clone());
            json_result(&payload)
        }
        Ok(current) => {
            let mut payload = common;
            payload["final_reply_received"] = json!(false);
            payload["reason"] = json!("timeout");
            payload["timeout_secs"] = json!(timeout_secs);
            payload["delivery_state"] =
                crate::collaboration_output::receipt(current.as_ref().unwrap_or(&sent))
                    ["delivery_state"]
                    .clone();
            payload["next_step"] = json!({
                "tool": "muxa_fleet_wait_reply",
                "arguments": {
                    "host": host,
                    "pane_key": pane_key,
                    "request_id": payload["request_id"],
                },
                "message": "The request remains durable on the target host; continue by exact request id. Do not poll terminal capture."
            });
            json_result(&payload)
        }
        Err(error) => {
            // Delivery already succeeded. Returning a tool error here would
            // discard the only correlation handle and tempt callers to send
            // duplicate work. Preserve the durable request and exact pane so
            // a later wait can resume after a transient relay failure.
            let mut payload = common;
            payload["final_reply_received"] = json!(false);
            payload["reason"] = json!("wait_failed");
            payload["error"] = json!(error);
            payload["delivery_state"] =
                crate::collaboration_output::receipt(&sent)["delivery_state"].clone();
            payload["next_step"] = json!({
                "tool": "muxa_fleet_wait_reply",
                "arguments": {
                    "host": host,
                    "pane_key": pane_key,
                    "request_id": payload["request_id"],
                },
                "message": "Delivery succeeded and remains durable; retry the exact reply wait. Do not resend the request or poll terminal capture."
            });
            json_result(&payload)
        }
    }
}

async fn fleet_wait_reply(client: &Client, args: &Value) -> Value {
    let Some(host) = args
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty())
    else {
        return error_result("fleet_wait_reply requires a non-empty `host` argument");
    };
    let Some(request_id) = args
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|request_id| !request_id.is_empty())
    else {
        return error_result("fleet_wait_reply requires a non-empty `request_id` argument");
    };
    let Some(pane_key_value) = args.get("pane_key") else {
        return error_result(
            "fleet_wait_reply requires the exact `pane_key` object returned by fleet_call_peer",
        );
    };
    let pane_key: muxa::PaneKey = match serde_json::from_value(pane_key_value.clone()) {
        Ok(pane_key) => pane_key,
        Err(error) => {
            return error_result(&format!(
                "fleet_wait_reply `pane_key` is not a valid exact PaneKey: {error}"
            ));
        }
    };
    if let Err(error) = ensure_fleet_collaboration_ready(client, host).await {
        return error_result(&format!("fleet_wait_reply refused: {error}"));
    }
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WAIT_SECS)
        .clamp(1, MAX_WAIT_SECS);
    match await_fleet_collaboration_reply(
        client,
        host,
        &pane_key,
        request_id,
        timeout_secs,
        args.get("after_update").and_then(Value::as_u64),
    )
    .await
    {
        Ok(Some(request)) if request.peer_interrupted() && !request.status.is_terminal() => {
            let mut payload = request_outcome(&request);
            payload["host"] = json!(host);
            payload["pane_key"] = json!(pane_key);
            json_result(&payload)
        }
        Ok(Some(request)) if request.status.is_terminal() => {
            let mut result = request_outcome(&request);
            result["host"] = json!(host);
            result["pane_key"] = json!(pane_key);
            result["updates"] = crate::collaboration_output::updates(
                &request,
                args.get("after_update")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
            result["next_after_update"] = json!(request.updates.last().map(|u| u.sequence));
            json_result(&result)
        }
        Ok(Some(request))
            if args
                .get("after_update")
                .and_then(Value::as_u64)
                .is_some_and(|seen| request.updates.last().is_some_and(|u| u.sequence > seen)) =>
        {
            json_result(&json!({
                "final_reply_received":false, "reason":"updated", "host":host, "pane_key":pane_key,
                "request_id":request.id, "delivery_state":crate::collaboration_output::receipt(&request)["delivery_state"], "updates":crate::collaboration_output::updates(&request, args.get("after_update").and_then(Value::as_u64).unwrap_or(0)),
                "first_retained_sequence":request.updates.first().map(|u| u.sequence),
                "next_after_update":request.updates.last().map(|u| u.sequence),
            }))
        }
        Ok(current) => json_result(&json!({
            "final_reply_received": false,
            "reason": "timeout",
            "timeout_secs": timeout_secs,
            "host": host,
            "pane_key": pane_key,
            "request_id": request_id,
            "delivery_state": current.as_ref().map(|request| crate::collaboration_output::receipt(request)["delivery_state"].clone()),
        })),
        Err(error) => error_result(&format!("fleet_wait_reply failed: {error}")),
    }
}

async fn ensure_fleet_collaboration_ready(
    client: &Client,
    host_alias: &str,
) -> std::result::Result<(), String> {
    let snapshot = client
        .fleet_snapshot(None)
        .await
        .map_err(|error| format!("could not read Fleet status: {error}"))?;
    let host = snapshot
        .hosts
        .into_iter()
        .find(|host| host.alias == host_alias)
        .ok_or_else(|| format!("Fleet host '{host_alias}' is not known"))?;
    validate_fleet_collaboration_authority(host_alias, host.local, host.mode, &host.capabilities)
}

fn validate_fleet_collaboration_authority(
    host_alias: &str,
    local: bool,
    mode: muxa::HostAccessMode,
    capabilities: &[String],
) -> std::result::Result<(), String> {
    if !local && mode != muxa::HostAccessMode::Control {
        return Err(format!(
            "host '{host_alias}' is observe-only; explicitly grant mode='control' before an agent may call it"
        ));
    }
    for capability in ["collaboration", "collaboration_get"] {
        if !capabilities.iter().any(|candidate| candidate == capability) {
            return Err(format!(
                "host '{host_alias}' does not advertise '{capability}'; connect it and upgrade muxa on that host"
            ));
        }
    }
    Ok(())
}

async fn await_fleet_collaboration_reply(
    client: &Client,
    host: &str,
    pane: &muxa::PaneKey,
    request_id: &str,
    timeout_secs: u64,
    after_update: Option<u64>,
) -> std::result::Result<Option<CollaborationRequest>, String> {
    let hello = client
        .hello(Duration::from_secs(3))
        .await
        .map_err(|e| e.to_string())?;
    if hello.capabilities.iter().any(|c| c == "fleet_wait_reply") {
        return client
            .fleet_wait_reply(host, pane, request_id, timeout_secs, after_update)
            .await
            .map_err(|e| e.to_string());
    }
    let mut poll_interval = FLEET_REPLY_POLL_INTERVAL;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut current = None;
    loop {
        let operation = muxa::FleetOperation::CollaborationGet {
            pane: pane.clone(),
            request_id: request_id.to_string(),
        };
        let result =
            match tokio::time::timeout_at(deadline, client.fleet_execute(host, &operation)).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => return Err(error.to_string()),
                Err(_) => return Ok(current),
            };
        let request = *result
            .collaboration_request
            .ok_or_else(|| "Fleet collaboration get returned no request".to_string())?;
        let terminal = request.status.is_terminal() || request.peer_interrupted();
        current = Some(request);
        let updated = after_update.is_some_and(|seen| {
            current
                .as_ref()
                .is_some_and(|r| r.updates.last().is_some_and(|u| u.sequence > seen))
        });
        if terminal || updated || tokio::time::Instant::now() >= deadline {
            return Ok(current);
        }
        tokio::time::sleep_until(std::cmp::min(
            deadline,
            tokio::time::Instant::now() + poll_interval,
        ))
        .await;
        poll_interval = (poll_interval * 2).min(Duration::from_secs(30));
    }
}

fn peer_call_contract(args: &Value) -> std::result::Result<(RequestKind, WorkMode), &'static str> {
    let kind = match args
        .get("intent")
        .and_then(Value::as_str)
        .unwrap_or("review")
    {
        "review" => RequestKind::Review,
        "question" => RequestKind::Question,
        "task" => RequestKind::Task,
        _ => return Err("call_peer intent must be review, question, or task"),
    };
    let execute = args
        .get("execute")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if execute && kind != RequestKind::Task {
        return Err("call_peer execute=true requires intent=task");
    }
    Ok((
        kind,
        if execute {
            WorkMode::Execute
        } else {
            WorkMode::ReadOnly
        },
    ))
}

/// `muxa work up` over MCP.
///
/// Resolution is async (it may spend a headless agent turn looking the
/// ticket up); everything after it shells out to tmux and is therefore
/// handed to `spawn_blocking` rather than run on the reactor.
async fn start_work(client: &Client, args: &Value, config: &muxa::config::Config) -> Value {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
    let Some(work) = text("work") else {
        return error_result("muxa_start_work requires a work argument");
    };
    let up = crate::work_up::UpArgs {
        work,
        external: text("external"),
        pipeline: text("pipeline"),
        workspace: text("workspace"),
        cwd: text("cwd").map(std::path::PathBuf::from),
        body: text("body"),
        skill: text("skill"),
        context: text("context"),
        dry_run: flag("dry_run"),
        show_prompts: false,
        yes: true,
        no_ticket: flag("no_ticket"),
        refresh: flag("refresh"),
        json: true,
    };
    let resolved = match crate::work_up::resolve(&up, config, Some(client)).await {
        Ok(resolved) => resolved,
        Err(error) => return error_result(&format!("{error:#}")),
    };
    let (endpoint, _) = match lifecycle_target(client, None).await {
        Ok(value) => value,
        Err(error) => return error_result(&format!("muxa_start_work failed: {error}")),
    };
    let result = if up.dry_run {
        match tokio::task::spawn_blocking(move || {
            crate::mux_control::with_endpoint(endpoint, || crate::work_up::apply(resolved, true))
        })
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return error_result(&format!("{error:#}")),
            Err(error) => return error_result(&format!("muxa_start_work worker failed: {error}")),
        }
    } else {
        let runtime = tokio::runtime::Handle::current();
        let client = client.clone();
        match tokio::task::spawn_blocking(move || {
            crate::mux_control::with_endpoint(endpoint, || {
                runtime.block_on(crate::work_up::apply_durable(resolved, &client))
            })
        })
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return error_result(&format!("{error:#}")),
            Err(error) => return error_result(&format!("muxa_start_work worker failed: {error}")),
        }
    };
    match serde_json::to_value(&result) {
        Ok(value) => json_result(&value),
        Err(error) => error_result(&format!("muxa_start_work could not serialize: {error}")),
    }
}

/// Adapter over [`muxa::request::compose`]: read the JSON-RPC argument
/// shape and turn "nothing supplied" into the error a peer call needs.
/// `muxa work up` shares the composer but not that rule — a pipeline can
/// legitimately be driven by its ticket alone.
fn peer_call_body(
    args: &Value,
    message_skills: &BTreeMap<String, String>,
) -> std::result::Result<(String, Option<String>), String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let composed = muxa::request::compose(
        muxa::request::RequestParts {
            skill: text("skill"),
            body: text("body"),
            context: text("context"),
        },
        message_skills,
    )
    .map_err(|error| error.to_string())?
    .ok_or("call_peer requires body, skill, or context")?;
    Ok((composed.text, composed.skill))
}

fn select_peer(
    room: &RoomContext,
    target: &str,
) -> std::result::Result<Option<PeerSelection>, String> {
    let target = target.trim();
    let live = room
        .peers
        .iter()
        .filter(|peer| peer_is_eligible(peer))
        .collect::<Vec<_>>();
    let workers: Vec<_> = live
        .iter()
        .copied()
        .filter(|peer| peer.accepts_peer_work())
        .collect();
    if target.is_empty()
        || target.eq_ignore_ascii_case("auto")
        || target.eq_ignore_ascii_case("@peer")
        || target.eq_ignore_ascii_case("@muxa-peer")
        || target.eq_ignore_ascii_case("peer")
    {
        return Ok(best_peer(&workers, &room.current).map(|peer| {
            PeerSelection::registered(
                peer.clone(),
                "auto: healthy different-provider preference, then state and numeric pane order"
                    .into(),
            )
        }));
    }
    let pane_target = target.strip_prefix("pane:").unwrap_or(target);
    let pane_target = if pane_target.starts_with('%') && room.current.pane.starts_with("rmux:") {
        format!("rmux:{pane_target}")
    } else {
        pane_target.to_owned()
    };
    if let Some(peer) = live.iter().copied().find(|peer| peer.pane == pane_target) {
        return Ok(Some(PeerSelection::registered(
            peer.clone(),
            format!("explicit pane {}", peer.pane),
        )));
    }
    if is_pane_selector(&pane_target) {
        // An explicitly addressed pane that hosts no registered peer is not an
        // error here: muxad resolves a muxa-launched pane whose agent has not
        // registered yet, and refuses anything else. Deciding that locally
        // would duplicate — and could contradict — the authoritative guard.
        return Ok(Some(PeerSelection::pending(
            pane_target.clone(),
            format!("explicit pane {pane_target}, pending its agent's registration"),
        )));
    }
    if let Some(kind) = provider_kind(target) {
        let candidates = workers
            .iter()
            .copied()
            .filter(|peer| peer.agent_kind == kind)
            .collect::<Vec<_>>();
        return Ok(best_peer(&candidates, &room.current).map(|peer| {
            PeerSelection::registered(peer.clone(), format!("explicit provider {kind}"))
        }));
    }
    if let Some(role) = target.strip_prefix("role:") {
        let matches = workers
            .iter()
            .copied()
            .filter(|peer| {
                peer.roles
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(role))
            })
            .collect::<Vec<_>>();
        return match matches.as_slice() {
            [] => Ok(None),
            [peer] => Ok(Some(PeerSelection::registered(
                (*peer).clone(),
                format!("explicit role {role}"),
            ))),
            _ => Err(format!(
                "role {role:?} matches multiple peers; use @alias or pane id"
            )),
        };
    }
    let alias = target
        .strip_prefix('@')
        .or_else(|| target.strip_prefix("alias:"))
        .unwrap_or(target);
    let matches = live
        .iter()
        .copied()
        .filter(|peer| {
            peer.alias
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(alias))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(format!(
            "no eligible peer matches {target:?}; use auto, a provider, @alias, role:<name>, or pane id"
        )),
        [peer] => Ok(Some(PeerSelection::registered(
            (*peer).clone(),
            format!("explicit alias @{alias}"),
        ))),
        _ => Err(format!(
            "alias @{alias} matches multiple peers; use an exact pane id"
        )),
    }
}

/// Does this selector name a concrete pane? `%N` for tmux/zellij, and the
/// namespaced ids other hosts use (`herdr:…`, `cmux:…`, `rmux:%N`).
fn is_pane_selector(target: &str) -> bool {
    target.starts_with('%') || muxa::backend::pane_id_host_kind(target).is_some()
}

fn peer_is_eligible(peer: &Participant) -> bool {
    if peer.draining {
        return false;
    }
    !matches!(peer.agent_kind, AgentKind::Task | AgentKind::Unknown)
        && !matches!(peer.state, AgentState::Error | AgentState::Stopped)
}

fn best_peer<'a>(peers: &[&'a Participant], current: &Participant) -> Option<&'a Participant> {
    peers.iter().copied().min_by(|left, right| {
        peer_sort_key(left, current)
            .cmp(&peer_sort_key(right, current))
            .then_with(|| left.pane.cmp(&right.pane))
    })
}

fn peer_sort_key(peer: &Participant, current: &Participant) -> (bool, u8, u8, u64) {
    (
        peer.agent_kind == current.agent_kind,
        peer_state_rank(peer.state),
        provider_rank(peer.agent_kind),
        numeric_pane_index(&peer.pane).unwrap_or(u64::MAX),
    )
}

fn peer_state_rank(state: AgentState) -> u8 {
    match state {
        AgentState::Idle => 0,
        AgentState::Working => 1,
        AgentState::Starting => 2,
        AgentState::WaitingInput | AgentState::WaitingChoice => 3,
        AgentState::Error => 4,
        AgentState::Stopped => 5,
    }
}

fn provider_rank(kind: AgentKind) -> u8 {
    match kind {
        AgentKind::Codex => 0,
        AgentKind::ClaudeCode => 1,
        AgentKind::GeminiCli => 2,
        AgentKind::Antigravity => 3,
        AgentKind::Opencode => 4,
        AgentKind::Task => 5,
        AgentKind::Unknown => 6,
    }
}

fn numeric_pane_index(pane: &str) -> Option<u64> {
    pane.rsplit_once('%')?.1.parse().ok()
}

fn provider_kind(target: &str) -> Option<AgentKind> {
    match target
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase()
        .as_str()
    {
        "claude" | "claude_code" | "claude-code" => Some(AgentKind::ClaudeCode),
        "codex" | "cx" => Some(AgentKind::Codex),
        "gemini" | "gemini_cli" | "gemini-cli" => Some(AgentKind::GeminiCli),
        "agy" | "antigravity" => Some(AgentKind::Antigravity),
        "opencode" => Some(AgentKind::Opencode),
        _ => None,
    }
}

/// Review routing defaults to the other provider; explicit routing remains authoritative.
fn review_provider_target(target: &str, kind: RequestKind, caller: AgentKind) -> &str {
    let automatic = matches!(
        target.trim().to_ascii_lowercase().as_str(),
        "" | "auto" | "peer" | "@peer" | "@muxa-peer"
    );
    if kind == RequestKind::Review && automatic {
        match caller {
            AgentKind::Codex => "claude",
            AgentKind::ClaudeCode => "codex",
            _ => target,
        }
    } else {
        target
    }
}

fn peer_spawn_program(
    args: &Value,
    current: &Participant,
    target: &str,
    guide: &muxa::config::McpGuideConfig,
) -> std::result::Result<crate::agent_launch::AgentProgram, String> {
    let requested = args
        .get("spawn_agent")
        .and_then(Value::as_str)
        .map(crate::agent_launch::AgentProgram::parse)
        .transpose()?;
    let targeted = provider_kind(target).map(agent_program_for_kind);
    let configured = guide
        .agent
        .as_deref()
        .map(crate::agent_launch::AgentProgram::parse)
        .transpose()?;
    if let (Some(requested), Some(targeted)) = (requested, targeted) {
        if requested != targeted {
            return Err(format!(
                "spawn_agent={} conflicts with target={target:?}",
                requested.label()
            ));
        }
    }
    // Enumerated rather than collapsed to a `_` arm, and the duplicate
    // `Codex` bodies are deliberate: spelling out every kind is what makes a
    // new `AgentKind` a compile error here instead of a silent default.
    #[allow(clippy::match_same_arms)]
    let fallback = match current.agent_kind {
        AgentKind::ClaudeCode => crate::agent_launch::AgentProgram::Codex,
        AgentKind::Codex => crate::agent_launch::AgentProgram::Claude,
        AgentKind::GeminiCli
        | AgentKind::Antigravity
        | AgentKind::Opencode
        | AgentKind::Task
        | AgentKind::Unknown => crate::agent_launch::AgentProgram::Codex,
    };
    Ok(requested.or(targeted).or(configured).unwrap_or(fallback))
}

fn agent_program_for_kind(kind: AgentKind) -> crate::agent_launch::AgentProgram {
    match kind {
        AgentKind::ClaudeCode => crate::agent_launch::AgentProgram::Claude,
        AgentKind::Codex | AgentKind::Task | AgentKind::Unknown => {
            crate::agent_launch::AgentProgram::Codex
        }
        AgentKind::GeminiCli => crate::agent_launch::AgentProgram::Gemini,
        AgentKind::Antigravity => crate::agent_launch::AgentProgram::Antigravity,
        AgentKind::Opencode => crate::agent_launch::AgentProgram::Opencode,
    }
}

/// The provider arguments an MCP launch should carry.
///
/// A caller-supplied `options` array wins; otherwise the daemon's own
/// configuration answers through [`muxa::config::Config::launch_options`],
/// which is keyed by provider — so this no longer has to guard against
/// handing one CLI's model name to another.
fn launch_options(
    args: &Value,
    config: &muxa::config::Config,
    program: crate::agent_launch::AgentProgram,
) -> std::result::Result<Vec<String>, String> {
    let options = match args.get("options") {
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("options[{index}] must be a string"))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
        Some(_) => return Err("options must be an array of strings".into()),
        None => config.launch_options(program.label(), None),
    };
    if let Some((index, _)) = options
        .iter()
        .enumerate()
        .find(|(_, option)| option.contains('\0'))
    {
        return Err(format!("options[{index}] must not contain a NUL byte"));
    }
    Ok(options)
}

/// The role a peer spawned for `target` takes: the one it names, or the
/// call's intent.
fn spawned_peer_role<'a>(target: &'a str, args: &Value) -> &'a str {
    target.strip_prefix("role:").unwrap_or_else(|| {
        if args.get("intent").and_then(Value::as_str) == Some("task") {
            "implementer"
        } else {
            "reviewer"
        }
    })
}

async fn acquire_peer_capacity(
    client: &Client,
    origin: &CollaborationOrigin,
    args: &Value,
    target: &str,
    program: crate::agent_launch::AgentProgram,
) -> std::result::Result<muxa::collaboration::WorkerSpawnAcquisition, String> {
    let hello = client
        .hello(Duration::from_secs(3))
        .await
        .map_err(|e| e.to_string())?;
    if !hello
        .capabilities
        .iter()
        .any(|cap| cap == muxa::collaboration::WORKER_SPAWN_CAPABILITY)
    {
        return Err("automatic worker creation requires muxad with collaboration_worker_spawn_v1; upgrade/restart muxad and run `muxa doctor` to check daemon and MCP version skew".into());
    }
    let spec = muxa::collaboration::WorkerSpawnSpec {
        agent_kind: muxa::discovery::classify_command(program.label())
            .ok_or("unknown worker provider")?,
        role: target.strip_prefix("role:").map(str::to_owned),
        launch_role: spawned_peer_role(target, args).into(),
    };
    let timeout = args
        .get("spawn_timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_SPAWN_GRACE_SECS)
        .clamp(1, 60);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    // Subscribe before acquisition, so a fast creator's Observe is not lost.
    let mut updates = client
        .collaboration_subscribe()
        .await
        .map_err(|e| e.to_string())?;
    loop {
        let acquisition = client
            .collaboration_worker_acquire(origin, &spec)
            .await
            .map_err(|e| e.to_string())?;
        if !matches!(&acquisition, muxa::collaboration::WorkerSpawnAcquisition::Pending { peer: None, phase, .. } if *phase != muxa::collaboration::WorkerSpawnPhase::Uncertain)
            || tokio::time::Instant::now() >= deadline
        {
            return Ok(acquisition);
        }
        // Periodic reconciliation covers a launcher that died without publishing
        // a change. Expired Launching leases still cannot create another token.
        let wake = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + Duration::from_secs(5),
        );
        match tokio::time::timeout_at(wake, updates.recv()).await {
            Ok(Ok(Some(_))) | Err(_) => {}
            Ok(Ok(None) | Err(_)) => return Ok(acquisition),
        }
    }
}

async fn spawn_peer(
    client: &Client,
    room: &RoomContext,
    args: &Value,
    target: &str,
    program: crate::agent_launch::AgentProgram,
    config: &muxa::config::Config,
    ticket: muxa::collaboration::WorkerSpawnTicket,
) -> std::result::Result<(PeerSelection, crate::agent_launch::StartResult), String> {
    let host = muxa::backend::pane_id_host_kind(&room.current.pane)
        .filter(|host| crate::mux_control::supported(*host))
        .ok_or("call_peer automatic spawn requires a tmux or rmux agent pane")?;
    let endpoint =
        select_lifecycle_endpoint(host, room.current.socket.clone().into_iter().collect())?;
    let native_pane = room
        .current
        .pane
        .strip_prefix("rmux:")
        .unwrap_or(&room.current.pane);
    let request = crate::agent_launch::StartRequest {
        creator: Some((&room.current).into()),
        worker_spawn: Some(ticket.clone()),
        bypass_permissions: config.collaboration.worker_bypass_permissions,
        purpose: muxa::collaboration::LaunchPurpose::Worker,
        socket: client.socket().to_path_buf(),
        agent: program,
        options: config.launch_options(program.label(), None),
        placement: crate::agent_launch::Placement::Pane,
        target: Some(native_pane.to_string()),
        cwd: room.current.cwd.as_deref().map(std::path::PathBuf::from),
        prompt: None,
        name: None,
        workspace: None,
        work: None,
        role: Some(spawned_peer_role(target, args).into()),
        task: Some("muxa peer call".into()),
        // A peer spawned for one call is not a pipeline pane, so it stays
        // unaliased and `muxa work up` reports it as unclaimed rather than
        // mistaking it for a role it should own.
        alias: None,
        generation: None,
        direction: crate::agent_launch::SplitDirection::parse(Some(
            config.mcp.guide.direction.as_str(),
        ))
        .expect("MCP guide direction has the same closed value set"),
        account: config.account_route(program.label()),
    };
    // Arm the daemon transition subscription before creating the pane. A
    // Started hook can register very quickly; subscribing first ensures the
    // signal is either in this stream or represented by the authoritative
    // context read after spawn, with no polling race between the two.
    let registrations = client
        .subscribe()
        .await
        .map_err(|error| format!("call_peer could not watch peer registration: {error}"))?;
    let mut launch = tokio::task::spawn_blocking(move || {
        crate::mux_control::with_endpoint(Some(endpoint), || crate::agent_launch::start(request))
    });
    let mut renewal = tokio::time::interval(Duration::from_secs(5));
    renewal.tick().await;
    let mut started = loop {
        tokio::select! {
            result = &mut launch => break result
                .map_err(|error| format!("call_peer spawn worker failed: {error}"))?
                .map_err(|error| format!("call_peer spawn failed: {error}"))?,
            _ = renewal.tick() => {
                // Failure never authorizes takeover. Begin is fenced, and a
                // launch already begun remains reserved even after expiry.
                let _ = client.collaboration_worker_spawn(&ticket, &muxa::collaboration::WorkerSpawnAction::Renew).await;
            }
        }
    };
    if host == muxa::HostKind::Rmux {
        started.pane = format!("rmux:{}", started.pane);
    }
    let timeout_secs = args
        .get("spawn_timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_SPAWN_GRACE_SECS)
        .clamp(1, 60);
    let peer = wait_for_spawned_peer_registration(
        client,
        &ticket.origin,
        &started.pane,
        timeout_secs,
        registrations,
    )
    .await?;
    if peer.is_none() && !has_pane_readiness_signal(program) {
        // Delivery to an unregistered pane needs a row that reports `Idle`,
        // which means discovery must classify the process or a screen manifest
        // must cover it. Neither holds for opencode today, so queueing against
        // the pane would promise a delivery that can never happen — fail fast
        // with something the caller can act on instead.
        return Err(format!(
            "created {} peer pane {} but it did not register within {timeout_secs}s, and muxa has no readiness signal for {} panes; keep the pane, submit one prompt in it so its hooks fire, then call again with target=\"pane:{}\"",
            program.label(),
            started.pane,
            program.label(),
            started.pane
        ));
    }
    // Registration is not a precondition for sending. An agent that registers
    // on boot (claude) lands in the grace window above; codex creates its
    // session — and so fires its first hook — only when a prompt arrives, so
    // waiting for it here would deadlock against the very request we are about
    // to send. Address the pane instead: muxad queues the request and delivers
    // it when the pane reads idle.
    let selection = match peer {
        Some(peer) => PeerSelection::registered(
            peer,
            format!(
                "spawned {} in {} after explicit confirmation for target {target:?}",
                program.label(),
                started.pane
            ),
        ),
        None => PeerSelection::pending(
            started.pane.clone(),
            format!(
                "spawned {} in {}; queued for the pane, which has not registered a session yet",
                program.label(),
                started.pane
            ),
        ),
    };
    Ok((selection, started))
}

/// Can muxad tell that a pane of this program is ready for input before its
/// agent registers?
///
/// That requires the pane to carry a registry row, which comes from discovery
/// classifying the process (`discovery::classify_command`) or from a bundled
/// screen manifest. Opencode has neither today — `DiscoveryReport::bump`
/// documents that it is never synthesized — so a request queued against an
/// unregistered opencode pane would wait forever.
fn has_pane_readiness_signal(program: crate::agent_launch::AgentProgram) -> bool {
    use crate::agent_launch::AgentProgram;
    match program {
        AgentProgram::Claude
        | AgentProgram::Codex
        | AgentProgram::Gemini
        | AgentProgram::Antigravity => true,
        AgentProgram::Opencode => false,
    }
}

async fn wait_for_spawned_peer_registration(
    client: &Client,
    origin: &CollaborationOrigin,
    pane: &str,
    timeout_secs: u64,
    mut registrations: TransitionStream,
) -> std::result::Result<Option<Participant>, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    if let Some(peer) = registered_peer(client, origin, pane).await? {
        return Ok(Some(peer));
    }
    loop {
        match tokio::time::timeout_at(deadline, registrations.recv()).await {
            Ok(Ok(Some(transition))) => {
                if transition.agent.pane.as_deref() == Some(pane) {
                    if let Some(peer) = registered_peer(client, origin, pane).await? {
                        return Ok(Some(peer));
                    }
                }
            }
            Ok(Ok(None) | Err(_)) => {
                // Re-arm before the authoritative read so a registration
                // racing stream recovery cannot be lost.
                registrations = match tokio::time::timeout_at(deadline, client.subscribe()).await {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(error)) => {
                        return Err(format!(
                            "call_peer registration stream could not reconnect: {error}"
                        ));
                    }
                    Err(_) => return registered_peer(client, origin, pane).await,
                };
                if let Some(peer) = registered_peer(client, origin, pane).await? {
                    return Ok(Some(peer));
                }
            }
            Err(_) => return registered_peer(client, origin, pane).await,
        }
    }
}

async fn registered_peer(
    client: &Client,
    origin: &CollaborationOrigin,
    pane: &str,
) -> std::result::Result<Option<Participant>, String> {
    let room = client
        .collaboration_context(origin)
        .await
        .map_err(|error| format!("call_peer context after spawn failed: {error}"))?;
    Ok(room
        .peers
        .into_iter()
        .find(|peer| peer.pane == pane && peer_is_eligible(peer)))
}

async fn await_collaboration_reply(
    client: &Client,
    origin: &CollaborationOrigin,
    request_id: &str,
    timeout_secs: u64,
) -> std::result::Result<Option<CollaborationRequest>, String> {
    let request = client
        .collaboration_wait_view(
            origin,
            request_id,
            timeout_secs,
            muxa::collaboration::CollaborationReadView::Outcome,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok((request.status.is_terminal() || request.peer_interrupted()).then_some(request))
}

tokio::task_local! {
    /// Agent session id the MCP host stamped on the current `tools/call`.
    static CALLER_SESSION: Option<String>;
}

/// The calling thread's session id from a `tools/call`'s `_meta`. Codex sends
/// `threadId`/`sessionId` (and the same id inside `x-codex-turn-metadata`) on
/// every call; other hosts send nothing and keep the pane-env identity.
fn caller_session_hint(params: Option<&Value>) -> Option<String> {
    let meta = params?.get("_meta")?;
    [
        meta.get("threadId"),
        meta.get("sessionId"),
        meta.pointer("/x-codex-turn-metadata/session_id"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .map(str::trim)
    .filter(|id| !id.is_empty())
    .map(str::to_owned)
}

/// Whether this process inherited its pane env from a shared Codex
/// app-server. Ancestry is fixed for the life of the process, so walk once.
pub(crate) fn pane_env_is_borrowed() -> bool {
    static SHARED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SHARED.get_or_init(|| {
        muxa::adapters::proc_ancestry::under_shared_codex_app_server(std::process::id())
    })
}

/// How long a session-id resolution is reused. Short enough that a binding
/// the reconciler repairs (a pane claimed before the fix, a late cwd match)
/// takes effect promptly; long enough that a burst of calls costs one lookup.
const SESSION_ORIGIN_TTL: Duration = Duration::from_secs(30);

/// The origin this MCP call or CLI invocation speaks for.
///
/// Normally the pane env names the caller ([`current_collaboration_origin`]).
/// Under a shared Codex app-server it names whichever pane started that
/// server, so the caller is identified by its Codex session id instead — the
/// MCP `_meta` thread id, or `$CODEX_THREAD_ID` for a shell command — and
/// mapped to the pane the registry bound that session to. A wrong identity is
/// worse than none: when the session is unknown or not yet bound to a pane,
/// this fails rather than borrowing the server's pane.
pub(crate) async fn resolve_collaboration_origin(
    client: &Client,
) -> std::result::Result<CollaborationOrigin, String> {
    let session = CALLER_SESSION
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .or_else(|| {
            std::env::var("CODEX_THREAD_ID")
                .ok()
                .filter(|id| !id.trim().is_empty())
        });
    if !pane_env_is_borrowed() {
        // The pane env is trustworthy, but a pane can still carry more than
        // one registry row; the session id lets the daemon pick the caller's.
        let mut origin = current_collaboration_origin()?;
        origin.agent_session_id = session;
        return Ok(origin);
    }
    let session = session.ok_or_else(|| {
        "collaboration cannot identify this Codex thread: it runs under the shared \
             Codex app-server, whose pane variables belong to another pane, and the \
             call carried no thread id. Upgrade Codex, or start it with `codex --no-daemon`"
            .to_string()
    })?;
    session_origin(client, &session).await
}

async fn session_origin(
    client: &Client,
    session: &str,
) -> std::result::Result<CollaborationOrigin, String> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, (std::time::Instant, CollaborationOrigin)>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, origin)) = cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(session).cloned())
    {
        if at.elapsed() < SESSION_ORIGIN_TTL {
            return Ok(origin);
        }
    }
    let agents = client
        .snapshot()
        .await
        .map_err(|error| format!("collaboration origin lookup failed: {error}"))?;
    let origin = origin_for_session(&agents, session)?;
    // An unbound origin is resolved by the daemon, which binds the session on
    // demand; ask it again next time instead of caching the empty pane.
    if !origin.pane.is_empty() {
        if let Ok(mut cache) = cache.lock() {
            cache.insert(
                session.to_owned(),
                (std::time::Instant::now(), origin.clone()),
            );
        }
    }
    Ok(origin)
}

fn origin_for_session(
    agents: &[Agent],
    session: &str,
) -> std::result::Result<CollaborationOrigin, String> {
    let agent = agents
        .iter()
        .find(|agent| agent.session_id == session)
        .ok_or_else(|| {
            format!(
                "collaboration origin: Codex thread {session} is not tracked yet; \
                 submit a prompt so its hooks register it, then retry"
            )
        })?;
    // Not bound yet: send the session alone. The daemon binds it from live
    // process evidence before resolving; an empty pane can never fall back
    // onto another agent's pane.
    let pane = agent.pane.clone().unwrap_or_default();
    Ok(CollaborationOrigin {
        pane,
        socket: agent.tmux_socket.clone(),
        console: false,
        agent_session_id: Some(session.to_owned()),
    })
}

pub(crate) fn current_collaboration_origin() -> std::result::Result<CollaborationOrigin, String> {
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
        Some(muxa::HostKind::Cmux) => Some(muxa::backend::cmux::endpoint_from_env()),
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
        agent_session_id: None,
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

/// Construct the daemon request only after extracting the optional causal and
/// Work identity carried by both MCP send surfaces. Keeping this seam shared
/// prevents `muxa_call_peer` and `muxa_send_message` from silently diverging.
fn new_request_from_args(
    args: &Value,
    kind: RequestKind,
    body: String,
    expects_reply: bool,
    work_mode: WorkMode,
    paths: Vec<String>,
    air_artifacts: Vec<AirArtifactReference>,
) -> std::result::Result<NewRequest, String> {
    Ok(NewRequest {
        dispatch_id: optional_string(args, "dispatch_id")?,
        source_node_id: None,
        delegation_parent: args
            .get("delegation_parent")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|e| format!("invalid delegation_parent: {e}"))?,
        human_action: args
            .get("human_action")
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|error| format!("invalid human_action: {error}"))
            })
            .transpose()?,
        initiator: None,
        kind,
        body,
        expects_reply,
        notify: match args.get("notify") {
            None => None,
            Some(Value::Bool(notify)) => Some(*notify),
            Some(_) => return Err("notify must be a boolean".into()),
        },
        work_mode,
        thread_id: optional_string(args, "thread_id")?,
        parent_request_id: optional_string(args, "parent_request_id")?,
        workspace_id: optional_string(args, "workspace_id")?,
        work_id: optional_string(args, "work_id")?,
        run_id: optional_string(args, "run_id")?,
        paths,
        artifacts: string_array(args, "artifacts"),
        links: string_array(args, "links"),
        air_artifacts,
        withdraw_reply_wakes: request_id_array(args, "withdraw_reply_wakes")?.unwrap_or_default(),
        recipient_slot: None,
    })
}

/// A strictly typed list of request ids. Unlike [`string_array`], a wrong
/// type, a non-string member or a blank id is an error: these lists drive
/// all-or-nothing mutations, which must not quietly act on a subset.
fn request_id_array(args: &Value, key: &str) -> std::result::Result<Option<Vec<String>>, String> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(format!("{key} must be an array of request ids"));
    };
    items
        .iter()
        .map(|item| match item.as_str() {
            Some(id) if !id.trim().is_empty() => Ok(id.to_string()),
            _ => Err(format!(
                "{key} must contain only non-blank request id strings"
            )),
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map(Some)
}

fn optional_string(args: &Value, key: &str) -> std::result::Result<Option<String>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
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
        .clamp(1, MAX_WAIT_SECS);
    let origin = match resolve_collaboration_origin(client).await {
        Ok(origin) => origin,
        Err(error) => return error_result(&error),
    };
    match client
        .collaboration_wait_view(
            &origin,
            request_id,
            timeout_secs,
            if args.get("full").and_then(Value::as_bool) == Some(true) {
                muxa::collaboration::CollaborationReadView::Full
            } else {
                muxa::collaboration::CollaborationReadView::Outcome
            },
        )
        .await
    {
        Ok(request) if request.status.is_terminal() || request.peer_interrupted() => {
            json_result(&request_outcome(&request))
        }
        Ok(request) => {
            let mut value = request_outcome(&request);
            value["reason"] = json!("timeout");
            json_result(&value)
        }
        Err(error) => error_result(&format!("wait_reply failed: {error}")),
    }
}

/// Report Enter transport acceptance separately from application acknowledgement.
/// Legacy daemons and missing hooks leave application submission unknown.
fn render_send_result(
    text_len: usize,
    pane: &str,
    submit: bool,
    submitted: Option<bool>,
    submission_confirmed: Option<bool>,
) -> Value {
    match submitted {
        Some(false) if submit => text_result(&format!(
            "sent {text_len} chars to {pane}, but the line was NOT submitted \
             (Enter not delivered) — the text is typed but not yet committed",
        )),
        _ => {
            let enter_sent = submitted.unwrap_or(false);
            text_result(&format!(
                "sent {text_len} chars to {pane}{}",
                if submission_confirmed == Some(true) {
                    " and submission confirmed"
                } else if enter_sent {
                    "; Enter sent, application submission unconfirmed (do not resend text)"
                } else if submit {
                    "; submission status unavailable (do not resend text)"
                } else {
                    ""
                },
            ))
        }
    }
}

/// Prefer a direct cmux socket call from the MCP process when it is itself
/// running inside cmux. cmux's default socket policy accepts descendants of a
/// cmux terminal but rejects a separately launched login daemon, so proxying
/// every request through muxad would make the default secure mode unusable.
/// A failed first text injection is safe to retry through the daemon; once text
/// lands, submit is reported separately and the text is never resent.
async fn send_prompt_with_local_cmux(
    client: &Client,
    pane: &str,
    prompt: &str,
    submit: bool,
) -> std::result::Result<SendPromptOutcome, String> {
    let inside_cmux = std::env::var("CMUX_SURFACE_ID").is_ok_and(|id| !id.trim().is_empty());
    if muxa::backend::pane_id_host_kind(pane) == Some(muxa::HostKind::Cmux) && inside_cmux {
        let env_endpoint = muxa::backend::cmux::endpoint_from_env();
        let mut endpoints = client
            .snapshot()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|agent| agent.pane.as_deref() == Some(pane))
            .filter_map(|agent| agent.tmux_socket)
            .collect::<Vec<_>>();
        endpoints.sort();
        endpoints.dedup();
        let endpoint = if endpoints.is_empty() || endpoints.contains(&env_endpoint) {
            env_endpoint
        } else if endpoints.len() == 1 {
            endpoints.remove(0)
        } else {
            return Err(format!(
                "cmux pane {pane} exists on multiple socket endpoints; run muxa mcp inside the target cmux instance"
            ));
        };
        let target_pane = pane.to_string();
        let target_prompt = prompt.to_string();
        if let Ok(Some(outcome)) = tokio::task::spawn_blocking(move || {
            let backend = muxa::backend::cmux::CmuxBackend::with_endpoint(endpoint);
            if !muxa::PaneBackend::send_prompt_text_on(&backend, None, &target_pane, &target_prompt)
            {
                return None;
            }
            let submitted = if submit {
                if !target_prompt.is_empty() {
                    std::thread::sleep(muxa::backend::PROMPT_SUBMIT_GRACE);
                }
                muxa::PaneBackend::send_text(&backend, &target_pane, "\r")
            } else {
                false
            };
            Some(SendPromptOutcome {
                sent: true,
                submitted,
                submission_confirmed: None,
            })
        })
        .await
        {
            return Ok(outcome);
        }
    }

    client
        .send_prompt(pane, prompt, submit)
        .await
        .map_err(|error| error.to_string())
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

/// Initial send receipts retain routing; subsequent mutations use `mutation_receipt`.
fn request_ack(request: &CollaborationRequest) -> Value {
    let mut value = crate::collaboration_output::receipt(request);
    value["notify"] = json!(request.should_notify());
    value
}

fn request_outcome(request: &CollaborationRequest) -> Value {
    crate::collaboration_output::outcome(request)
}

fn report_view(request: &CollaborationRequest, args: &Value) -> Value {
    if args.get("full").and_then(Value::as_bool).unwrap_or(false) {
        json!(request)
    } else {
        request_outcome(request)
    }
}

#[cfg(test)]
fn message_page(mut requests: Vec<CollaborationRequest>, args: &Value) -> Value {
    requests.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    let offset = usize::try_from(args.get("offset").and_then(Value::as_u64).unwrap_or(0))
        .unwrap_or(usize::MAX);
    let limit = usize::try_from(
        args.get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100),
    )
    .unwrap_or(100);
    let has_more = requests.len().saturating_sub(offset) > limit;
    json!({"requests":requests.iter().skip(offset).take(limit)
        .map(|request| message_view(request, args)).collect::<Vec<_>>(),
        "has_more":has_more, "next_offset":has_more.then(|| offset.saturating_add(limit))})
}

fn message_view(request: &CollaborationRequest, args: &Value) -> Value {
    if args.get("full").and_then(Value::as_bool).unwrap_or(false) {
        return json!(request);
    }
    if let Some(after) = args.get("after_update").and_then(Value::as_u64) {
        return crate::collaboration_output::progress(request, after);
    }
    let mut value = crate::collaboration_output::message(
        request,
        args.get("include_body")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    if args.get("request_id").is_some() {
        if let Some(reply) = &request.reply {
            value["reply"] = json!(reply);
        }
    }
    value
}

/// A tools/call result wrapping compact JSON text. Whitespace has no MCP
/// semantics and becomes input tokens on every model turn that carries it.
fn json_result(value: &Value) -> Value {
    let text = serde_json::to_string(value)
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

/// `muxa_swap_account`: `muxa swap --json` through the daemon's work
/// command transport, here or on a Fleet host. The caller's own pane is
/// never swapped from under its tool call.
async fn swap_account(client: &Client, args: &Value) -> Value {
    let flag = |name: &str| args.get(name).and_then(Value::as_bool).unwrap_or(false);
    let text = |name: &str| args.get(name).and_then(Value::as_str);
    let host = muxa::work_control::remote_host_alias(text("host"));
    let mut words: Vec<String> = args
        .get("targets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    for (name, switch) in [
        ("all", "--all"),
        ("spread", "--spread"),
        ("force", "--force"),
        ("no_continue", "--no-continue"),
        ("dry_run", "--dry-run"),
    ] {
        if flag(name) {
            words.push(switch.to_owned());
        }
    }
    for (name, option) in [
        ("provider", "--provider"),
        ("to", "--to"),
        ("continue", "--continue"),
    ] {
        if let Some(value) = text(name) {
            words.extend([option.to_owned(), value.to_owned()]);
        }
    }
    if host.is_none() {
        if let Some(pane) = muxa::default_backend()
            .current_pane()
            .or_else(current_process_host_pane)
        {
            words.extend(["--exclude".to_owned(), pane]);
        }
    }
    let command_line = match crate::swap::checked_argv(&words) {
        Ok(command_line) => command_line,
        Err(error) => return error_result(&format!("swap: {error}")),
    };
    let output = match client.work_command(host, &command_line, None).await {
        Ok(output) => output,
        Err(error) => return error_result(&format!("swap failed: {error}")),
    };
    let Ok(outcomes) = serde_json::from_str::<Value>(&output.stdout) else {
        return error_result(&format!(
            "swap failed (exit {}): {}",
            output.exit_code,
            output.stderr.trim()
        ));
    };
    let report = json!({ "host": host.unwrap_or("local"), "outcomes": outcomes });
    let failed = outcomes
        .as_array()
        .is_some_and(|all| all.iter().any(|o| o["status"] == "failed"));
    if failed || output.exit_code != 0 {
        error_result(&format!("some agents could not be swapped: {report}"))
    } else {
        json_result(&report)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_target_keeps_rmux_namespace_bound_to_registered_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("routing.sock");
        let store = Store::shared();
        let register = |session: &str, endpoint: &str| AgentEvent::Started {
            id: muxa::event::AgentId {
                kind: AgentKind::Codex,
                session_id: session.into(),
                surface: None,
                pane: Some("rmux:%42".into()),
                tmux_socket: Some(endpoint.into()),
                cwd: None,
            },
            at: time::OffsetDateTime::now_utc(),
        };
        store
            .apply(&register("routing-a", "/tmp/routing-a.sock"))
            .await;
        let server = Server::new(socket.clone(), store.clone()).with_backends(vec![]);
        let (shutdown, rx) = broadcast::channel(1);
        let daemon = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&socket).await;
        let client = Client::new(socket);
        let (endpoint, target) = lifecycle_target(&client, Some("rmux:%42")).await.unwrap();
        assert_eq!(
            endpoint.unwrap(),
            muxa::BackendEndpoint {
                host: HostKind::Rmux,
                socket: "/tmp/routing-a.sock".into()
            }
        );
        assert_eq!(target.as_deref(), Some("%42"));
        assert!(lifecycle_target(&client, Some("rmux:%999")).await.is_err());
        store
            .apply(&register("routing-b", "/tmp/routing-b.sock"))
            .await;
        assert!(lifecycle_target(&client, Some("rmux:%42")).await.is_err());
        let _ = shutdown.send(());
        daemon.await.unwrap();
    }

    #[test]
    fn lifecycle_resolution_rejects_unknown_or_ambiguous_servers() {
        use muxa::HostKind;
        assert!(select_lifecycle_endpoint(HostKind::Rmux, vec![]).is_err());
        assert!(select_lifecycle_endpoint(HostKind::Rmux, vec!["/a".into(), "/b".into()]).is_err());
        let endpoint =
            select_lifecycle_endpoint(HostKind::Rmux, vec!["/a".into(), "/a".into()]).unwrap();
        assert_eq!(endpoint.host, HostKind::Rmux);
        assert_eq!(endpoint.socket, "/a");
    }

    #[test]
    fn message_pages_are_bounded_ordered_and_do_not_repeat_bodies() {
        let requests: Vec<_> = (0..105)
            .map(|index| {
                let mut request = crate::collaboration_output::fixture();
                request.id = format!("req-{index:03}");
                request
            })
            .collect();
        let page = message_page(requests.clone(), &json!({}));
        assert_eq!(page["requests"].as_array().unwrap().len(), 20);
        assert_eq!(page["requests"][0]["request_id"], "req-104");
        assert!(page["requests"][0].get("body").is_none());
        assert_eq!(page["next_offset"], 20);
        let next = message_page(requests.clone(), &json!({"offset":20}));
        assert_eq!(next["requests"][0]["request_id"], "req-084");
        let capped = message_page(requests.clone(), &json!({"limit":1000}));
        assert_eq!(capped["requests"].as_array().unwrap().len(), 100);
        let tail = message_page(requests, &json!({"offset":100,"include_body":true}));
        assert_eq!(tail["requests"].as_array().unwrap().len(), 5);
        assert_eq!(tail["has_more"], false);
        assert!(tail["next_offset"].is_null());
        assert_eq!(tail["requests"][0]["body"], "original instructions");
    }

    #[test]
    fn report_and_message_views_are_bounded_and_full_is_explicit() {
        let request = crate::collaboration_output::fixture();
        let compact = report_view(&request, &json!({}));
        assert!(compact.get("body").is_none());
        assert_eq!(report_view(&request, &json!({"full":true})), json!(request));
        assert!(message_view(&request, &json!({})).get("body").is_none());
        assert_eq!(
            message_view(&request, &json!({"include_body":true}))["body"],
            request.body
        );
        assert_eq!(
            message_view(&request, &json!({"full":true})),
            json!(request)
        );
    }

    #[test]
    fn incremental_reads_do_not_repeat_completed_reply_or_instructions() {
        let mut request = crate::collaboration_output::fixture();
        request.status = RequestStatus::Completed;
        request.reply = Some(muxa::collaboration::CollaborationReply {
            status: RequestStatus::Completed,
            body: "large final reply".repeat(4096),
            artifacts: vec![],
            air_artifacts: vec![],
            at: time::OffsetDateTime::UNIX_EPOCH,
        });
        let args = json!({"request_id":request.id,"after_update":0,"include_body":true});
        let progress = message_view(&request, &args);
        assert_eq!(progress["final_reply_received"], true);
        assert_eq!(progress["reported_status"], "completed");
        assert_eq!(progress["updates"], json!([]));
        for key in ["reply", "body", "from", "to", "paths"] {
            assert!(progress.get(key).is_none(), "unexpected {key}");
        }
        let before = message_view(&request, &json!({"request_id":request.id}));
        let before_bytes = serde_json::to_vec(&before).unwrap().len();
        let after_bytes = serde_json::to_vec(&progress).unwrap().len();
        eprintln!("incremental_read_bytes: before={before_bytes}, after={after_bytes}");
        assert!(after_bytes < 256);
        assert_eq!(
            message_view(&request, &json!({"request_id":request.id}))["reply"]["body"],
            request.reply.as_ref().unwrap().body
        );
        assert_eq!(
            message_view(&request, &json!({"after_update":0,"full":true})),
            json!(request)
        );
    }

    #[test]
    fn caller_session_hint_reads_codex_meta() {
        let params = json!({
            "name": "muxa_room_context",
            "_meta": {
                "x-codex-turn-metadata": { "session_id": "turn-meta-id" },
                "threadId": "01a0fa66-913b-7242-bf6a-5b1165479643",
                "sessionId": "01a0fa66-913b-7242-bf6a-5b1165479643"
            }
        });
        assert_eq!(
            caller_session_hint(Some(&params)).as_deref(),
            Some("01a0fa66-913b-7242-bf6a-5b1165479643")
        );
        let nested_only = json!({ "_meta": { "x-codex-turn-metadata": { "session_id": "s1" } } });
        assert_eq!(
            caller_session_hint(Some(&nested_only)).as_deref(),
            Some("s1")
        );
        assert_eq!(
            caller_session_hint(Some(&json!({ "_meta": { "threadId": " " } }))),
            None
        );
        assert_eq!(caller_session_hint(Some(&json!({ "name": "x" }))), None);
        assert_eq!(caller_session_hint(None), None);
    }

    fn tracked(session: &str, pane: Option<&str>) -> Agent {
        let mut agent: Agent = serde_json::from_value(json!({
            "kind": "codex",
            "session_id": session,
            "state": "idle",
            "started_at": "2026-10-02T00:00:00Z",
            "last_activity_at": "2026-10-02T00:00:00Z",
            "state_entered_at": "2026-10-02T00:00:00Z"
        }))
        .expect("minimal agent");
        agent.pane = pane.map(str::to_owned);
        agent.tmux_socket = pane.map(|_| "default".to_owned());
        agent
    }

    /// Three Codex threads share one app-server whose env says `%9`; each
    /// must resolve to the pane the registry bound *its* session to.
    #[test]
    fn session_origin_uses_the_threads_own_binding() {
        let agents = [
            tracked("thread-junia", Some("rmux:%9")),
            tracked("thread-a", Some("rmux:%137")),
            tracked("thread-b", None),
        ];
        let origin = origin_for_session(&agents, "thread-a").unwrap();
        assert_eq!(origin.pane, "rmux:%137");
        assert_eq!(origin.socket.as_deref(), Some("default"));
        assert!(!origin.console);
        assert_eq!(origin.agent_session_id.as_deref(), Some("thread-a"));
        let unbound = origin_for_session(&agents, "thread-b").unwrap();
        assert_eq!(unbound.pane, "", "an unbound session never borrows a pane");
        assert_eq!(unbound.agent_session_id.as_deref(), Some("thread-b"));
        let unknown = origin_for_session(&agents, "thread-c").unwrap_err();
        assert!(unknown.contains("not tracked"), "{unknown}");
    }
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

    struct RegistrationBackend {
        panes: Vec<PaneInfo>,
    }

    impl PaneBackend for RegistrationBackend {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }
        fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
            self.panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .cloned()
        }
        fn capture_pane(&self, _: &str) -> Option<String> {
            None
        }
        fn pane_pid_map(&self) -> std::collections::HashMap<u32, String> {
            std::collections::HashMap::new()
        }
        fn current_pane(&self) -> Option<String> {
            self.panes.first().map(|pane| pane.pane_id.clone())
        }
        fn focus_pane(&self, _: &str) -> bool {
            false
        }
        fn caps(&self) -> BackendCaps {
            BackendCaps::default()
        }
    }

    fn registration_pane(pane_id: &str, pane_index: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_program: None,
            worker_launch_id: None,
            agent_role: None,
            agent_alias: None,
            workspace_id: None,
            work_id: None,
            pane_id: pane_id.into(),
            session_id: "$1".into(),
            session: "registration".into(),
            window_id: "@1".into(),
            window_name: "agents".into(),
            window_index: "0".into(),
            pane_index: pane_index.into(),
            tty: String::new(),
            current_command: "agent".into(),
            title: String::new(),
            current_path: "/repo".into(),
            pane_pid: 0,
            socket: Some("default".into()),
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

    /// A daemon that answers each `collaboration_send` per `script`, keyed
    /// by the request's target, and records every target it was sent.
    /// `None` drops the connection without an answer, like a lost ACK.
    fn scripted_send_daemon(
        script: &'static [(&'static str, Option<&'static str>)],
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("scripted.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    if request["kind"] != "collaboration_send" {
                        let _ = writer.write_all(b"{\"ok\":true}\n").await;
                        continue;
                    }
                    let target = request["target"].as_str().unwrap().to_string();
                    record.lock().unwrap().push(target.clone());
                    let answer = script
                        .iter()
                        .find(|(name, _)| *name == target)
                        .and_then(|(_, answer)| *answer);
                    match answer {
                        Some(body) => {
                            let _ = writer.write_all(format!("{body}\n").as_bytes()).await;
                        }
                        None => break,
                    }
                }
            }
        });
        (dir, socket, seen)
    }

    /// The pane fallback runs only after the daemon refused the slot, which
    /// proves nothing was stored. After a lost response the request may
    /// exist, so sending again by pane could duplicate the work.
    #[tokio::test]
    async fn a_slot_send_falls_back_only_after_an_explicit_refusal() {
        let origin = CollaborationOrigin {
            pane: "%1".into(),
            socket: None,
            console: false,
            agent_session_id: None,
        };
        let request = NewRequest {
            body: "status?".into(),
            ..NewRequest::default()
        };

        let (_lost_dir, lost, seen) = scripted_send_daemon(&[("role:reviewer", None)]);
        let result = send_peer_call(
            &Client::new(lost),
            &origin,
            "role:reviewer",
            true,
            Some("pane:%2"),
            &request,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            *seen.lock().unwrap(),
            ["role:reviewer"],
            "resent after a lost response"
        );

        let (_refused_dir, refused, seen) = scripted_send_daemon(&[
            (
                "role:reviewer",
                Some(r#"{"ok":false,"error":"target \"role:reviewer\" is ambiguous"}"#),
            ),
            ("pane:%2", Some(r#"{"ok":false,"error":"stop here"}"#)),
        ]);
        let _ = send_peer_call(
            &Client::new(refused),
            &origin,
            "role:reviewer",
            true,
            Some("pane:%2"),
            &request,
        )
        .await;
        assert_eq!(*seen.lock().unwrap(), ["role:reviewer", "pane:%2"]);
    }

    /// `muxa_call_peer` sends a named slot as itself, so the daemon records
    /// it and the request can follow a replaced holder: a
    /// `workspace/work/role` path (which local peer selection does not
    /// know) and a `role:` target alike.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one isolated transport lifecycle, including origin env
    async fn call_peer_sends_named_slots_as_slots() {
        const PROBE: &str = "MUXA_CALL_PEER_SLOT_PROBE";
        if std::env::var_os(PROBE).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mcp::tests::call_peer_sends_named_slots_as_slots",
                    "--nocapture",
                ])
                .env(PROBE, "1")
                .env_remove("MUXA_HOST")
                .env_remove("MUXA_HOSTS")
                .env_remove("RMUX")
                .env_remove("RMUX_PANE")
                .env_remove("CMUX_SOCKET_PATH")
                .env_remove("HERDR_PANE")
                .env("TMUX", "/tmp/muxa-slot-test/default,0,0")
                .env("TMUX_PANE", "%1")
                .env("CODEX_THREAD_ID", "slot-sender")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("slot.sock");
        let origin = current_collaboration_origin().unwrap();
        let store = Store::shared();
        for (pane, session) in [("%1", "slot-sender"), ("%2", "slot-reviewer")] {
            store
                .apply(&AgentEvent::Started {
                    id: muxa::event::AgentId {
                        kind: AgentKind::Codex,
                        session_id: session.into(),
                        surface: None,
                        pane: Some(pane.into()),
                        tmux_socket: origin.socket.clone(),
                        cwd: Some("/repo".into()),
                    },
                    at: time::OffsetDateTime::now_utc(),
                })
                .await;
        }
        let panes = [("%1", "0"), ("%2", "1")]
            .into_iter()
            .map(|(pane, index)| {
                let mut pane = registration_pane(pane, index);
                pane.socket.clone_from(&origin.socket);
                if pane.pane_id == "%2" {
                    pane.workspace_id = Some("ws".into());
                    pane.work_id = Some("W1".into());
                    pane.agent_role = Some("reviewer".into());
                }
                pane
            })
            .collect();
        let backend: SharedBackend = Arc::new(RegistrationBackend { panes });
        let mailbox = muxa::collaboration::CollaborationStore::in_memory(
            muxa::collaboration::CollaborationOptions::default(),
        );
        let server = Server::new(socket.clone(), store)
            .with_backends(vec![backend])
            .with_collaboration(mailbox.clone());
        let (shutdown, rx) = broadcast::channel(1);
        let daemon = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&socket).await;
        let client = Client::new(socket);
        let room = client.collaboration_context(&origin).await.unwrap();
        mailbox
            .set_purpose(&room.peers[0], muxa::collaboration::LaunchPurpose::Worker)
            .await
            .unwrap();
        let tool = |target: &str| {
            json!({"jsonrpc":"2.0", "id":1,"method":"tools/call",
            "params":{"name":"muxa_call_peer","arguments":{
                "target": target, "intent": "question", "body": "status?", "wait": false
            },"_meta":{"threadId":"slot-sender"}}})
        };
        for (target, role) in [
            ("ws/W1/reviewer", "reviewer"),
            ("role:reviewer", "reviewer"),
        ] {
            let response = dispatch(&client, &tool(target)).await.unwrap();
            assert!(
                response["result"].get("isError").is_none(),
                "{target}: {response}"
            );
            let sent: Value =
                serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            let id = sent["request_id"].as_str().unwrap();
            let stored = mailbox.get_for(&room.current, id).await.unwrap();
            assert_eq!(stored.to.agent_session_id, "slot-reviewer", "{target}");
            assert_eq!(
                stored
                    .to_slot
                    .as_ref()
                    .and_then(|slot| slot.role.as_deref()),
                Some(role),
                "{target} was not sent as a slot"
            );
        }
        // A context-bound call stays with the exact session it selected,
        // and a path cannot name one.
        let bound = |target: &str| {
            json!({"jsonrpc":"2.0", "id":1,"method":"tools/call",
            "params":{"name":"muxa_call_peer","arguments":{
                "target": target, "intent": "question", "body": "continue our thread",
                "wait": false, "context_bound": true
            },"_meta":{"threadId":"slot-sender"}}})
        };
        let response = dispatch(&client, &bound("role:reviewer")).await.unwrap();
        assert!(response["result"].get("isError").is_none(), "{response}");
        let sent: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        let stored = mailbox
            .get_for(&room.current, sent["request_id"].as_str().unwrap())
            .await
            .unwrap();
        assert!(
            stored.to_slot.is_none(),
            "a context-bound call became movable"
        );
        let response = dispatch(&client, &bound("ws/W1/reviewer")).await.unwrap();
        assert_eq!(response["result"]["isError"], true, "{response}");
        let _ = shutdown.send(());
        daemon.await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one isolated transport lifecycle, including origin env
    async fn compact_collaboration_dispatch_round_trip() {
        const PROBE: &str = "MUXA_COMPACT_DISPATCH_PROBE";
        if std::env::var_os(PROBE).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mcp::tests::compact_collaboration_dispatch_round_trip",
                    "--nocapture",
                ])
                .env(PROBE, "1")
                .env_remove("MUXA_HOST")
                .env_remove("MUXA_HOSTS")
                .env_remove("RMUX")
                .env_remove("RMUX_PANE")
                .env_remove("CMUX_SOCKET_PATH")
                .env_remove("HERDR_PANE")
                .env("TMUX", "/tmp/muxa-compact-test/default,0,0")
                .env("TMUX_PANE", "%1")
                .env("CODEX_THREAD_ID", "compact-recipient")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("compact.sock");
        let origin = current_collaboration_origin().unwrap();
        let store = Store::shared();
        for (pane, session, kind) in [
            ("%1", "compact-recipient", AgentKind::Codex),
            ("%2", "compact-sender", AgentKind::ClaudeCode),
        ] {
            store
                .apply(&AgentEvent::Started {
                    id: muxa::event::AgentId {
                        kind,
                        session_id: session.into(),
                        surface: None,
                        pane: Some(pane.into()),
                        tmux_socket: origin.socket.clone(),
                        cwd: Some("/repo".into()),
                    },
                    at: time::OffsetDateTime::now_utc(),
                })
                .await;
        }
        let panes = [("%1", "0"), ("%2", "1")]
            .into_iter()
            .map(|(pane, index)| {
                let mut pane = registration_pane(pane, index);
                pane.socket.clone_from(&origin.socket);
                pane
            })
            .collect();
        let backend: SharedBackend = Arc::new(RegistrationBackend { panes });
        let mailbox = muxa::collaboration::CollaborationStore::in_memory(
            muxa::collaboration::CollaborationOptions::default(),
        );
        let server = Server::new(socket.clone(), store)
            .with_backends(vec![backend])
            .with_collaboration(mailbox.clone());
        let (shutdown, rx) = broadcast::channel(1);
        let daemon = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&socket).await;
        let client = Client::new(socket);
        let room = client.collaboration_context(&origin).await.unwrap();
        mailbox
            .set_purpose(&room.current, muxa::collaboration::LaunchPurpose::Worker)
            .await
            .unwrap();
        let request = mailbox
            .create(
                room.peers[0].clone(),
                room.current.clone(),
                new_request_from_args(
                    &json!({"artifacts":["commit:abc"]}),
                    RequestKind::Task,
                    "instructions with pending and newline\nsecond line".into(),
                    true,
                    WorkMode::Execute,
                    vec!["src/allowed.rs".into()],
                    vec![],
                )
                .unwrap(),
            )
            .await
            .unwrap();
        mailbox
            .update_request(&room.peers[0], &request.id, "guidance 1".into())
            .await
            .unwrap();
        let tool = |name: &str, args: Value| {
            json!({"jsonrpc":"2.0", "id":1,"method":"tools/call",
            "params":{"name":name,"arguments":args,"_meta":{"threadId":"compact-recipient"}}})
        };
        let decode = |response: Value| {
            assert!(response["result"].get("isError").is_none(), "{response}");
            serde_json::from_str::<Value>(
                response["result"]["content"][0]["text"].as_str().unwrap(),
            )
            .unwrap()
        };
        let inbox = decode(
            dispatch(&client, &tool("muxa_inbox", json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(inbox["requests"][0]["body"], request.body);
        assert_eq!(inbox["requests"][0]["paths"], json!(["src/allowed.rs"]));
        assert_eq!(inbox["requests"][0]["updates"][0]["body"], "guidance 1");
        assert!(inbox["requests"][0].get("created_at").is_none());
        let listing = decode(
            dispatch(&client, &tool("muxa_list_messages", json!({})))
                .await
                .unwrap(),
        );
        assert!(listing["requests"][0].get("body").is_none());
        let bulk = dispatch(
            &client,
            &tool("muxa_list_messages", json!({"include_body":true})),
        )
        .await
        .unwrap();
        assert_eq!(bulk["result"]["isError"], true);
        let body = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_list_messages",
                    json!({
                        "request_id":request.id,"include_body":true
                    }),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(body["body"], request.body);

        let exact = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_list_messages",
                    json!({
            "request_id":request.id,"after_update":0}),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(exact["updates"][0]["body"], "guidance 1");
        let unchanged = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_list_messages",
                    json!({
            "request_id":request.id,"after_update":1}),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(unchanged["updates"], json!([]));
        for _ in 0..2 {
            crate::cmd_msg(
                &client,
                crate::MsgCmd::Update {
                    request_id: request.id.clone(),
                    body: "guidance 2".into(),
                    json: true,
                },
            )
            .await
            .unwrap();
        }
        let updated = client
            .collaboration_get(&origin, &request.id)
            .await
            .unwrap();
        assert_eq!(
            updated.updates.len(),
            2,
            "retry must not duplicate an update"
        );
        assert_eq!(updated.updates[1].sequence, 2);
        assert_eq!(updated.updates[1].body, "guidance 2");
        assert_eq!(updated.paths, request.paths);
        assert_eq!(updated.work_mode, request.work_mode);
        assert_eq!(updated.body, request.body);
        let ack = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_update_request",
                    json!({
                        "request_id":request.id,"body":"guidance 3"
                    }),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(ack["request_id"], request.id);
        assert_eq!(ack["next_after_update"], 3);
        eprintln!(
            "mutation_receipt_bytes: initial={}, follow_up={}",
            serde_json::to_vec(&request_ack(&updated)).unwrap().len(),
            serde_json::to_vec(&ack).unwrap().len()
        );

        for field in ["from", "to", "initiator", "body", "notify"] {
            assert!(ack.get(field).is_none(), "unexpected {field}");
        }
        let progress = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_list_messages",
                    json!({
                        "request_id":request.id,"after_update":2
                    }),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(progress["updates"].as_array().unwrap().len(), 1);
        assert_eq!(progress["updates"][0]["body"], "guidance 3");
        let parsed = <crate::Args as clap::Parser>::try_parse_from([
            "muxa",
            "msg",
            "list",
            "--request-id",
            request.id.as_str(),
            "--after-update",
            "2",
            "--json",
            "--scope",
            "caller",
        ])
        .unwrap();
        let Some(crate::Cmd::Msg { action }) = parsed.cmd else {
            panic!("expected msg list")
        };
        crate::cmd_msg(&client, action).await.unwrap();

        assert_eq!(
            client
                .collaboration_list(&origin, RequestMailbox::All)
                .await
                .unwrap()
                .len(),
            1
        );

        let invalid = dispatch(
            &client,
            &tool("muxa_list_messages", json!({"after_update":0})),
        )
        .await
        .unwrap();
        assert_eq!(invalid["result"]["isError"], true);
        let timeout = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_wait_reply",
                    json!({
            "request_id":request.id,"timeout_secs":1}),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(timeout["reason"], "timeout");
        assert_eq!(timeout["final_reply_received"], false);
        assert!(timeout.get("reported_status").is_none());
        let reply = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_reply",
                    json!({"request_id":request.id,
            "status":"failed","body":"pending text in a final failure reply"}),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(reply["final_reply_received"], true);
        assert_eq!(reply["reported_status"], "failed");
        assert!(reply.get("from").is_none());
        assert!(reply.get("to").is_none());
        assert!(reply.get("initiator").is_none());
        let late = crate::cmd_msg(
            &client,
            crate::MsgCmd::Update {
                request_id: request.id.clone(),
                body: "too late".into(),
                json: true,
            },
        )
        .await
        .unwrap_err();
        assert!(late.to_string().contains("terminal"), "{late}");

        let report = decode(
            dispatch(
                &client,
                &tool("muxa_peer_report", json!({"request_id":request.id})),
            )
            .await
            .unwrap(),
        );
        assert!(report["result"].get("body").is_none());
        assert_eq!(
            report["result"]["reply"]["body"],
            "pending text in a final failure reply"
        );
        let legacy = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_peer_report",
                    json!({"request_id":request.id,"full":true}),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(legacy["request"]["body"], request.body);
        let queued = mailbox
            .create(
                room.current.clone(),
                room.peers[0].clone(),
                new_request_from_args(
                    &json!({}),
                    RequestKind::Question,
                    "private cancellation instructions".into(),
                    true,
                    WorkMode::ReadOnly,
                    vec![],
                    vec![],
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let cancelled = decode(
            dispatch(
                &client,
                &tool(
                    "muxa_cancel_message",
                    json!({
                        "request_id":queued.id
                    }),
                ),
            )
            .await
            .unwrap(),
        );
        assert_eq!(cancelled["request_id"], queued.id);
        assert_eq!(cancelled["delivery_state"], "cancelled");
        assert_eq!(cancelled["final_reply_received"], false);
        for field in [
            "from",
            "to",
            "initiator",
            "body",
            "notify",
            "reported_status",
        ] {
            assert!(cancelled.get(field).is_none(), "unexpected {field}");
        }
        let stored = client.collaboration_get(&origin, &queued.id).await.unwrap();
        assert_eq!(stored.body, queued.body);
        assert_eq!(stored.status, RequestStatus::Cancelled);
        eprintln!(
            "cancel_receipt_bytes: initial={}, follow_up={}",
            serde_json::to_vec(&request_ack(&queued)).unwrap().len(),
            serde_json::to_vec(&cancelled).unwrap().len()
        );

        let attention_args = json!({"body":"Approve staging deployment?", "action":"approval", "dispatch_id":"a20fa7e0-5f79-4f9b-b172-049154f305cf"});
        let first = decode(
            dispatch(
                &client,
                &tool("muxa_request_attention", attention_args.clone()),
            )
            .await
            .unwrap(),
        );
        let retry = decode(
            dispatch(&client, &tool("muxa_request_attention", attention_args))
                .await
                .unwrap(),
        );
        assert_eq!(first["request_id"], retry["request_id"]);
        let id = first["request_id"].as_str().unwrap();
        let human_request = client.collaboration_get(&origin, id).await.unwrap();
        assert!(human_request.needs_human_response());
        assert_eq!(
            human_request.human_action,
            Some(muxa::collaboration::HumanAction::Approval)
        );
        assert_eq!(human_request.notify, Some(true));
        let mut other_room = room.current.room.clone();
        other_room.window_id = "@999".into();
        let operator = muxa::collaboration::Participant::console(other_room);
        assert!(mailbox
            .list_for(
                &operator,
                RequestMailbox::Incoming,
                muxa::collaboration::MailboxScope::Caller
            )
            .await
            .unwrap()
            .iter()
            .any(|r| r.id == id));
        mailbox
            .reply(
                &operator,
                id,
                RequestStatus::Completed,
                "Approved for staging only".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        let resolved = client.collaboration_get(&origin, id).await.unwrap();
        assert!(!resolved.needs_human_response());
        assert_eq!(resolved.reply.unwrap().body, "Approved for staging only");
        let invalid = dispatch(
            &client,
            &tool(
                "muxa_request_attention",
                json!({"body":"Test", "action":"notice"}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(invalid["result"]["isError"], true);

        shutdown.send(()).unwrap();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn spawned_peer_registration_resumes_from_transition_stream() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("peer-registration.sock");
        let store = Store::shared();
        let origin_id = muxa::event::AgentId {
            kind: AgentKind::Codex,
            session_id: "origin".into(),
            surface: None,
            pane: Some("%1".into()),
            tmux_socket: Some("default".into()),
            cwd: Some("/repo".into()),
        };
        store
            .apply(&AgentEvent::Started {
                id: origin_id,
                at: time::OffsetDateTime::now_utc(),
            })
            .await;
        let backend: SharedBackend = Arc::new(RegistrationBackend {
            panes: vec![registration_pane("%1", "0"), registration_pane("%2", "1")],
        });
        let server = Server::new(sock.clone(), store.clone()).with_backends(vec![backend]);
        let (shutdown, rx) = broadcast::channel(1);
        let daemon = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;
        let client = Client::new(sock);
        let registrations = client.subscribe().await.unwrap();
        let origin = CollaborationOrigin {
            pane: "%1".into(),
            socket: Some("default".into()),
            console: false,
            agent_session_id: None,
        };
        let waiting_client = client.clone();
        let waiter = tokio::spawn(async move {
            wait_for_spawned_peer_registration(&waiting_client, &origin, "%2", 2, registrations)
                .await
        });
        tokio::task::yield_now().await;

        let started = std::time::Instant::now();
        store
            .apply(&AgentEvent::Started {
                id: muxa::event::AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "spawned".into(),
                    surface: None,
                    pane: Some("%2".into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/repo".into()),
                },
                at: time::OffsetDateTime::now_utc(),
            })
            .await;
        let peer = tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("registration transition should wake without a poll interval")
            .unwrap()
            .unwrap()
            .expect("spawned peer should be visible");
        assert_eq!(peer.pane, "%2");
        assert!(started.elapsed() < Duration::from_millis(200));

        // The waiter dropped its stream. Nudge the server-side writer once so
        // it observes the closed socket before the daemon's bounded drain.
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: muxa::event::AgentId {
                    kind: AgentKind::Codex,
                    session_id: "origin".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/repo".into()),
                },
                prompt: "close registration stream".into(),
                at: time::OffsetDateTime::now_utc(),
            })
            .await;
        tokio::task::yield_now().await;
        shutdown.send(()).unwrap();
        daemon.await.unwrap();
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
        assert!(instructions.len() < 140, "instructions: {instructions}");
        assert!(instructions.contains("muxa_collaboration_guide"));
        assert!(instructions.contains("reported_status"));

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
                "muxa_guide",
                "muxa_start_agent",
                "muxa_start_work",
                "muxa_swap_account",
                "muxa_manage_tmux",
                "muxa_send_prompt",
                "muxa_capture_pane",
                "muxa_fleet_status",
                "muxa_fleet_capture",
                "muxa_fleet_send_prompt",
                "muxa_dispatch_work",
                "muxa_dispatch_status",
                "muxa_fleet_call_peer",
                "muxa_update_request",
                "muxa_fleet_update_request",
                "muxa_fleet_wait_reply",
                "muxa_wait_for_change",
                "muxa_collaboration_guide",
                "muxa_room_context",
                "muxa_call_peer",
                "muxa_peer_report",
                "muxa_set_identity",
                "muxa_request_attention",
                "muxa_send_message",
                "muxa_inbox",
                "muxa_list_messages",
                "muxa_reply",
                "muxa_wait_reply",
                "muxa_withdraw_reply_wake",
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
            json!(["claude", "codex", "gemini", "agy", "opencode"])
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
            .contains("workflows=true"));
        assert_peer_tool_definitions(tools);
        assert_fleet_tool_definitions(tools, instructions);

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[test]
    fn status_and_wait_tool_definitions_bound_context() {
        let tools = tool_definitions(&muxa::config::Config::default());
        let status = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_status")
            .unwrap();
        assert!(status["inputSchema"]["properties"]["full"].is_object());
        let wait = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_wait_for_change")
            .unwrap();
        assert_eq!(
            wait["inputSchema"]["properties"]["timeout_secs"]["maximum"],
            600
        );
    }

    fn assert_fleet_tool_definitions(tools: &[Value], _instructions: &str) {
        let fleet_call = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_fleet_call_peer")
            .unwrap();
        assert_eq!(
            fleet_call["inputSchema"]["required"],
            json!(["host", "pane"])
        );
        assert_eq!(
            fleet_call["inputSchema"]["properties"]["intent"]["enum"],
            json!(["review", "question", "task"])
        );
        assert!(fleet_call["description"]
            .as_str()
            .unwrap()
            .contains("never auto-selects or spawns"));
        let fleet_wait = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_fleet_wait_reply")
            .unwrap();
        assert_eq!(
            fleet_wait["inputSchema"]["required"],
            json!(["host", "pane_key", "request_id"])
        );
        assert_eq!(
            fleet_wait["inputSchema"]["properties"]["pane_key"]["type"],
            "object"
        );
    }

    fn assert_peer_tool_definitions(tools: &[Value]) {
        let call_peer = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_call_peer")
            .unwrap();
        assert_eq!(
            call_peer["inputSchema"]["properties"]["intent"]["enum"],
            json!(["review", "question", "task"])
        );
        assert!(call_peer["description"]
            .as_str()
            .unwrap()
            .contains("Defaults to review + read_only"));
        let peer_report = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_peer_report")
            .unwrap();
        assert!(peer_report["description"]
            .as_str()
            .unwrap()
            .contains("not a GitHub PR review"));
        assert!(peer_report["inputSchema"]["properties"]["request_id"].is_object());

        let send_message = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_send_message")
            .unwrap();
        for tool in [call_peer, send_message] {
            let properties = &tool["inputSchema"]["properties"];
            for field in [
                "thread_id",
                "parent_request_id",
                "workspace_id",
                "work_id",
                "run_id",
                "artifacts",
                "links",
            ] {
                assert!(properties[field].is_object(), "missing {field} on {tool}");
                assert!(
                    properties[field]["description"]
                        .as_str()
                        .is_some_and(|description| !description.is_empty()),
                    "missing {field} description on {tool}"
                );
            }
        }
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

        let legacy =
            collaboration_guide(room.clone(), &muxa::config::Config::default(), true, false);
        assert_eq!(legacy["capabilities"]["human_actions"], false);
        assert!(legacy["workflows"].get("human_feedback").is_none());
        let guide = collaboration_guide(room, &muxa::config::Config::default(), true, true);
        let recovery = &guide["workflows"]["peer_interruption"];
        assert_eq!(recovery["wait_slice_secs"], 60);
        assert!(recovery["health"]
            .as_str()
            .unwrap()
            .contains("to.state is stale"));
        assert!(recovery["handoff"]
            .as_str()
            .unwrap()
            .contains("Claimed requests"));
        assert!(recovery["human"].as_str().unwrap().contains("reuse its ID"));
        assert!(recovery["limitation"]
            .as_str()
            .unwrap()
            .contains("running coordinator"));
        let human = &guide["workflows"]["human_feedback"];
        assert_eq!(human["tool"], "muxa_send_message");
        assert_eq!(human["request"]["target"], "human");
        assert_eq!(human["request"]["expects_reply"], true);
        let input = new_request_from_args(
            &human["request"],
            RequestKind::Question,
            "Choose A or B".into(),
            true,
            WorkMode::ReadOnly,
            vec![],
            vec![],
        )
        .unwrap();
        assert_eq!(
            input.human_action,
            Some(muxa::collaboration::HumanAction::Choice)
        );
        assert!(human["wait"].as_str().unwrap().contains("muxa_wait_reply"));
        assert_eq!(guide["room"]["peers"][0]["pane"], "%2");
        let guardrails = guide["guardrails"].to_string();
        for rule in [
            "TMUX_PANE",
            "raw tmux",
            "GitHub/PR",
            "notify=true",
            "Codex to Claude",
        ] {
            assert!(guardrails.contains(rule), "missing {rule}");
        }

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
    fn initialize_keeps_registered_skills_out_of_repeated_instructions() {
        let mut config = muxa::config::Config::default();
        config.message.skills = BTreeMap::from([
            ("review-plan-feedback".into(), "review it".into()),
            ("summarize".into(), "summarize it".into()),
        ]);
        let result = initialize_result(&config);
        let instructions = result["instructions"].as_str().unwrap();
        assert!(!instructions.contains("/review-plan-feedback"));
        assert!(!instructions.contains("/summarize"));
        let request = crate::collaboration_output::fixture();
        let room: RoomContext = serde_json::from_value(json!({
            "self":request.from, "peers":[request.to], "unread":0, "unread_replies":0
        }))
        .unwrap();
        let guide = collaboration_guide(room, &config, false, true);
        assert!(guide.get("workflows").is_none());
        for rule in [
            "TMUX_PANE",
            "raw tmux",
            "GitHub/PR",
            "notify=true",
            "read_only",
            "expects_reply=true",
            "muxa_inbox",
        ] {
            assert!(guide["guardrails"].to_string().contains(rule));
        }
        assert_eq!(
            guide["message_skills"],
            json!(["review-plan-feedback", "summarize"])
        );
    }

    #[test]
    fn configured_launch_guide_changes_schema_and_options_without_inflating_instructions() {
        let mut config = muxa::config::Config::default();
        config.mcp.guide.placement = muxa::config::McpPlacement::Window;
        config.mcp.guide.agent = Some("codex".into());
        config.mcp.guide.options = vec!["--model".into(), "gpt-5.3-codex".into()];
        config.mcp.guide.direction = muxa::config::McpSplitDirection::Down;
        config.mcp.guide.instructions = Some("Keep one task per window.".into());

        let result = initialize_result(&config);
        let instructions = result["instructions"].as_str().unwrap();
        assert!(!instructions.contains("placement=window"));
        assert!(!instructions.contains("agent=codex"));
        assert!(!instructions.contains("gpt-5.3-codex"));
        assert!(!instructions.contains("pane direction=down"));
        assert!(!instructions.contains("Keep one task per window."));
        let preferences = launch_guide_value(&config);
        assert_eq!(preferences["instructions"], "Keep one task per window.");
        assert_eq!(preferences["launch_defaults"]["placement"], "window");
        assert_eq!(
            preferences["launch_defaults"]["options"],
            json!(["--model", "gpt-5.3-codex"])
        );

        let tools = tool_definitions(&config);
        let start_agent = tools
            .iter()
            .find(|tool| tool["name"] == "muxa_start_agent")
            .unwrap();
        assert_eq!(start_agent["inputSchema"]["required"], json!([]));
        assert!(start_agent["inputSchema"]["properties"]["options"].is_object());
        assert_eq!(
            launch_options(
                &json!({}),
                &config,
                crate::agent_launch::AgentProgram::Codex
            )
            .unwrap(),
            ["--model", "gpt-5.3-codex"]
        );
        assert!(launch_options(
            &json!({ "options": ["--search", 3] }),
            &config,
            crate::agent_launch::AgentProgram::Codex
        )
        .is_err());
        assert!(launch_options(
            &json!({ "options": [] }),
            &config,
            crate::agent_launch::AgentProgram::Codex
        )
        .unwrap()
        .is_empty());
        assert!(launch_options(
            &json!({}),
            &config,
            crate::agent_launch::AgentProgram::Claude
        )
        .unwrap()
        .is_empty());

        let guide = launch_guide_value(&config);
        assert_eq!(guide["launch_defaults"]["placement"], "window");
        assert_eq!(guide["launch_defaults"]["agent"], "codex");
    }

    fn peer_participant(kind: AgentKind, pane: &str, state: AgentState) -> Participant {
        serde_json::from_value(json!({
            "agent_kind": kind,
            "launch_purpose": "worker",
            "agent_session_id": format!("session-{pane}"),
            "pane": pane,
            "room": { "host": "tmux", "window_id": "@1" },
            "tmux_session_name": "cal-7000",
            "window_name": "agents",
            "state": state,
            "cwd": "/tmp/project"
        }))
        .unwrap()
    }

    fn peer_room(current: Participant, peers: Vec<Participant>) -> RoomContext {
        RoomContext {
            current,
            peers,
            unread: 0,
            unread_replies: 0,
        }
    }

    #[test]
    fn peer_call_expands_a_registered_skill_without_losing_context() {
        let skills = BTreeMap::from([(
            "review-plan-feedback".into(),
            "Run the iterative peer review workflow.".into(),
        )]);
        let (body, skill) = peer_call_body(
            &json!({
                "skill": "/review-plan-feedback",
                "body": "Review commit abc123.",
                "context": "Tests already passed."
            }),
            &skills,
        )
        .unwrap();

        assert_eq!(skill.as_deref(), Some("/review-plan-feedback"));
        assert_eq!(
            body,
            "Run the iterative peer review workflow.\n\nReview commit abc123.\n\nInvocation context:\nTests already passed."
        );
    }

    #[test]
    fn peer_call_unknown_skill_lists_available_names() {
        let skills = BTreeMap::from([("review".into(), "review it".into())]);
        let error = peer_call_body(&json!({ "skill": "missing" }), &skills).unwrap_err();
        assert!(error.contains("/missing"), "{error}");
        assert!(error.contains("/review"), "{error}");
    }

    #[test]
    fn peer_call_requires_explicit_task_contract_for_execute() {
        assert_eq!(
            peer_call_contract(&json!({})).unwrap(),
            (RequestKind::Review, WorkMode::ReadOnly)
        );
        assert!(peer_call_contract(&json!({ "execute": true })).is_err());
        assert_eq!(
            peer_call_contract(&json!({ "intent": "task", "execute": true })).unwrap(),
            (RequestKind::Task, WorkMode::Execute)
        );
    }

    #[test]
    fn fleet_peer_call_requires_control_and_exact_reply_capability() {
        let collaboration = vec!["collaboration".to_string()];
        let complete = vec!["collaboration".to_string(), "collaboration_get".to_string()];

        let observe = validate_fleet_collaboration_authority(
            "dev",
            false,
            muxa::HostAccessMode::Observe,
            &complete,
        )
        .unwrap_err();
        assert!(observe.contains("explicitly grant mode='control'"));

        let old_remote = validate_fleet_collaboration_authority(
            "dev",
            false,
            muxa::HostAccessMode::Control,
            &collaboration,
        )
        .unwrap_err();
        assert!(old_remote.contains("collaboration_get"));

        validate_fleet_collaboration_authority(
            "dev",
            false,
            muxa::HostAccessMode::Control,
            &complete,
        )
        .unwrap();
    }

    #[test]
    fn request_correlation_and_work_metadata_are_parsed_into_new_request() {
        let request = new_request_from_args(
            &json!({
                "thread_id": "thread-7345",
                "parent_request_id": "req-review-1",
                "workspace_id": "callabo",
                "work_id": "CAL-7345",
                "run_id": "resolve-2",
                "artifacts": ["commit:d4bf2aa53", "test:258-passed"],
                "links": ["https://linear.app/callabo/issue/CAL-7345"]
            }),
            RequestKind::Review,
            "review the must-fix follow-up".into(),
            true,
            WorkMode::ReadOnly,
            vec!["crates/muxa/src/collaboration.rs".into()],
            Vec::new(),
        )
        .unwrap();

        assert_eq!(request.thread_id.as_deref(), Some("thread-7345"));
        assert_eq!(request.parent_request_id.as_deref(), Some("req-review-1"));
        assert_eq!(request.workspace_id.as_deref(), Some("callabo"));
        assert_eq!(request.work_id.as_deref(), Some("CAL-7345"));
        assert_eq!(request.run_id.as_deref(), Some("resolve-2"));
        assert_eq!(request.artifacts, ["commit:d4bf2aa53", "test:258-passed"]);
        assert_eq!(request.links, ["https://linear.app/callabo/issue/CAL-7345"]);
        assert_eq!(
            request.paths,
            ["crates/muxa/src/collaboration.rs".to_string()]
        );
    }

    #[test]
    fn notification_override_is_typed_and_independent_of_reply_contract() {
        for notify in [None, Some(true), Some(false)] {
            let mut args = json!({});
            if let Some(value) = notify {
                args["notify"] = json!(value);
            }
            let request = new_request_from_args(
                &args,
                RequestKind::Notice,
                "checkpoint".into(),
                false,
                WorkMode::ReadOnly,
                Vec::new(),
                Vec::new(),
            )
            .unwrap();
            assert_eq!(request.notify, notify);
            assert!(!request.expects_reply);
        }
        for value in [json!("true"), json!(1), Value::Null] {
            let error = new_request_from_args(
                &json!({"notify": value}),
                RequestKind::Notice,
                "checkpoint".into(),
                false,
                WorkMode::ReadOnly,
                Vec::new(),
                Vec::new(),
            )
            .unwrap_err();
            assert_eq!(error, "notify must be a boolean");
        }
    }

    #[test]
    fn request_metadata_is_optional_and_rejects_non_string_ids() {
        let request = new_request_from_args(
            &json!({}),
            RequestKind::Question,
            "question".into(),
            true,
            WorkMode::ReadOnly,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert!(request.thread_id.is_none());
        assert!(request.parent_request_id.is_none());
        assert!(request.workspace_id.is_none());
        assert!(request.work_id.is_none());
        assert!(request.run_id.is_none());
        assert!(request.artifacts.is_empty());
        assert!(request.links.is_empty());

        let error = new_request_from_args(
            &json!({ "parent_request_id": 7345 }),
            RequestKind::Question,
            "question".into(),
            true,
            WorkMode::ReadOnly,
            Vec::new(),
            Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error, "parent_request_id must be a string");
    }

    /// Withdrawals drive an all-or-nothing mutation. A malformed list must
    /// fail the call, never shrink to the members that happened to parse.
    #[test]
    fn withdraw_reply_wakes_rejects_any_malformed_member() {
        let parse = |withdraw: Value| {
            new_request_from_args(
                &json!({ "withdraw_reply_wakes": withdraw }),
                RequestKind::Question,
                "question".into(),
                true,
                WorkMode::ReadOnly,
                Vec::new(),
                Vec::new(),
            )
        };
        assert_eq!(
            parse(json!(["req_a", "req_b"]))
                .unwrap()
                .withdraw_reply_wakes,
            ["req_a", "req_b"]
        );
        for bad in [
            json!(["req_a", 42]),
            json!("req_a"),
            json!(["req_a", " "]),
            json!([null]),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad} was accepted");
        }
        assert!(request_id_array(&json!({}), "request_ids")
            .unwrap()
            .is_none());
    }

    #[test]
    fn generic_pinned_primary_reuses_worker_but_context_bound_keeps_the_session() {
        let caller = peer_participant(AgentKind::ClaudeCode, "%1", AgentState::Idle);
        let mut primary = peer_participant(AgentKind::Codex, "%2", AgentState::Working);
        primary.launch_purpose = muxa::collaboration::LaunchPurpose::Primary;
        let worker = peer_participant(AgentKind::Codex, "%3", AgentState::Idle);
        let room = peer_room(caller, vec![primary, worker]);
        let (selection, program) = peer_worker_route(&room, "pane:%2", &json!({})).unwrap();
        assert_eq!(selection.unwrap().pane, "%3");
        assert_eq!(program, Some(crate::agent_launch::AgentProgram::Codex));
        let (selection, program) =
            peer_worker_route(&room, "pane:%2", &json!({"context_bound":true})).unwrap();
        assert_eq!(selection.unwrap().pane, "%2");
        assert!(program.is_none());
        let room = peer_room(room.current, vec![room.peers[0].clone()]);
        let (selection, program) = peer_worker_route(&room, "pane:%2", &json!({})).unwrap();
        assert!(selection.is_none());
        assert_eq!(program, Some(crate::agent_launch::AgentProgram::Codex));
        let (selection, program) = peer_worker_route(&room, "pane:%99", &json!({})).unwrap();
        assert_eq!(selection.unwrap().pane, "%99");
        assert!(
            program.is_none(),
            "unknown panes must be resolved by the daemon, not cause a duplicate worker"
        );
    }

    #[test]
    fn automatic_routes_skip_primary_but_explicit_targets_remain_addressable() {
        let current = peer_participant(AgentKind::Codex, "%1", AgentState::Idle);
        let mut primary = peer_participant(AgentKind::ClaudeCode, "%2", AgentState::Idle);
        primary.launch_purpose = muxa::collaboration::LaunchPurpose::Primary;
        primary.alias = Some("human-claude".into());
        primary.roles = vec!["reviewer".into()];
        let room = peer_room(current, vec![primary]);
        assert!(select_peer(&room, "auto").unwrap().is_none());
        assert!(select_peer(&room, "claude").unwrap().is_none());
        assert!(select_peer(&room, "role:reviewer").unwrap().is_none());
        for target in ["pane:%2", "@human-claude"] {
            let selected = select_peer(&room, target).unwrap().unwrap();
            assert_eq!(
                selected.peer.unwrap().launch_purpose,
                muxa::collaboration::LaunchPurpose::Primary
            );
        }
    }

    #[test]
    fn bare_pane_selectors_use_the_callers_rmux_namespace() {
        let current = peer_participant(AgentKind::Codex, "rmux:%1", AgentState::Idle);
        let peer = peer_participant(AgentKind::ClaudeCode, "rmux:%143", AgentState::Idle);
        let room = peer_room(current, vec![peer]);
        for target in ["%143", "pane:%143", "rmux:%143", "pane:rmux:%143"] {
            let selected = select_peer(&room, target).unwrap().unwrap();
            assert_eq!(selected.pane, "rmux:%143");
        }
        let pending = select_peer(&room, "%999").unwrap().unwrap();
        assert_eq!(pending.pane, "rmux:%999");
    }

    #[test]
    fn auto_peer_prefers_a_different_provider_then_numeric_pane_order() {
        let current = peer_participant(AgentKind::ClaudeCode, "%1", AgentState::Idle);
        let room = peer_room(
            current,
            vec![
                peer_participant(AgentKind::ClaudeCode, "%2", AgentState::Idle),
                peer_participant(AgentKind::Codex, "%12", AgentState::Idle),
                peer_participant(AgentKind::Codex, "%3", AgentState::Idle),
            ],
        );

        let selected = select_peer(&room, "auto").unwrap().unwrap();
        assert_eq!(selected.pane, "%3");
        assert_eq!(select_peer(&room, "@claude").unwrap().unwrap().pane, "%2");
        assert_eq!(
            select_peer(&room, "@muxa-peer").unwrap().unwrap().pane,
            "%3"
        );
    }

    #[test]
    fn a_launched_peer_without_a_first_prompt_is_selectable_by_every_target() {
        let current = peer_participant(AgentKind::ClaudeCode, "%1", AgentState::Idle);
        let mut pending = peer_participant(AgentKind::Codex, "%2", AgentState::Starting);
        pending.agent_session_id = "pending-pane:default:%2".into();
        pending.alias = Some("review".into());
        pending.roles = vec!["reviewer".into()];
        let room = peer_room(current, vec![pending]);
        for target in ["auto", "codex", "@review", "role:reviewer", "pane:%2"] {
            let selected = select_peer(&room, target).unwrap().unwrap();
            assert_eq!(selected.pane, "%2");
            assert!(muxa::collaboration::is_pending_session(
                &selected.peer.unwrap().agent_session_id
            ));
        }
    }

    /// An explicitly addressed pane with no registered peer is handed to muxad
    /// as a pending selection — that is the documented retry after a spawn,
    /// and the daemon owns the decision about whether the pane is addressable.
    #[test]
    fn explicit_pane_target_without_a_registered_peer_stays_addressable() {
        let current = peer_participant(AgentKind::ClaudeCode, "%1", AgentState::Idle);
        let room = peer_room(current, vec![]);

        let selection = select_peer(&room, "pane:%3").unwrap().unwrap();
        assert_eq!(selection.pane, "%3");
        assert!(selection.peer.is_none(), "no registered peer to pin to");

        let bare = select_peer(&room, "%3").unwrap().unwrap();
        assert_eq!(bare.pane, "%3");

        // A non-pane selector still reports that nothing matched.
        assert!(select_peer(&room, "@nobody")
            .unwrap_err()
            .contains("no eligible peer matches"));
    }

    /// Queueing against a pane only makes sense when muxa can tell that the
    /// pane became ready. Opencode has neither a discovery classification nor
    /// a screen manifest, so its spawns must keep failing fast.
    #[test]
    fn only_providers_with_a_readiness_signal_fall_back_to_the_pane() {
        use crate::agent_launch::AgentProgram;
        assert!(has_pane_readiness_signal(AgentProgram::Codex));
        assert!(has_pane_readiness_signal(AgentProgram::Claude));
        assert!(has_pane_readiness_signal(AgentProgram::Gemini));
        assert!(has_pane_readiness_signal(AgentProgram::Antigravity));
        assert!(!has_pane_readiness_signal(AgentProgram::Opencode));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newest_report_scans_metadata_pages_without_full_history() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("reports.sock");
        let mut newer = crate::collaboration_output::fixture();
        newer.status = RequestStatus::Completed;
        newer.reply = Some(muxa::collaboration::CollaborationReply {
            status: RequestStatus::Completed,
            body: "review result".into(),
            artifacts: vec![],
            air_artifacts: vec![],
            at: newer.created_at,
        });
        newer.id = "newer".into();
        newer.to.alias = Some("other".into());
        let mut older = newer.clone();
        older.id = "older".into();
        older.to.alias = Some("reviewer".into());
        muxa::collaboration::CollaborationReadView::Metadata.apply(&mut newer);
        muxa::collaboration::CollaborationReadView::Metadata.apply(&mut older);
        let cursor = muxa::collaboration::CollaborationCursor {
            created_at: newer.created_at,
            id: newer.id.clone(),
        };
        let server = crate::collaboration_output::mock_collaboration_responses(
            &socket,
            vec![
                (
                    json!({"/kind":"collaboration_list","/metadata_only":true,"/query/limit":20,"/query/cursor":null}),
                    json!({"ok":true,"collaboration_scope":"caller","collaboration_page":{"requests":[newer],"total":2,"has_more":true,"next_cursor":cursor}}),
                ),
                (
                    json!({"/kind":"collaboration_list","/metadata_only":true,"/query/cursor/id":"newer"}),
                    json!({"ok":true,"collaboration_scope":"caller","collaboration_page":{"requests":[older],"total":2,"has_more":false}}),
                ),
            ],
        );
        let origin = CollaborationOrigin {
            pane: "%1".into(),
            socket: None,
            console: false,
            agent_session_id: None,
        };
        let found = newest_peer_report(&Client::new(socket), &origin, "@reviewer")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "older");
        assert!(found.body.is_empty());
        assert!(found.reply.is_some());
        server.await.unwrap();
    }

    #[test]
    fn peer_report_filters_provider_alias_role_and_pane_without_github_inference() {
        let mut peer = peer_participant(AgentKind::ClaudeCode, "%12", AgentState::Idle);
        peer.alias = Some("reviewer".into());
        peer.roles = vec!["rust".into()];

        assert!(peer_report_target_matches(&peer, "peer"));
        assert!(peer_report_target_matches(&peer, "@muxa-peer"));
        assert!(peer_report_target_matches(&peer, "@claude"));
        assert!(peer_report_target_matches(&peer, "pane:%12"));
        assert!(peer_report_target_matches(&peer, "@reviewer"));
        assert!(peer_report_target_matches(&peer, "role:RUST"));
        assert!(!peer_report_target_matches(&peer, "@codex"));
        assert!(!peer_report_target_matches(&peer, "#2774"));
    }

    #[test]
    fn peer_alias_and_role_selection_refuse_ambiguity() {
        let current = peer_participant(AgentKind::ClaudeCode, "%1", AgentState::Idle);
        let mut reviewer = peer_participant(AgentKind::Codex, "%2", AgentState::Idle);
        reviewer.alias = Some("reviewer".into());
        reviewer.roles = vec!["rust".into()];
        let mut other = peer_participant(AgentKind::GeminiCli, "%3", AgentState::Idle);
        other.roles = vec!["rust".into()];
        let room = peer_room(current, vec![reviewer, other]);

        assert_eq!(select_peer(&room, "@reviewer").unwrap().unwrap().pane, "%2");
        assert!(select_peer(&room, "role:rust")
            .unwrap_err()
            .contains("multiple peers"));
    }

    #[test]
    fn review_auto_routing_uses_opposite_provider_without_overriding_explicit_targets() {
        for target in ["auto", "peer", "@peer", "@muxa-peer"] {
            assert_eq!(
                review_provider_target(target, RequestKind::Review, AgentKind::Codex),
                "claude"
            );
            assert_eq!(
                review_provider_target(target, RequestKind::Review, AgentKind::ClaudeCode),
                "codex"
            );
        }
        assert_eq!(
            review_provider_target("codex", RequestKind::Review, AgentKind::Codex),
            "codex"
        );
        assert_eq!(
            review_provider_target("pane:%2", RequestKind::Review, AgentKind::Codex),
            "pane:%2"
        );
        assert_eq!(
            review_provider_target("auto", RequestKind::Task, AgentKind::Codex),
            "auto"
        );
    }

    #[test]
    fn missing_peer_spawn_defaults_to_the_opposite_provider() {
        let claude = peer_participant(AgentKind::ClaudeCode, "%1", AgentState::Idle);
        let codex = peer_participant(AgentKind::Codex, "%2", AgentState::Idle);
        assert_eq!(
            peer_spawn_program(
                &json!({}),
                &claude,
                "auto",
                &muxa::config::McpGuideConfig::default()
            )
            .unwrap(),
            crate::agent_launch::AgentProgram::Codex
        );
        assert_eq!(
            peer_spawn_program(
                &json!({}),
                &codex,
                "auto",
                &muxa::config::McpGuideConfig::default()
            )
            .unwrap(),
            crate::agent_launch::AgentProgram::Claude
        );
        assert!(peer_spawn_program(
            &json!({ "spawn_agent": "claude" }),
            &codex,
            "@codex",
            &muxa::config::McpGuideConfig::default()
        )
        .is_err());

        let guide = muxa::config::McpGuideConfig {
            agent: Some("gemini".into()),
            ..muxa::config::McpGuideConfig::default()
        };
        assert_eq!(
            peer_spawn_program(&json!({}), &codex, "auto", &guide).unwrap(),
            crate::agent_launch::AgentProgram::Gemini
        );
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

        // The default is deliberately compact so a discovery call cannot
        // inject every agent's full prompts and responses into model context.
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
        assert!(status_payload["summary"].is_object(), "status text: {text}");
        assert!(status_payload["agents"].is_array(), "status text: {text}");
        assert!(
            status_payload.get("topology").is_none(),
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
        let focused_payload: Value = serde_json::from_str(focused_text).unwrap();
        assert_eq!(focused_payload["capture"], "screen of %1");
        assert!(focused_payload["prompts"].is_array());

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
    async fn full_status_is_an_explicit_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-full-status.sock");
        let (client, _sends, tx, handle) = spawn_daemon(&sock).await;

        let status = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 30, "method": "tools/call",
                "params": { "name": "muxa_status", "arguments": { "full": true } },
            }),
        )
        .await
        .unwrap();
        let text = status["result"]["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert!(payload["topology"]["sessions"].is_array(), "{text}");
        assert!(payload.get("agents").is_none(), "{text}");

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
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["changed"], false, "{text}");
        assert_eq!(payload["reason"], "timeout", "{text}");

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
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["matched"], true, "{text}");
        assert_eq!(payload["to"], "idle", "{text}");
        assert_eq!(payload["capture"], "screen of %1", "{text}");

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

    #[test]
    fn json_results_are_compact() {
        let result = json_result(&json!({ "answer": [1, 2], "ok": true }));
        assert_eq!(
            result["content"][0]["text"],
            r#"{"answer":[1,2],"ok":true}"#
        );
    }

    #[tokio::test]
    async fn agent_summary_keeps_endpoint_but_drops_prompt_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-agent-summary.sock");
        let (client, _sends, tx, daemon) = spawn_daemon(&sock).await;
        ingest(
            &client,
            "%7",
            "prompt_submitted",
            json!({ "prompt": "a large prompt that must not be repeated" }),
        )
        .await;
        let agent = client.by_pane("%7").await.unwrap().remove(0);

        let summary = agent_status_summary(
            &agent,
            Some("%7"),
            Some("workspace"),
            Some("work"),
            Some(HostKind::Tmux),
            Some("custom-socket"),
        );
        assert_eq!(summary["pane"], "%7");
        assert_eq!(summary["host"], "tmux");
        assert_eq!(summary["socket"], "custom-socket");
        assert!(summary.get("last_prompt").is_none());
        assert!(summary.get("last_response").is_none());

        tx.send(()).unwrap();
        daemon.await.unwrap();
    }

    /// `render_send_result` tolerates the unknown/legacy shape and the newer
    /// `{sent, submitted}` daemon signal, surfacing "not submitted" plainly.
    #[test]
    fn render_send_result_covers_all_submit_shapes() {
        let text = |v: &Value| v["content"][0]["text"].as_str().unwrap().to_string();

        // Older daemon: requested intent does not prove application submission.
        assert!(text(&render_send_result(5, "%1", true, None, None)).contains("status unavailable"));
        assert!(!text(&render_send_result(5, "%1", false, None, None)).contains("submitted"));
        // PromptSubmitted evidence confirms application submission.
        assert!(
            text(&render_send_result(5, "%1", true, Some(true), Some(true)))
                .contains("submission confirmed")
        );
        let unconfirmed = text(&render_send_result(5, "%1", true, Some(true), None));
        assert!(unconfirmed.contains("application submission unconfirmed"));
        assert!(!unconfirmed.contains("submission confirmed"));
        // Daemon says text landed but Enter did NOT — surfaced to the caller.
        let not_submitted = text(&render_send_result(5, "%1", true, Some(false), None));
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
    #[cfg(unix)]
    #[test]
    fn an_upgrade_is_noticed_whether_the_link_moves_or_the_file_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new, link) = (
            dir.path().join("old"),
            dir.path().join("new"),
            dir.path().join("muxa"),
        );
        std::fs::write(&old, b"build one").unwrap();
        std::fs::write(&new, b"build two!").unwrap();
        std::os::unix::fs::symlink(&old, &link).unwrap();
        let started = BinaryStamp::of(&link).unwrap();
        assert_eq!(BinaryStamp::of(&link), Some(started.clone()), "unchanged");

        // Per-commit installs repoint the link.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&new, &link).unwrap();
        assert_ne!(BinaryStamp::of(&link), Some(started));

        // `cargo install` rewrites the file in place.
        let before = BinaryStamp::of(&link).unwrap();
        std::fs::write(&new, b"build three, longer").unwrap();
        assert_ne!(BinaryStamp::of(&link), Some(before));
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_idle_server_reexecs_only_after_its_binary_is_replaced() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("muxa");
        std::fs::write(&binary, b"build one").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let watch = UpgradeWatch {
            path: binary.clone(),
            started: BinaryStamp::of(&binary).unwrap(),
            reexec: Box::new(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
        };
        let client = Client::new(std::path::PathBuf::from("/nonexistent/muxa.sock"));
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, client_out) = tokio::io::duplex(4096);
        let handle = tokio::spawn(serve_with_config(
            client,
            BufReader::new(server_in),
            server_out,
            Arc::new(muxa::config::Config::default()),
            Some(watch),
        ));
        let mut client_out = BufReader::new(client_out);

        // Not replaced: polls come and go, nothing happens, requests work.
        tokio::time::sleep(UPGRADE_POLL_INTERVAL * 3).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        let mut line = String::new();
        client_out.read_line(&mut line).await.unwrap();
        assert!(line.contains("\"id\":1"));

        // Replaced: the next idle poll hands over to the new binary once.
        std::fs::write(&binary, b"build two, longer").unwrap();
        tokio::time::sleep(UPGRADE_POLL_INTERVAL * 3).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(client_in);
        handle.await.unwrap().unwrap();
    }

    #[test]
    fn initialize_advertises_tool_list_changes() {
        let init = initialize_result(&muxa::config::Config::default());
        assert_eq!(init["capabilities"]["tools"]["listChanged"], true);
    }

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
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["reconciled"], true, "{text}");
        assert_eq!(payload["from"], "working", "{text}");
        assert_eq!(payload["to"], "idle", "{text}");

        tx.send(()).unwrap();
        daemon.await.unwrap();
    }
}
