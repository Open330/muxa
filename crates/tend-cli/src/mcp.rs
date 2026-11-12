//! `tend mcp` — a Model Context Protocol stdio server that lets a coding
//! agent orchestrate tend: ask what other agents are doing, send them
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
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tend::collaboration::{
    CollaborationOrigin, CollaborationRequest, NewRequest, Participant, RequestKind,
    RequestMailbox, RequestStatus, RoomContext, WorkMode,
};
use tend::event::{AgentKind, AgentState};
use tend::ipc::{Client, SendPromptOutcome, TransitionStream};
use tend::state::{Agent, Transition};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// MCP protocol revision we speak. `2024-11-05` is the widely-deployed
/// revision Claude Code and other hosts negotiate; a tools-only server is
/// wire-compatible across the later revisions too.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Deadline for the startup liveness probe against the daemon socket.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// Default `tend_wait_for_change` timeout when the caller omits one.
const DEFAULT_WAIT_SECS: u64 = 30;

/// How long a spawned peer is given to register before the request is queued
/// against its pane instead. Agents that register on boot (claude fires
/// `SessionStart` within a second) resolve inside this window and keep the
/// ordinary session-pinned path; the rest cost one grace period, not a
/// failure — codex cannot register until a prompt reaches it.
const DEFAULT_SPAWN_GRACE_SECS: u64 = 10;
/// Hard ceiling on `tend_wait_for_change` so a caller can't pin the stdio
/// loop forever on a typo'd huge timeout.
pub(crate) const MAX_WAIT_SECS: u64 = 600;
/// Compatibility polling is used only with controllers lacking shared reply waits.
const FLEET_REPLY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Sent to MCP hosts during initialization so collaboration is a first-class
/// workflow rather than a capability the model has to infer from tool names.
///
/// The last sentence reaches every agent whatever its task. The
/// agent-integration bootstrap carries the same rule, but agents read it
/// only for Tend work, so an agent on ordinary work would end its turn on a
/// prose question that tend can only infer (`tend::awaiting_reply`).
const MCP_SERVER_INSTRUCTIONS: &str =
    "Read tend_guide once. Delivery is not task success; reported_status is peer-declared. \
     Blocked on a person's decision? Call tend_request_attention, not just prose.";

/// How often `tend_wait_for_change` reconciles against a fresh daemon
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
/// It starts even when tendd is unreachable. Agent hosts launch MCP servers
/// once and do not retry: refusing to start because the daemon was
/// restarting (or, once, because its full snapshot outgrew an IPC frame)
/// left that agent without tend tools for the rest of its life. Each tool
/// call opens its own daemon connection, so calls fail with the daemon's
/// error while it is down and work again when it is back.
pub async fn run(client: Client, config: tend::config::Config) -> Result<()> {
    if let Err(e) = client.hello(PROBE_TIMEOUT).await {
        eprintln!(
            "tend mcp: daemon is not reachable ({e}); serving anyway. Tools report the \
             error until `tendd` is back (check TEND_SOCKET)."
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
#[cfg(not(test))]
const UPGRADE_POLL_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
const UPGRADE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Set on a server re-executed by [`UpgradeWatch`].
const REEXEC_ENV: &str = "TEND_MCP_REEXEC";

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
/// keep tend tools current without restarting. Agent hosts spawn `tend
/// mcp` once per session and never respawn it; before this, every upgrade
/// left each agent's tools on the old build until the agent restarted.
/// The server keeps no session state (each call opens its own daemon
/// connection), so the new process can take over the same stdio.
struct UpgradeWatch {
    /// The path the server was launched from (`tend` on PATH), not the
    /// build it resolved to: that is what an upgrade repoints.
    path: std::path::PathBuf,
    started: BinaryStamp,
    rejected: Option<tend::binary_upgrade::LaunchStamp>,
    reexec: Box<dyn FnMut(&std::path::Path) + Send>,
}

impl UpgradeWatch {
    #[cfg(unix)]
    fn for_this_process() -> Option<Self> {
        let path = launch_path()?;
        #[cfg(target_os = "linux")]
        let started = {
            let image = std::path::Path::new("/proc/self/exe");
            let metadata = std::fs::metadata(image).ok()?;
            BinaryStamp {
                target: std::fs::read_link(image).ok()?,
                len: metadata.len(),
                modified: metadata.modified().ok(),
            }
        };
        #[cfg(not(target_os = "linux"))]
        let started = std::env::current_exe()
            .ok()
            .and_then(|image| BinaryStamp::of(&image))?;
        Some(Self {
            path,
            started,
            rejected: None,
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
        .arg0(
            std::env::args_os()
                .next()
                .unwrap_or_else(|| path.as_os_str().to_owned()),
        )
        .args(std::env::args_os().skip(1))
        .env(REEXEC_ENV, "1")
        .exec();
    tracing::warn!(%error, path = %path.display(), "mcp: could not re-exec the upgraded binary; keeping this one");
}

/// Read newline-delimited JSON-RPC requests from `reader` and write one
/// response line per request to `writer`, **dispatching each request on its
/// own task**. This is the crux of the non-blocking transport: a
/// long-running tool (`tend_wait_for_change`, up to 600 s) must never wedge
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
        Arc::new(tend::config::Config::default()),
        None,
    )
    .await
}

async fn serve_with_config<R, W>(
    client: Client,
    reader: BufReader<R>,
    writer: W,
    config: Arc<tend::config::Config>,
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
                    let check=tend::binary_upgrade::UpgradeCheck::new(watch.path.clone(),std::env::args_os().skip(1).collect());
                    let stamp=check.stamp().ok();
                    if stamp.is_some() && stamp == watch.rejected {continue;}
                    watch.rejected=stamp;
                    match check.validate().await {
                        Ok(checked) if checked.unchanged()=>(watch.reexec)(&checked.target),
                        Ok(_)=>tracing::warn!("mcp: replacement changed after validation; keeping the live process"),
                        Err(reason)=>tracing::warn!(%reason,"mcp: upgrade blocked; keeping the live transport"),
                    }
                    // A returning exec failed or a test hook returned. Keep
                    // watching, but don't probe this binary/config pair again.
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
            let parsed = serde_json::from_str::<Value>(&line);
            let cost = tracing::enabled!(tracing::Level::DEBUG)
                .then(|| parsed.as_ref().ok().and_then(tool_call_cost))
                .flatten();
            let response = match parsed {
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
                match write_response(&writer, &resp).await {
                    Ok(response_wire_bytes) => {
                        if let Some((tool, input_body_bytes)) = cost {
                            tracing::debug!(
                                tool,
                                input_body_bytes,
                                request_json_bytes = line.len(),
                                response_wire_bytes,
                                result_text_bytes = result_text_bytes(&resp),
                                "mcp tool cost"
                            );
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "mcp: failed to write response"),
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

/// Numeric diagnostics only: never retain arguments or returned text.
fn tool_call_cost(request: &Value) -> Option<(String, usize)> {
    if request["method"].as_str()? != "tools/call" {
        return None;
    }
    Some((
        request["params"]["name"].as_str()?.to_owned(),
        request["params"]["arguments"]["body"]
            .as_str()
            .map_or(0, str::len),
    ))
}

fn result_text_bytes(response: &Value) -> usize {
    response["result"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .map(str::len)
        .sum()
}

/// Write one response object followed by a newline, holding the shared
/// writer lock across the entire write so concurrent responses never
/// interleave mid-line.
async fn write_response<W>(writer: &Arc<Mutex<W>>, resp: &Value) -> std::io::Result<usize>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(resp).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    let mut guard = writer.lock().await;
    guard.write_all(&bytes).await?;
    guard.flush().await?;
    Ok(bytes.len())
}

/// Handle one parsed JSON-RPC message. Returns `Some(response)` for
/// anything that warrants a reply and `None` for notifications (a request
/// object with no `id`), which JSON-RPC forbids answering.
///
/// Framing robustness (JSON-RPC 2.0 + MCP `2024-11-05`) — no valid JSON is
/// ever silently dropped:
/// - a **batch array** is rejected with a single `-32600` error (tend does
///   not implement batching; documented in `docs/MCP.md`);
/// - a **bare value** (number / string / bool / null) is not a request
///   object and carries no `id`, so `-32600` with `id: null`;
/// - an **object** is routed by [`dispatch_object`], which still returns a
///   proper error for a missing/invalid `jsonrpc` or `method` whenever an
///   `id` is present to address the reply to.
#[cfg(test)]
async fn dispatch(client: &Client, req: &Value) -> Option<Value> {
    dispatch_with_config(client, req, &tend::config::Config::default()).await
}

async fn dispatch_with_config(
    client: &Client,
    req: &Value,
    config: &tend::config::Config,
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
    config: &tend::config::Config,
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

fn initialize_result(_config: &tend::config::Config) -> Value {
    let instructions = MCP_SERVER_INSTRUCTIONS;
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": true } },
        "serverInfo": {
            "name": "tend",
            "version": crate::mcp_revision::server_version(),
        },
        "instructions": instructions,
    })
}

/// The provider arguments a launch of `program` would actually carry,
/// rendered for the guide. Reads through [`tend::config::Config::launch_options`]
/// rather than `[mcp.guide].options` directly: with `[agent.<program>]`
/// configured, quoting the guide's own list would advertise arguments the
/// launch is not going to use.
fn guide_options_for(config: &tend::config::Config, program: &str) -> Vec<String> {
    config.launch_options(program, None)
}

fn launch_guide_summary(config: &tend::config::Config) -> String {
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
        "User defaults: placement={}, agent={}, options={}, direction={}.",
        guide.placement.as_str(),
        agent,
        options,
        guide.direction.as_str()
    )
}

fn launch_guide_value(config: &tend::config::Config) -> Value {
    let guide = &config.mcp.guide;
    // Every provider tend can launch, so a caller choosing a non-default
    // agent still learns what that one will be started with.
    let agent_options: serde_json::Map<String, Value> = ["claude", "codex", "agy", "opencode"]
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
            "placement": "Applied to unmanaged tend_start_agent calls when placement is omitted.",
            "agent": "Applied when agent is omitted. Without a configured value, callers must choose an agent.",
            "options": "Additional arguments after Tend's built-in provider profile and before the initial prompt, for launch_defaults.agent. Explicit tool options replace these defaults.",
            "agent_options": "The same, per provider, from [agent.<program>] in tend's config. Use the entry for the agent you are launching.",
            "managed_work": "Managed Work always uses workspace=session, current Run=window, and agent=pane.",
            "peer_spawn": "Collaboration peer spawning always creates a pane in the current window; agent/options/direction preferences still apply when compatible."
        }
    })
}

/// The control and collaboration tools this server exposes, with JSON-Schema
/// `inputSchema`s.
#[allow(clippy::too_many_lines)] // declarative JSON schemas are clearest kept beside tool names
fn tool_definitions(config: &tend::config::Config) -> Vec<Value> {
    let start_required = if config.mcp.guide.agent.is_some() {
        json!([])
    } else {
        json!(["agent"])
    };
    let start_description = format!(
        "Start one allowlisted coding agent in a new tmux pane, window or session (or a managed Work window) and return its pane id. The codex profile runs codex --yolo. {}",
        launch_guide_summary(config)
    );
    let mut tools = vec![
        json!({
            "name": "tend_agent_find",
            "description": "Find where agents working on a task are currently located on this daemon's local observed panes. Start here for agent-location questions instead of reading full topology or transcripts. Extract a few task keywords/identifiers from prose; all whitespace-separated keywords must match (case-insensitive) across workspace, window, cwd, alias, titles, recap and latest prompt/response. Returns ranked candidates with exact host/socket/session/window/pane identity and up to three short evidence excerpts. Matches do not prove task ownership or completion. Exclude your investigating session when known. No match: try alternate keywords, then tend_agent_search for historical evidence; historical sessions do not establish a current pane. Remote Fleet hosts require a separate Fleet query.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Task identifier or a few keywords, not a full question." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Default 10." },
                    "refresh": { "type": "boolean", "description": "Re-observe inventory now. Default false reuses the daemon's shared inventory; freshness reports its observation age and configured maximum." },
                    "exclude_sessions": { "type": "array", "items": { "type": "string" }, "description": "Agent session IDs to exclude." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "tend_status",
            "description": "Observe agents: a compact summary by default (full=true for the whole topology), or one pane with its screen and recent prompts in one call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "string", "description": "Optional exact pane id." },
                    "full": { "type": "boolean", "description": "Whole topology." },
                    "include_capture": { "type": "boolean", "description": "pane: its visible screen." },
                    "history_limit": { "type": "integer", "minimum": 0, "maximum": 20, "description": "Recent prompts (of pane, or all panes)." },
                    "max_capture_lines": { "type": "integer", "minimum": 1, "maximum": 400, "description": "Default 120." }
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "tend_recent_prompts",
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
            "name": "tend_start_agent",
            "description": start_description,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "purpose": {"type":"string","enum":["primary","worker"],"description":"primary (default) is protected; worker takes peer requests."},
                    "agent": { "type": "string", "enum": ["claude", "codex", "agy", "opencode"], "description": "Optional when configured." },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Extra CLI arguments (replace the configured ones)." },
                    "placement": { "type": "string", "enum": ["pane", "window", "session"], "description": "Default from config (pane)." },
                    "target": { "type": "string", "description": "Pane to split; default yours." },
                    "cwd": { "type": "string", "description": "Default this process's cwd." },
                    "prompt": { "type": "string", "description": "First task; omit for an empty agent." },
                    "name": { "type": "string", "description": "Optional window/session name." },
                    "workspace": { "type": "string", "description": "With work; default the cwd basename." },
                    "work": { "type": "string", "description": "Managed Work id: reuse or create its window." },
                    "role": { "type": "string", "description": "Optional pane role such as implementer or reviewer." },
                    "alias": { "type": "string", "description": "Stable per-work pane name." },
                    "task": { "type": "string", "description": "Optional short pane task label." },
                    "direction": { "type": "string", "enum": ["right", "down"], "description": "Default from config (right)." }
                },
                "required": start_required,
                "additionalProperties": false
            },
        }),
        json!({
            "name": "tend_start_work",
            "description": "Bring one Work to the state its pipeline declares: resolve an external issue, route it to a workspace and directory, and create only the missing agent panes (a whole team in one call). Re-running converges: running panes get body typed in instead of a relaunch. Needs [[route]]/[pipeline.*] config or pipeline. dry_run=true shows the plan. CLI: `tend work up`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "work": { "type": "string", "description": "Stable Tend Work id. One managed tmux window per id." },
                    "external": { "type": "string", "description": "Optional external issue key to resolve and link, for example ENG-1234." },
                    "pipeline": { "type": "string", "description": "Pipeline name from [pipeline.*]. Defaults to the matching route's; required when no route matches." },
                    "workspace": { "type": "string", "description": "Workspace/project session. Defaults to the route's, then the cwd basename." },
                    "cwd": { "type": "string", "description": "Work directory. Overrides the route; refused when the route creates a git worktree." },
                    "body": { "type": "string", "description": "What the work is. Launched agents get it in their first prompt; agents already running get it typed into their pane." },
                    "skill": { "type": "string", "description": "Registered Tend message skill name, with or without leading /. Expanded ahead of body, exactly as in tend_call_peer." },
                    "context": { "type": "string", "description": "Optional bounded context appended after the skill/body. Tend never attaches transcripts or diffs implicitly." },
                    "no_ticket": { "type": "boolean", "description": "Skip ticket lookup and use the work id alone. Default false." },
                    "refresh": { "type": "boolean", "description": "Ignore the cached ticket and ask the resolver again. Default false." },
                    "dry_run": { "type": "boolean", "description": "Return the plan without creating panes or sending prompts. Default false." }
                },
                "required": ["work"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "tend_swap_account",
            "description": "Move Claude/Codex agents to another account and resume each conversation in its pane (stop, relaunch on the account with the same session, answer startup dialogs, continue only cut-off turns). Default targets: agents stopped by a usage cap. dry_run=true shows the plan. CLI: `tend swap`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Fleet host alias. Omit (or \"local\") for this host." },
                    "targets": { "type": "array", "items": { "type": "string" }, "description": "Panes (%5, rmux:%5) or provider session ids. Omit for every capped agent." },
                    "all": { "type": "boolean", "description": "Every Claude/Codex agent, not only capped ones." },
                    "provider": { "type": "string", "enum": ["claude", "codex"] },
                    "to": { "type": "string", "description": "Account name, auto (default), or same to restart each target on its own identified account." },
                    "when_idle": { "type": "boolean", "description": "Keep busy targets pending until idle (daemon job); force takes precedence." },
                    "idle_timeout": { "type": "string", "description": "Maximum idle wait, e.g. 30m or 6h (default 6h). Requires when_idle." },
                    "spread": { "type": "boolean", "description": "Round-robin agents over every account with quota left." },
                    "force": { "type": "boolean", "description": "Also stop agents that are mid-turn." },
                    "continue_text": { "type": "string", "description": "Follow-up prompt wording. Who gets it is continue_when; naming it does not widen it." },
                    "continue_when": { "type": "string", "enum": ["limited", "always", "never"], "description": "Who gets the follow-up: limited (default: turns cut off by a cap or this swap), always, never." },
                    "continue_evidence": { "type": "string", "enum": ["binding", "verified"], "description": "What the relaunch must show before the follow-up: binding (resumes that conversation) or verified (also confirmed on the account)." },
                    "no_continue": { "type": "boolean", "description": "Send no follow-up prompt." },
                    "dry_run": { "type": "boolean", "description": "Return the plan without touching any agent." },
                    "action": { "type": "string", "enum": ["swap", "retry", "status", "cancel"], "description": "swap (default), retry an ended job, status, cancel. Results carry the job handle; never resend a swap." },
                    "job": { "type": "string", "description": "For retry, status and cancel: the request key (or job id) a start returned." },
                    "request_key": { "type": "string", "description": "swap/retry: your own job key, so you hold the handle before anything is sent." },
                    "detach": { "type": "boolean", "description": "For swap and retry: return once the job is accepted instead of waiting for it." },
                    "wait_secs": { "type": "integer", "minimum": 0, "maximum": 120, "description": "status/cancel: wait for the job to end. Prefer tend_wait(job)." },
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "tend_manage_tmux",
            "description": "Manage Tend's tmux bindings: session=Workspace, window=current Run, \
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
                    "work": { "type": "string", "description": "Tend Work id for Work actions, for example TEST-0001." },
                    "confirm": { "type": "boolean", "description": "Must be true for terminate_agent, close_work, and close_workspace." }
                },
                "required": ["action"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "tend_send_prompt",
            "description": "Type keystrokes (and Enter) into an idle agent's pane; refused with target_not_idle otherwise. A control action: for work, use tend_call_peer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": { "type": "string", "description": "%12, rmux:%12, herdr:p1." },
                    "socket": { "type": "string", "description": "Server endpoint; default this process's." },
                    "session_id": { "type": "string", "description": "Only if this session still holds the pane." },
                    "text": { "type": "string", "description": "Literal text." },
                    "submit": { "type": "boolean", "description": "Press Enter after. Default true." },
                    "force": { "type": "boolean", "description": "Type even into a busy, prompting or untracked pane; only on a person's instruction." },
                },
                "required": ["pane", "text"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "tend_capture_pane",
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
            "name": "tend_fleet_status",
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
            "name": "tend_fleet_capture",
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
            "name": "tend_fleet_send_prompt",
            "description": "Send literal text to one exact pane on a named Fleet host. Local is always control-capable; remote hosts must use control mode. The daemon never retries this mutation. For ongoing agent work use tend_fleet_update_request; this tool has no request lifecycle or completion acknowledgement.",
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
            "name":"tend_dispatch_work",
            "description":"Delegate authorized Work through the shared Fleet coordinator. Resolves node selectors and personalized paths, prepares an isolated worktree at an exact commit, and launches the configured pipeline. Reuse dispatch_id on retries. plan=true is read-only; launched means started, not completed.",
            "inputSchema":{"type":"object","required":["dispatch_id","workspace","work","commit","body"],"properties":{
                "dispatch_id":{"type":"string"},"workspace":{"type":"string"},"work":{"type":"string"},"commit":{"type":"string"},"body":{"type":"string"},"selector":{"type":"string"},"host":{"type":"string"},"plan":{"type":"boolean","default":false}
            }}
        }),
        json!({
            "name":"tend_dispatch_status","description":"Read the durable Fleet dispatch by UUID through the shared coordinator. Does not repeat execution.",
            "inputSchema":{"type":"object","required":["dispatch_id"],"properties":{"dispatch_id":{"type":"string"}}}
        }),
        json!({
            "name": "tend_fleet_call_peer",
            "description": "Send one durable structured collaboration request to one exact agent pane on a named Fleet host and optionally wait for its reply. The host and pane are always explicit; this tool never auto-selects or spawns a remote agent. Remote hosts must be configured mode='control' and advertise collaboration reply support. Defaults to review + read_only. Set execute=true only after explicit user authorization. If the bounded wait expires, continue with tend_fleet_wait_reply and the returned pane_key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dispatch_id": {"type":"string", "description":"UUID for this dispatch. Reuse unchanged on any uncertain-delivery retry."},
                    "delegation_parent": {"type":"object", "required":["node_id","request_id"], "properties":{"node_id":{"type":"string"},"request_id":{"type":"string"}}},
                    "host": { "type": "string", "description": "Exact Fleet host alias. The remote host must be explicitly configured mode='control'." },
                    "pane": { "type": "string", "description": "Exact or unambiguous live pane id/path, for example %12 or session/window/%12." },
                    "intent": { "type": "string", "enum": ["review", "question", "task"], "description": "Default review." },
                    "body": { "type": "string", "description": "Request-specific instruction. Optional when skill is supplied." },
                    "skill": { "type": "string", "description": "Registered local Tend message skill name, with or without leading /. Its prompt is expanded before sending." },
                    "context": { "type": "string", "description": "Optional bounded context appended after the skill/body. Tend never attaches transcripts or diffs implicitly." },
                    "execute": { "type": "boolean", "description": "Default false. Requires intent=task, explicit user authorization, and a control-mode host." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Advisory task path scope. For new Claude peers only, existing non-private directory entries may become --add-dir context roots. Files do not grant parent directories; Codex receives no automatic write roots. Provider approval mode still governs edits; the task contract is not a filesystem ACL." },
                    "wait": { "type": "boolean", "description": "Wait for the structured reply in this call. Default true. For a quick check with no independent local work, this avoids a separate send receipt and later wait. Use false while doing independent work." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "description": "Reply wait timeout. Default 300." },
                    "thread_id": { "type": "string", "description": "Stable id shared by one causal conversation. Omit for a root request; tend assigns its request id." },
                    "parent_request_id": { "type": "string", "description": "Exact prior request that caused this follow-up." },
                    "workspace_id": { "type": "string", "description": "Stable workspace/project id used to group collaboration history." },
                    "work_id": { "type": "string", "description": "Durable Work id within the workspace, used to group requests across runs." },
                    "run_id": { "type": "string", "description": "Specific execution/run id for this request." },
                    "artifacts": { "type": "array", "items": { "type": "string" }, "description": "Generic artifact identifiers or locators attached to the request." },
                    "links": { "type": "array", "items": { "type": "string" }, "description": "Generic relationship or URL locators associated with the request." }
                },
                "required": ["host", "pane"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "tend_update_request",
            "description": "Append verified guidance or a progress report to an existing nonterminal request without completing it. Use this instead of sending a new message to pane:console or injecting a prompt. Only existing participants may update; scope and execution authority stay unchanged. Read updates through tend_list_messages and finish with one tend_reply.",
            "inputSchema": {"type":"object", "properties": {
                "request_id":{"type":"string"}, "body":{"type":"string","description":"One consolidated update, at most 8192 UTF-8 bytes. Link larger artifacts."}
            }, "required":["request_id","body"], "additionalProperties":false}
        }),
        json!({
            "name": "tend_fleet_update_request",
            "description": "Append consolidated guidance to an existing Fleet request using its exact returned host/pane_key/request_id. Does not send another task or inject terminal text. Existing authorized scope is unchanged. The recipient reads updates on the same request; terminal completion still uses tend_reply.",
            "inputSchema": {"type":"object", "properties": {
                "host":{"type":"string"}, "pane_key":{"type":"object"},
                "request_id":{"type":"string"}, "body":{"type":"string","description":"Verified guidance within the original scope, at most 8192 UTF-8 bytes."}
            }, "required":["host","pane_key","request_id","body"], "additionalProperties":false}
        }),
        json!({
            "name": "tend_fleet_wait_reply",
            "description": "Continue waiting for the structured reply to a prior tend_fleet_call_peer request. Pass the exact host, pane_key, and request_id returned by that call. The durable remote request remains readable even if the target pane has since exited. Remote hosts must remain explicitly configured mode='control'. Prefer completion wakes/checkpoints; use 30-60 seconds for a foreground wait, not repeated 1-second probes. Check fresh recipient/host health after timeout; follow peer_interruption in tend_collaboration_guide.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Exact Fleet host alias used for the original call." },
                    "pane_key": {
                        "type": "object",
                        "description": "Exact collision-free PaneKey object returned by tend_fleet_call_peer; do not reconstruct it from a pane id."
                    },
                    "request_id": { "type": "string", "description": "Exact collaboration request id returned by tend_fleet_call_peer." },
                    "after_update": {"type":"integer", "minimum":0, "description":"Optionally return when a progress/guidance sequence newer than this value is available, without waiting for terminal completion. Start at 0; resume with next_after_update."},
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": DEFAULT_WAIT_SECS, "description": "Reply wait timeout. Default 30 seconds." }
                },
                "required": ["host", "pane_key", "request_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "tend_wait_for_change",
            "description": "Block until an agent changes state. Set until=settled \
                to ignore intermediate working transitions and return only when the \
                agent is idle, blocked, errored, or stopped. A focused wait can also \
                include the final pane capture, avoiding polling loops and extra calls. \
                If a Codex host yields a background cell, resume that same cell with \
                yield_time_ms=60000 instead of starting another Tend wait.",
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
            "name": "tend_collaboration_guide",
            "description": "Return room.self identity, peers, launch preferences and messaging guardrails. Call once; refresh peers with tend_room_context. Set workflows=true only for detailed examples.",
            "inputSchema": { "type": "object", "properties": {"workflows":{"type":"boolean", "description":"Include detailed workflow examples. Default false."}}, "additionalProperties": false },
        }),
        json!({
            "name": "tend_room_context",
            "description": "Identify this agent (self) and list collaboration peers in the same tmux window, plus unread request and reply counts. A lightweight refresh after tend_collaboration_guide; use the returned pane, alias, role, and state to choose a reviewer or delegated subagent.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "tend_call_peer",
            "description": "Send one durable request to another agent; the reply returns to you. Default: Tend selects a healthy same-window worker for target (or creates one with configured permissions), expands an optional /skill, and returns at once. Defaults to review + read_only; execute=true only on the user's explicit authorization. context_bound=true when that exact conversation is required. For an existing reply use tend_inbox(view=report).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Default auto: peer, a provider (codex, claude, ...), %N, @alias, role:<name>, or workspace/work/role[@alias]." },
                    "intent": { "type": "string", "enum": ["review", "question", "task", "notice"], "description": "Default review. notice is one-way, delivered exactly to target." },
                    "body": { "type": "string", "description": "The instruction. Optional with skill." },
                    "skill": { "type": "string", "description": "Registered Tend message skill, expanded before sending." },
                    "context": { "type": "string", "description": "Bounded context appended after body; nothing is attached implicitly." },
                    "execute": { "type": "boolean", "description": "Needs intent=task and the user's authorization." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Advisory path scope for execute work." },
                    "wait": { "type": "boolean", "description": "Block for the reply in this call (timeout_secs, default 300). Default false: the reply arrives as a wake." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 },
                    "spawn_if_missing": { "type": "boolean", "description": "Override automatic worker creation." },
                    "context_bound": {"type":"boolean","description":"Require that exact session's conversation; a primary keeps it queued for acceptance unless user_authorized."},
                    "spawn_agent": { "type": "string", "enum": ["claude", "codex", "agy", "opencode"], "description": "Provider to create. Default: the opposite of yours." },
                    "spawn_timeout_secs": { "type": "integer", "minimum": 1, "maximum": 60 },
                    "parent_request_id": { "type": "string", "description": "The prior request this follows up." },
                    "artifacts": { "type": "array", "items": { "type": "string" }, "description": "Artifact identifiers or locators." },
                    "links": { "type": "array", "items": { "type": "string" }, "description": "Related URLs or locators." },
                    "user_authorized": { "type": "boolean", "description": "Only when the user told you to deliver this request to that session: a protected primary takes it at its next turn boundary without being typed into. Grants no execute authority." },
                    "withdraw_reply_wakes": { "type": "array", "maxItems": 16, "items": { "type": "string" }, "description": "Your earlier request ids this replaces; their late replies will not wake you." }
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "tend_peer_report",
            "description": "Read the newest received Tend peer report/reply sent to this agent, or retrieve an exact request_id. Use this first for possessive requests such as '@peer's report', 'peer findings', 'the peer conversation', or 'resolve the peer feedback'. This is Tend mailbox data, not a GitHub PR review; never infer a PR number or review thread from it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Optional report author filter: peer/auto/@tend-peer, provider, pane id, @alias, or role:<name>. Default newest report from any peer." },
                    "request_id": { "type": "string", "description": "Retrieve one exact prior Tend request/reply instead of selecting the newest report." },
                    "full": { "type": "boolean", "description": "Return the legacy full request record. Default false: only receipt and reply." }
                },
                "additionalProperties": false
            },
        }),
        json!({
            "name": "tend_set_identity",
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
            "name": "tend_request_attention",
            "description": "Ask the person for a required approval, choice or missing information, including when you would otherwise end your turn on a question only they can answer. One durable request, shown in every Fleet client and the Mac app. body says what and why; options, recommendation and evidence carry the choices, your pick and what to check. Keep the id; its answer arrives as a wake (tend_wait(id)). Timeout or delivery is never approval. Not for progress or peer work.",
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
            "name": "tend_send_message",
            "description": "Send a durable request to a same-window peer. Use review + read_only for an independent critique before finalizing important work; question + read_only for focused analysis; task + execute + narrow paths for bounded, non-overlapping delegated edits. Continue useful work, wait for the structured reply, and verify the result yourself. Targets: peer (only one peer), pane:%N, @alias, or role:<name> (only one matching role).",
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
                    "thread_id": { "type": "string", "description": "Stable id shared by one causal conversation. Omit for a root request; tend assigns its request id. When parent_request_id is set, this must match the parent's canonical thread." },
                    "parent_request_id": { "type": "string", "description": "Exact prior request that caused this follow-up. Tend validates the parent and inherits its canonical thread_id when thread_id is omitted." },
                    "workspace_id": { "type": "string", "description": "Stable workspace/project id used to group collaboration history." },
                    "work_id": { "type": "string", "description": "Durable Work id within the workspace, used to group requests across runs." },
                    "run_id": { "type": "string", "description": "Specific execution/run id for this request." },
                    "artifacts": { "type": "array", "items": { "type": "string" }, "description": "Generic artifact identifiers or locators attached to the request." },
                    "links": { "type": "array", "items": { "type": "string" }, "description": "Generic relationship or URL locators associated with the request." },
                    "user_authorized": { "type": "boolean", "description": "Set true only when the user explicitly told you to deliver this particular request to that session. A protected primary (a human's own session) then takes it without by-id acceptance, at its next turn boundary (the end of its current turn or the user's next prompt), through its provider hook; nothing is typed into its terminal. An exact primary target is kept rather than rerouted to a worker. It asserts the user's instruction to deliver; it grants no execute authority. Recorded on the request." },
                    "withdraw_reply_wakes": { "type": "array", "maxItems": 16, "items": { "type": "string" }, "description": "Your own earlier request ids this request replaces (for example one you gave up on after it was blocked). Their late replies will not wake you; their work, replies and tend_wait_reply results are unchanged. Applied atomically with this send." }
                },
                "required": ["target", "body"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "tend_inbox",
            "description": "Read worker inbox requests, or explicitly accept one request_id on a protected primary session. Primary sessions do not auto-claim queued peer work. Instructions are included; full=true is for diagnostics. Honor kind/work_mode/paths; reply only when requested.",
            "inputSchema": { "type": "object", "properties": {"request_id":{"type":"string","description":"Explicitly accept this one inbound request, including on a protected primary. Use the ID from a wake to avoid rereading other open requests."},"full": {"type":"boolean", "description":"Return legacy full records. Default false."}}, "additionalProperties": false },
        }),
        json!({
            "name": "tend_list_messages",
            "description": "List incoming, sent, or all Tend collaboration requests for this exact agent session without claiming them. Default: newest 20 metadata-only records. Use request_id + include_body for instructions, after_update for progress, or tend_peer_report for replies.",
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
            "name": "tend_reply",
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
            "name": "tend_wait_reply",
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
            "name": "tend_withdraw_reply_wake",
            "description": "Stop being woken for the replies to your own earlier requests that you no longer need, such as ones you gave up on or replaced. Works whatever their status. Only notification interest is withdrawn: the recipient's work, the reply, and tend_wait_reply results are unchanged. Use tend_cancel_message to stop queued work. When sending the replacement, prefer withdraw_reply_wakes on that send instead.",
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
            "name": "tend_cancel_message",
            "description": "Cancel a sent request only while it is still queued. A request already claimed by its recipient cannot be cancelled.",
            "inputSchema": {
                "type": "object",
                "properties": { "request_id": { "type": "string" } },
                "required": ["request_id"],
                "additionalProperties": false
            },
        }),
    ];
    for tool in &mut tools {
        if matches!(
            tool["name"].as_str(),
            Some("tend_send_message" | "tend_call_peer" | "tend_fleet_call_peer")
        ) {
            tool["inputSchema"]["properties"]["task_contract"] =
                tend::collaboration::task_contract_schema();
            tool["inputSchema"]["properties"]["supersedes_request_id"] = json!({"type":"string","description":"Close your read-only request for the same task_id and replace it; never transfers edit authority."});
        }
        if tool["name"] == "tend_request_attention" {
            if let (Some(properties), Value::Object(decision)) = (
                tool["inputSchema"]["properties"].as_object_mut(),
                crate::decision_record::decision_schema_properties(),
            ) {
                properties.extend(decision);
            }
        }
        if tool["name"] == "tend_reply" {
            tool["inputSchema"]["properties"]["report"] = tend::collaboration::task_report_schema();
        }
    }
    tools.extend(crate::agent_records::definitions());
    tools.extend(crate::decision_record::definitions());
    tools.extend(crate::mcp_surface::definitions());
    crate::mcp_surface::extend_schemas(&mut tools);
    tools.retain(|tool| {
        tool["name"]
            .as_str()
            .is_some_and(crate::mcp_surface::is_listed)
    });
    tools
}

/// Execute a `tools/call`. Returns `Err` only for protocol-level problems
/// (missing tool name, malformed params); a tool that runs but fails to do
/// its job returns an `isError` result so the calling model sees the
/// message rather than a transport fault.
#[allow(clippy::too_many_lines)] // one explicit MCP tool dispatch table
async fn call_tool(
    client: &Client,
    params: Option<&Value>,
    config: &tend::config::Config,
) -> Result<Value> {
    let name = params.and_then(|p| p.get("name")).and_then(Value::as_str);
    // Route listed tools onto their handlers; a retired name runs its own
    // handler and its result names the replacement.
    let routed = match name {
        Some(name) => {
            let args = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            match crate::mcp_surface::route(name, args) {
                Ok(routed) => Some((name.to_string(), routed)),
                Err(error) => return Ok(error_result(&error)),
            }
        }
        None => None,
    };
    // Judged on the operation that actually runs, not the name it was
    // called by.
    let (run_name, run_args) = match &routed {
        Some((_, routed)) => (Some(routed.name.as_str()), Some(&routed.args)),
        None => (name, params.and_then(|p| p.get("arguments"))),
    };
    let mutates = matches!(
        run_name,
        Some("tend_start_agent" | "tend_start_work" | "tend_dispatch_work")
    ) || (run_name == Some("tend_manage_tmux")
        && run_args
            .and_then(|args| args.get("action"))
            .and_then(Value::as_str)
            .is_some_and(|action| {
                matches!(action, "terminate_agent" | "close_work" | "close_workspace")
            }));
    let result = match &routed {
        Some((_, routed)) => {
            let mut args = routed.args.clone();
            // Exact delivery from tend_call_peer composes skill/body/context
            // the same way the colleague route does.
            if let Some(object) = args.as_object_mut() {
                if object.remove(crate::mcp_surface::COMPOSE).is_some() {
                    match peer_call_body(&Value::Object(object.clone()), &config.message.skills) {
                        Ok((body, _skill)) => {
                            object.remove("skill");
                            object.remove("context");
                            object.insert("body".into(), json!(body));
                        }
                        Err(error) => return Ok(error_result(&error)),
                    }
                }
            }
            let params = json!({"name": routed.name, "arguments": args});
            // Boxed: the routed dispatch future is large on some targets.
            Box::pin(call_tool_inner(client, Some(&params), config)).await
        }
        None => Box::pin(call_tool_inner(client, params, config)).await,
    };
    let result = match (&routed, result) {
        // tend_guide's default needs this session's identity; launch
        // preferences need none, so return those rather than only an error.
        (Some((old, routed)), Ok(value))
            if old == "tend_guide"
                && routed.name == "tend_collaboration_guide"
                && value["isError"] == true =>
        {
            let mut guide = launch_guide_value(config);
            guide["collaboration_unavailable"] = value["content"][0]["text"].clone();
            Ok(json_result(&guide))
        }
        (Some((old, routed)), Ok(value)) => {
            Ok(crate::mcp_surface::annotate(value, routed.replaced_by, old))
        }
        (_, result) => result,
    };
    if mutates {
        let _ = client.invalidate_agent_directory().await;
    }
    result
}

#[allow(clippy::too_many_lines)] // one explicit MCP tool dispatch table
async fn call_tool_inner(
    client: &Client,
    params: Option<&Value>,
    config: &tend::config::Config,
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

    let name = if name == "tend_request_attention" {
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
        if crate::decision_record::has_decision_fields(object) {
            let stores = match crate::decision_record::daemon_supports(
                client,
                tend::decision::HUMAN_DECISION_CAPABILITY,
            )
            .await
            {
                Ok(stores) => stores,
                Err(error) => return Ok(error_result(&error)),
            };
            if let Err(error) = crate::decision_record::attention_decision(object, stores) {
                return Ok(error_result(&error));
            }
        }
        "tend_send_message"
    } else {
        name
    };

    match name {
        "tend_agent_find" => {
            let Some(query) = args.get("query").and_then(Value::as_str) else {
                return Ok(error_result("query is required"));
            };
            let limit = match args.get("limit") {
                None => 10,
                Some(value) => match value.as_u64().filter(|n| (1..=50).contains(n)) {
                    Some(value) => usize::try_from(value).expect("limit is at most 50"),
                    None => return Ok(error_result("limit must be between 1 and 50")),
                },
            };
            let exclude: Vec<String> = match args.get("exclude_sessions") {
                None => Vec::new(),
                Some(Value::Array(values)) if values.iter().all(Value::is_string) => values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                Some(_) => return Ok(error_result("exclude_sessions must be an array of strings")),
            };
            Ok(
                match crate::agent_find::lookup(
                    client,
                    query,
                    limit,
                    &exclude,
                    args.get("refresh")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
                .await
                {
                    Ok(result) => json_result(&serde_json::to_value(result)?),
                    Err(error) => error_result(&error.to_string()),
                },
            )
        }
        crate::decision_record::FILE_RESULT_TOOL => {
            Ok(match crate::decision_record::call(client, args).await {
                Ok(result) => json_result(&result),
                Err(error) => error_result(&error),
            })
        }
        "tend_agent_search" | "tend_agent_read" => {
            Ok(match crate::agent_records::call(name, args).await {
                Ok(result) => json_result(&result),
                Err(error) => error_result(&error.to_string()),
            })
        }
        "tend_status" => {
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
                    let inputs = tend::active_backends()
                        .into_iter()
                        .map(|backend| {
                            tend::TopologyInput::new(backend.kind(), backend.list_panes())
                        })
                        .collect();
                    tend::TopologySnapshot::build(
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
        "tend_recent_prompts" => Ok(
            match crate::mcp_output::recent_prompts(client, &args).await {
                Ok(value) => json_result(&value),
                Err(error) => error_result(&format!("recent_prompts failed: {error}")),
            },
        ),
        "tend_launch_guide" => Ok(json_result(&launch_guide_value(config))),
        "tend_start_agent" => {
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
                .parse::<tend::collaboration::LaunchPurpose>()
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
                bypass_permissions: purpose != tend::collaboration::LaunchPurpose::Worker
                    || config.collaboration.worker_bypass_permissions,
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
                .is_some_and(|e| e.host == tend::HostKind::Rmux);
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
        "tend_manage_tmux" => {
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
        "tend_send_prompt" => {
            let Some(pane) = args.get("pane").and_then(Value::as_str) else {
                return Ok(error_result("send_prompt requires a `pane` argument"));
            };
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return Ok(error_result("send_prompt requires a `text` argument"));
            };
            for key in ["socket", "session_id"] {
                if args.get(key).is_some_and(|value| !value.is_string()) {
                    return Ok(error_result(&format!(
                        "send_prompt `{key}` must be a string"
                    )));
                }
            }
            // Submit defaults to true — the common case is "prompt and run".
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(true);
            if args.get("force").is_some_and(|value| !value.is_boolean()) {
                return Ok(error_result("send_prompt `force` must be a boolean"));
            }
            // Agents are gated: only an Idle target is typed into unless the
            // caller explicitly forces it.
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            Ok(
                match send_prompt_with_local_cmux(
                    client,
                    pane,
                    text,
                    submit,
                    args.get("socket").and_then(Value::as_str),
                    args.get("session_id").and_then(Value::as_str),
                    force,
                )
                .await
                {
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
        "tend_capture_pane" => {
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
        "tend_fleet_status" => {
            let selector = args.get("selector").and_then(Value::as_str);
            Ok(match client.fleet_snapshot(selector).await {
                Ok(snapshot) => json_result(&crate::mcp_output::fleet_view(
                    &snapshot,
                    args.get("full").and_then(Value::as_bool).unwrap_or(false),
                )),
                Err(error) => error_result(&format!("fleet status failed: {error}")),
            })
        }
        "tend_fleet_capture" => {
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
                    .fleet_execute(host, &tend::FleetOperation::Capture { pane: target })
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
        "tend_fleet_send_prompt" => {
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
            if args.get("force").is_some_and(|value| !value.is_boolean()) {
                return Ok(error_result("fleet_send_prompt `force` must be a boolean"));
            }
            // Agents are gated as locally: only an Idle pane, unless forced.
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            if !force {
                if let Err(error) = ensure_fleet_send_gate(client, host).await {
                    return Ok(error_result(&error));
                }
            }
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
                        &tend::FleetOperation::SendPrompt {
                            pane: target,
                            text: text.to_string(),
                            submit,
                            // MCP tools are called by agents.
                            origin: Some(tend::PromptOrigin::Peer),
                            force: Some(force),
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
        "tend_dispatch_work" => {
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
                    &json!({"state":"unknown","dispatch_id":args["dispatch_id"],"error":e.to_string(),"next_step":"read tend_wait(id) with this ID; do not create a replacement"}),
                )),
            }
        }
        "tend_dispatch_status" => {
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
        "tend_fleet_call_peer" => Ok(fleet_call_peer(client, &args, &config.message.skills).await),
        "tend_fleet_wait_reply" => Ok(fleet_wait_reply(client, &args).await),
        "tend_wait_for_change" => Ok(wait_for_change(client, &args).await),
        "tend_collaboration_guide" => {
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
        "tend_room_context" => {
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(match client.collaboration_context(&origin).await {
                Ok(room) => json_result(&json!(room)),
                Err(error) => error_result(&format!("room context failed: {error}")),
            })
        }
        "tend_call_peer" => Ok(call_peer(client, &args, config).await),
        "tend_start_work" => Ok(start_work(client, &args, config).await),
        "tend_swap_account" => Ok(swap_account(client, &args, config).await),
        "tend_peer_report" => Ok(peer_report(client, &args).await),
        "tend_set_identity" => {
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
        "tend_send_message" => {
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
            if let Err(error) = require_user_authorized_delivery(client, &request).await {
                return Ok(error_result(&error));
            }
            Ok(
                match client
                    .collaboration_send_view(
                        &origin,
                        target,
                        &request,
                        tend::collaboration::CollaborationReadView::Metadata,
                    )
                    .await
                {
                    Ok(request) => json_result(&request_ack(&request)),
                    Err(error) => error_result(&format!("send_message failed: {error}")),
                },
            )
        }
        "tend_inbox" => {
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
                    tend::collaboration::HUMAN_PRIORITY_CAPABILITY,
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
                Ok(requests) => json_result(
                    &json!({ "requests": crate::collaboration_output::inbox(&requests,args.get("full").and_then(Value::as_bool).unwrap_or(false)) }),
                ),
                Err(error) => error_result(&format!("inbox failed: {error}")),
            })
        }
        "tend_list_messages" => {
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
                            tend::collaboration::CollaborationReadView::Progress {
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
                                tend::collaboration::CollaborationReadView::Full
                            } else {
                                tend::collaboration::CollaborationReadView::Outcome
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
            let query = tend::collaboration::CollaborationQuery {
                limit: Some(limit),
                ..Default::default()
            };
            let metadata_only = args.get("full").and_then(Value::as_bool) != Some(true);
            Ok(
                match client
                    .collaboration_query(
                        &origin,
                        mailbox,
                        tend::collaboration::MailboxScope::Caller,
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
        "tend_update_request" | "tend_fleet_update_request" => {
            let Some(request_id) = args.get("request_id").and_then(Value::as_str) else {
                return Ok(error_result("request_id is required"));
            };
            let Some(body) = args.get("body").and_then(Value::as_str) else {
                return Ok(error_result("body is required"));
            };
            if body.trim().is_empty() || body.len() > 8192 {
                return Ok(error_result("body must contain 1..8192 UTF-8 bytes"));
            }
            if name == "tend_fleet_update_request" {
                let Some(host) = args.get("host").and_then(Value::as_str) else {
                    return Ok(error_result("host is required"));
                };
                let pane = match serde_json::from_value::<tend::PaneKey>(
                    args.get("pane_key").cloned().unwrap_or(Value::Null),
                ) {
                    Ok(pane) => pane,
                    Err(error) => return Ok(error_result(&format!("invalid pane_key: {error}"))),
                };
                return Ok(
                    match client
                        .fleet_execute(
                            host,
                            &tend::FleetOperation::CollaborationUpdate {
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
                        tend::collaboration::CollaborationReadView::Metadata,
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
        "tend_reply" => {
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
            let report: Option<tend::collaboration::TaskReport> = match args
                .get("report")
                .map(|v| tend::collaboration::TaskReport::from_input(v.clone()))
                .transpose()
            {
                Ok(v) => v,
                Err(e) => return Ok(error_result(&format!("invalid task report: {e}"))),
            };
            let artifacts = string_array(&args, "artifacts");
            let origin = match resolve_collaboration_origin(client).await {
                Ok(origin) => origin,
                Err(error) => return Ok(error_result(&error)),
            };
            Ok(
                match client
                    .collaboration_reply_report_view(
                        &origin,
                        request_id,
                        status,
                        body,
                        &artifacts,
                        report.as_ref(),
                        tend::collaboration::CollaborationReadView::Metadata,
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
        "tend_wait_reply" => Ok(wait_for_reply(client, &args).await),
        "tend_cancel_message" => {
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
                match client
                    .collaboration_cancel_view(
                        &origin,
                        request_id,
                        tend::collaboration::CollaborationReadView::Metadata,
                    )
                    .await
                {
                    Ok(request) => {
                        json_result(&crate::collaboration_output::mutation_receipt(&request))
                    }
                    Err(error) => error_result(&format!("cancel_message failed: {error}")),
                },
            )
        }
        "tend_withdraw_reply_wake" => {
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
) -> Result<(Option<tend::BackendEndpoint>, Option<String>), String> {
    let mut resolved_origin = resolve_collaboration_origin(client).await;
    if target.is_none_or(|t| tend::backend::pane_id_host_kind(t).is_none())
        && pane_env_is_borrowed()
    {
        let origin = resolved_origin.as_ref().map_err(Clone::clone)?;
        let room = client
            .collaboration_context(origin)
            .await
            .map_err(|error| error.to_string())?;
        resolved_origin = Ok(CollaborationOrigin {
            exact_session: true,
            pane: room.current.pane,
            socket: room.current.socket,
            console: room.current.console,
            agent_session_id: Some(room.current.agent_session_id),
        });
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
    let Some(host) = tend::backend::pane_id_host_kind(pane) else {
        // Session names belong to the caller's server, not the app-server's.
        let endpoint = origin.and_then(|o| {
            let host = tend::backend::pane_id_host_kind(&o.pane)?;
            crate::mux_control::supported(host).then_some(())?;
            Some(tend::BackendEndpoint {
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
    host: tend::HostKind,
    mut sockets: Vec<String>,
) -> Result<tend::BackendEndpoint, String> {
    sockets.retain(|s| !s.trim().is_empty());
    sockets.sort();
    sockets.dedup();
    if sockets.len() != 1 {
        return Err("cannot resolve a unique lifecycle server for this pane; refresh room context and use an exact tracked target".into());
    }
    Ok(tend::BackendEndpoint {
        host,
        socket: sockets.remove(0),
    })
}

/// Keep the default fleet view useful for routing without embedding every
/// agent's prompts, responses, workload tree, and backend metadata. Callers
/// can request `full=true` when they genuinely need the canonical topology.
fn topology_summary(topology: &tend::TopologySnapshot) -> Value {
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
                .and_then(tend::backend::pane_id_host_kind);
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
        "next_step": "Use tend_status with pane for details, or full=true only for the complete topology."
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
    host: Option<tend::HostKind>,
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
    config: &tend::config::Config,
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
            "reported_status": "Recipient-declared completed/blocked/declined/failed. Tend does not judge the work.",
            "timeout": "The wait ended; work is unchanged. Prefer completion wakes or substantive checkpoints. If waiting in the foreground, use 30-60 seconds; a 1-second probe is for an expected result, not a polling loop. Do not renew waits blindly.",
            "progress": "Use update_request (CLI: msg update REQUEST BODY) for consolidated guidance/progress on the same request; reply once at the end. No receipt acknowledgements or unchanged status messages.",
            "abandon": "When you give up on or replace your own request, name it in withdraw_reply_wakes on the replacement (CLI: msg send --withdraw REQUEST), or call tend_withdraw_reply_wake. Its late reply then stays readable but does not wake you. Nothing else changes.",
            "reads": "Reports omit original requests. Lists default to 20 metadata records; request_id + include_body reads instructions, full=true opts into legacy records.",
            "cli": "tend msg wait REQUEST --body [--output FILE] prints the reply directly. Keep exit status/stderr; do not hide errors with grep or retry sends after empty filtered output. --notify means true; --notify false is quiet. Never judge completion from body keywords.",
            "authority": "execute=true and bypass-permission spawning require existing explicit authorization. Preserve request identity on retry; a timeout never authorizes duplicate work.",
            "fleet": "Use exact returned host/pane_key/request_id. Observe hosts cannot execute. Resume yielded host tool cells instead of duplicating waits."
        },
        "next_step": next_step,
        "workflows": {
            "peer_interruption": {
                "wait_slice_secs": 60,
                "owner": "The coordinator owns recovery; a capped peer may never send a reply. Set an overall wait budget; never renew timed-out waits blindly.",
                "health": "Read the durable reply first, then fresh tend_status for the exact recipient agent/session/socket or tend_status(host=all) for Fleet host health (a pane capture with host+pane shows its screen, not a fresh recipient state). Request to.state is stale. Missing/offline is unknown availability, not proof of a cap or process exit.",
                "evidence": ["state", "rate_limit_scope", "rate_limit_source", "rate_limited_until", "last_notification"],
                "trigger": "Known cap/error/stopped, repeated unavailability, or exhausted overall budget requires a recovery decision. Utilization alone is insufficient; reset time is only a recheck hint.",
                "recovery": "Record one update with evidence/artifacts. Use authorized independent work, one bounded reset recheck, or a healthy peer. Verify current health and progress; do not repeatedly prompt the capped agent.",
                "handoff": "Cancel queued local requests with tend_update_request(action=cancel) and verify success. Claimed requests need supported authorized stop/handoff or isolated replacement work; a cap/offline state does not prevent resumption. Preserve request/thread identities and never impersonate the peer.",
                "human": "Only if a real choice/approval/information is needed, follow human_feedback with one linked request; reuse its ID and existing authorization. Preserve remote endpoint identity.",
                "completion": "Do not fabricate the peer reply. If ending your own incoming attempt, reply blocked with dependency/handoff evidence; keep it open during an active human decision.",
                "limitation": "Upgraded daemons return peer_interrupted without completing work. A running coordinator handles recovery first; if unavailable or unacknowledged for two minutes the daemon creates one durable human action. Reuse interruption.action_request_id; inspect interruption.decision without interpreting completion as consent. Older hosts still need bounded health checks."
            },
            "human_feedback": {
                "when": "Only when an actual operator approval, choice, or missing information is required; never for routine peer traffic or already-authorized work.",
                "tool": "tend_request_attention",
                "request": { "action": "choice" },
                "actions": ["approval", "choice", "information"],
                "context": "Include the exact decision, why required, options and recommendation. Include parent_request_id only for an existing request you participate in; inherit its thread and preserve known work identity.",
                "wait": "Retain the returned request_id; its answer arrives as a wake (tend_wait(id) to block). Continue independent work; never treat a timeout as consent or create duplicate questions. Keep the original peer request open while waiting.",
                "answer": "The human answers in Need you (watch: e). Read status and body; completed is not necessarily approval. Honor refusal/cancellation and complete the original peer request after authorized work.",
                "identity": "Never use console=true or claim/answer the operator inbox as an agent.",
                "fallback": "If capabilities.human_actions is false or tend_request_attention is not installed, ask in the existing user conversation. Do not silently drop the question or redirect it to a peer."
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
                "after_sending": "Continue any independent checks, read the reply when it wakes you (tend_inbox(view=report, request_id) or tend_wait(id)), then verify and address the findings."
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
            "ongoing_fleet_work": {
                "steps": [
                    "Consolidate verified guidance with tend_update_request on the returned request_id (host/pane_key from another process); do not inject repeated task prompts.",
                    "Report nonterminal progress with tend_update_request on the same request, never by addressing pane:console.",
                    "At agreed checkpoints read tend_inbox(view=incoming); updates do not interrupt terminal input or widen execution authority.",
                    "Wait with after_update=0 for progress, then resume with next_after_update; finish with exactly one terminal reply."
                ]
            },
            "incoming_request": {
                "steps": [
                    "Call tend_inbox to claim/read unless the complete request is already claimed and delivered in the prompt.",
                    "Honor kind, work_mode, and paths; read_only never authorizes edits.",
                    "Only when expects_reply=true, reply exactly once with completed, blocked, declined, or failed and useful artifacts. Never acknowledge one-way notices."
                ]
            }
        },
        "guardrails": [
            "Use room.self for identity; never derive it from TMUX_PANE or raw tmux commands, which may belong to a shared app-server.",
            "Use Tend lifecycle tools instead of raw tmux; name Fleet host/pane explicitly and respect observe mode.",
            "Never infer GitHub/PR work without an explicit PR number or URL.",
            "One-way notices need no acknowledgement; notify=true is for blockers, decisions, conflicts or handoffs. Batch progress at checkpoints.",
            "For review use the opposite provider: Codex to Claude, Claude to Codex; an explicit user selection takes precedence.",
            "Human sessions default to primary and are protected from peer injection. High-level calls reuse or create workers, including when a primary is named. When the user explicitly tells you to deliver a request to a session, pass user_authorized=true: a primary then takes it at its next turn boundary through its provider hook, without being typed into; never ask the user to accept it again on the target. It confers no execute authority. Set context_bound=true only for its exact conversation; those primary requests stay queued. Accept a specific request with tend_inbox(request_id) only when the human task permits it. Spawned peers use worker purpose with configured provider defaults, not a cloned conversation.",
            "Claim requests with tend_inbox unless already claimed and delivered in full. Honor kind/work_mode/paths; read_only never authorizes edits. Reply once only when expects_reply=true; progress uses update_request.",
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
    /// The registered peer, when one exists. `None` for a pane tend just
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
/// the reply, matching `tend_wait_reply` semantics.
async fn peer_report(client: &Client, args: &Value) -> Value {
    let origin = match resolve_collaboration_origin(client).await {
        Ok(origin) => origin,
        Err(error) => return error_result(&error),
    };
    let view = if args.get("full").and_then(Value::as_bool).unwrap_or(false) {
        tend::collaboration::CollaborationReadView::Full
    } else {
        tend::collaboration::CollaborationReadView::Outcome
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
                "Tend request {request_id:?} has no final reply yet"
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
            "no received Tend peer report matches {target:?}; use tend_call_peer only if the user wants a new request"
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
    let mut query = tend::collaboration::CollaborationQuery {
        limit: Some(20),
        ..Default::default()
    };
    loop {
        let result = client
            .collaboration_query_result(
                origin,
                RequestMailbox::Sent,
                tend::collaboration::MailboxScope::Caller,
                &query,
                0,
                tend::collaboration::CollaborationReadView::Metadata,
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
        || target.eq_ignore_ascii_case("@tend-peer")
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
    // The user asked for this exact session: deliver there, not to a worker.
    let user_authorized = args
        .get("user_authorized")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut worker_program = None;
    if !context_bound && !user_authorized {
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
    if target.starts_with("role:") || tend::collaboration::parse_slot_path(target).is_some() {
        return true;
    }
    let Some(alias) = target
        .strip_prefix('@')
        .or_else(|| target.strip_prefix("alias:"))
    else {
        return false;
    };
    !matches!(alias, "peer" | "tend-peer")
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
    view: tend::collaboration::CollaborationReadView,
) -> Result<tend::collaboration::CollaborationRequest, tend::ipc::RuntimeError> {
    if logical {
        match client
            .collaboration_send_view(origin, target, request, view)
            .await
        {
            Ok(sent) => return Ok(sent),
            // Only an explicit refusal proves nothing was stored. After a
            // lost or garbled response the request may exist, and a second
            // send would duplicate the work.
            Err(tend::ipc::RuntimeError::Daemon(_)) if pane_selector.is_some() => {}
            Err(error) => return Err(error),
        }
    }
    let selector = pane_selector.unwrap_or(target);
    client
        .collaboration_send_view(origin, selector, request, view)
        .await
}

/// One high-level colleague call behind the conversational `@peer` protocol.
/// The model still owns understanding the user's sentence; tend owns every
/// deterministic and security-sensitive step after that: prompt expansion,
/// target selection, contract construction, optional explicit spawn, durable
/// delivery, and reply correlation.
#[allow(clippy::too_many_lines)] // keep the security-sensitive call sequence linear and auditable
pub(crate) async fn call_peer(
    client: &Client,
    args: &Value,
    config: &tend::config::Config,
) -> Value {
    for key in ["context_bound", "spawn_if_missing", "user_authorized"] {
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
    let paths = string_array(args, "paths");
    let request = match new_request_from_args(args, kind, body, true, work_mode, paths) {
        Ok(request) => request,
        Err(error) => return error_result(&error),
    };
    if request.task_contract.is_some() {
        if let Err(error) = crate::require_capability(
            client,
            tend::collaboration::TASK_CONTRACT_CAPABILITY,
            "contracted peer request",
        )
        .await
        {
            return error_result(&error.to_string());
        }
    }
    if let Err(error) =
        crate::mcp_output::require_reply_wake_withdraw(client, &request.withdraw_reply_wakes).await
    {
        return error_result(&error);
    }
    if let Err(error) = require_user_authorized_delivery(client, &request).await {
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
    let path_target = tend::collaboration::parse_slot_path(target).is_some();
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
            tend::collaboration::WorkerSpawnAcquisition::Draining {
                drain_id,
                expires_at,
            } => {
                return json_result(
                    &json!({"sent":false,"reason":"worker_draining","drain_id":drain_id,"expires_at":expires_at,"next_step":"Retry after the worker drain completes; no replacement was spawned."}),
                );
            }
            tend::collaboration::WorkerSpawnAcquisition::Reuse { peer }
            | tend::collaboration::WorkerSpawnAcquisition::Pending {
                peer: Some(peer), ..
            } => {
                selection = Some(PeerSelection::registered(
                    peer,
                    "daemon worker capacity reuse".into(),
                ));
            }
            tend::collaboration::WorkerSpawnAcquisition::Pending {
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
            tend::collaboration::WorkerSpawnAcquisition::LaunchToken { ticket } => {
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
                                &tend::collaboration::WorkerSpawnAction::Uncertain {
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
                        let failed = phase == Some(tend::collaboration::WorkerSpawnPhase::Failed);
                        return json_result(&json!({
                            "sent":false, "reason":if failed { "worker_spawn_failed" } else { "launch_pending" }, "launch_id":ticket.launch_id,
                            "phase":phase, "error":error, "next_step":if failed { "Reservation closed; correct the reported failure and retry." } else { "Reconcile or explicitly release this launch before creating another worker." },
                        }));
                    }
                }
            }
        }
    }

    if let Some(selected) = &selection {
        let host = tend::backend::pane_id_host_kind(&selected.pane);
        if let Some(host) = host.filter(|host| crate::mux_control::supported(*host)) {
            let socket = selected
                .peer
                .as_ref()
                .and_then(|peer| peer.socket.clone())
                .or_else(|| room.current.socket.clone());
            let endpoint =
                match select_lifecycle_endpoint(host, socket.clone().into_iter().collect()) {
                    Ok(endpoint) => endpoint,
                    Err(error) => return error_result(&error),
                };
            let pane = selected
                .pane
                .strip_prefix("rmux:")
                .unwrap_or(&selected.pane);
            let observed = crate::mux_control::Control::Endpoint(endpoint.clone()).output(&[
                "display-message",
                "-p",
                "-t",
                pane,
                "#{@tend_worker_launch}\t#{@tend_agent}\t#{@tend_input_readiness}",
            ]);
            let (mut allowed, launch_id) =
                worker_dispatch_readiness(observed.as_deref().map_err(|_| ()));
            // A person may have resolved the retained worker's setup dialog.
            // Prove its composer ready without accepting any further dialog.
            let recover = blocked_worker_needs_probe(observed.as_deref().map_err(|_| ()));
            if !allowed
                && (recover
                    || legacy_worker_needs_probe(
                        observed.as_deref().map_err(|_| ()),
                        selected
                            .peer
                            .as_ref()
                            .is_some_and(|peer| peer.state == tend::event::AgentState::Idle),
                    ))
            {
                if let (Some(launch), Some(cwd)) = (
                    launch_id.clone(),
                    selected.peer.as_ref().and_then(|peer| peer.cwd.clone()),
                ) {
                    let pane = pane.to_owned();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::mux_control::with_endpoint(Some(endpoint), || {
                            if recover {
                                crate::worker_startup::probe(
                                    &pane,
                                    std::path::Path::new(&cwd),
                                    &launch,
                                )
                            } else {
                                crate::worker_startup::prepare(
                                    &pane,
                                    std::path::Path::new(&cwd),
                                    &launch,
                                    false,
                                )
                            }
                        })
                    })
                    .await;
                    allowed = matches!(result, Ok(Ok(report)) if report.state == tend::worker_startup::State::Ready);
                }
            }
            if !allowed {
                return json_result(&json!({"sent": false, "reason": "worker_not_input_ready",
                    "pane": selected.pane, "socket": socket, "launch_id": launch_id,
                    "next_step": "Run tend agent prepare with this pane, socket, launch id and its authorized cwd before retrying; no task was sent."}));
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
    let logical = !context_bound
        && !request.user_authorized
        && (path_target || (worker_program.is_none() && names_a_slot(target)));
    // Adaptive synchronous output needs the exact instruction to classify short calls.
    let adaptive_reply =
        config.collaboration.wake_payload == tend::config::CollaborationWakePayload::Adaptive;
    let sent = match send_peer_call(
        client,
        &origin,
        target,
        logical,
        pane_selector.as_deref(),
        &request,
        if args.get("wait").and_then(Value::as_bool).unwrap_or(true) && adaptive_reply {
            tend::collaboration::CollaborationReadView::Full
        } else {
            tend::collaboration::CollaborationReadView::Metadata
        },
    )
    .await
    {
        Ok(request) => request,
        Err(error) => return error_result(&format!("call_peer send failed: {error}")),
    };
    let peer_pending = tend::collaboration::is_pending_session(&sent.to.agent_session_id);
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
    if !sent.bypasses_primary_protection() && !sent.to.accepts_peer_work() {
        common["final_reply_received"] = json!(false);
        common["reason"] = json!("deferred_primary");
        common["next_step"] = json!("Target is a protected human session. Use a worker; explicit by-id acceptance remains available on the target.");
        return json_result(&common);
    }
    if peer_pending {
        common["peer_pending"] = json!(true);
        common["delivery"] = json!(format!(
            "pane {} has no registered agent session yet; tendd delivers this request as soon as \
             that pane reports idle, which needs tend to see an agent process there. Wait on \
             request_id {} with tend_wait — never watch the pane with capture-pane or \
             sleep. If the reply times out, check `tend status` for a row on the pane.",
            sent.to.pane, sent.id
        ));
    }
    if !args.get("wait").and_then(Value::as_bool).unwrap_or(true) {
        let mut payload = common;
        payload["final_reply_received"] = json!(false);
        payload["reason"] = json!("sent_without_waiting");
        return json_result(&payload);
    }

    let compact_short_reply =
        adaptive_reply && tend::collaboration::instruction::short_inline_prompt(&sent).is_some();
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
            if compact_short_reply && request.status.is_terminal() && request.reply.is_some() {
                payload = crate::collaboration_output::short_call_outcome(payload, &request);
            }
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
                "the request remains durable for {}; call tend_wait with request_id {} to keep \
                 waiting, or tend_inbox(view=report) later. Do not poll the pane.",
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
    node_id: &tend::NodeId,
    pane: &tend::PaneKey,
    request: &NewRequest,
) -> anyhow::Result<()> {
    use crate::owner_only::OwnerOnly;
    use std::io::Write;
    let root =
        tend::paths::default_node_id_file().ok_or_else(|| anyhow::anyhow!("no data directory"))?;
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
    let paths = string_array(args, "paths");
    let mut request = match new_request_from_args(args, kind, body, true, work_mode, paths) {
        Ok(request) => request,
        Err(error) => return error_result(&error),
    };
    if !request.withdraw_reply_wakes.is_empty() {
        // The withdrawn requests live in this host's mailbox and the new one
        // in the remote host's; there is no transaction spanning both.
        return error_result(
            "fleet_call_peer cannot withdraw reply wakes; call tend_update_request(action=withdraw_wake) on the host that holds those requests",
        );
    }

    if let Ok(origin) = resolve_collaboration_origin(client).await {
        if let Ok(room) = client.collaboration_context(&origin).await {
            request.initiator = Some(Box::new(room.current));
        }
    }
    if request.task_contract.is_some() && request.initiator.is_none() {
        return error_result(
            "contracted Fleet dispatch requires a resolved initiating agent session",
        );
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
    if request.task_contract.is_some()
        && !snapshot.hosts.iter().any(|h| {
            h.alias == host
                && h.capabilities
                    .iter()
                    .any(|c| c == tend::collaboration::TASK_CONTRACT_CAPABILITY)
        })
    {
        return error_result(
            "target must advertise collaboration_task_contract_v1 before contracted dispatch",
        );
    }
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
        return error_result("target must advertise collaboration_dispatch_v1; upgrade its tend and tendd before durable dispatch");
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
            &tend::FleetOperation::CollaborationSend {
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
    if let Ok(pane_key_value) = serde_json::to_value(&pane_key) {
        crate::mcp_surface::remember_fleet_request(&sent.id, host, &pane_key_value);
    }
    let common = json!({
        "dispatch_id": dispatch_id,
        "sent": true,
        "host": host,
        "pane_key": pane_key,
        "selected_peer": sent.to,
        "initiator": sent.initiator,
        "progress_route": {"tool":"tend_update_request", "request_id":sent.id},
        "guidance_route": {"tool":"tend_update_request", "host":host, "pane_key":pane_key, "request_id":sent.id},
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
                "tool": "tend_wait",
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
                "tool": "tend_wait",
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
    let pane_key: tend::PaneKey = match serde_json::from_value(pane_key_value.clone()) {
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

/// A gated Fleet prompt needs the host (its relay, or the local daemon) to
/// enforce `send_prompt_state_gate_v1`; an older one would type regardless.
async fn ensure_fleet_send_gate(
    client: &Client,
    host_alias: &str,
) -> std::result::Result<(), String> {
    let gate = tend::fleet::FLEET_SEND_PROMPT_GATE_CAPABILITY;
    // Both hops: this controller must carry `force`, and the host enforce it.
    let hello = client
        .hello(Duration::from_secs(3))
        .await
        .map_err(|error| error.to_string())?;
    if !hello
        .capabilities
        .iter()
        .any(|capability| capability == gate)
    {
        return Err(format!(
            "{}: this tendd cannot forward the idle gate to Fleet hosts (no {gate}); nothing was sent. Upgrade/restart tendd, or pass force=true.",
            tend::prompt_delivery::TARGET_NOT_IDLE
        ));
    }
    let snapshot = client
        .fleet_snapshot(None)
        .await
        .map_err(|error| format!("could not read Fleet status: {error}"))?;
    let host = snapshot
        .hosts
        .into_iter()
        .find(|host| host.alias == host_alias)
        .ok_or_else(|| format!("Fleet host '{host_alias}' is not known"))?;
    if host.local
        || host
            .capabilities
            .iter()
            .any(|capability| capability == gate)
    {
        Ok(())
    } else {
        Err(format!(
            "{}: host '{host_alias}' cannot check that the target is idle (no {gate}); nothing was sent. Upgrade tend there, or pass force=true to type regardless.",
            tend::prompt_delivery::TARGET_NOT_IDLE
        ))
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
    mode: tend::HostAccessMode,
    capabilities: &[String],
) -> std::result::Result<(), String> {
    if !local && mode != tend::HostAccessMode::Control {
        return Err(format!(
            "host '{host_alias}' is observe-only; explicitly grant mode='control' before an agent may call it"
        ));
    }
    for capability in ["collaboration", "collaboration_get"] {
        if !capabilities.iter().any(|candidate| candidate == capability) {
            return Err(format!(
                "host '{host_alias}' does not advertise '{capability}'; connect it and upgrade tend on that host"
            ));
        }
    }
    Ok(())
}

async fn await_fleet_collaboration_reply(
    client: &Client,
    host: &str,
    pane: &tend::PaneKey,
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
        let operation = tend::FleetOperation::CollaborationGet {
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

/// `tend work up` over MCP.
///
/// Resolution is async (it may spend a headless agent turn looking the
/// ticket up); everything after it shells out to tmux and is therefore
/// handed to `spawn_blocking` rather than run on the reactor.
async fn start_work(client: &Client, args: &Value, config: &tend::config::Config) -> Value {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
    let Some(work) = text("work") else {
        return error_result("tend_start_work requires a work argument");
    };
    let up = crate::work_up::UpArgs {
        body_from_plan: false,
        planned_cwd: None,
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
        Err(error) => return error_result(&format!("tend_start_work failed: {error}")),
    };
    let result = if up.dry_run {
        match tokio::task::spawn_blocking(move || {
            crate::mux_control::with_endpoint(endpoint, || crate::work_up::apply(resolved, true))
        })
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return error_result(&format!("{error:#}")),
            Err(error) => return error_result(&format!("tend_start_work worker failed: {error}")),
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
            Err(error) => return error_result(&format!("tend_start_work worker failed: {error}")),
        }
    };
    match serde_json::to_value(&result) {
        Ok(value) => json_result(&value),
        Err(error) => error_result(&format!("tend_start_work could not serialize: {error}")),
    }
}

/// Adapter over [`tend::request::compose`]: read the JSON-RPC argument
/// shape and turn "nothing supplied" into the error a peer call needs.
/// `tend work up` shares the composer but not that rule — a pipeline can
/// legitimately be driven by its ticket alone.
fn peer_call_body(
    args: &Value,
    message_skills: &BTreeMap<String, String>,
) -> std::result::Result<(String, Option<String>), String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let composed = tend::request::compose(
        tend::request::RequestParts {
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
        || target.eq_ignore_ascii_case("@tend-peer")
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
        // error here: tendd resolves a tend-launched pane whose agent has not
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
    target.starts_with('%') || tend::backend::pane_id_host_kind(target).is_some()
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
        AgentKind::Antigravity => 2,
        AgentKind::Opencode => 3,
        AgentKind::Task => 4,
        AgentKind::Unknown => 5,
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
        "agy" | "antigravity" => Some(AgentKind::Antigravity),
        "opencode" => Some(AgentKind::Opencode),
        _ => None,
    }
}

/// Review routing defaults to the other provider; explicit routing remains authoritative.
fn review_provider_target(target: &str, kind: RequestKind, caller: AgentKind) -> &str {
    let automatic = matches!(
        target.trim().to_ascii_lowercase().as_str(),
        "" | "auto" | "peer" | "@peer" | "@tend-peer"
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
    guide: &tend::config::McpGuideConfig,
) -> std::result::Result<crate::agent_launch::AgentProgram, String> {
    let requested = args
        .get("spawn_agent")
        .and_then(Value::as_str)
        .map(crate::agent_launch::AgentProgram::parse)
        .transpose()?;
    let targeted = provider_kind(target).map(agent_program_for_kind);
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
        AgentKind::Antigravity | AgentKind::Opencode | AgentKind::Task | AgentKind::Unknown => {
            crate::agent_launch::AgentProgram::Codex
        }
    };
    if let Some(chosen) = requested.or(targeted) {
        return Ok(chosen);
    }
    // The configured default is consulted only when the call names no
    // provider, so a stale one (a retired `gemini`) never blocks an explicit
    // choice.
    let configured = guide
        .agent
        .as_deref()
        .map(crate::agent_launch::AgentProgram::parse)
        .transpose()?;
    Ok(configured.unwrap_or(fallback))
}

fn agent_program_for_kind(kind: AgentKind) -> crate::agent_launch::AgentProgram {
    match kind {
        AgentKind::ClaudeCode => crate::agent_launch::AgentProgram::Claude,
        AgentKind::Codex | AgentKind::Task | AgentKind::Unknown => {
            crate::agent_launch::AgentProgram::Codex
        }
        AgentKind::Antigravity => crate::agent_launch::AgentProgram::Antigravity,
        AgentKind::Opencode => crate::agent_launch::AgentProgram::Opencode,
    }
}

/// The provider arguments an MCP launch should carry.
///
/// A caller-supplied `options` array wins; otherwise the daemon's own
/// configuration answers through [`tend::config::Config::launch_options`],
/// which is keyed by provider — so this no longer has to guard against
/// handing one CLI's model name to another.
fn launch_options(
    args: &Value,
    config: &tend::config::Config,
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

fn legacy_worker_needs_probe(
    observed: std::result::Result<&str, ()>,
    registered_idle: bool,
) -> bool {
    let Ok(observed) = observed else {
        return false;
    };
    let fields: Vec<_> = observed
        .trim_end_matches(['\r', '\n'])
        .splitn(3, '\t')
        .collect();
    registered_idle
        && fields.len() == 3
        && !fields[0].is_empty()
        && fields[2].is_empty()
        && matches!(
            tend::discovery::classify_command(fields[1]),
            Some(tend::event::AgentKind::ClaudeCode | tend::event::AgentKind::Codex)
        )
}

// Missing metadata on an old/primary pane remains compatible. A marked,
// supported worker must positively prove ready; lookup errors never authorize.
fn blocked_worker_needs_probe(observed: std::result::Result<&str, ()>) -> bool {
    let Ok(observed) = observed else {
        return false;
    };
    let fields: Vec<_> = observed
        .trim_end_matches(['\r', '\n'])
        .split('\t')
        .collect();
    fields.len() == 3
        && !fields[0].is_empty()
        && fields[2] == "blocked"
        && matches!(
            tend::discovery::classify_command(fields[1]),
            Some(tend::event::AgentKind::ClaudeCode | tend::event::AgentKind::Codex)
        )
}

fn worker_dispatch_readiness(observed: std::result::Result<&str, ()>) -> (bool, Option<String>) {
    let Ok(observed) = observed else {
        return (false, None);
    };
    let mut parts = observed.trim_end_matches(['\r', '\n']).splitn(3, '\t');
    let launch = parts.next().unwrap_or_default();
    let kind = parts.next().unwrap_or_default();
    let state = parts.next().unwrap_or_default();
    let supported = matches!(
        tend::discovery::classify_command(kind),
        Some(tend::event::AgentKind::ClaudeCode | tend::event::AgentKind::Codex)
    );
    (
        launch.is_empty() || (!supported && !kind.is_empty()) || state == "ready",
        (!launch.is_empty()).then(|| launch.to_owned()),
    )
}

/// Explicit peer input paths become bounded provider context directories before
/// startup. This does not change the provider's tool approval mode or the
/// request's read-only contract. Missing future output paths grant nothing.
fn peer_context_options(
    program: crate::agent_launch::AgentProgram,
    mut options: Vec<String>,
    args: &Value,
    cwd: Option<&str>,
) -> std::result::Result<Vec<String>, String> {
    use crate::agent_launch::AgentProgram;
    // Codex --add-dir grants additional write roots, not read context.
    if program != AgentProgram::Claude {
        return Ok(options);
    }
    let base = cwd
        .map(std::path::PathBuf::from)
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|e| e.to_string())?;
    let base = tend::paths::canonicalize(&base).map_err(|e| e.to_string())?;
    let home = dirs::home_dir().and_then(|p| tend::paths::canonicalize(&p).ok());
    let mut roots = std::collections::BTreeSet::new();
    for path in args
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = path.as_str().ok_or("peer paths must be strings")?;
        let path = std::path::Path::new(path);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            base.join(path)
        };
        let Ok(path) = tend::paths::canonicalize(&path) else {
            continue;
        };
        // A file is not authority for its entire parent directory. These are
        // advisory input paths: skip missing/private/root/file entries.
        if path.is_dir()
            && safe_peer_context_directory(&path, home.as_deref())
            && !path.starts_with(&base)
        {
            roots.insert(path);
        }
    }
    let at = options
        .iter()
        .position(|a| a == "--")
        .unwrap_or(options.len());
    let extra = roots
        .into_iter()
        .take(32)
        .map(|dir| format!("--add-dir={}", dir.to_string_lossy()));
    options.splice(at..at, extra);
    Ok(options)
}

fn safe_peer_context_directory(
    directory: &std::path::Path,
    home: Option<&std::path::Path>,
) -> bool {
    if directory.parent().is_none() {
        return false;
    }
    let Some(home) = home else {
        return false;
    };
    if directory == home {
        return false;
    }
    if let Ok(relative) = directory.strip_prefix(home) {
        if let Some(first) = relative.components().next() {
            let name = first.as_os_str().to_string_lossy();
            if name.starts_with('.')
                || name.eq_ignore_ascii_case("Library")
                || name.eq_ignore_ascii_case("AppData")
            {
                return false;
            }
        }
    }
    true
}

async fn acquire_peer_capacity(
    client: &Client,
    origin: &CollaborationOrigin,
    args: &Value,
    target: &str,
    program: crate::agent_launch::AgentProgram,
) -> std::result::Result<tend::collaboration::WorkerSpawnAcquisition, String> {
    let hello = client
        .hello(Duration::from_secs(3))
        .await
        .map_err(|e| e.to_string())?;
    if !hello
        .capabilities
        .iter()
        .any(|cap| cap == tend::collaboration::WORKER_SPAWN_CAPABILITY)
    {
        return Err("automatic worker creation requires tendd with collaboration_worker_spawn_v1; upgrade/restart tendd and run `tend doctor` to check daemon and MCP version skew".into());
    }
    let spec = tend::collaboration::WorkerSpawnSpec {
        agent_kind: tend::discovery::classify_command(program.label())
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
        if !matches!(&acquisition, tend::collaboration::WorkerSpawnAcquisition::Pending { peer: None, phase, .. } if *phase != tend::collaboration::WorkerSpawnPhase::Uncertain)
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

#[allow(clippy::too_many_lines)] // reservation, launch and registration form one worker lifecycle
async fn spawn_peer(
    client: &Client,
    room: &RoomContext,
    args: &Value,
    target: &str,
    program: crate::agent_launch::AgentProgram,
    config: &tend::config::Config,
    ticket: tend::collaboration::WorkerSpawnTicket,
) -> std::result::Result<(PeerSelection, crate::agent_launch::StartResult), String> {
    let host = tend::backend::pane_id_host_kind(&room.current.pane)
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
        purpose: tend::collaboration::LaunchPurpose::Worker,
        socket: client.socket().to_path_buf(),
        agent: program,
        options: peer_context_options(
            program,
            config.launch_options(program.label(), None),
            args,
            room.current.cwd.as_deref(),
        )?,
        placement: crate::agent_launch::Placement::Pane,
        target: Some(native_pane.to_string()),
        cwd: room.current.cwd.as_deref().map(std::path::PathBuf::from),
        prompt: None,
        name: None,
        workspace: None,
        work: None,
        role: Some(spawned_peer_role(target, args).into()),
        task: Some("tend peer call".into()),
        // A peer spawned for one call is not a pipeline pane, so it stays
        // unaliased and `tend work up` reports it as unclaimed rather than
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
                let _ = client.collaboration_worker_spawn(&ticket, &tend::collaboration::WorkerSpawnAction::Renew).await;
            }
        }
    };
    let _ = client.invalidate_agent_directory().await;
    if host == tend::HostKind::Rmux {
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
            "created {} peer pane {} but it did not register within {timeout_secs}s, and tend has no readiness signal for {} panes; keep the pane, submit one prompt in it so its hooks fire, then call again with target=\"pane:{}\"",
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
    // to send. Address the pane instead: tendd queues the request and delivers
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

/// Can tendd tell that a pane of this program is ready for input before its
/// agent registers?
///
/// That requires the pane to carry a registry row, which comes from discovery
/// classifying the process (`discovery::classify_command`) or from a bundled
/// screen manifest. Every launchable provider has one: opencode is
/// classified by its process name, like the rest.
fn has_pane_readiness_signal(program: crate::agent_launch::AgentProgram) -> bool {
    use crate::agent_launch::AgentProgram;
    match program {
        AgentProgram::Claude
        | AgentProgram::Codex
        | AgentProgram::Antigravity
        | AgentProgram::Opencode => true,
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
            tend::collaboration::CollaborationReadView::Outcome,
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
        tend::adapters::proc_ancestry::under_shared_codex_app_server(std::process::id())
    })
}

/// This process's own `$TMUX_PANE`, or `None` when the pane env was inherited
/// from a shared Codex app-server and so names whichever pane started that
/// server. CLI defaults read the caller's pane through this so a command run
/// from a Codex thread never acts on (or places next to) another pane.
pub(crate) fn own_tmux_pane() -> Option<String> {
    own_pane_from(std::env::var("TMUX_PANE").ok(), pane_env_is_borrowed)
}

/// The backend's current pane (`$TMUX_PANE`, then `$TMUX`, `$ZELLIJ_PANE_ID`,
/// …), under the same rule as [`own_tmux_pane`]: `None` when the pane env was
/// inherited from a shared Codex app-server.
pub(crate) fn own_current_pane() -> Option<String> {
    own_pane_from(tend::default_backend().current_pane(), pane_env_is_borrowed)
}

fn own_pane_from(pane: Option<String>, borrowed: impl FnOnce() -> bool) -> Option<String> {
    let pane = pane.filter(|pane| !pane.trim().is_empty())?;
    (!borrowed()).then_some(pane)
}

/// Resolve collaboration identity at the daemon, where the current session
/// binding is authoritative. Shared app-server pane variables belong to another
/// conversation: never copy them, cache a physical pane, or transfer a full
/// registry snapshot merely to identify the calling thread.
#[allow(clippy::unused_async)] // common async API; shared identity needs no IPC
pub(crate) async fn resolve_collaboration_origin(
    _client: &Client,
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
        let mut origin = current_collaboration_origin()?;
        origin.agent_session_id = session;
        return Ok(origin);
    }
    let session = session.ok_or_else(|| {
        "collaboration cannot identify this Codex thread: shared app-server pane variables belong to another pane and no thread id was supplied".to_owned()
    })?;
    Ok(shared_session_origin(session))
}

fn shared_session_origin(session: String) -> CollaborationOrigin {
    CollaborationOrigin {
        exact_session: true,
        pane: String::new(),
        socket: None,
        console: false,
        agent_session_id: Some(session),
    }
}

pub(crate) fn current_collaboration_origin() -> std::result::Result<CollaborationOrigin, String> {
    let pane = tend::default_backend()
        .current_pane()
        .or_else(current_process_host_pane)
        .ok_or_else(|| {
            "collaboration could not identify this MCP server's pane; \
             native pane variables are unset and process ancestry did not reach a pane shell. \
             For Codex, add env_vars = [\"RMUX\", \"RMUX_PANE\", \"TMUX\", \"TMUX_PANE\", \"TEND_SOCKET\"] \
             under [mcp_servers.tend] and restart Codex"
                .to_string()
        })?;
    let endpoint = match tend::backend::pane_id_host_kind(&pane) {
        Some(tend::HostKind::Cmux) => Some(tend::backend::cmux::endpoint_from_env()),
        Some(tend::HostKind::Rmux) => std::env::var("RMUX").ok(),
        Some(tend::HostKind::Tmux) => std::env::var("TMUX").ok(),
        Some(tend::HostKind::Zellij | tend::HostKind::Herdr) | None => None,
    };
    let socket = endpoint.and_then(|value| {
        let path = value.split(',').next()?.trim();
        (!path.is_empty()).then(|| tend::backend::pane_endpoint_identity(Some(&pane), path))
    });
    // An MCP caller *is* the agent in the pane, so it keeps the pane identity:
    // replies must route back to it and wake it.
    Ok(CollaborationOrigin {
        exact_session: false,
        pane,
        socket,
        console: false,
        agent_session_id: None,
    })
}

/// Recover the owning pane when an MCP host sanitizes subprocess
/// environment variables. Current Codex releases only forward variables
/// explicitly listed in `mcp_servers.<name>.env_vars`, so an existing tend
/// registration may launch without the host's native variables even though
/// Codex itself is running in a pane.
///
/// The MCP process is still a descendant of the pane shell. Match the first
/// ancestor whose PID appears in an active backend's pane inventory. Explicit
/// env remains authoritative because it also identifies the endpoint without
/// ambiguity.
fn current_process_host_pane() -> Option<String> {
    let pane_pids = tend::active_backends()
        .into_iter()
        .flat_map(|backend| backend.pane_pid_map())
        .collect::<HashMap<_, _>>();
    pane_from_ancestry(std::process::id(), &pane_pids, |pid| {
        tend::adapters::proc_ancestry::parent_pid(pid)
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
        tend::adapters::proc_ancestry::ancestor_in_set(start_pid, &candidates, parent_of)?;
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
/// prevents `tend_call_peer` and `tend_send_message` from silently diverging.
/// A user-authorized request sent to a daemon that ignores the flag would
/// sit queued on the protected primary, the very outcome the user overrode.
async fn require_user_authorized_delivery(
    client: &Client,
    request: &NewRequest,
) -> std::result::Result<(), String> {
    if !request.user_authorized {
        return Ok(());
    }
    crate::require_capability(
        client,
        tend::collaboration::USER_AUTHORIZED_DELIVERY_CAPABILITY,
        "user-authorized delivery",
    )
    .await
    .map_err(|error| error.to_string())
}

fn new_request_from_args(
    args: &Value,
    kind: RequestKind,
    body: String,
    expects_reply: bool,
    work_mode: WorkMode,
    paths: Vec<String>,
) -> std::result::Result<NewRequest, String> {
    let task_contract: Option<tend::collaboration::TaskContract> = args
        .get("task_contract")
        .map(|v| tend::collaboration::TaskContract::from_input(v.clone()))
        .transpose()
        .map_err(|e| format!("invalid task_contract: {e}"))?;
    if let Some(contract) = &task_contract {
        contract.validate().map_err(|e| e.to_string())?;
    }
    let supersedes_request_id = optional_string(args, "supersedes_request_id")?;
    if supersedes_request_id.is_some() && task_contract.is_none() {
        return Err("supersession requires task_contract".into());
    }
    if task_contract.is_some()
        && (!expects_reply || kind == RequestKind::Notice || args.get("human_action").is_some())
    {
        return Err("task_contract requires a reply-bearing agent request".into());
    }
    Ok(NewRequest {
        decision: crate::decision_record::request_decision(args)?,
        task_contract,
        supersedes_request_id,
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
        withdraw_reply_wakes: request_id_array(args, "withdraw_reply_wakes")?.unwrap_or_default(),
        recipient_slot: None,
        user_authorized: match args.get("user_authorized") {
            None => false,
            Some(Value::Bool(authorized)) => *authorized,
            Some(_) => return Err("user_authorized must be a boolean".into()),
        },
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
                tend::collaboration::CollaborationReadView::Full
            } else {
                tend::collaboration::CollaborationReadView::Outcome
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
/// every request through tendd would make the default secure mode unusable.
/// A failed first text injection is safe to retry through the daemon; once text
/// lands, submit is reported separately and the text is never resent.
async fn send_prompt_with_local_cmux(
    client: &Client,
    pane: &str,
    prompt: &str,
    submit: bool,
    socket: Option<&str>,
    session_id: Option<&str>,
    force: bool,
) -> std::result::Result<SendPromptOutcome, String> {
    let inside_cmux = std::env::var("CMUX_SURFACE_ID").is_ok_and(|id| !id.trim().is_empty());
    if tend::backend::pane_id_host_kind(pane) == Some(tend::HostKind::Cmux)
        && inside_cmux
        && socket.is_none()
        && session_id.is_none()
    {
        let env_endpoint = tend::backend::cmux::endpoint_from_env();
        let on_pane: Vec<_> = client
            .snapshot()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|agent| agent.pane.as_deref() == Some(pane))
            .collect();
        let mut endpoints = on_pane
            .iter()
            .filter_map(|agent| agent.tmux_socket.clone())
            .collect::<Vec<_>>();
        endpoints.sort();
        endpoints.dedup();
        let endpoint = if endpoints.is_empty() || endpoints.contains(&env_endpoint) {
            env_endpoint
        } else if endpoints.len() == 1 {
            endpoints.remove(0)
        } else {
            return Err(format!(
                "cmux pane {pane} exists on multiple socket endpoints; run tend mcp inside the target cmux instance"
            ));
        };
        // This path types without the daemon, so it applies the same gate,
        // judged on the endpoint it is about to type into. It has no daemon
        // input guard (it never had one); the state is as fresh as this read.
        if !force {
            tend::prompt_delivery::idle_gate(pane, &on_pane, Some(&endpoint), None)?;
        }
        let target_pane = pane.to_string();
        let target_prompt = prompt.to_string();
        if let Ok(Some(outcome)) = tokio::task::spawn_blocking(move || {
            let backend = tend::backend::cmux::CmuxBackend::with_endpoint(endpoint);
            if !tend::PaneBackend::send_prompt_text_on(&backend, None, &target_pane, &target_prompt)
            {
                return None;
            }
            let submitted = if submit {
                if !target_prompt.is_empty() {
                    std::thread::sleep(tend::backend::PROMPT_SUBMIT_GRACE);
                }
                tend::PaneBackend::send_text(&backend, &target_pane, "\r")
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

    // A daemon without the gate ignores `force` and would type into a busy
    // target; refuse rather than silently lose the gate.
    if !force {
        let hello = client
            .hello(Duration::from_secs(3))
            .await
            .map_err(|error| error.to_string())?;
        if !hello
            .capabilities
            .iter()
            .any(|cap| cap == tend::prompt_delivery::SEND_PROMPT_STATE_GATE_CAPABILITY)
        {
            return Err(format!(
                "{}: this tendd cannot check that the target is idle (no {}); nothing was sent. \
                 Upgrade/restart tendd, or pass force=true to type regardless.",
                tend::prompt_delivery::TARGET_NOT_IDLE,
                tend::prompt_delivery::SEND_PROMPT_STATE_GATE_CAPABILITY,
            ));
        }
    }
    // This process's endpoint identifies the caller's server, never the
    // daemon's ambient server. Explicit targeting always wins.
    let caller_socket = socket
        .map(ToOwned::to_owned)
        .or_else(|| prompt_socket_from_env(pane));
    client
        .send_prompt_with_gate(
            pane,
            prompt,
            submit,
            caller_socket.as_deref(),
            session_id,
            Some(force),
            None,
        )
        .await
        .map_err(|error| error.to_string())
}

fn prompt_socket_from_env(pane: &str) -> Option<String> {
    match tend::backend::pane_id_host_kind(pane) {
        Some(tend::HostKind::Rmux) => tend::backend::rmux::endpoint_from_env(),
        Some(tend::HostKind::Tmux) => {
            let endpoint = tend::backend::rmux::endpoint_from_value(&std::env::var("TMUX").ok()?)?;
            // rmux exports TMUX for compatibility; never pass it to native tmux.
            if tend::backend::rmux::is_endpoint(&endpoint)
                || tend::backend::rmux::endpoint_from_env().as_deref() == Some(&endpoint)
            {
                return None;
            }
            Some(endpoint)
        }
        _ => None,
    }
}

/// Terminal outcome of a `tend_wait_for_change` race, before it's rendered
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

/// `tend_wait_for_change`: block until a matching state transition is
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
                        // An Idle -> Idle broadcast announces a new question
                        // (`awaiting_reply`), not a state change.
                        if t.from != t.to
                            && pane.is_none_or(|p| t.agent.pane.as_deref() == Some(p))
                        {
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

/// `tend_swap_account`: `tend swap --json` through the daemon's work
/// command transport, here or on a Fleet host. `action` picks a start
/// (`swap`, the default), a `retry` of an ended job, or a read (`status`)
/// or `cancel` of one. A start or retry runs under a request key, the
/// caller's own or one minted here before anything is sent, and every
/// result carries it: the handle to find the job again. Nothing is ever
/// sent twice. The caller's own pane is never swapped from under its call.
async fn swap_account(client: &Client, args: &Value, config: &tend::config::Config) -> Value {
    let host = tend::work_control::remote_host_alias(args.get("host").and_then(Value::as_str));
    let plan = match swap_plan(args, host, own_pane()) {
        Ok(plan) => plan,
        Err(error) => return swap_error(&SwapCall::rejected(host, args), &error),
    };
    // Here, a start or retry needs the account module; a Fleet host
    // decides for itself, and reading a job never needs it.
    if host.is_none() && matches!(plan.action, SwapAction::Swap | SwapAction::Retry) {
        if let Some(why) = crate::account_module::unavailable(config) {
            return swap_error(&plan.call(host), &format!("{why}; nothing was sent"));
        }
    }
    if plan.action == SwapAction::Retry && host.is_none() {
        if let Err(error) =
            retry_spares_caller(client, plan.job.as_deref(), own_pane().as_deref()).await
        {
            return swap_error(&plan.call(host), &error);
        }
    }
    let output = match client.work_command(host, &plan.argv, None).await {
        Ok(output) => output,
        Err(error) => {
            return swap_error(
                &plan.call(host),
                &format!("{error}; whether it was accepted is unknown"),
            )
        }
    };
    swap_reply(&plan, host, &output)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwapAction {
    Swap,
    Retry,
    Status,
    Cancel,
}

/// One call, before it is sent: what to run and what to report.
#[derive(Debug)]
struct SwapPlan {
    action: SwapAction,
    argv: Vec<String>,
    job: Option<String>,
    request_key: Option<String>,
    dry_run: bool,
    detach: bool,
}

/// What every result names.
struct SwapCall {
    host: String,
    action: &'static str,
    job: Option<String>,
    request_key: Option<String>,
}

impl SwapCall {
    fn rejected(host: Option<&str>, args: &Value) -> Self {
        Self {
            host: host.unwrap_or("local").to_owned(),
            action: args
                .get("action")
                .and_then(Value::as_str)
                .map_or("swap", |a| match a {
                    "retry" => "retry",
                    "status" => "status",
                    "cancel" => "cancel",
                    _ => "swap",
                }),
            job: args.get("job").and_then(Value::as_str).map(str::to_owned),
            request_key: None,
        }
    }

    fn fields(&self) -> serde_json::Map<String, Value> {
        let mut fields = serde_json::Map::new();
        fields.insert("host".into(), json!(self.host));
        fields.insert("action".into(), json!(self.action));
        if let Some(job) = &self.job {
            fields.insert("job".into(), json!(job));
        }
        if let Some(key) = &self.request_key {
            fields.insert("request_key".into(), json!(key));
        }
        fields
    }
}

impl SwapPlan {
    fn call(&self, host: Option<&str>) -> SwapCall {
        SwapCall {
            host: host.unwrap_or("local").to_owned(),
            action: match self.action {
                SwapAction::Swap => "swap",
                SwapAction::Retry => "retry",
                SwapAction::Status => "status",
                SwapAction::Cancel => "cancel",
            },
            job: self.job.clone(),
            request_key: self.request_key.clone(),
        }
    }
}

fn own_pane() -> Option<String> {
    tend::default_backend()
        .current_pane()
        .or_else(current_process_host_pane)
}

/// Fields a start takes, and that a read or a retry must not.
const SWAP_START_FIELDS: &[&str] = &[
    "targets",
    "all",
    "provider",
    "to",
    "spread",
    "when_idle",
    "idle_timeout",
    "continue_text",
    "continue_when",
    "continue",
    "no_continue",
    "dry_run",
];

/// Turn the tool's arguments into one `tend swap` argv, refusing what the
/// action does not take before anything is sent.
#[allow(clippy::too_many_lines)] // one validation and translation per action, in order
fn swap_plan(
    args: &Value,
    host: Option<&str>,
    own_pane: Option<String>,
) -> Result<SwapPlan, String> {
    let flag = |name: &str| args.get(name).and_then(Value::as_bool).unwrap_or(false);
    let text = |name: &str| args.get(name).and_then(Value::as_str);
    let present = |name: &str| args.get(name).is_some_and(|v| !v.is_null());
    let action = match text("action").unwrap_or("swap") {
        "swap" => SwapAction::Swap,
        "retry" => SwapAction::Retry,
        "status" => SwapAction::Status,
        "cancel" => SwapAction::Cancel,
        other => {
            return Err(format!(
                "unknown action {other:?} (swap, retry, status, cancel)"
            ))
        }
    };
    let job = text("job").map(str::to_owned);
    if action != SwapAction::Swap && job.is_none() {
        return Err("this action needs `job` (a request key or job id)".into());
    }
    if action == SwapAction::Swap && job.is_some() {
        return Err("`job` goes with action retry, status or cancel".into());
    }
    if action != SwapAction::Swap {
        if let Some(field) = SWAP_START_FIELDS.iter().find(|field| present(field)) {
            return Err(format!("`{field}` is for action swap only"));
        }
    }
    let reads = matches!(action, SwapAction::Status | SwapAction::Cancel);
    if reads {
        for field in ["force", "continue_evidence", "detach", "request_key"] {
            if present(field) {
                return Err(format!(
                    "`{field}` does not go with action {}",
                    text("action").unwrap_or("")
                ));
            }
        }
    }
    let wait = args.get("wait_secs").and_then(Value::as_u64);
    if present("wait_secs") && !reads {
        return Err("`wait_secs` goes with action status or cancel".into());
    }
    if wait.is_some_and(|wait| wait > 120) {
        return Err("`wait_secs` is 0-120".into());
    }
    let dry_run = flag("dry_run");
    let detach = flag("detach");
    if detach && dry_run {
        return Err("`detach` does not go with `dry_run`".into());
    }
    // A start or a retry runs under a key the caller can keep.
    let request_key = match (action, dry_run) {
        (SwapAction::Swap | SwapAction::Retry, false) => Some(match text("request_key") {
            Some(key) if crate::swap::plain_request_key(key) => key.to_owned(),
            Some(_) => {
                return Err("`request_key`: 1-128 characters of letters, digits and -_.:@".into())
            }
            None => format!("cli-{}", uuid::Uuid::new_v4()),
        }),
        _ if present("request_key") => {
            return Err("`request_key` goes with a swap or retry that is not a dry run".into())
        }
        _ => None,
    };
    let mut words: Vec<String> = Vec::new();
    match action {
        SwapAction::Swap => {
            words.extend(
                args.get("targets")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
            for (name, switch) in [
                ("all", "--all"),
                ("spread", "--spread"),
                ("when_idle", "--when-idle"),
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
                ("idle_timeout", "--idle-timeout"),
                ("continue_text", "--continue-text"),
                ("continue_when", "--continue-when"),
                ("continue", "--continue"),
            ] {
                if let Some(value) = text(name) {
                    words.extend([option.to_owned(), value.to_owned()]);
                }
            }
            // Only a start widens nothing toward the caller: its own pane
            // is left out here.
            if host.is_none() {
                if let Some(pane) = own_pane {
                    words.extend(["--exclude".to_owned(), pane]);
                }
            }
        }
        SwapAction::Retry | SwapAction::Status | SwapAction::Cancel => {
            let flag = match action {
                SwapAction::Retry => "--retry",
                SwapAction::Status => "--status",
                _ => "--cancel",
            };
            words.extend([flag.to_owned(), job.clone().unwrap_or_default()]);
        }
    }
    if matches!(action, SwapAction::Swap | SwapAction::Retry) {
        if flag("force") {
            words.push("--force".to_owned());
        }
        if let Some(evidence) = text("continue_evidence") {
            words.extend(["--continue-evidence".to_owned(), evidence.to_owned()]);
        }
        if detach {
            words.push("--detach".to_owned());
        }
    }
    if reads {
        words.extend(["--wait-secs".to_owned(), wait.unwrap_or(0).to_string()]);
    }
    if let Some(key) = &request_key {
        words.extend(["--request-key".to_owned(), key.clone()]);
    }
    let command_line = crate::swap::checked_argv(&words)?;
    Ok(SwapPlan {
        action,
        argv: command_line,
        job,
        request_key,
        dry_run,
        detach,
    })
}

/// A retry acts on what its parent planned: refused here if that is the
/// caller's own pane, which a start would have left out.
async fn retry_spares_caller(
    client: &Client,
    parent: Option<&str>,
    own: Option<&str>,
) -> Result<(), String> {
    let (Some(parent), Some(own)) = (parent, own) else {
        return Ok(());
    };
    let parent_ref = if parent
        .strip_prefix("swap-")
        .is_some_and(|uuid| uuid::Uuid::parse_str(uuid).is_ok())
    {
        tend::swap_api::JobRef::Id(parent.to_owned())
    } else {
        tend::swap_api::JobRef::Key(parent.to_owned())
    };
    let view = client
        .swap_job_status(&parent_ref)
        .await
        .map_err(|error| format!("could not read the job to retry: {error}; nothing was sent"))?;
    if view
        .entries
        .iter()
        .any(|entry| entry.pane.as_deref() == Some(own))
    {
        return Err(format!(
            "job {} includes this caller's own pane {own}; a retry from here would swap it from \
             under the call. Nothing was sent; retry it from another pane or the CLI",
            view.job_id
        ));
    }
    Ok(())
}

/// The result of a sent call: its payload, the exit status, the host's
/// diagnostics, and, for a job that may exist, how to find it.
fn swap_reply(
    plan: &SwapPlan,
    host: Option<&str>,
    output: &tend::work_control::WorkCommandOutput,
) -> Value {
    let call = plan.call(host);
    // The payload as the action's own type: anything else (null, another
    // shape, an acknowledgement without its fields) is no result at all.
    let payload = typed_payload(plan, &output.stdout);
    let mut fields = call.fields();
    fields.insert("exit_code".into(), json!(output.exit_code));
    if !output.stderr.trim().is_empty() {
        fields.insert("stderr".into(), json!(output.stderr.trim()));
    }
    if let Some(job_id) = payload
        .as_ref()
        .and_then(|p| p.get("job_id"))
        .and_then(Value::as_str)
    {
        fields.insert("job_id".into(), json!(job_id));
    }
    // A detach acknowledges the key that was sent, or nothing settles.
    let mismatch = plan.detach
        && payload
            .as_ref()
            .and_then(|p| p.get("request_key"))
            .and_then(Value::as_str)
            .is_some_and(|key| Some(key) != plan.request_key.as_deref());
    let failed_rows = payload
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|rows| rows.iter().any(|o| o["status"] == "failed"));
    let field = match plan.action {
        SwapAction::Status | SwapAction::Cancel => "job_view",
        _ if plan.detach => "detached",
        _ => "outcomes",
    };
    if let Some(payload) = payload.clone() {
        fields.insert(field.into(), payload);
    }
    let ok = payload.is_some() && output.exit_code == 0 && !failed_rows && !mismatch;
    if !ok && !output.stdout.trim().is_empty() {
        // What the host printed may explain what happened.
        let raw: String = output.stdout.trim().chars().take(4000).collect();
        fields.insert("stdout".into(), json!(raw));
    }
    if !ok {
        if plan.request_key.is_some() {
            fields.insert("recovery".into(), recovery(&call));
        }
        let note = if mismatch {
            "the host acknowledged another request key; whether this one was accepted is unknown"
        } else if payload.is_none() {
            "no result came back; that is not proof nothing started"
        } else {
            "the call did not end cleanly"
        };
        fields.insert("error".into(), json!(note));
        return json!({
            "content": [ { "type": "text", "text": Value::Object(fields).to_string() } ],
            "isError": true,
        });
    }
    if plan.dry_run {
        fields.remove("request_key");
    }
    json_result(&Value::Object(fields))
}

/// `stdout` read as the action's result type, back to JSON; `None` if it is
/// not one.
fn typed_payload(plan: &SwapPlan, stdout: &str) -> Option<Value> {
    match plan.action {
        SwapAction::Status | SwapAction::Cancel => {
            serde_json::from_str::<tend::swap_api::JobView>(stdout)
                .ok()
                .and_then(|view| serde_json::to_value(view).ok())
        }
        _ if plan.detach => serde_json::from_str::<crate::swap::DetachedAck>(stdout)
            .ok()
            .filter(|ack| {
                [&ack.request_key, &ack.job_id, &ack.state]
                    .iter()
                    .all(|field| !field.trim().is_empty())
            })
            .and_then(|ack| serde_json::to_value(ack).ok()),
        _ => serde_json::from_str::<Vec<tend::swap_api::SwapOutcome>>(stdout)
            .ok()
            .and_then(|outcomes| serde_json::to_value(outcomes).ok()),
    }
}

/// How to find a job that may exist: never by sending it again.
fn recovery(call: &SwapCall) -> Value {
    let mut args = serde_json::Map::new();
    args.insert("action".into(), json!("status"));
    args.insert("job".into(), json!(call.request_key));
    if call.host != "local" {
        args.insert("host".into(), json!(call.host));
    }
    json!({ "tool": "tend_swap_account", "arguments": Value::Object(args) })
}

fn swap_error(call: &SwapCall, error: &str) -> Value {
    let mut fields = call.fields();
    fields.insert("error".into(), json!(error));
    if call.request_key.is_some() {
        fields.insert("recovery".into(), recovery(call));
    }
    json!({
        "content": [ { "type": "text", "text": Value::Object(fields).to_string() } ],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    /// `cargo test -p tend-cli --bin tend mcp_schema_bytes -- --ignored --nocapture`
    /// prints what an agent pays for `tools/list`, per tool and in total.
    #[test]
    #[ignore = "measurement"]
    fn mcp_schema_bytes() {
        let tools = tool_definitions(&tend::config::Config::default());
        let mut rows: Vec<_> = tools
            .iter()
            .map(|tool| {
                (
                    tool.to_string().len(),
                    tool["name"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        rows.sort();
        for (bytes, name) in &rows {
            println!("{bytes:>6} {name}");
            if std::env::var("TEND_SCHEMA_DETAIL").is_ok() {
                let tool = tools
                    .iter()
                    .find(|tool| tool["name"] == name.as_str())
                    .unwrap();
                println!(
                    "         description {}",
                    tool["description"].to_string().len()
                );
                if let Some(properties) = tool["inputSchema"]["properties"].as_object() {
                    let mut sizes: Vec<_> = properties
                        .iter()
                        .map(|(key, value)| (value.to_string().len(), key.clone()))
                        .collect();
                    sizes.sort();
                    for (size, key) in sizes.iter().rev().take(6) {
                        println!("         {size:>5} {key}");
                    }
                }
            }
        }
        println!(
            "{:>6} TOTAL {} tools",
            serde_json::json!({ "tools": tools }).to_string().len(),
            tools.len()
        );
    }

    use super::*;

    // --- tend_swap_account -------------------------------------------------

    fn key_in(argv: &[String]) -> Option<String> {
        argv.iter()
            .find_map(|w| w.strip_prefix("--request-key="))
            .map(str::to_owned)
    }

    /// A start runs under a key, the caller's or a minted one, and the key
    /// survives the argv's canonical form; the caller's pane is left out.
    #[test]
    fn a_swap_call_carries_its_key_through_the_argv() {
        let plan = swap_plan(&json!({"targets": ["%5"]}), None, Some("%9".into())).unwrap();
        let key = plan.request_key.clone().unwrap();
        assert!(key.starts_with("cli-"));
        assert_eq!(key_in(&plan.argv).as_deref(), Some(key.as_str()));
        assert!(
            plan.argv.contains(&"--exclude=%9".to_owned()),
            "{:?}",
            plan.argv
        );
        tend::work_control::validate_swap_argv(&plan.argv).unwrap();
        let own = swap_plan(&json!({"request_key": "agent-7"}), Some("devbox"), None).unwrap();
        assert_eq!(key_in(&own.argv).as_deref(), Some("agent-7"));
        assert!(!own.argv.iter().any(|w| w.starts_with("--exclude")));
        // A dry run makes no job and has no key.
        let dry = swap_plan(&json!({"dry_run": true}), None, None).unwrap();
        assert_eq!(dry.request_key, None);
        assert_eq!(key_in(&dry.argv), None);
        assert!(swap_plan(&json!({"dry_run": true, "request_key": "k"}), None, None).is_err());
        assert!(swap_plan(&json!({"request_key": "has space"}), None, None).is_err());
    }

    /// Retries, reads and cancels take only what they mean.
    #[test]
    fn each_swap_action_takes_only_its_own_fields() {
        let retry = swap_plan(
            &json!({"action": "retry", "job": "cli-1", "force": true}),
            None,
            Some("%9".into()),
        )
        .unwrap();
        assert!(retry.argv.contains(&"--retry=cli-1".to_owned()));
        assert!(key_in(&retry.argv).is_some());
        assert!(!retry.argv.iter().any(|w| w.starts_with("--exclude")));
        let status = swap_plan(&json!({"action": "status", "job": "cli-1"}), None, None).unwrap();
        assert!(status.argv.contains(&"--status=cli-1".to_owned()));
        assert!(status.argv.contains(&"--wait-secs=0".to_owned()));
        assert_eq!(status.request_key, None);
        tend::work_control::validate_swap_argv(&status.argv).unwrap();
        let cancel = swap_plan(
            &json!({"action": "cancel", "job": "k", "wait_secs": 30}),
            Some("h"),
            None,
        )
        .unwrap();
        assert!(cancel.argv.contains(&"--cancel=k".to_owned()));
        assert!(cancel.argv.contains(&"--wait-secs=30".to_owned()));
        for bad in [
            json!({"action": "status"}),
            json!({"action": "nope"}),
            json!({"job": "k"}),
            json!({"action": "status", "job": "k", "targets": ["%5"]}),
            json!({"action": "retry", "job": "k", "to": "a"}),
            json!({"action": "cancel", "job": "k", "request_key": "x"}),
            json!({"action": "status", "job": "k", "wait_secs": 121}),
            json!({"wait_secs": 5}),
            json!({"detach": true, "dry_run": true}),
        ] {
            assert!(swap_plan(&bad, None, None).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn cost_counters_separate_original_utf8_text_from_encoded_wire() {
        let body = "한글\n\"";
        let request =
            json!({"method":"tools/call","params":{"name":"tend_reply","arguments":{"body":body}}});
        assert_eq!(
            tool_call_cost(&request),
            Some(("tend_reply".into(), body.len()))
        );
        assert!(tool_call_cost(&json!({"method":"initialize"})).is_none());
        let response = json!({"id":7,"result":text_result(body)});
        assert_eq!(result_text_bytes(&response), body.len());
        let (writer, reader) = tokio::io::duplex(1024);
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let bytes = write_response(&writer, &response).await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(bytes, line.len());
        assert!(bytes > body.len());
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["result"]["content"][0]["text"],
            body
        );
    }

    fn output(exit_code: i32, stdout: &str, stderr: &str) -> tend::work_control::WorkCommandOutput {
        tend::work_control::WorkCommandOutput {
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn reply_fields(reply: &Value) -> (bool, Value) {
        let error = reply["isError"].as_bool().unwrap_or(false);
        let text = reply["content"][0]["text"].as_str().unwrap();
        (error, serde_json::from_str(text).unwrap())
    }

    /// Every result names its handle; a failure keeps the host's output and
    /// says how to find the job, never to send it again.
    #[test]
    fn a_swap_reply_keeps_its_handle_whatever_happened() {
        let plan = swap_plan(
            &json!({"targets": ["%5"], "request_key": "k-1"}),
            Some("devbox"),
            None,
        )
        .unwrap();
        let (error, fields) =
            reply_fields(&swap_reply(&plan, Some("devbox"), &output(0, "[]", "")));
        assert!(!error);
        assert_eq!(fields["request_key"], "k-1");
        assert_eq!(fields["host"], "devbox");
        let failed =
            r#"[{"pane":"%5","agent_session_id":"s","kind":"claude_code","status":"failed"}]"#;
        let (error, fields) = reply_fields(&swap_reply(
            &plan,
            Some("devbox"),
            &output(1, failed, "boom"),
        ));
        assert!(error);
        assert_eq!(fields["outcomes"][0]["status"], "failed");
        assert_eq!(fields["stderr"], "boom");
        assert_eq!(fields["recovery"]["arguments"]["job"], "k-1");
        assert_eq!(fields["recovery"]["arguments"]["host"], "devbox");
        let (error, fields) = reply_fields(&swap_reply(
            &plan,
            Some("devbox"),
            &output(1, "", "refused"),
        ));
        assert!(error);
        assert!(fields["error"]
            .as_str()
            .unwrap()
            .contains("not proof nothing started"));
        // A lost answer: the error still names the key.
        let (error, fields) = reply_fields(&swap_error(&plan.call(Some("devbox")), "disconnected"));
        assert!(error);
        assert_eq!(fields["recovery"]["arguments"]["action"], "status");
    }

    /// A detach is settled only by an acknowledgement of the key sent.
    #[test]
    fn a_detached_swap_needs_its_own_acknowledgement() {
        let plan = swap_plan(&json!({"detach": true, "request_key": "k-2"}), None, None).unwrap();
        let ack =
            |key: &str| format!(r#"{{"request_key":"{key}","job_id":"swap-9","state":"running"}}"#);
        let (error, fields) = reply_fields(&swap_reply(&plan, None, &output(0, &ack("k-2"), "")));
        assert!(!error);
        assert_eq!(fields["job_id"], "swap-9");
        assert_eq!(fields["detached"]["request_key"], "k-2");
        let (error, fields) = reply_fields(&swap_reply(&plan, None, &output(0, &ack("other"), "")));
        assert!(error);
        assert!(fields["error"]
            .as_str()
            .unwrap()
            .contains("another request key"));
    }

    /// Output that is not the action's own result type is no result, and
    /// what the host printed is kept with the way to find the job.
    #[test]
    fn a_swap_reply_of_the_wrong_shape_is_no_result() {
        let swap = swap_plan(
            &json!({"targets": ["%5"], "request_key": "k-3"}),
            None,
            None,
        )
        .unwrap();
        let status = swap_plan(&json!({"action": "status", "job": "k"}), None, None).unwrap();
        let detach = swap_plan(&json!({"detach": true, "request_key": "k-4"}), None, None).unwrap();
        let cases = [
            (&swap, "null"),
            (&swap, "{}"),
            (&status, "null"),
            (&status, "[]"),
            (&status, r#"{"job_id":"swap-1"}"#),
            (&detach, "{}"),
            (&detach, r#"{"request_key":"k-4","job_id":"swap-9"}"#),
            (
                &detach,
                r#"{"request_key":"k-4","job_id":"","state":"running"}"#,
            ),
        ];
        for (plan, stdout) in cases {
            let (error, fields) = reply_fields(&swap_reply(plan, None, &output(0, stdout, "")));
            assert!(error, "{stdout} was taken as a result");
            assert_eq!(fields["stdout"], stdout);
            assert!(fields["error"]
                .as_str()
                .unwrap()
                .contains("not proof nothing started"));
            if plan.request_key.is_some() {
                assert_eq!(fields["recovery"]["arguments"]["action"], "status");
            }
        }
        // A failure's raw output is bounded.
        let long = "x".repeat(10_000);
        let (_, fields) = reply_fields(&swap_reply(&swap, None, &output(1, &long, "")));
        assert_eq!(fields["stdout"].as_str().unwrap().len(), 4000);
    }

    /// A daemon whose one job planned pane `%9`.
    struct OneJob;

    impl tend::swap_api::SwapJobs for OneJob {
        fn health(&self) -> tend::swap_api::SwapHealth {
            tend::swap_api::SwapHealth {
                readiness: tend::swap_api::SwapReadiness::Ready,
                reason: None,
                schema: Some(2),
                expected_schema: 2,
                fault: None,
                jobs_running: 0,
                maintenance: None,
            }
        }
        fn active(&self) -> Result<(), String> {
            Ok(())
        }
        fn plan(
            &self,
            _: &tend::swap_api::SwapOverrides,
        ) -> Result<tend::swap_api::PlanView, String> {
            Err("no".into())
        }
        fn submit(
            &self,
            _: &str,
            _: &tend::swap_api::SwapOverrides,
        ) -> Result<tend::swap_api::Accepted, String> {
            Err("no".into())
        }
        fn retry(
            &self,
            _: &str,
            _: &tend::swap_api::JobRef,
            _: &tend::swap_api::RetryOverrides,
        ) -> Result<tend::swap_api::Accepted, String> {
            Err("no".into())
        }
        fn cancel(&self, _: &tend::swap_api::JobRef) -> Result<tend::swap_api::JobView, String> {
            Err("no".into())
        }
        fn status(&self, _: &tend::swap_api::JobRef) -> Result<tend::swap_api::JobView, String> {
            Ok(serde_json::from_value(json!({
                "job_id": "swap-1", "node_id": "n", "state": "done", "revision": 1,
                "reason": null, "fault": null,
                "entries": [{"report_index": 0, "pane": "%9", "disposition": "failed",
                    "reason": null, "uncertain_effects": 0, "outcome": null}]
            }))
            .unwrap())
        }
    }

    /// A retry from here is refused if its job planned the caller's own
    /// pane, before anything is sent; another caller may retry it.
    #[tokio::test]
    async fn a_retry_never_swaps_the_callers_own_pane() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("retry.sock");
        let server = Server::new(socket.clone(), Store::shared())
            .with_swap(Some(std::sync::Arc::new(OneJob)));
        let (shutdown, rx) = broadcast::channel(1);
        let daemon = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&socket).await;
        let client = Client::new(socket);
        let refused = retry_spares_caller(&client, Some("cli-1"), Some("%9")).await;
        let allowed = retry_spares_caller(&client, Some("cli-1"), Some("%2")).await;
        shutdown.send(()).unwrap();
        daemon.await.unwrap();
        assert!(refused.unwrap_err().contains("own pane %9"));
        assert!(allowed.is_ok());
    }

    /// A job still at work is a successful read, not a failure.
    #[test]
    fn a_status_of_a_running_job_is_a_success() {
        let plan = swap_plan(&json!({"action": "status", "job": "k"}), None, None).unwrap();
        let view = r#"{"job_id":"swap-3","node_id":"n","state":"running","revision":1,"reason":null,"entries":[],"fault":null}"#;
        let (error, fields) = reply_fields(&swap_reply(&plan, None, &output(0, view, "")));
        assert!(!error);
        assert_eq!(fields["job_view"]["state"], "running");
        assert_eq!(fields["job_id"], "swap-3");
        assert!(fields.get("request_key").is_none());
    }

    #[tokio::test]
    async fn lifecycle_target_keeps_rmux_namespace_bound_to_registered_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("routing.sock");
        let store = Store::shared();
        let register = |session: &str, endpoint: &str| AgentEvent::Started {
            id: tend::event::AgentId {
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
            tend::BackendEndpoint {
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
        use tend::HostKind;
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
        request.reply = Some(tend::collaboration::CollaborationReply {
            report: None,
            status: RequestStatus::Completed,
            body: "large final reply".repeat(4096),
            artifacts: vec![],
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
    fn own_pane_drops_a_borrowed_shared_app_server_pane() {
        assert_eq!(
            own_pane_from(Some("%3".into()), || false).as_deref(),
            Some("%3")
        );
        assert_eq!(own_pane_from(Some("%3".into()), || true), None);
        assert_eq!(own_pane_from(Some("  ".into()), || false), None);
        assert_eq!(
            own_pane_from(None, || panic!("no pane, no ancestry walk")),
            None
        );
    }

    #[test]
    fn caller_session_hint_reads_codex_meta() {
        let params = json!({
            "name": "tend_room_context",
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

    /// Shared-host identity follows the conversation at the daemon and cannot
    /// fall back to a cached pane after that conversation has gone away.
    #[test]
    fn shared_origin_has_no_physical_fallback_or_snapshot_dependency() {
        let origin = shared_session_origin("thread-a".into());
        assert_eq!(origin.agent_session_id.as_deref(), Some("thread-a"));
        assert!(origin.exact_session);
        assert!(origin.pane.is_empty());
        assert!(origin.socket.is_none());
        assert!(!origin.console);
        let mut caller = crate::collaboration_output::fixture().from;
        caller.agent_session_id = "thread-a".into();
        caller.pane = "rmux:%1".into();
        assert_eq!(
            tend::collaboration::resolve_origin(&origin, &[caller.clone()], &[])
                .unwrap()
                .pane,
            "rmux:%1"
        );
        caller.pane = "rmux:%2".into();
        assert_eq!(
            tend::collaboration::resolve_origin(&origin, &[caller.clone()], &[])
                .unwrap()
                .pane,
            "rmux:%2"
        );
        caller.agent_session_id = "successor".into();
        caller.pane = "rmux:%1".into();
        assert!(tend::collaboration::resolve_origin(&origin, &[caller], &[]).is_err());
    }
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tend::backend::{BackendCaps, HostKind, PaneBackend, SharedBackend};
    use tend::event::AgentEvent;
    use tend::ipc::Server;
    use tend::state::Store;
    use tend::tmux::PaneInfo;
    use tokio::sync::broadcast;

    /// Fake backend that always accepts `send_text` and records the calls,
    /// so the MCP → IPC → backend path can be exercised in-process.
    struct FakeBackend {
        sends: Arc<Mutex<Vec<(String, String)>>>,
        kind: HostKind,
        sockets: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl PaneBackend for FakeBackend {
        fn kind(&self) -> HostKind {
            self.kind
        }
        fn send_text_on(&self, socket: Option<&str>, pane: &str, text: &str) -> bool {
            self.sockets
                .lock()
                .unwrap()
                .push(socket.map(ToOwned::to_owned));
            self.send_text(pane, text)
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
            kind: HostKind::Tmux,
            sockets: Arc::default(),
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
            exact_session: false,
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
            tend::collaboration::CollaborationReadView::Full,
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
            tend::collaboration::CollaborationReadView::Full,
        )
        .await;
        assert_eq!(*seen.lock().unwrap(), ["role:reviewer", "pane:%2"]);
    }

    /// `tend_call_peer` sends a named slot as itself, so the daemon records
    /// it and the request can follow a replaced holder: a
    /// `workspace/work/role` path (which local peer selection does not
    /// know) and a `role:` target alike.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one isolated transport lifecycle, including origin env
    async fn call_peer_sends_named_slots_as_slots() {
        use std::os::unix::fs::PermissionsExt;
        const PROBE: &str = "TEND_CALL_PEER_SLOT_PROBE";
        if std::env::var_os(PROBE).is_none() {
            // This fixture models routing and has no real terminal backend.
            // Make metadata explicit instead of querying the developer's tmux.
            let control = tempfile::tempdir().unwrap();
            let tmux = control.path().join("tmux");
            std::fs::write(&tmux, "#!/bin/sh\ncase \"$*\" in\n*-V*) printf 'tmux 3.6\\n';;\n*display-message*) printf '\\t\\t\\n';;\n*) exit 1;;\nesac\n").unwrap();
            std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut paths = vec![control.path().to_owned()];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mcp::tests::call_peer_sends_named_slots_as_slots",
                    "--nocapture",
                ])
                .env(PROBE, "1")
                .env("PATH", std::env::join_paths(paths).unwrap())
                .env_remove("TEND_HOST")
                .env_remove("TEND_HOSTS")
                .env_remove("RMUX")
                .env_remove("RMUX_PANE")
                .env_remove("CMUX_SOCKET_PATH")
                .env_remove("HERDR_PANE")
                .env("TMUX", "/tmp/tend-slot-test/default,0,0")
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
        for (pane, session) in [
            ("%1", "slot-sender"),
            ("%2", "slot-reviewer"),
            ("%3", "slot-primary"),
        ] {
            store
                .apply(&AgentEvent::Started {
                    id: tend::event::AgentId {
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
        let panes = [("%1", "0"), ("%2", "1"), ("%3", "2")]
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
        let mailbox = tend::collaboration::CollaborationStore::in_memory(
            tend::collaboration::CollaborationOptions::default(),
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
            .set_purpose(
                room.peers.iter().find(|peer| peer.pane == "%2").unwrap(),
                tend::collaboration::LaunchPurpose::Worker,
            )
            .await
            .unwrap();
        let tool = |target: &str| {
            json!({"jsonrpc":"2.0", "id":1,"method":"tools/call",
            "params":{"name":"tend_call_peer","arguments":{
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
        // The CLI shares the high-level routing: naming a protected primary
        // must send once to the existing worker, without touching real panes.
        crate::peer_cli::run(
            &client,
            &tend::config::Config::default(),
            crate::peer_cli::Args {
                target: "%3".into(),
                body: "CLI worker routing regression".into(),
                intent: "question".into(),
                execute: false,
                context_bound: false,
                user_authorized: false,
                no_spawn: true,
                wait: false,
                timeout_secs: 60,
                paths: vec![],
            },
        )
        .await
        .unwrap();
        let requests = client
            .collaboration_list(&origin, RequestMailbox::Sent)
            .await
            .unwrap();
        let cli: Vec<_> = requests
            .iter()
            .filter(|r| r.body == "CLI worker routing regression")
            .collect();
        assert_eq!(cli.len(), 1);
        assert_eq!(cli[0].to.pane, "%2");
        assert!(cli[0].to.accepts_peer_work());
        assert_eq!(cli[0].work_mode, tend::collaboration::WorkMode::ReadOnly);
        // A context-bound call stays with the exact session it selected,
        // and a path cannot name one.
        let bound = |target: &str| {
            json!({"jsonrpc":"2.0", "id":1,"method":"tools/call",
            "params":{"name":"tend_call_peer","arguments":{
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
        const PROBE: &str = "TEND_COMPACT_DISPATCH_PROBE";
        if std::env::var_os(PROBE).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mcp::tests::compact_collaboration_dispatch_round_trip",
                    "--nocapture",
                ])
                .env(PROBE, "1")
                // The probe parses the whole CLI, as `tend` does on its 8 MiB
                // main thread; a 2 MiB test thread is too tight for an
                // unoptimized clap tree.
                .env("RUST_MIN_STACK", "8388608")
                .env_remove("TEND_HOST")
                .env_remove("TEND_HOSTS")
                .env_remove("RMUX")
                .env_remove("RMUX_PANE")
                .env_remove("CMUX_SOCKET_PATH")
                .env_remove("HERDR_PANE")
                .env("TMUX", "/tmp/tend-compact-test/default,0,0")
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
                    id: tend::event::AgentId {
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
        let mailbox = tend::collaboration::CollaborationStore::in_memory(
            tend::collaboration::CollaborationOptions::default(),
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
            .set_purpose(&room.current, tend::collaboration::LaunchPurpose::Worker)
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
            dispatch(&client, &tool("tend_inbox", json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(inbox["requests"][0]["body"], request.body);
        assert_eq!(inbox["requests"][0]["paths"], json!(["src/allowed.rs"]));
        assert_eq!(inbox["requests"][0]["updates"][0]["body"], "guidance 1");
        assert!(inbox["requests"][0].get("created_at").is_none());
        let listing = decode(
            dispatch(&client, &tool("tend_list_messages", json!({})))
                .await
                .unwrap(),
        );
        assert!(listing["requests"][0].get("body").is_none());
        let bulk = dispatch(
            &client,
            &tool("tend_list_messages", json!({"include_body":true})),
        )
        .await
        .unwrap();
        assert_eq!(bulk["result"]["isError"], true);
        let body = decode(
            dispatch(
                &client,
                &tool(
                    "tend_list_messages",
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
                    "tend_list_messages",
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
                    "tend_list_messages",
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
                    "tend_update_request",
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
                    "tend_list_messages",
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
            "tend",
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
            &tool("tend_list_messages", json!({"after_update":0})),
        )
        .await
        .unwrap();
        assert_eq!(invalid["result"]["isError"], true);
        let timeout = decode(
            dispatch(
                &client,
                &tool(
                    "tend_wait_reply",
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
                    "tend_reply",
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
                &tool("tend_peer_report", json!({"request_id":request.id})),
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
                    "tend_peer_report",
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
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let cancelled = decode(
            dispatch(
                &client,
                &tool(
                    "tend_cancel_message",
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
                &tool("tend_request_attention", attention_args.clone()),
            )
            .await
            .unwrap(),
        );
        let retry = decode(
            dispatch(&client, &tool("tend_request_attention", attention_args))
                .await
                .unwrap(),
        );
        assert_eq!(first["request_id"], retry["request_id"]);
        let id = first["request_id"].as_str().unwrap();
        let human_request = client.collaboration_get(&origin, id).await.unwrap();
        assert!(human_request.needs_human_response());
        assert_eq!(
            human_request.human_action,
            Some(tend::collaboration::HumanAction::Approval)
        );
        assert_eq!(human_request.notify, Some(true));
        let mut other_room = room.current.room.clone();
        other_room.window_id = "@999".into();
        let operator = tend::collaboration::Participant::console(other_room);
        assert!(mailbox
            .list_for(
                &operator,
                RequestMailbox::Incoming,
                tend::collaboration::MailboxScope::Caller
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
            )
            .await
            .unwrap();
        let resolved = client.collaboration_get(&origin, id).await.unwrap();
        assert!(!resolved.needs_human_response());
        assert_eq!(resolved.reply.unwrap().body, "Approved for staging only");
        let invalid = dispatch(
            &client,
            &tool(
                "tend_request_attention",
                json!({"body":"Test", "action":"notice"}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(invalid["result"]["isError"], true);

        // Structured fields are stored on the human request, not in body.
        let structured = decode(
            dispatch(
                &client,
                &tool(
                    "tend_request_attention",
                    json!({"body":"Merge #12?", "action":"choice",
                        "options":["merge","hold"], "recommendation":"merge",
                        "evidence":["https://github.com/o/r/pull/12"]}),
                ),
            )
            .await
            .unwrap(),
        );
        let stored = client
            .collaboration_get(&origin, structured["request_id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(stored.body, "Merge #12?");
        let decision = stored.decision.expect("decision stored");
        assert_eq!(decision.options, ["merge", "hold"]);
        assert_eq!(decision.recommendation.as_deref(), Some("merge"));

        // One call files the caller's result record; the key makes a retry
        // return the same item.
        let args = json!({"summary":"Done: retry path", "commit":["abc123"],
            "decision_needed":"Ship now?", "options":["ship","hold"],
            "evidence":["test-pass:cargo test"], "key":"result-1"});
        let filed = decode(
            dispatch(&client, &tool("tend_file_result", args.clone()))
                .await
                .unwrap(),
        );
        let again = decode(
            dispatch(&client, &tool("tend_file_result", args))
                .await
                .unwrap(),
        );
        assert_eq!(filed["record"]["id"], again["record"]["id"]);
        assert_eq!(
            filed["record"]["target"],
            json!({"kind":"session","provider":"codex","session_id":"compact-recipient"})
        );
        assert_eq!(filed["record"]["requested_by"]["kind"], "agent");
        assert_eq!(
            filed["record"]["decision"]["evidence"][0],
            json!({"kind":"commit","sha":"abc123"})
        );
        for bad in [
            json!({"summary":"x", "target":"someone"}),
            json!({"summary":"x", "evidence":"test-fail:cargo test"}),
            json!({"summary":"x", "evidence":{"kind":"path","path":"a"}}),
        ] {
            let refused = dispatch(&client, &tool("tend_file_result", bad.clone()))
                .await
                .unwrap();
            assert_eq!(refused["result"]["isError"], true, "{bad}");
        }

        shutdown.send(()).unwrap();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn spawned_peer_registration_resumes_from_transition_stream() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("peer-registration.sock");
        let store = Store::shared();
        let origin_id = tend::event::AgentId {
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
            exact_session: false,
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
                id: tend::event::AgentId {
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
                id: tend::event::AgentId {
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
        assert_eq!(init["result"]["serverInfo"]["name"], "tend");
        assert_eq!(init["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert!(init["result"]["capabilities"]["tools"].is_object());
        let instructions = init["result"]["instructions"].as_str().unwrap();
        // Every session pays for these: a pointer to the guide, plus the one
        // rule every agent needs whatever its task (asking the person).
        assert!(instructions.len() < 200, "instructions: {instructions}");
        assert!(instructions.contains("tend_guide"));
        assert!(instructions.contains("reported_status"));
        assert!(instructions.contains("tend_request_attention"));

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
                "tend_status",
                "tend_start_agent",
                "tend_start_work",
                "tend_swap_account",
                "tend_manage_tmux",
                "tend_send_prompt",
                "tend_update_request",
                "tend_call_peer",
                "tend_set_identity",
                "tend_request_attention",
                "tend_inbox",
                "tend_reply",
                "tend_agent_search",
                "tend_file_result",
                "tend_guide",
                "tend_wait",
            ],
        );
        let start_agent = tools
            .iter()
            .find(|tool| tool["name"] == "tend_start_agent")
            .unwrap();
        assert_eq!(start_agent["inputSchema"]["required"], json!(["agent"]));
        assert_eq!(
            start_agent["inputSchema"]["properties"]["agent"]["enum"],
            json!(["claude", "codex", "agy", "opencode"])
        );
        assert!(start_agent["inputSchema"]["properties"]["work"].is_object());
        assert!(start_agent["inputSchema"]["properties"]["workspace"].is_object());
        let manage_tmux = tools
            .iter()
            .find(|tool| tool["name"] == "tend_manage_tmux")
            .unwrap();
        assert_eq!(manage_tmux["inputSchema"]["required"], json!(["action"]));
        assert!(manage_tmux["inputSchema"]["properties"]["workspace"].is_object());
        let guide = tools
            .iter()
            .find(|tool| tool["name"] == "tend_guide")
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
        let tools = tool_definitions(&tend::config::Config::default());
        let status = tools
            .iter()
            .find(|tool| tool["name"] == "tend_status")
            .unwrap();
        assert!(status["inputSchema"]["properties"]["full"].is_object());
        assert!(status["inputSchema"]["properties"]["query"].is_object());
        assert_eq!(status["inputSchema"]["properties"]["limit"]["maximum"], 50);
        let wait = tools
            .iter()
            .find(|tool| tool["name"] == "tend_wait")
            .unwrap();
        assert_eq!(
            wait["inputSchema"]["properties"]["timeout_secs"]["maximum"],
            600
        );
    }

    fn assert_fleet_tool_definitions(tools: &[Value], _instructions: &str) {
        let call = tools
            .iter()
            .find(|tool| tool["name"] == "tend_call_peer")
            .unwrap();
        assert!(call["inputSchema"]["properties"]["host"]["description"]
            .as_str()
            .unwrap()
            .contains("never auto-selects remotely"));
        let wait = tools
            .iter()
            .find(|tool| tool["name"] == "tend_wait")
            .unwrap();
        assert_eq!(
            wait["inputSchema"]["properties"]["pane_key"]["type"],
            "object"
        );
    }

    fn assert_peer_tool_definitions(tools: &[Value]) {
        let call_peer = tools
            .iter()
            .find(|tool| tool["name"] == "tend_call_peer")
            .unwrap();
        assert_eq!(
            call_peer["inputSchema"]["properties"]["intent"]["enum"],
            json!(["review", "question", "task", "notice"])
        );
        assert!(call_peer["description"]
            .as_str()
            .unwrap()
            .contains("Defaults to review + read_only"));
        let inbox = tools
            .iter()
            .find(|tool| tool["name"] == "tend_inbox")
            .unwrap();
        assert!(inbox["inputSchema"]["properties"]["view"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("report")));
        let properties = &call_peer["inputSchema"]["properties"];
        for field in ["parent_request_id", "artifacts", "links"] {
            assert!(properties[field]["description"]
                .as_str()
                .is_some_and(|description| !description.is_empty()));
        }
        // Stamped from the caller's pane; accepted but not advertised.
        for field in ["thread_id", "workspace_id", "work_id", "run_id"] {
            assert!(properties.get(field).is_none(), "{field} still listed");
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
                "tmux_session_name": "eng-6924",
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
            collaboration_guide(room.clone(), &tend::config::Config::default(), true, false);
        assert_eq!(legacy["capabilities"]["human_actions"], false);
        assert!(legacy["workflows"].get("human_feedback").is_none());
        let guide = collaboration_guide(room, &tend::config::Config::default(), true, true);
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
        assert_eq!(human["tool"], "tend_request_attention");
        assert_eq!(human["request"]["action"], "choice");
        assert!(human["wait"].as_str().unwrap().contains("tend_wait"));
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
        let mut config = tend::config::Config::default();
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
            "tend_inbox",
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
        let mut config = tend::config::Config::default();
        config.mcp.guide.placement = tend::config::McpPlacement::Window;
        config.mcp.guide.agent = Some("codex".into());
        config.mcp.guide.options = vec!["--model".into(), "gpt-5.3-codex".into()];
        config.mcp.guide.direction = tend::config::McpSplitDirection::Down;
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
            .find(|tool| tool["name"] == "tend_start_agent")
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
            "tmux_session_name": "eng-7000",
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
            tend::HostAccessMode::Observe,
            &complete,
        )
        .unwrap_err();
        assert!(observe.contains("explicitly grant mode='control'"));

        let old_remote = validate_fleet_collaboration_authority(
            "dev",
            false,
            tend::HostAccessMode::Control,
            &collaboration,
        )
        .unwrap_err();
        assert!(old_remote.contains("collaboration_get"));

        validate_fleet_collaboration_authority(
            "dev",
            false,
            tend::HostAccessMode::Control,
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
                "workspace_id": "webapp",
                "work_id": "ENG-7345",
                "run_id": "resolve-2",
                "artifacts": ["commit:d4bf2aa53", "test:258-passed"],
                "links": ["https://linear.app/webapp/issue/ENG-7345"]
            }),
            RequestKind::Review,
            "review the must-fix follow-up".into(),
            true,
            WorkMode::ReadOnly,
            vec!["crates/tend/src/collaboration.rs".into()],
        )
        .unwrap();

        assert_eq!(request.thread_id.as_deref(), Some("thread-7345"));
        assert_eq!(request.parent_request_id.as_deref(), Some("req-review-1"));
        assert_eq!(request.workspace_id.as_deref(), Some("webapp"));
        assert_eq!(request.work_id.as_deref(), Some("ENG-7345"));
        assert_eq!(request.run_id.as_deref(), Some("resolve-2"));
        assert_eq!(request.artifacts, ["commit:d4bf2aa53", "test:258-passed"]);
        assert_eq!(request.links, ["https://linear.app/webapp/issue/ENG-7345"]);
        assert_eq!(
            request.paths,
            ["crates/tend/src/collaboration.rs".to_string()]
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
        primary.launch_purpose = tend::collaboration::LaunchPurpose::Primary;
        let worker = peer_participant(AgentKind::Codex, "%3", AgentState::Idle);
        let room = peer_room(caller, vec![primary, worker]);
        let (selection, program) = peer_worker_route(&room, "pane:%2", &json!({})).unwrap();
        assert_eq!(selection.unwrap().pane, "%3");
        assert_eq!(program, Some(crate::agent_launch::AgentProgram::Codex));
        let (selection, program) =
            peer_worker_route(&room, "pane:%2", &json!({"context_bound":true})).unwrap();
        assert_eq!(selection.unwrap().pane, "%2");
        assert!(program.is_none());
        // The user told the agent to deliver to that session: keep it.
        let (selection, program) =
            peer_worker_route(&room, "pane:%2", &json!({"user_authorized":true})).unwrap();
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
        primary.launch_purpose = tend::collaboration::LaunchPurpose::Primary;
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
                tend::collaboration::LaunchPurpose::Primary
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
            select_peer(&room, "@tend-peer").unwrap().unwrap().pane,
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
            assert!(tend::collaboration::is_pending_session(
                &selected.peer.unwrap().agent_session_id
            ));
        }
    }

    /// An explicitly addressed pane with no registered peer is handed to tendd
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

    /// Queueing against a pane only makes sense when tend can tell that the
    /// pane became ready: discovery classifies every launchable provider,
    /// opencode included, by its process name.
    #[test]
    fn legacy_workers_are_probed_instead_of_assuming_registration_is_ready() {
        assert!(legacy_worker_needs_probe(Ok("launch\tclaude\t"), true));
        assert!(!legacy_worker_needs_probe(Ok("launch\tclaude\t"), false));
        assert!(!legacy_worker_needs_probe(
            Ok("launch\tclaude\tpending"),
            true
        ));
        assert!(!legacy_worker_needs_probe(Err(()), true));
        assert!(!legacy_worker_needs_probe(Ok("\tclaude\t"), true));
    }

    #[test]
    fn blocked_workers_get_a_passive_recovery_probe() {
        assert!(blocked_worker_needs_probe(Ok("launch\tcodex\tblocked")));
        assert!(blocked_worker_needs_probe(Ok("launch\tclaude\tblocked")));
        for state in [
            "launch\tcodex\tpending",
            "launch\tcodex\tready",
            "\tcodex\tblocked",
            "launch\tunknown\tblocked",
        ] {
            assert!(!blocked_worker_needs_probe(Ok(state)));
        }
        assert!(!blocked_worker_needs_probe(Err(())));
    }

    #[test]
    fn marked_workers_require_positive_readiness_before_dispatch() {
        assert!(!worker_dispatch_readiness(Err(())).0);
        for state in ["", "pending", "blocked", "unexpected"] {
            let screen = format!("launch\tclaude\t{state}");
            assert_eq!(
                worker_dispatch_readiness(Ok(&screen)),
                (false, Some("launch".into()))
            );
        }
        assert!(worker_dispatch_readiness(Ok("launch\tcodex\tready")).0);
        assert!(worker_dispatch_readiness(Ok("\tclaude\t")).0);
        assert!(worker_dispatch_readiness(Ok("launch\tagy\t")).0);
        assert!(!worker_dispatch_readiness(Ok("launch\t\t")).0);
    }

    #[test]
    fn only_providers_with_a_readiness_signal_fall_back_to_the_pane() {
        use crate::agent_launch::AgentProgram;
        assert!(has_pane_readiness_signal(AgentProgram::Codex));
        assert!(has_pane_readiness_signal(AgentProgram::Claude));
        assert!(has_pane_readiness_signal(AgentProgram::Antigravity));
        assert!(has_pane_readiness_signal(AgentProgram::Opencode));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newest_report_scans_metadata_pages_without_full_history() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("reports.sock");
        let mut newer = crate::collaboration_output::fixture();
        newer.status = RequestStatus::Completed;
        newer.reply = Some(tend::collaboration::CollaborationReply {
            report: None,
            status: RequestStatus::Completed,
            body: "review result".into(),
            artifacts: vec![],
            at: newer.created_at,
        });
        newer.id = "newer".into();
        newer.to.alias = Some("other".into());
        let mut older = newer.clone();
        older.id = "older".into();
        older.to.alias = Some("reviewer".into());
        tend::collaboration::CollaborationReadView::Metadata.apply(&mut newer);
        tend::collaboration::CollaborationReadView::Metadata.apply(&mut older);
        let cursor = tend::collaboration::CollaborationCursor {
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
            exact_session: false,
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
        assert!(peer_report_target_matches(&peer, "@tend-peer"));
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
        let mut other = peer_participant(AgentKind::Antigravity, "%3", AgentState::Idle);
        other.roles = vec!["rust".into()];
        let room = peer_room(current, vec![reviewer, other]);

        assert_eq!(select_peer(&room, "@reviewer").unwrap().unwrap().pane, "%2");
        assert!(select_peer(&room, "role:rust")
            .unwrap_err()
            .contains("multiple peers"));
    }

    #[test]
    fn review_auto_routing_uses_opposite_provider_without_overriding_explicit_targets() {
        for target in ["auto", "peer", "@peer", "@tend-peer"] {
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
                &tend::config::McpGuideConfig::default()
            )
            .unwrap(),
            crate::agent_launch::AgentProgram::Codex
        );
        assert_eq!(
            peer_spawn_program(
                &json!({}),
                &codex,
                "auto",
                &tend::config::McpGuideConfig::default()
            )
            .unwrap(),
            crate::agent_launch::AgentProgram::Claude
        );
        assert!(peer_spawn_program(
            &json!({ "spawn_agent": "claude" }),
            &codex,
            "@codex",
            &tend::config::McpGuideConfig::default()
        )
        .is_err());

        let guide = tend::config::McpGuideConfig {
            agent: Some("agy".into()),
            ..tend::config::McpGuideConfig::default()
        };
        assert_eq!(
            peer_spawn_program(&json!({}), &codex, "auto", &guide).unwrap(),
            crate::agent_launch::AgentProgram::Antigravity
        );
        // A config written before the Gemini CLI was retired still loads;
        // spawning through it says why instead of picking another CLI.
        let retired = tend::config::McpGuideConfig {
            agent: Some("gemini".into()),
            ..tend::config::McpGuideConfig::default()
        };
        let error = peer_spawn_program(&json!({}), &codex, "auto", &retired).unwrap_err();
        assert!(error.contains("no longer supported"), "{error}");
        // An explicit provider still wins over the stale default.
        assert_eq!(
            peer_spawn_program(
                &json!({ "spawn_agent": "claude" }),
                &codex,
                "auto",
                &retired
            )
            .unwrap(),
            crate::agent_launch::AgentProgram::Claude
        );
        assert_eq!(
            peer_spawn_program(&json!({}), &codex, "@codex", &retired).unwrap(),
            crate::agent_launch::AgentProgram::Codex
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
                "params": { "name": "tend_status", "arguments": {} },
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
                    "name": "tend_status",
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
                    "name": "tend_manage_tmux",
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

        // Forced, it routes through to the backend with a submit CR.
        let send = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {
                    "name": "tend_send_prompt",
                    "arguments": { "pane": "%1", "text": "hello", "submit": true, "socket": "default", "force": true },
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

        // tend_capture_pane returns the backend's screen text.
        let cap = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": { "name": "tend_capture_pane", "arguments": { "pane": "%1" } },
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
    async fn send_prompt_is_gated_on_an_idle_target_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-gated-send.sock");
        let (client, sends, tx, handle) = spawn_daemon(&sock).await;
        // tend_send_prompt is gated: an untracked (state unknown) pane is
        // refused before anything is typed.
        let gated = dispatch(
            &client,
            &json!({
                "jsonrpc": "2.0", "id": 40, "method": "tools/call",
                "params": {
                    "name": "tend_send_prompt",
                    "arguments": { "pane": "%1", "text": "hello", "submit": true, "socket": "default" },
                },
            }),
        )
        .await
        .unwrap();
        assert_eq!(gated["result"]["isError"], true, "{gated}");
        assert!(gated.to_string().contains("target_not_idle"), "{gated}");
        assert!(sends.lock().unwrap().is_empty());

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// A daemon that predates `send_prompt_state_gate_v1` would ignore
    /// `force` and type into a busy target, so a gated send refuses before
    /// asking it to type anything. A forced send still works there.
    #[tokio::test]
    async fn send_prompt_refuses_to_lose_the_gate_against_an_older_daemon() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("old-daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let kinds = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = kinds.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    record
                        .lock()
                        .unwrap()
                        .push(request["kind"].as_str().unwrap_or_default().to_string());
                    // No capabilities: an old daemon.
                    let _ = writer.write_all(b"{\"ok\":true}\n").await;
                }
            }
        });
        let client = Client::new(socket);
        let call = |force: bool| {
            json!({
                "jsonrpc":"2.0", "id":41, "method":"tools/call",
                "params":{"name":"tend_send_prompt", "arguments":{
                    "pane":"%1", "text":"hi", "socket":"default", "force":force}},
            })
        };
        let refused = dispatch(&client, &call(false)).await.unwrap();
        assert_eq!(refused["result"]["isError"], true, "{refused}");
        assert!(refused.to_string().contains("send_prompt_state_gate_v1"));
        assert!(!kinds
            .lock()
            .unwrap()
            .iter()
            .any(|kind| kind == "send_prompt"));
        let forced = dispatch(&client, &call(true)).await.unwrap();
        assert_ne!(forced["result"]["isError"], true, "{forced}");
        assert!(kinds
            .lock()
            .unwrap()
            .iter()
            .any(|kind| kind == "send_prompt"));
    }

    /// New MCP, older controller: the controller would drop `force` on the
    /// way to the relay, so a gated Fleet prompt is refused before any
    /// Fleet operation is issued.
    #[tokio::test]
    async fn a_gated_fleet_prompt_needs_a_controller_that_forwards_the_gate() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("old-controller.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let kinds = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = kinds.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    record
                        .lock()
                        .unwrap()
                        .push(request["kind"].as_str().unwrap_or_default().to_string());
                    // Has the local gate, not the Fleet forwarding one.
                    let _ = writer
                        .write_all(
                            b"{\"ok\":true,\"capabilities\":[\"send_prompt_state_gate_v1\"]}\n",
                        )
                        .await;
                }
            }
        });
        let refused = dispatch(
            &Client::new(socket),
            &json!({
                "jsonrpc":"2.0", "id":42, "method":"tools/call",
                "params":{"name":"tend_send_prompt", "arguments":{
                    "host":"mini", "pane":"%1", "text":"hi"}},
            }),
        )
        .await
        .unwrap();
        assert_eq!(refused["result"]["isError"], true, "{refused}");
        assert!(refused.to_string().contains("fleet_send_prompt_gate_v1"));
        let kinds = kinds.lock().unwrap();
        assert!(
            !kinds.iter().any(|kind| kind.starts_with("fleet_")),
            "no Fleet operation issued: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn send_prompt_refuses_untracked_and_mismatched_targets() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp-refused-send.sock");
        let (client, sends, tx, handle) = spawn_daemon(&sock).await;
        // Unknown panes and failed session preconditions must never inject.
        for arguments in [
            json!({"pane":"%1", "text":"stale", "socket":""}),
            json!({"pane":"%1", "text":"stale", "socket":"default", "session_id":"gone"}),
            json!({"pane":"%1", "text":"stale", "socket":"default", "session_id":123}),
        ] {
            let refused = dispatch(
                &client,
                &json!({
                    "jsonrpc":"2.0", "id":33, "method":"tools/call",
                    "params":{"name":"tend_send_prompt", "arguments":arguments},
                }),
            )
            .await
            .unwrap();
            assert_eq!(refused["result"]["isError"], true, "{refused}");
        }
        assert!(sends.lock().unwrap().is_empty());

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn send_prompt_uses_callers_server_context() {
        const PROBE: &str = "TEND_PROMPT_CONTEXT_PROBE";
        let Ok(case) = std::env::var(PROBE) else {
            for case in ["tmux", "rmux", "none", "explicit", "rmux-compat"] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child
                    .args([
                        "--exact",
                        "mcp::tests::send_prompt_uses_callers_server_context",
                        "--nocapture",
                    ])
                    .env(PROBE, case)
                    .env_remove("TMUX")
                    .env_remove("RMUX");
                match case {
                    "tmux" | "explicit" => {
                        child.env("TMUX", "/tmp/caller-tmux/amux,123,0");
                    }
                    "rmux" | "rmux-compat" => {
                        child
                            .env("RMUX", "/tmp/rmux-501/amux,123,0")
                            .env("TMUX", "/tmp/rmux-501/amux,123,0");
                    }
                    _ => {}
                }
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caller-context.sock");
        let sends = Arc::new(Mutex::new(Vec::new()));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let kind = if case == "rmux" {
            HostKind::Rmux
        } else {
            HostKind::Tmux
        };
        let pane = if case == "rmux" { "rmux:%9" } else { "%9" };
        let backend: SharedBackend = Arc::new(FakeBackend {
            sends: sends.clone(),
            kind,
            sockets: sockets.clone(),
        });
        // Empty registry: only the caller's context can authorize this target.
        let server = Server::new(sock.clone(), Store::shared()).with_backends(vec![backend]);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;
        // Forced: this checks endpoint selection, not the idle gate.
        let mut arguments = json!({"pane":pane, "text":"hello", "submit":false, "force":true});
        if case == "explicit" {
            arguments["socket"] = json!("/tmp/explicit/amux");
        }
        let response = dispatch(
            &Client::new(sock),
            &json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"tend_send_prompt", "arguments":arguments},
            }),
        )
        .await
        .unwrap();
        let expected = match case.as_str() {
            "tmux" => Some("/tmp/caller-tmux/amux"),
            "rmux" => Some("/tmp/rmux-501/amux"),
            "explicit" => Some("/tmp/explicit/amux"),
            _ => None,
        };
        if let Some(endpoint) = expected {
            assert_ne!(response["result"]["isError"], true, "{response}");
            assert_eq!(*sockets.lock().unwrap(), vec![Some(endpoint.into())]);
        } else {
            assert_eq!(response["result"]["isError"], true, "{response}");
            assert!(response.to_string().contains("unknown pane"), "{response}");
            assert!(sends.lock().unwrap().is_empty());
        }
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
                "params": { "name": "tend_status", "arguments": { "full": true } },
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
                    "name": "tend_wait_for_change",
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

    #[test]
    fn adaptive_reply_cost_matrix() {
        use tend::collaboration::instruction::short_inline_prompt;
        let participant = json!({"agent_kind":"codex",
            "agent_session_id":"01a1820e-a385-79d0-acfd-8ebda41e2d5b",
            "pane":"rmux:%1","room":{"host":"rmux","window_id":"@1"},
            "state":"idle","launch_purpose":"worker"});
        let mut rows = Vec::new();
        for (language, unit) in [("ascii", "a"), ("korean", "한"), ("escaped", "\"\n")] {
            for target in [16, 40, 160, 480, 512, 513, 640, 768, 769, 1024] {
                let body = unit.repeat(target / unit.len()) + &"a".repeat(target % unit.len());
                assert_eq!(body.len(), target);
                let mut request: CollaborationRequest = serde_json::from_value(json!({
                    "id":"req_0123456789abcdef_1","from":participant,"to":participant,
                    "kind":"review","work_mode":"read_only","body":body,
                    "expects_reply":true,"status":"claimed",
                    "created_at":time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap()
                })).unwrap();
                let inline = short_inline_prompt(&request).is_some();
                request.status = tend::collaboration::RequestStatus::Completed;
                request.reply = Some(
                    serde_json::from_value(json!({"status":"completed",
                    "body":"확인했습니다.","artifacts":[],"at":"2026-10-29T00:00:00Z"}))
                    .unwrap(),
                );
                let mut full = json!({"sent":true,"selected_peer":request.to,
                    "selection_reason":"explicit","expanded_skill":null,"spawned":null,
                    "thread_id":null,"parent_request_id":null,"workspace_id":null,
                    "work_id":null,"run_id":null,"kind":"review","work_mode":"read_only"});
                full.as_object_mut()
                    .unwrap()
                    .extend(request_outcome(&request).as_object().unwrap().clone());
                let compact =
                    crate::collaboration_output::short_call_outcome(full.clone(), &request);
                assert_eq!(compact["reply"], full["reply"]);
                let bytes =
                    |v: &Value| json_result(v)["content"][0]["text"].as_str().unwrap().len();
                let before_compact = inline && body.len() <= 512;
                rows.push(json!({"language":language,"target_bytes":target,
                    "body_bytes":body.len(),"reply_body_bytes":"확인했습니다.".len(),
                    "before_sender_result_bytes":bytes(if before_compact {&compact} else {&full}),
                    "sender_result_bytes":bytes(if inline {&compact} else {&full}),
                    "reply_receipt_bytes":bytes(&crate::collaboration_output::mutation_receipt(&request))}));
            }
        }
        if let Ok(path) = std::env::var("TEND_REPLY_MATRIX_REPORT") {
            std::fs::write(path, serde_json::to_string_pretty(&json!({
                "scope":"MCP result content text using production projections; fixed completed Korean reply; no live model latency or billing measurement",
                "rows":rows
            })).unwrap()).unwrap();
        }
    }

    #[tokio::test]
    async fn record_tools_validate_arguments_without_a_daemon() {
        let client = Client::new(std::path::PathBuf::from("/missing/tend-record-test.sock"));
        for (name, args) in [
            ("tend_agent_find", json!({})),
            ("tend_agent_find", json!({"query":"task","limit":0})),
            ("tend_agent_find", json!({"query":" ","limit":1})),
            (
                "tend_agent_find",
                json!({"query":"task","exclude_sessions":[1]}),
            ),
            (
                "tend_agent_search",
                json!({"providers":["unknown"],"query":"bucket"}),
            ),
            (
                "tend_agent_read",
                json!({"event_id":"test","unexpected":true}),
            ),
        ] {
            let reply=dispatch(&client,&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).await.unwrap();
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            assert!(!reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("socket"));
        }
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
        let client = Client::new(std::path::PathBuf::from("/nonexistent/tend.sock"));

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
            dir.path().join("tend"),
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
    #[tokio::test]
    async fn an_idle_server_reexecs_only_after_its_binary_is_replaced() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("tend");
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let watch = UpgradeWatch {
            path: binary.clone(),
            started: BinaryStamp::of(&binary).unwrap(),
            rejected: None,
            reexec: Box::new(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
        };
        let client = Client::new(std::path::PathBuf::from("/nonexistent/tend.sock"));
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, client_out) = tokio::io::duplex(4096);
        let handle = tokio::spawn(serve_with_config(
            client,
            BufReader::new(server_in),
            server_out,
            Arc::new(tend::config::Config::default()),
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

        // A config-incompatible install cannot take down this live pipe,
        // and the same rejected binary/config pair is probed only once.
        let probes = dir.path().join("probes");
        std::fs::write(
            &binary,
            format!("#!/bin/sh\nprintf p >> '{}'\nexit 1\n", probes.display()),
        )
        .unwrap();
        // A poll schedules an OS process; its completion is not bounded by
        // a few timer ticks on a busy host. Wait for the probe's evidence.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !std::fs::read_to_string(&probes).is_ok_and(|text| !text.is_empty()) {
                tokio::time::sleep(UPGRADE_POLL_INTERVAL).await;
            }
        })
        .await
        .expect("replacement binary was not probed");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read_to_string(&probes).unwrap(), "p");
        tokio::time::sleep(UPGRADE_POLL_INTERVAL * 3).await;
        assert_eq!(std::fs::read_to_string(&probes).unwrap(), "p");
        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        line.clear();
        client_out.read_line(&mut line).await.unwrap();
        assert!(line.contains("\"id\":2"));

        // Replaced: the next idle poll hands over to the new binary once.
        std::fs::write(&binary, b"#!/bin/sh\n# validated new build\nexit 0\n").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(UPGRADE_POLL_INTERVAL).await;
            }
        })
        .await
        .expect("validated replacement did not trigger reexec");
        tokio::time::sleep(UPGRADE_POLL_INTERVAL * 3).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(client_in);
        handle.await.unwrap().unwrap();
    }

    #[test]
    fn initialize_advertises_tool_list_changes() {
        let init = initialize_result(&tend::config::Config::default());
        assert_eq!(init["capabilities"]["tools"]["listChanged"], true);
    }

    #[tokio::test]
    async fn serve_reports_parse_error_for_garbage_line() {
        let client = Client::new(std::path::PathBuf::from("/nonexistent/tend.sock"));
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

    /// The core non-blocking property: a slow `tend_wait_for_change` in flight
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
                  {\"name\":\"tend_wait_for_change\",\"arguments\":{\"timeout_secs\":2}}}\n",
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
    fn task_contract_fixture() -> Value {
        json!({"version":1,"task_id":"review-current","objective":"Review the exact commit","basis_revision":"a".repeat(40),"acceptance_criteria":["Check correctness"]})
    }

    #[test]
    fn task_contract_tools_advertise_and_parse_the_same_bounded_contract() {
        let defs = tool_definitions(&tend::config::Config::default());
        let tool = defs.iter().find(|t| t["name"] == "tend_call_peer").unwrap();
        assert_eq!(
            tool["inputSchema"]["properties"]["task_contract"]["properties"]["version"]["const"],
            1
        );
        let reply = defs.iter().find(|t| t["name"] == "tend_reply").unwrap();
        assert!(reply["inputSchema"]["properties"]["report"].is_object());
        let args = json!({"task_contract":task_contract_fixture()});
        let request = new_request_from_args(
            &args,
            RequestKind::Review,
            "Review".into(),
            true,
            WorkMode::ReadOnly,
            vec![],
        )
        .unwrap();
        assert_eq!(request.task_contract.unwrap().task_id, "review-current");
        let mut bad = args;
        bad["task_contract"]["version"] = json!(2);
        assert!(new_request_from_args(
            &bad,
            RequestKind::Review,
            "Review".into(),
            true,
            WorkMode::ReadOnly,
            vec![]
        )
        .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn contract_send_and_reply_fail_closed_against_an_older_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("old-daemon.sock");
        let server = crate::collaboration_output::mock_collaboration_responses(
            &socket,
            vec![
                (
                    json!({"/kind":"collaboration_send_contract"}),
                    json!({"ok":false,"error":"unknown request kind: collaboration_send_contract"}),
                ),
                (
                    json!({"/kind":"collaboration_reply_contract"}),
                    json!({"ok":false,"error":"unknown request kind: collaboration_reply_contract"}),
                ),
            ],
        );
        let client = Client::new(socket);
        let origin: CollaborationOrigin =
            serde_json::from_value(json!({"pane":"%1","agent_session_id":"sender"})).unwrap();
        let request = new_request_from_args(
            &json!({"task_contract":task_contract_fixture()}),
            RequestKind::Review,
            "Review".into(),
            true,
            WorkMode::ReadOnly,
            vec![],
        )
        .unwrap();
        let error = client
            .collaboration_send(&origin, "%2", &request)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown request kind"));
        let report = tend::collaboration::TaskReport {
            attempt_id: "attempt".into(),
            basis_revision: Some("a".repeat(40)),
            criteria: vec![],
            evidence: vec![],
        };
        let error = client
            .collaboration_reply_report_view(
                &origin,
                "request",
                RequestStatus::Completed,
                "Done",
                &[],
                Some(&report),
                tend::collaboration::CollaborationReadView::Full,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown request kind"));
        server.await.unwrap();
    }
    #[test]
    fn peer_context_is_bounded_deduplicated_and_before_flag_separator() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("cwd");
        let inputs = root.path().join("inputs");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&inputs).unwrap();
        let file = inputs.join("evidence.json");
        std::fs::write(&file, "{}").unwrap();
        let args = json!({"paths":[inputs, file, cwd.join("future-output")]});
        let options = vec![
            "--permission-mode".into(),
            "default".into(),
            "--".into(),
            "body".into(),
        ];
        let out = peer_context_options(
            crate::agent_launch::AgentProgram::Claude,
            options,
            &args,
            cwd.to_str(),
        )
        .unwrap();
        assert_eq!(&out[..2], ["--permission-mode", "default"]);
        assert_eq!(
            out[2],
            format!(
                "--add-dir={}",
                tend::paths::canonicalize(&inputs).unwrap().display()
            )
        );
        assert_eq!(&out[3..], ["--", "body"]);
    }
    #[test]
    #[cfg(unix)]
    fn peer_context_never_authorizes_the_filesystem_root() {
        let args = json!({"paths":["/"]});
        assert!(peer_context_options(
            crate::agent_launch::AgentProgram::Claude,
            vec![],
            &args,
            Some("/tmp")
        )
        .unwrap()
        .is_empty());
    }
    #[test]
    fn peer_context_files_and_codex_do_not_expand_directory_permissions() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("cwd");
        let inputs = root.path().join("inputs");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&inputs).unwrap();
        let file = inputs.join("one.txt");
        std::fs::write(&file, "data").unwrap();
        let original = vec!["--model".into(), "configured".into()];
        for (program, args) in [
            (
                crate::agent_launch::AgentProgram::Claude,
                json!({"paths":[file]}),
            ),
            (
                crate::agent_launch::AgentProgram::Codex,
                json!({"paths":[inputs]}),
            ),
        ] {
            assert_eq!(
                peer_context_options(program, original.clone(), &args, cwd.to_str()).unwrap(),
                original
            );
        }
    }
    #[test]
    fn peer_context_never_adds_home_or_private_configuration_roots() {
        let home = std::path::Path::new("/Users/fixture");
        for path in [
            "/",
            "/Users/fixture",
            "/Users/fixture/.ssh",
            "/Users/fixture/.config/asx",
            "/Users/fixture/Library/Keychains",
            "/Users/fixture/AppData/Roaming",
        ] {
            assert!(
                !safe_peer_context_directory(std::path::Path::new(path), Some(home)),
                "{path}"
            );
        }
        assert!(safe_peer_context_directory(
            std::path::Path::new("/Users/fixture/src/project/.context"),
            Some(home)
        ));
    }
    #[test]
    fn swap_same_when_idle_translates_and_rejects_read_options() {
        let args = json!({"all": true, "to": "same", "when_idle": true, "idle_timeout": "30m", "detach": true});
        let plan = swap_plan(&args, None, None).unwrap();
        assert!(plan.argv.contains(&"--when-idle".to_owned()));
        assert!(plan.argv.contains(&"--idle-timeout=30m".to_owned()));
        assert!(plan.argv.contains(&"--to=same".to_owned()));
        assert!(swap_plan(
            &json!({"action": "status", "job": "k", "when_idle": true}),
            None,
            None
        )
        .is_err());
        assert!(swap_plan(&json!({"idle_timeout": "1m"}), None, None).is_err());
    }
}
