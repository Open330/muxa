//! Append-only audit ledger for collaboration IPC calls.
//!
//! The durable mailbox records the request lifecycle and message bodies. This
//! ledger answers a different question: which local process invoked which
//! collaboration operation while representing which pane agent? Bodies are
//! deliberately excluded so the audit trail does not become a second message
//! store.

use crate::collaboration::{
    CollaborationOrigin, CollaborationProvenance, Participant, RequestMailbox, RequestStatus,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

#[cfg(unix)]
const COLLABORATION_AUDIT_FILE_MODE: u32 = 0o600;

pub const COLLABORATION_AUDIT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationAuditOperation {
    Context,
    SetIdentity,
    Send,
    Inbox,
    List,
    Reply,
    Get,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollaborationAuditEntry {
    #[serde(default)]
    pub v: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub operation: CollaborationAuditOperation,
    /// OS-observed caller plus its relationship to `represented_origin`.
    pub actor: CollaborationProvenance,
    /// Pane identity supplied by the caller and resolved as the represented
    /// agent. This remains advisory authority, not caller authentication.
    pub represented_origin: CollaborationOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub represented_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub represented_session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<RequestMailbox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RequestStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_count: Option<usize>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CollaborationAuditContext {
    pub operation: CollaborationAuditOperation,
    pub represented_origin: CollaborationOrigin,
    pub target: Option<String>,
    pub request_id: Option<String>,
    pub mailbox: Option<RequestMailbox>,
    pub status: Option<RequestStatus>,
    pub message_bytes: Option<usize>,
}

impl CollaborationAuditContext {
    pub fn new(
        operation: CollaborationAuditOperation,
        represented_origin: CollaborationOrigin,
    ) -> Self {
        Self {
            operation,
            represented_origin,
            target: None,
            request_id: None,
            mailbox: None,
            status: None,
            message_bytes: None,
        }
    }

    pub fn finish(
        self,
        actor: CollaborationProvenance,
        represented: Option<&Participant>,
        response_request_id: Option<&str>,
        result_count: Option<usize>,
        error: Option<&str>,
    ) -> CollaborationAuditEntry {
        CollaborationAuditEntry {
            v: COLLABORATION_AUDIT_SCHEMA_VERSION,
            at: OffsetDateTime::now_utc(),
            operation: self.operation,
            actor,
            represented_origin: self.represented_origin,
            represented_session_id: represented.map(|p| p.agent_session_id.clone()),
            represented_session_name: represented.and_then(|p| p.tmux_session_name.clone()),
            target: self.target,
            request_id: self
                .request_id
                .or_else(|| response_request_id.map(str::to_string)),
            mailbox: self.mailbox,
            status: self.status,
            message_bytes: self.message_bytes,
            result_count,
            ok: error.is_none(),
            error: error.map(str::to_string),
        }
    }
}

/// Serialized appender. Audit persistence is deliberately best-effort: a
/// full or unavailable data directory must never reduce collaboration
/// authority or make a message send fail.
#[derive(Debug)]
pub struct CollaborationAuditLog {
    path: Option<PathBuf>,
    append_lock: Mutex<()>,
    memory: Mutex<Vec<CollaborationAuditEntry>>,
}

impl CollaborationAuditLog {
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            path: None,
            append_lock: Mutex::new(()),
            memory: Mutex::new(Vec::new()),
        })
    }

    pub fn at_path(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path: Some(path),
            append_lock: Mutex::new(()),
            memory: Mutex::new(Vec::new()),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub async fn append(&self, entry: CollaborationAuditEntry) {
        let _append = self.append_lock.lock().await;
        let Some(path) = self.path.as_deref() else {
            self.memory.lock().await.push(entry);
            return;
        };
        if let Err(error) = append_entry(path, &entry).await {
            tracing::warn!(%error, path = %path.display(), "collaboration audit append failed");
        }
    }

    #[cfg(test)]
    pub async fn entries(&self) -> Vec<CollaborationAuditEntry> {
        self.memory.lock().await.clone()
    }
}

pub async fn load(path: &Path) -> std::io::Result<Vec<CollaborationAuditEntry>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<CollaborationAuditEntry>(trimmed) {
            if entry.v == COLLABORATION_AUDIT_SCHEMA_VERSION {
                entries.push(entry);
            }
        }
    }
    Ok(entries)
}

async fn append_entry(path: &Path, entry: &CollaborationAuditEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(COLLABORATION_AUDIT_FILE_MODE);
    let mut file = options.open(path).await?;
    let mut bytes = serde_json::to_vec(entry)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');
    file.write_all(&bytes).await?;
    file.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{
        CollaborationClientKind, CollaborationOriginMatch, CollaborationPaneEvidence,
        CollaborationProvenance,
    };

    fn entry() -> CollaborationAuditEntry {
        CollaborationAuditContext::new(
            CollaborationAuditOperation::Send,
            CollaborationOrigin {
                pane: "%3".into(),
                socket: Some("default".into()),
            },
        )
        .finish(
            CollaborationProvenance {
                client_kind: CollaborationClientKind::Watch,
                caller_pid: Some(123),
                caller_uid: Some(1000),
                caller_gid: Some(1000),
                executable: Some("muxa".into()),
                observed_pane: Some("%3".into()),
                pane_evidence: Some(CollaborationPaneEvidence::ProcessAncestry),
                origin_match: CollaborationOriginMatch::Matched,
            },
            None,
            Some("req_1"),
            Some(1),
            None,
        )
    }

    #[tokio::test]
    async fn appends_private_ndjson_without_message_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collaboration-audit.ndjson");
        let log = CollaborationAuditLog::at_path(path.clone());
        log.append(entry()).await;

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("req_1"));
        assert!(!raw.contains("message body"));
        let loaded = load(&path).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].operation, CollaborationAuditOperation::Send);
        assert_eq!(loaded[0].request_id.as_deref(), Some("req_1"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(&path)
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
