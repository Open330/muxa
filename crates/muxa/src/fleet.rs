//! Remote-host fleet model shared by muxad, the SSH relay, and clients.
//!
//! A fleet host is a physical machine. This is deliberately distinct from
//! [`crate::backend::HostKind`], whose historical name describes a local
//! multiplexer backend (tmux/rmux/zellij/herdr). Every remote target is keyed
//! by [`NodeId`] first, then by its backend endpoint and native topology ids.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use uuid::Uuid;

use crate::backend::{BackendCaps, HostKind};
use crate::collaboration::{CollaborationRequest, NewRequest, RequestStatus};
use crate::event::AgentState;
use crate::ipc::SendPromptOutcome;
use crate::state::{Agent, Transition};
use crate::tmux::layout::PaneGeometry;
use crate::tmux::{PaneInfo, SessionInfo};
use crate::topology::{PaneKey, WindowKey};

pub const FLEET_PROTOCOL_VERSION: u32 = 1;
pub const FLEET_MIN_PROTOCOL_VERSION: u32 = 1;
/// Reserved inventory alias for the daemon's own physical node.
pub const LOCAL_HOST_ALIAS: &str = "local";
/// Truth-bearing labels populated by muxad for the controller node. Users may
/// add metadata alongside these keys but cannot override them.
pub const LOCAL_MANAGED_LABELS: &[&str] = &[
    "muxa.io/local",
    "muxa.io/transport",
    "kubernetes.io/hostname",
    "kubernetes.io/os",
    "kubernetes.io/arch",
];
pub const FLEET_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const FLEET_MAX_DIAGNOSTIC_BYTES: usize = 1024;
/// Keep each Fleet mailbox tab comfortably below the relay frame ceiling even
/// when every request carries maximum-sized text and AIR references.
pub const FLEET_MAILBOX_REQUEST_LIMIT: usize = 32;
pub const FLEET_MAX_CAPTURE_BYTES: usize = 256 * 1024;
pub const FLEET_CAPABILITIES: &[&str] = &[
    "snapshot_watch",
    "capture",
    "window_capture",
    "send_prompt",
    "collaboration",
    "collaboration_get",
    "exact_pane_ref",
    "labels_v1",
    "raw_capture_base64",
];

/// Read one UTF-8 line without allowing a peer that withholds `\n` to grow
/// memory past `limit`. The terminator is retained, matching `read_line`.
pub async fn read_bounded_line<R>(
    reader: &mut R,
    line: &mut String,
    limit: usize,
) -> std::io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(0);
            }
            break;
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if bytes.len().saturating_add(take) > limit {
            return Err(std::io::Error::other(format!(
                "fleet frame exceeds {limit} bytes"
            )));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    *line = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(line.len())
}

/// Drain a stream to EOF so the child process cannot block on a full pipe,
/// retaining at most `limit` bytes for diagnostics.
pub async fn drain_bounded<R>(reader: &mut R, limit: usize) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::with_capacity(limit.min(4096));
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    Ok(retained)
}

/// Remove terminal control sequences from data that may originate on a
/// compromised remote node before rendering it in a trusted terminal.
#[must_use]
pub fn sanitize_terminal_text(value: &str) -> String {
    #[derive(Clone, Copy)]
    enum Escape {
        Text,
        Esc,
        Csi,
        Osc,
        OscEsc,
    }
    let mut state = Escape::Text;
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        state = match state {
            Escape::Text if character == '\u{1b}' => Escape::Esc,
            Escape::Text => {
                if !character.is_control() || matches!(character, '\n' | '\t') {
                    output.push(character);
                }
                Escape::Text
            }
            Escape::Esc if character == '[' => Escape::Csi,
            Escape::Esc if character == ']' => Escape::Osc,
            Escape::Esc => Escape::Text,
            Escape::Csi if ('@'..='~').contains(&character) => Escape::Text,
            Escape::Csi => Escape::Csi,
            Escape::Osc if character == '\u{7}' => Escape::Text,
            Escape::Osc if character == '\u{1b}' => Escape::OscEsc,
            Escape::OscEsc if character == '\\' => Escape::Text,
            Escape::Osc | Escape::OscEsc => Escape::Osc,
        };
    }
    output
}

/// Sanitize and retain only the newest bounded portion of a terminal capture.
#[must_use]
pub fn sanitize_capture_text(value: String) -> String {
    let mut text = sanitize_terminal_text(&value);
    if text.len() <= FLEET_MAX_CAPTURE_BYTES {
        return text;
    }
    let mut start = text.len() - FLEET_MAX_CAPTURE_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.drain(..start);
    text
}

/// Encode the newest bounded portion of a pane capture without interpreting
/// terminal control sequences. Base64 keeps hostile CSI/OSC bytes inert while
/// an authorized UI can still expose an escaped diagnostic view.
#[must_use]
pub fn raw_capture_base64(value: &str) -> String {
    let mut start = value.len().saturating_sub(FLEET_MAX_CAPTURE_BYTES);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    BASE64_STANDARD.encode(&value.as_bytes()[start..])
}

/// Revalidate an untrusted relay's raw capture and reapply the byte bound.
/// Invalid base64 is dropped instead of being forwarded to a UI.
#[must_use]
pub fn sanitize_raw_capture_base64(value: String) -> Option<String> {
    let decoded = BASE64_STANDARD.decode(value).ok()?;
    let start = decoded.len().saturating_sub(FLEET_MAX_CAPTURE_BYTES);
    Some(BASE64_STANDARD.encode(&decoded[start..]))
}

/// Durable UUID belonging to a physical machine.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        Uuid::parse_str(&value).map_err(|error| format!("invalid node id: {error}"))?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for NodeId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// Read a node id, or atomically create an owner-only identity file.
pub fn load_or_create_node_id(path: &Path) -> std::io::Result<NodeId> {
    match std::fs::read_to_string(path) {
        Ok(value) => NodeId::parse(value.trim()).map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            let generated = NodeId::generate();
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut file) => {
                    use std::io::Write as _;
                    writeln!(file, "{generated}")?;
                    file.sync_all()?;
                    Ok(generated)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let value = std::fs::read_to_string(path)?;
                    NodeId::parse(value.trim()).map_err(std::io::Error::other)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Per-host authorization at the central manager.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAccessMode {
    #[default]
    Observe,
    Control,
}

/// Complete collision-free pane address across physical nodes and local
/// multiplexer endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalPaneRef {
    pub node_id: NodeId,
    pub pane: PaneKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetCapturedWindowPane {
    pub geometry: PaneGeometry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetWindowCapture {
    pub window: WindowKey,
    pub panes: Vec<FleetCapturedWindowPane>,
    pub zoomed: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
}

/// Serializable subset of backend capabilities advertised by a relay.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetBackendInfo {
    pub kind: HostKind,
    pub current_command: bool,
    pub pane_pid_map: bool,
    pub capture_pane: bool,
    pub focus_pane: bool,
    pub send_text: bool,
}

impl FleetBackendInfo {
    #[must_use]
    pub fn new(kind: HostKind, caps: BackendCaps) -> Self {
        Self {
            kind,
            current_command: caps.current_command,
            pane_pid_map: caps.pane_pid_map,
            capture_pane: caps.capture_pane,
            focus_pane: caps.focus_pane,
            send_text: caps.send_text,
        }
    }
}

/// One coherent remote observation. Detailed terminal contents and retained
/// prompt history are intentionally absent; callers fetch a selected capture
/// on demand instead of centralizing every PTY.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSnapshot {
    pub revision: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    pub agents: Vec<Agent>,
    pub panes: Vec<PaneInfo>,
    pub sessions: Vec<SessionInfo>,
    pub backends: Vec<FleetBackendInfo>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetHostState {
    Disabled,
    #[default]
    Connecting,
    Online,
    Degraded,
    Offline,
    AuthFailed,
    VersionSkew,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetHostSnapshot {
    pub alias: String,
    /// True for the controller node itself. The local node is always present
    /// and never traverses SSH, even when remote Fleet connections are
    /// disabled.
    #[serde(default)]
    pub local: bool,
    pub ssh_target: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub mode: HostAccessMode,
    pub state: FleetHostState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muxa_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub last_seen_at: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub received_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteSnapshot>,
}

impl FleetHostSnapshot {
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.remote.as_ref().map_or(0, |remote| remote.agents.len())
    }

    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.remote.as_ref().map_or(0, |remote| remote.panes.len())
    }

    #[must_use]
    pub fn needs_attention(&self) -> usize {
        self.remote.as_ref().map_or(0, |remote| {
            remote
                .agents
                .iter()
                .filter(|agent| {
                    matches!(
                        agent.state,
                        AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
                    )
                })
                .count()
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetSnapshot {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub hosts: Vec<FleetHostSnapshot>,
}

impl FleetSnapshot {
    #[must_use]
    pub fn select(mut self, selector: Option<&LabelSelector>) -> Self {
        if let Some(selector) = selector {
            self.hosts.retain(|host| selector.matches(&host.labels));
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayHello {
    pub fleet_protocol: u32,
    pub min_fleet_protocol: u32,
    pub node_id: NodeId,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub muxa_version: String,
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_generation: Option<u64>,
    pub boot_id: String,
    pub backends: Vec<FleetBackendInfo>,
    #[serde(with = "time::serde::rfc3339")]
    pub server_time: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayRequest {
    Snapshot {
        request_id: String,
    },
    Ping {
        request_id: String,
    },
    Capture {
        request_id: String,
        pane: PaneKey,
    },
    CaptureWindow {
        request_id: String,
        window: WindowKey,
    },
    SendPrompt {
        request_id: String,
        pane: PaneKey,
        text: String,
        submit: bool,
    },
    CollaborationSend {
        request_id: String,
        pane: PaneKey,
        request: NewRequest,
    },
    CollaborationMailbox {
        request_id: String,
        pane: PaneKey,
    },
    CollaborationGet {
        request_id: String,
        pane: PaneKey,
        collaboration_request_id: String,
    },
    CollaborationClaim {
        request_id: String,
        pane: PaneKey,
    },
    CollaborationReply {
        request_id: String,
        pane: PaneKey,
        collaboration_request_id: String,
        status: RequestStatus,
        body: String,
    },
}

impl RelayRequest {
    #[must_use]
    pub fn request_id(&self) -> &str {
        match self {
            Self::Snapshot { request_id }
            | Self::Ping { request_id }
            | Self::Capture { request_id, .. }
            | Self::CaptureWindow { request_id, .. }
            | Self::SendPrompt { request_id, .. }
            | Self::CollaborationSend { request_id, .. }
            | Self::CollaborationMailbox { request_id, .. }
            | Self::CollaborationGet { request_id, .. }
            | Self::CollaborationClaim { request_id, .. }
            | Self::CollaborationReply { request_id, .. } => request_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayFrame {
    Hello {
        hello: RelayHello,
    },
    Snapshot {
        request_id: String,
        snapshot: RemoteSnapshot,
    },
    Transition {
        revision: u64,
        transition: Transition,
    },
    Keepalive {
        revision: u64,
        #[serde(with = "time::serde::rfc3339")]
        observed_at: OffsetDateTime,
        /// Optional durable mailbox invalidation. Keeping it on the existing
        /// keepalive frame is additive for mixed Fleet versions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mailbox_revision: Option<u64>,
    },
    Result {
        request_id: String,
        result: FleetCommandResult,
    },
    Error {
        request_id: String,
        code: String,
        message: String,
    },
    ResyncRequired {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetOperation {
    Connect,
    Disconnect,
    Refresh,
    Capture {
        pane: PaneKey,
    },
    CaptureWindow {
        window: WindowKey,
    },
    SendPrompt {
        pane: PaneKey,
        text: String,
        #[serde(default)]
        submit: bool,
    },
    CollaborationSend {
        pane: PaneKey,
        request: NewRequest,
    },
    CollaborationMailbox {
        pane: PaneKey,
    },
    CollaborationGet {
        pane: PaneKey,
        request_id: String,
    },
    CollaborationClaim {
        pane: PaneKey,
    },
    CollaborationReply {
        pane: PaneKey,
        request_id: String,
        status: RequestStatus,
        body: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetCommandResult {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_raw_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<SendPromptOutcomeWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_capture: Option<FleetWindowCapture>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collaboration_request: Option<Box<CollaborationRequest>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collaboration_incoming: Vec<CollaborationRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collaboration_sent: Vec<CollaborationRequest>,
}

impl FleetCommandResult {
    #[must_use]
    pub fn accepted(message: impl Into<String>) -> Self {
        Self {
            accepted: true,
            message: Some(message.into()),
            capture: None,
            capture_raw_base64: None,
            send: None,
            window_capture: None,
            collaboration_request: None,
            collaboration_incoming: Vec::new(),
            collaboration_sent: Vec::new(),
        }
    }

    #[must_use]
    pub fn capture(capture: Option<String>) -> Self {
        Self {
            accepted: true,
            message: None,
            capture,
            capture_raw_base64: None,
            send: None,
            window_capture: None,
            collaboration_request: None,
            collaboration_incoming: Vec::new(),
            collaboration_sent: Vec::new(),
        }
    }

    #[must_use]
    pub fn capture_with_raw(raw_capture: Option<String>) -> Self {
        let capture_raw_base64 = raw_capture.as_deref().map(raw_capture_base64);
        let capture = raw_capture.map(sanitize_capture_text);
        Self {
            accepted: true,
            message: None,
            capture,
            capture_raw_base64,
            send: None,
            window_capture: None,
            collaboration_request: None,
            collaboration_incoming: Vec::new(),
            collaboration_sent: Vec::new(),
        }
    }

    #[must_use]
    pub fn sent(outcome: SendPromptOutcome) -> Self {
        Self {
            accepted: true,
            message: None,
            capture: None,
            capture_raw_base64: None,
            send: Some(outcome.into()),
            window_capture: None,
            collaboration_request: None,
            collaboration_incoming: Vec::new(),
            collaboration_sent: Vec::new(),
        }
    }

    #[must_use]
    pub fn window_capture(capture: FleetWindowCapture) -> Self {
        Self {
            accepted: true,
            message: None,
            capture: None,
            capture_raw_base64: None,
            send: None,
            window_capture: Some(capture),
            collaboration_request: None,
            collaboration_incoming: Vec::new(),
            collaboration_sent: Vec::new(),
        }
    }

    #[must_use]
    pub fn collaboration_request(request: CollaborationRequest) -> Self {
        Self {
            accepted: true,
            message: None,
            capture: None,
            capture_raw_base64: None,
            send: None,
            window_capture: None,
            collaboration_request: Some(Box::new(request)),
            collaboration_incoming: Vec::new(),
            collaboration_sent: Vec::new(),
        }
    }

    #[must_use]
    pub fn collaboration_mailbox(
        mut incoming: Vec<CollaborationRequest>,
        mut sent: Vec<CollaborationRequest>,
    ) -> Self {
        incoming.truncate(FLEET_MAILBOX_REQUEST_LIMIT);
        sent.truncate(FLEET_MAILBOX_REQUEST_LIMIT);
        Self {
            accepted: true,
            message: None,
            capture: None,
            capture_raw_base64: None,
            send: None,
            window_capture: None,
            collaboration_request: None,
            collaboration_incoming: incoming,
            collaboration_sent: sent,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SendPromptOutcomeWire {
    pub sent: bool,
    pub submitted: bool,
}

impl From<SendPromptOutcome> for SendPromptOutcomeWire {
    fn from(value: SendPromptOutcome) -> Self {
        Self {
            sent: value.sent,
            submitted: value.submitted,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetUpdate {
    pub host: String,
    pub state: FleetHostState,
    pub revision: Option<u64>,
    /// The broadcast receiver lagged and the client must reconcile the whole
    /// selected snapshot rather than infer anything from `host`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resync: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox_revision: Option<u64>,
}

/// Central cache. Remote snapshots remain isolated per node and are never
/// inserted into the local agent Store, preventing pane-id collisions and
/// local reconciler/GC activity from mutating remote truth.
pub struct FleetStore {
    hosts: RwLock<BTreeMap<String, FleetHostSnapshot>>,
    updates: broadcast::Sender<FleetUpdate>,
}

impl Default for FleetStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FleetStore {
    #[must_use]
    pub fn new() -> Self {
        let (updates, _) = broadcast::channel(512);
        Self {
            hosts: RwLock::new(BTreeMap::new()),
            updates,
        }
    }

    pub async fn snapshot(&self) -> FleetSnapshot {
        self.snapshot_selected(None).await
    }

    pub async fn snapshot_selected(&self, selector: Option<&LabelSelector>) -> FleetSnapshot {
        let mut hosts = self
            .hosts
            .read()
            .await
            .values()
            .filter(|host| selector.is_none_or(|selector| selector.matches(&host.labels)))
            .cloned()
            .collect::<Vec<_>>();
        // The controller is the operator's anchor and should not disappear
        // below alphabetically-earlier SSH aliases.
        hosts.sort_by(|left, right| {
            right
                .local
                .cmp(&left.local)
                .then(left.alias.cmp(&right.alias))
        });
        FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<FleetUpdate> {
        self.updates.subscribe()
    }

    /// Return whether the current cached host matches `selector`. A missing
    /// host never matches. Fleet streams use this after receiving an update
    /// so selector-scoped watchers are not woken by unrelated hosts.
    pub async fn host_matches_selector(
        &self,
        alias: &str,
        selector: Option<&LabelSelector>,
    ) -> bool {
        self.hosts
            .read()
            .await
            .get(alias)
            .is_some_and(|host| selector.is_none_or(|selector| selector.matches(&host.labels)))
    }

    pub async fn upsert_host(&self, host: FleetHostSnapshot) {
        let alias = host.alias.clone();
        let state = host.state;
        let revision = host.remote.as_ref().map(|remote| remote.revision);
        self.hosts.write().await.insert(alias.clone(), host);
        let _ = self.updates.send(FleetUpdate {
            host: alias,
            state,
            revision,
            resync: false,
            mailbox_revision: None,
        });
    }

    pub async fn mutate_host(&self, alias: &str, mutate: impl FnOnce(&mut FleetHostSnapshot)) {
        let mut hosts = self.hosts.write().await;
        let Some(host) = hosts.get_mut(alias) else {
            return;
        };
        mutate(host);
        let update = FleetUpdate {
            host: alias.to_string(),
            state: host.state,
            revision: host.remote.as_ref().map(|remote| remote.revision),
            resync: false,
            mailbox_revision: None,
        };
        drop(hosts);
        let _ = self.updates.send(update);
    }

    /// Update controller-only bookkeeping (last-seen timestamps) without
    /// waking snapshot consumers whose UI-visible topology did not change.
    pub async fn mutate_host_silent(
        &self,
        alias: &str,
        mutate: impl FnOnce(&mut FleetHostSnapshot),
    ) {
        let mut hosts = self.hosts.write().await;
        if let Some(host) = hosts.get_mut(alias) {
            mutate(host);
        }
    }

    /// Publish a content-free mailbox invalidation without rebuilding or
    /// mutating the host topology snapshot.
    pub async fn notify_mailbox(&self, alias: &str, mailbox_revision: u64) {
        let hosts = self.hosts.read().await;
        let Some(host) = hosts.get(alias) else { return };
        let update = FleetUpdate {
            host: alias.to_string(),
            state: host.state,
            revision: host.remote.as_ref().map(|remote| remote.revision),
            resync: false,
            mailbox_revision: Some(mailbox_revision),
        };
        drop(hosts);
        let _ = self.updates.send(update);
    }

    pub async fn apply_transition(&self, alias: &str, revision: u64, transition: Transition) {
        self.mutate_host(alias, |host| {
            let Some(remote) = host.remote.as_mut() else {
                host.state = FleetHostState::Degraded;
                host.error = Some("transition arrived before initial snapshot".into());
                return;
            };
            if revision <= remote.revision {
                return;
            }
            if revision != remote.revision.saturating_add(1) {
                host.state = FleetHostState::Degraded;
                host.error = Some(format!(
                    "remote event gap: expected {}, received {revision}",
                    remote.revision.saturating_add(1)
                ));
            }
            let incoming = transition.agent.as_ref();
            if let Some(existing) = remote.agents.iter_mut().find(|agent| {
                agent.kind == incoming.kind && agent.session_id == incoming.session_id
            }) {
                *existing = incoming.clone();
            } else {
                remote.agents.push(incoming.clone());
            }
            remote.revision = revision;
            remote.observed_at = OffsetDateTime::now_utc();
            host.last_seen_at = Some(remote.observed_at);
            host.received_at = Some(OffsetDateTime::now_utc());
        })
        .await;
    }
}

struct FleetDispatch {
    host: String,
    operation: FleetOperation,
    reply: oneshot::Sender<Result<FleetCommandResult, String>>,
}

#[derive(Clone)]
pub struct FleetRuntime {
    pub store: Arc<FleetStore>,
    commands: mpsc::Sender<FleetDispatch>,
}

impl FleetRuntime {
    #[must_use]
    pub fn new(store: Arc<FleetStore>) -> (Self, FleetCommandReceiver) {
        let (commands, receiver) = mpsc::channel(128);
        (Self { store, commands }, FleetCommandReceiver(receiver))
    }

    pub async fn execute(
        &self,
        host: impl Into<String>,
        operation: FleetOperation,
        timeout: Duration,
    ) -> Result<FleetCommandResult, String> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(FleetDispatch {
                host: host.into(),
                operation,
                reply,
            })
            .await
            .map_err(|_| "fleet manager is not running".to_string())?;
        tokio::time::timeout(timeout, response)
            .await
            .map_err(|_| format!("fleet command timed out after {timeout:?}"))?
            .map_err(|_| "fleet manager dropped the command".to_string())?
    }
}

pub struct FleetCommandEnvelope {
    pub host: String,
    pub operation: FleetOperation,
    pub reply: oneshot::Sender<Result<FleetCommandResult, String>>,
}

pub struct FleetCommandReceiver(mpsc::Receiver<FleetDispatch>);

impl FleetCommandReceiver {
    pub async fn recv(&mut self) -> Option<FleetCommandEnvelope> {
        self.0.recv().await.map(|dispatch| FleetCommandEnvelope {
            host: dispatch.host,
            operation: dispatch.operation,
            reply: dispatch.reply,
        })
    }
}

/// Kubernetes-style label selector. Supported requirements are equality,
/// inequality, set membership, non-membership, existence, and non-existence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelSelector(Vec<LabelRequirement>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum LabelRequirement {
    Exists(String),
    NotExists(String),
    Eq(String, String),
    NotEq(String, String),
    In(String, HashSet<String>),
    NotIn(String, HashSet<String>),
}

impl LabelSelector {
    #[must_use]
    pub fn all() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        self.0.iter().all(|requirement| match requirement {
            LabelRequirement::Exists(key) => labels.contains_key(key),
            LabelRequirement::NotExists(key) => !labels.contains_key(key),
            LabelRequirement::Eq(key, value) => labels.get(key) == Some(value),
            LabelRequirement::NotEq(key, value) => labels.get(key) != Some(value),
            LabelRequirement::In(key, values) => {
                labels.get(key).is_some_and(|value| values.contains(value))
            }
            LabelRequirement::NotIn(key, values) => {
                labels.get(key).is_none_or(|value| !values.contains(value))
            }
        })
    }
}

impl FromStr for LabelSelector {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(Self::all());
        }
        split_selector(input)?
            .into_iter()
            .map(parse_requirement)
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

fn split_selector(input: &str) -> Result<Vec<&str>, String> {
    let mut depth = 0_u32;
    let mut start = 0;
    let mut parts = Vec::new();
    for (index, ch) in input.char_indices() {
        match ch {
            '(' => depth = depth.saturating_add(1),
            ')' => {
                if depth == 0 {
                    return Err("label selector has an unmatched ')'".into());
                }
                depth -= 1;
            }
            ',' if depth == 0 => {
                let part = input[start..index].trim();
                if part.is_empty() {
                    return Err("label selector contains an empty requirement".into());
                }
                parts.push(part);
                start = index + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("label selector has an unmatched '('".into());
    }
    let tail = input[start..].trim();
    if tail.is_empty() {
        return Err("label selector contains an empty requirement".into());
    }
    parts.push(tail);
    Ok(parts)
}

fn parse_requirement(input: &str) -> Result<LabelRequirement, String> {
    let input = input.trim();
    if let Some(key) = input.strip_prefix('!') {
        validate_label_key(key.trim())?;
        return Ok(LabelRequirement::NotExists(key.trim().to_string()));
    }
    for (operator, is_not) in [(" notin ", true), (" in ", false)] {
        if let Some((key, values)) = input.split_once(operator) {
            let key = key.trim();
            validate_label_key(key)?;
            let values = values.trim();
            let inner = values
                .strip_prefix('(')
                .and_then(|value| value.strip_suffix(')'))
                .ok_or_else(|| format!("selector '{input}' requires parenthesized values"))?;
            let parsed = inner
                .split(',')
                .map(str::trim)
                .map(|value| {
                    validate_label_value(value)?;
                    Ok(value.to_string())
                })
                .collect::<Result<HashSet<_>, String>>()?;
            if parsed.is_empty() {
                return Err(format!("selector '{input}' has no values"));
            }
            return Ok(if is_not {
                LabelRequirement::NotIn(key.to_string(), parsed)
            } else {
                LabelRequirement::In(key.to_string(), parsed)
            });
        }
    }
    if let Some((key, value)) = input.split_once("!=") {
        let (key, value) = (key.trim(), value.trim());
        validate_label_key(key)?;
        validate_label_value(value)?;
        return Ok(LabelRequirement::NotEq(key.to_string(), value.to_string()));
    }
    let equality = input.split_once("==").or_else(|| input.split_once('='));
    if let Some((key, value)) = equality {
        let (key, value) = (key.trim(), value.trim());
        validate_label_key(key)?;
        validate_label_value(value)?;
        return Ok(LabelRequirement::Eq(key.to_string(), value.to_string()));
    }
    validate_label_key(input)?;
    Ok(LabelRequirement::Exists(input.to_string()))
}

/// Validate the Kubernetes label-key shape: optional DNS prefix plus a
/// 63-character name with alphanumeric endpoints and `-_.` in the middle.
pub fn validate_label_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("label key cannot be empty".into());
    }
    let (prefix, name) = key
        .rsplit_once('/')
        .map_or((None, key), |(prefix, name)| (Some(prefix), name));
    if let Some(prefix) = prefix {
        if prefix.len() > 253 || !valid_dns_subdomain(prefix) {
            return Err(format!("invalid label DNS prefix '{prefix}'"));
        }
    }
    validate_label_name(name, false)
        .map_err(|message| format!("invalid label key '{key}': {message}"))
}

pub fn validate_label_value(value: &str) -> Result<(), String> {
    validate_label_name(value, true)
        .map_err(|message| format!("invalid label value '{value}': {message}"))
}

fn validate_label_name(value: &str, allow_empty: bool) -> Result<(), String> {
    if value.is_empty() {
        return allow_empty
            .then_some(())
            .ok_or_else(|| "name cannot be empty".into());
    }
    if value.len() > 63 {
        return Err("must be at most 63 bytes".into());
    }
    let valid_edge = |byte: u8| byte.is_ascii_alphanumeric();
    let bytes = value.as_bytes();
    if !valid_edge(bytes[0]) || !valid_edge(bytes[bytes.len() - 1]) {
        return Err("must begin and end with an alphanumeric character".into());
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("may contain only alphanumeric characters, '-', '_' or '.'".into());
    }
    Ok(())
}

fn valid_dns_subdomain(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && part.as_bytes()[0].is_ascii_alphanumeric()
                && part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionKey;
    use tempfile::tempdir;
    use tokio::io::{AsyncWriteExt, BufReader};

    fn host(alias: &str, local: bool, labels: BTreeMap<String, String>) -> FleetHostSnapshot {
        FleetHostSnapshot {
            alias: alias.into(),
            local,
            ssh_target: alias.into(),
            labels,
            annotations: BTreeMap::new(),
            mode: if local {
                HostAccessMode::Control
            } else {
                HostAccessMode::Observe
            },
            state: FleetHostState::Online,
            node_id: None,
            hostname: None,
            os: None,
            arch: None,
            muxa_version: None,
            protocol: None,
            capabilities: Vec::new(),
            daemon_generation: None,
            boot_id: None,
            latency_ms: None,
            last_seen_at: None,
            received_at: None,
            error: None,
            remote: None,
        }
    }

    #[test]
    fn node_id_is_stable_and_owner_scoped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("muxa/host-id");
        let first = load_or_create_node_id(&path).unwrap();
        let second = load_or_create_node_id(&path).unwrap();
        assert_eq!(first, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn selector_supports_kubernetes_operators() {
        let labels = BTreeMap::from([
            ("environment".into(), "production".into()),
            ("tier".into(), "worker".into()),
            ("accelerator".into(), "gpu".into()),
        ]);
        assert!("environment=production,tier in (worker,api),accelerator"
            .parse::<LabelSelector>()
            .unwrap()
            .matches(&labels));
        assert!("environment!=qa,!draining,tier notin (frontend)"
            .parse::<LabelSelector>()
            .unwrap()
            .matches(&labels));
        assert!(!"environment=qa"
            .parse::<LabelSelector>()
            .unwrap()
            .matches(&labels));
    }

    #[test]
    fn selector_rejects_invalid_keys_and_parentheses() {
        assert!("UPPER/key=value".parse::<LabelSelector>().is_err());
        assert!("tier in worker".parse::<LabelSelector>().is_err());
        assert!("tier in (worker,api".parse::<LabelSelector>().is_err());
        assert!("a,,b".parse::<LabelSelector>().is_err());
    }

    #[tokio::test]
    async fn bounded_line_rejects_an_unterminated_oversized_frame() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let sender = tokio::spawn(async move {
            writer.write_all(b"123456789").await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let error = read_bounded_line(&mut reader, &mut line, 8)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 8 bytes"));
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn bounded_drain_discards_excess_but_reaches_eof() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let sender = tokio::spawn(async move {
            writer.write_all(b"abcdefgh").await.unwrap();
            writer.shutdown().await.unwrap();
        });
        assert_eq!(drain_bounded(&mut reader, 4).await.unwrap(), b"abcd");
        sender.await.unwrap();
    }

    #[test]
    fn terminal_sanitizer_removes_csi_osc_and_c0_controls() {
        assert_eq!(
            sanitize_terminal_text("ok\u{1b}[31mred\u{1b}[0m\u{1b}]0;bad\u{7}\u{0}done"),
            "okreddone"
        );
    }

    #[test]
    fn raw_capture_is_bounded_encoded_and_kept_separate_from_safe_text() {
        let raw = "ok\u{1b}[31mred\u{1b}[0m\r\n";
        let result = FleetCommandResult::capture_with_raw(Some(raw.into()));
        assert_eq!(result.capture.as_deref(), Some("okred\n"));
        assert_eq!(
            BASE64_STANDARD
                .decode(result.capture_raw_base64.unwrap())
                .unwrap(),
            raw.as_bytes()
        );
        assert!(sanitize_raw_capture_base64("not-base64".into()).is_none());
    }

    #[test]
    fn collaboration_relay_requests_round_trip_with_exact_pane_identity() {
        let pane = PaneKey {
            window: WindowKey {
                session: SessionKey {
                    endpoint: crate::BackendEndpoint {
                        host: HostKind::Tmux,
                        socket: "remote-default".into(),
                    },
                    session_id: "$7".into(),
                },
                window_id: "@8".into(),
            },
            pane_id: "%9".into(),
        };
        let request = RelayRequest::CollaborationSend {
            request_id: "relay-1".into(),
            pane: pane.clone(),
            request: NewRequest {
                kind: crate::collaboration::RequestKind::Review,
                body: "review this change".into(),
                expects_reply: true,
                work_mode: crate::collaboration::WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                air_artifacts: Vec::new(),
            },
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: RelayRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.request_id(), "relay-1");
        match decoded {
            RelayRequest::CollaborationSend {
                pane: decoded_pane,
                request,
                ..
            } => {
                assert_eq!(decoded_pane, pane);
                assert_eq!(request.kind, crate::collaboration::RequestKind::Review);
                assert_eq!(request.body, "review this change");
            }
            _ => panic!("wrong relay request variant"),
        }

        let get = RelayRequest::CollaborationGet {
            request_id: "relay-2".into(),
            pane: pane.clone(),
            collaboration_request_id: "collab-42".into(),
        };
        let encoded = serde_json::to_string(&get).unwrap();
        assert!(encoded.contains("\"kind\":\"collaboration_get\""));
        let decoded: RelayRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.request_id(), "relay-2");
        match decoded {
            RelayRequest::CollaborationGet {
                pane: decoded_pane,
                collaboration_request_id,
                ..
            } => {
                assert_eq!(decoded_pane, pane);
                assert_eq!(collaboration_request_id, "collab-42");
            }
            _ => panic!("wrong relay request variant"),
        }
    }

    #[tokio::test]
    async fn fleet_store_keeps_local_first_and_applies_selectors() {
        let store = FleetStore::new();
        store
            .upsert_host(host(
                "alpha",
                false,
                BTreeMap::from([("environment".into(), "production".into())]),
            ))
            .await;
        store
            .upsert_host(host(
                LOCAL_HOST_ALIAS,
                true,
                BTreeMap::from([("muxa.io/local".into(), "true".into())]),
            ))
            .await;
        let all = store.snapshot().await;
        assert_eq!(all.hosts[0].alias, LOCAL_HOST_ALIAS);
        let selector = "muxa.io/local=true".parse::<LabelSelector>().unwrap();
        let selected = store.snapshot_selected(Some(&selector)).await;
        assert_eq!(selected.hosts.len(), 1);
        assert!(selected.hosts[0].local);
    }
}
