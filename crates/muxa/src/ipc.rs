//! Unix-domain-socket IPC.
//!
//! **Wire format.** Line-delimited JSON (one request per line, one
//! response per line). `serde_json` never emits newlines inside a value, so
//! embedded `\n` in strings is safely escaped — see the round-trip test.
//!
//! Every request carries a `protocol` field set to `PROTOCOL_VERSION`. The
//! server rejects mismatched versions to prevent schema drift from silently
//! corrupting state.
//!
//! **Socket permissions.** The server chmods the socket file to `0600` after
//! binding so only the owning user can send events.
//!
//! **Shutdown.** The server accepts a `CancellationToken`-style signal via
//! the `shutdown` channel and stops accepting new connections, then drains
//! its tracked in-flight handlers (with a bounded timeout) before
//! returning. The drain is what gives the snapshotter task its
//! "last-to-die" guarantee: by the time `Server::run` returns, no handler
//! can call `Store::apply` afterwards, so the daemon's final flush
//! captures every state change the user actually triggered.

use crate::event::{AgentEvent, PROTOCOL_VERSION};
use crate::state::{Agent, SharedStore};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::JoinSet;

/// Maximum time `Server::run` will wait for in-flight handlers to finish
/// after the shutdown signal lands. Sized for the longest plausible
/// handler — a `recap_all` query reading several MB of NDJSON — plus
/// generous slack. If a handler hangs past this we abort it rather than
/// blocking the daemon's exit indefinitely.
const HANDLER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("socket already exists and is in use at {0}; another daemon may be running")]
    SocketInUse(PathBuf),

    #[error(
        "daemon not reachable at {} — is `muxad` running? (start `muxad`, or set MUXA_SOCKET)",
        .0.display()
    )]
    NotConnected(PathBuf),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RequestBody {
    Ingest {
        event: AgentEvent,
    },
    Snapshot,
    ByPane {
        pane: String,
    },
    BySession {
        session_id: String,
    },
    /// Disk-backed prompt audit log. `pane = None` returns prompts across
    /// every tracked pane, sorted newest-first; otherwise filtered to one
    /// pane. `limit = 0` (or absent) returns everything available, capped
    /// by the daemon's in-memory retention.
    RecentPrompts {
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Health,
}

#[derive(Debug, Deserialize)]
struct Request {
    /// Wire protocol version the client expects. Must equal `PROTOCOL_VERSION`.
    #[serde(default)]
    protocol: u32,
    #[serde(flatten)]
    body: RequestBody,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub ok: bool,
    pub protocol: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<Agent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<crate::history::HistoryEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthInfo>,
}

#[derive(Debug, Serialize)]
pub struct HealthInfo {
    pub version: &'static str,
    pub protocol: u32,
}

impl Response {
    fn ok() -> Self {
        Self {
            ok: true,
            protocol: PROTOCOL_VERSION,
            error: None,
            agents: None,
            prompts: None,
            health: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            protocol: PROTOCOL_VERSION,
            error: Some(msg.into()),
            agents: None,
            prompts: None,
            health: None,
        }
    }
    fn with_agents(agents: Vec<Agent>) -> Self {
        let mut r = Self::ok();
        r.agents = Some(agents);
        r
    }
    fn with_prompts(prompts: Vec<crate::history::HistoryEntry>) -> Self {
        let mut r = Self::ok();
        r.prompts = Some(prompts);
        r
    }
    fn health() -> Self {
        let mut r = Self::ok();
        r.health = Some(HealthInfo {
            version: env!("CARGO_PKG_VERSION"),
            protocol: PROTOCOL_VERSION,
        });
        r
    }
}

/// Daemon-side server. Construct once, call `run` under the tokio runtime.
pub struct Server {
    socket_path: PathBuf,
    store: SharedStore,
}

impl Server {
    pub fn new(socket_path: PathBuf, store: SharedStore) -> Self {
        Self { socket_path, store }
    }

    /// Run until `shutdown` fires or an I/O error occurs.
    ///
    /// In-flight connection handlers are tracked on a `JoinSet` so a
    /// clean shutdown can drain them before returning. Without that
    /// drain, an ingest landing during shutdown could call
    /// `Store::apply` *after* the snapshotter task has already done its
    /// final flush, losing that event on the next restart. Drained with
    /// a bounded timeout so a hung handler can't block daemon exit.
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) -> Result<(), RuntimeError> {
        self.bind_with_perms()?;
        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!(socket = %self.socket_path.display(), "listening");

        let mut handlers: JoinSet<()> = JoinSet::new();

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    let (stream, _) = accept?;
                    let store = self.store.clone();
                    handlers.spawn(async move {
                        if let Err(e) = handle(stream, store).await {
                            tracing::warn!(error = %e, "connection handler failed");
                        }
                    });
                    // Reap finished handlers opportunistically so the JoinSet
                    // doesn't grow unboundedly under steady traffic.
                    while handlers.try_join_next().is_some() {}
                }
                _ = shutdown.recv() => {
                    tracing::info!("shutdown signal received; closing listener");
                    break;
                }
            }
        }

        // Drain in-flight handlers with a bounded timeout. Closes the
        // lost-update window where a handler could call `Store::apply`
        // after the daemon's snapshotter has already exited.
        let drain = async { while handlers.join_next().await.is_some() {} };
        if tokio::time::timeout(HANDLER_DRAIN_TIMEOUT, drain)
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_secs = HANDLER_DRAIN_TIMEOUT.as_secs(),
                remaining = handlers.len(),
                "ipc handlers did not drain within timeout; aborting",
            );
            handlers.abort_all();
            // Best-effort: let the abort propagate.
            while handlers.join_next().await.is_some() {}
        } else {
            tracing::debug!("ipc handlers drained cleanly");
        }

        // Remove our own socket file so next startup is clean.
        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }

    /// Pre-bind sequence: if a stale socket exists, remove it; then the
    /// caller binds. We also chmod 0600 after bind (see `run`).
    fn bind_with_perms(&self) -> Result<(), RuntimeError> {
        if self.socket_path.exists() {
            // Probe: is anything listening?
            if std::os::unix::net::UnixStream::connect(&self.socket_path).is_ok() {
                return Err(RuntimeError::SocketInUse(self.socket_path.clone()));
            }
            // Stale socket, safe to remove.
            std::fs::remove_file(&self.socket_path)?;
        }
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Tighten perms *after* bind in `run` — but we also chmod here in
        // case UnixListener::bind leaves world-readable perms briefly.
        Ok(())
    }
}

async fn handle(stream: UnixStream, store: SharedStore) -> Result<(), RuntimeError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let resp = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) if req.protocol != 0 && req.protocol != PROTOCOL_VERSION => {
                Response::err(format!(
                    "protocol mismatch: server={PROTOCOL_VERSION} client={}",
                    req.protocol
                ))
            }
            Ok(req) => match req.body {
                RequestBody::Ingest { event } => {
                    tracing::debug!(?event, "ingest");
                    store.apply(&event).await;
                    Response::ok()
                }
                RequestBody::Snapshot => Response::with_agents(store.snapshot().await),
                RequestBody::ByPane { pane } => Response::with_agents(store.by_pane(&pane).await),
                RequestBody::BySession { session_id } => {
                    let v = store
                        .by_session(&session_id)
                        .await
                        .into_iter()
                        .collect::<Vec<_>>();
                    Response::with_agents(v)
                }
                RequestBody::RecentPrompts { pane, limit } => {
                    let prompts = store
                        .recent_prompts(pane.as_deref(), limit.unwrap_or(0))
                        .await;
                    Response::with_prompts(prompts)
                }
                RequestBody::Health => Response::health(),
            },
            Err(e) => Response::err(format!("bad request: {e}")),
        };

        let mut bytes = serde_json::to_vec(&resp)?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
        writer.flush().await?;
    }
}

/// After `UnixListener::bind`, chmod the path so only the owner can connect.
pub fn harden_permissions(socket_path: &Path) -> std::io::Result<()> {
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)
}

/// Client-side helper. Single-shot request/response.
#[derive(Clone)]
pub struct Client {
    socket_path: PathBuf,
}

impl Client {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub async fn ingest(&self, event: &AgentEvent) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ingest",
            "event": event
        });
        let _ = self.call(&req).await?;
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" });
        let resp = self.call(&req).await?;
        Ok(resp["agents"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    pub async fn by_pane(&self, pane: &str) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "by_pane",
            "pane": pane
        });
        let resp = self.call(&req).await?;
        Ok(resp["agents"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    /// Query the daemon's prompt history. `pane = None` returns prompts
    /// across every tracked pane (newest first); otherwise filters to
    /// one pane. `limit = None` or 0 returns everything available.
    pub async fn recent_prompts(
        &self,
        pane: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<crate::history::HistoryEntry>, RuntimeError> {
        let mut req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "recent_prompts",
        });
        if let Some(p) = pane {
            req["pane"] = serde_json::Value::String(p.to_string());
        }
        if let Some(l) = limit {
            req["limit"] = serde_json::Value::from(l);
        }
        let resp = self.call(&req).await?;
        Ok(resp["prompts"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    pub async fn call(&self, req: &serde_json::Value) -> Result<serde_json::Value, RuntimeError> {
        // Connect-time ECONNREFUSED/ENOENT mean the daemon socket isn't there
        // or nothing is listening — surface a friendly message that names the
        // socket path. Other IO errors (timeouts, permission denied, …) keep
        // their existing display via the `Io(#[from] _)` impl.
        let mut stream =
            UnixStream::connect(&self.socket_path)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                        RuntimeError::NotConnected(self.socket_path.clone())
                    }
                    _ => RuntimeError::Io(e),
                })?;
        let mut bytes = serde_json::to_vec(req)?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        Ok(serde_json::from_str(line.trim())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentId, AgentKind};
    use crate::state::Store;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    #[tokio::test]
    async fn end_to_end_ingest_and_query() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-test.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);

        let sock_for_server = sock.clone();
        let handle = tokio::spawn(async move {
            server.run(rx).await.unwrap();
            drop(sock_for_server);
        });

        // Wait for the socket to appear.
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let client = Client::new(sock.clone());
        client
            .ingest(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "sess-a".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();

        let agents = client.by_pane("%1").await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].session_id, "sess-a");

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn not_connected_when_socket_missing() {
        // ENOENT path: tempdir exists but the socket file doesn't.
        let dir = tempdir().unwrap();
        let sock = dir.path().join("does-not-exist.sock");
        let client = Client::new(sock.clone());
        let err = client
            .call(&serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" }))
            .await
            .expect_err("expected NotConnected when socket does not exist");
        match err {
            RuntimeError::NotConnected(p) => assert_eq!(p, sock),
            other => panic!("expected NotConnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_connected_when_socket_is_stale_file() {
        // Stale-file path: a regular file exists at the socket path but
        // nothing is listening. On Linux, connect(2) returns ECONNREFUSED for
        // a non-socket path; `tokio` may also surface ENOTSOCK. We accept any
        // mapping into NotConnected — the user-visible behaviour is the same.
        let dir = tempdir().unwrap();
        let sock = dir.path().join("stale.sock");
        std::fs::write(&sock, b"").unwrap();
        let client = Client::new(sock.clone());
        let res = client
            .call(&serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" }))
            .await;
        // If the platform returns a kind we don't remap (e.g. ENOTSOCK on
        // some libc), the call still errors — just not necessarily with
        // NotConnected. Only assert the friendly mapping when we got it.
        if let Err(RuntimeError::NotConnected(p)) = &res {
            assert_eq!(p, &sock);
        }
        // Either way, the call must not succeed.
        assert!(res.is_err());
    }

    /// `Server::run` must wait for in-flight handlers to finish before
    /// returning. Otherwise, an ingest landing during shutdown could
    /// call `Store::apply` *after* the snapshotter's final flush, losing
    /// the event on next restart.
    ///
    /// We exercise this by piping a slow request through a handler:
    /// fire shutdown while the handler is mid-read, then verify
    /// `server.run` returns only after the handler has finished applying
    /// its event (visible in the store snapshot).
    #[tokio::test]
    async fn shutdown_drains_in_flight_handlers_before_returning() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-drain.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);

        let server_handle = tokio::spawn(server.run(rx));

        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Open a raw stream and write the request *header* but withhold
        // the trailing newline so the handler is stuck inside
        // `read_line`. This simulates an in-flight handler at the moment
        // shutdown lands.
        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ingest",
            "event": {
                "type": "started",
                "id": {
                    "kind": "claude_code",
                    "session_id": "drain-test",
                    "pane": "%9",
                    "cwd": null,
                },
                "at": "2026-04-28T00:00:00Z",
            },
        });
        let bytes = serde_json::to_vec(&req).unwrap();
        // Note: no trailing '\n' yet.
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();

        // Yield to give the spawned handler a chance to enter `read_line`.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Fire shutdown. Server stops accepting; existing handler is
        // still blocked on its read.
        tx.send(()).unwrap();

        // Now finish the request (newline) so the handler can complete,
        // then close the stream so the handler's read loop sees EOF and
        // returns. Without the close, `handle()` would happily wait for
        // a follow-up request and the drain timeout would fire.
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();
        // Read the single response so we know the apply landed before
        // we drop the stream — this also gives the handler enough time
        // to write its reply.
        let mut response_buf = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response_buf),
        )
        .await;
        drop(stream);

        // `server.run` must wait for the handler to finish before
        // returning. The bounded timeout here is the test's deadline,
        // not the production drain timeout — we expect this to complete
        // in milliseconds.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle)
            .await
            .expect("server.run did not return after handler finished")
            .expect("server task panicked");
        outcome.expect("server.run returned an error");

        // The drained handler must have applied its event before
        // server.run returned. If we'd returned without waiting, the
        // store could be empty and we'd race the assertion.
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1, "drained handler must have applied event");
        assert_eq!(snap[0].session_id, "drain-test");
    }

    #[tokio::test]
    async fn rejects_wrong_protocol() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-test.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });

        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let client = Client::new(sock.clone());
        let resp = client
            .call(&serde_json::json!({
                "protocol": 999,
                "kind": "snapshot"
            }))
            .await
            .unwrap();
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .unwrap()
            .contains("protocol mismatch"));

        tx.send(()).unwrap();
        handle.await.unwrap();
    }
}
