//! Durable, same-room request/reply collaboration for pane-hosted agents.
//!
//! tmux supplies topology; muxad remains the broker. Messages are pinned to
//! the concrete agent session occupying a pane so a later process reusing the
//! pane never inherits stale work.

use crate::event::{AgentKind, AgentState};
use crate::state::Agent;
use crate::tmux::PaneInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock};

pub const COLLABORATION_SCHEMA_VERSION: u32 = 1;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct CollaborationOptions {
    pub enabled: bool,
    pub path: Option<PathBuf>,
    pub max_message_bytes: usize,
}

impl Default for CollaborationOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            max_message_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollaborationOrigin {
    pub pane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct RoomId {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    pub window_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Participant {
    pub agent_kind: AgentKind,
    pub agent_session_id: String,
    pub pane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    pub room: RoomId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_name: Option<String>,
    pub state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl Participant {
    pub fn label(&self) -> String {
        format!("{}@{}", self.agent_kind, self.pane)
    }

    fn same_endpoint(&self, other: &Self) -> bool {
        self.pane == other.pane
            && self.socket == other.socket
            && self.agent_session_id == other.agent_session_id
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Question,
    Review,
    Task,
    Notice,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    #[default]
    ReadOnly,
    Execute,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Queued,
    Claimed,
    Completed,
    Blocked,
    Declined,
    Failed,
    Expired,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestMailbox {
    Incoming,
    Sent,
    #[default]
    All,
}

impl RequestStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Blocked
                | Self::Declined
                | Self::Failed
                | Self::Expired
                | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollaborationReply {
    pub status: RequestStatus,
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollaborationRequest {
    pub id: String,
    pub from: Participant,
    pub to: Participant,
    pub kind: RequestKind,
    pub body: String,
    pub expects_reply: bool,
    pub work_mode: WorkMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    pub status: RequestStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub claimed_at: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub notified_at: Option<OffsetDateTime>,
    /// Set when muxad successfully injects a short reply notification. A
    /// sender that reads the result first also sets this to suppress a later
    /// redundant wake. The reply body itself remains in the mailbox.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub reply_notified_at: Option<OffsetDateTime>,
    /// Set only when the sender retrieves the terminal result. Kept separate
    /// from `reply_notified_at` so room context can report replies whose wake
    /// prompt landed but whose body has not yet been read.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub reply_read_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<CollaborationReply>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomContext {
    #[serde(rename = "self")]
    pub current: Participant,
    pub peers: Vec<Participant>,
    pub unread: usize,
    #[serde(default)]
    pub unread_replies: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRequest {
    pub kind: RequestKind,
    pub body: String,
    pub expects_reply: bool,
    pub work_mode: WorkMode,
    pub paths: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CollaborationError {
    #[error("agent collaboration is disabled; enable [collaboration].enabled")]
    Disabled,
    #[error("collaboration origin is not a tracked pane agent: {0}")]
    UnknownOrigin(String),
    #[error("collaboration origin is ambiguous across tmux servers: {0}")]
    AmbiguousOrigin(String),
    #[error("no peer is available in this tmux window")]
    NoPeer,
    #[error("target {0:?} is ambiguous in this tmux window")]
    AmbiguousTarget(String),
    #[error("target {0:?} is not a peer in this tmux window")]
    UnknownTarget(String),
    #[error("message is empty")]
    EmptyMessage,
    #[error("message exceeds the configured {0}-byte limit")]
    MessageTooLarge(usize),
    #[error("request not found: {0}")]
    NotFound(String),
    #[error("request {0} does not belong to the calling participant")]
    NotParticipant(String),
    #[error("reply status must be completed, blocked, declined, or failed")]
    InvalidReplyStatus,
    #[error("request {0} is already terminal")]
    AlreadyTerminal(String),
    #[error("request {0} has already been claimed and can no longer be cancelled")]
    AlreadyClaimed(String),
    #[error("persistence error: {0}")]
    Persistence(#[from] std::io::Error),
    #[error("invalid persisted mailbox: {0}")]
    InvalidSnapshot(#[from] serde_json::Error),
    #[error("unsupported collaboration schema: {0}")]
    UnsupportedSchema(u32),
}

#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    requests: Vec<CollaborationRequest>,
}

/// In-memory mailbox with an optional atomic JSON snapshot. Collaboration
/// traffic is low-volume, so rewriting the bounded mailbox after mutations is
/// simpler and safer than maintaining a database or partially-replayed log.
pub struct CollaborationStore {
    opts: CollaborationOptions,
    requests: RwLock<HashMap<String, CollaborationRequest>>,
    persist_lock: Mutex<()>,
}

impl CollaborationStore {
    pub fn in_memory(options: CollaborationOptions) -> Arc<Self> {
        Arc::new(Self {
            opts: CollaborationOptions {
                path: None,
                ..options
            },
            requests: RwLock::new(HashMap::new()),
            persist_lock: Mutex::new(()),
        })
    }

    pub async fn load(options: CollaborationOptions) -> Result<Arc<Self>, CollaborationError> {
        let mut requests = HashMap::new();
        if let Some(path) = options.path.as_ref() {
            match tokio::fs::read(path).await {
                Ok(bytes) => {
                    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
                    if snapshot.version != COLLABORATION_SCHEMA_VERSION {
                        return Err(CollaborationError::UnsupportedSchema(snapshot.version));
                    }
                    requests.extend(snapshot.requests.into_iter().map(|r| (r.id.clone(), r)));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Arc::new(Self {
            opts: options,
            requests: RwLock::new(requests),
            persist_lock: Mutex::new(()),
        }))
    }

    pub fn enabled(&self) -> bool {
        self.opts.enabled
    }

    pub async fn create(
        &self,
        from: Participant,
        to: Participant,
        input: NewRequest,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        let body = input.body.trim();
        if body.is_empty() {
            return Err(CollaborationError::EmptyMessage);
        }
        if body.len() > self.opts.max_message_bytes {
            return Err(CollaborationError::MessageTooLarge(
                self.opts.max_message_bytes,
            ));
        }
        let now = OffsetDateTime::now_utc();
        let request = CollaborationRequest {
            id: next_request_id(now),
            from,
            to,
            kind: input.kind,
            body: body.to_string(),
            expects_reply: input.expects_reply,
            work_mode: input.work_mode,
            paths: input.paths,
            status: RequestStatus::Queued,
            created_at: now,
            claimed_at: None,
            notified_at: None,
            reply_notified_at: None,
            reply_read_at: None,
            reply: None,
        };
        self.requests
            .write()
            .await
            .insert(request.id.clone(), request.clone());
        self.persist().await?;
        Ok(request)
    }

    pub async fn claim_for(
        &self,
        caller: &Participant,
    ) -> Result<Vec<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        let now = OffsetDateTime::now_utc();
        let mut changed = false;
        let mut inbox = Vec::new();
        {
            let mut requests = self.requests.write().await;
            for request in requests.values_mut() {
                if request.to.same_endpoint(caller)
                    && matches!(
                        request.status,
                        RequestStatus::Queued | RequestStatus::Claimed
                    )
                {
                    if request.status == RequestStatus::Queued {
                        request.status = if request.expects_reply {
                            RequestStatus::Claimed
                        } else {
                            RequestStatus::Completed
                        };
                        request.claimed_at = Some(now);
                        changed = true;
                    }
                    inbox.push(request.clone());
                }
            }
        }
        inbox.sort_by_key(|request| request.created_at);
        if changed {
            self.persist().await?;
        }
        Ok(inbox)
    }

    pub async fn reply(
        &self,
        caller: &Participant,
        request_id: &str,
        status: RequestStatus,
        body: String,
        artifacts: Vec<String>,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        if !matches!(
            status,
            RequestStatus::Completed
                | RequestStatus::Blocked
                | RequestStatus::Declined
                | RequestStatus::Failed
        ) {
            return Err(CollaborationError::InvalidReplyStatus);
        }
        if body.len() > self.opts.max_message_bytes {
            return Err(CollaborationError::MessageTooLarge(
                self.opts.max_message_bytes,
            ));
        }
        let updated = {
            let mut requests = self.requests.write().await;
            let request = requests
                .get_mut(request_id)
                .ok_or_else(|| CollaborationError::NotFound(request_id.to_string()))?;
            if !request.to.same_endpoint(caller) {
                return Err(CollaborationError::NotParticipant(request_id.to_string()));
            }
            if request.status.is_terminal() {
                return Err(CollaborationError::AlreadyTerminal(request_id.to_string()));
            }
            request.status = status;
            request.reply = Some(CollaborationReply {
                status,
                body,
                artifacts,
                at: OffsetDateTime::now_utc(),
            });
            request.clone()
        };
        self.persist().await?;
        Ok(updated)
    }

    pub async fn get_for(
        &self,
        caller: &Participant,
        request_id: &str,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        let mut changed = false;
        let request = {
            let mut requests = self.requests.write().await;
            let request = requests
                .get_mut(request_id)
                .ok_or_else(|| CollaborationError::NotFound(request_id.to_string()))?;
            let is_sender = request.from.same_endpoint(caller);
            if !is_sender && !request.to.same_endpoint(caller) {
                return Err(CollaborationError::NotParticipant(request_id.to_string()));
            }
            if is_sender && request.reply.is_some() && request.reply_read_at.is_none() {
                let now = OffsetDateTime::now_utc();
                request.reply_read_at = Some(now);
                request.reply_notified_at.get_or_insert(now);
                changed = true;
            }
            request.clone()
        };
        if changed {
            self.persist().await?;
        }
        Ok(request)
    }

    pub async fn list_for(
        &self,
        caller: &Participant,
        mailbox: RequestMailbox,
    ) -> Result<Vec<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        let mut requests: Vec<_> = self
            .requests
            .read()
            .await
            .values()
            .filter(|request| match mailbox {
                RequestMailbox::Incoming => request.to.same_endpoint(caller),
                RequestMailbox::Sent => request.from.same_endpoint(caller),
                RequestMailbox::All => {
                    request.from.same_endpoint(caller) || request.to.same_endpoint(caller)
                }
            })
            .cloned()
            .collect();
        requests.sort_by_key(|request| std::cmp::Reverse(request.created_at));
        Ok(requests)
    }

    pub async fn cancel_for(
        &self,
        caller: &Participant,
        request_id: &str,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        let cancelled = {
            let mut requests = self.requests.write().await;
            let request = requests
                .get_mut(request_id)
                .ok_or_else(|| CollaborationError::NotFound(request_id.to_string()))?;
            if !request.from.same_endpoint(caller) {
                return Err(CollaborationError::NotParticipant(request_id.to_string()));
            }
            match request.status {
                RequestStatus::Queued => request.status = RequestStatus::Cancelled,
                RequestStatus::Claimed => {
                    return Err(CollaborationError::AlreadyClaimed(request_id.to_string()));
                }
                _ => return Err(CollaborationError::AlreadyTerminal(request_id.to_string())),
            }
            request.clone()
        };
        self.persist().await?;
        Ok(cancelled)
    }

    pub async fn pending_unnotified(&self) -> Vec<CollaborationRequest> {
        self.requests
            .read()
            .await
            .values()
            .filter(|r| r.status == RequestStatus::Queued && r.notified_at.is_none())
            .cloned()
            .collect()
    }

    pub async fn pending_reply_unnotified(&self) -> Vec<CollaborationRequest> {
        self.requests
            .read()
            .await
            .values()
            .filter(|request| {
                request.status.is_terminal()
                    && request.reply.is_some()
                    && request.reply_notified_at.is_none()
            })
            .cloned()
            .collect()
    }

    pub async fn mark_notified(&self, request_id: &str) -> Result<(), CollaborationError> {
        let changed = {
            let mut requests = self.requests.write().await;
            requests.get_mut(request_id).is_some_and(|request| {
                if request.status == RequestStatus::Queued && request.notified_at.is_none() {
                    request.notified_at = Some(OffsetDateTime::now_utc());
                    true
                } else {
                    false
                }
            })
        };
        if changed {
            self.persist().await?;
        }
        Ok(())
    }

    pub async fn mark_reply_notified(&self, request_id: &str) -> Result<(), CollaborationError> {
        let changed = {
            let mut requests = self.requests.write().await;
            requests.get_mut(request_id).is_some_and(|request| {
                if request.status.is_terminal()
                    && request.reply.is_some()
                    && request.reply_notified_at.is_none()
                {
                    request.reply_notified_at = Some(OffsetDateTime::now_utc());
                    true
                } else {
                    false
                }
            })
        };
        if changed {
            self.persist().await?;
        }
        Ok(())
    }

    pub async fn unread_count(&self, participant: &Participant) -> usize {
        self.requests
            .read()
            .await
            .values()
            .filter(|r| r.status == RequestStatus::Queued && r.to.same_endpoint(participant))
            .count()
    }

    pub async fn unread_reply_count(&self, participant: &Participant) -> usize {
        self.requests
            .read()
            .await
            .values()
            .filter(|request| {
                request.from.same_endpoint(participant)
                    && request.reply.is_some()
                    && request.reply_read_at.is_none()
            })
            .count()
    }

    fn ensure_enabled(&self) -> Result<(), CollaborationError> {
        if self.opts.enabled {
            Ok(())
        } else {
            Err(CollaborationError::Disabled)
        }
    }

    async fn persist(&self) -> Result<(), CollaborationError> {
        let Some(path) = self.opts.path.as_ref() else {
            return Ok(());
        };
        let _guard = self.persist_lock.lock().await;
        let mut requests: Vec<_> = self.requests.read().await.values().cloned().collect();
        requests.sort_by_key(|request| request.created_at);
        let snapshot = Snapshot {
            version: COLLABORATION_SCHEMA_VERSION,
            requests,
        };
        let bytes = serde_json::to_vec_pretty(&snapshot)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(tmp, path).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        Ok(())
    }
}

/// Correlate live agent rows with pane topology, keeping the newest live
/// agent when adapter/synthetic rows briefly overlap on the same pane.
pub fn participants_from(agents: &[Agent], panes: &[PaneInfo]) -> Vec<Participant> {
    let mut resolved: HashMap<(Option<String>, String), (OffsetDateTime, Participant)> =
        HashMap::new();
    for agent in agents.iter().filter(|agent| {
        agent.pane.is_some() && agent.state != AgentState::Stopped && agent.kind != AgentKind::Task
    }) {
        let pane_id = agent.pane.as_ref().expect("filtered pane");
        let agent_socket = agent
            .tmux_socket
            .as_deref()
            .map(crate::tmux::socket_short_name);
        let candidates: Vec<_> = panes
            .iter()
            .filter(|pane| {
                pane.pane_id == *pane_id
                    && match agent_socket.as_deref() {
                        Some(socket) => pane.socket.as_deref() == Some(socket),
                        None => true,
                    }
            })
            .collect();
        if candidates.len() != 1 {
            continue;
        }
        let pane = candidates[0];
        let host = crate::backend::pane_id_host_kind(pane_id)
            .map_or_else(|| "unknown".to_string(), |kind| kind.to_string());
        let window_id = if pane.window_id.is_empty() {
            format!("{}:{}", pane.session, pane.window_index)
        } else {
            pane.window_id.clone()
        };
        let socket = pane.socket.clone().or(agent_socket);
        let participant = Participant {
            agent_kind: agent.kind,
            agent_session_id: agent.session_id.clone(),
            pane: pane_id.clone(),
            socket: socket.clone(),
            room: RoomId {
                host,
                socket: socket.clone(),
                window_id,
            },
            tmux_session_id: (!pane.session_id.is_empty()).then(|| pane.session_id.clone()),
            tmux_session_name: Some(pane.session.clone()),
            window_name: (!pane.window_name.is_empty()).then(|| pane.window_name.clone()),
            state: agent.state,
            cwd: agent.cwd.clone(),
        };
        let key = (socket, pane_id.clone());
        let replace = resolved
            .get(&key)
            .is_none_or(|(at, _)| *at < agent.last_activity_at);
        if replace {
            resolved.insert(key, (agent.last_activity_at, participant));
        }
    }
    let mut participants: Vec<_> = resolved
        .into_values()
        .map(|(_, participant)| participant)
        .collect();
    participants.sort_by(|left, right| left.pane.cmp(&right.pane));
    participants
}

pub fn resolve_origin(
    origin: &CollaborationOrigin,
    participants: &[Participant],
) -> Result<Participant, CollaborationError> {
    let matches: Vec<_> = participants
        .iter()
        .filter(|participant| {
            participant.pane == origin.pane
                && origin
                    .socket
                    .as_deref()
                    .is_none_or(|socket| participant.socket.as_deref() == Some(socket))
        })
        .cloned()
        .collect();
    match matches.as_slice() {
        [participant] => Ok(participant.clone()),
        [] => Err(CollaborationError::UnknownOrigin(origin.pane.clone())),
        _ => Err(CollaborationError::AmbiguousOrigin(origin.pane.clone())),
    }
}

pub fn resolve_target(
    sender: &Participant,
    selector: &str,
    participants: &[Participant],
) -> Result<Participant, CollaborationError> {
    let peers: Vec<_> = participants
        .iter()
        .filter(|candidate| candidate.room == sender.room && !candidate.same_endpoint(sender))
        .collect();
    if selector == "peer" {
        return match peers.as_slice() {
            [peer] => Ok((*peer).clone()),
            [] => Err(CollaborationError::NoPeer),
            _ => Err(CollaborationError::AmbiguousTarget(selector.to_string())),
        };
    }
    let pane = selector.strip_prefix("pane:").unwrap_or(selector);
    let matches: Vec<_> = peers
        .into_iter()
        .filter(|candidate| candidate.pane == pane || candidate.label() == selector)
        .collect();
    match matches.as_slice() {
        [participant] => Ok((*participant).clone()),
        [] => Err(CollaborationError::UnknownTarget(selector.to_string())),
        _ => Err(CollaborationError::AmbiguousTarget(selector.to_string())),
    }
}

pub async fn room_context(
    mailbox: &CollaborationStore,
    current: Participant,
    participants: &[Participant],
) -> RoomContext {
    let peers = participants
        .iter()
        .filter(|participant| {
            participant.room == current.room && !participant.same_endpoint(&current)
        })
        .cloned()
        .collect();
    let unread = mailbox.unread_count(&current).await;
    let unread_replies = mailbox.unread_reply_count(&current).await;
    RoomContext {
        current,
        peers,
        unread,
        unread_replies,
    }
}

fn next_request_id(now: OffsetDateTime) -> String {
    let nanos = now.unix_timestamp_nanos();
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("req_{nanos:x}_{counter:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(pane: &str, session: &str) -> Participant {
        Participant {
            agent_kind: AgentKind::Codex,
            agent_session_id: session.into(),
            pane: pane.into(),
            socket: Some("default".into()),
            room: RoomId {
                host: "tmux".into(),
                socket: Some("default".into()),
                window_id: "@1".into(),
            },
            tmux_session_id: Some("$1".into()),
            tmux_session_name: Some("main".into()),
            window_name: Some("feature".into()),
            state: AgentState::Idle,
            cwd: Some("/repo".into()),
        }
    }

    #[tokio::test]
    async fn request_claim_and_reply_round_trip() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "review auth".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: vec!["src/auth.rs".into()],
                },
            )
            .await
            .unwrap();
        let inbox = mailbox.claim_for(&recipient).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].status, RequestStatus::Claimed);

        let completed = mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "looks good".into(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(completed.status, RequestStatus::Completed);
        assert_eq!(mailbox.unread_reply_count(&sender).await, 1);
        assert_eq!(mailbox.pending_reply_unnotified().await.len(), 1);
        assert_eq!(
            mailbox
                .get_for(&sender, &request.id)
                .await
                .unwrap()
                .reply
                .unwrap()
                .body,
            "looks good"
        );
        assert_eq!(mailbox.unread_reply_count(&sender).await, 0);
        assert!(mailbox.pending_reply_unnotified().await.is_empty());
    }

    #[tokio::test]
    async fn sender_can_list_and_cancel_only_unclaimed_requests() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let first = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Question,
                    body: "first".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                },
            )
            .await
            .unwrap();
        let second = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "second".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: vec!["src/**".into()],
                },
            )
            .await
            .unwrap();

        assert_eq!(
            mailbox
                .list_for(&sender, RequestMailbox::Sent)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(mailbox
            .list_for(&recipient, RequestMailbox::Sent)
            .await
            .unwrap()
            .is_empty());

        let cancelled = mailbox.cancel_for(&sender, &first.id).await.unwrap();
        assert_eq!(cancelled.status, RequestStatus::Cancelled);
        mailbox.claim_for(&recipient).await.unwrap();
        assert!(matches!(
            mailbox.cancel_for(&sender, &second.id).await,
            Err(CollaborationError::AlreadyClaimed(_))
        ));
        assert!(matches!(
            mailbox.cancel_for(&recipient, &first.id).await,
            Err(CollaborationError::NotParticipant(_))
        ));
    }

    #[tokio::test]
    async fn mailbox_survives_reload_and_does_not_follow_a_reused_pane() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collaboration.json");
        let options = CollaborationOptions {
            path: Some(path),
            ..CollaborationOptions::default()
        };
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient-v1");
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        mailbox
            .create(
                sender,
                recipient,
                NewRequest {
                    kind: RequestKind::Question,
                    body: "what changed?".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                },
            )
            .await
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(options.path.as_ref().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let reloaded = CollaborationStore::load(options).await.unwrap();
        let replacement = participant("%2", "recipient-v2");
        assert!(reloaded.claim_for(&replacement).await.unwrap().is_empty());
        assert_eq!(
            reloaded
                .claim_for(&participant("%2", "recipient-v1"))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn peer_selector_refuses_ambiguity() {
        let sender = participant("%1", "sender");
        let peers = vec![
            sender.clone(),
            participant("%2", "two"),
            participant("%3", "three"),
        ];
        assert!(matches!(
            resolve_target(&sender, "peer", &peers),
            Err(CollaborationError::AmbiguousTarget(_))
        ));
        assert_eq!(resolve_target(&sender, "%2", &peers).unwrap().pane, "%2");
    }
}
