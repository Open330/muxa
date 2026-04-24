//! Unix-domain-socket IPC.
//!
//! Wire format: line-delimited JSON. Each connection is one request/response
//! for query calls, or one-shot ingest for events.
//!
//! Protocol (all messages are single JSON objects, \n-terminated):
//!
//!   → {"kind":"ingest","event":<AgentEvent>}            // fire-and-forget
//!     ← {"ok":true}
//!
//!   → {"kind":"snapshot"}
//!     ← {"ok":true,"agents":[...]}
//!
//!   → {"kind":"by_pane","pane":"%12"}
//!     ← {"ok":true,"agents":[...]}

use crate::event::AgentEvent;
use crate::state::{Agent, SharedStore};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Ingest { event: AgentEvent },
    Snapshot,
    ByPane { pane: String },
    BySession { session_id: String },
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentWire>>,
}

#[derive(Debug, Serialize)]
pub struct AgentWire {
    pub kind: String,
    pub session_id: String,
    pub pane: Option<String>,
    pub cwd: Option<String>,
    pub state: String,
    pub last_prompt: Option<String>,
    pub last_notification: Option<String>,
    pub model: Option<String>,
    pub context_used_pct: Option<f32>,
    pub cost_usd: Option<f64>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_activity_at: time::OffsetDateTime,
}

impl From<Agent> for AgentWire {
    fn from(a: Agent) -> Self {
        Self {
            kind: serde_json::to_string(&a.kind)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
            session_id: a.session_id,
            pane: a.pane,
            cwd: a.cwd,
            state: serde_json::to_string(&a.state)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
            last_prompt: a.last_prompt,
            last_notification: a.last_notification,
            model: a.model,
            context_used_pct: a.context_used_pct,
            cost_usd: a.cost_usd,
            started_at: a.started_at,
            last_activity_at: a.last_activity_at,
        }
    }
}

impl Response {
    fn ok() -> Self {
        Self { ok: true, error: None, agents: None }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), agents: None }
    }
    fn with_agents(agents: Vec<Agent>) -> Self {
        Self {
            ok: true,
            error: None,
            agents: Some(agents.into_iter().map(AgentWire::from).collect()),
        }
    }
}

pub async fn serve(socket_path: &Path, store: SharedStore) -> Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)
            .with_context(|| format!("removing stale socket at {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding unix socket at {}", socket_path.display()))?;
    tracing::info!(socket = %socket_path.display(), "listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, store).await {
                tracing::warn!(error = %e, "connection handler failed");
            }
        });
    }
}

async fn handle(stream: UnixStream, store: SharedStore) -> Result<()> {
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
            Ok(Request::Ingest { event }) => {
                tracing::debug!(?event, "ingest");
                store.apply(&event).await;
                Response::ok()
            }
            Ok(Request::Snapshot) => Response::with_agents(store.snapshot().await),
            Ok(Request::ByPane { pane }) => Response::with_agents(store.by_pane(&pane).await),
            Ok(Request::BySession { session_id }) => {
                let agents = store
                    .by_session(&session_id)
                    .await
                    .into_iter()
                    .collect::<Vec<_>>();
                Response::with_agents(agents)
            }
            Err(e) => Response::err(format!("bad request: {e}")),
        };

        let mut bytes = serde_json::to_vec(&resp)?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
        writer.flush().await?;
    }
}

/// Fire-and-forget ingest helper (used by the CLI `muxa ingest`).
pub async fn send_ingest(socket_path: &Path, event: &AgentEvent) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path).await?;
    let req = serde_json::json!({ "kind": "ingest", "event": event });
    let mut bytes = serde_json::to_vec(&req)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    Ok(())
}

pub async fn query(socket_path: &Path, req: &serde_json::Value) -> Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket_path).await?;
    let mut bytes = serde_json::to_vec(req)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    Ok(serde_json::from_str(line.trim())?)
}
