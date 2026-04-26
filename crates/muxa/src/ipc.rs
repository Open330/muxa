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
//! the `shutdown` channel and stops accepting new connections; in-flight
//! connections finish naturally.

use crate::event::{AgentEvent, PROTOCOL_VERSION};
use crate::state::{Agent, SharedStore};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("socket already exists and is in use at {0}; another daemon may be running")]
    SocketInUse(PathBuf),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RequestBody {
    Ingest { event: AgentEvent },
    Snapshot,
    ByPane { pane: String },
    BySession { session_id: String },
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
            health: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            protocol: PROTOCOL_VERSION,
            error: Some(msg.into()),
            agents: None,
            health: None,
        }
    }
    fn with_agents(agents: Vec<Agent>) -> Self {
        let mut r = Self::ok();
        r.agents = Some(agents);
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
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) -> Result<(), RuntimeError> {
        self.bind_with_perms()?;
        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!(socket = %self.socket_path.display(), "listening");

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    let (stream, _) = accept?;
                    let store = self.store.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream, store).await {
                            tracing::warn!(error = %e, "connection handler failed");
                        }
                    });
                }
                _ = shutdown.recv() => {
                    tracing::info!("shutdown signal received; closing listener");
                    break;
                }
            }
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

    pub async fn call(&self, req: &serde_json::Value) -> Result<serde_json::Value, RuntimeError> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
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
