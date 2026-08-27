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
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{watch, Mutex, RwLock};

pub const COLLABORATION_SCHEMA_VERSION: u32 = 1;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);
const MAX_AIR_ARTIFACT_REFERENCES: usize = 8;
const MAX_AIR_REFERENCE_LABEL_BYTES: usize = 256;
const MAX_AIR_LOCATOR_DISPLAY_BYTES: usize = 4096;

#[derive(Debug, Clone)]
pub struct CollaborationOptions {
    pub enabled: bool,
    pub path: Option<PathBuf>,
    pub max_message_bytes: usize,
    pub scope: crate::config::CollaborationScope,
}

impl Default for CollaborationOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            max_message_bytes: 16 * 1024,
            scope: crate::config::CollaborationScope::default(),
        }
    }
}

/// The stable pane-shaped identity of the operator console. It is not a real
/// pane id — `pane_id_host_kind` rejects it, which is what keeps the wake path
/// from ever trying to type at a human.
pub const CONSOLE_PANE: &str = "console";
/// The console's agent session id. Fixed, so every request a human dispatches
/// shares one sender identity no matter which pane the console was opened from.
pub const CONSOLE_SESSION_ID: &str = "console";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollaborationOrigin {
    pub pane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// Act as the operator console rather than as the agent occupying `pane`.
    ///
    /// A human pressing a key in `muxa watch` is the sender; the agent that
    /// happens to sit in the pane the popup was opened from is not. `pane`
    /// still travels with the origin — it names the room the console is
    /// looking at and is the provenance evidence for who dialled — but it no
    /// longer supplies the identity, so the console can address that pane too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub console: bool,
}

/// Local IPC surface that initiated a collaboration call. This describes the
/// transport actor, not the agent session represented by `from`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationClientKind {
    Cli,
    Watch,
    Mcp,
    Dashboard,
    #[default]
    Unknown,
}

impl CollaborationClientKind {
    pub fn hello_label(self) -> String {
        let surface = match self {
            Self::Cli => "cli",
            Self::Watch => "watch",
            Self::Mcp => "mcp",
            Self::Dashboard => "dashboard",
            Self::Unknown => "unknown",
        };
        format!("muxa/{surface}/{}", env!("CARGO_PKG_VERSION"))
    }

    pub fn from_hello_label(label: &str) -> Self {
        let mut parts = label.split('/');
        if parts.next() != Some("muxa") {
            return Self::Unknown;
        }
        match parts.next() {
            Some("cli") => Self::Cli,
            Some("watch") => Self::Watch,
            Some("mcp") => Self::Mcp,
            Some("dashboard") => Self::Dashboard,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for CollaborationClientKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Cli => "cli",
            Self::Watch => "watch",
            Self::Mcp => "mcp",
            Self::Dashboard => "dashboard",
            Self::Unknown => "unknown",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationOriginMatch {
    Matched,
    Mismatched,
    Unverifiable,
}

impl std::fmt::Display for CollaborationOriginMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Matched => "matched",
            Self::Mismatched => "mismatched",
            Self::Unverifiable => "unverifiable",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationPaneEvidence {
    ProcessEnvironment,
    ProcessAncestry,
}

/// OS-observed provenance for a collaboration call. It is audit evidence,
/// not an authorization gate: callers retain the existing ability to act on
/// behalf of any resolvable pane agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollaborationProvenance {
    pub client_kind: CollaborationClientKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_gid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_evidence: Option<CollaborationPaneEvidence>,
    pub origin_match: CollaborationOriginMatch,
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
    /// Optional room-local address registered by this exact agent session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Advisory capabilities/responsibilities used by `role:<name>` routing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// This participant is the operator console, not a pane-hosted agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub console: bool,
}

impl Participant {
    /// The human driving `muxa watch` (or another operator surface).
    ///
    /// Identity is fixed so the sent mailbox stays one coherent thread across
    /// popups, while `room` follows the window the console was opened from so
    /// room-scoped peer selection still resolves the agents in front of the
    /// operator. `room` is deliberately outside `same_endpoint`, so moving the
    /// console to another window does not fork its identity.
    pub fn console(room: RoomId) -> Self {
        Self {
            agent_kind: AgentKind::Unknown,
            agent_session_id: CONSOLE_SESSION_ID.to_string(),
            pane: CONSOLE_PANE.to_string(),
            socket: None,
            room,
            tmux_session_id: None,
            tmux_session_name: None,
            window_name: None,
            state: AgentState::Idle,
            cwd: None,
            alias: None,
            roles: Vec::new(),
            console: true,
        }
    }

    pub fn label(&self) -> String {
        if self.console {
            return CONSOLE_PANE.to_string();
        }
        self.alias.as_ref().map_or_else(
            || format!("{}@{}", self.agent_kind, self.pane),
            |alias| format!("{alias}@{}", self.pane),
        )
    }

    fn same_endpoint(&self, other: &Self) -> bool {
        self.pane == other.pane
            && self.socket == other.socket
            && self.agent_session_id == other.agent_session_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CollaborationIdentity {
    room: RoomId,
    pane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    socket: Option<String>,
    agent_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    roles: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl CollaborationIdentity {
    fn matches(&self, participant: &Participant) -> bool {
        self.room == participant.room
            && self.pane == participant.pane
            && self.socket == participant.socket
            && self.agent_session_id == participant.agent_session_id
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

/// AIR 1 profiles that may be referenced by a collaboration request or
/// reply. A reference does not make muxa an AIR producer or validator; the
/// artifact itself remains subject to AIR Workbench's schema and runtime
/// conformance checks.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AirArtifactProfile {
    #[serde(rename = "https://open330.github.io/air/profiles/1.0.0/workflow-skill")]
    WorkflowSkill,
    #[serde(rename = "https://open330.github.io/air/profiles/1.0.0/plan-native-cli")]
    PlanNativeCli,
    #[serde(rename = "https://open330.github.io/air/profiles/1.0.0/trace-native-run")]
    TraceNativeRun,
    #[serde(rename = "https://open330.github.io/air/profiles/1.0.0/trace-session-snapshot")]
    TraceSessionSnapshot,
}

impl AirArtifactProfile {
    pub fn kind(self) -> &'static str {
        match self {
            Self::WorkflowSkill => "workflow",
            Self::PlanNativeCli => "plan",
            Self::TraceNativeRun | Self::TraceSessionSnapshot => "trace",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::WorkflowSkill => "AIR WORKFLOW",
            Self::PlanNativeCli => "AIR PLAN",
            Self::TraceNativeRun => "AIR TRACE",
            Self::TraceSessionSnapshot => "AIR SESSION",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AirLocatorDisclosure {
    LocalOnly,
    Redacted,
}

/// Display-only locator following AIR 1's locator vocabulary. It is never
/// treated as authority or opened automatically by muxa.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AirArtifactLocator {
    pub display: String,
    pub disclosure: AirLocatorDisclosure,
}

/// A typed reference to an AIR 1 artifact envelope. Only the stable content
/// identity/profile and optional display metadata travel through muxa; the
/// source-bearing artifact stays in AIR Workbench or another AIR consumer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AirArtifactReference {
    pub artifact_id: String,
    pub profile: AirArtifactProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<AirArtifactLocator>,
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

/// Durable recovery state for a request body being delivered directly into
/// an idle agent prompt. This is normally absent: it exists only between the
/// automatic claim and successful prompt submission.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WakeDeliveryState {
    Prepared,
    PromptWritten,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub air_artifacts: Vec<AirArtifactReference>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollaborationRequest {
    pub id: String,
    pub from: Participant,
    pub to: Participant,
    /// How the request entered muxad. `from` remains the represented agent;
    /// this field identifies the local caller that exercised that authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<CollaborationProvenance>,
    pub kind: RequestKind,
    pub body: String,
    pub expects_reply: bool,
    pub work_mode: WorkMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub air_artifacts: Vec<AirArtifactReference>,
    pub status: RequestStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub claimed_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_delivery: Option<WakeDeliveryState>,
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
    #[serde(default)]
    pub air_artifacts: Vec<AirArtifactReference>,
}

#[derive(Debug, thiserror::Error)]
pub enum CollaborationError {
    #[error("agent collaboration is disabled; enable [collaboration].enabled")]
    Disabled,
    #[error(
        "collaboration origin is not a hook-correlated tracked pane agent: {0}; trigger an agent event or restart the agent"
    )]
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
    #[error("invalid AIR artifact reference: {0}")]
    InvalidAirArtifactReference(String),
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
    #[error("invalid collaboration alias {0:?}; use 1-32 letters, digits, '.', '_', or '-'")]
    InvalidAlias(String),
    #[error("invalid collaboration role {0:?}; use 1-32 letters, digits, '.', '_', or '-'")]
    InvalidRole(String),
    #[error("an agent may register at most 8 collaboration roles")]
    TooManyRoles,
    #[error("collaboration alias {0:?} is already used by a live peer in this room")]
    AliasInUse(String),
    #[error("the operator console has no agent session to name; run `muxa identity` from the agent's own pane")]
    ConsoleIdentity,
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
    #[serde(default)]
    identities: Vec<CollaborationIdentity>,
}

/// In-memory mailbox with an optional atomic JSON snapshot. Collaboration
/// traffic is low-volume, so rewriting the mailbox after mutations is
/// simpler and safer than maintaining a database or partially-replayed log.
pub struct CollaborationStore {
    opts: CollaborationOptions,
    requests: RwLock<HashMap<String, CollaborationRequest>>,
    identities: RwLock<Vec<CollaborationIdentity>>,
    /// Serializes each in-memory mutation with its durable snapshot. Wake
    /// scans also take this lock, so an unpersisted request is never visible
    /// to the delivery loop.
    transaction_lock: Mutex<()>,
    /// Monotonic invalidation signal for durable collaboration state. The
    /// mailbox remains the source of truth: waiters wake on a revision change
    /// and re-read the exact request they care about. `watch` retains the
    /// latest revision and wakes every subscriber, avoiding both lost
    /// notifications and broadcast-lag recovery machinery.
    changes: watch::Sender<u64>,
}

impl CollaborationStore {
    /// The configured reach of explicit pane targets. Read by the IPC send
    /// handler; everything else stays window-scoped regardless.
    pub fn scope(&self) -> crate::config::CollaborationScope {
        self.opts.scope
    }

    pub fn in_memory(options: CollaborationOptions) -> Arc<Self> {
        let (changes, _) = watch::channel(0);
        Arc::new(Self {
            opts: CollaborationOptions {
                path: None,
                ..options
            },
            requests: RwLock::new(HashMap::new()),
            identities: RwLock::new(Vec::new()),
            transaction_lock: Mutex::new(()),
            changes,
        })
    }

    pub async fn load(options: CollaborationOptions) -> Result<Arc<Self>, CollaborationError> {
        let mut requests = HashMap::new();
        let mut identities = Vec::new();
        if let Some(path) = options.path.as_ref() {
            match tokio::fs::read(path).await {
                Ok(bytes) => {
                    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
                    if snapshot.version != COLLABORATION_SCHEMA_VERSION {
                        return Err(CollaborationError::UnsupportedSchema(snapshot.version));
                    }
                    requests.extend(snapshot.requests.into_iter().map(|r| (r.id.clone(), r)));
                    identities = snapshot.identities;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let (changes, _) = watch::channel(0);
        Ok(Arc::new(Self {
            opts: options,
            requests: RwLock::new(requests),
            identities: RwLock::new(identities),
            transaction_lock: Mutex::new(()),
            changes,
        }))
    }

    pub fn enabled(&self) -> bool {
        self.opts.enabled
    }

    /// Subscribe to durable mailbox/identity invalidations. Callers must
    /// subscribe before reading their baseline state, then re-read after each
    /// change; the revision deliberately carries no request body.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn publish_change(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    /// Attach persisted aliases and roles only to the exact live agent
    /// generation that registered them. A later process reusing the same pane
    /// inherits no *self-registered* identity — it stays anonymous until it
    /// registers its own.
    ///
    /// What it does keep is whatever `participants_from` read off the pane's
    /// muxa options. That is a different kind of claim: the launcher's
    /// declaration about the slot ("this pane is the pipeline's reviewer"),
    /// not an agent's statement about itself, and it remains true for whoever
    /// occupies the slot. So a registered identity overwrites it, and its
    /// absence leaves it standing.
    pub async fn enrich_participants(&self, participants: &mut [Participant]) {
        let _transaction = self.transaction_lock.lock().await;
        let identities = self.identities.read().await;
        for participant in participants {
            if let Some(identity) = identities
                .iter()
                .find(|identity| identity.matches(participant))
            {
                participant.alias.clone_from(&identity.alias);
                participant.roles.clone_from(&identity.roles);
            }
        }
    }

    pub async fn set_identity(
        &self,
        caller: &Participant,
        live_participants: &[Participant],
        alias: Option<String>,
        roles: Vec<String>,
    ) -> Result<Participant, CollaborationError> {
        self.ensure_enabled()?;
        // Aliases are room-local and a console borrows the room it was opened
        // from, so letting it register one would squat a name the agents in
        // that window route by.
        if caller.console {
            return Err(CollaborationError::ConsoleIdentity);
        }
        let alias = normalize_alias(alias)?;
        let roles = normalize_roles(roles)?;
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.identities.read().await.clone();
        {
            let mut identities = self.identities.write().await;
            if let Some(alias) = alias.as_deref() {
                let in_use = identities.iter().any(|identity| {
                    identity.room == caller.room
                        && identity.alias.as_deref() == Some(alias)
                        && !identity.matches(caller)
                        && live_participants
                            .iter()
                            .any(|participant| identity.matches(participant))
                });
                if in_use {
                    return Err(CollaborationError::AliasInUse(alias.to_string()));
                }
            }
            identities.retain(|identity| !identity.matches(caller));
            if alias.is_some() || !roles.is_empty() {
                identities.push(CollaborationIdentity {
                    room: caller.room.clone(),
                    pane: caller.pane.clone(),
                    socket: caller.socket.clone(),
                    agent_session_id: caller.agent_session_id.clone(),
                    alias: alias.clone(),
                    roles: roles.clone(),
                    updated_at: OffsetDateTime::now_utc(),
                });
            }
        }
        if let Err(error) = self.persist_current().await {
            *self.identities.write().await = previous;
            return Err(error);
        }
        self.publish_change();
        let mut updated = caller.clone();
        updated.alias = alias;
        updated.roles = roles;
        Ok(updated)
    }

    pub async fn create(
        &self,
        from: Participant,
        to: Participant,
        input: NewRequest,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.create_with_provenance(from, to, input, None).await
    }

    pub async fn create_with_provenance(
        &self,
        from: Participant,
        to: Participant,
        input: NewRequest,
        provenance: Option<CollaborationProvenance>,
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
        validate_air_artifact_references(&input.air_artifacts)?;
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
        let now = OffsetDateTime::now_utc();
        let request = CollaborationRequest {
            id: next_request_id(now),
            from,
            to,
            provenance,
            kind: input.kind,
            body: body.to_string(),
            expects_reply: input.expects_reply,
            work_mode: input.work_mode,
            paths: input.paths,
            air_artifacts: input.air_artifacts,
            status: RequestStatus::Queued,
            created_at: now,
            claimed_at: None,
            wake_delivery: None,
            notified_at: None,
            reply_notified_at: None,
            reply_read_at: None,
            reply: None,
        };
        self.requests
            .write()
            .await
            .insert(request.id.clone(), request.clone());
        if let Err(error) = self.persist_current().await {
            *self.requests.write().await = previous;
            return Err(error);
        }
        self.publish_change();
        Ok(request)
    }

    pub async fn claim_for(
        &self,
        caller: &Participant,
    ) -> Result<Vec<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
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
                    } else if request.wake_delivery.take().is_some() {
                        // A direct wake was prepared but the recipient pulled
                        // its inbox first. Manual retrieval wins and suppresses
                        // any recovery injection from the daemon.
                        request.notified_at.get_or_insert(now);
                        if !request.expects_reply {
                            request.status = RequestStatus::Completed;
                        }
                        changed = true;
                    }
                    inbox.push(request.clone());
                }
            }
        }
        inbox.sort_by_key(|request| request.created_at);
        if changed {
            if let Err(error) = self.persist_current().await {
                *self.requests.write().await = previous;
                return Err(error);
            }
            self.publish_change();
        }
        Ok(inbox)
    }

    /// Atomically reserve one queued request for direct prompt delivery.
    /// Claiming before the terminal side effect closes the sender-cancel race:
    /// once a body may reach the agent, the request can no longer be cancelled.
    pub async fn prepare_direct_wake(
        &self,
        caller: &Participant,
        request_id: &str,
    ) -> Result<Option<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
        let prepared = {
            let mut requests = self.requests.write().await;
            let Some(request) = requests.get_mut(request_id) else {
                return Err(CollaborationError::NotFound(request_id.to_string()));
            };
            if !request.to.same_endpoint(caller) {
                return Err(CollaborationError::NotParticipant(request_id.to_string()));
            }
            if request.status != RequestStatus::Queued || request.notified_at.is_some() {
                None
            } else {
                request.status = RequestStatus::Claimed;
                request.claimed_at = Some(OffsetDateTime::now_utc());
                request.wake_delivery = Some(WakeDeliveryState::Prepared);
                Some(request.clone())
            }
        };
        if prepared.is_some() {
            if let Err(error) = self.persist_current().await {
                *self.requests.write().await = previous;
                return Err(error);
            }
            self.publish_change();
        }
        Ok(prepared)
    }

    /// Record that the direct prompt text reached the pane. Submission is a
    /// separate keystroke, so a daemon restart can retry only Enter rather
    /// than injecting the request body a second time.
    pub async fn mark_wake_prompt_written(
        &self,
        request_id: &str,
    ) -> Result<(), CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let changed = {
            let mut requests = self.requests.write().await;
            requests.get_mut(request_id).is_some_and(|request| {
                if request.wake_delivery == Some(WakeDeliveryState::Prepared) {
                    request.wake_delivery = Some(WakeDeliveryState::PromptWritten);
                    true
                } else {
                    false
                }
            })
        };
        if changed {
            // The text side effect cannot be rolled back. Retain the in-memory
            // phase even if persistence fails, matching notification markers.
            let result = self.persist_current().await;
            self.publish_change();
            result?;
        }
        Ok(())
    }

    pub async fn reply(
        &self,
        caller: &Participant,
        request_id: &str,
        status: RequestStatus,
        body: String,
        artifacts: Vec<String>,
        air_artifacts: Vec<AirArtifactReference>,
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
        validate_air_artifact_references(&air_artifacts)?;
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
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
                air_artifacts,
                at: OffsetDateTime::now_utc(),
            });
            request.clone()
        };
        if let Err(error) = self.persist_current().await {
            *self.requests.write().await = previous;
            return Err(error);
        }
        self.publish_change();
        Ok(updated)
    }

    pub async fn get_for(
        &self,
        caller: &Participant,
        request_id: &str,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
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
            if let Err(error) = self.persist_current().await {
                *self.requests.write().await = previous;
                return Err(error);
            }
            self.publish_change();
        }
        Ok(request)
    }

    /// Wait for one participant-visible request to become terminal without
    /// polling. Subscription happens before the first read, closing the race
    /// where a reply could land between observing `claimed` and beginning to
    /// wait. A final authoritative read at the deadline covers a reply racing
    /// with timeout and returns the latest non-terminal request on timeout.
    pub async fn wait_for_terminal(
        &self,
        caller: &Participant,
        request_id: &str,
        timeout: Duration,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        let mut changes = self.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let request = self.get_for(caller, request_id).await?;
            if request.status.is_terminal() {
                return Ok(request);
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return self.get_for(caller, request_id).await,
            }
        }
    }

    pub async fn list_for(
        &self,
        caller: &Participant,
        mailbox: RequestMailbox,
    ) -> Result<Vec<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
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
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
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
        if let Err(error) = self.persist_current().await {
            *self.requests.write().await = previous;
            return Err(error);
        }
        self.publish_change();
        Ok(cancelled)
    }

    pub async fn pending_unnotified(&self) -> Vec<CollaborationRequest> {
        let _transaction = self.transaction_lock.lock().await;
        let mut requests: Vec<_> = self
            .requests
            .read()
            .await
            .values()
            .filter(|request| {
                request.notified_at.is_none()
                    && (request.status == RequestStatus::Queued || request.wake_delivery.is_some())
            })
            .cloned()
            .collect();
        requests.sort_by_key(|request| request.created_at);
        requests
    }

    /// Replies still owed a wake to their sender.
    ///
    /// A console sender is excluded rather than skipped downstream: it has no
    /// pane to type into, so it would never be marked notified and would be
    /// re-scanned on every daemon tick for the life of the mailbox. Its reply
    /// is delivered by being readable in the recipient's mailbox.
    pub async fn pending_reply_unnotified(&self) -> Vec<CollaborationRequest> {
        let _transaction = self.transaction_lock.lock().await;
        let mut requests: Vec<_> = self
            .requests
            .read()
            .await
            .values()
            .filter(|request| {
                request.status.is_terminal()
                    && request.reply.is_some()
                    && request.reply_notified_at.is_none()
                    && !request.from.console
            })
            .cloned()
            .collect();
        requests.sort_by_key(|request| request.created_at);
        requests
    }

    pub async fn mark_notified(&self, request_id: &str) -> Result<(), CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let changed = {
            let mut requests = self.requests.write().await;
            requests.get_mut(request_id).is_some_and(|request| {
                if request.notified_at.is_none()
                    && (request.status == RequestStatus::Queued || request.wake_delivery.is_some())
                {
                    request.notified_at = Some(OffsetDateTime::now_utc());
                    request.wake_delivery = None;
                    if !request.expects_reply && request.status == RequestStatus::Claimed {
                        request.status = RequestStatus::Completed;
                    }
                    true
                } else {
                    false
                }
            })
        };
        if changed {
            // The terminal injection already happened. Keep the in-memory
            // marker even if disk persistence fails so the live daemon does
            // not inject the same wake on the next revision/reconcile scan.
            let result = self.persist_current().await;
            self.publish_change();
            result?;
        }
        Ok(())
    }

    pub async fn mark_reply_notified(&self, request_id: &str) -> Result<(), CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
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
            // As above, a delivered side effect cannot be rolled back.
            let result = self.persist_current().await;
            self.publish_change();
            result?;
        }
        Ok(())
    }

    pub async fn unread_count(&self, participant: &Participant) -> usize {
        let _transaction = self.transaction_lock.lock().await;
        self.requests
            .read()
            .await
            .values()
            .filter(|request| {
                request.to.same_endpoint(participant)
                    && (request.status == RequestStatus::Queued || request.wake_delivery.is_some())
            })
            .count()
    }

    /// Replies the participant has not read yet.
    ///
    /// "Unread" is cleared by the sender fetching the reply, which an operator
    /// console never does — it reads replies off the recipient's row. Counting
    /// them would give watch a `mail 0/N` badge that only ever grows.
    pub async fn unread_reply_count(&self, participant: &Participant) -> usize {
        if participant.console {
            return 0;
        }
        let _transaction = self.transaction_lock.lock().await;
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

    /// Persist the current transaction. The caller must hold
    /// `transaction_lock` until this returns.
    async fn persist_current(&self) -> Result<(), CollaborationError> {
        let Some(path) = self.opts.path.as_ref() else {
            return Ok(());
        };
        let mut requests: Vec<_> = self.requests.read().await.values().cloned().collect();
        requests.sort_by_key(|request| request.created_at);
        let mut identities = self.identities.read().await.clone();
        identities.sort_by(|left, right| {
            (
                &left.room.host,
                &left.room.socket,
                &left.room.window_id,
                &left.pane,
                &left.agent_session_id,
            )
                .cmp(&(
                    &right.room.host,
                    &right.room.socket,
                    &right.room.window_id,
                    &right.pane,
                    &right.agent_session_id,
                ))
        });
        let snapshot = Snapshot {
            version: COLLABORATION_SCHEMA_VERSION,
            requests,
            identities,
        };
        let bytes = serde_json::to_vec_pretty(&snapshot)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, bytes).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(tmp, path).await?;
        Ok(())
    }
}

/// Correlate live agent rows with pane topology, keeping the newest live
/// agent when adapter/synthetic rows briefly overlap on the same pane.
pub fn participants_from(agents: &[Agent], panes: &[PaneInfo]) -> Vec<Participant> {
    let mut resolved: HashMap<(Option<String>, String), (OffsetDateTime, Participant)> =
        HashMap::new();
    for agent in agents.iter().filter(|agent| {
        agent.pane.is_some()
            && agent.state != AgentState::Stopped
            && agent.kind != AgentKind::Task
            && !agent
                .session_id
                .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
    }) {
        let pane_id = agent.pane.as_ref().expect("filtered pane");
        let agent_socket = agent
            .tmux_socket
            .as_deref()
            .map(|endpoint| crate::backend::pane_endpoint_identity(Some(pane_id), endpoint));
        let candidates: Vec<_> = panes
            .iter()
            .filter(|pane| {
                pane.pane_id == *pane_id
                    && match agent_socket.as_deref() {
                        Some(socket) => pane.socket.as_deref().is_some_and(|candidate| {
                            crate::backend::pane_endpoints_match(Some(pane_id), candidate, socket)
                        }),
                        None => true,
                    }
            })
            .collect();
        if candidates.len() != 1 {
            continue;
        }
        let pane = candidates[0];
        let socket = pane.socket.clone().or(agent_socket);
        let participant = Participant {
            agent_kind: agent.kind,
            agent_session_id: agent.session_id.clone(),
            pane: pane_id.clone(),
            socket: socket.clone(),
            room: pane_room(pane_id, pane, socket.clone()),
            tmux_session_id: (!pane.session_id.is_empty()).then(|| pane.session_id.clone()),
            tmux_session_name: Some(pane.session.clone()),
            window_name: (!pane.window_name.is_empty()).then(|| pane.window_name.clone()),
            state: agent.state,
            cwd: agent.cwd.clone(),
            // Seeded from what the launcher stamped on the pane. A pipeline
            // agent is addressable as `role:reviewer` / `@review` from the
            // moment its pane exists, without waiting for it to run
            // `muxa identity set` — which it never does, and could not do
            // before its first turn anyway. `enrich_participants` replaces
            // both if the agent registered an identity of its own.
            alias: pane.agent_alias.clone(),
            roles: pane.agent_role.clone().into_iter().collect(),
            console: false,
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

/// The durable room identity of a pane: `(host, socket, window_id)`, never the
/// mutable window name or index.
fn pane_room(pane_id: &str, pane: &PaneInfo, socket: Option<String>) -> RoomId {
    RoomId {
        host: crate::backend::pane_id_host_kind(pane_id)
            .map_or_else(|| "unknown".to_string(), |kind| kind.to_string()),
        socket,
        window_id: if pane.window_id.is_empty() {
            format!("{}:{}", pane.session, pane.window_index)
        } else {
            pane.window_id.clone()
        },
    }
}

/// The console looking out from the window it was opened in.
///
/// Unlike an agent origin the pane need not host an agent — an operator can
/// open the console from a bare shell — so this reads the pane inventory
/// directly. A pane the backend cannot list still yields a console, just one
/// with no room peers; host-scoped pane targets keep working from there.
///
/// The session/window names are carried for hints only. They sit outside
/// `same_endpoint`, so a console that moves to another window keeps one
/// identity and one sent mailbox.
fn console_participant(
    origin: &CollaborationOrigin,
    panes: &[PaneInfo],
) -> Result<Participant, CollaborationError> {
    let mut matches = panes.iter().filter(|pane| {
        pane.pane_id == origin.pane
            && match origin.socket.as_deref() {
                Some(socket) => pane.socket.as_deref().is_some_and(|candidate| {
                    crate::backend::pane_endpoints_match(Some(&origin.pane), candidate, socket)
                }),
                None => true,
            }
    });
    let Some(pane) = matches.next() else {
        return Ok(Participant::console(RoomId {
            host: CONSOLE_PANE.to_string(),
            socket: None,
            window_id: CONSOLE_PANE.to_string(),
        }));
    };
    if matches.next().is_some() {
        return Err(CollaborationError::AmbiguousOrigin(origin.pane.clone()));
    }
    let socket = pane.socket.clone().or_else(|| origin.socket.clone());
    let mut console = Participant::console(pane_room(&origin.pane, pane, socket));
    console.tmux_session_id = (!pane.session_id.is_empty()).then(|| pane.session_id.clone());
    console.tmux_session_name = Some(pane.session.clone());
    console.window_name = (!pane.window_name.is_empty()).then(|| pane.window_name.clone());
    Ok(console)
}

pub fn resolve_origin(
    origin: &CollaborationOrigin,
    participants: &[Participant],
    panes: &[PaneInfo],
) -> Result<Participant, CollaborationError> {
    // A console never has to exist in the participant table: it is the
    // operator, not a tracked agent. That is what lets `muxa watch` address
    // every pane uniformly — including the one it was opened from, and
    // including the case where that pane hosts no agent at all.
    if origin.console {
        return console_participant(origin, panes);
    }
    let matches: Vec<_> = participants
        .iter()
        .filter(|participant| {
            participant.pane == origin.pane
                && origin.socket.as_deref().is_none_or(|socket| {
                    participant.socket.as_deref().is_some_and(|candidate| {
                        crate::backend::pane_endpoints_match(Some(&origin.pane), candidate, socket)
                    })
                })
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
    scope: crate::config::CollaborationScope,
) -> Result<Participant, CollaborationError> {
    // Host scope widens *explicit pane* targets only. `peer`, `@alias` and
    // `role:` remain room concepts: a pane id is unique on the host, but an
    // alias is only unique among live peers of one room, and matching it
    // host-wide would deliver to whichever unrelated agent happens to share
    // the name.
    if scope == crate::config::CollaborationScope::Host {
        let pane = selector.strip_prefix("pane:").unwrap_or(selector);
        if pane.starts_with('%') {
            let matches: Vec<_> = participants
                .iter()
                .filter(|candidate| candidate.pane == pane && !candidate.same_endpoint(sender))
                .collect();
            return match matches.as_slice() {
                [participant] => Ok((*participant).clone()),
                [] => Err(CollaborationError::UnknownTarget(selector.to_string())),
                _ => Err(CollaborationError::AmbiguousTarget(selector.to_string())),
            };
        }
    }
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
    if let Some(alias) = selector
        .strip_prefix('@')
        .or_else(|| selector.strip_prefix("alias:"))
    {
        let matches: Vec<_> = peers
            .into_iter()
            .filter(|candidate| {
                candidate
                    .alias
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(alias))
            })
            .collect();
        return select_target(matches, selector);
    }
    if let Some(role) = selector.strip_prefix("role:") {
        let matches: Vec<_> = peers
            .into_iter()
            .filter(|candidate| {
                candidate
                    .roles
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(role))
            })
            .collect();
        return select_target(matches, selector);
    }
    let pane = selector.strip_prefix("pane:").unwrap_or(selector);
    let matches: Vec<_> = peers
        .into_iter()
        .filter(|candidate| {
            candidate.pane == pane
                || candidate.label() == selector
                || format!("{}@{}", candidate.agent_kind, candidate.pane) == selector
        })
        .collect();
    select_target(matches, selector)
}

fn select_target(
    matches: Vec<&Participant>,
    selector: &str,
) -> Result<Participant, CollaborationError> {
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

fn validate_air_artifact_references(
    references: &[AirArtifactReference],
) -> Result<(), CollaborationError> {
    if references.len() > MAX_AIR_ARTIFACT_REFERENCES {
        return Err(CollaborationError::InvalidAirArtifactReference(format!(
            "at most {MAX_AIR_ARTIFACT_REFERENCES} references are allowed"
        )));
    }
    for (index, reference) in references.iter().enumerate() {
        let digest = reference
            .artifact_id
            .strip_prefix("urn:air:sha256:")
            .filter(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        if digest.is_none() {
            return Err(CollaborationError::InvalidAirArtifactReference(
                "artifact_id must be urn:air:sha256 followed by 64 lowercase hex characters".into(),
            ));
        }
        if references[..index]
            .iter()
            .any(|prior| prior.artifact_id == reference.artifact_id)
        {
            return Err(CollaborationError::InvalidAirArtifactReference(format!(
                "duplicate artifact_id {}",
                reference.artifact_id
            )));
        }
        if reference.label.as_ref().is_some_and(|label| {
            label.trim().is_empty() || label.len() > MAX_AIR_REFERENCE_LABEL_BYTES
        }) {
            return Err(CollaborationError::InvalidAirArtifactReference(format!(
                "label must be non-empty and at most {MAX_AIR_REFERENCE_LABEL_BYTES} bytes"
            )));
        }
        if reference.locator.as_ref().is_some_and(|locator| {
            locator.display.trim().is_empty()
                || locator.display.len() > MAX_AIR_LOCATOR_DISPLAY_BYTES
        }) {
            return Err(CollaborationError::InvalidAirArtifactReference(format!(
                "locator display must be non-empty and at most {MAX_AIR_LOCATOR_DISPLAY_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn normalize_alias(alias: Option<String>) -> Result<Option<String>, CollaborationError> {
    let Some(alias) = alias else {
        return Ok(None);
    };
    let alias = alias.trim();
    if alias.is_empty() {
        return Ok(None);
    }
    if !valid_identity_token(alias) {
        return Err(CollaborationError::InvalidAlias(alias.to_string()));
    }
    Ok(Some(alias.to_ascii_lowercase()))
}

fn normalize_roles(roles: Vec<String>) -> Result<Vec<String>, CollaborationError> {
    let mut normalized = Vec::new();
    for role in roles {
        let role = role.trim();
        if !valid_identity_token(role) {
            return Err(CollaborationError::InvalidRole(role.to_string()));
        }
        let role = role.to_ascii_lowercase();
        if !normalized.contains(&role) {
            normalized.push(role);
        }
    }
    if normalized.len() > 8 {
        return Err(CollaborationError::TooManyRoles);
    }
    normalized.sort();
    Ok(normalized)
}

fn valid_identity_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CollaborationScope;

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
            alias: None,
            roles: Vec::new(),
            console: false,
        }
    }

    fn pane_info(pane_id: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_role: None,
            agent_alias: None,
            pane_id: pane_id.into(),
            session_id: "$1".into(),
            session: "main".into(),
            window_id: "@1".into(),
            window_name: "agents".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "codex".into(),
            title: String::new(),
            current_path: "/repo".into(),
            pane_pid: 0,
            socket: Some("default".into()),
        }
    }

    fn air_reference(profile: AirArtifactProfile) -> AirArtifactReference {
        AirArtifactReference {
            artifact_id: format!("urn:air:sha256:{}", "a".repeat(64)),
            profile,
            label: Some("review plan".into()),
            locator: Some(AirArtifactLocator {
                display: "plans/review.air.json".into(),
                disclosure: AirLocatorDisclosure::LocalOnly,
            }),
        }
    }

    fn managed_pane(pane_id: &str, role: &str, alias: &str) -> PaneInfo {
        PaneInfo {
            agent_role: Some(role.into()),
            agent_alias: Some(alias.into()),
            ..pane_info(pane_id)
        }
    }

    /// The bug this closes: `muxa work up` stamps `@muxa_agent_role` on the
    /// pane, but the room only knew about identities an agent registered for
    /// itself — which a pipeline agent never does. `role:implementer` matched
    /// nobody, so the reviewer's handoff was dropped and the pair stalled with
    /// the work looking finished.
    #[tokio::test]
    async fn pipeline_roles_from_pane_metadata_resolve_a_target() {
        let store = crate::Store::shared();
        for (session, pane) in [("impl-session", "%1"), ("review-session", "%2")] {
            store
                .apply(&crate::event::AgentEvent::Started {
                    id: crate::event::AgentId {
                        kind: AgentKind::Codex,
                        session_id: session.into(),
                        surface: None,
                        pane: Some(pane.into()),
                        tmux_socket: Some("default".into()),
                        cwd: Some("/repo".into()),
                    },
                    at: OffsetDateTime::now_utc(),
                })
                .await;
        }
        let panes = vec![
            managed_pane("%1", "implementer", "impl"),
            managed_pane("%2", "reviewer", "review"),
        ];
        let participants = participants_from(&store.snapshot().await, &panes);
        assert_eq!(participants.len(), 2);

        let reviewer = participants
            .iter()
            .find(|p| p.pane == "%2")
            .expect("reviewer participant");

        for selector in [
            "role:implementer",
            "role:IMPLEMENTER",
            "@impl",
            "alias:impl",
        ] {
            let target = resolve_target(
                reviewer,
                selector,
                &participants,
                crate::config::CollaborationScope::Window,
            )
            .unwrap_or_else(|e| panic!("{selector} must resolve: {e}"));
            assert_eq!(target.pane, "%1", "{selector} routed to the wrong pane");
        }
    }

    /// Pane metadata is the launcher's claim about the slot; an identity the
    /// agent registered for itself is more specific and must win. Registering
    /// only an alias must not leave the launcher's role behind, or an agent
    /// could never shed a role it was launched with.
    #[tokio::test]
    async fn self_registered_identity_overrides_pane_metadata() {
        let collab = CollaborationStore::in_memory(CollaborationOptions::default());
        let panes = vec![managed_pane("%1", "implementer", "impl")];
        let store = crate::Store::shared();
        store
            .apply(&crate::event::AgentEvent::Started {
                id: crate::event::AgentId {
                    kind: AgentKind::Codex,
                    session_id: "impl-session".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/repo".into()),
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let mut participants = participants_from(&store.snapshot().await, &panes);

        // No registration yet: the launcher's claim stands.
        collab.enrich_participants(&mut participants).await;
        assert_eq!(participants[0].roles, vec!["implementer".to_string()]);
        assert_eq!(participants[0].alias.as_deref(), Some("impl"));

        let caller = participants[0].clone();
        collab
            .set_identity(
                &caller,
                &participants.clone(),
                Some("driver".into()),
                vec!["rust".into()],
            )
            .await
            .expect("identity registers");

        collab.enrich_participants(&mut participants).await;
        assert_eq!(participants[0].alias.as_deref(), Some("driver"));
        assert_eq!(
            participants[0].roles,
            vec!["rust".to_string()],
            "the registered role set replaces the launcher's, never merges",
        );
    }

    #[tokio::test]
    async fn synthetic_rows_are_not_collaboration_participants() {
        let store = crate::Store::shared();
        let started = |session_id: &str| crate::event::AgentEvent::Started {
            id: crate::event::AgentId {
                kind: AgentKind::Codex,
                session_id: session_id.into(),
                surface: None,
                pane: Some("%1".into()),
                tmux_socket: Some("default".into()),
                cwd: Some("/repo".into()),
            },
            at: OffsetDateTime::now_utc(),
        };
        store.apply(&started("synthetic-7:default:%1")).await;
        let panes = vec![pane_info("%1")];
        assert!(participants_from(&store.snapshot().await, &panes).is_empty());

        store.apply(&started("real-session")).await;
        let participants = participants_from(&store.snapshot().await, &panes);
        assert_eq!(participants.len(), 1);
        assert_eq!(participants[0].agent_session_id, "real-session");
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
                    air_artifacts: vec![air_reference(AirArtifactProfile::PlanNativeCli)],
                },
            )
            .await
            .unwrap();
        assert_eq!(request.air_artifacts[0].profile.kind(), "plan");
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
                vec![air_reference(AirArtifactProfile::TraceNativeRun)],
            )
            .await
            .unwrap();
        assert_eq!(completed.status, RequestStatus::Completed);
        assert_eq!(
            completed.reply.as_ref().unwrap().air_artifacts[0]
                .profile
                .label(),
            "AIR TRACE"
        );
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
    async fn direct_wake_claims_before_delivery_and_tracks_submission_phase() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "apply the scoped change".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: vec!["src/auth.rs".into()],
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();

        let prepared = mailbox
            .prepare_direct_wake(&recipient, &request.id)
            .await
            .unwrap()
            .expect("queued request should be reserved");
        assert_eq!(prepared.status, RequestStatus::Claimed);
        assert_eq!(prepared.wake_delivery, Some(WakeDeliveryState::Prepared));
        assert!(matches!(
            mailbox.cancel_for(&sender, &request.id).await,
            Err(CollaborationError::AlreadyClaimed(_))
        ));
        assert_eq!(mailbox.pending_unnotified().await.len(), 1);

        mailbox.mark_wake_prompt_written(&request.id).await.unwrap();
        assert_eq!(
            mailbox.pending_unnotified().await[0].wake_delivery,
            Some(WakeDeliveryState::PromptWritten)
        );

        mailbox.mark_notified(&request.id).await.unwrap();
        assert!(mailbox.pending_unnotified().await.is_empty());
        let delivered = mailbox.get_for(&recipient, &request.id).await.unwrap();
        assert_eq!(delivered.status, RequestStatus::Claimed);
        assert_eq!(delivered.wake_delivery, None);
        assert!(delivered.notified_at.is_some());
    }

    #[tokio::test]
    async fn inbox_pull_supersedes_prepared_direct_wake() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let recipient = participant("%2", "recipient");
        let request = mailbox
            .create(
                participant("%1", "sender"),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Notice,
                    body: "deployment finished".into(),
                    expects_reply: false,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        mailbox
            .prepare_direct_wake(&recipient, &request.id)
            .await
            .unwrap()
            .unwrap();

        let inbox = mailbox.claim_for(&recipient).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].body, "deployment finished");
        assert_eq!(inbox[0].status, RequestStatus::Completed);
        assert_eq!(inbox[0].wake_delivery, None);
        assert!(inbox[0].notified_at.is_some());
        assert!(mailbox.pending_unnotified().await.is_empty());
    }

    #[tokio::test]
    async fn direct_wake_recovery_phase_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("collaboration.json")),
            ..CollaborationOptions::default()
        };
        let recipient = participant("%2", "recipient");
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        let request = mailbox
            .create(
                participant("%1", "sender"),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "survive a daemon restart".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        mailbox
            .prepare_direct_wake(&recipient, &request.id)
            .await
            .unwrap()
            .unwrap();
        mailbox.mark_wake_prompt_written(&request.id).await.unwrap();
        drop(mailbox);

        let reloaded = CollaborationStore::load(options).await.unwrap();
        let pending = reloaded.pending_unnotified().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, RequestStatus::Claimed);
        assert_eq!(
            pending[0].wake_delivery,
            Some(WakeDeliveryState::PromptWritten)
        );
    }

    #[tokio::test]
    async fn delivered_direct_notice_completes_without_a_reply() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let recipient = participant("%2", "recipient");
        let request = mailbox
            .create(
                participant("%1", "sender"),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Notice,
                    body: "build completed".into(),
                    expects_reply: false,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        mailbox
            .prepare_direct_wake(&recipient, &request.id)
            .await
            .unwrap()
            .unwrap();
        mailbox.mark_wake_prompt_written(&request.id).await.unwrap();
        mailbox.mark_notified(&request.id).await.unwrap();

        let delivered = mailbox.get_for(&recipient, &request.id).await.unwrap();
        assert_eq!(delivered.status, RequestStatus::Completed);
        assert!(delivered.reply.is_none());
        assert!(mailbox.pending_unnotified().await.is_empty());
    }

    #[tokio::test]
    async fn durable_changes_wake_every_subscriber() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut first = mailbox.subscribe();
        let mut second = mailbox.subscribe();

        mailbox
            .create(
                participant("%1", "sender"),
                participant("%2", "recipient"),
                NewRequest {
                    kind: RequestKind::Question,
                    body: "wake both subscribers".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_millis(100), first.changed())
            .await
            .expect("first subscriber should wake")
            .expect("change sender remains live");
        tokio::time::timeout(Duration::from_millis(100), second.changed())
            .await
            .expect("second subscriber should wake")
            .expect("change sender remains live");
        assert_eq!(*first.borrow_and_update(), 1);
        assert_eq!(*second.borrow_and_update(), 1);
    }

    #[tokio::test]
    async fn wait_for_terminal_resumes_on_reply_without_polling() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "event-driven review".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        mailbox.claim_for(&recipient).await.unwrap();

        let waiting_mailbox = mailbox.clone();
        let waiting_sender = sender.clone();
        let request_id = request.id.clone();
        let waiter = tokio::spawn(async move {
            waiting_mailbox
                .wait_for_terminal(&waiting_sender, &request_id, Duration::from_secs(2))
                .await
        });
        tokio::task::yield_now().await;
        mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "done".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();

        let completed = tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("revision should wake the waiter immediately")
            .expect("wait task should not panic")
            .expect("wait should succeed");
        assert_eq!(completed.status, RequestStatus::Completed);
        assert_eq!(completed.reply.unwrap().body, "done");
        assert_eq!(mailbox.unread_reply_count(&sender).await, 0);
    }

    #[tokio::test]
    async fn wait_for_terminal_timeout_returns_latest_authoritative_state() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let request = mailbox
            .create(
                sender.clone(),
                participant("%2", "recipient"),
                NewRequest {
                    kind: RequestKind::Question,
                    body: "not answered".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();

        let latest = mailbox
            .wait_for_terminal(&sender, &request.id, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(latest.status, RequestStatus::Queued);
        assert_eq!(latest.id, request.id);
    }

    #[tokio::test]
    async fn invalid_air_reference_is_rejected_before_persistence() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut invalid = air_reference(AirArtifactProfile::WorkflowSkill);
        invalid.artifact_id = format!("urn:air:sha256:{}", "A".repeat(64));
        let result = mailbox
            .create(
                participant("%1", "sender"),
                participant("%2", "recipient"),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "review workflow".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: vec![invalid],
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(CollaborationError::InvalidAirArtifactReference(_))
        ));
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
                    air_artifacts: Vec::new(),
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
                    air_artifacts: Vec::new(),
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
                    air_artifacts: Vec::new(),
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

    #[tokio::test]
    async fn failed_persistence_rolls_back_request_before_wake_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("mailbox");
        std::fs::create_dir(&parent).unwrap();
        let options = CollaborationOptions {
            path: Some(parent.join("collaboration.json")),
            ..CollaborationOptions::default()
        };
        let mailbox = CollaborationStore::load(options).await.unwrap();

        std::fs::remove_dir(&parent).unwrap();
        std::fs::write(&parent, b"blocks create_dir_all").unwrap();
        let sender = participant("%1", "sender");
        let result = mailbox
            .create(
                sender.clone(),
                participant("%2", "recipient"),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "must be durable before delivery".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await;

        assert!(matches!(result, Err(CollaborationError::Persistence(_))));
        assert!(mailbox.pending_unnotified().await.is_empty());
        assert!(mailbox
            .list_for(&sender, RequestMailbox::All)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn delivered_wake_marker_stays_in_memory_when_persistence_fails() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("mailbox");
        let path = parent.join("collaboration.json");
        let mailbox = CollaborationStore::load(CollaborationOptions {
            path: Some(path.clone()),
            ..CollaborationOptions::default()
        })
        .await
        .unwrap();
        let request = mailbox
            .create(
                participant("%1", "sender"),
                participant("%2", "recipient"),
                NewRequest {
                    kind: RequestKind::Question,
                    body: "persist me first".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();

        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(&parent).unwrap();
        std::fs::write(&parent, b"blocks create_dir_all").unwrap();
        assert!(matches!(
            mailbox.mark_notified(&request.id).await,
            Err(CollaborationError::Persistence(_))
        ));
        assert!(mailbox.pending_unnotified().await.is_empty());
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
            resolve_target(&sender, "peer", &peers, CollaborationScope::Window),
            Err(CollaborationError::AmbiguousTarget(_))
        ));
        assert_eq!(
            resolve_target(&sender, "%2", &peers, CollaborationScope::Window)
                .unwrap()
                .pane,
            "%2"
        );
    }

    #[test]
    fn host_scope_reaches_panes_in_other_rooms_but_room_selectors_do_not() {
        let sender = participant("%1", "s1");
        let mut peers = vec![participant("%2", "s2")];
        let mut other_room = participant("%99", "s99");
        other_room.room.window_id = "@other".into();
        peers.push(other_room);

        assert_eq!(
            resolve_target(&sender, "%99", &peers, CollaborationScope::Host)
                .unwrap()
                .pane,
            "%99"
        );
        // Same target, default scope: refused — co-location is the consent.
        assert!(resolve_target(&sender, "%99", &peers, CollaborationScope::Window).is_err());
        // `peer` stays a room concept even under host scope.
        assert_eq!(
            resolve_target(&sender, "peer", &peers, CollaborationScope::Host)
                .unwrap()
                .pane,
            "%2"
        );
    }

    /// The bug this whole console notion exists for: pressing `m` on the row
    /// you happened to be sitting in when you opened `muxa watch` used to find
    /// no recipient, because the launch pane's agent *was* the sender.
    #[test]
    fn a_console_can_address_the_pane_it_was_opened_from() {
        let launch = participant("%1", "the-agent-under-the-cursor");
        let peers = vec![launch.clone(), participant("%2", "s2")];
        let origin = CollaborationOrigin {
            pane: "%1".into(),
            socket: Some("default".into()),
            console: true,
        };
        let console = resolve_origin(&origin, &peers, &[pane_info("%1")]).unwrap();

        assert!(console.console);
        assert_eq!(console.label(), "console");
        assert_eq!(
            resolve_target(&console, "pane:%1", &peers, CollaborationScope::Host)
                .unwrap()
                .agent_session_id,
            "the-agent-under-the-cursor"
        );
        // The same origin without the console flag is the old behaviour: the
        // launch pane resolves to itself and is filtered out as the sender.
        let agent = resolve_origin(
            &CollaborationOrigin {
                console: false,
                ..origin
            },
            &peers,
            &[pane_info("%1")],
        )
        .unwrap();
        assert!(matches!(
            resolve_target(&agent, "pane:%1", &peers, CollaborationScope::Host),
            Err(CollaborationError::UnknownTarget(_))
        ));
    }

    /// A console keeps working from a pane that hosts no agent, and from a
    /// pane the backend cannot even list — messaging must not depend on where
    /// the popup was opened.
    #[test]
    fn a_console_resolves_without_a_tracked_launch_pane() {
        let peers = vec![participant("%2", "s2")];

        let shell = resolve_origin(
            &CollaborationOrigin {
                pane: "%77".into(),
                socket: None,
                console: true,
            },
            &peers,
            &[pane_info("%77")],
        )
        .unwrap();
        assert_eq!(shell.room.window_id, "@1");
        assert_eq!(
            resolve_target(&shell, "pane:%2", &peers, CollaborationScope::Host)
                .unwrap()
                .pane,
            "%2"
        );

        let unlisted = resolve_origin(
            &CollaborationOrigin {
                pane: String::new(),
                socket: None,
                console: true,
            },
            &peers,
            &[],
        )
        .unwrap();
        assert_eq!(unlisted.room.window_id, CONSOLE_PANE);
        assert_eq!(
            resolve_target(&unlisted, "pane:%2", &peers, CollaborationScope::Host)
                .unwrap()
                .pane,
            "%2"
        );
    }

    #[test]
    fn a_console_requires_an_endpoint_when_pane_ids_repeat_across_servers() {
        let first = pane_info("%1");
        let mut second = pane_info("%1");
        second.socket = Some("other".into());
        second.window_id = "@other".into();
        let panes = [first, second];

        let ambiguous = resolve_origin(
            &CollaborationOrigin {
                pane: "%1".into(),
                socket: None,
                console: true,
            },
            &[],
            &panes,
        );
        assert!(matches!(
            ambiguous,
            Err(CollaborationError::AmbiguousOrigin(pane)) if pane == "%1"
        ));

        let resolved = resolve_origin(
            &CollaborationOrigin {
                pane: "%1".into(),
                socket: Some("/tmp/tmux-1000/default".into()),
                console: true,
            },
            &[],
            &panes,
        )
        .unwrap();
        assert_eq!(resolved.room.socket.as_deref(), Some("default"));
        assert_eq!(resolved.room.window_id, "@1");
    }

    #[test]
    fn agent_origin_normalizes_full_and_short_tmux_socket_names() {
        let agent = participant("%1", "sender");
        let resolved = resolve_origin(
            &CollaborationOrigin {
                pane: "%1".into(),
                socket: Some("/tmp/tmux-1000/default".into()),
                console: false,
            },
            std::slice::from_ref(&agent),
            &[pane_info("%1")],
        )
        .unwrap();
        assert_eq!(resolved, agent);
    }

    /// Console identity is fixed, so one operator's dispatch log stays a
    /// single thread no matter which window each popup was opened from.
    #[tokio::test]
    async fn console_sent_mailbox_survives_moving_between_windows() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let recipient = participant("%2", "recipient");
        let from_here = resolve_origin(
            &CollaborationOrigin {
                pane: "%1".into(),
                socket: None,
                console: true,
            },
            &[],
            &[pane_info("%1")],
        )
        .unwrap();
        let mut elsewhere = pane_info("%9");
        elsewhere.window_id = "@other".into();
        let other_window = resolve_origin(
            &CollaborationOrigin {
                pane: "%9".into(),
                socket: None,
                console: true,
            },
            &[],
            std::slice::from_ref(&elsewhere),
        )
        .unwrap();
        assert_ne!(from_here.room, other_window.room);

        for console in [&from_here, &other_window] {
            mailbox
                .create(
                    console.clone(),
                    recipient.clone(),
                    NewRequest {
                        kind: RequestKind::Task,
                        body: "do the thing".into(),
                        expects_reply: true,
                        work_mode: WorkMode::ReadOnly,
                        paths: Vec::new(),
                        air_artifacts: Vec::new(),
                    },
                )
                .await
                .unwrap();
        }

        assert_eq!(
            mailbox
                .list_for(&from_here, RequestMailbox::Sent)
                .await
                .unwrap()
                .len(),
            2
        );
        // And the replies land in the recipient's mailbox, which is where the
        // operator reads them — by pointing the cursor at that row.
        assert_eq!(
            mailbox
                .list_for(&recipient, RequestMailbox::Incoming)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    /// A console has no pane to be woken at, so its replies must leave the
    /// wake queue on their own — otherwise every reply a human ever collected
    /// is re-scanned on each daemon tick — and must not accumulate in an
    /// unread badge that nothing can clear.
    #[tokio::test]
    async fn console_replies_leave_no_permanent_wake_or_unread_debt() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let agent = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let console = resolve_origin(
            &CollaborationOrigin {
                pane: "%1".into(),
                socket: None,
                console: true,
            },
            &[],
            &[pane_info("%1")],
        )
        .unwrap();

        let mut ids = Vec::new();
        for sender in [console.clone(), agent.clone()] {
            let request = mailbox
                .create(
                    sender,
                    recipient.clone(),
                    NewRequest {
                        kind: RequestKind::Question,
                        body: "who sent this".into(),
                        expects_reply: true,
                        work_mode: WorkMode::ReadOnly,
                        paths: Vec::new(),
                        air_artifacts: Vec::new(),
                    },
                )
                .await
                .unwrap();
            ids.push(request.id);
        }
        mailbox.claim_for(&recipient).await.unwrap();
        for id in &ids {
            mailbox
                .reply(
                    &recipient,
                    id,
                    RequestStatus::Completed,
                    "answered".into(),
                    Vec::new(),
                    Vec::new(),
                )
                .await
                .unwrap();
        }

        // Only the pane-backed sender is still owed a wake.
        let pending = mailbox.pending_reply_unnotified().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from.pane, "%1");
        assert!(!pending[0].from.console);

        assert_eq!(mailbox.unread_reply_count(&console).await, 0);
        assert_eq!(mailbox.unread_reply_count(&agent).await, 1);
    }

    #[tokio::test]
    async fn a_console_cannot_squat_an_alias_in_the_room_it_borrows() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let console = resolve_origin(
            &CollaborationOrigin {
                pane: "%1".into(),
                socket: None,
                console: true,
            },
            &[],
            &[pane_info("%1")],
        )
        .unwrap();

        assert!(matches!(
            mailbox
                .set_identity(&console, &[], Some("reviewer".into()), Vec::new())
                .await,
            Err(CollaborationError::ConsoleIdentity)
        ));
    }

    #[tokio::test]
    async fn aliases_and_roles_route_only_to_live_exact_sessions() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let reviewer = participant("%2", "reviewer-session");
        let second_reviewer = participant("%3", "second-reviewer-session");
        let live = vec![sender.clone(), reviewer.clone(), second_reviewer.clone()];

        mailbox
            .set_identity(
                &reviewer,
                &live,
                Some("Reviewer".into()),
                vec!["Rust".into(), "review".into()],
            )
            .await
            .unwrap();
        mailbox
            .set_identity(
                &second_reviewer,
                &live,
                Some("Verifier".into()),
                vec!["review".into()],
            )
            .await
            .unwrap();
        assert!(matches!(
            mailbox
                .set_identity(&second_reviewer, &live, Some("reviewer".into()), Vec::new(),)
                .await,
            Err(CollaborationError::AliasInUse(_))
        ));

        let mut enriched = live;
        mailbox.enrich_participants(&mut enriched).await;
        let enriched_sender = enriched
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap();
        assert_eq!(
            resolve_target(
                enriched_sender,
                "@REVIEWER",
                &enriched,
                CollaborationScope::Window
            )
            .unwrap()
            .pane,
            "%2"
        );
        assert_eq!(
            resolve_target(
                enriched_sender,
                "role:rust",
                &enriched,
                CollaborationScope::Window
            )
            .unwrap()
            .pane,
            "%2"
        );
        assert!(matches!(
            resolve_target(
                enriched_sender,
                "role:review",
                &enriched,
                CollaborationScope::Window
            ),
            Err(CollaborationError::AmbiguousTarget(_))
        ));

        let mut replacement = vec![participant("%2", "replacement-session")];
        mailbox.enrich_participants(&mut replacement).await;
        assert!(replacement[0].alias.is_none());
        assert!(replacement[0].roles.is_empty());
    }

    #[tokio::test]
    async fn identity_survives_reload_without_following_pane_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collaboration.json");
        let options = CollaborationOptions {
            path: Some(path),
            ..CollaborationOptions::default()
        };
        let original = participant("%4", "original-session");
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        mailbox
            .set_identity(
                &original,
                std::slice::from_ref(&original),
                Some("builder".into()),
                vec!["implementation".into()],
            )
            .await
            .unwrap();

        let reloaded = CollaborationStore::load(options).await.unwrap();
        let mut participants = vec![original, participant("%4", "replacement-session")];
        reloaded.enrich_participants(&mut participants).await;
        assert_eq!(participants[0].alias.as_deref(), Some("builder"));
        assert_eq!(participants[0].roles, vec!["implementation"]);
        assert!(participants[1].alias.is_none());
    }

    #[test]
    fn collaboration_client_hello_labels_round_trip_without_granting_authority() {
        for kind in [
            CollaborationClientKind::Cli,
            CollaborationClientKind::Watch,
            CollaborationClientKind::Mcp,
            CollaborationClientKind::Dashboard,
        ] {
            assert_eq!(
                CollaborationClientKind::from_hello_label(&kind.hello_label()),
                kind
            );
        }
        assert_eq!(
            CollaborationClientKind::from_hello_label("muxa/0.8.29"),
            CollaborationClientKind::Unknown
        );
    }
}
