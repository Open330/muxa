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
use muxa::ipc::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(trimmed) {
            Ok(req) => dispatch(&client, &req).await,
            Err(e) => Some(error_response(
                &Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        // Notifications (and parse-error-on-a-notification) produce no
        // response; only write when there is one.
        if let Some(resp) = response {
            let mut bytes = serde_json::to_vec(&resp)?;
            bytes.push(b'\n');
            stdout.write_all(&bytes).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// Handle one JSON-RPC message. Returns `Some(response)` for requests and
/// `None` for notifications (messages without an `id`).
async fn dispatch(client: &Client, req: &Value) -> Option<Value> {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = req.get("id").cloned();

    // A message without an `id` is a notification: never answered.
    let Some(id) = id else {
        // The only notification we expect is `notifications/initialized`;
        // anything else is harmlessly ignored.
        return None;
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
        "instructions": "muxa control plane. Use muxa_status to see what agents \
            are doing, muxa_send_prompt to drive one, muxa_capture_pane to read \
            its screen, and muxa_wait_for_change to block until an agent changes \
            state.",
    })
}

/// The five tools this server exposes, with JSON-Schema `inputSchema`s.
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
    ]
}

/// Execute a `tools/call`. Returns `Err` only for protocol-level problems
/// (missing tool name, malformed params); a tool that runs but fails to do
/// its job returns an `isError` result so the calling model sees the
/// message rather than a transport fault.
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
                Ok(()) => text_result(&format!(
                    "sent {} chars to {pane}{}",
                    text.len(),
                    if submit { " and submitted" } else { "" },
                )),
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
        other => Ok(error_result(&format!("unknown tool: {other}"))),
    }
}

/// `muxa_wait_for_change`: stream transitions from the daemon until one
/// matches (optionally scoped to a pane) or the timeout elapses.
async fn wait_for_change(client: &Client, args: &Value) -> Value {
    let pane = args.get("pane").and_then(Value::as_str);
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WAIT_SECS)
        .clamp(1, MAX_WAIT_SECS);

    let mut stream = match client.subscribe().await {
        Ok(s) => s,
        Err(e) => return error_result(&format!("subscribe failed: {e}")),
    };

    let deadline = Duration::from_secs(timeout_secs);
    let outcome = tokio::time::timeout(deadline, async {
        loop {
            match stream.recv().await {
                Ok(Some(t)) => {
                    if pane.is_none_or(|p| t.agent.pane.as_deref() == Some(p)) {
                        return Some(t);
                    }
                }
                // Daemon closed the stream (shutdown) or a read error —
                // either way, stop waiting.
                Ok(None) | Err(_) => return None,
            }
        }
    })
    .await;

    match outcome {
        Ok(Some(t)) => json_result(&json!({
            "changed": true,
            "from": t.from,
            "to": t.to,
            "agent": &*t.agent,
        })),
        Ok(None) => json_result(&json!({
            "changed": false,
            "reason": "stream closed before a matching change",
        })),
        Err(_) => json_result(&json!({
            "changed": false,
            "reason": "timeout",
            "timeout_secs": timeout_secs,
        })),
    }
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
    use muxa::ipc::Server;
    use muxa::state::Store;
    use muxa::tmux::PaneInfo;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
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
}
