//! Canonical session → window → pane topology shared by every muxa surface.
//!
//! The low-level backend seam still returns flat [`PaneInfo`] observations.
//! This module is the one place that turns those observations into durable,
//! collision-free identities and joins agent state to panes. Consumers must
//! not rebuild a `pane_id`-only lookup: tmux/rmux ids repeat per server.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::backend::{pane_endpoint_identity, pane_id_host_kind, HostKind};
use crate::event::{AgentState, SurfaceKind};
use crate::state::Agent;
use crate::tmux::PaneInfo;

pub const TOPOLOGY_SCHEMA_VERSION: u8 = 1;

/// Backend-local server identity. `socket` is always present in topology
/// keys, including for hosts that expose only one server (where muxa uses a
/// documented host-specific sentinel).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BackendEndpoint {
    pub host: HostKind,
    pub socket: String,
}

/// Stable identity of one session/project container.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    #[serde(flatten)]
    pub endpoint: BackendEndpoint,
    /// Backend-native stable session id. For tmux this is `$N`, never the
    /// mutable session name.
    pub session_id: String,
}

/// Stable identity of one window/ticket/collaboration room.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WindowKey {
    pub session: SessionKey,
    /// Backend-native stable window/tab id. For tmux this is `@N`.
    pub window_id: String,
}

/// Stable identity of one agent execution pane.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PaneKey {
    pub window: WindowKey,
    /// Backend-native pane id. It is deliberately never used without its
    /// host, socket, session, and window ancestry.
    pub pane_id: String,
}

impl SessionKey {
    /// Construct the canonical session key represented by a pane observation.
    #[must_use]
    pub fn from_pane(host: HostKind, pane: &PaneInfo) -> Self {
        Self {
            endpoint: endpoint_for(host, pane),
            session_id: stable_session_id(pane),
        }
    }
}

impl WindowKey {
    /// Construct the canonical window key represented by a pane observation.
    #[must_use]
    pub fn from_pane(host: HostKind, pane: &PaneInfo) -> Self {
        Self {
            session: SessionKey::from_pane(host, pane),
            window_id: stable_window_id(pane),
        }
    }
}

impl PaneKey {
    /// Construct the canonical pane key represented by a pane observation.
    #[must_use]
    pub fn from_pane(host: HostKind, pane: &PaneInfo) -> Self {
        Self {
            window: WindowKey::from_pane(host, pane),
            pane_id: pane.pane_id.clone(),
        }
    }
}

/// A key suitable for persisted watch selection and expansion state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "level", content = "key", rename_all = "snake_case")]
pub enum TopologyNodeKey {
    Session(SessionKey),
    Window(WindowKey),
    Pane(PaneKey),
}

impl TopologyNodeKey {
    /// The backend endpoint this node lives on, whatever level it is.
    ///
    /// Every level carries the same endpoint through its ancestry, so callers
    /// that only need "which server, which multiplexer" — a rename, a control
    /// command — do not have to match the variant to find out.
    #[must_use]
    pub fn endpoint(&self) -> &BackendEndpoint {
        match self {
            TopologyNodeKey::Session(session) => &session.endpoint,
            TopologyNodeKey::Window(window) => &window.session.endpoint,
            TopologyNodeKey::Pane(pane) => &pane.window.session.endpoint,
        }
    }
}

/// Whether a hierarchy level is native, mapped without inventing nodes, or
/// unavailable from a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HierarchyCapability {
    Native,
    Mapped,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendTopologyCapabilities {
    pub host: HostKind,
    pub session: HierarchyCapability,
    pub window: HierarchyCapability,
    pub pane: HierarchyCapability,
}

impl BackendTopologyCapabilities {
    #[must_use]
    pub const fn for_host(host: HostKind) -> Self {
        match host {
            HostKind::Tmux | HostKind::Rmux | HostKind::Zellij => Self {
                host,
                session: HierarchyCapability::Native,
                window: HierarchyCapability::Native,
                pane: HierarchyCapability::Native,
            },
            HostKind::Herdr => Self {
                host,
                session: HierarchyCapability::Mapped,
                window: HierarchyCapability::Mapped,
                pane: HierarchyCapability::Native,
            },
            HostKind::Cmux => Self {
                host,
                session: HierarchyCapability::Native,
                window: HierarchyCapability::Mapped,
                pane: HierarchyCapability::Native,
            },
        }
    }
}

/// Fixed state distribution used by parent nodes. Keeping counts explicit
/// makes JSON consumers independent of enum-map encoding details.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDistribution {
    pub starting: usize,
    pub working: usize,
    pub idle: usize,
    pub waiting_input: usize,
    pub waiting_choice: usize,
    pub error: usize,
    pub stopped: usize,
}

impl StateDistribution {
    fn add(&mut self, state: AgentState) {
        match state {
            AgentState::Starting => self.starting += 1,
            AgentState::Working => self.working += 1,
            AgentState::Idle => self.idle += 1,
            AgentState::WaitingInput => self.waiting_input += 1,
            AgentState::WaitingChoice => self.waiting_choice += 1,
            AgentState::Error => self.error += 1,
            AgentState::Stopped => self.stopped += 1,
        }
    }

    #[must_use]
    pub const fn total(&self) -> usize {
        self.starting
            + self.working
            + self.idle
            + self.waiting_input
            + self.waiting_choice
            + self.error
            + self.stopped
    }

    #[must_use]
    pub const fn needs_attention(&self) -> bool {
        self.waiting_input > 0 || self.waiting_choice > 0 || self.error > 0
    }

    fn extend(&mut self, other: &Self) {
        self.starting += other.starting;
        self.working += other.working;
        self.idle += other.idle;
        self.waiting_input += other.waiting_input;
        self.waiting_choice += other.waiting_choice;
        self.error += other.error;
        self.stopped += other.stopped;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneNode {
    pub key: PaneKey,
    pub index: String,
    pub tty: String,
    pub current_command: String,
    pub title: String,
    pub cwd: String,
    pub pane_pid: u32,
    /// At most one live agent owns a pane. If stale registry rows also point
    /// at it, the newest non-stopped row wins and the rest stay in
    /// `TopologySnapshot::unassigned_agents` rather than being silently lost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
    /// Pipeline alias the launcher stamped on this pane, when it has one.
    /// Present on the node because completion is counted over aliases, and a
    /// hand-split pane has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_alias: Option<String>,
}

impl PaneNode {
    #[must_use]
    pub fn node_key(&self) -> TopologyNodeKey {
        TopologyNodeKey::Pane(self.key.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowNode {
    pub key: WindowKey,
    pub name: String,
    pub index: String,
    pub cwd: Option<String>,
    pub states: StateDistribution,
    pub panes: Vec<PaneNode>,
    /// How far this window's pipeline has reported, or `None` when nothing
    /// knows.
    ///
    /// The durable pipeline Run is the only source: it holds both what the
    /// pipeline asked for and what reported back. tmux carries neither, so
    /// `TopologySnapshot::build` leaves this `None` and the surfaces that can
    /// reach a Run fill it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<WorkCompletion>,
    /// Durable daemon-owned pipeline state, when this window is bound to a
    /// declarative Work Run. Unlike `panes`, this includes aliases that have
    /// not launched yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_run: Option<crate::pipeline_run::PipelineRunSummary>,
}

/// `done` of `total` pipeline agents have reported `muxa work done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCompletion {
    pub done: usize,
    pub total: usize,
}

impl WorkCompletion {
    #[must_use]
    pub fn is_complete(self) -> bool {
        self.done == self.total
    }
}

impl WindowNode {
    #[must_use]
    pub fn node_key(&self) -> TopologyNodeKey {
        TopologyNodeKey::Window(self.key.clone())
    }

    #[must_use]
    pub fn active_pane(&self) -> Option<&PaneNode> {
        self.panes
            .iter()
            .filter(|pane| {
                pane.agent
                    .as_ref()
                    .is_some_and(|a| a.state != AgentState::Stopped)
            })
            .max_by_key(|pane| pane.agent.as_ref().map(|a| a.last_activity_at))
            .or_else(|| self.panes.first())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionNode {
    pub key: SessionKey,
    pub name: String,
    /// Only populated when a source supplies authoritative client metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attached_clients: Option<u32>,
    pub states: StateDistribution,
    pub windows: Vec<WindowNode>,
}

impl SessionNode {
    #[must_use]
    pub fn node_key(&self) -> TopologyNodeKey {
        TopologyNodeKey::Session(self.key.clone())
    }

    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.windows.iter().map(|window| window.panes.len()).sum()
    }

    #[must_use]
    pub fn active_window(&self) -> Option<&WindowNode> {
        self.windows
            .iter()
            .filter_map(|window| {
                let at = window
                    .panes
                    .iter()
                    .filter_map(|pane| pane.agent.as_ref())
                    .filter(|agent| agent.state != AgentState::Stopped)
                    .map(|agent| agent.last_activity_at)
                    .max()?;
                Some((at, window))
            })
            .max_by_key(|(at, _)| *at)
            .map(|(_, window)| window)
            .or_else(|| self.windows.first())
    }
}

/// Optional session metadata that cannot be recovered from pane rows alone.
#[derive(Debug, Clone)]
pub struct TopologySessionInput {
    pub endpoint: BackendEndpoint,
    pub session_id: String,
    pub name: String,
    pub attached_clients: Option<u32>,
}

/// One backend observation fed to [`TopologySnapshot::build`].
#[derive(Debug, Clone)]
pub struct TopologyInput {
    pub host: HostKind,
    pub capabilities: BackendTopologyCapabilities,
    pub panes: Vec<PaneInfo>,
    pub sessions: Vec<TopologySessionInput>,
}

impl TopologyInput {
    #[must_use]
    pub fn new(host: HostKind, panes: Vec<PaneInfo>) -> Self {
        Self {
            host,
            capabilities: BackendTopologyCapabilities::for_host(host),
            panes,
            sessions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologySnapshot {
    pub schema_version: u8,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub capabilities: Vec<BackendTopologyCapabilities>,
    pub sessions: Vec<SessionNode>,
    /// Raw observations from a backend that explicitly marks a required
    /// hierarchy level unsupported. They remain inspectable, but muxa does
    /// not manufacture session/window ancestors to force them into the tree.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unmapped_panes: Vec<PaneInfo>,
    /// Paneless agents, ambiguous legacy rows, and superseded stale rows are
    /// explicit instead of being joined to a representative pane by guesswork.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unassigned_agents: Vec<Agent>,
}

/// Collapse tmux session groups so one window set produces one tree.
///
/// A session group (`tmux new-session -t <session>`) shares one window list
/// across several sessions, each keeping its own current window. That is what
/// lets two terminals sit on two windows of one workspace — and it means
/// `list-panes -a`, which walks sessions, reports every pane once per member.
/// Left alone the topology shows the same workspace two or three times over,
/// with the agents attached to whichever copy happened to be scanned first.
///
/// The oldest live member (the lowest numeric `$N` session id) survives. tmux
/// creates every grouped sibling after the session it was created from, so the
/// original workspace is the oldest member even after it is renamed. The
/// group name cannot identify it: `rename-session` does not update
/// `#{session_group}`. If the original is gone, the oldest remaining view is a
/// stable fallback instead of whichever row the scan happened to return first.
///
/// Rows for the other members are rewritten onto the survivor's identity and
/// then deduplicated by pane, which is what actually merges the trees: the
/// duplicates were only ever distinguished by the session half of their key.
fn fold_session_groups(rows: Vec<(HostKind, PaneInfo)>) -> Vec<(HostKind, PaneInfo)> {
    // Nothing is grouped in the overwhelmingly common case; skip the work
    // rather than pay two passes and a map on every tick.
    if rows.iter().all(|(_, pane)| pane.session_group.is_none()) {
        return rows;
    }
    let mut survivors: HashMap<(HostKind, Option<String>, String), (String, String)> =
        HashMap::new();
    for (host, pane) in &rows {
        let Some(group) = pane.session_group.clone() else {
            continue;
        };
        let key = (*host, pane.socket.clone(), group.clone());
        let candidate = (pane.session_id.clone(), pane.session.clone());
        match survivors.get(&key) {
            Some((id, _)) if !tmux_session_id_is_earlier(&candidate.0, id) => {}
            _ => {
                survivors.insert(key, candidate);
            }
        }
    }
    let mut seen = HashSet::new();
    let mut folded = Vec::with_capacity(rows.len());
    for (host, mut pane) in rows {
        if let Some(group) = pane.session_group.clone() {
            if let Some((id, name)) = survivors.get(&(host, pane.socket.clone(), group)) {
                pane.session_id.clone_from(id);
                pane.session.clone_from(name);
            }
        }
        if seen.insert((
            host,
            pane.socket.clone(),
            pane.session_id.clone(),
            pane.window_id.clone(),
            pane.pane_id.clone(),
        )) {
            folded.push((host, pane));
        }
    }
    folded
}

/// tmux session ids are monotonically allocated decimal numbers prefixed by
/// `$`. Compare that numeric payload rather than the rendered string: `$107`
/// is newer than `$99`, despite sorting before it lexically.
fn tmux_session_id_is_earlier(candidate: &str, current: &str) -> bool {
    let sequence = |id: &str| id.strip_prefix('$').and_then(|raw| raw.parse::<u64>().ok());
    match (sequence(candidate), sequence(current)) {
        (Some(candidate), Some(current)) => candidate < current,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => candidate < current,
    }
}

impl TopologySnapshot {
    /// Build a nested snapshot and join agents by the complete
    /// `(host, socket, pane_id)` identity. An agent without endpoint metadata
    /// is joined only when its pane id has exactly one candidate on that host.
    #[must_use]
    pub fn build(
        generated_at: OffsetDateTime,
        inputs: Vec<TopologyInput>,
        agents: Vec<Agent>,
    ) -> Self {
        let mut capabilities = Vec::new();
        let mut session_meta = HashMap::new();
        let mut pane_rows = Vec::new();
        let mut unmapped_panes = Vec::new();
        for input in inputs {
            let hierarchy_supported = input.capabilities.session
                != HierarchyCapability::Unsupported
                && input.capabilities.window != HierarchyCapability::Unsupported
                && input.capabilities.pane != HierarchyCapability::Unsupported;
            if !capabilities
                .iter()
                .any(|caps: &BackendTopologyCapabilities| caps.host == input.host)
            {
                capabilities.push(input.capabilities);
            }
            for session in input.sessions {
                let key = SessionKey {
                    endpoint: session.endpoint,
                    session_id: session.session_id,
                };
                session_meta.insert(key, (session.name, session.attached_clients));
            }
            if hierarchy_supported {
                pane_rows.extend(input.panes.into_iter().map(|pane| (input.host, pane)));
            } else {
                unmapped_panes.extend(input.panes);
            }
        }
        append_cmux_hook_panes(&mut pane_rows, &agents, &mut capabilities);
        let pane_rows = fold_session_groups(pane_rows);
        capabilities.sort_by_key(|caps| caps.host);

        let candidate_counts = pane_candidate_counts(&pane_rows);
        let mut claimed_agents = HashSet::new();
        let mut sessions: BTreeMap<SessionKey, SessionBuilder> = BTreeMap::new();

        for (host, pane) in pane_rows {
            let pane_key = PaneKey::from_pane(host, &pane);
            let window_key = pane_key.window.clone();
            let session_key = window_key.session.clone();

            let agent_index =
                matching_agent(host, &pane, &agents, &claimed_agents, &candidate_counts);
            let agent = agent_index.map(|index| {
                claimed_agents.insert(index);
                agents[index].clone()
            });

            let session = sessions.entry(session_key.clone()).or_insert_with(|| {
                let (name, attached_clients) = session_meta
                    .get(&session_key)
                    .cloned()
                    .unwrap_or_else(|| (pane.session.clone(), None));
                SessionBuilder {
                    key: session_key,
                    name,
                    attached_clients,
                    windows: BTreeMap::new(),
                }
            });
            let window = session
                .windows
                .entry(window_key.clone())
                .or_insert_with(|| WindowBuilder {
                    key: window_key,
                    name: if pane.window_name.is_empty() {
                        format!("window {}", pane.window_index)
                    } else {
                        pane.window_name.clone()
                    },
                    index: pane.window_index.clone(),
                    panes: Vec::new(),
                });
            window.panes.push(PaneNode {
                key: pane_key,
                index: pane.pane_index,
                tty: pane.tty,
                current_command: pane.current_command,
                title: pane.title,
                cwd: pane.current_path,
                pane_pid: pane.pane_pid,
                agent,
                agent_alias: pane.agent_alias,
            });
        }

        let sessions = sessions.into_values().map(SessionBuilder::finish).collect();
        let unassigned_agents = agents
            .into_iter()
            .enumerate()
            .filter_map(|(index, agent)| (!claimed_agents.contains(&index)).then_some(agent))
            .collect();

        Self {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            generated_at,
            capabilities,
            sessions,
            unmapped_panes,
            unassigned_agents,
        }
    }

    #[must_use]
    pub fn find(&self, key: &TopologyNodeKey) -> Option<TopologyNodeRef<'_>> {
        match key {
            TopologyNodeKey::Session(wanted) => self
                .sessions
                .iter()
                .find(|session| &session.key == wanted)
                .map(TopologyNodeRef::Session),
            TopologyNodeKey::Window(wanted) => self
                .sessions
                .iter()
                .flat_map(|session| &session.windows)
                .find(|window| &window.key == wanted)
                .map(TopologyNodeRef::Window),
            TopologyNodeKey::Pane(wanted) => self
                .sessions
                .iter()
                .flat_map(|session| &session.windows)
                .flat_map(|window| &window.panes)
                .find(|pane| &pane.key == wanted)
                .map(TopologyNodeRef::Pane),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TopologyNodeRef<'a> {
    Session(&'a SessionNode),
    Window(&'a WindowNode),
    Pane(&'a PaneNode),
}

#[derive(Debug)]
struct SessionBuilder {
    key: SessionKey,
    name: String,
    attached_clients: Option<u32>,
    windows: BTreeMap<WindowKey, WindowBuilder>,
}

impl SessionBuilder {
    fn finish(self) -> SessionNode {
        let mut windows: Vec<_> = self
            .windows
            .into_values()
            .map(WindowBuilder::finish)
            .collect();
        windows.sort_by(|left, right| {
            numeric_index(&left.index)
                .cmp(&numeric_index(&right.index))
                .then_with(|| left.key.cmp(&right.key))
        });
        let mut states = StateDistribution::default();
        for window in &windows {
            states.extend(&window.states);
        }
        SessionNode {
            key: self.key,
            name: self.name,
            attached_clients: self.attached_clients,
            states,
            windows,
        }
    }
}

#[derive(Debug)]
struct WindowBuilder {
    key: WindowKey,
    name: String,
    index: String,
    panes: Vec<PaneNode>,
}

impl WindowBuilder {
    fn finish(mut self) -> WindowNode {
        self.panes.sort_by(|left, right| {
            numeric_index(&left.index)
                .cmp(&numeric_index(&right.index))
                .then_with(|| left.key.cmp(&right.key))
        });
        let cwd = common_cwd(&self.panes);
        let mut states = StateDistribution::default();
        for agent in self.panes.iter().filter_map(|pane| pane.agent.as_ref()) {
            states.add(agent.state);
        }
        WindowNode {
            key: self.key,
            name: self.name,
            index: self.index,
            cwd,
            states,
            panes: self.panes,
            // Both are filled in by whoever holds the durable pipeline Run;
            // the tmux topology cannot know either.
            completion: None,
            pipeline_run: None,
        }
    }
}

fn numeric_index(value: &str) -> u64 {
    value.parse().unwrap_or(u64::MAX)
}

fn common_cwd(panes: &[PaneNode]) -> Option<String> {
    let first = panes.first()?.cwd.as_str();
    (!first.is_empty() && panes.iter().all(|pane| pane.cwd == first)).then(|| first.to_string())
}

fn stable_session_id(pane: &PaneInfo) -> String {
    if pane.session_id.is_empty() {
        pane.session.clone()
    } else {
        pane.session_id.clone()
    }
}

fn stable_window_id(pane: &PaneInfo) -> String {
    if pane.window_id.is_empty() {
        pane.window_index.clone()
    } else {
        pane.window_id.clone()
    }
}

fn default_socket(host: HostKind) -> &'static str {
    match host {
        HostKind::Tmux | HostKind::Rmux => "default",
        HostKind::Cmux => "cmux",
        HostKind::Zellij => "zellij",
        HostKind::Herdr => "herdr",
    }
}

fn endpoint_for(host: HostKind, pane: &PaneInfo) -> BackendEndpoint {
    let socket = pane.socket.as_deref().map_or_else(
        || default_socket(host).to_string(),
        |socket| pane_endpoint_identity(Some(&pane.pane_id), socket),
    );
    BackendEndpoint { host, socket }
}

fn pane_candidate_counts(panes: &[(HostKind, PaneInfo)]) -> HashMap<(HostKind, String), usize> {
    let mut counts = HashMap::new();
    for (host, pane) in panes {
        *counts.entry((*host, pane.pane_id.clone())).or_default() += 1;
    }
    counts
}

/// Turn authoritative cmux hook metadata into a topology pane when the
/// environment-only backend cannot enumerate that surface. This is not a fake
/// durable Work node: the hook supplies the exact surface UUID, workspace UUID,
/// and socket endpoint, so the row is an execution binding with the same shape
/// a future full cmux inventory scan will produce.
fn append_cmux_hook_panes(
    panes: &mut Vec<(HostKind, PaneInfo)>,
    agents: &[Agent],
    capabilities: &mut Vec<BackendTopologyCapabilities>,
) {
    for agent in agents {
        let Some(surface) = agent
            .surface
            .as_ref()
            .filter(|surface| surface.kind == SurfaceKind::Cmux)
        else {
            continue;
        };
        let Some(workspace) = surface
            .workspace
            .as_deref()
            .filter(|workspace| !workspace.trim().is_empty())
        else {
            continue;
        };
        let Some(pane_id) = agent.pane.as_deref() else {
            continue;
        };
        if pane_id != crate::backend::cmux::namespace_pane_id(&surface.id) {
            continue;
        }
        let wanted_endpoint = agent.tmux_socket.as_deref().map_or_else(
            || default_socket(HostKind::Cmux).to_string(),
            |socket| pane_endpoint_identity(Some(pane_id), socket),
        );
        let already_observed = panes.iter().any(|(host, pane)| {
            *host == HostKind::Cmux
                && pane.pane_id == pane_id
                && endpoint_for(*host, pane).socket == wanted_endpoint
        });
        if already_observed {
            continue;
        }
        if !capabilities.iter().any(|caps| caps.host == HostKind::Cmux) {
            capabilities.push(BackendTopologyCapabilities::for_host(HostKind::Cmux));
        }
        panes.push((
            HostKind::Cmux,
            PaneInfo {
                session_group: None,
                agent_role: None,
                agent_alias: None,
                workspace_id: None,
                work_id: None,
                socket: agent.tmux_socket.clone(),
                pane_id: pane_id.to_string(),
                session_id: workspace.to_string(),
                session: workspace.to_string(),
                window_id: workspace.to_string(),
                window_name: String::new(),
                window_index: "0".into(),
                pane_index: "0".into(),
                tty: String::new(),
                current_command: agent.kind.to_string(),
                title: String::new(),
                pane_pid: agent.pid.unwrap_or(0),
                current_path: agent.cwd.clone().unwrap_or_default(),
            },
        ));
    }
}

fn matching_agent(
    host: HostKind,
    pane: &PaneInfo,
    agents: &[Agent],
    claimed: &HashSet<usize>,
    candidate_counts: &HashMap<(HostKind, String), usize>,
) -> Option<usize> {
    let endpoint = endpoint_for(host, pane);
    agents
        .iter()
        .enumerate()
        .filter(|(index, agent)| {
            !claimed.contains(index) && agent.pane.as_deref() == Some(&pane.pane_id)
        })
        .filter(|_| pane_id_host_kind(&pane.pane_id).is_none_or(|kind| kind == host))
        .filter(|(_, agent)| match agent.tmux_socket.as_deref() {
            Some(socket) => pane_endpoint_identity(Some(&pane.pane_id), socket) == endpoint.socket,
            None => {
                candidate_counts
                    .get(&(host, pane.pane_id.clone()))
                    .copied()
                    .unwrap_or(0)
                    == 1
            }
        })
        .max_by_key(|(_, agent)| (agent.state != AgentState::Stopped, agent.last_activity_at))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentKind, SurfaceRef};
    use crate::process_tree::WorkloadSummary;

    struct IsolatedTmuxServers(Vec<std::path::PathBuf>);

    impl Drop for IsolatedTmuxServers {
        fn drop(&mut self) {
            for socket in &self.0 {
                let _ = std::process::Command::new(crate::tmux::tmux_binary())
                    .args(["-S", socket.to_string_lossy().as_ref(), "kill-server"])
                    .status();
            }
        }
    }

    fn pane(socket: &str, session_name: &str, window_name: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_role: None,
            agent_alias: None,
            workspace_id: None,
            work_id: None,
            pane_id: "%1".into(),
            session_id: "$1".into(),
            session: session_name.into(),
            window_id: "@1".into(),
            window_name: window_name.into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "codex".into(),
            title: String::new(),
            current_path: "/repo".into(),
            pane_pid: 10,
            socket: Some(socket.into()),
        }
    }

    fn agent(agent_session_id: &str, socket: Option<&str>) -> Agent {
        let now = OffsetDateTime::UNIX_EPOCH;
        Agent {
            kind: AgentKind::Codex,
            session_id: agent_session_id.into(),
            surface: None,
            pane: Some("%1".into()),
            tmux_socket: socket.map(Into::into),
            tmux_session: None,
            cwd: Some("/repo".into()),
            pid: None,
            workload: WorkloadSummary::default(),
            subagents: Vec::new(),
            state: AgentState::Working,
            last_prompt: None,
            last_prompt_at: None,
            last_response: None,
            recap: None,
            ai_title: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: now,
            last_activity_at: now,
            state_entered_at: now,
        }
    }

    /// One pane of a session group, as it is reported for `member`.
    fn grouped(session_id: &str, session: &str, group: &str) -> (HostKind, PaneInfo) {
        let mut pane = pane("default", session, "w0");
        pane.session_id = session_id.into();
        pane.session_group = Some(group.into());
        (HostKind::Tmux, pane)
    }

    #[test]
    fn folding_keeps_the_original_session_after_it_is_renamed() {
        // tmux leaves `session_group` as `base` after the original session is
        // renamed. The original still survives because it has the older
        // numeric id; relying on the stale group name would select no member.
        for rows in [
            vec![
                grouped("$99", "renamed", "base"),
                grouped("$107", "base~view~9", "base"),
            ],
            vec![
                grouped("$107", "base~view~9", "base"),
                grouped("$99", "renamed", "base"),
            ],
        ] {
            let folded = fold_session_groups(rows);
            assert_eq!(folded.len(), 1, "the duplicate row must be dropped");
            assert_eq!(folded[0].1.session, "renamed");
            assert_eq!(folded[0].1.session_id, "$99");
        }
    }

    #[test]
    fn folding_is_stable_when_the_namesake_session_is_gone() {
        // A view can outlive the session it was opened from. There is then no
        // right answer, only a stable one: the same member every tick, not
        // whichever the scan reached first.
        let a = grouped("$9", "view~9~base", "base");
        let b = grouped("$107", "view~8~base", "base");
        let forward = fold_session_groups(vec![a.clone(), b.clone()]);
        let reverse = fold_session_groups(vec![b, a]);
        assert_eq!(forward.len(), 1);
        assert_eq!(forward[0].1.session_id, "$9");
        assert_eq!(reverse[0].1.session_id, forward[0].1.session_id);
    }

    #[test]
    fn folding_leaves_ungrouped_sessions_alone() {
        // Two ordinary sessions are two workspaces, however similar they look.
        let mut one = pane("default", "main", "w0");
        one.session_id = "$0".into();
        let mut two = pane("default", "other", "w0");
        two.session_id = "$1".into();
        let folded = fold_session_groups(vec![(HostKind::Tmux, one), (HostKind::Tmux, two)]);
        assert_eq!(folded.len(), 2);
    }

    #[test]
    fn folding_does_not_reach_across_servers() {
        // Session ids and group names are only unique per tmux server, so a
        // group named `base` on one socket says nothing about `base` on
        // another. Folding them together would merge two real workspaces.
        let (_, mut other) = grouped("$0", "base", "base");
        other.socket = Some("amux".into());
        let folded =
            fold_session_groups(vec![grouped("$0", "base", "base"), (HostKind::Tmux, other)]);
        assert_eq!(folded.len(), 2);
    }

    #[test]
    fn a_grouped_workspace_is_one_tree_with_its_agent_attached() {
        // The user-visible point. Before folding, `list-panes -a` reported this
        // pane once per member, the topology showed the workspace twice, and
        // `matching_agent` claimed the agent for whichever copy was scanned
        // first — leaving the other tree a bare pane.
        let snapshot = TopologySnapshot::build(
            OffsetDateTime::UNIX_EPOCH,
            vec![TopologyInput {
                host: HostKind::Tmux,
                capabilities: BackendTopologyCapabilities::for_host(HostKind::Tmux),
                panes: vec![
                    grouped("$0", "base", "base").1,
                    grouped("$1", "view~9~base", "base").1,
                ],
                sessions: Vec::new(),
            }],
            vec![agent("agent-base", Some("default"))],
        );

        assert_eq!(snapshot.sessions.len(), 1, "one workspace, one tree");
        let session = &snapshot.sessions[0];
        assert_eq!(session.name, "base");
        assert_eq!(session.windows.len(), 1);
        assert_eq!(session.windows[0].panes.len(), 1);
        assert!(
            session.windows[0].panes[0].agent.is_some(),
            "the surviving tree keeps the agent"
        );
    }

    #[test]
    fn identical_native_ids_on_two_sockets_never_collide() {
        let snapshot = TopologySnapshot::build(
            OffsetDateTime::UNIX_EPOCH,
            vec![TopologyInput::new(
                HostKind::Tmux,
                vec![pane("default", "main", "one"), pane("amux", "other", "two")],
            )],
            vec![
                agent("agent-main", Some("default")),
                agent("agent-amux", Some("amux")),
            ],
        );

        assert_eq!(snapshot.sessions.len(), 2);
        let sessions: HashMap<_, _> = snapshot
            .sessions
            .iter()
            .map(|session| {
                let agent = session.windows[0].panes[0]
                    .agent
                    .as_ref()
                    .expect("socket-specific agent");
                (
                    session.key.endpoint.socket.as_str(),
                    agent.session_id.as_str(),
                )
            })
            .collect();
        assert_eq!(sessions.get("default"), Some(&"agent-main"));
        assert_eq!(sessions.get("amux"), Some(&"agent-amux"));
        assert_ne!(snapshot.sessions[0].key, snapshot.sessions[1].key);
        assert_ne!(
            snapshot.sessions[0].windows[0].panes[0].key,
            snapshot.sessions[1].windows[0].panes[0].key
        );
    }

    #[test]
    fn endpointless_agent_is_not_guessed_across_duplicate_pane_ids() {
        let snapshot = TopologySnapshot::build(
            OffsetDateTime::UNIX_EPOCH,
            vec![TopologyInput::new(
                HostKind::Tmux,
                vec![pane("default", "main", "one"), pane("amux", "other", "two")],
            )],
            vec![agent("ambiguous", None)],
        );

        assert!(snapshot
            .sessions
            .iter()
            .all(|session| session.windows[0].panes[0].agent.is_none()));
        assert_eq!(snapshot.unassigned_agents.len(), 1);
        assert_eq!(snapshot.unassigned_agents[0].session_id, "ambiguous");
    }

    #[test]
    fn parent_state_distribution_bubbles_attention() {
        let mut waiting = agent("waiting", Some("default"));
        waiting.state = AgentState::WaitingChoice;
        let snapshot = TopologySnapshot::build(
            OffsetDateTime::UNIX_EPOCH,
            vec![TopologyInput::new(
                HostKind::Tmux,
                vec![pane("default", "main", "one")],
            )],
            vec![waiting],
        );

        let session = &snapshot.sessions[0];
        assert_eq!(session.states.waiting_choice, 1);
        assert!(session.states.needs_attention());
        assert_eq!(session.windows[0].states.waiting_choice, 1);
    }

    #[test]
    fn unsupported_hierarchy_is_reported_without_fake_ancestors() {
        let mut input =
            TopologyInput::new(HostKind::Zellij, vec![pane("zellij", "session", "tab")]);
        input.capabilities.session = HierarchyCapability::Unsupported;
        let snapshot = TopologySnapshot::build(OffsetDateTime::UNIX_EPOCH, vec![input], Vec::new());
        assert!(snapshot.sessions.is_empty());
        assert_eq!(snapshot.unmapped_panes.len(), 1);
        assert_eq!(
            snapshot.capabilities[0].session,
            HierarchyCapability::Unsupported
        );
    }

    #[test]
    fn cmux_hook_metadata_builds_workspace_topology_without_daemon_env() {
        let mut hooked = agent("cmux-agent", Some("/tmp/cmux-debug.sock"));
        hooked.pane = Some("cmux:surface-7".into());
        hooked.surface = Some(SurfaceRef {
            kind: SurfaceKind::Cmux,
            id: "surface-7".into(),
            workspace: Some("workspace-2".into()),
        });

        let snapshot =
            TopologySnapshot::build(OffsetDateTime::UNIX_EPOCH, Vec::new(), vec![hooked]);

        assert_eq!(snapshot.sessions.len(), 1);
        let session = &snapshot.sessions[0];
        assert_eq!(session.key.endpoint.host, HostKind::Cmux);
        assert_eq!(session.key.endpoint.socket, "/tmp/cmux-debug.sock");
        assert_eq!(session.key.session_id, "workspace-2");
        assert_eq!(session.windows[0].panes[0].key.pane_id, "cmux:surface-7");
        assert_eq!(
            session.windows[0].panes[0]
                .agent
                .as_ref()
                .map(|agent| agent.session_id.as_str()),
            Some("cmux-agent"),
        );
        assert!(snapshot.unassigned_agents.is_empty());
    }

    /// Exercise the contract against two real isolated tmux servers. Each
    /// server starts its counters from the same native ids (`$0/@0/%0`), so
    /// socket ancestry is the only thing preventing a collision.
    #[test]
    fn isolated_tmux_sockets_with_identical_native_ids_stay_distinct() {
        if !std::process::Command::new(crate::tmux::tmux_binary())
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }

        let dir = tempfile::tempdir().expect("temporary socket directory");
        let sockets = [dir.path().join("one"), dir.path().join("two")];
        let _servers = IsolatedTmuxServers(sockets.to_vec());
        let mut panes = Vec::new();
        for socket in &sockets {
            let socket_arg = socket.to_string_lossy();
            let created = std::process::Command::new(crate::tmux::tmux_binary())
                .args([
                    "-S",
                    socket_arg.as_ref(),
                    "new-session",
                    "-d",
                    "-s",
                    "same",
                    "-n",
                    "same",
                ])
                .status()
                .expect("start isolated tmux server");
            assert!(created.success());
            let output = std::process::Command::new(crate::tmux::tmux_binary())
                .args([
                    "-S",
                    socket_arg.as_ref(),
                    "list-panes",
                    "-a",
                    "-F",
                    crate::tmux::PANE_FMT,
                ])
                .output()
                .expect("list isolated panes");
            assert!(output.status.success());
            let stdout = String::from_utf8(output.stdout).expect("tmux UTF-8 output");
            panes.extend(crate::tmux::parse_pane_lines_for_socket(
                &stdout,
                socket.file_name().and_then(|name| name.to_str()),
            ));
        }

        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].session_id, panes[1].session_id);
        assert_eq!(panes[0].window_id, panes[1].window_id);
        assert_eq!(panes[0].pane_id, panes[1].pane_id);

        let snapshot = TopologySnapshot::build(
            OffsetDateTime::UNIX_EPOCH,
            vec![TopologyInput::new(HostKind::Tmux, panes)],
            Vec::new(),
        );
        assert_eq!(snapshot.sessions.len(), 2);
        assert_ne!(snapshot.sessions[0].key, snapshot.sessions[1].key);
        assert_ne!(
            snapshot.sessions[0].windows[0].panes[0].key,
            snapshot.sessions[1].windows[0].panes[0].key
        );
    }
}
