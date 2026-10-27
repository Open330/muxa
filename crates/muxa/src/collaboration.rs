//! Durable, same-room request/reply collaboration for pane-hosted agents.
//!
//! tmux supplies topology; muxad remains the broker. Messages are pinned to
//! the concrete agent session occupying a pane so a later process reusing the
//! pane never inherits stale work.

mod overview;
pub use overview::{
    CollaborationEndpoint, CollaborationLink, CollaborationLocation, CollaborationOverview,
};
mod purpose;
mod worker_inventory;
use purpose::{load_purposes, PurposeGrant};
pub use purpose::{
    LaunchPurpose, HUMAN_PRIORITY_CAPABILITY, PIPELINE_PURPOSE_CAPABILITY,
    PURPOSE_PROMOTE_CAPABILITY,
};
pub use worker_inventory::{ObservedIdle, WorkerEntry, WorkerFlag, WorkerInventory};

use crate::event::{AgentKind, AgentState};
use crate::state::Agent;
use crate::tmux::PaneInfo;
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::io::AsyncReadExt;
use tokio::sync::{watch, Mutex, RwLock};

pub const COLLABORATION_SCHEMA_VERSION: u32 = 1;
const COLLABORATION_DATABASE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_QUERY_LIMIT: usize = 100;
const MAX_QUERY_LIMIT: usize = 500;
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
    /// Optional terminal-thread retention. `None` preserves history forever.
    pub retention_days: Option<u64>,
}

impl Default for CollaborationOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            max_message_bytes: 16 * 1024,
            scope: crate::config::CollaborationScope::default(),
            retention_days: None,
        }
    }
}

/// Explicit daemon support for the human decision protocol.
pub const HUMAN_ACTIONS_CAPABILITY: &str = "collaboration_human_actions_v1";

/// The stable pane-shaped identity of the operator console. It is not a real
/// pane id — `pane_id_host_kind` rejects it, which is what keeps the wake path
/// from ever trying to type at a human.
pub const CONSOLE_PANE: &str = "console";
/// The console's agent session id. Fixed, so every request a human dispatches
/// shares one sender identity no matter which pane the console was opened from.
pub const CONSOLE_SESSION_ID: &str = "console";

/// Session-id prefix of a **pending** recipient: a muxa-launched pane that
/// hosts an agent CLI which has not registered a session yet.
///
/// Codex is why this exists. Its `SessionStart` hook fires when the first
/// prompt is submitted, not when the TUI boots — a freshly spawned codex pane
/// therefore cannot become a participant until something types into it, while
/// the sender is waiting for exactly that registration. Addressing the *pane*
/// breaks the deadlock: the request is queued immediately, the daemon's waker
/// delivers it once the pane reads idle (a screen-detected row is enough), and
/// the concrete session that registers there adopts the request the first time
/// it claims or answers it — see [`pin_pending_recipient`].
///
/// The id is only ever a placeholder inside `to`. Every ordinary match stays
/// session-pinned; [`addresses`] is the single seam where a pane-scoped
/// recipient is honoured.
pub const PENDING_SESSION_PREFIX: &str = "pending-pane:";

/// Is this the placeholder identity of a pane whose agent has not registered?
#[must_use]
pub fn is_pending_session(session_id: &str) -> bool {
    session_id.starts_with(PENDING_SESSION_PREFIX)
}

/// The placeholder session id for one pane on one control endpoint. Pane ids
/// repeat across tmux servers, so the endpoint rides along: `same_endpoint`
/// compares `(pane, socket, session)` and must not collapse two servers'
/// `%3` into one recipient.
fn pending_session_id(pane: &str, socket: Option<&str>) -> String {
    match socket {
        Some(socket) => format!("{PENDING_SESSION_PREFIX}{socket}:{pane}"),
        None => format!("{PENDING_SESSION_PREFIX}{pane}"),
    }
}

/// Does `caller` own the request addressed to `to`?
///
/// The ordinary answer is the session-pinned one. The exception is a *pending*
/// recipient: the request was addressed to a pane before any session existed
/// there, so the real agent that registered on that pane — and only on that
/// exact pane and control endpoint — owns it too.
/// Whether `caller` may receive a request addressed to `to`: the exact
/// session, or, for a request still waiting on a launching agent, the agent
/// that now occupies that pane *in the same room*. Delivery reserves the
/// request through this predicate, so anything picking a recipient must use
/// it too: a candidate it rejects can never be served.
pub fn addresses(to: &Participant, caller: &Participant) -> bool {
    if to.same_session(caller) {
        return true;
    }
    is_pending_session(&to.agent_session_id)
        && !is_pending_session(&caller.agent_session_id)
        && !caller.console
        && to.pane == caller.pane
        && to.socket == caller.socket
        // The room too, not just the endpoint. Session ids are globally
        // unique, so the session-pinned path needs no such guard; pane ids are
        // only unique per server and restart at `%0` when one does, and a
        // queued request outlives that. Without the room a stale pending
        // request could be adopted by whatever agent later occupies the same
        // pane id in an unrelated window.
        && to.room == caller.room
}

fn same_endpoint_in_room(left: &Participant, right: &Participant) -> bool {
    left.same_endpoint(right) && left.room == right.room
}

fn dispatch_request_id(
    input: &NewRequest,
    now: OffsetDateTime,
) -> Result<String, CollaborationError> {
    let id = if let Some(dispatch_id) = input.dispatch_id.as_deref() {
        let uuid = uuid::Uuid::parse_str(dispatch_id).map_err(|_| {
            CollaborationError::InvalidDispatch("dispatch_id must be a UUID".into())
        })?;
        format!("dispatch-{uuid}")
    } else {
        next_request_id(now)
    };
    if let Some(parent) = &input.delegation_parent {
        if parent.request_id.is_empty() || parent.request_id.len() > 256 {
            return Err(CollaborationError::InvalidDispatch(
                "parent request id must be 1..256 bytes".into(),
            ));
        }
    }
    Ok(id)
}

fn dispatch_matches(
    existing: &CollaborationRequest,
    from: &Participant,
    to: &Participant,
    input: &NewRequest,
    body: &str,
) -> bool {
    // A request that followed its slot is matched by that slot: a retry
    // resolves the slot afresh and may reach the successor. Two different
    // slots are different requests even when they reach the same session.
    let same_slot = match (&existing.to_slot, &input.recipient_slot) {
        (Some(existing), Some(retry)) => Some(existing.same_slot(retry)),
        _ => None,
    };
    existing.from.same_endpoint(from)
        && same_slot != Some(false)
        && (existing.to.same_endpoint(to) || addresses(&existing.to, to) || same_slot == Some(true))
        && existing.kind == input.kind
        && existing.body == body
        && existing.expects_reply == input.expects_reply
        && existing.notify == input.notify
        && existing.human_action == input.human_action
        && existing.work_mode == input.work_mode
        && existing.source_node_id == input.source_node_id
        && existing.delegation_parent == input.delegation_parent
        && existing.parent_request_id == normalize_optional_id(input.parent_request_id.clone())
        && input
            .thread_id
            .as_ref()
            .is_none_or(|v| existing.thread_id.as_ref() == Some(v))
        && existing.workspace_id == normalize_optional_id(input.workspace_id.clone())
        && existing.work_id == normalize_optional_id(input.work_id.clone())
        && existing.run_id == normalize_optional_id(input.run_id.clone())
        && existing.paths == input.paths
        && existing.artifacts == input.artifacts
        && existing.links == input.links
        && existing.air_artifacts == input.air_artifacts
        && existing.withdrew_reply_wakes == normalize_withdrawals(&input.withdraw_reply_wakes)
}

/// Release the reply wake of each listed request as withdrawn, returning the
/// rows that changed. A wake already released keeps its first reason; a
/// reservation in flight is voided.
fn withdraw_reply_wakes_in(
    requests: &mut HashMap<String, CollaborationRequest>,
    ids: &[String],
    now: OffsetDateTime,
) -> Vec<CollaborationRequest> {
    ids.iter()
        .filter_map(|id| {
            let request = requests.get_mut(id)?;
            if matches!(request.reply_wake, Some(ReplyWake::Released { .. })) {
                return None;
            }
            request.reply_wake = Some(ReplyWake::Released {
                reason: ReplyWakeRelease::Withdrawn,
                at: now,
            });
            Some(request.clone())
        })
        .collect()
}

fn sort_by_reply_time(requests: &mut [CollaborationRequest]) {
    requests.sort_by(|left, right| {
        let key = |request: &CollaborationRequest| request.reply.as_ref().map(|reply| reply.at);
        key(left)
            .cmp(&key(right))
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// A blank id is a caller mistake, not something to skip: dropping it would
/// let the rest of an all-or-nothing withdrawal go through.
fn reject_blank_withdrawals(ids: &[String]) -> Result<(), CollaborationError> {
    if ids.iter().any(|id| id.trim().is_empty()) {
        return Err(CollaborationError::InvalidWithdrawal(
            "request ids must not be blank".into(),
        ));
    }
    Ok(())
}

/// Trimmed, de-duplicated, order-preserving withdrawal ids.
fn normalize_withdrawals(ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty() && seen.insert(*id))
        .map(str::to_string)
        .collect()
}

fn same_participant_pair(
    parent: &CollaborationRequest,
    from: &Participant,
    to: &Participant,
) -> bool {
    (same_endpoint_in_room(&parent.from, from) && same_endpoint_in_room(&parent.to, to))
        || (same_endpoint_in_room(&parent.from, to) && same_endpoint_in_room(&parent.to, from))
}

/// Pin a pending recipient to the concrete session now acting as it, so every
/// later match is the ordinary session-pinned one and a *different* agent
/// reusing the pane can never inherit the work.
///
/// Returns whether anything changed, so callers can persist only real edits.
fn pin_pending_recipient(request: &mut CollaborationRequest, caller: &Participant) -> bool {
    if !is_pending_session(&request.to.agent_session_id)
        || is_pending_session(&caller.agent_session_id)
        || caller.console
    {
        return false;
    }
    // Keep the alias/roles the launcher stamped when the registering session
    // has none of its own: they are what `@alias` / `role:` routing already
    // resolved this pane by.
    let mut pinned = caller.clone();
    if pinned.alias.is_none() {
        pinned.alias.clone_from(&request.to.alias);
    }
    if pinned.roles.is_empty() {
        pinned.roles.clone_from(&request.to.roles);
    }
    request.to = pinned;
    true
}

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
    /// The calling agent's own session id, when its host supplies one
    /// (Codex stamps a thread id on every MCP call and shell command).
    ///
    /// It is the exact identity: a pane can be claimed by more than one
    /// registry row, or by the wrong one when the caller's pane variables were
    /// inherited from another pane (Codex's shared app-server). A participant
    /// with this session wins over a pane match; without one, `pane` decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
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
    /// Unknown and human-facing sessions are protected from inbound peer work.
    #[serde(default)]
    pub launch_purpose: LaunchPurpose,
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
            launch_purpose: LaunchPurpose::Primary,
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

    pub fn accepts_peer_work(&self) -> bool {
        self.console || self.launch_purpose == LaunchPurpose::Worker
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

    /// Durable mailbox ownership follows a registered session across pane/window
    /// moves on the same host and control socket. Pending recipients remain
    /// pinned to their endpoint until a real session adopts them.
    pub fn same_session(&self, other: &Self) -> bool {
        if self.console
            || other.console
            || self.agent_session_id.is_empty()
            || is_pending_session(&self.agent_session_id)
        {
            return self.same_endpoint(other);
        }
        self.agent_session_id == other.agent_session_id
            && self.socket == other.socket
            && self.room.host == other.room.host
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

/// The sender's claim on being woken for a request's terminal reply.
///
/// Absent means the wake is still owed once the request carries a terminal
/// reply. `Reserved` is one delivery attempt that the store handed to the
/// daemon's waker; only that attempt's token may complete it, so a read or
/// withdrawal that lands mid-attempt is never undone by the attempt's
/// outcome. `Released` is final: the sender read the reply, said it no longer
/// wants the wake, or was told.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ReplyWake {
    Reserved {
        token: u64,
    },
    Released {
        reason: ReplyWakeRelease,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplyWakeRelease {
    /// The sender retrieved the reply before any wake landed.
    Read,
    /// The sender withdrew its interest; the reply stays readable and unread.
    Withdrawn,
    /// A wake prompt reached the sender's pane.
    Delivered,
}

/// Replies the store reserved for one wake prompt to one sender. Holds no
/// lock; [`CollaborationStore::complete_reply_wakes`] settles exactly these
/// members, and only those still carrying this batch's token.
#[derive(Debug, Clone)]
pub struct ReplyWakeBatch {
    pub token: u64,
    /// Reserved replies, oldest reply first.
    pub requests: Vec<CollaborationRequest>,
    /// Further owed replies for the same sender that did not fit this batch.
    /// They stay owed and are offered by a later batch.
    pub remaining: usize,
}

/// Most replies a single wake prompt advertises.
pub const REPLY_WAKE_BATCH_LIMIT: usize = 5;

/// Most request ids one withdrawal names.
pub const MAX_REPLY_WAKE_WITHDRAWALS: usize = 16;

/// `hello` capability: the daemon supports withdrawing reply-wake interest,
/// alone or alongside a send. A client must not send `withdraw_reply_wakes`
/// to a daemon without it, which would silently ignore the field.
pub const REPLY_WAKE_WITHDRAW_CAPABILITY: &str = "collaboration_reply_withdraw_v1";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestMailbox {
    Incoming,
    Sent,
    #[default]
    All,
}

/// How wide a mailbox listing reaches.
///
/// [`RequestMailbox`] picks a *direction*; this picks *whose* traffic is in
/// scope. The two are independent — `Sent` at [`MailboxScope::Room`] is
/// everything the room dispatched, not just what the caller sent.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MailboxScope {
    /// The caller's own mailbox — all an agent is entitled to.
    #[default]
    Caller,
    /// Every participant in the caller's room.
    Room,
    /// Every request the store holds, across rooms, sessions and hosts.
    ///
    /// `mailbox` stops applying here: with no endpoint to sit on one side of,
    /// every request is equally incoming and sent.
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

/// A progress report or guidance update, separate from the one terminal reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollaborationUpdate {
    pub sequence: u64,
    pub author: Participant,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Cross-node causal link. Metadata only; it grants no participant authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DelegationRef {
    pub node_id: crate::fleet::NodeId,
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollaborationRequest {
    /// Caller-generated UUID, reused for retries of the same immutable request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_node_id: Option<crate::fleet::NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_parent: Option<DelegationRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<PeerInterruption>,
    /// Explicit operator decision. Never inferred from prose or the sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_action: Option<HumanAction>,
    pub id: String,
    pub from: Participant,
    pub to: Participant,
    /// Advisory identity of the agent initiating a Fleet console request;
    /// never used for authorization or participant matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<Box<Participant>>,
    /// Bounded nonterminal conversation on this exact request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updates: Vec<CollaborationUpdate>,
    /// How the request entered muxad. `from` remains the represented agent;
    /// this field identifies the local caller that exercised that authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<CollaborationProvenance>,
    pub kind: RequestKind,
    pub body: String,
    pub expects_reply: bool,
    /// Override recipient wake delivery. None keeps one-way notices quiet and
    /// wakes actionable requests. This never changes the reply contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
    pub work_mode: WorkMode,
    /// Stable id shared by every request in one causal conversation.
    ///
    /// A child whose caller omits this value inherits its parent's canonical
    /// thread id. For a legacy parent with no explicit id, the parent's
    /// request id becomes the canonical thread id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// The exact request that caused this request. The store rejects missing
    /// parents and cross-thread parent links when creating new requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Generic artifact identifiers or locators. AIR artifacts retain their
    /// typed representation in `air_artifacts`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    /// Generic relationship/URL locators associated with the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub air_artifacts: Vec<AirArtifactReference>,
    /// Earlier requests by the same sender whose reply wake was withdrawn
    /// when this one was sent. Informational: it does not mark them replaced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withdrew_reply_wakes: Vec<String>,
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
    /// The logical slot the sender addressed (`@alias`, `role:`, or a
    /// `workspace/work/role` path), next to the session it resolved to.
    /// Absent for pane targets, which pin one session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_slot: Option<RecipientSlot>,
    /// Sessions this request was addressed to before it followed its
    /// recipient (a moved session, or a successor in its slot), oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rebound_from: Vec<String>,
    /// Whether the sender is still owed a wake for the terminal reply. Rows
    /// written before this field existed derive it from the two timestamps
    /// above on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_wake: Option<ReplyWake>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<CollaborationReply>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanAction {
    Approval,
    Choice,
    Information,
}

impl CollaborationRequest {
    /// Whether the sender is still owed a wake for this request's reply.
    ///
    /// A console sender is excluded: it has no pane to type into, so the wake
    /// could never be delivered. Its reply is read off the request instead.
    pub fn reply_wake_owed(&self) -> bool {
        self.status.is_terminal()
            && self.reply.is_some()
            && self.reply_wake.is_none()
            && !self.from.console
    }

    /// Fill `reply_wake` for a row loaded from disk. A reservation names an
    /// attempt by a previous daemon whose outcome is unknown, so it is owed
    /// again (a wake that did land may repeat, as before this state existed).
    /// Rows from before the field derive it from their timestamps.
    fn normalize_loaded_reply_wake(&mut self) {
        match self.reply_wake {
            Some(ReplyWake::Released { .. }) => {}
            Some(ReplyWake::Reserved { .. }) | None => {
                self.reply_wake = if let Some(at) = self.reply_read_at {
                    Some(ReplyWake::Released {
                        reason: ReplyWakeRelease::Read,
                        at,
                    })
                } else {
                    self.reply_notified_at.map(|at| ReplyWake::Released {
                        reason: ReplyWakeRelease::Delivered,
                        at,
                    })
                };
            }
        }
    }

    /// Record that the sender has the reply. Releases an owed wake and voids
    /// one in flight: that attempt's completion then finds no reservation of
    /// its own to settle. A wake already released keeps its first reason.
    pub(crate) fn mark_reply_read(&mut self, now: OffsetDateTime) {
        self.reply_read_at.get_or_insert(now);
        self.reply_notified_at.get_or_insert(now);
        if !matches!(self.reply_wake, Some(ReplyWake::Released { .. })) {
            self.reply_wake = Some(ReplyWake::Released {
                reason: ReplyWakeRelease::Read,
                at: now,
            });
        }
    }

    /// Whether the sender may withdraw this request's reply wake. Human
    /// decisions and interrupted-peer recovery have their own workflows;
    /// muting them could hide an answer the sender is required to act on.
    fn reply_wake_withdrawable(&self) -> bool {
        !self.to.console && self.human_action.is_none() && self.interruption.is_none()
    }

    /// Whether the recipient needs a wake; terminal replies use their own path.
    pub fn should_notify(&self) -> bool {
        self.notify
            .unwrap_or(self.kind != RequestKind::Notice || self.expects_reply)
    }

    pub fn needs_human_response(&self) -> bool {
        self.to.console
            && self.expects_reply
            && self.kind != RequestKind::Notice
            && matches!(self.status, RequestStatus::Queued | RequestStatus::Claimed)
    }
}

/// A handle the daemon has promised to one pane, pending the scan that will
/// show the pane holding it.
#[derive(Debug, Clone)]
struct HandleReservation {
    room: RoomId,
    pane: String,
    handle: String,
    at: OffsetDateTime,
}

/// How long a promise outlives the answer.
///
/// Long enough to cover a reconcile tick plus the CLI's tmux write, short
/// enough that a caller which died before writing does not hold a name out of
/// circulation for a session. A reservation is also dropped early, the moment
/// a scan shows its pane actually holding the handle.
const HANDLE_RESERVATION_TTL: time::Duration = time::Duration::minutes(2);

/// What a caller wants from the namespace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HandleRequest {
    /// Take the first free name in the `base`, `base2`, `base3`… family —
    /// what muxa mints for a pane nobody named.
    Mint { base: String },
    /// Claim this exact name, because a launcher was told to use it. Fails
    /// rather than picking something else: a pipeline's `reviewer` that
    /// silently became `reviewer2` would break the config that named it.
    Reserve { handle: String },
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
    /// Caller-generated UUID, reused for retries of the same immutable request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_node_id: Option<crate::fleet::NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_parent: Option<DelegationRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_action: Option<HumanAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<Box<Participant>>,
    pub kind: RequestKind,
    pub body: String,
    pub expects_reply: bool,
    /// Override recipient wake delivery. None keeps one-way notices quiet and
    /// wakes actionable requests. This never changes the reply contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
    pub work_mode: WorkMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(default)]
    pub air_artifacts: Vec<AirArtifactReference>,
    /// The sender's own earlier requests whose reply wake this request
    /// replaces. Withdrawn in the same transaction that stores this request;
    /// the earlier work and its reply are untouched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withdraw_reply_wakes: Vec<String>,
    /// Set by the daemon when the target was a logical slot; never read
    /// from a client.
    #[serde(skip)]
    pub recipient_slot: Option<RecipientSlot>,
}

impl Default for NewRequest {
    fn default() -> Self {
        Self {
            dispatch_id: None,
            source_node_id: None,
            delegation_parent: None,
            human_action: None,
            initiator: None,
            kind: RequestKind::Question,
            body: String::new(),
            expects_reply: true,
            notify: None,
            work_mode: WorkMode::ReadOnly,
            thread_id: None,
            parent_request_id: None,
            workspace_id: None,
            work_id: None,
            run_id: None,
            paths: Vec::new(),
            artifacts: Vec::new(),
            links: Vec::new(),
            air_artifacts: Vec::new(),
            withdraw_reply_wakes: Vec::new(),
            recipient_slot: None,
        }
    }
}

/// A logical recipient: whoever holds `role` and/or `alias` in one work
/// window. A request addressed this way may follow the slot to a successor
/// session while it is still undelivered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipientSlot {
    /// The window the slot lives in.
    pub room: RoomId,
    /// Durable work identity of that window, when muxa manages it. Preferred
    /// over `room`, whose window id does not survive a server restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

impl RecipientSlot {
    /// The slot `recipient` occupies for a role or alias selector.
    pub fn of(
        recipient: &Participant,
        panes: &[PaneInfo],
        role: Option<&str>,
        alias: Option<&str>,
    ) -> Self {
        let pane = pane_of(recipient, panes);
        Self {
            room: recipient.room.clone(),
            workspace_id: pane.and_then(|pane| pane.workspace_id.clone()),
            work_id: pane.and_then(|pane| pane.work_id.clone()),
            role: role.map(str::to_string),
            alias: alias.map(str::to_string),
        }
    }

    /// Whether `participant` holds this slot now.
    ///
    /// Always on the slot's own server: work ids are only unique per
    /// server. Within it, the durable work identity when the slot has one
    /// (a recreated window keeps it), else the exact window. Roles come from
    /// the participant, which is already seeded from the launch mark and
    /// overridden by any registered identity.
    pub fn held_by(&self, participant: &Participant, panes: &[PaneInfo]) -> bool {
        if participant.console
            || participant.room.host != self.room.host
            || participant.room.socket != self.room.socket
        {
            return false;
        }
        let in_work = match (self.workspace_id.as_deref(), self.work_id.as_deref()) {
            (Some(workspace), Some(work)) => pane_of(participant, panes).is_some_and(|pane| {
                pane.workspace_id.as_deref() == Some(workspace)
                    && pane.work_id.as_deref() == Some(work)
            }),
            _ => participant.room == self.room,
        };
        let role = self.role.as_deref().is_none_or(|role| {
            participant
                .roles
                .iter()
                .any(|held| held.eq_ignore_ascii_case(role))
        });
        let alias = self.alias.as_deref().is_none_or(|alias| {
            participant
                .alias
                .as_deref()
                .is_some_and(|held| held.eq_ignore_ascii_case(alias))
        });
        in_work && role && alias
    }

    /// Whether two slots name the same logical recipient: same server, same
    /// durable work (or the same window when there is none), same role and
    /// alias. The window id alone may differ after a work's window is
    /// recreated.
    pub fn same_slot(&self, other: &Self) -> bool {
        let text_eq = |left: Option<&str>, right: Option<&str>| match (left, right) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            (None, None) => true,
            _ => false,
        };
        let durable = |slot: &Self| slot.workspace_id.is_some() && slot.work_id.is_some();
        let same_work = if durable(self) && durable(other) {
            self.workspace_id == other.workspace_id && self.work_id == other.work_id
        } else {
            self.room == other.room
        };
        self.room.host == other.room.host
            && self.room.socket == other.room.socket
            && same_work
            && text_eq(self.role.as_deref(), other.role.as_deref())
            && text_eq(self.alias.as_deref(), other.alias.as_deref())
    }
}

/// The pane scan row for `participant`. Matched on its socket too, because
/// pane ids repeat across servers; only a participant with no known socket
/// falls back to the pane id alone.
fn pane_of<'p>(participant: &Participant, panes: &'p [PaneInfo]) -> Option<&'p PaneInfo> {
    panes.iter().find(|pane| {
        pane.pane_id == participant.pane
            && (participant.socket.is_none() || pane.socket == participant.socket)
    })
}

/// Parse `workspace/work/role`, `workspace/work/role@alias` or
/// `workspace/work/@alias`. Anything else is not a slot path.
pub fn parse_slot_path(selector: &str) -> Option<(String, String, Option<String>, Option<String>)> {
    if crate::backend::pane_id_host_kind(selector).is_some() || selector.starts_with("pane:") {
        return None;
    }
    let mut parts = selector.split('/');
    let (workspace, work, holder) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || workspace.is_empty() || work.is_empty() || holder.is_empty() {
        return None;
    }
    let (role, alias) = match holder.split_once('@') {
        Some((role, alias)) => (role, Some(alias)),
        None => (holder, None),
    };
    let role = (!role.is_empty()).then(|| role.to_string());
    let alias = alias.filter(|alias| !alias.is_empty()).map(str::to_string);
    if role.is_none() && alias.is_none() {
        return None;
    }
    Some((workspace.to_string(), work.to_string(), role, alias))
}

/// Resolve a `workspace/work/role[@alias]` path to the one live agent that
/// holds it, with the slot to record on the request.
///
/// # Errors
/// Unknown when no live agent holds the slot, ambiguous when several do.
pub fn resolve_slot_path(
    sender: &Participant,
    selector: &str,
    participants: &[Participant],
    panes: &[PaneInfo],
) -> Option<Result<(Participant, RecipientSlot), CollaborationError>> {
    let (workspace, work, role, alias) = parse_slot_path(selector)?;
    let probe = RecipientSlot {
        room: sender.room.clone(),
        workspace_id: Some(workspace),
        work_id: Some(work),
        role,
        alias,
    };
    let matches: Vec<_> = participants
        .iter()
        .filter(|candidate| !candidate.same_endpoint(sender) && probe.held_by(candidate, panes))
        .collect();
    Some(match matches.as_slice() {
        [recipient] => {
            let mut slot = probe.clone();
            slot.room = recipient.room.clone();
            Ok(((*recipient).clone(), slot))
        }
        [] => Err(CollaborationError::UnknownTarget(selector.to_string())),
        _ => Err(CollaborationError::AmbiguousTarget(selector.to_string())),
    })
}

/// Where an undelivered request should go now that its pinned recipient is
/// gone, if anywhere.
///
/// Follows the same session when it moved to another pane (a restore, a
/// relocation): that is the same recipient. Otherwise, for a slot-addressed
/// request only, the one live worker now holding the slot. Never when the
/// pinned session still has a live registry row (it may only be momentarily
/// out of the pane scan), and never to a choice between several.
pub fn successor_for(
    request: &CollaborationRequest,
    participants: &[Participant],
    agents: &[Agent],
    panes: &[PaneInfo],
) -> Option<Participant> {
    let pinned = &request.to;
    if pinned.console
        || is_pending_session(&pinned.agent_session_id)
        || participants.iter().any(|p| p.same_endpoint(pinned))
    {
        return None;
    }
    let moved: Vec<_> = participants
        .iter()
        .filter(|p| !p.console && p.same_session(pinned))
        .collect();
    if let [moved] = moved.as_slice() {
        return Some((*moved).clone());
    }
    if !moved.is_empty()
        || agents.iter().any(|agent| {
            agent.session_id == pinned.agent_session_id && agent.state != AgentState::Stopped
        })
    {
        return None;
    }
    let slot = request.to_slot.as_ref()?;
    let holders: Vec<_> = participants
        .iter()
        .filter(|p| {
            !p.same_endpoint(&request.from) && p.accepts_peer_work() && slot.held_by(p, panes)
        })
        .collect();
    match holders.as_slice() {
        [successor] => Some((*successor).clone()),
        _ => None,
    }
}

/// Exclusive keyset cursor for newest-first collaboration history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollaborationCursor {
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub id: String,
}

/// Opt-in transport views. Full execution contracts remain the default.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationReadView {
    #[default]
    Full,
    Metadata,
    Preview,
    Outcome,
    Progress {
        after_update: u64,
    },
}

impl CollaborationReadView {
    pub fn apply(self, request: &mut CollaborationRequest) {
        match self {
            Self::Full => {}
            Self::Metadata => request.omit_list_bodies(),
            Self::Preview => {
                let body = request.body.chars().take(256).collect();
                let reply = request
                    .reply
                    .as_ref()
                    .map(|reply| reply.body.chars().take(256).collect::<String>());
                request.omit_list_bodies();
                request.body = body;
                if let (Some(reply), Some(body)) = (&mut request.reply, reply) {
                    reply.body = body;
                }
            }
            Self::Outcome => {
                let reply = request.reply.take();
                request.omit_list_bodies();
                request.reply = reply;
            }
            Self::Progress { after_update } => {
                let updates = std::mem::take(&mut request.updates)
                    .into_iter()
                    .map(|mut update| {
                        if update.sequence <= after_update {
                            update.body.clear();
                        }
                        update
                    })
                    .collect();
                request.omit_list_bodies();
                request.updates = updates;
            }
        }
    }
}

impl CollaborationRequest {
    /// A display-only projection. Exact get/claim/reply paths retain their full contract.
    pub fn omit_list_bodies(&mut self) {
        self.body.clear();
        self.paths.clear();
        self.artifacts.clear();
        self.links.clear();
        self.air_artifacts.clear();
        if let Some(reply) = &mut self.reply {
            reply.body.clear();
            reply.artifacts.clear();
            reply.air_artifacts.clear();
        }
        if let Some(mut last) = self.updates.pop() {
            last.body.clear();
            self.updates.clear();
            self.updates.push(last);
        }
    }
}

/// Indexed collaboration-history filters. `since` is inclusive; `cursor` is
/// exclusive. Room matching is exact across host, socket and tmux window id.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollaborationQuery {
    /// Optional stored-payload budget per page, with at least one whole record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
    /// CLI calendar ranges use an exclusive upper bound; existing queries remain inclusive.
    #[serde(default)]
    pub until_exclusive: bool,
    /// Either endpoint's window id, name, or session:window label (case insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub until: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default)]
    pub search_bodies: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub since: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RequestKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RequestStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<RoomId>,
    /// Exact tmux session aggregate. Agent-originated requests are anchored
    /// to `from`; console-originated requests are anchored to their target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CollaborationCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollaborationPage {
    pub requests: Vec<CollaborationRequest>,
    /// Number of rows matching the filters and mailbox scope before applying
    /// the cursor. This lets overview surfaces size history without offsets.
    pub total: usize,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<CollaborationCursor>,
}

#[derive(Debug, thiserror::Error)]
pub enum CollaborationError {
    #[error("invalid dispatch metadata: {0}")]
    InvalidDispatch(String),
    #[error("dispatch {0} already exists with a different recipient or payload")]
    DispatchConflict(String),
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
    #[error("parent collaboration request not found: {0}")]
    ParentNotFound(String),
    #[error("parent collaboration request {0} does not involve this participant pair and room")]
    InvalidParentScope(String),
    #[error("thread {thread_id:?} conflicts with parent {parent_request_id}'s thread {parent_thread_id:?}")]
    ThreadMismatch {
        thread_id: String,
        parent_request_id: String,
        parent_thread_id: String,
    },
    #[error("request {0} does not belong to the calling participant")]
    NotParticipant(String),
    #[error(
        "listing past your own mailbox is an operator-console operation; this origin speaks for a pane agent"
    )]
    ScopeDenied,
    #[error("reply status must be completed, blocked, declined, or failed")]
    InvalidReplyStatus,
    #[error("request {0} is already terminal")]
    AlreadyTerminal(String),
    #[error("request {0} has already been claimed and can no longer be cancelled")]
    AlreadyClaimed(String),
    #[error(
        "request {0} is a human decision or peer-recovery request; its reply wake cannot be withdrawn"
    )]
    ReplyWakeNotWithdrawable(String),
    #[error("a request may withdraw at most {0} reply wakes")]
    TooManyWithdrawals(usize),
    #[error("invalid reply-wake withdrawal: {0}")]
    InvalidWithdrawal(String),
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
    #[error("collaboration database error: {0}")]
    Database(#[from] rusqlite::Error),
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

fn has_database_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "sqlite" | "sqlite3" | "db"
            )
        })
}

fn collaboration_database_path(legacy_path: &Path) -> PathBuf {
    if has_database_extension(legacy_path) {
        legacy_path.to_path_buf()
    } else {
        legacy_path.with_extension("sqlite3")
    }
}

fn is_sqlite_header(bytes: &[u8]) -> bool {
    bytes.starts_with(b"SQLite format 3\0")
}

#[cfg(unix)]
fn secure_database_files(path: &Path) -> Result<(), CollaborationError> {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        let candidate = PathBuf::from(candidate);
        if candidate.exists() {
            std::fs::set_permissions(candidate, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // same signature as the Unix arm
fn secure_database_files(_path: &Path) -> Result<(), CollaborationError> {
    Ok(())
}

/// Create the main database with owner-only permissions before `SQLite` opens
/// it. Chmod existing files as well: relying on the process umask would leave
/// a short first-open window where collaboration bodies could be world-readable.
#[cfg(unix)]
fn prepare_database_file(path: &Path) -> Result<(), CollaborationError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_mode(0o600);
    file.set_permissions(permissions)?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // same signature as the Unix arm
fn prepare_database_file(_path: &Path) -> Result<(), CollaborationError> {
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep additive mailbox schema initialization together.
fn open_database(path: &Path) -> Result<Connection, CollaborationError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    prepare_database_file(path)?;
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > COLLABORATION_DATABASE_SCHEMA_VERSION {
        return Err(CollaborationError::UnsupportedSchema(version));
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS collaboration_requests (
             id TEXT PRIMARY KEY,
             created_at_ns INTEGER NOT NULL,
             thread_id TEXT,
             parent_request_id TEXT,
             workspace_id TEXT,
             work_id TEXT,
             run_id TEXT,
             kind TEXT NOT NULL,
             status TEXT NOT NULL,
             from_room_host TEXT NOT NULL,
             from_room_socket TEXT,
             from_room_window_id TEXT NOT NULL,
             to_room_host TEXT NOT NULL,
             to_room_socket TEXT,
             to_room_window_id TEXT NOT NULL,
             from_pane TEXT NOT NULL,
             from_socket TEXT,
             from_agent_session_id TEXT NOT NULL,
             to_pane TEXT NOT NULL,
             to_socket TEXT,
             to_agent_session_id TEXT NOT NULL,
             anchor_tmux_session_id TEXT,
             anchor_tmux_session_name TEXT,
             payload TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS collab_created_idx
             ON collaboration_requests(created_at_ns DESC, id DESC);
         CREATE INDEX IF NOT EXISTS collab_work_idx
             ON collaboration_requests(
                 workspace_id, work_id, created_at_ns DESC, id DESC
             );
         CREATE INDEX IF NOT EXISTS collab_work_id_idx
             ON collaboration_requests(work_id, created_at_ns DESC, id DESC);
         CREATE INDEX IF NOT EXISTS collab_thread_idx
             ON collaboration_requests(thread_id, created_at_ns DESC, id DESC);
         CREATE INDEX IF NOT EXISTS collab_parent_idx
             ON collaboration_requests(parent_request_id, created_at_ns DESC, id DESC);
         CREATE INDEX IF NOT EXISTS collab_kind_idx
             ON collaboration_requests(kind, created_at_ns DESC, id DESC);
         CREATE INDEX IF NOT EXISTS collab_status_idx
             ON collaboration_requests(status, created_at_ns DESC, id DESC);
         CREATE INDEX IF NOT EXISTS collab_from_room_idx
             ON collaboration_requests(
                 from_room_host, from_room_socket, from_room_window_id,
                 created_at_ns DESC, id DESC
             );
         CREATE INDEX IF NOT EXISTS collab_to_room_idx
             ON collaboration_requests(
                 to_room_host, to_room_socket, to_room_window_id,
                 created_at_ns DESC, id DESC
             );
         CREATE INDEX IF NOT EXISTS collab_from_session_idx ON collaboration_requests(from_agent_session_id, from_socket, from_room_host, created_at_ns DESC, id DESC);
         CREATE INDEX IF NOT EXISTS collab_to_session_idx ON collaboration_requests(to_agent_session_id, to_socket, to_room_host, created_at_ns DESC, id DESC);
         CREATE INDEX IF NOT EXISTS collab_from_endpoint_idx
             ON collaboration_requests(
                 from_pane, from_socket, from_agent_session_id,
                 created_at_ns DESC, id DESC
             );
         CREATE INDEX IF NOT EXISTS collab_to_endpoint_idx
             ON collaboration_requests(
                 to_pane, to_socket, to_agent_session_id,
                 created_at_ns DESC, id DESC
             );
         CREATE INDEX IF NOT EXISTS collab_tmux_session_idx
             ON collaboration_requests(
                 anchor_tmux_session_id, created_at_ns DESC, id DESC
             );
         CREATE INDEX IF NOT EXISTS collab_tmux_session_name_idx
             ON collaboration_requests(
                 anchor_tmux_session_name, created_at_ns DESC, id DESC
             );
         CREATE TABLE IF NOT EXISTS collaboration_identities (
             identity_key TEXT PRIMARY KEY,
             payload TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS collaboration_purposes (id TEXT PRIMARY KEY, payload TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS collaboration_metadata (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );",
    )?;
    // Every persist opens the database. Writing an unchanged user_version
    // would still be a committed, fsync'd write transaction each time.
    if version != COLLABORATION_DATABASE_SCHEMA_VERSION {
        connection.pragma_update(None, "user_version", COLLABORATION_DATABASE_SCHEMA_VERSION)?;
    }
    secure_database_files(path)?;
    Ok(connection)
}

fn timestamp_nanos(value: OffsetDateTime) -> Result<i64, CollaborationError> {
    i64::try_from(value.unix_timestamp_nanos()).map_err(|_| {
        CollaborationError::Persistence(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "collaboration timestamp is outside SQLite's indexed range",
        ))
    })
}

fn request_kind_name(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Question => "question",
        RequestKind::Review => "review",
        RequestKind::Task => "task",
        RequestKind::Notice => "notice",
    }
}

fn request_status_name(status: RequestStatus) -> &'static str {
    match status {
        RequestStatus::Queued => "queued",
        RequestStatus::Claimed => "claimed",
        RequestStatus::Completed => "completed",
        RequestStatus::Blocked => "blocked",
        RequestStatus::Declined => "declined",
        RequestStatus::Failed => "failed",
        RequestStatus::Expired => "expired",
        RequestStatus::Cancelled => "cancelled",
    }
}

const UPSERT_REQUEST_SQL: &str = "INSERT INTO collaboration_requests (
         id, created_at_ns, thread_id, parent_request_id, workspace_id, work_id, run_id,
         kind, status,
         from_room_host, from_room_socket, from_room_window_id,
         to_room_host, to_room_socket, to_room_window_id,
         from_pane, from_socket, from_agent_session_id,
         to_pane, to_socket, to_agent_session_id,
         anchor_tmux_session_id, anchor_tmux_session_name, payload
     ) VALUES (
         ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
         ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
     ) ON CONFLICT(id) DO UPDATE SET
         created_at_ns=excluded.created_at_ns,
         thread_id=excluded.thread_id,
         parent_request_id=excluded.parent_request_id,
         workspace_id=excluded.workspace_id,
         work_id=excluded.work_id,
         run_id=excluded.run_id,
         kind=excluded.kind,
         status=excluded.status,
         from_room_host=excluded.from_room_host,
         from_room_socket=excluded.from_room_socket,
         from_room_window_id=excluded.from_room_window_id,
         to_room_host=excluded.to_room_host,
         to_room_socket=excluded.to_room_socket,
         to_room_window_id=excluded.to_room_window_id,
         from_pane=excluded.from_pane,
         from_socket=excluded.from_socket,
         from_agent_session_id=excluded.from_agent_session_id,
         to_pane=excluded.to_pane,
         to_socket=excluded.to_socket,
         to_agent_session_id=excluded.to_agent_session_id,
         anchor_tmux_session_id=excluded.anchor_tmux_session_id,
         anchor_tmux_session_name=excluded.anchor_tmux_session_name,
         payload=excluded.payload";

const IMPORT_REQUEST_SQL: &str = "INSERT OR IGNORE INTO collaboration_requests (
         id, created_at_ns, thread_id, parent_request_id, workspace_id, work_id, run_id,
         kind, status,
         from_room_host, from_room_socket, from_room_window_id,
         to_room_host, to_room_socket, to_room_window_id,
         from_pane, from_socket, from_agent_session_id,
         to_pane, to_socket, to_agent_session_id,
         anchor_tmux_session_id, anchor_tmux_session_name, payload
     ) VALUES (
         ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
         ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
     )";

fn write_request(
    connection: &Connection,
    request: &CollaborationRequest,
    import_only: bool,
) -> Result<(), CollaborationError> {
    let payload = serde_json::to_string(request)?;
    let anchor = if request.from.console {
        &request.to
    } else {
        &request.from
    };
    connection.execute(
        if import_only {
            IMPORT_REQUEST_SQL
        } else {
            UPSERT_REQUEST_SQL
        },
        params![
            request.id,
            timestamp_nanos(request.created_at)?,
            request.thread_id,
            request.parent_request_id,
            request.workspace_id,
            request.work_id,
            request.run_id,
            request_kind_name(request.kind),
            request_status_name(request.status),
            request.from.room.host,
            request.from.room.socket,
            request.from.room.window_id,
            request.to.room.host,
            request.to.room.socket,
            request.to.room.window_id,
            request.from.pane,
            request.from.socket,
            request.from.agent_session_id,
            request.to.pane,
            request.to.socket,
            request.to.agent_session_id,
            anchor.tmux_session_id,
            anchor.tmux_session_name,
            payload,
        ],
    )?;
    Ok(())
}

fn identity_key(identity: &CollaborationIdentity) -> Result<String, CollaborationError> {
    Ok(serde_json::to_string(&(
        &identity.room,
        &identity.pane,
        &identity.socket,
        &identity.agent_session_id,
    ))?)
}

fn load_database(
    path: &Path,
    legacy: Option<&Snapshot>,
) -> Result<
    (
        HashMap<String, CollaborationRequest>,
        Vec<CollaborationIdentity>,
    ),
    CollaborationError,
> {
    let mut connection = open_database(path)?;
    let migrated = connection
        .query_row(
            "SELECT value FROM collaboration_metadata WHERE key = 'legacy_json_imported'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if !migrated {
        let transaction = connection.transaction()?;
        if let Some(snapshot) = legacy {
            for request in &snapshot.requests {
                let mut request = request.clone();
                request.thread_id.get_or_insert_with(|| request.id.clone());
                write_request(&transaction, &request, true)?;
            }
            for identity in &snapshot.identities {
                transaction.execute(
                    "INSERT OR IGNORE INTO collaboration_identities (identity_key, payload)
                     VALUES (?1, ?2)",
                    params![identity_key(identity)?, serde_json::to_string(identity)?],
                )?;
            }
        }
        transaction.execute(
            "INSERT OR REPLACE INTO collaboration_metadata (key, value)
             VALUES ('legacy_json_imported', '1')",
            [],
        )?;
        transaction.commit()?;
        secure_database_files(path)?;
    }

    let mut requests = HashMap::new();
    let mut backfilled = Vec::new();
    {
        let mut statement = connection.prepare("SELECT payload FROM collaboration_requests")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            let mut request: CollaborationRequest = serde_json::from_str(&row?)?;
            if request.thread_id.is_none() {
                request.thread_id = Some(request.id.clone());
                backfilled.push(request.clone());
            }
            requests.insert(request.id.clone(), request);
        }
    }
    if !backfilled.is_empty() {
        let transaction = connection.transaction()?;
        for request in &backfilled {
            write_request(&transaction, request, false)?;
        }
        transaction.commit()?;
        secure_database_files(path)?;
    }
    let mut identities = Vec::new();
    {
        let mut statement = connection.prepare("SELECT payload FROM collaboration_identities")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            identities.push(serde_json::from_str(&row?)?);
        }
    }
    Ok((requests, identities))
}

fn nullable_text(value: Option<&String>) -> Value {
    value.map_or(Value::Null, |value| Value::Text(value.clone()))
}

fn push_room_values(values: &mut Vec<Value>, room: &RoomId) {
    values.push(Value::Text(room.host.clone()));
    values.push(nullable_text(room.socket.as_ref()));
    values.push(Value::Text(room.window_id.clone()));
}

fn push_endpoint_values(values: &mut Vec<Value>, participant: &Participant) {
    values.push(Value::Text(participant.pane.clone()));
    values.push(nullable_text(participant.socket.as_ref()));
    values.push(Value::Text(participant.agent_session_id.clone()));
}

fn session_caller_clause(prefix: &str, caller: &Participant, values: &mut Vec<Value>) -> String {
    if caller.console
        || caller.agent_session_id.is_empty()
        || is_pending_session(&caller.agent_session_id)
    {
        push_endpoint_values(values, caller);
        return format!(
            "({prefix}_pane = ? AND {prefix}_socket IS ? AND {prefix}_agent_session_id = ?)"
        );
    }
    values.push(nullable_text(caller.socket.as_ref()));
    values.push(Value::Text(caller.agent_session_id.clone()));
    values.push(Value::Text(caller.room.host.clone()));
    format!("({prefix}_socket IS ? AND {prefix}_agent_session_id = ? AND {prefix}_room_host = ?)")
}

fn incoming_caller_clause(caller: &Participant, values: &mut Vec<Value>) -> String {
    let mut alternatives = vec![session_caller_clause("to", caller, values)];
    if !caller.console && !is_pending_session(&caller.agent_session_id) {
        alternatives.push(
            "(to_agent_session_id LIKE 'pending-pane:%' AND to_pane = ? AND to_socket IS ?
              AND to_room_host = ? AND to_room_socket IS ? AND to_room_window_id = ?)"
                .to_string(),
        );
        values.push(Value::Text(caller.pane.clone()));
        values.push(nullable_text(caller.socket.as_ref()));
        push_room_values(values, &caller.room);
    }
    format!("({})", alternatives.join(" OR "))
}

fn request_access_clause(
    caller: &Participant,
    mailbox: RequestMailbox,
    scope: MailboxScope,
    values: &mut Vec<Value>,
) -> Option<String> {
    match scope {
        MailboxScope::All => None,
        MailboxScope::Room => {
            let incoming = "(to_room_host = ? AND to_room_socket IS ? AND to_room_window_id = ?)";
            let sent = "(from_room_host = ? AND from_room_socket IS ? AND from_room_window_id = ?)";
            match mailbox {
                RequestMailbox::Incoming => {
                    push_room_values(values, &caller.room);
                    Some(incoming.to_string())
                }
                RequestMailbox::Sent => {
                    push_room_values(values, &caller.room);
                    Some(sent.to_string())
                }
                RequestMailbox::All => {
                    push_room_values(values, &caller.room);
                    push_room_values(values, &caller.room);
                    Some(format!("({incoming} OR {sent})"))
                }
            }
        }
        MailboxScope::Caller => match mailbox {
            RequestMailbox::Incoming => Some(incoming_caller_clause(caller, values)),
            RequestMailbox::Sent => Some(session_caller_clause("from", caller, values)),
            RequestMailbox::All => {
                let incoming = incoming_caller_clause(caller, values);
                let sent = session_caller_clause("from", caller, values);
                Some(format!("({incoming} OR {sent})"))
            }
        },
    }
}

fn append_session_query_filters(
    connection: &Connection,
    query: &CollaborationQuery,
    clauses: &mut Vec<String>,
    values: &mut Vec<Value>,
) -> Result<(), CollaborationError> {
    if let Some(name) = &query.tmux_session_name {
        // Include retained records made through a numeric client view, while
        // keeping the filter before pagination/counting and index-backed.
        let mut statement = connection.prepare(
            "SELECT DISTINCT anchor_tmux_session_name FROM collaboration_requests WHERE anchor_tmux_session_name IS NOT NULL",
        )?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|candidate| {
                crate::tmux::base_session_name(candidate) == crate::tmux::base_session_name(name)
            })
            .collect::<Vec<_>>();
        if names.is_empty() {
            clauses.push("0".to_string());
        } else {
            clauses.push(format!(
                "anchor_tmux_session_name IN ({})",
                vec!["?"; names.len()].join(",")
            ));
            values.extend(names.into_iter().map(Value::Text));
        }
    }
    if let Some(socket) = &query.tmux_socket {
        clauses.push("(CASE WHEN json_extract(payload, '$.from.console') THEN COALESCE(to_socket, to_room_socket) ELSE COALESCE(from_socket, from_room_socket) END) = ?".to_string());
        values.push(Value::Text(socket.clone()));
    }
    Ok(())
}

fn collaboration_search_paths() -> &'static [&'static str] {
    &[
        "$.id",
        "$.kind",
        "$.status",
        "$.workspace_id",
        "$.work_id",
        "$.thread_id",
        "$.parent_request_id",
        "$.from.alias",
        "$.from.roles",
        "$.from.agent_kind",
        "$.from.pane",
        "$.from.agent_session_id",
        "$.from.tmux_session_name",
        "$.from.window_name",
        "$.to.alias",
        "$.to.roles",
        "$.to.agent_kind",
        "$.to.pane",
        "$.to.agent_session_id",
        "$.to.tmux_session_name",
        "$.to.window_name",
    ]
}

fn collaboration_search_text(request: &CollaborationRequest, bodies: bool) -> String {
    let value = serde_json::to_value(request).expect("collaboration request serializes");
    let mut fields = collaboration_search_paths()
        .iter()
        .filter_map(|path| {
            value.pointer(&format!(
                "/{}",
                path.trim_start_matches("$.").replace('.', "/")
            ))
        })
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string)
        })
        .collect::<Vec<_>>();
    if bodies {
        fields.push(request.body.clone());
        if let Some(reply) = &request.reply {
            fields.push(reply.body.clone());
        }
        fields.extend(request.updates.iter().map(|update| update.body.clone()));
    }
    fields.join(" ")
}

fn append_search_query_filters(
    query: &CollaborationQuery,
    clauses: &mut Vec<String>,
    values: &mut Vec<Value>,
) -> Result<(), CollaborationError> {
    for (operator, instant) in [
        (">=", query.since),
        (if query.until_exclusive { "<" } else { "<=" }, query.until),
    ] {
        if let Some(instant) = instant {
            clauses.push(format!("created_at_ns {operator} ?"));
            values.push(Value::Integer(timestamp_nanos(instant)?));
        }
    }
    if let Some(search) = &query.search {
        let mut fields = collaboration_search_paths()
            .iter()
            .map(|path| format!("COALESCE(CAST(json_extract(payload, '{path}') AS TEXT), '')"))
            .collect::<Vec<_>>();
        if query.search_bodies {
            fields.extend(
                ["$.body", "$.reply.body"]
                    .iter()
                    .map(|path| format!("COALESCE(json_extract(payload, '{path}'), '')")),
            );
            fields.push("COALESCE((SELECT group_concat(json_extract(value, '$.body'), ' ') FROM json_each(payload, '$.updates')), '')".to_string());
        }
        clauses.push(format!(
            "instr(lower({}), lower(?)) > 0",
            fields.join(" || ' ' || ")
        ));
        values.push(Value::Text(search.clone()));
    }
    Ok(())
}

fn query_database(
    path: &Path,
    caller: &Participant,
    mailbox: RequestMailbox,
    scope: MailboxScope,
    query: &CollaborationQuery,
    offset: usize,
) -> Result<CollaborationPage, CollaborationError> {
    let connection = open_database(path)?;
    query_connection(
        &connection,
        caller,
        mailbox,
        scope,
        query,
        offset,
        MAX_QUERY_LIMIT,
    )
}

/// Read the persisted mailbox without creating, migrating, pruning or mutating
/// it. Used by the standalone CLI viewer, including with an older daemon whose
/// unbounded IPC list can exceed the transport's message size limit.
pub fn read_history_page(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<CollaborationPage, CollaborationError> {
    let database = collaboration_database_path(path);
    if !database.exists() {
        return Ok(CollaborationPage {
            requests: Vec::new(),
            total: 0,
            has_more: false,
            next_cursor: None,
        });
    }
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let caller = Participant::console(RoomId {
        host: CONSOLE_PANE.into(),
        socket: None,
        window_id: CONSOLE_PANE.into(),
    });
    query_connection(
        &connection,
        &caller,
        RequestMailbox::All,
        MailboxScope::All,
        &CollaborationQuery {
            limit: Some(limit),
            ..Default::default()
        },
        offset,
        5000,
    )
}

fn append_window_query_filter(
    query: &CollaborationQuery,
    clauses: &mut Vec<String>,
    values: &mut Vec<Value>,
) {
    if let Some(window) = &query.window {
        let endpoints = ["from", "to"].into_iter().map(|side| format!(
            "(lower(json_extract(payload, '$.{side}.room.window_id')) = lower(?) OR lower(json_extract(payload, '$.{side}.window_name')) = lower(?) OR lower(CASE WHEN json_extract(payload, '$.{side}.console') THEN 'console' ELSE COALESCE(json_extract(payload, '$.{side}.tmux_session_name'), json_extract(payload, '$.{side}.tmux_session_id'), '?') || ':' || COALESCE(json_extract(payload, '$.{side}.window_name'), json_extract(payload, '$.{side}.room.window_id')) END) = lower(?))"
        )).collect::<Vec<_>>();
        clauses.push(format!("({})", endpoints.join(" OR ")));
        values.extend((0..6).map(|_| Value::Text(window.clone())));
    }
}

fn collect_query_rows(
    rows: impl Iterator<Item = rusqlite::Result<String>>,
    query: &CollaborationQuery,
    limit: usize,
) -> Result<(Vec<CollaborationRequest>, bool), CollaborationError> {
    let mut requests: Vec<CollaborationRequest> = Vec::new();
    let mut used = 0_usize;
    let mut byte_limited = false;
    for row in rows {
        let payload = row?;
        if !requests.is_empty()
            && query
                .byte_budget()
                .is_some_and(|budget| used.saturating_add(payload.len()) > budget)
        {
            byte_limited = true;
            break;
        }
        used = used.saturating_add(payload.len());
        requests.push(serde_json::from_str(&payload)?);
    }
    let has_more = byte_limited || requests.len() > limit;
    requests.truncate(limit);
    Ok((requests, has_more))
}

#[allow(clippy::too_many_arguments)]
fn query_connection(
    connection: &Connection,
    caller: &Participant,
    mailbox: RequestMailbox,
    scope: MailboxScope,
    query: &CollaborationQuery,
    offset: usize,
    max_limit: usize,
) -> Result<CollaborationPage, CollaborationError> {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    if let Some(clause) = request_access_clause(caller, mailbox, scope, &mut values) {
        clauses.push(clause);
    }
    append_search_query_filters(query, &mut clauses, &mut values)?;
    for (column, value) in [
        ("id", query.request_id.as_ref()),
        ("workspace_id", query.workspace_id.as_ref()),
        ("work_id", query.work_id.as_ref()),
        ("thread_id", query.thread_id.as_ref()),
        ("parent_request_id", query.parent_request_id.as_ref()),
        ("anchor_tmux_session_id", query.tmux_session_id.as_ref()),
    ] {
        if let Some(value) = value {
            clauses.push(format!("{column} = ?"));
            values.push(Value::Text(value.clone()));
        }
    }
    append_session_query_filters(connection, query, &mut clauses, &mut values)?;
    append_window_query_filter(query, &mut clauses, &mut values);
    if let Some(kind) = query.kind {
        clauses.push("kind = ?".to_string());
        values.push(Value::Text(request_kind_name(kind).to_string()));
    }
    if let Some(status) = query.status {
        clauses.push("status = ?".to_string());
        values.push(Value::Text(request_status_name(status).to_string()));
    }
    if let Some(room) = query.room.as_ref() {
        clauses.push(
            "((from_room_host = ? AND from_room_socket IS ? AND from_room_window_id = ?)
              OR (to_room_host = ? AND to_room_socket IS ? AND to_room_window_id = ?))"
                .to_string(),
        );
        push_room_values(&mut values, room);
        push_room_values(&mut values, room);
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let total: i64 = connection.query_row(
        &format!("SELECT COUNT(*) FROM collaboration_requests{where_sql}"),
        params_from_iter(values.iter()),
        |row| row.get(0),
    )?;

    let mut page_clauses = clauses;
    let mut page_values = values;
    if let Some(cursor) = query.cursor.as_ref() {
        let cursor_at = timestamp_nanos(cursor.created_at)?;
        page_clauses.push("(created_at_ns < ? OR (created_at_ns = ? AND id < ?))".to_string());
        page_values.push(Value::Integer(cursor_at));
        page_values.push(Value::Integer(cursor_at));
        page_values.push(Value::Text(cursor.id.clone()));
    }
    let page_where_sql = if page_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", page_clauses.join(" AND "))
    };
    let limit = query
        .limit
        .unwrap_or(DEFAULT_QUERY_LIMIT)
        .clamp(1, max_limit);
    page_values.push(Value::Integer(i64::try_from(limit + 1).unwrap_or(i64::MAX)));
    page_values.push(Value::Integer(i64::try_from(offset).unwrap_or(i64::MAX)));
    let mut statement = connection.prepare(&format!(
        "SELECT payload FROM collaboration_requests{page_where_sql}
         ORDER BY created_at_ns DESC, id DESC LIMIT ? OFFSET ?"
    ))?;
    let rows = statement.query_map(params_from_iter(page_values.iter()), |row| {
        row.get::<_, String>(0)
    })?;
    let (requests, has_more) = collect_query_rows(rows, query, limit)?;
    let next_cursor = if has_more {
        requests.last().map(|last| CollaborationCursor {
            created_at: last.created_at,
            id: last.id.clone(),
        })
    } else {
        None
    };
    Ok(CollaborationPage {
        requests,
        total: usize::try_from(total).unwrap_or(usize::MAX),
        has_more,
        next_cursor,
    })
}

fn request_visible(
    request: &CollaborationRequest,
    caller: &Participant,
    mailbox: RequestMailbox,
    scope: MailboxScope,
) -> bool {
    match scope {
        MailboxScope::Caller => match mailbox {
            RequestMailbox::Incoming => addresses(&request.to, caller),
            RequestMailbox::Sent => request.from.same_session(caller),
            RequestMailbox::All => {
                request.from.same_session(caller) || addresses(&request.to, caller)
            }
        },
        MailboxScope::Room => match mailbox {
            RequestMailbox::Incoming => request.to.room == caller.room,
            RequestMailbox::Sent => request.from.room == caller.room,
            RequestMailbox::All => {
                request.from.room == caller.room || request.to.room == caller.room
            }
        },
        MailboxScope::All => true,
    }
}

fn endpoint_matches_window(participant: &Participant, wanted: &str) -> bool {
    let label = if participant.console {
        "console".into()
    } else {
        format!(
            "{}:{}",
            participant
                .tmux_session_name
                .as_deref()
                .or(participant.tmux_session_id.as_deref())
                .unwrap_or("?"),
            participant
                .window_name
                .as_deref()
                .unwrap_or(&participant.room.window_id)
        )
    };
    participant
        .window_name
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
        || participant.room.window_id.eq_ignore_ascii_case(wanted)
        || label.eq_ignore_ascii_case(wanted)
}

impl CollaborationQuery {
    pub fn byte_budget(&self) -> Option<usize> {
        self.max_bytes
            .map(|bytes| bytes.clamp(64 * 1024, 4 * 1024 * 1024))
    }

    /// Compatibility selection when an older daemon returns the unbounded mailbox.
    pub fn page(&self, requests: &[CollaborationRequest], offset: usize) -> CollaborationPage {
        select_query_page(requests.iter(), self, offset)
    }
}

fn select_query_page<'a>(
    requests: impl Iterator<Item = &'a CollaborationRequest>,
    query: &CollaborationQuery,
    offset: usize,
) -> CollaborationPage {
    let mut matched = requests
        .filter(|request| request_matches_query(request, query))
        .collect::<Vec<_>>();
    matched.sort_by(|left, right| (right.created_at, &right.id).cmp(&(left.created_at, &left.id)));
    let total = matched.len();
    if let Some(cursor) = &query.cursor {
        matched
            .retain(|request| (request.created_at, &request.id) < (cursor.created_at, &cursor.id));
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_QUERY_LIMIT)
        .clamp(1, MAX_QUERY_LIMIT);
    let available = matched.len().saturating_sub(offset);
    let mut requests = Vec::new();
    let mut used = 0_usize;
    for request in matched.into_iter().skip(offset).take(limit) {
        if let Some(budget) = query.byte_budget() {
            let size = serde_json::to_vec(request).map_or(usize::MAX, |bytes| bytes.len());
            if !requests.is_empty() && used.saturating_add(size) > budget {
                break;
            }
            used = used.saturating_add(size);
        }
        requests.push(request.clone());
    }
    let has_more = available > requests.len();
    let next_cursor = has_more.then(|| {
        let last = requests.last().expect("nonempty page");
        CollaborationCursor {
            created_at: last.created_at,
            id: last.id.clone(),
        }
    });
    CollaborationPage {
        requests,
        total,
        has_more,
        next_cursor,
    }
}

fn request_matches_query(request: &CollaborationRequest, query: &CollaborationQuery) -> bool {
    let session_anchor = if request.from.console {
        &request.to
    } else {
        &request.from
    };
    query.since.is_none_or(|since| request.created_at >= since)
        && query.until.is_none_or(|until| {
            if query.until_exclusive {
                request.created_at < until
            } else {
                request.created_at <= until
            }
        })
        && query.window.as_ref().is_none_or(|window| {
            endpoint_matches_window(&request.from, window)
                || endpoint_matches_window(&request.to, window)
        })
        && query.request_id.as_ref().is_none_or(|id| request.id == *id)
        && query.search.as_ref().is_none_or(|search| {
            collaboration_search_text(request, query.search_bodies)
                .to_ascii_lowercase()
                .contains(&search.to_ascii_lowercase())
        })
        && query
            .workspace_id
            .as_ref()
            .is_none_or(|workspace_id| request.workspace_id.as_ref() == Some(workspace_id))
        && query
            .work_id
            .as_ref()
            .is_none_or(|work_id| request.work_id.as_ref() == Some(work_id))
        && query
            .thread_id
            .as_ref()
            .is_none_or(|thread_id| request.thread_id.as_ref() == Some(thread_id))
        && query
            .parent_request_id
            .as_ref()
            .is_none_or(|parent| request.parent_request_id.as_ref() == Some(parent))
        && query.kind.is_none_or(|kind| request.kind == kind)
        && query.status.is_none_or(|status| request.status == status)
        && query
            .room
            .as_ref()
            .is_none_or(|room| request.from.room == *room || request.to.room == *room)
        && query
            .tmux_session_id
            .as_ref()
            .is_none_or(|session_id| session_anchor.tmux_session_id.as_ref() == Some(session_id))
        && query.tmux_session_name.as_ref().is_none_or(|session_name| {
            session_anchor
                .tmux_session_name
                .as_deref()
                .is_some_and(|name| {
                    crate::tmux::base_session_name(name)
                        == crate::tmux::base_session_name(session_name)
                })
        })
        && query.tmux_socket.as_ref().is_none_or(|socket| {
            session_anchor
                .socket
                .as_ref()
                .or(session_anchor.room.socket.as_ref())
                == Some(socket)
        })
}

fn request_latest_activity(request: &CollaborationRequest) -> OffsetDateTime {
    [
        request.claimed_at,
        request.notified_at,
        request.reply_notified_at,
        request.reply_read_at,
        request.reply.as_ref().map(|reply| reply.at),
    ]
    .into_iter()
    .flatten()
    .fold(request.created_at, std::cmp::max)
}

/// In-memory mailbox projection backed by indexed `SQLite` row updates when a
/// durable path is configured. The projection keeps wake and routing reads
/// cheap while `transaction_lock` preserves atomic durable visibility.
pub struct CollaborationStore {
    opts: CollaborationOptions,
    /// Indexed `SQLite` sidecar. The configured JSON path remains the migration
    /// source and is deliberately retained as a recoverable backup.
    database_path: Option<PathBuf>,
    requests: RwLock<HashMap<String, CollaborationRequest>>,
    identities: RwLock<Vec<CollaborationIdentity>>,
    purposes: RwLock<Vec<PurposeGrant>>,
    /// Serializes each in-memory mutation with its durable row update. Wake
    /// scans also take this lock, so an unpersisted request is never visible
    /// to the delivery loop.
    transaction_lock: Mutex<()>,
    /// Source of [`ReplyWake::Reserved`] tokens. Reservations never outlive
    /// the store instance: [`Self::load`] turns persisted ones back into owed.
    reply_wake_tokens: AtomicU64,
    /// Handles issued but not yet visible in a pane scan.
    ///
    /// The daemon arbitrates the namespace but does not write pane options;
    /// the CLI that asked does, and the scan that would prove it lands a tick
    /// later. Without this, two agents starting in the same instant both ask,
    /// both see a free `claude`, and both are told to take it. A reservation
    /// closes that window from the arbiter's side, which is the only side
    /// that sees every request.
    reservations: RwLock<Vec<HandleReservation>>,
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
            database_path: None,
            requests: RwLock::new(HashMap::new()),
            identities: RwLock::new(Vec::new()),
            purposes: RwLock::new(Vec::new()),
            reservations: RwLock::new(Vec::new()),
            transaction_lock: Mutex::new(()),
            reply_wake_tokens: AtomicU64::new(1),
            changes,
        })
    }

    pub async fn load(options: CollaborationOptions) -> Result<Arc<Self>, CollaborationError> {
        let (legacy, configured_path_is_database) = if let Some(path) = options.path.as_ref() {
            if has_database_extension(path) {
                (None, true)
            } else {
                match tokio::fs::File::open(path).await {
                    Ok(mut file) => {
                        let mut header = [0_u8; 16];
                        let bytes_read = file.read(&mut header).await?;
                        if is_sqlite_header(&header[..bytes_read]) {
                            (None, true)
                        } else {
                            let bytes = tokio::fs::read(path).await?;
                            let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
                            if snapshot.version != COLLABORATION_SCHEMA_VERSION {
                                return Err(CollaborationError::UnsupportedSchema(
                                    snapshot.version,
                                ));
                            }
                            (Some(snapshot), false)
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, false),
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            (None, false)
        };
        let database_path = options.path.as_deref().map(|path| {
            if configured_path_is_database {
                path.to_path_buf()
            } else {
                collaboration_database_path(path)
            }
        });
        let (requests, identities) = if let Some(path) = database_path.as_ref() {
            load_database(path, legacy.as_ref())?
        } else {
            let snapshot = legacy.unwrap_or(Snapshot {
                version: COLLABORATION_SCHEMA_VERSION,
                requests: Vec::new(),
                identities: Vec::new(),
            });
            (
                snapshot
                    .requests
                    .into_iter()
                    .map(|request| (request.id.clone(), request))
                    .collect(),
                snapshot.identities,
            )
        };
        let mut requests = requests;
        for request in requests.values_mut() {
            request.normalize_loaded_reply_wake();
        }
        let (changes, _) = watch::channel(0);
        let purposes = load_purposes(database_path.as_deref())?;
        let store = Arc::new(Self {
            opts: options,
            database_path,
            requests: RwLock::new(requests),
            identities: RwLock::new(identities),
            purposes: RwLock::new(purposes),
            reservations: RwLock::new(Vec::new()),
            transaction_lock: Mutex::new(()),
            reply_wake_tokens: AtomicU64::new(1),
            changes,
        });
        if let Some(days) = store.opts.retention_days {
            let seconds = days.saturating_mul(24 * 60 * 60).min(i64::MAX as u64);
            let cutoff = OffsetDateTime::now_utc()
                - time::Duration::seconds(i64::try_from(seconds).unwrap_or(i64::MAX));
            store.prune_history(cutoff).await?;
        }
        Ok(store)
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

    /// Issue a room-local handle for `pane`, or refuse if the room already
    /// answers to the one being asked for.
    ///
    /// This is the single gate the three writers of a handle now share.
    /// Before it existed each enforced its own rule against its own view —
    /// the minting allocator saw tmux pane options, the launcher's explicit
    /// stamp saw nothing at all, and `set_identity` saw the identity store —
    /// so every ordering between them produced a different way for one room
    /// to answer to `@claude` twice. Only the daemon sees all three at once.
    ///
    /// What is unified is the moment of *issue*, not the lifetimes. A handle
    /// still lands on the pane option, where it outlives muxad and the agent
    /// restarting in place, and a registered identity still belongs to one
    /// agent session and dies with it.
    pub async fn issue_handle(
        &self,
        room: &RoomId,
        pane: &str,
        participants: &[Participant],
        request: HandleRequest,
    ) -> Result<Option<String>, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let now = OffsetDateTime::now_utc();
        let taken = self.taken_handles(room, pane, participants, now).await;
        let issued = match request {
            HandleRequest::Reserve { handle } => {
                let handle = normalize_alias(Some(handle))?
                    .ok_or_else(|| CollaborationError::InvalidAlias(String::new()))?;
                if taken.iter().any(|held| held.eq_ignore_ascii_case(&handle)) {
                    return Err(CollaborationError::AliasInUse(handle));
                }
                handle
            }
            HandleRequest::Mint { base } => {
                let Some(base) = normalize_alias(Some(base)).ok().flatten() else {
                    return Ok(None);
                };
                let Some(handle) = mint_from_family(&base, &taken) else {
                    return Ok(None);
                };
                handle
            }
        };
        let mut reservations = self.reservations.write().await;
        reservations.retain(|held| !(held.pane == pane && held.room == *room));
        reservations.push(HandleReservation {
            room: room.clone(),
            pane: pane.to_string(),
            handle: issued.clone(),
            at: now,
        });
        Ok(Some(issued))
    }

    /// Every handle this room currently answers to, other than `pane`'s own.
    ///
    /// Two sources, because a handle can be live without being visible in
    /// either alone: what participants carry (a pane option, or the identity
    /// that overrode it) and what has been promised but not yet written.
    async fn taken_handles(
        &self,
        room: &RoomId,
        pane: &str,
        participants: &[Participant],
        now: OffsetDateTime,
    ) -> Vec<String> {
        let mut taken: Vec<String> = participants
            .iter()
            .filter(|participant| participant.room == *room && participant.pane != pane)
            .filter_map(|participant| participant.alias.clone())
            .collect();
        let mut reservations = self.reservations.write().await;
        // Drop promises that expired, and those a scan has since confirmed —
        // the participant list already speaks for those, and keeping them
        // would hold a name a restarted agent could otherwise reuse.
        reservations.retain(|held| {
            now - held.at < HANDLE_RESERVATION_TTL
                && !participants.iter().any(|participant| {
                    participant.pane == held.pane
                        && participant
                            .alias
                            .as_deref()
                            .is_some_and(|alias| alias.eq_ignore_ascii_case(&held.handle))
                })
        });
        taken.extend(
            reservations
                .iter()
                .filter(|held| held.room == *room && held.pane != pane)
                .map(|held| held.handle.clone()),
        );
        taken
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
        // Same namespace, same gate: a name promised to a pane that has not
        // written it yet is taken, exactly as it is for a mint.
        let reserved = self
            .taken_handles(
                &caller.room,
                &caller.pane,
                live_participants,
                OffsetDateTime::now_utc(),
            )
            .await;
        let previous = self.identities.read().await.clone();
        {
            let mut identities = self.identities.write().await;
            if let Some(alias) = alias.as_deref() {
                // A name is taken if *anything* in the room answers to it,
                // which is two sources, not one.
                //
                // Identities registered through here are the obvious half.
                // The other half is what a participant was seeded with:
                // `@muxa_agent_alias`, the name a launcher stamped on the
                // pane or that muxa minted for it. Checking only the store
                // let an agent register a name a live peer already answers
                // to, leaving `@claude` ambiguous for both — a hole only
                // pipeline panes could fall into before, and one any room
                // can now that every pane carries a minted handle.
                //
                // Both, rather than the seeded alias alone, because a caller
                // is not required to hand us enriched participants; the
                // store is always current.
                let registered = identities.iter().any(|identity| {
                    identity.room == caller.room
                        && identity
                            .alias
                            .as_deref()
                            .is_some_and(|held| held.eq_ignore_ascii_case(alias))
                        && !identity.matches(caller)
                        && live_participants
                            .iter()
                            .any(|participant| identity.matches(participant))
                });
                let answered_to = reserved.iter().any(|held| held.eq_ignore_ascii_case(alias));
                if registered || answered_to {
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
        let current_identities = self.identities.read().await.clone();
        if let Err(error) = self.persist_identities(&current_identities) {
            *self.identities.write().await = previous;
            return Err(error);
        }
        // Reserve it too. `enrich_participants` will carry this alias into
        // the next scan, but a mint asking in between would not see it, which
        // is the identity-then-mint ordering that made `@codex` ambiguous.
        {
            let mut reservations = self.reservations.write().await;
            reservations.retain(|held| !(held.pane == caller.pane && held.room == caller.room));
            if let Some(alias) = alias.as_deref() {
                reservations.push(HandleReservation {
                    room: caller.room.clone(),
                    pane: caller.pane.clone(),
                    handle: alias.to_string(),
                    at: OffsetDateTime::now_utc(),
                });
            }
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

    #[allow(clippy::too_many_lines)] // request construction and persistence rollback form one transaction
    pub async fn create_with_provenance(
        &self,
        from: Participant,
        to: Participant,
        input: NewRequest,
        provenance: Option<CollaborationProvenance>,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        let body = input.body.trim().to_string();
        if input.human_action.is_some()
            && (!to.console || !input.expects_reply || input.kind == RequestKind::Notice)
        {
            return Err(CollaborationError::UnknownTarget(
                "human_action requires target human and a reply-bearing request".into(),
            ));
        }
        if body.is_empty() {
            return Err(CollaborationError::EmptyMessage);
        }
        if body.len() > self.opts.max_message_bytes {
            return Err(CollaborationError::MessageTooLarge(
                self.opts.max_message_bytes,
            ));
        }
        validate_air_artifact_references(&input.air_artifacts)?;
        reject_blank_withdrawals(&input.withdraw_reply_wakes)?;
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
        let now = OffsetDateTime::now_utc();
        let id = dispatch_request_id(&input, now)?;
        if let Some(existing) = self.requests.read().await.get(&id) {
            let same = dispatch_matches(existing, &from, &to, &input, &body);
            return if same {
                Ok(existing.clone())
            } else {
                Err(CollaborationError::DispatchConflict(id))
            };
        }
        let withdrawals = normalize_withdrawals(&input.withdraw_reply_wakes);
        self.validate_reply_wake_withdrawals(&from, &withdrawals)
            .await?;
        let supplied_thread_id = normalize_optional_id(input.thread_id);
        let parent_request_id = normalize_optional_id(input.parent_request_id);
        let thread_id = if let Some(parent_request_id) = parent_request_id.as_deref() {
            let requests = self.requests.read().await;
            let parent = requests
                .get(parent_request_id)
                .ok_or_else(|| CollaborationError::ParentNotFound(parent_request_id.to_string()))?;
            let human_escalation = to.console
                && input.human_action.is_some()
                && (addresses(&parent.from, &from) || addresses(&parent.to, &from));
            if !same_participant_pair(parent, &from, &to) && !human_escalation {
                return Err(CollaborationError::InvalidParentScope(
                    parent_request_id.to_string(),
                ));
            }
            let parent_thread_id = parent
                .thread_id
                .clone()
                .unwrap_or_else(|| parent.id.clone());
            if let Some(thread_id) = supplied_thread_id.as_deref() {
                if thread_id != parent_thread_id {
                    return Err(CollaborationError::ThreadMismatch {
                        thread_id: thread_id.to_string(),
                        parent_request_id: parent_request_id.to_string(),
                        parent_thread_id,
                    });
                }
            }
            Some(parent_thread_id)
        } else {
            Some(supplied_thread_id.unwrap_or_else(|| id.clone()))
        };
        let request = CollaborationRequest {
            dispatch_id: input.dispatch_id,
            source_node_id: input.source_node_id,
            delegation_parent: input.delegation_parent,
            interruption: None,
            human_action: input.human_action,
            id,
            from,
            to,
            initiator: input.initiator,
            updates: Vec::new(),
            provenance,
            kind: input.kind,
            body,
            expects_reply: input.expects_reply,
            notify: input.notify,
            work_mode: input.work_mode,
            thread_id,
            parent_request_id,
            workspace_id: normalize_optional_id(input.workspace_id),
            work_id: normalize_optional_id(input.work_id),
            run_id: normalize_optional_id(input.run_id),
            paths: input.paths,
            artifacts: input.artifacts,
            links: input.links,
            air_artifacts: input.air_artifacts,
            withdrew_reply_wakes: withdrawals,
            to_slot: input.recipient_slot,
            rebound_from: Vec::new(),
            status: RequestStatus::Queued,
            created_at: now,
            claimed_at: None,
            wake_delivery: None,
            notified_at: None,
            reply_notified_at: None,
            reply_read_at: None,
            reply_wake: None,
            reply: None,
        };
        let changed = {
            let mut requests = self.requests.write().await;
            let mut changed =
                withdraw_reply_wakes_in(&mut requests, &request.withdrew_reply_wakes, now);
            requests.insert(request.id.clone(), request.clone());
            changed.push(request.clone());
            changed
        };
        if let Err(error) = self.persist_requests(&changed) {
            *self.requests.write().await = previous;
            return Err(error);
        }
        self.publish_change();
        Ok(request)
    }

    /// Check that `caller` may withdraw the reply wake of every request in
    /// `ids`, before anything changes. All or nothing: one bad id rejects
    /// the whole operation.
    async fn validate_reply_wake_withdrawals(
        &self,
        caller: &Participant,
        ids: &[String],
    ) -> Result<(), CollaborationError> {
        if ids.len() > MAX_REPLY_WAKE_WITHDRAWALS {
            return Err(CollaborationError::TooManyWithdrawals(
                MAX_REPLY_WAKE_WITHDRAWALS,
            ));
        }
        let requests = self.requests.read().await;
        for id in ids {
            let request = requests
                .get(id)
                .ok_or_else(|| CollaborationError::NotFound(id.clone()))?;
            if caller.console || !request.from.same_session(caller) {
                return Err(CollaborationError::NotParticipant(id.clone()));
            }
            if !request.reply_wake_withdrawable() {
                return Err(CollaborationError::ReplyWakeNotWithdrawable(id.clone()));
            }
        }
        Ok(())
    }

    /// Stop waking `caller` for the replies to its own requests `ids`,
    /// whatever their status, including replies that have not arrived yet.
    ///
    /// This only withdraws notification interest. The recipient's work, the
    /// reply, its unread state and `wait` results are unchanged; a queued
    /// request still needs [`Self::cancel_for`] to stop the recipient. A wake
    /// already being typed into the pane cannot be recalled.
    pub async fn withdraw_reply_wakes(
        &self,
        caller: &Participant,
        ids: &[String],
    ) -> Result<Vec<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        reject_blank_withdrawals(ids)?;
        let ids = normalize_withdrawals(ids);
        if ids.is_empty() {
            return Err(CollaborationError::InvalidWithdrawal(
                "name at least one request id".into(),
            ));
        }
        let _transaction = self.transaction_lock.lock().await;
        self.validate_reply_wake_withdrawals(caller, &ids).await?;
        let previous = self.requests.read().await.clone();
        let (changed, result) = {
            let mut requests = self.requests.write().await;
            let changed = withdraw_reply_wakes_in(&mut requests, &ids, OffsetDateTime::now_utc());
            let result: Vec<_> = ids
                .iter()
                .filter_map(|id| requests.get(id).cloned())
                .collect();
            (changed, result)
        };
        if !changed.is_empty() {
            if let Err(error) = self.persist_requests(&changed) {
                *self.requests.write().await = previous;
                return Err(error);
            }
            self.publish_change();
        }
        Ok(result)
    }

    pub async fn claim_for(
        &self,
        caller: &Participant,
    ) -> Result<Vec<CollaborationRequest>, CollaborationError> {
        self.claim_selected(caller, None).await
    }

    pub async fn claim_by_id(
        &self,
        caller: &Participant,
        request_id: &str,
    ) -> Result<CollaborationRequest, CollaborationError> {
        if let Some(request) = self
            .claim_selected(caller, Some(request_id))
            .await?
            .into_iter()
            .next()
        {
            return Ok(request);
        }
        // Distinguish a missing/foreign id from an already finished request,
        // without acknowledging a reply just to explain acceptance failure.
        let request = self.peek_for(caller, request_id).await?;
        if !addresses(&request.to, caller) {
            return Err(CollaborationError::NotParticipant(request_id.to_string()));
        }
        Err(CollaborationError::AlreadyTerminal(request_id.to_string()))
    }

    async fn claim_selected(
        &self,
        caller: &Participant,
        selected: Option<&str>,
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
                if addresses(&request.to, caller)
                    && selected.is_none_or(|id| request.id == id)
                    && (selected.is_some()
                        || request.from.console
                        || caller.accepts_peer_work()
                        || request.status == RequestStatus::Claimed)
                    && matches!(
                        request.status,
                        RequestStatus::Queued | RequestStatus::Claimed
                    )
                {
                    // The session pulling this inbox is the one the pane's
                    // pending request was waiting for: adopt it, so every
                    // later match is session-pinned.
                    changed |= pin_pending_recipient(request, caller);
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
            if let Err(error) = self.persist_requests(&inbox) {
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
            if !addresses(&request.to, caller) {
                return Err(CollaborationError::NotParticipant(request_id.to_string()));
            }
            if (!request.from.console && !caller.accepts_peer_work())
                || request.status != RequestStatus::Queued
                || request.notified_at.is_some()
            {
                None
            } else {
                pin_pending_recipient(request, caller);
                request.status = RequestStatus::Claimed;
                request.claimed_at = Some(OffsetDateTime::now_utc());
                request.wake_delivery = Some(WakeDeliveryState::Prepared);
                Some(request.clone())
            }
        };
        if prepared.is_some() {
            if let Err(error) = self.persist_requests(prepared.as_slice()) {
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
            requests.get_mut(request_id).and_then(|request| {
                (request.wake_delivery == Some(WakeDeliveryState::Prepared)).then(|| {
                    request.wake_delivery = Some(WakeDeliveryState::PromptWritten);
                    request.clone()
                })
            })
        };
        if let Some(changed) = changed {
            // The text side effect cannot be rolled back. Retain the in-memory
            // phase even if persistence fails, matching notification markers.
            let result = self.persist_requests(std::slice::from_ref(&changed));
            self.publish_change();
            result?;
        }
        Ok(())
    }

    /// Append bounded guidance/progress without claiming or completing work.
    pub async fn update_request(
        &self,
        caller: &Participant,
        request_id: &str,
        body: String,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        let body = body.trim().to_string();
        if body.is_empty() {
            return Err(CollaborationError::EmptyMessage);
        }
        let limit = self.opts.max_message_bytes.min(8192);
        if body.len() > limit {
            return Err(CollaborationError::MessageTooLarge(limit));
        }
        let _transaction = self.transaction_lock.lock().await;
        let mut updated = self
            .requests
            .read()
            .await
            .get(request_id)
            .cloned()
            .ok_or_else(|| CollaborationError::NotFound(request_id.into()))?;
        if !addresses(&updated.from, caller) && !addresses(&updated.to, caller) {
            return Err(CollaborationError::NotParticipant(request_id.into()));
        }
        if updated.status.is_terminal() {
            return Err(CollaborationError::AlreadyTerminal(request_id.into()));
        }
        // Retried identical updates from the same participant are idempotent.
        if updated
            .updates
            .last()
            .is_some_and(|last| addresses(&last.author, caller) && last.body == body)
        {
            return Ok(updated);
        }
        let sequence = updated
            .updates
            .last()
            .map_or(1, |last| last.sequence.saturating_add(1));
        updated.updates.push(CollaborationUpdate {
            sequence,
            author: caller.clone(),
            body,
            at: OffsetDateTime::now_utc(),
        });
        while updated.updates.len() > 32
            || updated.updates.iter().map(|u| u.body.len()).sum::<usize>() > 32768
        {
            updated.updates.remove(0);
        }
        self.persist_requests(std::slice::from_ref(&updated))?;
        self.requests
            .write()
            .await
            .insert(request_id.into(), updated.clone());
        self.publish_change();
        Ok(updated)
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
        // The same guard `create` puts on a request body, for the same reason
        // and one more: a reply is a terminal write. An empty one — an
        // argument that did not survive its shell, a variable that expanded to
        // nothing — closes the request with no answer in it, and the real
        // answer can never be posted to that thread afterwards. The sender
        // sees `completed` and an empty body, which reads as a reviewer with
        // nothing to say rather than as a delivery that went missing.
        // A refusal without a reason is no more useful than an empty
        // completion, so this holds for every terminal status.
        let body = body.trim().to_string();
        if body.is_empty() {
            return Err(CollaborationError::EmptyMessage);
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
            if !addresses(&request.to, caller) {
                return Err(CollaborationError::NotParticipant(request_id.to_string()));
            }
            if request.status.is_terminal() {
                return Err(CollaborationError::AlreadyTerminal(request_id.to_string()));
            }
            pin_pending_recipient(request, caller);
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
        if let Err(error) = self.persist_requests(std::slice::from_ref(&updated)) {
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
        self.read_for(caller, request_id, true).await
    }

    /// Observe instructions/progress without acknowledging an unseen reply.
    pub async fn peek_for(
        &self,
        caller: &Participant,
        request_id: &str,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.read_for(caller, request_id, false).await
    }

    async fn read_for(
        &self,
        caller: &Participant,
        request_id: &str,
        mark_reply_read: bool,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let mut previous = None;
        let request = {
            let mut requests = self.requests.write().await;
            // Participants may await the daemon's linked human decision, but
            // this grants no authority to answer/cancel the service request.
            let recovery_observer = requests
                .get(request_id)
                .filter(|r| r.to.console && r.from.agent_session_id == "muxa:peer-recovery")
                .and_then(|r| r.interruption.as_ref())
                .filter(|i| i.action_request_id.as_deref() == Some(request_id))
                .and_then(|i| requests.get(&i.request_id))
                .is_some_and(|parent| {
                    parent
                        .interruption
                        .as_ref()
                        .is_some_and(|i| i.action_request_id.as_deref() == Some(request_id))
                        && (addresses(&parent.from, caller) || addresses(&parent.to, caller))
                });
            let request = requests
                .get_mut(request_id)
                .ok_or_else(|| CollaborationError::NotFound(request_id.to_string()))?;
            let is_sender = request.from.same_session(caller);
            if !caller.console
                && !is_sender
                && !addresses(&request.to, caller)
                && !recovery_observer
            {
                return Err(CollaborationError::NotParticipant(request_id.to_string()));
            }
            if mark_reply_read
                && is_sender
                && request.reply.is_some()
                && request.reply_read_at.is_none()
            {
                previous = Some(request.clone());
                request.mark_reply_read(OffsetDateTime::now_utc());
            }
            request.clone()
        };
        if let Some(previous) = previous {
            if let Err(error) = self.persist_requests(std::slice::from_ref(&request)) {
                self.requests
                    .write()
                    .await
                    .insert(request_id.to_string(), previous);
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
            if request.status.is_terminal() || request.peer_interrupted() {
                return Ok(request);
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return self.get_for(caller, request_id).await,
            }
        }
    }

    /// List one mailbox. `scope` widens the listing past the caller's own
    /// endpoint; see [`MailboxScope`].
    pub async fn list_for(
        &self,
        caller: &Participant,
        mailbox: RequestMailbox,
        scope: MailboxScope,
    ) -> Result<Vec<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        // Reading past your own mailbox is an operator act. An agent must not
        // be able to read what its room-mates said to each other just because
        // it shares their window.
        if !matches!(scope, MailboxScope::Caller) && !caller.console {
            return Err(CollaborationError::ScopeDenied);
        }
        let _transaction = self.transaction_lock.lock().await;
        let mut requests: Vec<_> = self
            .requests
            .read()
            .await
            .values()
            .filter(|request| match scope {
                MailboxScope::Caller => match mailbox {
                    RequestMailbox::Incoming => addresses(&request.to, caller),
                    RequestMailbox::Sent => request.from.same_session(caller),
                    RequestMailbox::All => {
                        request.from.same_session(caller) || addresses(&request.to, caller)
                    }
                },
                MailboxScope::Room => match mailbox {
                    RequestMailbox::Incoming => request.to.room == caller.room,
                    RequestMailbox::Sent => request.from.room == caller.room,
                    RequestMailbox::All => {
                        request.from.room == caller.room || request.to.room == caller.room
                    }
                },
                MailboxScope::All => true,
            })
            .cloned()
            .collect();
        requests.sort_by_key(|request| std::cmp::Reverse(request.created_at));
        Ok(requests)
    }

    /// Query indexed durable history without changing the legacy unbounded
    /// `list_for` contract used by older CLI, watch and MCP clients.
    pub async fn query_for(
        &self,
        caller: &Participant,
        mailbox: RequestMailbox,
        scope: MailboxScope,
        query: &CollaborationQuery,
    ) -> Result<CollaborationPage, CollaborationError> {
        self.query_for_offset(caller, mailbox, scope, query, 0)
            .await
    }

    pub async fn query_for_offset(
        &self,
        caller: &Participant,
        mailbox: RequestMailbox,
        scope: MailboxScope,
        query: &CollaborationQuery,
        offset: usize,
    ) -> Result<CollaborationPage, CollaborationError> {
        self.ensure_enabled()?;
        if !matches!(scope, MailboxScope::Caller) && !caller.console {
            return Err(CollaborationError::ScopeDenied);
        }
        let _transaction = self.transaction_lock.lock().await;
        if let Some(path) = self.database_path.as_ref() {
            return query_database(path, caller, mailbox, scope, query, offset);
        }

        let requests = self.requests.read().await;
        Ok(select_query_page(
            requests
                .values()
                .filter(|request| request_visible(request, caller, mailbox, scope)),
            query,
            offset,
        ))
    }

    /// Remove whole, fully delivered terminal threads whose newest activity
    /// predates `cutoff`. Parent chains are never split, and live/unread work
    /// is retained even when another row in the thread is old.
    pub async fn prune_history(&self, cutoff: OffsetDateTime) -> Result<usize, CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let request_ids = {
            let requests = self.requests.read().await;
            let mut threads: HashMap<String, Vec<&CollaborationRequest>> = HashMap::new();
            for request in requests.values() {
                let thread_id = request
                    .thread_id
                    .clone()
                    .unwrap_or_else(|| request.id.clone());
                threads.entry(thread_id).or_default().push(request);
            }
            threads
                .into_values()
                .filter(|thread| {
                    thread.iter().all(|request| {
                        request.dispatch_id.is_none()
                            && request.status.is_terminal()
                            && request.wake_delivery.is_none()
                            && request.reply.as_ref().is_none_or(|_| {
                                request.reply_read_at.is_some() || request.from.console
                            })
                    }) && thread
                        .iter()
                        .map(|request| request_latest_activity(request))
                        .max()
                        .is_some_and(|latest| latest < cutoff)
                })
                .flat_map(|thread| thread.into_iter().map(|request| request.id.clone()))
                .collect::<Vec<_>>()
        };
        if request_ids.is_empty() {
            return Ok(0);
        }
        self.delete_requests(&request_ids)?;
        let mut requests = self.requests.write().await;
        for request_id in &request_ids {
            requests.remove(request_id);
        }
        drop(requests);
        self.publish_change();
        Ok(request_ids.len())
    }

    /// Operator housekeeping: mark every request still `Queued` (never
    /// claimed) and created before `cutoff` as `Expired`, and return them.
    /// Covers what nobody else can close: a sender may cancel only its own
    /// request, and the sender of a request stuck for weeks is usually an
    /// agent session that no longer exists. `dry_run` reports without
    /// writing.
    pub async fn expire_queued_before(
        &self,
        cutoff: OffsetDateTime,
        dry_run: bool,
    ) -> Result<Vec<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
        let mut expired: Vec<CollaborationRequest> = {
            let mut requests = self.requests.write().await;
            requests
                .values_mut()
                .filter(|request| {
                    request.status == RequestStatus::Queued && request.created_at < cutoff
                })
                .map(|request| {
                    if !dry_run {
                        request.status = RequestStatus::Expired;
                    }
                    request.clone()
                })
                .collect()
        };
        expired.sort_by_key(|request| request.created_at);
        if dry_run || expired.is_empty() {
            return Ok(expired);
        }
        if let Err(error) = self.persist_requests(&expired) {
            *self.requests.write().await = previous;
            return Err(error);
        }
        self.publish_change();
        Ok(expired)
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
            if !request.from.same_session(caller) {
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
        if let Err(error) = self.persist_requests(std::slice::from_ref(&cancelled)) {
            *self.requests.write().await = previous;
            return Err(error);
        }
        self.publish_change();
        Ok(cancelled)
    }

    /// One-way notices still unread in `recipient`'s mailbox. They never wake
    /// anyone on their own, so a wake sent for something else is where the
    /// recipient learns they exist; otherwise they wait until it happens to
    /// read its inbox, however long that takes.
    pub async fn quiet_notices_waiting(&self, recipient: &Participant) -> usize {
        self.requests
            .read()
            .await
            .values()
            .filter(|request| {
                request.status == RequestStatus::Queued
                    && request.notified_at.is_none()
                    && !request.should_notify()
                    && addresses(&request.to, recipient)
            })
            .count()
    }

    pub async fn pending_unnotified(&self) -> Vec<CollaborationRequest> {
        let _transaction = self.transaction_lock.lock().await;
        let mut requests: Vec<_> = self
            .requests
            .read()
            .await
            .values()
            .filter(|request| {
                !request.to.console && request.notified_at.is_none()
                    // Finish an already reserved delivery even after an upgrade.
                    && (request.should_notify() || request.wake_delivery.is_some())
                    && (request.status == RequestStatus::Queued || request.wake_delivery.is_some())
            })
            .cloned()
            .collect();
        requests.sort_by_key(|request| request.created_at);
        requests
    }

    /// Human attention has its own delivery queue; never wake a pane for it.
    pub async fn pending_human_notifications(&self) -> Vec<CollaborationRequest> {
        let _transaction = self.transaction_lock.lock().await;
        self.requests
            .read()
            .await
            .values()
            .filter(|request| request.needs_human_response() && request.notified_at.is_none())
            .cloned()
            .collect()
    }

    /// Requests that have not reached their recipient in any way yet:
    /// queued, unclaimed, not notified and with no direct wake in progress.
    /// Includes quiet ones that never wake anybody, which still need to
    /// follow a recipient that moved or was replaced.
    pub async fn undelivered_requests(&self) -> Vec<CollaborationRequest> {
        let _transaction = self.transaction_lock.lock().await;
        self.requests
            .read()
            .await
            .values()
            .filter(|request| {
                request.status == RequestStatus::Queued
                    && request.claimed_at.is_none()
                    && request.notified_at.is_none()
                    && request.wake_delivery.is_none()
                    && !request.to.console
            })
            .cloned()
            .collect()
    }

    /// Readdress an undelivered request from the recipient it was pinned to
    /// (`from`) to `to`, as chosen by [`successor_for`].
    ///
    /// Only before any delivery evidence: still queued, never claimed,
    /// notified or part-way through a direct wake, and still pinned to
    /// `from` (a concurrent change wins). Returns the updated request, or
    /// `None` when it no longer qualifies. The sender, contract, ids and
    /// thread are untouched; the old session is kept in `rebound_from`.
    pub async fn rebind_recipient(
        &self,
        request_id: &str,
        from: &Participant,
        to: Participant,
    ) -> Result<Option<CollaborationRequest>, CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
        let rebound = {
            let mut requests = self.requests.write().await;
            let Some(request) = requests.get_mut(request_id) else {
                return Ok(None);
            };
            if request.status != RequestStatus::Queued
                || request.claimed_at.is_some()
                || request.notified_at.is_some()
                || request.wake_delivery.is_some()
                || !request.to.same_endpoint(from)
                || to.console
            {
                return Ok(None);
            }
            let old = std::mem::replace(&mut request.to, to);
            request.rebound_from.push(old.agent_session_id);
            request.clone()
        };
        if let Err(error) = self.persist_requests(std::slice::from_ref(&rebound)) {
            *self.requests.write().await = previous;
            return Err(error);
        }
        self.publish_change();
        Ok(Some(rebound))
    }

    /// Replies still owed a wake to their sender, oldest reply first.
    ///
    /// A scan result only says who might be owed; whether a wake is actually
    /// sent is decided by [`Self::reserve_reply_wakes`] at delivery time.
    pub async fn pending_reply_unnotified(&self) -> Vec<CollaborationRequest> {
        let _transaction = self.transaction_lock.lock().await;
        let mut requests: Vec<_> = self
            .requests
            .read()
            .await
            .values()
            .filter(|request| request.reply_wake_owed())
            .cloned()
            .collect();
        sort_by_reply_time(&mut requests);
        requests
    }

    /// Reserve the replies still owed to `sender` for one wake prompt.
    ///
    /// This is the delivery decision: a reply read or withdrawn since the
    /// caller's scan is no longer owed and is not offered. Returns `None` when
    /// nothing is owed or `sender` already has a batch in flight. At most
    /// [`REPLY_WAKE_BATCH_LIMIT`] replies are reserved; the rest stay owed.
    pub async fn reserve_reply_wakes(
        &self,
        sender: &Participant,
    ) -> Result<Option<ReplyWakeBatch>, CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let previous = self.requests.read().await.clone();
        let batch = {
            let mut requests = self.requests.write().await;
            let mine = |request: &CollaborationRequest| request.from.same_endpoint(sender);
            if requests.values().any(|request| {
                mine(request) && matches!(request.reply_wake, Some(ReplyWake::Reserved { .. }))
            }) {
                return Ok(None);
            }
            let mut owed: Vec<_> = requests
                .values()
                .filter(|request| mine(request) && request.reply_wake_owed())
                .map(|request| {
                    (
                        request.reply.as_ref().map(|reply| reply.at),
                        request.id.clone(),
                    )
                })
                .collect();
            if owed.is_empty() {
                return Ok(None);
            }
            owed.sort();
            let remaining = owed.len().saturating_sub(REPLY_WAKE_BATCH_LIMIT);
            let token = self.reply_wake_tokens.fetch_add(1, Ordering::Relaxed);
            let reserved: Vec<_> = owed
                .into_iter()
                .take(REPLY_WAKE_BATCH_LIMIT)
                .filter_map(|(_, id)| {
                    let request = requests.get_mut(&id)?;
                    request.reply_wake = Some(ReplyWake::Reserved { token });
                    Some(request.clone())
                })
                .collect();
            ReplyWakeBatch {
                token,
                requests: reserved,
                remaining,
            }
        };
        if let Err(error) = self.persist_requests(&batch.requests) {
            *self.requests.write().await = previous;
            return Err(error);
        }
        // Deliberately unpublished: a reservation is the waker's own
        // bookkeeping, and the waker rescans on every published change. If it
        // were published, a pane that keeps refusing input would be reserved,
        // released and retried in a tight loop instead of on the next idle
        // transition or periodic reconcile, as a failed wake always was.
        Ok(Some(batch))
    }

    /// Settle a reserved batch. `sent` means the prompt reached the pane:
    /// its members are released as delivered. Otherwise they are owed again.
    ///
    /// Only members still holding this batch's token change. One the sender
    /// read or withdrew meanwhile keeps that outcome, so a failed attempt
    /// cannot resurrect it and a successful one cannot overwrite it.
    pub async fn complete_reply_wakes(
        &self,
        batch: &ReplyWakeBatch,
        sent: bool,
    ) -> Result<(), CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let changed: Vec<_> = {
            let mut requests = self.requests.write().await;
            let now = OffsetDateTime::now_utc();
            batch
                .requests
                .iter()
                .filter_map(|member| {
                    let request = requests.get_mut(&member.id)?;
                    if request.reply_wake != Some(ReplyWake::Reserved { token: batch.token }) {
                        return None;
                    }
                    if sent {
                        request.reply_wake = Some(ReplyWake::Released {
                            reason: ReplyWakeRelease::Delivered,
                            at: now,
                        });
                        request.reply_notified_at.get_or_insert(now);
                    } else {
                        request.reply_wake = None;
                    }
                    Some(request.clone())
                })
                .collect()
        };
        if changed.is_empty() {
            return Ok(());
        }
        // Keep the settled state in memory even if persisting it fails. A
        // delivered prompt cannot be recalled, so this daemon must not type it
        // again; a failed one must stay retryable, and restoring the
        // reservation would strand it with no attempt left to settle it. The
        // row on disk still says reserved, which the next load turns into owed.
        let result = self.persist_requests(&changed);
        // Only a delivery is news to readers (`reply_notified_at`). Returning
        // a failed attempt to owed is unpublished for the reason given in
        // `reserve_reply_wakes`.
        if sent {
            self.publish_change();
        }
        result
    }

    pub async fn mark_notified(&self, request_id: &str) -> Result<(), CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let changed = {
            let mut requests = self.requests.write().await;
            requests.get_mut(request_id).and_then(|request| {
                if request.notified_at.is_none()
                    && (request.status == RequestStatus::Queued
                        || request.wake_delivery.is_some()
                        || request.needs_human_response())
                {
                    request.notified_at = Some(OffsetDateTime::now_utc());
                    request.wake_delivery = None;
                    if !request.expects_reply && request.status == RequestStatus::Claimed {
                        request.status = RequestStatus::Completed;
                    }
                    Some(request.clone())
                } else {
                    None
                }
            })
        };
        if let Some(changed) = changed {
            // The terminal injection already happened. Keep the in-memory
            // marker even if disk persistence fails so the live daemon does
            // not inject the same wake on the next revision/reconcile scan.
            let result = self.persist_requests(std::slice::from_ref(&changed));
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
                addresses(&request.to, participant)
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
                request.from.same_session(participant)
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

    /// Upsert only requests changed by the current transaction. History
    /// growth therefore does not turn every state transition into a rewrite
    /// of every prior message.
    fn persist_requests(
        &self,
        requests: &[CollaborationRequest],
    ) -> Result<(), CollaborationError> {
        let Some(path) = self.database_path.as_ref() else {
            return Ok(());
        };
        let mut connection = open_database(path)?;
        let transaction = connection.transaction()?;
        for request in requests {
            write_request(&transaction, request, false)?;
        }
        transaction.commit()?;
        secure_database_files(path)?;
        Ok(())
    }

    /// Identity rows are tiny and mutable (including deletion), so replace
    /// their independent table atomically without touching request history.
    fn persist_identities(
        &self,
        identities: &[CollaborationIdentity],
    ) -> Result<(), CollaborationError> {
        let Some(path) = self.database_path.as_ref() else {
            return Ok(());
        };
        let mut connection = open_database(path)?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM collaboration_identities", [])?;
        for identity in identities {
            transaction.execute(
                "INSERT INTO collaboration_identities (identity_key, payload) VALUES (?1, ?2)",
                params![identity_key(identity)?, serde_json::to_string(identity)?],
            )?;
        }
        transaction.commit()?;
        secure_database_files(path)?;
        Ok(())
    }

    fn delete_requests(&self, request_ids: &[String]) -> Result<(), CollaborationError> {
        let Some(path) = self.database_path.as_ref() else {
            return Ok(());
        };
        let mut connection = open_database(path)?;
        let transaction = connection.transaction()?;
        {
            let mut statement =
                transaction.prepare("DELETE FROM collaboration_requests WHERE id = ?1")?;
            for request_id in request_ids {
                statement.execute([request_id])?;
            }
        }
        transaction.commit()?;
        secure_database_files(path)?;
        Ok(())
    }
}

/// Correlate live agent rows with pane topology, keeping the newest live
/// agent when adapter/synthetic rows briefly overlap on the same pane.
/// Muxa-launched CLIs without a real hook become pending peers when process
/// discovery confirms their provider. Wrapper discovery reads the process
/// table, so async callers must run this on a blocking worker.
/// Whether `participant` is still the idle agent session that owns its pane,
/// judged from the live agent rows for that pane. The waker resolves
/// participants from a scan and then awaits a reservation; by the time it
/// would type, the agent may have started work or been replaced by a new
/// session in the same pane. Mirrors [`participants_from`]: the most recently
/// active tracked agent on the pane (and socket, when both are known) wins.
pub fn still_idle_owner(pane_agents: &[Agent], participant: &Participant) -> bool {
    pane_agents
        .iter()
        .filter(|agent| {
            agent.pane.as_deref() == Some(participant.pane.as_str())
                && agent.state != AgentState::Stopped
                && agent.kind != AgentKind::Task
                && !agent
                    .session_id
                    .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
                && match (agent.tmux_socket.as_deref(), participant.socket.as_deref()) {
                    (Some(endpoint), Some(socket)) => crate::backend::pane_endpoints_match(
                        Some(&participant.pane),
                        socket,
                        &crate::backend::pane_endpoint_identity(Some(&participant.pane), endpoint),
                    ),
                    _ => true,
                }
        })
        .max_by_key(|agent| agent.last_activity_at)
        .is_some_and(|agent| {
            agent.session_id == participant.agent_session_id && agent.state == AgentState::Idle
        })
}

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
        let Some(pane) = same_pane_seen_twice(&candidates) else {
            continue;
        };
        let socket = pane.socket.clone().or(agent_socket);
        let participant = Participant {
            launch_purpose: LaunchPurpose::Primary,
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
    // A launcher-marked process is a peer before its first hook. Keep its
    // recipient pane-scoped until a real session can adopt queued requests.
    // Process evidence is required: launch marks can outlive a CLI in a pane.
    let launched = panes
        .iter()
        .filter(|pane| {
            pane.agent_program.is_some()
                && !participants
                    .iter()
                    .any(|peer| peer.pane == pane.pane_id && peer.socket == pane.socket)
        })
        .cloned()
        .collect::<Vec<_>>();
    for discovered in crate::discovery::discover_from_panes(&launched) {
        let pane = &discovered.pane;
        if pane
            .agent_program
            .as_deref()
            .and_then(crate::discovery::classify_command)
            != Some(discovered.kind)
            || participants
                .iter()
                .any(|peer| peer.pane == pane.pane_id && peer.socket == pane.socket)
        {
            continue;
        }
        let row = live_pane_row(&pane.pane_id, pane.socket.as_deref(), agents);
        let mut pending = pane_participant(pane, pane.socket.clone(), row);
        pending.agent_kind = discovered.kind;
        participants.push(pending);
    }
    participants.sort_by(|left, right| left.pane.cmp(&right.pane));
    participants
}

/// The durable room identity of a pane: `(host, socket, window_id)`, never the
/// mutable window name or index.
/// Public view of [`pane_room`], for callers that need a room for a pane the
/// participant table does not cover yet.
pub fn room_of_pane(pane_id: &str, pane: &PaneInfo, socket: Option<String>) -> RoomId {
    pane_room(pane_id, pane, socket)
}

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
    let matches: Vec<_> = panes
        .iter()
        .filter(|pane| {
            pane.pane_id == origin.pane
                && match origin.socket.as_deref() {
                    Some(socket) => pane.socket.as_deref().is_some_and(|candidate| {
                        crate::backend::pane_endpoints_match(Some(&origin.pane), candidate, socket)
                    }),
                    None => true,
                }
        })
        .collect();
    if matches.is_empty() {
        return Ok(Participant::console(RoomId {
            host: CONSOLE_PANE.to_string(),
            socket: None,
            window_id: CONSOLE_PANE.to_string(),
        }));
    }
    let Some(pane) = same_pane_seen_twice(&matches) else {
        return Err(CollaborationError::AmbiguousOrigin(origin.pane.clone()));
    };
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
    if let Some(session) = origin.agent_session_id.as_deref() {
        if let Some(participant) = participants
            .iter()
            .find(|participant| participant.agent_session_id == session)
        {
            return Ok(participant.clone());
        }
        if origin.pane.is_empty() {
            return Err(CollaborationError::UnknownOrigin(format!(
                "Codex thread {session} is not bound to a pane yet (its hooks run in the \
                 shared Codex app-server; muxa binds it from a `codex resume <id>` process \
                 or the one Codex pane at its cwd — use a distinct cwd or `codex --no-daemon`)"
            )));
        }
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
    // Host scope widens *explicit pane* targets only, including namespaced
    // backend ids. Reject ambiguous pane ids across servers. `peer`, `@alias`
    // and `role:` remain room concepts: aliases are unique only in one room.
    if selector == "human" {
        return Ok(Participant::console(sender.room.clone()));
    }
    if scope == crate::config::CollaborationScope::Host {
        let pane = selector.strip_prefix("pane:").unwrap_or(selector);
        if crate::backend::pane_id_host_kind(pane).is_some() {
            let matches = in_sender_namespace_first(pane, sender, |pane| {
                participants
                    .iter()
                    .filter(|candidate| candidate.pane == pane && !candidate.same_endpoint(sender))
                    .collect()
            });
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
        let workers: Vec<_> = peers
            .iter()
            .copied()
            .filter(|peer| peer.accepts_peer_work())
            .collect();
        return match workers.as_slice() {
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
                    && candidate.accepts_peer_work()
            })
            .collect();
        return select_target(matches, selector);
    }
    let pane = selector.strip_prefix("pane:").unwrap_or(selector);
    let matches = in_sender_namespace_first(pane, sender, |pane| {
        peers
            .iter()
            .copied()
            .filter(|candidate| {
                candidate.pane == pane
                    || candidate.label() == selector
                    || format!("{}@{}", candidate.agent_kind, candidate.pane) == selector
            })
            .collect()
    });
    select_target(matches, selector)
}

/// Match `pane` read in the sender's namespace first, then as typed: a bare
/// `%N` from an rmux pane usually means a peer on that same server, but a
/// tmux pane has no prefix to name it by, so it must stay reachable.
fn in_sender_namespace_first<'p>(
    pane: &str,
    sender: &Participant,
    find: impl Fn(&str) -> Vec<&'p Participant>,
) -> Vec<&'p Participant> {
    let namespaced = in_sender_namespace(pane, sender);
    let matches = find(&namespaced);
    if matches.is_empty() && namespaced != pane {
        find(pane)
    } else {
        matches
    }
}

/// A bare `%N` read in the sender's pane namespace. rmux shows and exports
/// `%N` while muxa stores its panes as `rmux:%N`, so a target typed the way
/// rmux displays it would otherwise never match a peer on the same server.
fn in_sender_namespace<'a>(pane: &'a str, sender: &Participant) -> std::borrow::Cow<'a, str> {
    match sender.pane.find('%') {
        Some(at) if at > 0 && pane.starts_with('%') => {
            std::borrow::Cow::Owned(format!("{}{pane}", &sender.pane[..at]))
        }
        _ => std::borrow::Cow::Borrowed(pane),
    }
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

/// Resolve an explicit `pane:%N` target whose agent has not registered yet.
///
/// Launcher-backed pending peers are normally served by [`resolve_target`].
/// This narrower fallback covers explicitly addressed panes with role/alias
/// marks or discovery evidence that do not yet appear in the peer topology.
///
/// Deliberately narrow. It requires all of:
/// * an **explicit pane selector** — automatic routing uses the peer topology;
/// * a pane muxa itself marked as an agent launch (`@muxa_agent_role` /
///   `@muxa_agent_alias`) or that discovery already classified as an agent
///   CLI, so a request can never be typed at a human's shell;
/// * no live participant on that pane, so a registered agent always wins; and
/// * the sender's own room, unless the deployment widened scope to the host.
pub fn resolve_pending_pane_target(
    sender: &Participant,
    selector: &str,
    participants: &[Participant],
    agents: &[Agent],
    panes: &[PaneInfo],
    scope: crate::config::CollaborationScope,
) -> Result<Participant, CollaborationError> {
    let pane_id = selector.strip_prefix("pane:").unwrap_or(selector);
    if !pane_id.starts_with('%') && crate::backend::pane_id_host_kind(pane_id).is_none() {
        return Err(CollaborationError::UnknownTarget(selector.to_string()));
    }
    let pane_id = in_sender_namespace(pane_id, sender);
    let pane_id = pane_id.as_ref();
    if pane_id == sender.pane {
        return Err(CollaborationError::UnknownTarget(selector.to_string()));
    }
    // A registered agent on the pane is always the better recipient, and
    // `resolve_target` already had its chance at it.
    if participants
        .iter()
        .any(|participant| participant.pane == pane_id)
    {
        return Err(CollaborationError::UnknownTarget(selector.to_string()));
    }
    let pane = unique_pane(pane_id, sender.socket.as_deref(), panes)
        .ok_or_else(|| CollaborationError::UnknownTarget(selector.to_string()))?;
    let socket = pane.socket.clone().or_else(|| sender.socket.clone());
    let row = live_pane_row(pane_id, socket.as_deref(), agents);
    if !muxa_launched_agent_pane(pane, row) {
        return Err(CollaborationError::UnknownTarget(selector.to_string()));
    }
    let pending = pane_participant(pane, socket, row);
    if scope != crate::config::CollaborationScope::Host && pending.room != sender.room {
        return Err(CollaborationError::UnknownTarget(selector.to_string()));
    }
    Ok(pending)
}

/// The recipient to deliver a *pending* request to, or `None` while the pane
/// is not ready for input.
///
/// Readiness is the pane's current row, whatever produced it: a hook, or the
/// discovery/screen placeholder that stands in before the agent registers. A
/// discovered agent pane reads `Idle`, which includes the seconds a TUI spends
/// booting — that is deliberate and safe: keys sent then sit in the pty until
/// the TUI reads them (verified against codex mid-`Starting MCP servers`,
/// where the queued prompt arrived verbatim).
///
/// What must hold delivery is a pane asking a question, and that is exactly
/// what the bundled screen manifests classify: codex's startup trust gate
/// lands the row on `WaitingInput`/`WaitingChoice`, never `Idle`, so a queued
/// request cannot answer a policy prompt by accident. A vanished pane yields
/// `None` too, so work queued for a dead pane stays queued instead of being
/// typed at whatever replaced it.
#[must_use]
pub fn pending_recipient_ready(
    to: &Participant,
    agents: &[Agent],
    panes: &[PaneInfo],
) -> Option<Participant> {
    if !is_pending_session(&to.agent_session_id) {
        return None;
    }
    let pane = unique_pane(&to.pane, to.socket.as_deref(), panes)?;
    let row = live_pane_row(&to.pane, to.socket.as_deref(), agents)?;
    if row.state != AgentState::Idle {
        return None;
    }
    // Re-apply the send-time guard at delivery time. `@muxa_agent_role` is a
    // tmux *pane* option that outlives the process it was stamped for, so a
    // kept-alive pane stays addressable; without this re-check an unrelated
    // `Task`/`Unknown` row idling there would be enough to get a request body
    // typed into it.
    if !muxa_launched_agent_pane(pane, Some(row)) {
        return None;
    }
    let socket = pane.socket.clone().or_else(|| to.socket.clone());
    let ready = pane_participant(pane, socket, Some(row));
    // The identity must stay byte-identical to `request.to` — delivery
    // reserves the request through the session-pinned `prepare_direct_wake` —
    // and the pane must still live in the room the request was addressed to.
    (ready.same_endpoint(to) && ready.room == to.room).then_some(ready)
}

/// The single pane a candidate list describes, or `None` when the candidates
/// are genuinely different panes.
///
/// tmux lists a pane once per session that shows it, and a session *group*
/// shows one window through several sessions. muxa's own `tmux-auto-view`
/// creates exactly that: every attached client gets a `<session>~view~<pid>`
/// member of the group, so an ordinary two-terminal setup lists every pane
/// twice. Those rows differ only in the session they were seen through — same
/// server, same window, same pane — and treating the second one as ambiguity
/// made every pane in such a session invisible to collaboration: no
/// participants, no origin, no peers, and an error telling the operator to
/// restart an agent that was working fine.
///
/// The ambiguity that matters is a pane id repeated across *servers*, which
/// differs by socket. That still refuses.
///
/// The base session wins over a view for display: it is the durable name, and
/// the one the operator recognises.
fn same_pane_seen_twice<'a>(candidates: &[&'a PaneInfo]) -> Option<&'a PaneInfo> {
    let first = *candidates.first()?;
    if !candidates
        .iter()
        .all(|pane| pane.socket == first.socket && pane.window_id == first.window_id)
    {
        return None;
    }
    Some(
        candidates
            .iter()
            .copied()
            .find(|pane| !pane.session.contains("~view~"))
            .unwrap_or(first),
    )
}

/// The one pane with this id, disambiguated by control endpoint the same way
/// [`participants_from`] does — pane ids repeat across tmux servers.
fn unique_pane<'a>(
    pane_id: &str,
    socket: Option<&str>,
    panes: &'a [PaneInfo],
) -> Option<&'a PaneInfo> {
    let candidates: Vec<_> = panes
        .iter()
        .filter(|pane| pane.pane_id == pane_id)
        .collect();
    if let Some(pane) = same_pane_seen_twice(&candidates) {
        return Some(pane);
    }
    // Several real panes share the id, so the control endpoint decides.
    let socket = socket?;
    let matching: Vec<_> = candidates
        .into_iter()
        .filter(|pane| {
            pane.socket.as_deref().is_some_and(|candidate| {
                crate::backend::pane_endpoints_match(Some(pane_id), candidate, socket)
            })
        })
        .collect();
    same_pane_seen_twice(&matching)
}

/// The live agent row occupying a pane, real or synthetic. Discovery and
/// screen detection share one synthetic key per pane, so at most one row is
/// returned for a pane that has never registered a session.
fn live_pane_row<'a>(
    pane_id: &str,
    socket: Option<&str>,
    agents: &'a [Agent],
) -> Option<&'a Agent> {
    agents
        .iter()
        .filter(|agent| {
            agent.pane.as_deref() == Some(pane_id)
                && agent.state != AgentState::Stopped
                && socket.is_none_or(|socket| {
                    agent.tmux_socket.as_deref().is_none_or(|candidate| {
                        crate::backend::pane_endpoints_match(Some(pane_id), candidate, socket)
                    })
                })
        })
        // A real row wins a tie with a synthetic placeholder for the same pane.
        .min_by_key(|agent| {
            u8::from(
                agent
                    .session_id
                    .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX),
            )
        })
}

/// Evidence that an agent CLI — not a human's shell — occupies this pane:
/// muxa's own launch marks, or a discovery row that classified the process.
fn muxa_launched_agent_pane(pane: &PaneInfo, row: Option<&Agent>) -> bool {
    pane.agent_role.is_some()
        || pane.agent_alias.is_some()
        || row.is_some_and(|row| row.kind != AgentKind::Task && row.kind != AgentKind::Unknown)
}

/// Build the pane-scoped participant a pending recipient is addressed as.
fn pane_participant(pane: &PaneInfo, socket: Option<String>, row: Option<&Agent>) -> Participant {
    Participant {
        launch_purpose: LaunchPurpose::Primary,
        agent_kind: row.map_or_else(
            || {
                pane.agent_program
                    .as_deref()
                    .and_then(crate::discovery::classify_command)
                    .unwrap_or(AgentKind::Unknown)
            },
            |row| row.kind,
        ),
        agent_session_id: pending_session_id(&pane.pane_id, socket.as_deref()),
        pane: pane.pane_id.clone(),
        socket: socket.clone(),
        room: pane_room(&pane.pane_id, pane, socket),
        tmux_session_id: (!pane.session_id.is_empty()).then(|| pane.session_id.clone()),
        tmux_session_name: Some(pane.session.clone()),
        window_name: (!pane.window_name.is_empty()).then(|| pane.window_name.clone()),
        // `Starting` is the honest state for a pane whose agent has not
        // reported anything yet; the waker refuses to type into it until a row
        // says `Idle`.
        state: row.map_or(AgentState::Starting, |row| row.state),
        cwd: (!pane.current_path.is_empty()).then(|| pane.current_path.clone()),
        alias: pane.agent_alias.clone(),
        roles: pane.agent_role.clone().into_iter().collect(),
        console: false,
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

/// `claude`, then `claude2`, `claude3`… — the first name in the family the
/// room is not already using.
///
/// The first agent of a runtime gets the bare name: it is what people type,
/// and what the MCP instructions already tell agents to route by. Numbering
/// only on collision keeps the common case memorable.
fn mint_from_family(base: &str, taken: &[String]) -> Option<String> {
    (1..=MINT_FAMILY_LIMIT).find_map(|n| {
        let candidate = if n == 1 {
            base.to_string()
        } else {
            format!("{base}{n}")
        };
        taken
            .iter()
            .all(|held| !held.eq_ignore_ascii_case(&candidate))
            .then_some(candidate)
    })
}

/// A room with this many agents of one runtime is not a room; this bounds the
/// walk rather than expressing a policy.
const MINT_FAMILY_LIMIT: usize = 64;

fn normalize_optional_id(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
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
    #[tokio::test]
    async fn overview_is_body_free_room_scoped_and_does_not_consume_messages() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let mut outside = participant("%3", "outside");
        outside.room.window_id = "@other".into();
        let participants = vec![sender.clone(), recipient.clone(), outside.clone()];
        let console = Participant::console(sender.room.clone());
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "SECRET REQUEST BODY".into(),
                    expects_reply: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        mailbox
            .create(
                outside.clone(),
                outside,
                NewRequest {
                    body: "OTHER ROOM BODY".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            mailbox.overview_for(&sender, &participants).await,
            Err(CollaborationError::ScopeDenied)
        ));
        let overview = mailbox.overview_for(&console, &participants).await.unwrap();
        assert_eq!(overview.participants.len(), 2);
        assert_eq!(overview.links.len(), 1);
        assert_eq!(overview.links[0].status, RequestStatus::Queued);
        assert!(!serde_json::to_string(&overview).unwrap().contains("BODY"));
        assert_eq!(mailbox.claim_for(&recipient).await.unwrap().len(), 1);
        mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Blocked,
                "SECRET REPLY BODY".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        let overview = mailbox.overview_for(&console, &participants).await.unwrap();
        assert!(overview.links[0].reply_received);
        assert_eq!(overview.links[0].status, RequestStatus::Blocked);
        assert!(!serde_json::to_string(&overview).unwrap().contains("BODY"));
        assert_eq!(mailbox.unread_reply_count(&sender).await, 1);
        assert_eq!(mailbox.pending_reply_unnotified().await.len(), 1);
    }

    #[tokio::test]
    async fn overview_caps_links_and_keeps_active_work_before_terminal_history() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let active = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "active".into(),
                    expects_reply: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        for _ in 0..70 {
            let notice = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        body: "notice".into(),
                        kind: RequestKind::Notice,
                        expects_reply: false,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            mailbox.claim_for(&recipient).await.unwrap();
            // Notices become terminal when delivered, independent of a reply.
            assert!(mailbox
                .peek_for(&sender, &notice.id)
                .await
                .unwrap()
                .status
                .is_terminal());
        }
        let overview = mailbox
            .overview_for(
                &Participant::console(sender.room.clone()),
                &[sender, recipient],
            )
            .await
            .unwrap();
        assert_eq!(overview.links.len(), 64);
        assert_eq!(overview.links[0].request_id, active.id);
        assert!(!overview.links[0].status.is_terminal());
    }

    async fn create_window_query_history(
        mailbox: &CollaborationStore,
        sender: &Participant,
        recipient: &Participant,
    ) -> Vec<CollaborationRequest> {
        let mut created = Vec::new();
        for _ in 0..3 {
            created.push(
                mailbox
                    .create(
                        sender.clone(),
                        recipient.clone(),
                        NewRequest {
                            body: "PRIVATE".into(),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap(),
            );
        }
        created
    }

    #[tokio::test]
    async fn byte_bounded_pages_keep_complete_records_and_cursors_in_memory_and_sqlite() {
        for durable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = if durable {
                CollaborationStore::load(CollaborationOptions {
                    path: Some(dir.path().join("budget.sqlite3")),
                    ..Default::default()
                })
                .await
                .unwrap()
            } else {
                CollaborationStore::in_memory(CollaborationOptions::default())
            };
            let sender = participant("%1", "sender");
            let recipient = participant("%2", "recipient");
            for _ in 0..20 {
                store
                    .create(
                        sender.clone(),
                        recipient.clone(),
                        NewRequest {
                            body: "x".repeat(8192),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
            }
            let mut query = CollaborationQuery {
                limit: Some(500),
                max_bytes: Some(64 * 1024),
                ..Default::default()
            };
            let mut ids = std::collections::HashSet::new();
            loop {
                let page = store
                    .query_for(&sender, RequestMailbox::All, MailboxScope::Caller, &query)
                    .await
                    .unwrap();
                assert_eq!(page.total, 20);
                assert!(!page.requests.is_empty());
                assert!(page.requests.len() < 20);
                let bytes: usize = page
                    .requests
                    .iter()
                    .map(|request| serde_json::to_vec(request).unwrap().len())
                    .sum();
                assert!(bytes <= 64 * 1024);
                for request in page.requests {
                    assert_eq!(request.body.len(), 8192);
                    assert!(ids.insert(request.id), "cursor must not duplicate a record");
                }
                if !page.has_more {
                    break;
                }
                assert!(page.next_cursor.is_some());
                query.cursor = page.next_cursor;
            }
            assert_eq!(ids.len(), 20, "byte limits must not skip history");
        }
    }

    #[tokio::test]
    async fn list_window_and_time_filters_precede_offset_in_memory_and_sqlite() {
        for durable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mailbox = if durable {
                CollaborationStore::load(CollaborationOptions {
                    path: Some(dir.path().join("pages.sqlite3")),
                    ..Default::default()
                })
                .await
                .unwrap()
            } else {
                CollaborationStore::in_memory(CollaborationOptions::default())
            };
            let mut sender = participant("%1", "sender");
            sender.tmux_session_name = Some("workspace".into());
            sender.window_name = Some("review".into());
            let mut recipient = participant("%2", "recipient");
            recipient.window_name = Some("implement".into());
            let created = create_window_query_history(&mailbox, &sender, &recipient).await;
            let query = CollaborationQuery {
                window: Some("WORKSPACE:REVIEW".into()),
                limit: Some(1),
                ..Default::default()
            };
            let page = mailbox
                .query_for_offset(
                    &sender,
                    RequestMailbox::Sent,
                    MailboxScope::Caller,
                    &query,
                    1,
                )
                .await
                .unwrap();
            assert_eq!(page.total, 3);
            assert_eq!(page.requests[0].id, created[1].id);
            assert!(page.has_more);
            mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        body: "new arrival".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let next = mailbox
                .query_for(
                    &sender,
                    RequestMailbox::Sent,
                    MailboxScope::Caller,
                    &CollaborationQuery {
                        cursor: page.next_cursor,
                        ..query.clone()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                next.requests[0].id, created[0].id,
                "new arrivals must not duplicate the previous page"
            );
            let end = created[2].created_at;
            let query = CollaborationQuery {
                window: Some("IMPLEMENT".into()),
                until: Some(end),
                until_exclusive: true,
                ..Default::default()
            };
            let page = mailbox
                .query_for(&sender, RequestMailbox::All, MailboxScope::Caller, &query)
                .await
                .unwrap();
            assert_eq!(page.total, 2, "upper time bound is exclusive");
            let beyond = mailbox
                .query_for_offset(
                    &sender,
                    RequestMailbox::All,
                    MailboxScope::Caller,
                    &query,
                    usize::MAX,
                )
                .await
                .unwrap();
            assert_eq!(beyond.total, 2);
            assert!(beyond.requests.is_empty());
            assert!(!beyond.has_more);
            assert!(matches!(
                mailbox
                    .query_for(&sender, RequestMailbox::All, MailboxScope::All, &query)
                    .await,
                Err(CollaborationError::ScopeDenied)
            ));
        }
    }

    #[tokio::test]
    async fn history_search_filters_before_pagination_in_memory_and_sqlite() {
        for durable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mailbox = if durable {
                CollaborationStore::load(CollaborationOptions {
                    path: Some(dir.path().join("history.sqlite3")),
                    ..Default::default()
                })
                .await
                .unwrap()
            } else {
                CollaborationStore::in_memory(CollaborationOptions::default())
            };
            let sender = participant("%123456", "sender");
            let recipient = participant("%2", "recipient");
            let root = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        body: "hidden needle %_ 한글".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            mailbox
                .update_request(&sender, &root.id, "progress needle".into())
                .await
                .unwrap();
            mailbox
                .reply(
                    &recipient,
                    &root.id,
                    RequestStatus::Completed,
                    "reply needle".into(),
                    vec![],
                    vec![],
                )
                .await
                .unwrap();
            mailbox
                .create(
                    sender,
                    recipient,
                    NewRequest {
                        body: "newer unrelated".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let console = Participant::console(root.from.room.clone());
            for text in ["hidden needle %_ 한글", "progress needle", "reply needle"] {
                let query = CollaborationQuery {
                    search: Some(text.into()),
                    search_bodies: true,
                    limit: Some(1),
                    ..Default::default()
                };
                let page = mailbox
                    .query_for(&console, RequestMailbox::All, MailboxScope::All, &query)
                    .await
                    .unwrap();
                assert_eq!(page.total, 1);
                assert_eq!(page.requests[0].id, root.id);
                let public = mailbox
                    .query_for(
                        &console,
                        RequestMailbox::All,
                        MailboxScope::All,
                        &CollaborationQuery {
                            search_bodies: false,
                            ..query
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(public.total, 0);
            }
            let page = mailbox
                .query_for(
                    &console,
                    RequestMailbox::All,
                    MailboxScope::All,
                    &CollaborationQuery {
                        request_id: Some(root.id.clone()),
                        until: Some(root.created_at),
                        search: Some("%123456".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(page.total, 1);
        }
    }

    use super::*;
    use crate::config::CollaborationScope;

    #[tokio::test]
    async fn dispatch_retries_survive_concurrency_restart_and_reject_changed_payload() {
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("mailbox.json")),
            ..Default::default()
        };
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        let from = participant("%1", "sender");
        let to = participant("%2", "worker");
        let input = NewRequest {
            dispatch_id: Some(uuid::Uuid::new_v4().to_string()),
            source_node_id: Some(crate::fleet::NodeId::generate()),
            body: "implement once".into(),
            delegation_parent: Some(DelegationRef {
                node_id: crate::fleet::NodeId::generate(),
                request_id: "external-parent".into(),
            }),
            ..Default::default()
        };
        let (a, b) = tokio::join!(
            mailbox.create(from.clone(), to.clone(), input.clone()),
            mailbox.create(from.clone(), to.clone(), input.clone())
        );
        let a = a.unwrap();
        assert_eq!(a.id, b.unwrap().id);
        assert_eq!(mailbox.claim_for(&to).await.unwrap().len(), 1);
        let reloaded = CollaborationStore::load(options).await.unwrap();
        assert_eq!(
            reloaded
                .create(from.clone(), to.clone(), input.clone())
                .await
                .unwrap()
                .id,
            a.id
        );
        let mut changed = input.clone();
        changed.body = "different task".into();
        assert!(matches!(
            reloaded.create(from.clone(), to.clone(), changed).await,
            Err(CollaborationError::DispatchConflict(_))
        ));
        assert!(matches!(
            reloaded
                .create(from, participant("%3", "other"), input)
                .await,
            Err(CollaborationError::DispatchConflict(_))
        ));
    }

    #[tokio::test]
    async fn dispatch_retries_cannot_change_wake_or_human_decision_contract() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let human = console_in_window("@1");
        let input = NewRequest {
            dispatch_id: Some(uuid::Uuid::new_v4().to_string()),
            body: "choose".into(),
            human_action: Some(HumanAction::Choice),
            ..Default::default()
        };
        let request = store
            .create(sender.clone(), human.clone(), input.clone())
            .await
            .unwrap();
        for (action, notify) in [
            (HumanAction::Approval, None),
            (HumanAction::Choice, Some(true)),
        ] {
            let mut changed = input.clone();
            changed.human_action = Some(action);
            changed.notify = notify;
            assert!(matches!(
                store.create(sender.clone(), human.clone(), changed).await,
                Err(CollaborationError::DispatchConflict(_))
            ));
        }
        assert_eq!(
            store.create(sender, human, input).await.unwrap().id,
            request.id
        );
    }

    #[tokio::test]
    async fn human_escalation_is_separate_durable_and_resolved_by_operator() {
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("human.json")),
            ..CollaborationOptions::default()
        };
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        let sender = participant("%1", "sender");
        let peer = participant("%2", "peer");
        let human = resolve_target(&peer, "human", &[], CollaborationScope::Window).unwrap();
        let parent = mailbox
            .create(
                sender.clone(),
                peer.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        assert!(!parent.needs_human_response());
        let request = mailbox
            .create(
                peer.clone(),
                human.clone(),
                NewRequest {
                    body: "Choose A or B; recommend A because it preserves compatibility".into(),
                    human_action: Some(HumanAction::Choice),
                    parent_request_id: Some(parent.id.clone()),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(request.thread_id, parent.thread_id);
        assert!(request.needs_human_response());
        assert_eq!(mailbox.pending_human_notifications().await.len(), 1);
        assert!(mailbox
            .pending_unnotified()
            .await
            .iter()
            .all(|r| r.id != request.id));
        // Reading/claiming is not an answer and must not cause repeat alerts.
        let claimed = mailbox.claim_for(&human).await.unwrap();
        assert!(claimed[0].needs_human_response());
        mailbox.mark_notified(&request.id).await.unwrap();
        let reloaded = CollaborationStore::load(options).await.unwrap();
        assert!(reloaded.pending_human_notifications().await.is_empty());
        assert!(reloaded
            .reply(
                &sender,
                &request.id,
                RequestStatus::Completed,
                "spoof".into(),
                vec![],
                vec![]
            )
            .await
            .is_err());
        let resolved = reloaded
            .reply(
                &human,
                &request.id,
                RequestStatus::Completed,
                "A".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(!resolved.needs_human_response());
        assert_eq!(reloaded.pending_reply_unnotified().await.len(), 1);
        let legacy = serde_json::to_value(&parent).unwrap();
        assert!(!serde_json::from_value::<CollaborationRequest>(legacy)
            .unwrap()
            .needs_human_response());
    }

    #[tokio::test]
    async fn request_updates_are_durable_bounded_and_do_not_complete_work() {
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("mailbox.json")),
            ..CollaborationOptions::default()
        };
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let initiator = participant("%9", "coordinator");
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "bounded task".into(),
                    initiator: Some(Box::new(initiator.clone())),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let mut changes = mailbox.subscribe();
        mailbox
            .update_request(&recipient, &request.id, "progress".into())
            .await
            .unwrap();
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();
        let retried = mailbox
            .update_request(&recipient, &request.id, "progress".into())
            .await
            .unwrap();
        assert_eq!(retried.updates.len(), 1);
        assert!(!changes.has_changed().unwrap());
        // Advisory initiator metadata does not grant participant authority.
        assert!(matches!(
            mailbox
                .update_request(&initiator, &request.id, "spoof".into())
                .await,
            Err(CollaborationError::NotParticipant(_))
        ));
        assert!(matches!(
            mailbox
                .update_request(&sender, &request.id, " ".into())
                .await,
            Err(CollaborationError::EmptyMessage)
        ));
        assert!(matches!(
            mailbox
                .update_request(&sender, &request.id, "x".repeat(8193))
                .await,
            Err(CollaborationError::MessageTooLarge(_))
        ));
        for i in 0..40 {
            mailbox
                .update_request(&sender, &request.id, format!("guidance {i}"))
                .await
                .unwrap();
        }
        let reloaded = CollaborationStore::load(options).await.unwrap();
        let restored = reloaded.get_for(&recipient, &request.id).await.unwrap();
        assert_eq!(restored.initiator, Some(Box::new(initiator)));
        assert_eq!(restored.updates.len(), 32);
        assert_eq!(restored.updates.last().unwrap().sequence, 41);
        assert_eq!(restored.status, RequestStatus::Queued);
        assert!(restored.reply.is_none());
        reloaded
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "done".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(matches!(
            reloaded
                .update_request(&sender, &request.id, "late".into())
                .await,
            Err(CollaborationError::AlreadyTerminal(_))
        ));
    }

    #[tokio::test]
    async fn moved_sessions_keep_mailbox_ownership_without_cross_host_access() {
        for durable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mailbox = CollaborationStore::load(CollaborationOptions {
                path: durable.then(|| dir.path().join("moved.sqlite3")),
                ..Default::default()
            })
            .await
            .unwrap();
            let sender = participant("%1", "sender");
            let recipient = participant("%2", "recipient");
            let request = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        body: "move test".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            for (original, mailbox_kind) in [
                (&sender, RequestMailbox::Sent),
                (&recipient, RequestMailbox::Incoming),
            ] {
                let mut moved = original.clone();
                moved.pane = "%9".into();
                moved.room.window_id = "@9".into();
                assert!(mailbox.get_for(&moved, &request.id).await.is_ok());
                assert_eq!(
                    mailbox
                        .list_for(&moved, mailbox_kind, MailboxScope::Caller)
                        .await
                        .unwrap()
                        .len(),
                    1
                );
                assert_eq!(
                    mailbox
                        .query_for(
                            &moved,
                            mailbox_kind,
                            MailboxScope::Caller,
                            &CollaborationQuery::default()
                        )
                        .await
                        .unwrap()
                        .total,
                    1
                );
                let mut strangers = vec![moved.clone(); 3];
                strangers[0].agent_session_id = "stranger".into();
                strangers[1].socket = Some("other".into());
                strangers[2].room.host = "other-host".into();
                for stranger in strangers {
                    assert!(matches!(
                        mailbox.get_for(&stranger, &request.id).await,
                        Err(CollaborationError::NotParticipant(_))
                    ));
                    assert_eq!(
                        mailbox
                            .query_for(
                                &stranger,
                                mailbox_kind,
                                MailboxScope::Caller,
                                &CollaborationQuery::default()
                            )
                            .await
                            .unwrap()
                            .total,
                        0
                    );
                }
            }
            let mut moved_recipient = recipient;
            moved_recipient.pane = "%10".into();
            moved_recipient.room.window_id = "@10".into();
            mailbox
                .reply(
                    &moved_recipient,
                    &request.id,
                    RequestStatus::Completed,
                    "done".into(),
                    vec![],
                    vec![],
                )
                .await
                .unwrap();
            let mut moved_sender = sender;
            moved_sender.pane = "%11".into();
            assert_eq!(mailbox.unread_reply_count(&moved_sender).await, 1);
            assert!(mailbox
                .get_for(&moved_sender, &request.id)
                .await
                .unwrap()
                .reply_read_at
                .is_some());
        }
    }

    #[test]
    fn pending_session_does_not_follow_pane_move() {
        let pending = participant("%1", "pending-pane:default:%1");
        let mut moved = pending.clone();
        moved.pane = "%2".into();
        assert!(!addresses(&pending, &moved));
    }

    fn participant(pane: &str, session: &str) -> Participant {
        Participant {
            launch_purpose: LaunchPurpose::Worker,
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

    /// Codex threads behind a shared app-server once all claimed `%9`. The
    /// caller's session id must pick its own row; an unknown id falls back
    /// to the pane, exactly as an origin without one does.
    #[test]
    fn origin_session_id_wins_over_a_shared_pane() {
        let mut stale = participant("%9", "thread-a");
        stale.socket = None;
        let peers = [
            participant("%9", "junia"),
            stale,
            participant("%24", "thread-b"),
        ];
        let origin = |pane: &str, session: Option<&str>| CollaborationOrigin {
            pane: pane.into(),
            socket: Some("default".into()),
            console: false,
            agent_session_id: session.map(str::to_owned),
        };
        let resolved = resolve_origin(&origin("%9", Some("thread-b")), &peers, &[]).unwrap();
        assert_eq!(resolved.agent_session_id, "thread-b");
        assert_eq!(resolved.pane, "%24");
        let fallback = resolve_origin(&origin("%24", Some("untracked")), &peers, &[]).unwrap();
        assert_eq!(fallback.agent_session_id, "thread-b");
        let by_pane = resolve_origin(&origin("%9", None), &peers, &[]).unwrap();
        assert_eq!(by_pane.agent_session_id, "junia");
        let unbound = resolve_origin(&origin("", Some("fresh")), &peers, &[]).unwrap_err();
        assert!(unbound.to_string().contains("not bound"), "{unbound}");
    }

    fn pane_info(pane_id: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_program: None,
            agent_role: None,
            agent_alias: None,
            workspace_id: None,
            work_id: None,
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

    fn remove_database_files(legacy_path: &Path) {
        let database_path = collaboration_database_path(legacy_path);
        for suffix in ["", "-wal", "-shm"] {
            let mut candidate = database_path.as_os_str().to_os_string();
            candidate.push(suffix);
            let candidate = PathBuf::from(candidate);
            match std::fs::remove_file(candidate) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database file: {error}"),
            }
        }
    }

    fn managed_pane(pane_id: &str, role: &str, alias: &str) -> PaneInfo {
        PaneInfo {
            agent_program: None,
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
        let mut participants = participants_from(&store.snapshot().await, &panes);
        assert_eq!(participants.len(), 2);
        assert!(
            resolve_target(
                &participants[1],
                "role:implementer",
                &participants,
                CollaborationScope::Window
            )
            .is_err(),
            "role metadata alone never grants worker purpose"
        );
        for participant in &mut participants {
            participant.launch_purpose = LaunchPurpose::Worker;
        }

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

    /// A pane muxa launched but whose agent has not registered yet is
    /// addressable by pane id. This is the codex spawn case: no hook can fire
    /// until a prompt arrives, so waiting for registration before sending
    /// would deadlock.
    #[tokio::test]
    async fn explicit_pane_target_resolves_a_launched_pane_with_no_session() {
        let sender = participant("%1", "sender");
        let mut launched = pane_info("%2");
        launched.agent_role = Some("peer".into());
        let panes = vec![pane_info("%1"), launched];
        let agents = Vec::new();

        // Ordinary resolution has nothing to select: the pane hosts no
        // participant.
        assert!(resolve_target(&sender, "pane:%2", &[], CollaborationScope::Window).is_err());

        let pending = resolve_pending_pane_target(
            &sender,
            "pane:%2",
            &[],
            &agents,
            &panes,
            CollaborationScope::Window,
        )
        .expect("launched pane resolves");
        assert_eq!(pending.pane, "%2");
        assert!(is_pending_session(&pending.agent_session_id));
        assert_eq!(
            pending.state,
            AgentState::Starting,
            "a pane with no row at all has reported nothing yet",
        );
        assert_eq!(pending.roles, vec!["peer".to_string()]);
    }

    /// The guards that keep a queued request off a human's shell and off a
    /// pane that already has a registered agent.
    #[tokio::test]
    async fn pending_pane_target_refuses_unmarked_panes_and_registered_ones() {
        let sender = participant("%1", "sender");
        let plain = pane_info("%2");
        let panes = vec![pane_info("%1"), plain];

        // No muxa launch mark and no classified row: not an agent pane.
        assert!(resolve_pending_pane_target(
            &sender,
            "pane:%2",
            &[],
            &[],
            &panes,
            CollaborationScope::Window,
        )
        .is_err());

        // A registered participant on the pane always wins; the pending path
        // must not shadow it with a pane-scoped placeholder.
        let registered = participant("%2", "real-session");
        let mut launched = pane_info("%2");
        launched.agent_role = Some("peer".into());
        assert!(resolve_pending_pane_target(
            &sender,
            "pane:%2",
            std::slice::from_ref(&registered),
            &[],
            &[pane_info("%1"), launched],
            CollaborationScope::Window,
        )
        .is_err());

        // Automatic routing never lands on a pending pane.
        assert!(resolve_pending_pane_target(
            &sender,
            "peer",
            &[],
            &[],
            &panes,
            CollaborationScope::Window,
        )
        .is_err());
    }

    /// Delivery waits for the pane to read idle — a screen-detected row is
    /// enough, which is the only readiness signal a pre-session codex emits.
    #[tokio::test]
    async fn pending_recipient_becomes_deliverable_once_the_pane_reads_idle() {
        let store = crate::Store::shared();
        let id = crate::event::AgentId {
            kind: AgentKind::Codex,
            session_id: format!("{}default:%2", crate::state::SYNTHETIC_SESSION_PREFIX),
            surface: None,
            pane: Some("%2".into()),
            tmux_socket: Some("default".into()),
            cwd: Some("/repo".into()),
        };
        store
            .apply(&crate::event::AgentEvent::Started {
                id: id.clone(),
                at: OffsetDateTime::now_utc(),
            })
            .await;

        let sender = participant("%1", "sender");
        let mut launched = pane_info("%2");
        launched.agent_role = Some("peer".into());
        let panes = vec![pane_info("%1"), launched];
        let agents = store.snapshot().await;
        let pending = resolve_pending_pane_target(
            &sender,
            "pane:%2",
            &[],
            &agents,
            &panes,
            CollaborationScope::Window,
        )
        .expect("discovered agent pane resolves");
        assert_eq!(pending.agent_kind, AgentKind::Codex);

        // A discovered agent pane reads idle, boot window included.
        let ready = pending_recipient_ready(&pending, &agents, &panes).expect("idle pane delivers");
        assert!(ready.same_endpoint(&pending), "identity must stay pinned");
        assert_eq!(ready.state, AgentState::Idle);

        // The startup gate — screen detection's whole reason for covering
        // codex — must hold delivery rather than answer the question.
        store
            .apply(&crate::event::AgentEvent::NotificationFired {
                id,
                level: crate::event::NotificationLevel::NeedsInput,
                message: "codex is waiting".into(),
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let waiting = store.snapshot().await;
        assert!(pending_recipient_ready(&pending, &waiting, &panes).is_none());

        // A pane that vanished delivers to nothing.
        assert!(pending_recipient_ready(&pending, &agents, &[pane_info("%1")]).is_none());
    }

    /// The session that finally registers on the pane adopts the request, and
    /// from then on the request is session-pinned like any other.
    #[tokio::test]
    async fn a_registering_session_adopts_and_pins_its_pane_request() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let mut pending = participant("%2", "placeholder");
        pending.agent_session_id = format!("{PENDING_SESSION_PREFIX}default:%2");
        pending.state = AgentState::Starting;
        pending.roles = vec!["peer".into()];

        let request = mailbox
            .create(
                sender.clone(),
                pending.clone(),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "review the diff".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        // An agent on a different pane must never inherit the work.
        let intruder = participant("%3", "other-session");
        assert!(mailbox.claim_for(&intruder).await.unwrap().is_empty());

        let registered = participant("%2", "codex-session");
        let inbox = mailbox.claim_for(&registered).await.unwrap();
        assert_eq!(inbox.len(), 1, "the pane's agent claims the queued request");
        assert_eq!(inbox[0].id, request.id);

        let stored = mailbox.get_for(&registered, &request.id).await.unwrap();
        assert_eq!(
            stored.to.agent_session_id, "codex-session",
            "the claiming session is pinned into the recipient",
        );
        assert_eq!(
            stored.to.roles,
            vec!["peer".to_string()],
            "the launcher's role survives adoption when the session has none",
        );

        // Pinned means pinned: the placeholder no longer matches.
        assert!(mailbox.claim_for(&pending).await.unwrap().is_empty());

        let replied = mailbox
            .reply(
                &registered,
                &request.id,
                RequestStatus::Completed,
                "looks good".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(replied.status, RequestStatus::Completed);
    }

    /// Pane ids restart at `%0` when a tmux server does, and a queued request
    /// outlives that. Ownership therefore checks the room as well as the
    /// endpoint, or a recycled pane id would inherit unrelated work.
    #[tokio::test]
    async fn a_recycled_pane_id_in_another_room_does_not_inherit_the_request() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let mut pending = participant("%2", "placeholder");
        pending.agent_session_id = format!("{PENDING_SESSION_PREFIX}default:%2");

        mailbox
            .create(
                sender,
                pending,
                NewRequest {
                    kind: RequestKind::Review,
                    body: "review the diff".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        let mut elsewhere = participant("%2", "unrelated-session");
        elsewhere.room.window_id = "@9".into();
        assert!(
            mailbox.claim_for(&elsewhere).await.unwrap().is_empty(),
            "same pane id, different room: not the addressed pane",
        );
        assert_eq!(mailbox.unread_count(&elsewhere).await, 0);

        let same_room = participant("%2", "codex-session");
        assert_eq!(mailbox.claim_for(&same_room).await.unwrap().len(), 1);
    }

    /// The launch mark is a tmux *pane* option that outlives the process it was
    /// stamped for, so the delivery gate re-applies the send-time evidence
    /// rather than trusting the mark alone.
    #[tokio::test]
    async fn delivery_refuses_a_pane_whose_agent_evidence_is_gone() {
        let store = crate::Store::shared();
        store
            .apply(&crate::event::AgentEvent::Started {
                id: crate::event::AgentId {
                    kind: AgentKind::Unknown,
                    session_id: format!("{}default:%2", crate::state::SYNTHETIC_SESSION_PREFIX),
                    surface: None,
                    pane: Some("%2".into()),
                    tmux_socket: Some("default".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let agents = store.snapshot().await;
        let mut pending = participant("%2", "placeholder");
        pending.agent_session_id = format!("{PENDING_SESSION_PREFIX}default:%2");

        // No launch mark left, and the row classifies as nothing in
        // particular: the pane is no longer evidently an agent.
        assert!(pending_recipient_ready(&pending, &agents, &[pane_info("%2")]).is_none());

        // The same row on a pane muxa marked is still deliverable.
        let mut marked = pane_info("%2");
        marked.agent_role = Some("peer".into());
        assert!(pending_recipient_ready(&pending, &agents, &[marked]).is_some());
    }

    /// A pending request answered without an inbox pull — the direct-wake path
    /// — is adopted just the same.
    #[tokio::test]
    async fn a_pending_request_can_be_answered_by_the_registered_session_directly() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let mut pending = participant("%2", "placeholder");
        pending.agent_session_id = format!("{PENDING_SESSION_PREFIX}default:%2");

        let request = mailbox
            .create(
                sender,
                pending,
                NewRequest {
                    kind: RequestKind::Question,
                    body: "is the release green?".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        let registered = participant("%2", "codex-session");
        assert_eq!(mailbox.unread_count(&registered).await, 1);
        let replied = mailbox
            .reply(
                &registered,
                &request.id,
                RequestStatus::Completed,
                "green".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(replied.to.agent_session_id, "codex-session");
    }

    /// A pane shown through a session group is listed once per session, and
    /// muxa's own `tmux-auto-view` puts every attached client in one. Two
    /// terminals on a workspace therefore list every pane twice — which used
    /// to make all of them invisible to collaboration, with an error telling
    /// the operator to restart an agent that was working fine.
    #[tokio::test]
    async fn a_pane_seen_through_a_view_session_is_one_participant() {
        let store = crate::Store::shared();
        store
            .apply(&crate::event::AgentEvent::Started {
                id: crate::event::AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-session".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/repo".into()),
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let agents = store.snapshot().await;

        let mut base = pane_info("%1");
        base.session = "muxa".into();
        base.session_group = Some("muxa".into());
        let mut view = pane_info("%1");
        view.session = "muxa~view~348778".into();
        view.session_group = Some("muxa".into());
        view.session_id = "$99".into();

        let participants = participants_from(&agents, &[base, view]);
        assert_eq!(participants.len(), 1, "one pane, one participant");
        assert_eq!(
            participants[0].tmux_session_name.as_deref(),
            Some("muxa"),
            "the durable session names it, not the per-client view",
        );
    }

    /// `muxa watch` resolves its origin as a console against the pane list, so
    /// the same duplication took the operator's own surface out too.
    #[tokio::test]
    async fn a_console_origin_resolves_through_a_view_session() {
        let mut base = pane_info("%1");
        base.session = "muxa".into();
        base.session_group = Some("muxa".into());
        let mut view = pane_info("%1");
        view.session = "muxa~view~348778".into();
        view.session_group = Some("muxa".into());

        let origin = CollaborationOrigin {
            pane: "%1".into(),
            socket: Some("default".into()),
            console: true,
            agent_session_id: None,
        };
        let resolved = resolve_origin(&origin, &[], &[base, view]).expect("console resolves");
        assert!(resolved.console);
        assert_eq!(resolved.room.window_id, "@1");
    }

    /// The ambiguity that matters is a pane id repeated across *servers*. It
    /// still refuses — this fix narrows the check, it does not remove it.
    #[tokio::test]
    async fn the_same_pane_id_on_two_servers_is_still_ambiguous() {
        let mut here = pane_info("%1");
        here.socket = Some("default".into());
        let mut elsewhere = pane_info("%1");
        elsewhere.socket = Some("other".into());
        elsewhere.window_id = "@7".into();

        let origin = CollaborationOrigin {
            pane: "%1".into(),
            socket: None,
            console: true,
            agent_session_id: None,
        };
        assert!(matches!(
            resolve_origin(&origin, &[], &[here, elsewhere]),
            Err(CollaborationError::AmbiguousOrigin(_))
        ));
    }

    /// The pending-pane resolver reads the same pane list and had the same
    /// shape, so a marked or spawned pane vanished under a view too.
    #[tokio::test]
    async fn a_pending_pane_resolves_through_a_view_session() {
        let sender = participant("%2", "sender");
        let mut base = pane_info("%1");
        base.session = "muxa".into();
        base.agent_role = Some("peer".into());
        let mut view = pane_info("%1");
        view.session = "muxa~view~348778".into();
        view.agent_role = Some("peer".into());

        let pending = resolve_pending_pane_target(
            &sender,
            "pane:%1",
            &[],
            &[],
            &[base, view, pane_info("%2")],
            CollaborationScope::Window,
        )
        .expect("a launched pane stays addressable through a view");
        assert_eq!(pending.pane, "%1");
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
    async fn launched_peers_are_visible_before_first_prompt_and_adopt_requests() {
        for (program, kind) in [
            ("codex", AgentKind::Codex),
            ("claude", AgentKind::ClaudeCode),
        ] {
            let mut pane = pane_info("%2");
            pane.current_command = program.into();
            pane.agent_program = Some(program.into());
            pane.agent_alias = Some("review".into());
            pane.agent_role = Some("reviewer".into());
            let panes = vec![pane.clone()];
            let mut pending = participants_from(&[], &panes).remove(0);
            assert_eq!(pending.agent_kind, kind);
            assert_eq!(pending.launch_purpose, LaunchPurpose::Primary);
            pending.launch_purpose = LaunchPurpose::Worker;
            assert_eq!(pending.state, AgentState::Starting);
            assert!(is_pending_session(&pending.agent_session_id));
            let sender = participant("%1", "sender");
            for selector in ["peer", "@review", "role:reviewer", "pane:%2"] {
                assert!(resolve_target(
                    &sender,
                    selector,
                    std::slice::from_ref(&pending),
                    CollaborationScope::Window
                )
                .is_ok());
            }
            assert!(pending_recipient_ready(&pending, &[], &panes).is_none());
            let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
            let request = mailbox
                .create(
                    sender,
                    pending.clone(),
                    NewRequest {
                        kind: RequestKind::Question,
                        body: "review without an initial prompt".into(),
                        expects_reply: true,
                        work_mode: WorkMode::ReadOnly,
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
            let store = crate::Store::shared();
            let discovered = crate::discovery::Discovered { pane, kind };
            store
                .apply(&crate::discovery::synthesize_started(
                    &discovered,
                    OffsetDateTime::now_utc(),
                ))
                .await;
            let agents = store.snapshot().await;
            let peers = participants_from(&agents, &panes);
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].agent_session_id, pending.agent_session_id);
            assert!(pending_recipient_ready(&pending, &agents, &panes).is_some());
            store
                .apply(&crate::event::AgentEvent::Started {
                    id: crate::event::AgentId {
                        kind,
                        session_id: "real-session".into(),
                        surface: None,
                        pane: Some("%2".into()),
                        tmux_socket: Some("default".into()),
                        cwd: None,
                    },
                    at: OffsetDateTime::now_utc(),
                })
                .await;
            let real = participants_from(&store.snapshot().await, &panes);
            assert_eq!(real.len(), 1);
            assert_eq!(real[0].agent_session_id, "real-session");
            assert!(addresses(&pending, &real[0]));
            let mut other_server = real[0].clone();
            other_server.socket = Some("other".into());
            assert!(!addresses(&pending, &other_server));
            assert!(mailbox.claim_for(&other_server).await.unwrap().is_empty());
            let claimed = [mailbox.claim_by_id(&real[0], &request.id).await.unwrap()];
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].id, request.id);
            assert_eq!(
                mailbox
                    .get_for(&real[0], &request.id)
                    .await
                    .unwrap()
                    .to
                    .agent_session_id,
                "real-session"
            );
        }
    }

    #[test]
    fn launch_marks_do_not_expose_shells_or_different_processes_as_peers() {
        let mut pane = pane_info("%2");
        pane.agent_program = Some("codex".into());
        for command in ["zsh", "vim", "claude"] {
            pane.current_command = command.into();
            assert!(participants_from(&[], &[pane.clone()]).is_empty());
        }
        pane.current_command = "codex".into();
        pane.agent_program = None;
        assert!(participants_from(&[], &[pane]).is_empty());
    }

    /// A reply is a terminal write, so an empty one must not spend it. This
    /// cost a real review round trip: a peer's `muxa msg reply` lost its body
    /// to the shell, the request closed with nothing in it, and the findings
    /// that followed were refused as "already terminal".
    #[tokio::test]
    async fn an_empty_reply_is_refused_and_leaves_the_request_answerable() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "reviewer");
        let request = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "review the diff".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox.claim_for(&recipient).await.unwrap();

        for blank in ["", "   ", "\n\t "] {
            let refused = mailbox
                .reply(
                    &recipient,
                    &request.id,
                    RequestStatus::Completed,
                    blank.into(),
                    Vec::new(),
                    Vec::new(),
                )
                .await;
            assert!(
                matches!(refused, Err(CollaborationError::EmptyMessage)),
                "{blank:?} should be refused, got {refused:?}",
            );
        }

        // Refusing a terminal write is only worth anything if the request is
        // still there to answer.
        let stored = mailbox.get_for(&recipient, &request.id).await.unwrap();
        assert_eq!(stored.status, RequestStatus::Claimed);
        assert!(stored.reply.is_none());

        let answered = mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "  no blockers found  ".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(answered.status, RequestStatus::Completed);
        assert_eq!(
            answered.reply.expect("reply").body,
            "no blockers found",
            "stored trimmed, the way a request body is",
        );
    }

    /// A decline with no reason tells the sender as little as an empty
    /// completion does.
    #[tokio::test]
    async fn every_terminal_status_needs_a_body() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "reviewer");
        for status in [
            RequestStatus::Declined,
            RequestStatus::Blocked,
            RequestStatus::Failed,
        ] {
            let request = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        kind: RequestKind::Question,
                        body: "is the release green?".into(),
                        expects_reply: true,
                        work_mode: WorkMode::ReadOnly,
                        paths: Vec::new(),
                        air_artifacts: Vec::new(),
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
            let refused = mailbox
                .reply(
                    &recipient,
                    &request.id,
                    status,
                    String::new(),
                    Vec::new(),
                    Vec::new(),
                )
                .await;
            assert!(
                matches!(refused, Err(CollaborationError::EmptyMessage)),
                "{status:?} with no body should be refused, got {refused:?}",
            );
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
                    air_artifacts: vec![air_reference(AirArtifactProfile::PlanNativeCli)],
                    ..NewRequest::default()
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
        let peeked = mailbox.peek_for(&sender, &request.id).await.unwrap();
        assert!(peeked.reply_read_at.is_none());
        assert!(peeked.reply_notified_at.is_none());
        assert_eq!(mailbox.unread_reply_count(&sender).await, 1);
        assert_eq!(mailbox.pending_reply_unnotified().await.len(), 1);
        assert!(mailbox
            .peek_for(&participant("%3", "stranger"), &request.id)
            .await
            .is_err());
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
    async fn notification_policy_survives_reload_and_quiet_records_remain_claimable() {
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("collaboration.json")),
            ..CollaborationOptions::default()
        };
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let cases = [
            (RequestKind::Notice, false, None, false),
            (RequestKind::Notice, false, Some(true), true),
            (RequestKind::Notice, true, None, true),
            (RequestKind::Question, true, Some(false), false),
            (RequestKind::Question, true, None, true),
            (RequestKind::Review, true, None, true),
            (RequestKind::Task, false, None, true),
        ];
        let mut created = Vec::new();
        for (kind, expects_reply, notify, wakes) in cases {
            let request = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        kind,
                        body: "checkpoint result".into(),
                        expects_reply,
                        notify,
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(request.should_notify(), wakes);
            // Old durable records omit the optional field, preserving auto policy.
            if notify.is_none() {
                let value = serde_json::to_value(&request).unwrap();
                assert!(value.get("notify").is_none());
                let legacy: CollaborationRequest = serde_json::from_value(value).unwrap();
                assert_eq!(legacy.should_notify(), wakes);
            }
            created.push(request);
        }
        drop(mailbox);
        let mailbox = CollaborationStore::load(options).await.unwrap();
        let pending = mailbox.pending_unnotified().await;
        for (request, (_, _, _, wakes)) in created.iter().zip(cases) {
            assert_eq!(pending.iter().any(|entry| entry.id == request.id), wakes);
            let stored = mailbox.get_for(&recipient, &request.id).await.unwrap();
            assert_eq!(stored.notify, request.notify);
            assert_eq!(stored.status, RequestStatus::Queued);
            assert!(stored.notified_at.is_none());
        }
        let inbox = mailbox.claim_for(&recipient).await.unwrap();
        assert_eq!(inbox.len(), cases.len());
        for request in inbox {
            assert_eq!(
                request.status,
                if request.expects_reply {
                    RequestStatus::Claimed
                } else {
                    RequestStatus::Completed
                }
            );
            assert!(request.reply.is_none());
        }
        assert!(mailbox.pending_unnotified().await.is_empty());
        // Suppressing a request's wake must not suppress its completion reply.
        mailbox
            .reply(
                &recipient,
                &created[3].id,
                RequestStatus::Completed,
                "review complete".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        let replies = mailbox.pending_reply_unnotified().await;
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, created[3].id);
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
                    ..NewRequest::default()
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
                    ..NewRequest::default()
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
                    ..NewRequest::default()
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
                    ..NewRequest::default()
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
                    ..NewRequest::default()
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
                    ..NewRequest::default()
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
                    ..NewRequest::default()
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
                    ..NewRequest::default()
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(CollaborationError::InvalidAirArtifactReference(_))
        ));
    }

    #[tokio::test]
    async fn expiry_closes_only_unclaimed_requests_older_than_the_cutoff() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let other = participant("%3", "other");
        let ask = |to: &Participant, body: &str| {
            let (mailbox, sender, to, body) =
                (&mailbox, sender.clone(), to.clone(), body.to_string());
            async move {
                mailbox
                    .create(
                        sender,
                        to,
                        NewRequest {
                            body,
                            ..NewRequest::default()
                        },
                    )
                    .await
                    .unwrap()
            }
        };
        let stale = ask(&recipient, "nobody claimed this").await;
        let claimed = ask(&other, "claimed, so it is the recipient's to finish").await;
        mailbox.claim_for(&other).await.unwrap();

        // Nothing is older than an hour yet.
        let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
        assert!(mailbox
            .expire_queued_before(past, false)
            .await
            .unwrap()
            .is_empty());

        let future = OffsetDateTime::now_utc() + time::Duration::seconds(1);
        let preview = mailbox.expire_queued_before(future, true).await.unwrap();
        assert_eq!(
            preview.iter().map(|r| &r.id).collect::<Vec<_>>(),
            [&stale.id]
        );
        assert_eq!(
            preview[0].status,
            RequestStatus::Queued,
            "a dry run changes nothing"
        );

        let expired = mailbox.expire_queued_before(future, false).await.unwrap();
        assert_eq!(
            expired.iter().map(|r| &r.id).collect::<Vec<_>>(),
            [&stale.id]
        );
        assert_eq!(expired[0].status, RequestStatus::Expired);
        let sent = mailbox
            .list_for(&sender, RequestMailbox::Sent, MailboxScope::Caller)
            .await
            .unwrap();
        let status = |id: &str| sent.iter().find(|r| r.id == id).unwrap().status;
        assert_eq!(status(&stale.id), RequestStatus::Expired);
        assert_eq!(status(&claimed.id), RequestStatus::Claimed);
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
                    ..NewRequest::default()
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
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(
            mailbox
                .list_for(&sender, RequestMailbox::Sent, MailboxScope::Caller)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(mailbox
            .list_for(&recipient, RequestMailbox::Sent, MailboxScope::Caller)
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
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(collaboration_database_path(options.path.as_ref().unwrap()))
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
    async fn direct_empty_sqlite_path_initializes_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collaboration.sqlite3");
        std::fs::write(&path, []).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let store = CollaborationStore::load(CollaborationOptions {
            path: Some(path.clone()),
            ..CollaborationOptions::default()
        })
        .await
        .unwrap();
        assert!(store.enabled());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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

        remove_database_files(&parent.join("collaboration.json"));
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
                    ..NewRequest::default()
                },
            )
            .await;

        assert!(matches!(result, Err(CollaborationError::Persistence(_))));
        assert!(mailbox.pending_unnotified().await.is_empty());
        assert!(mailbox
            .list_for(&sender, RequestMailbox::All, MailboxScope::Caller)
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
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        remove_database_files(&path);
        std::fs::remove_dir(&parent).unwrap();
        std::fs::write(&parent, b"blocks create_dir_all").unwrap();
        assert!(matches!(
            mailbox.mark_notified(&request.id).await,
            Err(CollaborationError::Persistence(_))
        ));
        assert!(mailbox.pending_unnotified().await.is_empty());
    }

    fn participant_in_window(pane: &str, session: &str, window_id: &str) -> Participant {
        let mut participant = participant(pane, session);
        participant.room.window_id = window_id.into();
        participant
    }

    fn console_in_window(window_id: &str) -> Participant {
        Participant::console(RoomId {
            host: "tmux".into(),
            socket: Some("default".into()),
            window_id: window_id.into(),
        })
    }

    async fn ask(store: &CollaborationStore, from: Participant, to: Participant, body: &str) {
        store
            .create(
                from,
                to,
                NewRequest {
                    kind: RequestKind::Question,
                    body: body.into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn operator_exact_read_matches_all_scope_without_acknowledging_agent_reply() {
        for durable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = CollaborationStore::load(CollaborationOptions {
                path: durable.then(|| dir.path().join("operator-read.sqlite3")),
                ..Default::default()
            })
            .await
            .unwrap();
            let sender = participant_in_window("%1", "sender", "@1");
            let recipient = participant_in_window("%2", "recipient", "@1");
            let request = store
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        body: "read exactly one".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            store
                .reply(
                    &recipient,
                    &request.id,
                    RequestStatus::Completed,
                    "reply".into(),
                    vec![],
                    vec![],
                )
                .await
                .unwrap();
            let peeked = store.peek_for(&sender, &request.id).await.unwrap();
            assert!(peeked.reply_read_at.is_none());
            assert!(peeked.reply_notified_at.is_none());
            let console = console_in_window("@2");
            let listed = store
                .list_for(&console, RequestMailbox::All, MailboxScope::All)
                .await
                .unwrap();
            let read = store.get_for(&console, &request.id).await.unwrap();
            assert_eq!(read.id, listed[0].id);
            assert!(read.reply_read_at.is_none());
            assert_eq!(store.unread_reply_count(&sender).await, 1);
            assert!(matches!(
                store
                    .get_for(&participant("%3", "stranger"), &request.id)
                    .await,
                Err(CollaborationError::NotParticipant(_))
            ));
            assert!(matches!(
                store.get_for(&console, "missing").await,
                Err(CollaborationError::NotFound(_))
            ));
            let reloaded = if durable {
                CollaborationStore::load(CollaborationOptions {
                    path: Some(dir.path().join("operator-read.sqlite3")),
                    ..Default::default()
                })
                .await
                .unwrap()
            } else {
                store
            };
            assert!(reloaded
                .get_for(&console, &request.id)
                .await
                .unwrap()
                .reply_read_at
                .is_none());
            assert!(reloaded
                .get_for(&sender, &request.id)
                .await
                .unwrap()
                .reply_read_at
                .is_some());
        }
    }

    #[tokio::test]
    async fn room_scope_lists_traffic_the_caller_was_never_party_to() {
        // The operator console dispatches and never receives, so its own
        // mailbox says nothing about what the agents in front of it are
        // saying to each other. That is the whole point of widening.
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        ask(
            &store,
            participant_in_window("%1", "one", "@1"),
            participant_in_window("%2", "two", "@1"),
            "peer to peer",
        )
        .await;
        let console = console_in_window("@1");

        assert!(store
            .list_for(&console, RequestMailbox::All, MailboxScope::Caller)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .list_for(&console, RequestMailbox::All, MailboxScope::Room)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn all_scope_reaches_rooms_the_console_is_not_in() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        ask(
            &store,
            participant_in_window("%1", "one", "@1"),
            participant_in_window("%2", "two", "@1"),
            "inside the room",
        )
        .await;
        ask(
            &store,
            participant_in_window("%3", "three", "@2"),
            participant_in_window("%4", "four", "@2"),
            "another window entirely",
        )
        .await;
        // Crosses rooms: sent by @1, received in @2.
        ask(
            &store,
            participant_in_window("%1", "one", "@1"),
            participant_in_window("%3", "three", "@2"),
            "across the two",
        )
        .await;
        let console = console_in_window("@1");

        assert_eq!(
            store
                .list_for(&console, RequestMailbox::All, MailboxScope::Room)
                .await
                .unwrap()
                .len(),
            2,
            "the room's own traffic plus what it sent out"
        );
        // Direction still discriminates inside a widened scope: the crossing
        // request left @1 and landed in @2.
        assert_eq!(
            store
                .list_for(&console, RequestMailbox::Incoming, MailboxScope::Room)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .list_for(&console, RequestMailbox::All, MailboxScope::All)
                .await
                .unwrap()
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn an_agent_cannot_list_past_its_own_mailbox() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        ask(
            &store,
            participant_in_window("%1", "one", "@1"),
            participant_in_window("%2", "two", "@1"),
            "not yours to read",
        )
        .await;
        // A room-mate, not the console: same window, no operator authority.
        let agent = participant_in_window("%9", "nine", "@1");

        for scope in [MailboxScope::Room, MailboxScope::All] {
            assert!(matches!(
                store.list_for(&agent, RequestMailbox::All, scope).await,
                Err(CollaborationError::ScopeDenied)
            ));
        }
        assert!(store
            .list_for(&agent, RequestMailbox::All, MailboxScope::Caller)
            .await
            .unwrap()
            .is_empty());
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

    /// rmux displays `%N` but muxa stores `rmux:%N`; the id as displayed must
    /// still name the peer on the sender's own server.
    #[test]
    fn a_bare_pane_id_resolves_in_the_rmux_senders_namespace() {
        let sender = participant("rmux:%1", "sender");
        let peers = vec![sender.clone(), participant("rmux:%2", "two")];
        for selector in ["%2", "pane:%2", "rmux:%2"] {
            for scope in [CollaborationScope::Window, CollaborationScope::Host] {
                assert_eq!(
                    resolve_target(&sender, selector, &peers, scope)
                        .unwrap()
                        .pane,
                    "rmux:%2",
                    "{selector} under {scope:?}"
                );
            }
        }
    }

    /// A tmux pane has no prefix, so an rmux sender can only name it bare.
    #[test]
    fn a_bare_pane_id_still_reaches_a_tmux_peer_from_an_rmux_sender() {
        let sender = participant("rmux:%1", "sender");
        let peers = vec![sender.clone(), participant("%2", "tmux")];
        for scope in [CollaborationScope::Window, CollaborationScope::Host] {
            assert_eq!(
                resolve_target(&sender, "%2", &peers, scope).unwrap().pane,
                "%2",
                "{scope:?}"
            );
        }
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

    #[test]
    fn host_scope_reaches_namespaced_panes_in_other_sessions_only_by_exact_id() {
        for (host, from, to) in [
            ("rmux", "rmux:%1", "rmux:%99"),
            ("cmux", "cmux:surface-1", "cmux:surface-99"),
        ] {
            let mut sender = participant(from, "sender");
            sender.room.host = host.into();
            let mut recipient = participant(to, "recipient");
            recipient.room.host = host.into();
            recipient.room.window_id = "@other".into();
            recipient.tmux_session_id = Some("$2".into());
            recipient.tmux_session_name = Some("other-session".into());
            recipient.alias = Some("reviewer".into());
            recipient.roles = vec!["reviewer".into()];
            let peers = [sender.clone(), recipient.clone()];
            for selector in [to.to_string(), format!("pane:{to}")] {
                assert_eq!(
                    resolve_target(&sender, &selector, &peers, CollaborationScope::Host).unwrap(),
                    recipient
                );
                assert!(
                    resolve_target(&sender, &selector, &peers, CollaborationScope::Window).is_err()
                );
            }
            for selector in ["peer", "@reviewer", "alias:reviewer", "role:reviewer"] {
                assert!(
                    resolve_target(&sender, selector, &peers, CollaborationScope::Host).is_err()
                );
            }
        }
    }

    #[test]
    fn host_scope_rejects_ambiguous_namespaced_panes_on_different_servers() {
        let mut sender = participant("rmux:%1", "sender");
        sender.room.host = "rmux".into();
        let mut first = participant("rmux:%99", "first");
        first.room.host = "rmux".into();
        first.room.window_id = "@other".into();
        let mut second = first.clone();
        second.agent_session_id = "second".into();
        second.socket = Some("/tmp/other-rmux.sock".into());
        second.room.socket.clone_from(&second.socket);
        assert!(matches!(
            resolve_target(
                &sender,
                "rmux:%99",
                &[first, second],
                CollaborationScope::Host
            ),
            Err(CollaborationError::AmbiguousTarget(_))
        ));
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
            agent_session_id: None,
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
                agent_session_id: None,
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
                agent_session_id: None,
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
                agent_session_id: None,
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
                agent_session_id: None,
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
                agent_session_id: None,
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
                agent_session_id: None,
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
                agent_session_id: None,
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
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
        }

        assert_eq!(
            mailbox
                .list_for(&from_here, RequestMailbox::Sent, MailboxScope::Caller)
                .await
                .unwrap()
                .len(),
            2
        );
        // And the replies land in the recipient's mailbox, which is where the
        // operator reads them — by pointing the cursor at that row.
        assert_eq!(
            mailbox
                .list_for(&recipient, RequestMailbox::Incoming, MailboxScope::Caller)
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
                agent_session_id: None,
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
                        ..NewRequest::default()
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
                agent_session_id: None,
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

    fn room_of(participant: &Participant) -> RoomId {
        participant.room.clone()
    }

    fn mint(base: &str) -> HandleRequest {
        HandleRequest::Mint {
            base: base.to_string(),
        }
    }

    #[tokio::test]
    async fn minting_walks_the_family_until_it_finds_a_free_name() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let a = participant("%1", "a");
        let room = room_of(&a);
        let live = vec![a.clone(), participant("%2", "b"), participant("%3", "c")];

        // Nothing is named yet, so each pane in turn takes the next name —
        // and the reservation, not the pane option, is what makes the second
        // caller skip `claude`. No scan has happened.
        let mut issued = Vec::new();
        for pane in ["%1", "%2", "%3"] {
            issued.push(
                mailbox
                    .issue_handle(&room, pane, &live, mint("claude"))
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(issued, ["claude", "claude2", "claude3"]);
    }

    #[tokio::test]
    async fn identity_then_mint_does_not_hand_out_the_identity_name() {
        // The ordering the pane-option-only allocator could not see: `%1`
        // carries a minted `claude` but has registered itself as `codex`, so
        // a codex pane starting next must not be handed `codex`.
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let registered = Participant {
            alias: Some("claude".into()),
            ..participant("%1", "registered-session")
        };
        let newcomer = participant("%2", "newcomer-session");
        let live = vec![registered.clone(), newcomer.clone()];
        let room = room_of(&registered);

        mailbox
            .set_identity(&registered, &live, Some("codex".into()), Vec::new())
            .await
            .unwrap();

        let issued = mailbox
            .issue_handle(&room, "%2", &live, mint("codex"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            issued, "codex2",
            "the registered identity holds `codex` even though no pane option says so",
        );
    }

    #[tokio::test]
    async fn mint_then_identity_refuses_the_minted_name() {
        // The mirror ordering: a name promised to `%2` is taken before `%2`
        // has written it, so `%1` cannot register it in the meantime.
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let first = participant("%1", "first-session");
        let second = participant("%2", "second-session");
        let live = vec![first.clone(), second.clone()];
        let room = room_of(&first);

        let issued = mailbox
            .issue_handle(&room, "%2", &live, mint("claude"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(issued, "claude");

        assert!(matches!(
            mailbox
                .set_identity(&first, &live, Some("claude".into()), Vec::new())
                .await,
            Err(CollaborationError::AliasInUse(_)),
        ));
    }

    #[tokio::test]
    async fn an_explicit_reservation_and_a_mint_cannot_collide() {
        // A launcher's explicit alias registers before it stamps the pane, so
        // a mint in flight for another pane skips the name — the cross-pane
        // hole that neither the pane option check nor `set-option -o` closed.
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let a = participant("%1", "a");
        let live = vec![a.clone(), participant("%2", "b")];
        let room = room_of(&a);

        mailbox
            .issue_handle(
                &room,
                "%1",
                &live,
                HandleRequest::Reserve {
                    handle: "claude".into(),
                },
            )
            .await
            .unwrap();
        let issued = mailbox
            .issue_handle(&room, "%2", &live, mint("claude"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(issued, "claude2");
    }

    #[tokio::test]
    async fn two_explicit_reservations_of_one_name_are_refused() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let a = participant("%1", "a");
        let live = vec![a.clone(), participant("%2", "b")];
        let room = room_of(&a);
        let reserve = || HandleRequest::Reserve {
            handle: "reviewer".into(),
        };

        mailbox
            .issue_handle(&room, "%1", &live, reserve())
            .await
            .unwrap();
        assert!(matches!(
            mailbox.issue_handle(&room, "%2", &live, reserve()).await,
            Err(CollaborationError::AliasInUse(_)),
        ));
        // The same pane re-reserving its own name is not a conflict — a
        // relaunch into the same slot keeps the name the config gave it.
        assert!(mailbox
            .issue_handle(&room, "%1", &live, reserve())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_confirmed_reservation_stops_holding_the_name() {
        // Once a scan shows the pane actually carrying the handle, the
        // participant list speaks for it. Keeping the promise as well would
        // hold a name out of circulation after the pane released it.
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let a = participant("%1", "a");
        let live = vec![a.clone(), participant("%2", "b")];
        let room = room_of(&a);
        mailbox
            .issue_handle(&room, "%1", &live, mint("claude"))
            .await
            .unwrap();

        let scanned = vec![
            Participant {
                alias: Some("claude".into()),
                ..a.clone()
            },
            participant("%2", "b"),
        ];
        // `%1` gave the name up between scans; nothing holds it now.
        let released = vec![participant("%1", "a"), participant("%2", "b")];
        assert_eq!(
            mailbox
                .issue_handle(&room, "%2", &scanned, mint("claude"))
                .await
                .unwrap()
                .unwrap(),
            "claude2",
            "a confirmed handle is still taken while the pane holds it",
        );
        assert_eq!(
            mailbox
                .issue_handle(&room, "%2", &released, mint("claude"))
                .await
                .unwrap()
                .unwrap(),
            "claude",
        );
    }

    #[tokio::test]
    async fn a_minted_handle_cannot_be_squatted_by_a_registered_identity() {
        // The seeded half of "taken". `%2` answers to `claude` because muxa
        // minted it onto the pane, not because anybody registered it — and
        // an agent registering the same name would leave `@claude`
        // ambiguous for both of them.
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let minted = Participant {
            alias: Some("claude".into()),
            ..participant("%2", "minted-session")
        };
        let other = participant("%3", "other-session");
        let live = vec![minted, other.clone()];

        assert!(matches!(
            mailbox
                .set_identity(&other, &live, Some("CLAUDE".into()), Vec::new())
                .await,
            Err(CollaborationError::AliasInUse(_)),
        ));
        // A free name still goes through.
        mailbox
            .set_identity(&other, &live, Some("verifier".into()), Vec::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_agent_may_re_register_the_handle_it_already_answers_to() {
        // Its own seeded name is not somebody else's claim on it.
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let mine = Participant {
            alias: Some("claude".into()),
            ..participant("%2", "my-session")
        };
        let live = vec![mine.clone()];
        mailbox
            .set_identity(&mine, &live, Some("claude".into()), vec!["review".into()])
            .await
            .unwrap();
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

    #[tokio::test]
    async fn parent_links_derive_one_exact_thread_and_reject_foreign_parents() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let root = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "round one".into(),
                    workspace_id: Some("workspace-a".into()),
                    work_id: Some("CAL-7345".into()),
                    artifacts: vec!["commit:d4bf2aa53".into()],
                    links: vec!["https://example.invalid/review/1".into()],
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(root.thread_id.as_deref(), Some(root.id.as_str()));

        let child = mailbox
            .create(
                recipient.clone(),
                sender.clone(),
                NewRequest {
                    kind: RequestKind::Notice,
                    body: "changes required".into(),
                    parent_request_id: Some(root.id.clone()),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(child.thread_id, root.thread_id);
        assert_eq!(child.parent_request_id.as_deref(), Some(root.id.as_str()));

        let conflict = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "wrong thread".into(),
                    thread_id: Some("guessed-thread".into()),
                    parent_request_id: Some(root.id.clone()),
                    ..NewRequest::default()
                },
            )
            .await;
        assert!(matches!(
            conflict,
            Err(CollaborationError::ThreadMismatch { .. })
        ));

        let mut foreign_sender = sender;
        let mut foreign_recipient = recipient;
        foreign_sender.room.window_id = "@99".into();
        foreign_recipient.room.window_id = "@99".into();
        let foreign = mailbox
            .create(
                foreign_sender,
                foreign_recipient,
                NewRequest {
                    body: "cross-room parent".into(),
                    parent_request_id: Some(root.id.clone()),
                    ..NewRequest::default()
                },
            )
            .await;
        assert!(matches!(
            foreign,
            Err(CollaborationError::InvalidParentScope(_))
        ));
        let missing = mailbox
            .create(
                child.from,
                child.to,
                NewRequest {
                    body: "missing parent".into(),
                    parent_request_id: Some("req-does-not-exist".into()),
                    ..NewRequest::default()
                },
            )
            .await;
        assert!(matches!(
            missing,
            Err(CollaborationError::ParentNotFound(_))
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one scenario proves the indexed filters and cursor boundary together
    async fn session_scope_filters_client_views_and_socket_before_pagination() {
        let dir = tempfile::tempdir().unwrap();
        for database in [false, true] {
            let mailbox = CollaborationStore::load(CollaborationOptions {
                path: database.then(|| dir.path().join("session-scope.sqlite3")),
                ..CollaborationOptions::default()
            })
            .await
            .unwrap();
            let caller = Participant::console(participant("%0", "caller").room);
            for (name, socket) in [
                ("callabo", "default"),
                ("callabo~view~123", "default"),
                ("junia", "default"),
                ("callabo", "other"),
            ] {
                let mut from = participant("%1", &format!("sender-{name}-{socket}"));
                from.tmux_session_name = Some(name.into());
                from.socket = Some(socket.into());
                from.room.socket = Some(socket.into());
                let mut to = from.clone();
                to.pane = "%2".into();
                to.agent_session_id = format!("receiver-{name}-{socket}");
                mailbox
                    .create(
                        from,
                        to,
                        NewRequest {
                            body: "scope test".into(),
                            ..NewRequest::default()
                        },
                    )
                    .await
                    .unwrap();
            }
            let query = CollaborationQuery {
                tmux_session_name: Some("callabo".into()),
                tmux_socket: Some("default".into()),
                limit: Some(1),
                ..CollaborationQuery::default()
            };
            let page = mailbox
                .query_for(&caller, RequestMailbox::All, MailboxScope::All, &query)
                .await
                .unwrap();
            assert_eq!(page.total, 2);
            assert_eq!(page.requests.len(), 1);
            assert!(page.has_more);
            let second = mailbox
                .query_for(
                    &caller,
                    RequestMailbox::All,
                    MailboxScope::All,
                    &CollaborationQuery {
                        cursor: page.next_cursor.clone(),
                        ..query
                    },
                )
                .await
                .unwrap();
            assert_eq!(second.total, 2);
            assert_eq!(second.requests.len(), 1);
            assert!(!second.has_more);
            assert_ne!(page.requests[0].id, second.requests[0].id);
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one scenario proves indexed filters and cursor boundaries together
    async fn sqlite_query_filters_and_keyset_pages_use_room_and_session_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collaboration.sqlite3");
        let mailbox = CollaborationStore::load(CollaborationOptions {
            path: Some(path),
            ..CollaborationOptions::default()
        })
        .await
        .unwrap();
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let root = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "review".into(),
                    workspace_id: Some("workspace-a".into()),
                    work_id: Some("CAL-7345".into()),
                    run_id: Some("run-1".into()),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox.claim_for(&recipient).await.unwrap();
        mailbox
            .reply(
                &recipient,
                &root.id,
                RequestStatus::Completed,
                "approved".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        mailbox.get_for(&sender, &root.id).await.unwrap();
        let child = mailbox
            .create(
                recipient.clone(),
                sender.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "follow up".into(),
                    parent_request_id: Some(root.id.clone()),
                    workspace_id: Some("workspace-a".into()),
                    work_id: Some("CAL-7345".into()),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let mut other_room = recipient.clone();
        other_room.room.window_id = "@2".into();
        mailbox
            .create(
                sender.clone(),
                other_room.clone(),
                NewRequest {
                    body: "cross room".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let mut other_session = recipient;
        other_session.tmux_session_id = Some("$9".into());
        other_session.tmux_session_name = Some("release".into());
        mailbox
            .create(
                Participant::console(sender.room.clone()),
                other_session,
                NewRequest {
                    kind: RequestKind::Notice,
                    body: "operator notice".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        let console = Participant::console(sender.room);
        let filtered = mailbox
            .query_for(
                &console,
                RequestMailbox::All,
                MailboxScope::All,
                &CollaborationQuery {
                    workspace_id: Some("workspace-a".into()),
                    work_id: Some("CAL-7345".into()),
                    thread_id: root.thread_id.clone(),
                    kind: Some(RequestKind::Review),
                    status: Some(RequestStatus::Completed),
                    ..CollaborationQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(filtered.requests.len(), 1);
        assert_eq!(filtered.requests[0].id, root.id);

        let children = mailbox
            .query_for(
                &console,
                RequestMailbox::All,
                MailboxScope::All,
                &CollaborationQuery {
                    parent_request_id: Some(root.id.clone()),
                    ..CollaborationQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(children.requests[0].id, child.id);

        let room_page = mailbox
            .query_for(
                &console,
                RequestMailbox::All,
                MailboxScope::All,
                &CollaborationQuery {
                    room: Some(other_room.room),
                    ..CollaborationQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(room_page.requests.len(), 1);
        let session_page = mailbox
            .query_for(
                &console,
                RequestMailbox::All,
                MailboxScope::All,
                &CollaborationQuery {
                    tmux_session_id: Some("$9".into()),
                    tmux_session_name: Some("release".into()),
                    ..CollaborationQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(session_page.requests.len(), 1);
        assert!(session_page.requests[0].from.console);

        let first_page = mailbox
            .query_for(
                &console,
                RequestMailbox::All,
                MailboxScope::All,
                &CollaborationQuery {
                    limit: Some(2),
                    ..CollaborationQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(first_page.total, 4);
        assert!(first_page.has_more);
        let second_page = mailbox
            .query_for(
                &console,
                RequestMailbox::All,
                MailboxScope::All,
                &CollaborationQuery {
                    limit: Some(2),
                    cursor: first_page.next_cursor,
                    ..CollaborationQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(second_page.requests.len(), 2);
    }

    #[tokio::test]
    async fn readonly_history_pages_preserve_database_and_follow_new_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collaboration.json");
        assert_eq!(read_history_page(&path, 0, 10).unwrap().total, 0);
        assert!(!collaboration_database_path(&path).exists());
        let store = CollaborationStore::load(CollaborationOptions {
            path: Some(path.clone()),
            ..CollaborationOptions::default()
        })
        .await
        .unwrap();
        for body in ["first", "second", "third"] {
            store
                .create(
                    participant("%1", "sender"),
                    participant("%2", "recipient"),
                    NewRequest {
                        body: body.into(),
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
        }
        let db = collaboration_database_path(&path);
        let before = std::fs::read(&db).unwrap();
        let page = read_history_page(&path, 0, 2).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.requests.len(), 2);
        assert!(page.has_more);
        let older = read_history_page(&path, 2, 2).unwrap();
        assert_eq!(older.requests.len(), 1);
        assert!(!older.has_more);
        assert!(page.requests.iter().all(|r| r.id != older.requests[0].id));
        assert_eq!(std::fs::read(&db).unwrap(), before);
        store
            .create(
                participant("%1", "sender"),
                participant("%2", "recipient"),
                NewRequest {
                    body: "newest".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let refreshed = read_history_page(&path, 0, 1).unwrap();
        assert_eq!(refreshed.total, 4);
        assert_eq!(refreshed.requests[0].body, "newest");
    }

    #[tokio::test]
    async fn legacy_json_migrates_idempotently_and_backfills_thread_and_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collaboration.json");
        let seed = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut request = seed
            .create(
                participant("%1", "sender"),
                participant("%2", "recipient"),
                NewRequest {
                    body: "legacy message".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        request.thread_id = None;
        let original = participant("%4", "identity-session");
        let snapshot = Snapshot {
            version: COLLABORATION_SCHEMA_VERSION,
            requests: vec![request.clone()],
            identities: vec![CollaborationIdentity {
                room: original.room.clone(),
                pane: original.pane.clone(),
                socket: original.socket.clone(),
                agent_session_id: original.agent_session_id.clone(),
                alias: Some("builder".into()),
                roles: vec!["implementation".into()],
                updated_at: OffsetDateTime::now_utc(),
            }],
        };
        let legacy_bytes = serde_json::to_vec_pretty(&snapshot).unwrap();
        std::fs::write(&path, &legacy_bytes).unwrap();

        let options = CollaborationOptions {
            path: Some(path.clone()),
            ..CollaborationOptions::default()
        };
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        let loaded = mailbox
            .list_for(
                &participant("%1", "sender"),
                RequestMailbox::Sent,
                MailboxScope::Caller,
            )
            .await
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].thread_id.as_deref(), Some(request.id.as_str()));
        let mut enriched = vec![original];
        mailbox.enrich_participants(&mut enriched).await;
        assert_eq!(enriched[0].alias.as_deref(), Some("builder"));
        assert_eq!(std::fs::read(&path).unwrap(), legacy_bytes);

        let database_path = collaboration_database_path(&path);
        assert!(database_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let mut candidate = database_path.as_os_str().to_os_string();
                candidate.push(suffix);
                let candidate = PathBuf::from(candidate);
                if candidate.exists() {
                    assert_eq!(
                        std::fs::metadata(candidate).unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                }
            }
        }
        drop(mailbox);
        let reloaded = CollaborationStore::load(CollaborationOptions {
            path: Some(database_path),
            ..CollaborationOptions::default()
        })
        .await
        .unwrap();
        assert_eq!(
            reloaded
                .list_for(
                    &participant("%1", "sender"),
                    RequestMailbox::Sent,
                    MailboxScope::Caller,
                )
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one planted history covers every whole-thread retention guard
    async fn retention_prunes_only_whole_old_terminal_threads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collaboration.json");
        let seed = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("%1", "sender");
        let recipient = participant("%2", "recipient");
        let mut eligible = seed
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "old terminal".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let mut mixed_root = seed
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "old root with live child".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let mut mixed_child = seed
            .create(
                recipient.clone(),
                sender.clone(),
                NewRequest {
                    body: "still queued".into(),
                    parent_request_id: Some(mixed_root.id.clone()),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let mut unread = seed
            .create(
                sender,
                recipient,
                NewRequest {
                    body: "unread reply".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let old = OffsetDateTime::UNIX_EPOCH;
        eligible.created_at = old;
        eligible.status = RequestStatus::Cancelled;
        mixed_root.created_at = old;
        mixed_root.status = RequestStatus::Completed;
        mixed_child.created_at = old;
        unread.created_at = old;
        unread.status = RequestStatus::Completed;
        unread.reply = Some(CollaborationReply {
            status: RequestStatus::Completed,
            body: "not read".into(),
            artifacts: Vec::new(),
            air_artifacts: Vec::new(),
            at: old,
        });
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&Snapshot {
                version: COLLABORATION_SCHEMA_VERSION,
                requests: vec![
                    eligible.clone(),
                    mixed_root.clone(),
                    mixed_child.clone(),
                    unread.clone(),
                ],
                identities: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let mailbox = CollaborationStore::load(CollaborationOptions {
            path: Some(path),
            retention_days: Some(1),
            ..CollaborationOptions::default()
        })
        .await
        .unwrap();
        let remaining = mailbox
            .list_for(
                &Participant::console(mixed_root.from.room.clone()),
                RequestMailbox::All,
                MailboxScope::All,
            )
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> = remaining
            .iter()
            .map(|request| request.id.as_str())
            .collect();
        assert_eq!(remaining.len(), 3);
        assert!(!ids.contains(eligible.id.as_str()));
        assert!(ids.contains(mixed_root.id.as_str()));
        assert!(ids.contains(mixed_child.id.as_str()));
        assert!(ids.contains(unread.id.as_str()));
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

#[cfg(test)]
mod database_tests;
#[cfg(test)]
mod purpose_promote_tests;
#[cfg(test)]
mod recipient_slot_tests;
mod recovery;
#[cfg(test)]
mod reply_wake_tests;
pub use recovery::PeerInterruption;
