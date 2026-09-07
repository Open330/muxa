//! Central host → session → window → pane(agent) Fleet TUI.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa::ask::{AskEntry, AskStatus};
use muxa::collaboration::{CollaborationRequest, NewRequest, RequestKind, RequestStatus, WorkMode};
use muxa::config::{
    WatchCollaborationMode, WatchLayout, WatchSortKey, WatchTheme, WatchTreeExpansion, WatchView,
};
use muxa::fleet::{
    FleetCommandResult, FleetHostSnapshot, FleetHostState, FleetOperation, FleetSnapshot,
    FleetWindowCapture, GlobalPaneRef,
};
use muxa::ipc::Client;
use muxa::{
    AgentState, Config, HostKind, PaneKey, PaneNode, SessionKey, SessionNode, StateDistribution,
    TopologyInput, TopologySnapshot, WindowKey, WindowNode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{Frame, Terminal};
use time::OffsetDateTime;
use tokio::sync::mpsc;

const POLL_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const STREAM_FALLBACK_INTERVAL: Duration = Duration::from_secs(15);
const STREAM_COALESCE_INTERVAL: Duration = Duration::from_millis(75);
const STREAM_MIN_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const WINDOW_CAPTURE_INTERVAL: Duration = Duration::from_secs(2);
const INPUT_POLL: Duration = Duration::from_millis(50);
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_secs(1);

type FleetTerminal = Terminal<CrosstermBackend<Stdout>>;

pub(crate) fn uses_native_local_watch(snapshot: &FleetSnapshot) -> bool {
    snapshot.hosts.len() == 1 && snapshot.hosts[0].local
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NodeKey {
    Host(String),
    Session {
        host: String,
        key: SessionKey,
    },
    Window {
        host: String,
        key: WindowKey,
    },
    Pane {
        host: String,
        key: PaneKey,
    },
    PanelessAgent {
        host: String,
        kind: String,
        session_id: String,
    },
}

impl NodeKey {
    fn host(&self) -> &str {
        match self {
            Self::Host(host)
            | Self::Session { host, .. }
            | Self::Window { host, .. }
            | Self::Pane { host, .. }
            | Self::PanelessAgent { host, .. } => host,
        }
    }

    fn parent(&self) -> Option<Self> {
        match self {
            Self::Host(_) => None,
            Self::Session { host, .. } | Self::PanelessAgent { host, .. } => {
                Some(Self::Host(host.clone()))
            }
            Self::Window { host, key } => Some(Self::Session {
                host: host.clone(),
                key: key.session.clone(),
            }),
            Self::Pane { host, key } => Some(Self::Window {
                host: host.clone(),
                key: key.window.clone(),
            }),
        }
    }

    fn ancestors(&self) -> Vec<Self> {
        let mut ancestors = Vec::new();
        let mut current = self.parent();
        while let Some(parent) = current {
            current = parent.parent();
            ancestors.push(parent);
        }
        ancestors
    }

    fn is_parent(&self) -> bool {
        !matches!(self, Self::Pane { .. } | Self::PanelessAgent { .. })
    }
}

#[derive(Debug, Clone)]
struct TreeRow {
    key: NodeKey,
    depth: usize,
    label: String,
    detail: String,
    state: Option<AgentState>,
    attention: usize,
    children: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search,
    Message,
    Ask,
    Reply,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum MailboxTab {
    #[default]
    Incoming,
    Sent,
}

#[derive(Debug, Clone, Default)]
struct MailboxState {
    open: bool,
    loading: bool,
    host: Option<String>,
    pane: Option<PaneKey>,
    incoming: Vec<CollaborationRequest>,
    sent: Vec<CollaborationRequest>,
    selected: usize,
    tab: MailboxTab,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SkillEditorField {
    #[default]
    Name,
    Prompt,
}

#[derive(Debug, Clone, Default)]
struct SkillEditor {
    name: String,
    prompt: String,
    field: SkillEditorField,
}

#[allow(clippy::struct_excessive_bools)] // independent TUI state flags
struct App {
    snapshot: FleetSnapshot,
    topologies: HashMap<String, TopologySnapshot>,
    topology_versions: HashMap<String, (Option<muxa::NodeId>, u64, OffsetDateTime)>,
    rows: Vec<TreeRow>,
    selected: usize,
    expanded: HashSet<NodeKey>,
    query: String,
    attention_only: bool,
    mode: InputMode,
    composer: String,
    popup: Option<String>,
    help: bool,
    status: Option<(String, Instant)>,
    selector: Option<String>,
    window_capture: Option<(String, FleetWindowCapture)>,
    window_capture_pending: Option<(String, WindowKey)>,
    last_window_capture: Option<Instant>,
    theme: WatchTheme,
    message_skills: BTreeMap<String, String>,
    skill_palette: Option<crate::message_skill::Palette>,
    layout: WatchLayout,
    view: WatchView,
    expansion: WatchTreeExpansion,
    expansion_initialized: bool,
    sort: WatchSortKey,
    show_paneless: bool,
    ask_agent: String,
    ask_entries: Vec<AskEntry>,
    ask_selected: usize,
    ask_panel: bool,
    message_kind: RequestKind,
    message_mode: WatchCollaborationMode,
    mailbox: MailboxState,
    reply_request_id: Option<String>,
    skill_editor: Option<SkillEditor>,
    skill_delete_confirm: Option<String>,
}

impl App {
    fn new(
        selector: Option<String>,
        theme: WatchTheme,
        message_skills: BTreeMap<String, String>,
        layout: WatchLayout,
        view: WatchView,
        expansion: WatchTreeExpansion,
        sort: WatchSortKey,
    ) -> Self {
        Self {
            snapshot: FleetSnapshot {
                generated_at: OffsetDateTime::now_utc(),
                hosts: Vec::new(),
            },
            topologies: HashMap::new(),
            topology_versions: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
            query: String::new(),
            attention_only: false,
            mode: InputMode::Normal,
            composer: String::new(),
            popup: None,
            help: false,
            status: None,
            selector,
            window_capture: None,
            window_capture_pending: None,
            last_window_capture: None,
            theme,
            message_skills,
            skill_palette: None,
            layout,
            view,
            expansion,
            expansion_initialized: false,
            sort,
            show_paneless: false,
            ask_agent: "claude".into(),
            ask_entries: Vec::new(),
            ask_selected: 0,
            ask_panel: false,
            message_kind: RequestKind::Question,
            message_mode: WatchCollaborationMode::ReadOnly,
            mailbox: MailboxState::default(),
            reply_request_id: None,
            skill_editor: None,
            skill_delete_confirm: None,
        }
    }

    fn apply_snapshot(&mut self, mut snapshot: FleetSnapshot) {
        let selected = self.selected_key().cloned();
        snapshot.hosts.sort_by(|left, right| {
            right
                .local
                .cmp(&left.local)
                .then_with(|| compare_hosts(left, right, self.sort))
                .then_with(|| left.alias.cmp(&right.alias))
        });
        let mut topologies = HashMap::new();
        let mut versions = HashMap::new();
        for host in &snapshot.hosts {
            let Some(remote) = &host.remote else {
                continue;
            };
            let version = (host.node_id.clone(), remote.revision, remote.observed_at);
            let topology = if self.topology_versions.get(&host.alias) == Some(&version) {
                self.topologies.remove(&host.alias)
            } else {
                host_topology(host)
            };
            if let Some(topology) = topology {
                topologies.insert(host.alias.clone(), topology);
                versions.insert(host.alias.clone(), version);
            }
        }
        self.topologies = topologies;
        self.topology_versions = versions;
        self.snapshot = snapshot;
        if !self.expansion_initialized && !self.snapshot.hosts.is_empty() {
            self.initialize_expansion();
            self.expansion_initialized = true;
        }
        self.rebuild_rows(selected.as_ref());
    }

    fn initialize_expansion(&mut self) {
        let hosts = if self.expansion == WatchTreeExpansion::Always {
            self.snapshot.hosts.iter().collect::<Vec<_>>()
        } else if self.expansion == WatchTreeExpansion::Focus {
            self.snapshot.hosts.first().into_iter().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for host in hosts {
            let host_key = NodeKey::Host(host.alias.clone());
            self.expanded.insert(host_key);
            if self.view == WatchView::Session {
                continue;
            }
            let Some(topology) = self.topologies.get(&host.alias) else {
                continue;
            };
            let sessions = if self.expansion == WatchTreeExpansion::Always {
                topology.sessions.iter().collect::<Vec<_>>()
            } else {
                topology.sessions.first().into_iter().collect::<Vec<_>>()
            };
            for session in sessions {
                self.expanded.insert(NodeKey::Session {
                    host: host.alias.clone(),
                    key: session.key.clone(),
                });
                if self.view != WatchView::Pane {
                    continue;
                }
                let windows = if self.expansion == WatchTreeExpansion::Always {
                    session.windows.iter().collect::<Vec<_>>()
                } else {
                    session.windows.first().into_iter().collect::<Vec<_>>()
                };
                for window in windows {
                    self.expanded.insert(NodeKey::Window {
                        host: host.alias.clone(),
                        key: window.key.clone(),
                    });
                }
            }
        }
    }

    fn selected_key(&self) -> Option<&NodeKey> {
        self.rows.get(self.selected).map(|row| &row.key)
    }

    fn selected_host(&self) -> Option<&FleetHostSnapshot> {
        let alias = self.selected_key()?.host();
        self.snapshot.hosts.iter().find(|host| host.alias == alias)
    }

    fn selected_pane(&self) -> Option<(String, PaneKey)> {
        match self.selected_key()? {
            NodeKey::Pane { host, key } => Some((host.clone(), key.clone())),
            _ => None,
        }
    }

    /// Resolve a message target without forcing the operator to descend a
    /// single-child tree. Parent nodes choose the lowest stable window/pane
    /// index that currently owns a live agent.
    fn selected_message_pane(&self) -> Option<(String, PaneKey)> {
        let selected = self.selected_key()?;
        let host_alias = selected.host();
        let topology = self.topologies.get(host_alias)?;
        let pane = match selected {
            NodeKey::Pane { key, .. } => topology
                .sessions
                .iter()
                .flat_map(|session| &session.windows)
                .flat_map(|window| &window.panes)
                .find(|pane| pane.key == *key && pane_has_live_agent(pane)),
            NodeKey::Window { key, .. } => topology
                .sessions
                .iter()
                .flat_map(|session| &session.windows)
                .find(|window| window.key == *key)
                .and_then(lowest_live_pane),
            NodeKey::Session { key, .. } => topology
                .sessions
                .iter()
                .find(|session| session.key == *key)
                .and_then(lowest_live_session_pane),
            NodeKey::Host(_) | NodeKey::PanelessAgent { .. } => None,
        }?;
        Some((host_alias.to_string(), pane.key.clone()))
    }

    /// Return a Work identity only when the selected pane carries the complete
    /// managed pair. Partially stamped and legacy Fleet snapshots keep the
    /// pre-existing unscoped message behavior.
    fn work_identity_for_message_pane(
        &self,
        host_alias: &str,
        key: &PaneKey,
    ) -> (Option<String>, Option<String>) {
        self.snapshot
            .hosts
            .iter()
            .find(|host| host.alias == host_alias)
            .and_then(|host| host.remote.as_ref())
            .and_then(|remote| {
                remote
                    .panes
                    .iter()
                    .find(|pane| PaneKey::from_pane(key.window.session.endpoint.host, pane) == *key)
            })
            .and_then(|pane| Some((pane.workspace_id.clone()?, pane.work_id.clone()?)))
            .map_or((None, None), |(workspace_id, work_id)| {
                (Some(workspace_id), Some(work_id))
            })
    }

    fn selected_window(&self) -> Option<(String, WindowKey)> {
        match self.selected_key()? {
            NodeKey::Window { host, key } => Some((host.clone(), key.clone())),
            _ => None,
        }
    }

    fn rebuild_rows(&mut self, preserve: Option<&NodeKey>) {
        let preserve = preserve.cloned().or_else(|| self.selected_key().cloned());
        let swarm_selected = (self.layout == WatchLayout::Swarm).then(|| {
            let nodes = all_swarm_keys(
                &self.snapshot,
                &self.topologies,
                &self.query,
                self.attention_only,
                self.sort,
                self.show_paneless,
            );
            preserve
                .clone()
                .filter(|key| nodes.contains(key))
                .or_else(|| nodes.into_iter().next())
        });
        // `selected` indexes the backing tree even in Swarm mode. Its leaf
        // therefore needs visible ancestors regardless of tree-expansion
        // policy; those structural rows are not rendered in the Swarm table.
        if let Some(Some(key)) = &swarm_selected {
            self.expanded.extend(key.ancestors());
        }
        self.rows = build_rows(
            &self.snapshot,
            &self.topologies,
            &self.expanded,
            &self.query,
            self.attention_only,
            self.sort,
            self.show_paneless,
        );
        if self.layout == WatchLayout::Swarm {
            if let Some(Some(key)) = swarm_selected {
                if let Some(index) = self.rows.iter().position(|row| row.key == key) {
                    self.selected = index;
                    return;
                }
            }
        } else if let Some(key) = preserve {
            if let Some(index) = self.rows.iter().position(|row| row.key == key) {
                self.selected = index;
                return;
            }
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn focus_key(&mut self, key: NodeKey) {
        self.select_key(key, false);
    }

    fn reveal_key(&mut self, key: NodeKey) {
        self.select_key(key, true);
    }

    fn select_key(&mut self, key: NodeKey, reveal: bool) {
        if self.expansion == WatchTreeExpansion::Focus {
            self.expanded.clear();
        }
        if reveal || self.expansion == WatchTreeExpansion::Focus {
            for ancestor in key.ancestors() {
                self.expanded.insert(ancestor);
            }
        }
        if self.expansion == WatchTreeExpansion::Focus
            && match &key {
                NodeKey::Host(_) => true,
                NodeKey::Session { .. } => self.view != WatchView::Session,
                NodeKey::Window { .. } => self.view == WatchView::Pane,
                NodeKey::Pane { .. } | NodeKey::PanelessAgent { .. } => false,
            }
        {
            self.expanded.insert(key.clone());
        }
        self.rebuild_rows(Some(&key));
    }

    fn move_vertical(&mut self, delta: isize) {
        if self.layout == WatchLayout::Swarm {
            self.move_swarm(delta);
        } else if self.expansion == WatchTreeExpansion::Focus {
            self.move_focus_sibling(delta);
        } else {
            self.move_row(delta);
        }
    }

    fn move_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = wrapped_step(self.selected, self.rows.len(), delta);
        if let Some(key) = self.selected_key().cloned() {
            self.focus_key(key);
        }
    }

    /// Match native watch's focus-mode navigation: descendants are context,
    /// while vertical movement stays in the selected node's sibling group.
    /// If that group contains only one node, bubble to the parent's siblings
    /// so a singleton pane/window chain never traps the cursor.
    fn move_focus_sibling(&mut self, delta: isize) {
        let Some(mut anchor) = self.selected_key().cloned() else {
            return;
        };
        loop {
            let parent = anchor.parent();
            let candidates = self
                .rows
                .iter()
                .filter(|row| row.key.parent() == parent)
                .map(|row| row.key.clone())
                .collect::<Vec<_>>();
            let Some(current) = candidates.iter().position(|key| key == &anchor) else {
                return;
            };
            if candidates.len() != 1 {
                let next = wrapped_step(current, candidates.len(), delta);
                self.focus_key(candidates[next].clone());
                return;
            }
            let Some(parent) = parent else {
                return;
            };
            anchor = parent;
        }
    }

    /// Uppercase `J/K` preserves Fleet's fast global jump between actionable
    /// panes without changing the familiar native-watch meaning of `j/k`.
    fn jump_pane(&mut self, delta: isize) {
        let panes = all_pane_keys(
            &self.snapshot,
            &self.topologies,
            &self.query,
            self.attention_only,
            self.sort,
        );
        if panes.is_empty() {
            self.move_row(delta);
            return;
        }
        let current = self.selected_key();
        let index = current
            .and_then(|key| panes.iter().position(|pane| pane == key))
            .or_else(|| {
                current.and_then(|key| {
                    let host = key.host();
                    panes.iter().position(|pane| pane.host() == host)
                })
            });
        let next = match index {
            Some(index) => wrapped_step(index, panes.len(), delta),
            None if delta.is_negative() => panes.len() - 1,
            None => 0,
        };
        self.reveal_key(panes[next].clone());
    }

    fn move_swarm(&mut self, delta: isize) {
        let nodes = all_swarm_keys(
            &self.snapshot,
            &self.topologies,
            &self.query,
            self.attention_only,
            self.sort,
            self.show_paneless,
        );
        if nodes.is_empty() {
            return;
        }
        let current = self.selected_key();
        let index = current.and_then(|key| nodes.iter().position(|node| node == key));
        let next = match index {
            Some(index) => wrapped_step(index, nodes.len(), delta),
            None if delta.is_negative() => nodes.len() - 1,
            None => 0,
        };
        self.reveal_key(nodes[next].clone());
    }

    fn toggle_selected(&mut self) {
        let Some(key) = self.selected_key().cloned() else {
            return;
        };
        if !key.is_parent() {
            return;
        }
        if !self.expanded.remove(&key) {
            self.expanded.insert(key.clone());
        }
        self.rebuild_rows(Some(&key));
    }

    fn collapse_or_parent(&mut self) {
        let Some(key) = self.selected_key().cloned() else {
            return;
        };
        if self.expanded.remove(&key) {
            self.rebuild_rows(Some(&key));
        } else if let Some(parent) = key.parent() {
            self.focus_key(parent);
        }
    }

    fn expand_or_child(&mut self) {
        let Some(key) = self.selected_key().cloned() else {
            return;
        };
        self.expanded.insert(key.clone());
        self.rebuild_rows(Some(&key));
        if let Some(index) = self
            .rows
            .iter()
            .enumerate()
            .skip(self.selected + 1)
            .find_map(|(index, row)| (row.key.parent().as_ref() == Some(&key)).then_some(index))
        {
            self.selected = index;
        }
    }

    fn status(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    fn active_status(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(4))
            .map(|(message, _)| message.as_str())
    }

    fn mailbox_requests(&self) -> &[CollaborationRequest] {
        match self.mailbox.tab {
            MailboxTab::Incoming => &self.mailbox.incoming,
            MailboxTab::Sent => &self.mailbox.sent,
        }
    }

    fn clamp_mailbox(&mut self) {
        self.mailbox.selected = self
            .mailbox
            .selected
            .min(self.mailbox_requests().len().saturating_sub(1));
    }
}

fn wrapped_step(current: usize, len: usize, delta: isize) -> usize {
    debug_assert!(len > 0);
    if delta.is_negative() {
        (current + len - (delta.unsigned_abs() % len)) % len
    } else {
        (current + (delta.unsigned_abs() % len)) % len
    }
}

fn pane_has_live_agent(pane: &PaneNode) -> bool {
    pane.agent
        .as_ref()
        .is_some_and(|agent| agent.state != AgentState::Stopped)
}

fn compare_numeric_index(left: &str, right: &str) -> std::cmp::Ordering {
    match (left.parse::<u64>().ok(), right.parse::<u64>().ok()) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

fn compare_optional_ascending<T: Ord>(left: Option<T>, right: Option<T>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn lowest_live_pane(window: &WindowNode) -> Option<&PaneNode> {
    window
        .panes
        .iter()
        .filter(|pane| pane_has_live_agent(pane))
        .min_by(|left, right| {
            compare_numeric_index(&left.index, &right.index)
                .then_with(|| left.key.pane_id.cmp(&right.key.pane_id))
        })
}

fn lowest_live_session_pane(session: &SessionNode) -> Option<&PaneNode> {
    session
        .windows
        .iter()
        .filter_map(|window| lowest_live_pane(window).map(|pane| (window, pane)))
        .min_by(|(left_window, left_pane), (right_window, right_pane)| {
            compare_numeric_index(&left_window.index, &right_window.index)
                .then_with(|| left_window.key.window_id.cmp(&right_window.key.window_id))
                .then_with(|| compare_numeric_index(&left_pane.index, &right_pane.index))
                .then_with(|| left_pane.key.pane_id.cmp(&right_pane.key.pane_id))
        })
        .map(|(_, pane)| pane)
}

enum BackgroundResult {
    Window {
        host: String,
        window: WindowKey,
        result: std::result::Result<FleetCommandResult, String>,
    },
    PaneCapture(std::result::Result<FleetCommandResult, String>),
    Command(std::result::Result<FleetCommandResult, String>),
    AskAgent(std::result::Result<String, String>),
    AskSent(std::result::Result<AskEntry, String>),
    AskList(std::result::Result<Vec<AskEntry>, String>),
    CollaborationSent(std::result::Result<FleetCommandResult, String>),
    Mailbox(std::result::Result<FleetCommandResult, String>),
    MailboxClaimed(std::result::Result<FleetCommandResult, String>),
    CollaborationReply(std::result::Result<FleetCommandResult, String>),
}

#[allow(clippy::too_many_lines)] // one event loop keeps refresh and terminal cleanup auditable
pub(crate) async fn run(
    client: Client,
    cfg: &Config,
    selector: Option<String>,
    initial: FleetSnapshot,
    invocation: crate::WatchInvocation,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    let theme = invocation
        .theme
        .map(WatchTheme::from)
        .or(cfg.watch.theme)
        .unwrap_or(cfg.ui.theme);
    let layout = invocation
        .layout
        .map_or(cfg.watch.layout, WatchLayout::from);
    let view = invocation.view.map_or(cfg.watch.view, WatchView::from);
    let sort = invocation.sort.map_or_else(
        || {
            cfg.watch
                .sort
                .first()
                .copied()
                .unwrap_or(WatchSortKey::Name)
        },
        |sort| sort.keys()[0],
    );
    let mut app = App::new(
        selector,
        theme,
        cfg.message.skills.clone(),
        layout,
        view,
        cfg.watch.tree_expansion,
        sort,
    );
    app.message_kind = cfg
        .watch
        .collaboration_kind
        .unwrap_or(RequestKind::Question);
    app.message_mode = cfg.watch.collaboration_mode.unwrap_or_default();
    app.show_paneless = invocation.include_paneless || !cfg.watch.hide_paneless;
    app.apply_snapshot(initial);
    let (background_tx, mut background_rx) = mpsc::unbounded_channel();
    let (fleet_update_tx, mut fleet_update_rx) = mpsc::channel(1);
    let fleet_stream = client.fleet_subscribe(app.selector.as_deref()).await.ok();
    let mut streaming = fleet_stream.is_some();
    if streaming {
        // The server installs its receiver before ACK. Fetching again here
        // closes the initial-snapshot/subscription gap without requiring a
        // heavyweight snapshot on every stream frame.
        match client.fleet_snapshot(app.selector.as_deref()).await {
            Ok(snapshot) => app.apply_snapshot(snapshot),
            Err(error) => app.status(format!("initial fleet refresh failed: {error}")),
        }
    }
    let fleet_update_task = fleet_stream.map(|mut stream| {
        tokio::spawn(async move {
            loop {
                match stream.recv().await {
                    Ok(Some(_)) => {
                        // One pending invalidation represents the entire cache;
                        // a fresh snapshot subsumes any burst behind it.
                        let _ = fleet_update_tx.try_send(());
                    }
                    Ok(None) | Err(_) => return,
                }
            }
        })
    });
    let now = Instant::now();
    let mut last_refresh = now;
    let mut last_ask_refresh = now;
    let mut refresh_deadline = None;
    let mut last_render = now.checked_sub(IDLE_REDRAW_INTERVAL).unwrap_or(now);
    let mut needs_render = true;
    let mut quit = false;

    while !quit {
        let mut invalidated = false;
        while fleet_update_rx.try_recv().is_ok() {
            invalidated = true;
        }
        if streaming && fleet_update_rx.is_closed() {
            streaming = false;
        }
        let refresh_interval = if streaming {
            STREAM_FALLBACK_INTERVAL
        } else {
            POLL_REFRESH_INTERVAL
        };
        if invalidated && refresh_deadline.is_none() {
            let now = Instant::now();
            refresh_deadline = Some(
                (now + STREAM_COALESCE_INTERVAL).max(last_refresh + STREAM_MIN_REFRESH_INTERVAL),
            );
        }
        let streamed_refresh_due =
            refresh_deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if streamed_refresh_due || last_refresh.elapsed() >= refresh_interval {
            match client.fleet_snapshot(app.selector.as_deref()).await {
                Ok(snapshot) => {
                    app.apply_snapshot(snapshot);
                    needs_render = true;
                }
                Err(error) => app.status(format!("fleet refresh failed: {error}")),
            }
            last_refresh = Instant::now();
            refresh_deadline = None;
        }
        if app.ask_panel && last_ask_refresh.elapsed() >= Duration::from_secs(1) {
            match client.ask_list().await {
                Ok(entries) => {
                    app.ask_entries = entries;
                    app.ask_selected = app
                        .ask_selected
                        .min(app.ask_entries.len().saturating_sub(1));
                    needs_render = true;
                }
                Err(error) => app.status(format!("ask history refresh failed: {error}")),
            }
            last_ask_refresh = Instant::now();
        }

        while let Ok(result) = background_rx.try_recv() {
            needs_render = true;
            match result {
                BackgroundResult::Window {
                    host,
                    window,
                    result,
                } => {
                    app.window_capture_pending = None;
                    app.last_window_capture = Some(Instant::now());
                    match result {
                        Ok(result) => {
                            if let Some(capture) = result.window_capture {
                                if capture.window == window {
                                    app.window_capture = Some((host, capture));
                                }
                            }
                        }
                        Err(error) => app.status(format!("window capture: {error}")),
                    }
                }
                BackgroundResult::PaneCapture(result) => match result {
                    Ok(result) => {
                        app.popup = Some(result.capture.map_or_else(
                            || "(no capture available)".into(),
                            |capture| safe_text(&capture),
                        ));
                    }
                    Err(error) => app.status(format!("pane capture: {error}")),
                },
                BackgroundResult::Command(result) => match result {
                    Ok(result) => app.status(
                        result
                            .message
                            .unwrap_or_else(|| "remote command completed".into()),
                    ),
                    Err(error) => app.status(error),
                },
                BackgroundResult::AskAgent(result) => match result {
                    Ok(agent) => {
                        app.ask_agent.clone_from(&agent);
                        if app.mode == InputMode::Ask {
                            app.status(format!("ask agent: {agent}"));
                        }
                    }
                    Err(error) => app.status(format!("ask agent: {error}")),
                },
                BackgroundResult::AskSent(result) => match result {
                    Ok(entry) => {
                        app.status(format!("asked {} — answer lands in A", entry.agent));
                        app.ask_entries.push(entry);
                        app.ask_selected = app.ask_entries.len().saturating_sub(1);
                        app.ask_panel = true;
                        last_ask_refresh = Instant::now();
                    }
                    Err(error) => app.status(format!("ask failed: {error}")),
                },
                BackgroundResult::AskList(result) => match result {
                    Ok(entries) => {
                        app.ask_entries = entries;
                        app.ask_selected = app.ask_entries.len().saturating_sub(1);
                        app.ask_panel = true;
                        last_ask_refresh = Instant::now();
                    }
                    Err(error) => app.status(format!("ask history: {error}")),
                },
                BackgroundResult::CollaborationSent(result) => match result {
                    Ok(result) => {
                        let id = result
                            .collaboration_request
                            .as_ref()
                            .map_or("request", |request| short_request_id(&request.id));
                        app.status(format!("collaboration {id} sent — reply lands in M"));
                    }
                    Err(error) => app.status(format!("message failed: {error}")),
                },
                BackgroundResult::Mailbox(result) => {
                    app.mailbox.loading = false;
                    match result {
                        Ok(result) => {
                            app.mailbox.incoming = result.collaboration_incoming;
                            app.mailbox.sent = result.collaboration_sent;
                            app.clamp_mailbox();
                        }
                        Err(error) => app.status(format!("mailbox: {error}")),
                    }
                }
                BackgroundResult::MailboxClaimed(result) => match result {
                    Ok(result) => {
                        app.mailbox.incoming = result.collaboration_incoming;
                        app.clamp_mailbox();
                        app.status("claimed queued inbox requests");
                    }
                    Err(error) => app.status(format!("claim inbox: {error}")),
                },
                BackgroundResult::CollaborationReply(result) => match result {
                    Ok(_) => {
                        app.status("reply sent");
                        refresh_mailbox(client.clone(), &mut app, background_tx.clone());
                    }
                    Err(error) => app.status(format!("reply failed: {error}")),
                },
            }
        }

        maybe_capture_window(&client, &mut app, &background_tx);
        if needs_render || last_render.elapsed() >= IDLE_REDRAW_INTERVAL {
            terminal.terminal.draw(|frame| render(frame, &app))?;
            last_render = Instant::now();
            needs_render = false;
        }

        if event::poll(INPUT_POLL)? {
            needs_render = true;
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    quit = handle_key(
                        key,
                        &client,
                        cfg,
                        config_path.as_deref(),
                        &mut terminal,
                        &mut app,
                        &background_tx,
                    )?;
                }
                Event::Paste(text) => {
                    if let Some(editor) = app.skill_editor.as_mut() {
                        match editor.field {
                            SkillEditorField::Name => {
                                editor.name.push_str(&text.replace(['\r', '\n'], " "));
                            }
                            SkillEditorField::Prompt => editor.prompt.push_str(&text),
                        }
                    } else {
                        match app.mode {
                            InputMode::Search => {
                                app.query.push_str(&safe_text(&text).replace('\n', " "));
                                app.rebuild_rows(None);
                            }
                            InputMode::Message | InputMode::Ask | InputMode::Reply => {
                                app.composer.push_str(&text);
                            }
                            InputMode::Normal => {}
                        }
                    }
                }
                Event::Resize(_, _)
                | Event::Mouse(_)
                | Event::FocusGained
                | Event::FocusLost
                | Event::Key(_) => {}
            }
        }
    }
    if let Some(task) = fleet_update_task {
        task.abort();
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // one keymap is easier to audit than scattered state handlers
fn handle_key(
    key: KeyEvent,
    client: &Client,
    cfg: &Config,
    config_path: Option<&Path>,
    terminal: &mut TerminalSession,
    app: &mut App,
    background: &mpsc::UnboundedSender<BackgroundResult>,
) -> Result<bool> {
    if let Some(name) = app.skill_delete_confirm.clone() {
        match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                app.skill_delete_confirm = None;
                let Some(path) = config_path else {
                    app.status("no config path is available for deleting the skill");
                    return Ok(false);
                };
                match crate::message_skill::remove(path, &name) {
                    Ok(()) => {
                        app.message_skills.remove(&name);
                        if let Some(palette) = app.skill_palette.as_mut() {
                            palette.move_selection(0, &app.message_skills);
                        }
                        app.status(format!("removed /{name}"));
                    }
                    Err(error) => app.status(format!("skill delete failed: {error}")),
                }
            }
            KeyCode::Esc | KeyCode::Char('n' | 'N' | 'q') => {
                app.skill_delete_confirm = None;
            }
            _ => {}
        }
        return Ok(false);
    }
    if app.skill_editor.is_some() {
        handle_skill_editor_key(key, config_path, app);
        return Ok(false);
    }
    match app.mode {
        InputMode::Search => {
            match key.code {
                KeyCode::Esc => {
                    app.query.clear();
                    app.mode = InputMode::Normal;
                    app.rebuild_rows(None);
                }
                KeyCode::Enter => app.mode = InputMode::Normal,
                KeyCode::Backspace => {
                    app.query.pop();
                    app.rebuild_rows(None);
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.query.push(character);
                    app.rebuild_rows(None);
                }
                _ => {}
            }
            return Ok(false);
        }
        InputMode::Message => {
            if handle_skill_palette_key(key, app) {
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc => {
                    app.mode = InputMode::Normal;
                    app.composer.clear();
                    app.skill_palette = None;
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    app.composer.push('\n');
                }
                KeyCode::Enter => {
                    let Some((host, pane)) = app.selected_message_pane() else {
                        app.status("selected node has no live agent pane");
                        return Ok(false);
                    };
                    if app.composer.trim().is_empty() {
                        app.status("message is empty");
                        return Ok(false);
                    }
                    if app.message_mode != WatchCollaborationMode::JustSend
                        && !host_supports_collaboration(app, &host)
                    {
                        app.status(format!(
                            "host '{host}' needs a muxa upgrade for Fleet collaboration"
                        ));
                        return Ok(false);
                    }
                    let text = std::mem::take(&mut app.composer);
                    app.mode = InputMode::Normal;
                    if app.message_mode == WatchCollaborationMode::JustSend {
                        spawn_command(
                            client,
                            background,
                            host,
                            FleetOperation::SendPrompt {
                                pane,
                                text,
                                submit: true,
                            },
                        );
                        app.status("sending prompt…");
                    } else {
                        let (workspace_id, work_id) =
                            app.work_identity_for_message_pane(&host, &pane);
                        let request = NewRequest {
                            kind: app.message_kind,
                            body: text,
                            expects_reply: app.message_kind != RequestKind::Notice,
                            work_mode: match app.message_mode {
                                WatchCollaborationMode::Execute => WorkMode::Execute,
                                WatchCollaborationMode::ReadOnly
                                | WatchCollaborationMode::JustSend => WorkMode::ReadOnly,
                            },
                            thread_id: None,
                            parent_request_id: None,
                            workspace_id,
                            work_id,
                            run_id: None,
                            paths: Vec::new(),
                            artifacts: Vec::new(),
                            links: Vec::new(),
                            air_artifacts: Vec::new(),
                        };
                        spawn_collaboration_send(client, background, app, host, pane, request);
                        app.status("sending collaboration request…");
                    }
                }
                KeyCode::Tab => {
                    app.message_kind = next_request_kind(app.message_kind);
                    persist_message_defaults_or_status(config_path, app);
                }
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.message_mode = next_message_mode(app.message_mode);
                    persist_message_defaults_or_status(config_path, app);
                }
                KeyCode::Backspace => {
                    app.composer.pop();
                }
                KeyCode::Char('/') => {
                    app.skill_palette = Some(crate::message_skill::Palette::default());
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.composer.push(character);
                }
                _ => {}
            }
            return Ok(false);
        }
        InputMode::Ask => {
            if handle_skill_palette_key(key, app) {
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc => {
                    app.mode = InputMode::Normal;
                    app.composer.clear();
                    app.skill_palette = None;
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    app.composer.push('\n');
                }
                KeyCode::Enter => {
                    if app.composer.trim().is_empty() {
                        app.status("question is empty");
                        return Ok(false);
                    }
                    let prompt = std::mem::take(&mut app.composer);
                    app.mode = InputMode::Normal;
                    let client = client.clone();
                    let sender = background.clone();
                    tokio::spawn(async move {
                        let result = client
                            .ask_send(&prompt)
                            .await
                            .map_err(|error| error.to_string());
                        let _ = sender.send(BackgroundResult::AskSent(result));
                    });
                    app.status("asking…");
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let next = if app.ask_agent == "claude" {
                        "codex"
                    } else {
                        "claude"
                    };
                    let client = client.clone();
                    let sender = background.clone();
                    tokio::spawn(async move {
                        let result = client
                            .ask_agent(Some(next))
                            .await
                            .map_err(|error| error.to_string());
                        let _ = sender.send(BackgroundResult::AskAgent(result));
                    });
                    app.status(format!("switching Ask to {next}…"));
                }
                KeyCode::Char('/') => {
                    app.skill_palette = Some(crate::message_skill::Palette::default());
                }
                KeyCode::Backspace => {
                    app.composer.pop();
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.composer.push(character);
                }
                _ => {}
            }
            return Ok(false);
        }
        InputMode::Reply => {
            if handle_skill_palette_key(key, app) {
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc => {
                    app.mode = InputMode::Normal;
                    app.composer.clear();
                    app.reply_request_id = None;
                    app.mailbox.open = true;
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    app.composer.push('\n');
                }
                KeyCode::Enter => {
                    if app.composer.trim().is_empty() {
                        app.status("reply is empty");
                        return Ok(false);
                    }
                    let Some(request_id) = app.reply_request_id.take() else {
                        app.status("reply target is no longer available");
                        app.mode = InputMode::Normal;
                        return Ok(false);
                    };
                    let Some(host) = app.mailbox.host.clone() else {
                        app.status("mailbox host is no longer available");
                        app.mode = InputMode::Normal;
                        return Ok(false);
                    };
                    let Some(pane) = app.mailbox.pane.clone() else {
                        app.status("mailbox pane is no longer available");
                        app.mode = InputMode::Normal;
                        return Ok(false);
                    };
                    let body = std::mem::take(&mut app.composer);
                    app.mode = InputMode::Normal;
                    app.mailbox.open = true;
                    spawn_collaboration_reply(client, background, host, pane, request_id, body);
                    app.status("sending reply…");
                }
                KeyCode::Backspace => {
                    app.composer.pop();
                }
                KeyCode::Char('/') => {
                    app.skill_palette = Some(crate::message_skill::Palette::default());
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.composer.push(character);
                }
                _ => {}
            }
            return Ok(false);
        }
        InputMode::Normal => {}
    }

    if app.ask_panel {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'A') => app.ask_panel = false,
            KeyCode::Char('a') => {
                app.ask_panel = false;
                open_ask_composer(client, app, background);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.ask_selected =
                    (app.ask_selected + 1).min(app.ask_entries.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.ask_selected = app.ask_selected.saturating_sub(1);
            }
            KeyCode::Char('r') => refresh_ask_history(client, background),
            _ => {}
        }
        return Ok(false);
    }

    if app.mailbox.open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'M' | 'b') => app.mailbox.open = false,
            KeyCode::Char('m') => {
                app.mailbox.open = false;
                if app.selected_message_pane().is_some() {
                    app.mode = InputMode::Message;
                    app.composer.clear();
                    app.skill_palette = None;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                app.mailbox.tab = match app.mailbox.tab {
                    MailboxTab::Incoming => MailboxTab::Sent,
                    MailboxTab::Sent => MailboxTab::Incoming,
                };
                app.mailbox.selected = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = app.mailbox_requests().len();
                if len > 0 {
                    app.mailbox.selected = wrapped_step(app.mailbox.selected, len, 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let len = app.mailbox_requests().len();
                if len > 0 {
                    app.mailbox.selected = wrapped_step(app.mailbox.selected, len, -1);
                }
            }
            KeyCode::Char('r') => {
                refresh_mailbox(client.clone(), app, background.clone());
            }
            KeyCode::Char('i') => claim_mailbox(client, app, background),
            KeyCode::Char('e') => open_reply_composer(app),
            _ => {}
        }
        return Ok(false);
    }

    if app.popup.is_some() {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
            app.popup = None;
        }
        return Ok(false);
    }
    if app.help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?' | 'q')) {
            app.help = false;
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Down | KeyCode::Char('j') => app.move_vertical(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_vertical(-1),
        KeyCode::Char('J') => app.jump_pane(1),
        KeyCode::Char('K') => app.jump_pane(-1),
        KeyCode::Home | KeyCode::Char('g') => {
            if app.layout == WatchLayout::Swarm {
                if let Some(first) = all_swarm_keys(
                    &app.snapshot,
                    &app.topologies,
                    &app.query,
                    app.attention_only,
                    app.sort,
                    app.show_paneless,
                )
                .into_iter()
                .next()
                {
                    app.reveal_key(first);
                }
            } else if let Some(first) = app.rows.first().map(|row| row.key.clone()) {
                app.focus_key(first);
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            if app.layout == WatchLayout::Swarm {
                if let Some(last) = all_swarm_keys(
                    &app.snapshot,
                    &app.topologies,
                    &app.query,
                    app.attention_only,
                    app.sort,
                    app.show_paneless,
                )
                .into_iter()
                .last()
                {
                    app.reveal_key(last);
                }
            } else if let Some(last) = app.rows.last().map(|row| row.key.clone()) {
                app.focus_key(last);
            }
        }
        KeyCode::Left | KeyCode::Char('h') => app.collapse_or_parent(),
        KeyCode::Right | KeyCode::Char('l') => app.expand_or_child(),
        KeyCode::Char(' ') => app.toggle_selected(),
        KeyCode::Char('/') => app.mode = InputMode::Search,
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.attention_only = !app.attention_only;
            app.rebuild_rows(None);
        }
        KeyCode::Char('a') => open_ask_composer(client, app, background),
        KeyCode::Char('A') => refresh_ask_history(client, background),
        KeyCode::Char('?') => app.help = true,
        KeyCode::Char('r') => {
            if let Some(host) = app.selected_key().map(|key| key.host().to_string()) {
                spawn_command(client, background, host, FleetOperation::Refresh);
                app.status("refresh requested…");
            }
        }
        KeyCode::Char('c') => {
            if let Some(host) = app.selected_host() {
                if host.local {
                    app.status("local host is always connected");
                    return Ok(false);
                }
                let operation = if matches!(
                    host.state,
                    FleetHostState::Online | FleetHostState::Connecting | FleetHostState::Degraded
                ) {
                    FleetOperation::Disconnect
                } else {
                    FleetOperation::Connect
                };
                let alias = host.alias.clone();
                spawn_command(client, background, alias, operation);
                app.status("connection command requested…");
            }
        }
        KeyCode::Char('o' | 'p') => {
            if let Some((host, pane)) = app.selected_pane() {
                let client = client.clone();
                let sender = background.clone();
                tokio::spawn(async move {
                    let result = client
                        .fleet_execute(&host, &FleetOperation::Capture { pane })
                        .await
                        .map_err(|error| error.to_string());
                    let _ = sender.send(BackgroundResult::PaneCapture(result));
                });
                app.status("capturing selected pane…");
            }
        }
        KeyCode::Char('m') => {
            if app.selected_message_pane().is_some() {
                app.mode = InputMode::Message;
                app.composer.clear();
                app.skill_palette = None;
            } else {
                app.status("select a session, window, or pane with a live agent");
            }
        }
        KeyCode::Char('M' | 'b') => open_mailbox(client, app, background),
        KeyCode::Enter => {
            if let Some((host_alias, pane)) = app.selected_pane() {
                let Some(host) = app
                    .snapshot
                    .hosts
                    .iter()
                    .find(|host| host.alias == host_alias)
                else {
                    return Ok(false);
                };
                let Some(node_id) = host.node_id.clone() else {
                    app.status("host has not completed its relay handshake");
                    return Ok(false);
                };
                let target = GlobalPaneRef {
                    node_id,
                    pane,
                    agent_session_id: None,
                };
                terminal.suspend()?;
                let result = crate::fleet_cli::attach_exact(cfg, &host_alias, &target, false);
                terminal.resume()?;
                if let Err(error) = result {
                    app.status(format!("attach failed: {error}"));
                }
                app.window_capture = None;
                app.last_window_capture = None;
            } else {
                app.toggle_selected();
            }
        }
        _ => {}
    }
    Ok(false)
}

fn open_ask_composer(
    client: &Client,
    app: &mut App,
    background: &mpsc::UnboundedSender<BackgroundResult>,
) {
    app.mode = InputMode::Ask;
    app.composer.clear();
    app.skill_palette = None;
    let client = client.clone();
    let sender = background.clone();
    tokio::spawn(async move {
        let result = client
            .ask_agent(None)
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send(BackgroundResult::AskAgent(result));
    });
}

fn refresh_ask_history(client: &Client, background: &mpsc::UnboundedSender<BackgroundResult>) {
    let client = client.clone();
    let sender = background.clone();
    tokio::spawn(async move {
        let result = client.ask_list().await.map_err(|error| error.to_string());
        let _ = sender.send(BackgroundResult::AskList(result));
    });
}

fn handle_skill_palette_key(key: KeyEvent, app: &mut App) -> bool {
    let Some(palette) = app.skill_palette.as_ref() else {
        return false;
    };
    let selected_name = crate::message_skill::matching_skills(&app.message_skills, &palette.query)
        .get(palette.selected)
        .map(|(name, _)| (*name).clone());
    match key.code {
        KeyCode::F(2) | KeyCode::Char('a')
            if key.code == KeyCode::F(2) || key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.skill_editor = Some(SkillEditor::default());
            app.skill_palette = None;
        }
        KeyCode::Delete | KeyCode::Char('d')
            if key.code == KeyCode::Delete || key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            if let Some(name) = selected_name {
                app.skill_delete_confirm = Some(name);
            } else {
                app.status("no message skill selected");
            }
        }
        KeyCode::Esc => app.skill_palette = None,
        KeyCode::Enter => {
            let prompt = app
                .skill_palette
                .as_ref()
                .and_then(|palette| palette.selected_prompt(&app.message_skills));
            if let Some(prompt) = prompt {
                let mut cursor = app.composer.chars().count();
                crate::message_skill::insert_prompt(&mut app.composer, &mut cursor, &prompt);
                app.skill_palette = None;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(palette) = app.skill_palette.as_mut() {
                palette.move_selection(1, &app.message_skills);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(palette) = app.skill_palette.as_mut() {
                palette.move_selection(-1, &app.message_skills);
            }
        }
        KeyCode::Backspace => {
            if app
                .skill_palette
                .as_mut()
                .is_some_and(|palette| !palette.backspace())
            {
                app.skill_palette = None;
            }
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(palette) = app.skill_palette.as_mut() {
                palette.insert(character);
            }
        }
        _ => {}
    }
    true
}

fn handle_skill_editor_key(key: KeyEvent, config_path: Option<&Path>, app: &mut App) {
    match key.code {
        KeyCode::Esc => app.skill_editor = None,
        KeyCode::Tab | KeyCode::BackTab => {
            if let Some(editor) = app.skill_editor.as_mut() {
                editor.field = match editor.field {
                    SkillEditorField::Name => SkillEditorField::Prompt,
                    SkillEditorField::Prompt => SkillEditorField::Name,
                };
            }
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if let Some(editor) = app
                .skill_editor
                .as_mut()
                .filter(|editor| editor.field == SkillEditorField::Prompt)
            {
                editor.prompt.push('\n');
            }
        }
        KeyCode::Enter => {
            let Some(editor) = app.skill_editor.as_mut() else {
                return;
            };
            if editor.field == SkillEditorField::Name {
                match crate::message_skill::validate_name(&editor.name) {
                    Ok(()) => editor.field = SkillEditorField::Prompt,
                    Err(error) => app.status(error.to_string()),
                }
                return;
            }
            let name = editor.name.clone();
            let prompt = editor.prompt.clone();
            let Some(path) = config_path else {
                app.status("no config path is available for saving the skill");
                return;
            };
            match crate::message_skill::upsert(path, &name, &prompt) {
                Ok(()) => {
                    app.message_skills.insert(name.clone(), prompt);
                    app.skill_editor = None;
                    app.status(format!("saved /{name}"));
                }
                Err(error) => app.status(format!("skill save failed: {error}")),
            }
        }
        KeyCode::Backspace => {
            if let Some(editor) = app.skill_editor.as_mut() {
                match editor.field {
                    SkillEditorField::Name => {
                        editor.name.pop();
                    }
                    SkillEditorField::Prompt => {
                        editor.prompt.pop();
                    }
                }
            }
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(editor) = app.skill_editor.as_mut() {
                match editor.field {
                    SkillEditorField::Name => editor.name.push(character),
                    SkillEditorField::Prompt => editor.prompt.push(character),
                }
            }
        }
        _ => {}
    }
}

fn host_supports_collaboration(app: &App, alias: &str) -> bool {
    app.snapshot
        .hosts
        .iter()
        .find(|host| host.alias == alias)
        .is_some_and(|host| {
            host.local
                || host
                    .capabilities
                    .iter()
                    .any(|capability| capability == "collaboration")
        })
}

fn spawn_collaboration_send(
    client: &Client,
    background: &mpsc::UnboundedSender<BackgroundResult>,
    app: &mut App,
    host: String,
    pane: PaneKey,
    request: NewRequest,
) {
    if !host_supports_collaboration(app, &host) {
        app.status(format!(
            "host '{host}' needs a muxa upgrade for Fleet collaboration"
        ));
        return;
    }
    let client = client.clone();
    let sender = background.clone();
    tokio::spawn(async move {
        let result = client
            .fleet_execute(&host, &FleetOperation::CollaborationSend { pane, request })
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send(BackgroundResult::CollaborationSent(result));
    });
}

fn open_mailbox(
    client: &Client,
    app: &mut App,
    background: &mpsc::UnboundedSender<BackgroundResult>,
) {
    let Some((host, pane)) = app.selected_message_pane() else {
        app.status("select a session, window, or pane with a live agent");
        return;
    };
    if !host_supports_collaboration(app, &host) {
        app.status(format!(
            "host '{host}' needs a muxa upgrade for Fleet collaboration"
        ));
        return;
    }
    app.mailbox.open = true;
    app.mailbox.loading = true;
    app.mailbox.host = Some(host);
    app.mailbox.pane = Some(pane);
    app.mailbox.selected = 0;
    refresh_mailbox(client.clone(), app, background.clone());
}

fn refresh_mailbox(
    client: Client,
    app: &mut App,
    background: mpsc::UnboundedSender<BackgroundResult>,
) {
    let (Some(host), Some(pane)) = (app.mailbox.host.clone(), app.mailbox.pane.clone()) else {
        app.status("mailbox target is no longer available");
        return;
    };
    app.mailbox.loading = true;
    tokio::spawn(async move {
        let result = client
            .fleet_execute(&host, &FleetOperation::CollaborationMailbox { pane })
            .await
            .map_err(|error| error.to_string());
        let _ = background.send(BackgroundResult::Mailbox(result));
    });
}

fn claim_mailbox(
    client: &Client,
    app: &mut App,
    background: &mpsc::UnboundedSender<BackgroundResult>,
) {
    if app.mailbox.tab != MailboxTab::Incoming {
        app.status("switch to incoming requests before claiming");
        return;
    }
    let (Some(host), Some(pane)) = (app.mailbox.host.clone(), app.mailbox.pane.clone()) else {
        app.status("mailbox target is no longer available");
        return;
    };
    let client = client.clone();
    let sender = background.clone();
    tokio::spawn(async move {
        let result = client
            .fleet_execute(&host, &FleetOperation::CollaborationClaim { pane })
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send(BackgroundResult::MailboxClaimed(result));
    });
    app.status("claiming queued inbox requests…");
}

fn open_reply_composer(app: &mut App) {
    if app.mailbox.tab != MailboxTab::Incoming {
        app.status("switch to incoming requests to reply");
        return;
    }
    let Some(request) = app.mailbox_requests().get(app.mailbox.selected).cloned() else {
        app.status("no incoming request selected");
        return;
    };
    if request.status == RequestStatus::Queued {
        app.status("press i to claim the request before replying");
        return;
    }
    if request.status.is_terminal() {
        app.status("selected request is already terminal");
        return;
    }
    app.reply_request_id = Some(request.id);
    app.composer.clear();
    app.skill_palette = None;
    app.mailbox.open = false;
    app.mode = InputMode::Reply;
}

fn spawn_collaboration_reply(
    client: &Client,
    background: &mpsc::UnboundedSender<BackgroundResult>,
    host: String,
    pane: PaneKey,
    request_id: String,
    body: String,
) {
    let client = client.clone();
    let sender = background.clone();
    tokio::spawn(async move {
        let result = client
            .fleet_execute(
                &host,
                &FleetOperation::CollaborationReply {
                    pane,
                    request_id,
                    status: RequestStatus::Completed,
                    body,
                },
            )
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send(BackgroundResult::CollaborationReply(result));
    });
}

fn next_request_kind(kind: RequestKind) -> RequestKind {
    match kind {
        RequestKind::Question => RequestKind::Review,
        RequestKind::Review => RequestKind::Task,
        RequestKind::Task => RequestKind::Notice,
        RequestKind::Notice => RequestKind::Question,
    }
}

fn next_message_mode(mode: WatchCollaborationMode) -> WatchCollaborationMode {
    match mode {
        WatchCollaborationMode::ReadOnly => WatchCollaborationMode::Execute,
        WatchCollaborationMode::Execute => WatchCollaborationMode::JustSend,
        WatchCollaborationMode::JustSend => WatchCollaborationMode::ReadOnly,
    }
}

fn persist_message_defaults_or_status(path: Option<&Path>, app: &mut App) {
    let Some(path) = path else {
        return;
    };
    if let Err(error) = persist_message_defaults(path, app.message_kind, app.message_mode) {
        app.status(format!("message kind/mode save failed: {error}"));
    }
}

fn persist_message_defaults(
    path: &Path,
    kind: RequestKind,
    mode: WatchCollaborationMode,
) -> std::result::Result<(), String> {
    let original = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let mut document = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original
            .parse::<toml_edit::DocumentMut>()
            .map_err(|error| format!("parse {}: {error}", path.display()))?
    };
    match document.get("watch") {
        Some(toml_edit::Item::Table(_)) | None => {}
        Some(_) => return Err("[watch] is not a table".into()),
    }
    if document.get("watch").is_none() {
        document["watch"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let watch = document["watch"]
        .as_table_mut()
        .ok_or_else(|| "[watch] is not a table".to_string())?;
    watch["collaboration_kind"] = toml_edit::value(request_kind_label(kind));
    watch["collaboration_mode"] = toml_edit::value(match mode {
        WatchCollaborationMode::ReadOnly => "read_only",
        WatchCollaborationMode::Execute => "execute",
        WatchCollaborationMode::JustSend => "just_send",
    });
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    std::fs::write(path, document.to_string())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn spawn_command(
    client: &Client,
    background: &mpsc::UnboundedSender<BackgroundResult>,
    host: String,
    operation: FleetOperation,
) {
    let client = client.clone();
    let sender = background.clone();
    tokio::spawn(async move {
        let result = client
            .fleet_execute(&host, &operation)
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send(BackgroundResult::Command(result));
    });
}

fn maybe_capture_window(
    client: &Client,
    app: &mut App,
    background: &mpsc::UnboundedSender<BackgroundResult>,
) {
    let Some((host, window)) = app.selected_window() else {
        return;
    };
    let same = app
        .window_capture
        .as_ref()
        .is_some_and(|(capture_host, capture)| capture_host == &host && capture.window == window);
    let due = !same
        || app
            .last_window_capture
            .is_none_or(|last| last.elapsed() >= WINDOW_CAPTURE_INTERVAL);
    if !due || app.window_capture_pending.is_some() {
        return;
    }
    app.window_capture_pending = Some((host.clone(), window.clone()));
    let client = client.clone();
    let sender = background.clone();
    let request_window = window.clone();
    tokio::spawn(async move {
        let result = client
            .fleet_execute(
                &host,
                &FleetOperation::CaptureWindow {
                    window: request_window.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send(BackgroundResult::Window {
            host,
            window: request_window,
            result,
        });
    });
}

fn host_topology(host: &FleetHostSnapshot) -> Option<TopologySnapshot> {
    let remote = host.remote.as_ref()?;
    let mut grouped = HashMap::<HostKind, Vec<muxa::tmux::PaneInfo>>::new();
    for pane in &remote.panes {
        let kind = muxa::backend::pane_id_host_kind(&pane.pane_id)
            .or_else(|| {
                remote
                    .backends
                    .iter()
                    .find(|backend| {
                        pane.socket.as_ref().is_none_or(|socket| {
                            backend.kind != HostKind::Tmux || !socket.is_empty()
                        })
                    })
                    .map(|backend| backend.kind)
            })
            .unwrap_or(HostKind::Tmux);
        grouped.entry(kind).or_default().push(pane.clone());
    }
    let inputs = grouped
        .into_iter()
        .map(|(kind, panes)| TopologyInput::new(kind, panes))
        .collect();
    Some(TopologySnapshot::build(
        remote.observed_at,
        inputs,
        remote.agents.clone(),
    ))
}

#[allow(clippy::too_many_lines)] // hierarchy construction stays in one deterministic traversal
fn build_rows(
    snapshot: &FleetSnapshot,
    topologies: &HashMap<String, TopologySnapshot>,
    expanded: &HashSet<NodeKey>,
    query: &str,
    attention_only: bool,
    sort: WatchSortKey,
    show_paneless: bool,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for host in &snapshot.hosts {
        let topology = topologies.get(&host.alias);
        if !host_relevant(host, topology, query, attention_only, show_paneless) {
            continue;
        }
        let host_key = NodeKey::Host(host.alias.clone());
        let paneless_count = topology.map_or(0, |topology| {
            usize::from(show_paneless) * topology.unassigned_agents.len()
        });
        rows.push(TreeRow {
            key: host_key.clone(),
            depth: 0,
            label: host.alias.clone(),
            detail: format!(
                "{} · {} sessions · {} panes · {} agents",
                host_state_label(host.state),
                topology.map_or(0, |topology| topology.sessions.len()),
                host.pane_count(),
                host.agent_count(),
            ),
            state: dominant_host_state(host),
            attention: visible_host_attention(host, topology, show_paneless),
            children: topology.map_or(0, |topology| topology.sessions.len()) + paneless_count,
        });
        let show_host = expanded.contains(&host_key) || !query.is_empty() || attention_only;
        if !show_host {
            continue;
        }
        let Some(topology) = topology else {
            continue;
        };
        let mut sessions = topology.sessions.iter().collect::<Vec<_>>();
        sessions.sort_by(|left, right| compare_sessions(left, right, sort));
        for session in sessions {
            if !session_relevant(session, query, attention_only) {
                continue;
            }
            let session_key = NodeKey::Session {
                host: host.alias.clone(),
                key: session.key.clone(),
            };
            rows.push(TreeRow {
                key: session_key.clone(),
                depth: 1,
                label: session.name.clone(),
                detail: format!(
                    "{} windows · {} panes",
                    session.windows.len(),
                    session.pane_count()
                ),
                state: dominant_distribution(&session.states),
                attention: distribution_attention(&session.states),
                children: session.windows.len(),
            });
            let show_session =
                expanded.contains(&session_key) || !query.is_empty() || attention_only;
            if !show_session {
                continue;
            }
            let mut windows = session.windows.iter().collect::<Vec<_>>();
            windows.sort_by(|left, right| compare_windows(left, right, sort));
            for window in windows {
                if !window_relevant(window, query, attention_only) {
                    continue;
                }
                let window_key = NodeKey::Window {
                    host: host.alias.clone(),
                    key: window.key.clone(),
                };
                rows.push(TreeRow {
                    key: window_key.clone(),
                    depth: 2,
                    label: if window.name.is_empty() {
                        format!("window {}", window.index)
                    } else {
                        window.name.clone()
                    },
                    detail: format!("{} panes", window.panes.len()),
                    state: dominant_distribution(&window.states),
                    attention: distribution_attention(&window.states),
                    children: window.panes.len(),
                });
                let show_window =
                    expanded.contains(&window_key) || !query.is_empty() || attention_only;
                if !show_window {
                    continue;
                }
                let mut panes = window.panes.iter().collect::<Vec<_>>();
                panes.sort_by(|left, right| compare_panes(left, right, sort));
                for pane in panes {
                    if !pane_relevant(pane, query, attention_only) {
                        continue;
                    }
                    let (state, attention, detail) = pane.agent.as_ref().map_or_else(
                        || (None, 0, pane.current_command.clone()),
                        |agent| {
                            (
                                Some(agent.state),
                                usize::from(needs_attention(agent.state)),
                                format!(
                                    "{} · {}",
                                    agent.kind,
                                    agent
                                        .ai_title
                                        .as_deref()
                                        .or(agent.last_prompt.as_deref())
                                        .unwrap_or(&agent.session_id)
                                ),
                            )
                        },
                    );
                    rows.push(TreeRow {
                        key: NodeKey::Pane {
                            host: host.alias.clone(),
                            key: pane.key.clone(),
                        },
                        depth: 3,
                        label: format!("{} ({})", pane.key.pane_id, pane.index),
                        detail,
                        state,
                        attention,
                        children: 0,
                    });
                }
            }
        }
        if show_paneless {
            let mut agents = topology
                .unassigned_agents
                .iter()
                .filter(|agent| agent_relevant(agent, query, attention_only))
                .collect::<Vec<_>>();
            agents.sort_by(|left, right| compare_agents(left, right, sort));
            for agent in agents {
                rows.push(TreeRow {
                    key: paneless_agent_key(&host.alias, agent),
                    depth: 1,
                    label: format!("[paneless] {}", agent.kind),
                    detail: format!(
                        "{} · {} · {}",
                        agent.session_id,
                        state_label(agent.state),
                        agent_summary(agent)
                    ),
                    state: Some(agent.state),
                    attention: usize::from(needs_attention(agent.state)),
                    children: 0,
                });
            }
        }
    }
    rows
}

fn paneless_agent_key(host: &str, agent: &muxa::Agent) -> NodeKey {
    NodeKey::PanelessAgent {
        host: host.to_string(),
        kind: agent.kind.to_string(),
        session_id: agent.session_id.clone(),
    }
}

fn compare_agents(
    left: &muxa::Agent,
    right: &muxa::Agent,
    sort: WatchSortKey,
) -> std::cmp::Ordering {
    let order = match sort {
        WatchSortKey::Activity => right.last_activity_at.cmp(&left.last_activity_at),
        WatchSortKey::Duration => left.state_entered_at.cmp(&right.state_entered_at),
        WatchSortKey::State => {
            state_sort_rank(Some(right.state)).cmp(&state_sort_rank(Some(left.state)))
        }
        WatchSortKey::Name | WatchSortKey::Pane | WatchSortKey::PaneId => {
            left.kind.to_string().cmp(&right.kind.to_string())
        }
    };
    order.then_with(|| left.session_id.cmp(&right.session_id))
}

fn compare_sessions(
    left: &SessionNode,
    right: &SessionNode,
    sort: WatchSortKey,
) -> std::cmp::Ordering {
    compare_node_agents(
        left.windows.iter().flat_map(|window| &window.panes),
        right.windows.iter().flat_map(|window| &window.panes),
        sort,
    )
    .then_with(|| left.name.cmp(&right.name))
}

fn compare_hosts(
    left: &FleetHostSnapshot,
    right: &FleetHostSnapshot,
    sort: WatchSortKey,
) -> std::cmp::Ordering {
    match sort {
        WatchSortKey::Activity => host_latest_activity(right).cmp(&host_latest_activity(left)),
        WatchSortKey::Duration => {
            compare_optional_ascending(host_earliest_start(left), host_earliest_start(right))
        }
        WatchSortKey::State => right
            .needs_attention()
            .cmp(&left.needs_attention())
            .then_with(|| right.agent_count().cmp(&left.agent_count())),
        WatchSortKey::Name | WatchSortKey::Pane | WatchSortKey::PaneId => {
            left.alias.cmp(&right.alias)
        }
    }
}

fn host_latest_activity(host: &FleetHostSnapshot) -> Option<OffsetDateTime> {
    host.remote
        .as_ref()?
        .agents
        .iter()
        .map(|agent| agent.last_activity_at)
        .max()
}

fn host_earliest_start(host: &FleetHostSnapshot) -> Option<OffsetDateTime> {
    host.remote
        .as_ref()?
        .agents
        .iter()
        .map(|agent| agent.started_at)
        .min()
}

fn compare_windows(
    left: &WindowNode,
    right: &WindowNode,
    sort: WatchSortKey,
) -> std::cmp::Ordering {
    let order = match sort {
        WatchSortKey::Pane => compare_numeric_index(&left.index, &right.index),
        _ => compare_node_agents(left.panes.iter(), right.panes.iter(), sort),
    };
    order.then_with(|| left.name.cmp(&right.name))
}

fn compare_node_agents<'a>(
    left: impl Iterator<Item = &'a PaneNode>,
    right: impl Iterator<Item = &'a PaneNode>,
    sort: WatchSortKey,
) -> std::cmp::Ordering {
    let left = left
        .filter_map(|pane| pane.agent.as_ref())
        .collect::<Vec<_>>();
    let right = right
        .filter_map(|pane| pane.agent.as_ref())
        .collect::<Vec<_>>();
    match sort {
        WatchSortKey::Activity => right
            .iter()
            .map(|agent| agent.last_activity_at)
            .max()
            .cmp(&left.iter().map(|agent| agent.last_activity_at).max()),
        WatchSortKey::Duration => compare_optional_ascending(
            left.iter().map(|agent| agent.started_at).min(),
            right.iter().map(|agent| agent.started_at).min(),
        ),
        WatchSortKey::State => {
            let rank = |agents: &[&muxa::Agent]| {
                agents
                    .iter()
                    .map(|agent| state_sort_rank(Some(agent.state)))
                    .max()
                    .unwrap_or(0)
            };
            rank(&right).cmp(&rank(&left))
        }
        WatchSortKey::Name | WatchSortKey::Pane | WatchSortKey::PaneId => std::cmp::Ordering::Equal,
    }
}

fn compare_panes(left: &PaneNode, right: &PaneNode, sort: WatchSortKey) -> std::cmp::Ordering {
    let left_agent = left.agent.as_ref();
    let right_agent = right.agent.as_ref();
    match sort {
        WatchSortKey::Activity => right_agent
            .map(|agent| agent.last_activity_at)
            .cmp(&left_agent.map(|agent| agent.last_activity_at)),
        WatchSortKey::Duration => compare_optional_ascending(
            left_agent.map(|agent| agent.state_entered_at),
            right_agent.map(|agent| agent.state_entered_at),
        ),
        WatchSortKey::State => state_sort_rank(right_agent.map(|agent| agent.state))
            .cmp(&state_sort_rank(left_agent.map(|agent| agent.state))),
        WatchSortKey::Pane => compare_numeric_index(&left.index, &right.index),
        WatchSortKey::PaneId => left.key.pane_id.cmp(&right.key.pane_id),
        WatchSortKey::Name => left.index.cmp(&right.index),
    }
}

fn all_pane_keys(
    snapshot: &FleetSnapshot,
    topologies: &HashMap<String, TopologySnapshot>,
    query: &str,
    attention_only: bool,
    sort: WatchSortKey,
) -> Vec<NodeKey> {
    let host_order = snapshot
        .hosts
        .iter()
        .enumerate()
        .map(|(index, host)| (host.alias.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut panes = snapshot
        .hosts
        .iter()
        .filter(|host| {
            host_relevant(
                host,
                topologies.get(&host.alias),
                query,
                attention_only,
                false,
            )
        })
        .flat_map(|host| {
            topologies
                .get(&host.alias)
                .into_iter()
                .flat_map(|topology| &topology.sessions)
                .flat_map(|session| &session.windows)
                .flat_map(|window| &window.panes)
                .filter(|pane| pane_relevant(pane, query, attention_only))
                .map(|pane| NodeKey::Pane {
                    host: host.alias.clone(),
                    key: pane.key.clone(),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    panes.sort_by(|left, right| {
        host_order
            .get(left.host())
            .cmp(&host_order.get(right.host()))
            .then_with(|| compare_pane_keys(left, right, topologies, sort))
    });
    panes
}

fn all_swarm_keys(
    snapshot: &FleetSnapshot,
    topologies: &HashMap<String, TopologySnapshot>,
    query: &str,
    attention_only: bool,
    sort: WatchSortKey,
    show_paneless: bool,
) -> Vec<NodeKey> {
    let mut nodes = Vec::new();
    for host in &snapshot.hosts {
        let Some(topology) = topologies.get(&host.alias) else {
            continue;
        };
        let mut panes = topology
            .sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
            .filter(|pane| pane_relevant(pane, query, attention_only))
            .map(|pane| NodeKey::Pane {
                host: host.alias.clone(),
                key: pane.key.clone(),
            })
            .collect::<Vec<_>>();
        panes.sort_by(|left, right| compare_pane_keys(left, right, topologies, sort));
        nodes.extend(panes);
        if show_paneless {
            let mut agents = topology
                .unassigned_agents
                .iter()
                .filter(|agent| agent_relevant(agent, query, attention_only))
                .collect::<Vec<_>>();
            agents.sort_by(|left, right| compare_agents(left, right, sort));
            nodes.extend(
                agents
                    .into_iter()
                    .map(|agent| paneless_agent_key(&host.alias, agent)),
            );
        }
    }
    nodes
}

fn compare_pane_keys(
    left: &NodeKey,
    right: &NodeKey,
    topologies: &HashMap<String, TopologySnapshot>,
    sort: WatchSortKey,
) -> std::cmp::Ordering {
    let pane = |key: &NodeKey| match key {
        NodeKey::Pane { host, key } => topologies
            .get(host)
            .and_then(|topology| find_pane(topology, key)),
        _ => None,
    };
    let Some(left_pane) = pane(left) else {
        return std::cmp::Ordering::Equal;
    };
    let Some(right_pane) = pane(right) else {
        return std::cmp::Ordering::Equal;
    };
    let left_agent = left_pane.agent.as_ref();
    let right_agent = right_pane.agent.as_ref();
    match sort {
        WatchSortKey::Activity => right_agent
            .map(|agent| agent.last_activity_at)
            .cmp(&left_agent.map(|agent| agent.last_activity_at)),
        WatchSortKey::Duration => compare_optional_ascending(
            left_agent.map(|agent| agent.state_entered_at),
            right_agent.map(|agent| agent.state_entered_at),
        ),
        WatchSortKey::State => state_sort_rank(right_agent.map(|agent| agent.state))
            .cmp(&state_sort_rank(left_agent.map(|agent| agent.state))),
        WatchSortKey::Pane => compare_numeric_index(&left_pane.index, &right_pane.index),
        WatchSortKey::PaneId => left_pane.key.pane_id.cmp(&right_pane.key.pane_id),
        WatchSortKey::Name => left_pane
            .key
            .window
            .session
            .session_id
            .cmp(&right_pane.key.window.session.session_id)
            .then(
                left_pane
                    .key
                    .window
                    .window_id
                    .cmp(&right_pane.key.window.window_id),
            )
            .then(left_pane.index.cmp(&right_pane.index)),
    }
}

fn state_sort_rank(state: Option<AgentState>) -> u8 {
    match state {
        Some(AgentState::Error) => 7,
        Some(AgentState::WaitingInput) => 6,
        Some(AgentState::WaitingChoice) => 5,
        Some(AgentState::Working) => 4,
        Some(AgentState::Starting) => 3,
        Some(AgentState::Idle) => 2,
        Some(AgentState::Stopped) => 1,
        None => 0,
    }
}

fn host_relevant(
    host: &FleetHostSnapshot,
    topology: Option<&TopologySnapshot>,
    query: &str,
    attention_only: bool,
    show_paneless: bool,
) -> bool {
    if attention_only && visible_host_attention(host, topology, show_paneless) == 0 {
        return false;
    }
    let query = query.to_lowercase();
    query.is_empty()
        || searchable(&host.alias, &query)
        || host
            .hostname
            .as_deref()
            .is_some_and(|value| searchable(value, &query))
        || host
            .labels
            .iter()
            .any(|(key, value)| searchable(key, &query) || searchable(value, &query))
        || topology.is_some_and(|topology| {
            topology
                .sessions
                .iter()
                .any(|session| session_relevant(session, &query, false))
                || (show_paneless
                    && topology
                        .unassigned_agents
                        .iter()
                        .any(|agent| agent_relevant(agent, &query, false)))
        })
}

fn visible_host_attention(
    host: &FleetHostSnapshot,
    topology: Option<&TopologySnapshot>,
    show_paneless: bool,
) -> usize {
    topology.map_or_else(
        || host.needs_attention(),
        |topology| {
            let attached = topology
                .sessions
                .iter()
                .map(|session| distribution_attention(&session.states))
                .sum::<usize>();
            attached
                + usize::from(show_paneless)
                    * topology
                        .unassigned_agents
                        .iter()
                        .filter(|agent| needs_attention(agent.state))
                        .count()
        },
    )
}

fn session_relevant(session: &SessionNode, query: &str, attention_only: bool) -> bool {
    (!attention_only || distribution_attention(&session.states) > 0)
        && (query.is_empty()
            || searchable(&session.name, query)
            || session
                .windows
                .iter()
                .any(|window| window_relevant(window, query, false)))
}

fn window_relevant(window: &WindowNode, query: &str, attention_only: bool) -> bool {
    (!attention_only || distribution_attention(&window.states) > 0)
        && (query.is_empty()
            || searchable(&window.name, query)
            || window
                .panes
                .iter()
                .any(|pane| pane_relevant(pane, query, false)))
}

fn pane_relevant(pane: &PaneNode, query: &str, attention_only: bool) -> bool {
    let attention = pane
        .agent
        .as_ref()
        .is_some_and(|agent| needs_attention(agent.state));
    (!attention_only || attention)
        && (query.is_empty()
            || searchable(&pane.key.pane_id, query)
            || searchable(&pane.title, query)
            || searchable(&pane.cwd, query)
            || pane.agent.as_ref().is_some_and(|agent| {
                searchable(&agent.session_id, query)
                    || agent
                        .last_prompt
                        .as_deref()
                        .is_some_and(|value| searchable(value, query))
                    || agent
                        .ai_title
                        .as_deref()
                        .is_some_and(|value| searchable(value, query))
            }))
}

fn agent_relevant(agent: &muxa::Agent, query: &str, attention_only: bool) -> bool {
    (!attention_only || needs_attention(agent.state))
        && (query.is_empty()
            || searchable(&agent.kind.to_string(), query)
            || searchable(&agent.session_id, query)
            || agent
                .cwd
                .as_deref()
                .is_some_and(|value| searchable(value, query))
            || agent
                .last_prompt
                .as_deref()
                .is_some_and(|value| searchable(value, query))
            || agent
                .ai_title
                .as_deref()
                .is_some_and(|value| searchable(value, query)))
}

fn searchable(value: &str, lowercase_query: &str) -> bool {
    value.to_lowercase().contains(lowercase_query)
}

fn dominant_host_state(host: &FleetHostSnapshot) -> Option<AgentState> {
    let mut distribution = StateDistribution::default();
    if let Some(remote) = &host.remote {
        for agent in &remote.agents {
            match agent.state {
                AgentState::Starting => distribution.starting += 1,
                AgentState::Working => distribution.working += 1,
                AgentState::Idle => distribution.idle += 1,
                AgentState::WaitingInput => distribution.waiting_input += 1,
                AgentState::WaitingChoice => distribution.waiting_choice += 1,
                AgentState::Error => distribution.error += 1,
                AgentState::Stopped => distribution.stopped += 1,
            }
        }
    }
    dominant_distribution(&distribution)
}

fn dominant_distribution(distribution: &StateDistribution) -> Option<AgentState> {
    if distribution.error > 0 {
        Some(AgentState::Error)
    } else if distribution.waiting_choice > 0 {
        Some(AgentState::WaitingChoice)
    } else if distribution.waiting_input > 0 {
        Some(AgentState::WaitingInput)
    } else if distribution.working > 0 {
        Some(AgentState::Working)
    } else if distribution.starting > 0 {
        Some(AgentState::Starting)
    } else if distribution.idle > 0 {
        Some(AgentState::Idle)
    } else if distribution.stopped > 0 {
        Some(AgentState::Stopped)
    } else {
        None
    }
}

fn distribution_attention(distribution: &StateDistribution) -> usize {
    distribution.waiting_input + distribution.waiting_choice + distribution.error
}

fn needs_attention(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
    )
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let vertical = area.width < 100;
    let chunks = Layout::default()
        .direction(if vertical {
            Direction::Vertical
        } else {
            Direction::Horizontal
        })
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    if app.layout == WatchLayout::Swarm {
        render_swarm(frame, chunks[0], app);
    } else {
        render_tree(frame, chunks[0], app);
    }
    render_inspector(frame, chunks[1], app);
    if app.ask_panel {
        render_ask_panel(frame, area, app);
    }
    if app.mailbox.open {
        render_mailbox(frame, area, app);
    }
    if matches!(
        app.mode,
        InputMode::Message | InputMode::Ask | InputMode::Reply
    ) {
        render_composer(frame, area, app);
        if app.skill_palette.is_some() {
            render_skill_palette(frame, area, app);
        }
    }
    if app.skill_editor.is_some() {
        render_skill_editor(frame, area, app);
    }
    if let Some(name) = app.skill_delete_confirm.as_deref() {
        render_popup(
            frame,
            area,
            " delete message skill ",
            &format!("Delete /{name}?\n\ny/Enter confirm · n/Esc cancel"),
            app.theme,
        );
    }
    if let Some(capture) = &app.popup {
        render_popup(frame, area, " pane capture ", capture, app.theme);
    }
    if app.help {
        render_popup(
            frame,
            area,
            " fleet keys ",
            "↑/↓ · j/k siblings in focus; visible nodes otherwise\nJ/K previous/next agent pane across Fleet\nh/l collapse/expand    Space toggle\nEnter attach pane      p capture pane\na ask · A history      m message · M mailbox (b alias)\nTab kind · Ctrl-E mode i claim · e reply in mailbox\nr refresh host         c connect/disconnect\nAlt-a attention only   / search · ? help · q quit",
            app.theme,
        );
    }
}

fn render_tree(frame: &mut Frame, area: Rect, app: &App) {
    let theme = crate::watch::watch_theme(app.theme);
    let total_agents: usize = app
        .snapshot
        .hosts
        .iter()
        .map(FleetHostSnapshot::agent_count)
        .sum();
    let attention: usize = app
        .snapshot
        .hosts
        .iter()
        .map(FleetHostSnapshot::needs_attention)
        .sum();
    let selector = app
        .selector
        .as_deref()
        .map_or(String::new(), |selector| format!(" · -l {selector}"));
    let title = format!(
        " muxa fleet · {} hosts · {total_agents} agents · {attention} attention{selector} ",
        app.snapshot.hosts.len()
    );
    let widths = [
        Constraint::Percentage(48),
        Constraint::Length(12),
        Constraint::Percentage(52),
    ];
    let rows = app.rows.iter().map(|row| {
        let branch = if row.children == 0 {
            "•"
        } else if app.expanded.contains(&row.key) || !app.query.is_empty() || app.attention_only {
            "▾"
        } else {
            "▸"
        };
        let indent = "  ".repeat(row.depth);
        let node = Line::from(vec![
            Span::styled(format!("{indent}{branch} "), theme.dim_style()),
            Span::styled(
                safe_text(&row.label),
                if row.depth == 0 {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
        ]);
        let mut state = vec![Span::styled(
            row.state.map_or("○", state_marker),
            row.state
                .map_or_else(|| theme.dim_style(), |state| theme.state_style(state)),
        )];
        if row.attention > 0 {
            state.push(Span::styled(
                format!(" !{}", row.attention),
                theme.state_style(AgentState::Error),
            ));
        }
        Row::new(vec![
            Cell::from(node),
            Cell::from(Line::from(state)),
            Cell::from(Span::styled(safe_text(&row.detail), theme.dim_style())),
        ])
    });
    let footer = app.active_status().map_or_else(
        || match app.mode {
            InputMode::Search => format!(" search: {}_ ", app.query),
            _ if !app.query.is_empty() => format!(" filter: {} · Esc/q clear/quit ", app.query),
            _ if app.attention_only => " attention only · Alt-a/Esc clear ".into(),
            _ => {
                " j/k move · J/K agents · Enter attach · a/A ask · m/M collaborate · ? help ".into()
            }
        },
        |status| format!(" {} ", safe_text(status)),
    );
    let table = Table::new(rows, widths)
        .header(Row::new(["NODE", "STATE", "DETAIL"]).style(theme.table_header_style()))
        .block(
            Block::default()
                .title(title)
                .title_bottom(footer)
                .borders(Borders::ALL)
                .border_type(theme.border_type)
                .border_style(theme.border_style()),
        )
        .row_highlight_style(theme.selected_style())
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_swarm(frame: &mut Frame, area: Rect, app: &App) {
    let theme = crate::watch::watch_theme(app.theme);
    let nodes = all_swarm_keys(
        &app.snapshot,
        &app.topologies,
        &app.query,
        app.attention_only,
        app.sort,
        app.show_paneless,
    );
    if nodes.is_empty() {
        frame.render_widget(
            Paragraph::new("No agents or panes match the current Fleet filter.").block(
                Block::default()
                    .title(" muxa fleet · swarm ")
                    .borders(Borders::ALL)
                    .border_type(theme.border_type)
                    .border_style(theme.border_style()),
            ),
            area,
        );
        return;
    }
    let now = OffsetDateTime::now_utc();
    let rows = nodes
        .iter()
        .filter_map(|key| swarm_row(key, app, theme, now));
    let title = format!(
        " muxa fleet · swarm · {} hosts · {} rows ",
        app.snapshot.hosts.len(),
        nodes.len()
    );
    let footer = if app.query.is_empty() {
        " j/k move · Enter attach · a/A ask · m/M collaborate · / search · ? help ".to_string()
    } else {
        format!(" filter: {} · Esc clear ", app.query)
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Percentage(30),
            Constraint::Length(15),
            Constraint::Length(7),
            Constraint::Percentage(70),
        ],
    )
    .header(
        Row::new(["HOST", "AGENT / PANE", "STATE", "AGE", "SUMMARY"])
            .style(theme.table_header_style()),
    )
    .block(
        Block::default()
            .title(title)
            .title_bottom(footer)
            .borders(Borders::ALL)
            .border_type(theme.border_type)
            .border_style(theme.border_style()),
    )
    .row_highlight_style(theme.selected_style())
    .highlight_symbol("> ")
    .highlight_spacing(HighlightSpacing::Always);
    let selected = app
        .selected_key()
        .and_then(|selected| nodes.iter().position(|key| key == selected))
        .unwrap_or(0);
    let mut state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn swarm_row(
    key: &NodeKey,
    app: &App,
    theme: crate::watch::WatchThemeSpec,
    now: OffsetDateTime,
) -> Option<Row<'static>> {
    let host = key.host();
    let (agent, state, age, summary) = match key {
        NodeKey::Pane { key, .. } => {
            let pane = app
                .topologies
                .get(host)
                .and_then(|topology| find_pane(topology, key))?;
            pane.agent.as_ref().map_or_else(
                || {
                    (
                        safe_text(&pane.current_command),
                        Line::from(Span::styled("process", theme.dim_style())),
                        "-".into(),
                        safe_text(&pane.title),
                    )
                },
                |agent| {
                    (
                        safe_text(&format!("{} · {}", agent.kind, pane.key.pane_id)),
                        Line::from(Span::styled(
                            format!("{} {}", state_marker(agent.state), state_label(agent.state)),
                            theme.state_style(agent.state),
                        )),
                        format_age(agent.state_entered_at, now),
                        agent_summary(agent),
                    )
                },
            )
        }
        NodeKey::PanelessAgent {
            kind, session_id, ..
        } => {
            let agent = app
                .topologies
                .get(host)
                .and_then(|topology| find_paneless_agent(topology, kind, session_id))?;
            (
                format!("{} · paneless", agent.kind),
                Line::from(Span::styled(
                    format!("{} {}", state_marker(agent.state), state_label(agent.state)),
                    theme.state_style(agent.state),
                )),
                format_age(agent.state_entered_at, now),
                agent_summary(agent),
            )
        }
        _ => return None,
    };
    Some(Row::new(vec![
        Cell::from(safe_text(host)),
        Cell::from(agent),
        Cell::from(state),
        Cell::from(age),
        Cell::from(summary),
    ]))
}

fn render_inspector(frame: &mut Frame, area: Rect, app: &App) {
    let theme = crate::watch::watch_theme(app.theme);
    let block = Block::default()
        .title(Span::styled(" Inspector ", theme.accent_badge()))
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.border_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(key) = app.selected_key() else {
        frame.render_widget(Paragraph::new("No fleet hosts configured."), inner);
        return;
    };
    let host = app
        .snapshot
        .hosts
        .iter()
        .find(|host| host.alias == key.host());
    let topology = app.topologies.get(key.host());
    let mut lines = Vec::new();
    match key {
        NodeKey::Host(_) => {
            if let Some(host) = host {
                lines.extend(host_inspector(host));
            }
        }
        NodeKey::Session { key, .. } => {
            if let Some(session) = topology
                .and_then(|topology| topology.sessions.iter().find(|session| &session.key == key))
            {
                lines.extend(session_inspector(host, session));
            }
        }
        NodeKey::Window { key, .. } => {
            if let Some(window) = topology.and_then(|topology| find_window(topology, key)) {
                lines.extend(window_inspector(host, window));
            }
        }
        NodeKey::Pane { key, .. } => {
            if let Some(pane) = topology.and_then(|topology| find_pane(topology, key)) {
                lines.extend(pane_inspector(host, pane));
            }
        }
        NodeKey::PanelessAgent {
            kind, session_id, ..
        } => {
            if let Some(agent) =
                topology.and_then(|topology| find_paneless_agent(topology, kind, session_id))
            {
                lines.extend(paneless_agent_inspector(host, agent));
            }
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );

    if let NodeKey::Window { host, key } = key {
        if let Some((capture_host, capture)) = app
            .window_capture
            .as_ref()
            .filter(|(capture_host, capture)| capture_host == host && &capture.window == key)
        {
            let _ = capture_host;
            render_window_mosaic(frame, inner, capture, app.theme);
        }
    }
}

fn host_inspector(host: &FleetHostSnapshot) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading(format!("{}  {}", host.alias, host_state_label(host.state))),
        kv("ssh", &host.ssh_target),
        kv(
            "node",
            host.node_id.as_ref().map_or("-", muxa::NodeId::as_str),
        ),
        kv("hostname", host.hostname.as_deref().unwrap_or("-")),
        kv(
            "platform",
            &format!(
                "{}/{}",
                host.os.as_deref().unwrap_or("-"),
                host.arch.as_deref().unwrap_or("-")
            ),
        ),
        kv("version", host.muxa_version.as_deref().unwrap_or("-")),
        kv("mode", &format!("{:?}", host.mode).to_lowercase()),
        kv("labels", &format_map(&host.labels)),
        kv("annotations", &format_map(&host.annotations)),
        kv(
            "latency",
            &host
                .latency_ms
                .map_or_else(|| "-".into(), |value| format!("{value} ms")),
        ),
        kv(
            "last seen",
            &host.received_at.map_or_else(
                || "never".into(),
                |at| format_age(at, OffsetDateTime::now_utc()),
            ),
        ),
        kv(
            "inventory",
            &format!(
                "{} agents · {} panes · {} attention",
                host.agent_count(),
                host.pane_count(),
                host.needs_attention()
            ),
        ),
    ];
    if let Some(error) = &host.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            safe_text(error),
            Style::default().add_modifier(Modifier::BOLD),
        )));
    }
    lines
}

fn session_inspector(
    host: Option<&FleetHostSnapshot>,
    session: &SessionNode,
) -> Vec<Line<'static>> {
    let now = OffsetDateTime::now_utc();
    let agents = session
        .windows
        .iter()
        .flat_map(|window| &window.panes)
        .filter_map(|pane| pane.agent.as_ref())
        .collect::<Vec<_>>();
    let latest = agents
        .iter()
        .max_by_key(|agent| agent.last_activity_at)
        .map_or_else(
            || "-".into(),
            |agent| {
                format!(
                    "{} · {} · {}",
                    format_age(agent.last_activity_at, now),
                    agent.kind,
                    agent_summary(agent)
                )
            },
        );
    let processes: usize = agents
        .iter()
        .map(|agent| usize::from(agent.workload.process_count))
        .sum();
    let subagents: usize = agents
        .iter()
        .map(|agent| usize::from(agent.workload.subagent_count).max(agent.subagents.len()))
        .sum();
    let mut lines = vec![
        heading(format!(
            "{} › {}",
            host.map_or("?", |host| host.alias.as_str()),
            safe_text(&session.name)
        )),
        kv("session id", &session.key.session_id),
        kv("backend", &session.key.endpoint.host.to_string()),
        kv("endpoint", &session.key.endpoint.socket),
        kv(
            "topology",
            &format!(
                "{} windows · {} panes",
                session.windows.len(),
                session.pane_count()
            ),
        ),
        kv("states", &distribution_label(&session.states)),
        kv(
            "presence",
            &session
                .attached_clients
                .map_or_else(|| "unknown".into(), |count| format!("{count} clients")),
        ),
        kv(
            "load",
            &format!("{processes} processes · {subagents} subagents"),
        ),
        kv("latest", &latest),
        Line::from(""),
        heading("windows"),
    ];
    for window in &session.windows {
        lines.push(Line::from(format!(
            "  {}  {} panes · {}",
            safe_text(&window.name),
            window.panes.len(),
            distribution_label(&window.states)
        )));
        for pane in &window.panes {
            lines.push(Line::from(safe_text(&format!(
                "    {}  {} · {}",
                pane.key.pane_id,
                pane.agent.as_ref().map_or_else(
                    || safe_text(&pane.current_command),
                    |agent| format!("{} {}", agent.kind, state_label(agent.state))
                ),
                pane.agent
                    .as_ref()
                    .map_or_else(|| "-".into(), agent_summary),
            ))));
        }
    }
    lines
}

fn window_inspector(host: Option<&FleetHostSnapshot>, window: &WindowNode) -> Vec<Line<'static>> {
    let now = OffsetDateTime::now_utc();
    let agents = window
        .panes
        .iter()
        .filter_map(|pane| pane.agent.as_ref())
        .collect::<Vec<_>>();
    let latest = agents
        .iter()
        .max_by_key(|agent| agent.last_activity_at)
        .map_or_else(
            || "-".into(),
            |agent| {
                format!(
                    "{} · {} · {}",
                    format_age(agent.last_activity_at, now),
                    agent.kind,
                    agent_summary(agent)
                )
            },
        );
    let process_count: usize = agents
        .iter()
        .map(|agent| usize::from(agent.workload.process_count))
        .sum();
    let shell_count: usize = agents
        .iter()
        .map(|agent| usize::from(agent.workload.shell_count))
        .sum();
    let subagent_count: usize = agents
        .iter()
        .map(|agent| usize::from(agent.workload.subagent_count).max(agent.subagents.len()))
        .sum();
    let mut lines = vec![
        heading(format!(
            "{} › {}",
            host.map_or("?", |host| host.alias.as_str()),
            safe_text(&window.name)
        )),
        kv("window id", &window.key.window_id),
        kv("index", &window.index),
        kv("panes", &window.panes.len().to_string()),
        kv("states", &distribution_label(&window.states)),
        kv("cwd", window.cwd.as_deref().unwrap_or("mixed/unknown")),
        kv(
            "load",
            &format!(
                "{process_count} processes · {shell_count} shells · {subagent_count} subagents"
            ),
        ),
        kv("latest", &latest),
        kv("preview", "live layout · selected window only"),
    ];
    lines.push(Line::from(""));
    lines.push(heading("panes"));
    for pane in &window.panes {
        lines.push(Line::from(safe_text(&format!(
            "  {}  {}  {}",
            pane.key.pane_id,
            pane.agent
                .as_ref()
                .map_or("process", |agent| state_label(agent.state)),
            pane.agent
                .as_ref()
                .map_or_else(|| safe_text(&pane.current_command), agent_summary)
        ))));
    }
    lines
}

fn pane_inspector(host: Option<&FleetHostSnapshot>, pane: &PaneNode) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading(format!(
            "{} › {}",
            host.map_or("?", |host| host.alias.as_str()),
            pane.key.pane_id
        )),
        kv(
            "backend",
            &pane.key.window.session.endpoint.host.to_string(),
        ),
        kv("endpoint", &pane.key.window.session.endpoint.socket),
        kv("cwd", &pane.cwd),
        kv("command", &pane.current_command),
        kv("title", &pane.title),
    ];
    if let Some(agent) = &pane.agent {
        let now = OffsetDateTime::now_utc();
        lines.extend([
            kv("agent", &agent.kind.to_string()),
            kv(
                "state",
                &format!(
                    "{} · {}",
                    state_label(agent.state),
                    format_age(agent.state_entered_at, now)
                ),
            ),
            kv("activity", &format_age(agent.last_activity_at, now)),
            kv("agent session", &agent.session_id),
            kv("model", agent.model.as_deref().unwrap_or("-")),
            kv(
                "usage",
                &format!(
                    "ctx {} · cost {}",
                    agent
                        .context_used_pct
                        .map_or_else(|| "-".into(), |value| format!("{value:.0}%")),
                    agent
                        .cost_usd
                        .map_or_else(|| "-".into(), |value| format!("${value:.2}"))
                ),
            ),
            kv(
                "workload",
                &format!(
                    "{} processes · {} shells · {} subagents",
                    agent.workload.process_count,
                    agent.workload.shell_count,
                    usize::from(agent.workload.subagent_count).max(agent.subagents.len())
                ),
            ),
            Line::from(""),
            heading("last prompt"),
            Line::from(safe_text(agent.last_prompt.as_deref().unwrap_or("-"))),
            Line::from(""),
            heading("last response"),
            Line::from(safe_text(agent.last_response.as_deref().unwrap_or("-"))),
        ]);
        if !agent.workload.preview.is_empty() {
            lines.push(Line::from(""));
            lines.push(heading("process tree"));
            lines.extend(agent.workload.preview.iter().map(|process| {
                Line::from(format!(
                    "  {}pid {}  {}",
                    "  ".repeat(usize::from(process.depth.saturating_sub(1))),
                    process.pid,
                    safe_text(&process.command)
                ))
            }));
        }
    }
    lines
}

fn paneless_agent_inspector(
    host: Option<&FleetHostSnapshot>,
    agent: &muxa::Agent,
) -> Vec<Line<'static>> {
    let now = OffsetDateTime::now_utc();
    let mut lines = vec![
        heading(format!(
            "{} › {} (paneless)",
            host.map_or("?", |host| host.alias.as_str()),
            agent.kind
        )),
        kv("agent session", &agent.session_id),
        kv("cwd", agent.cwd.as_deref().unwrap_or("-")),
        kv(
            "state",
            &format!(
                "{} · {}",
                state_label(agent.state),
                format_age(agent.state_entered_at, now)
            ),
        ),
        kv("activity", &format_age(agent.last_activity_at, now)),
        kv("model", agent.model.as_deref().unwrap_or("-")),
        kv(
            "usage",
            &format!(
                "ctx {} · cost {}",
                agent
                    .context_used_pct
                    .map_or_else(|| "-".into(), |value| format!("{value:.0}%")),
                agent
                    .cost_usd
                    .map_or_else(|| "-".into(), |value| format!("${value:.2}"))
            ),
        ),
        Line::from(""),
        heading("last prompt"),
        Line::from(safe_text(agent.last_prompt.as_deref().unwrap_or("-"))),
        Line::from(""),
        heading("last response"),
        Line::from(safe_text(agent.last_response.as_deref().unwrap_or("-"))),
    ];
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "No pane is attached; attach, capture, and message actions are unavailable.",
        Style::default().add_modifier(Modifier::DIM),
    )));
    lines
}

fn agent_summary(agent: &muxa::Agent) -> String {
    safe_text(
        agent
            .recap
            .as_deref()
            .or(agent.ai_title.as_deref())
            .or(agent.last_prompt.as_deref())
            .or(agent.last_notification.as_deref())
            .unwrap_or("-"),
    )
    .replace('\n', " ")
}

fn render_window_mosaic(
    frame: &mut Frame,
    inner: Rect,
    capture: &FleetWindowCapture,
    watch_theme: WatchTheme,
) {
    if capture.panes.is_empty() || inner.height < 12 || inner.width < 32 {
        return;
    }
    // Keep the dense window summary (scope/cwd/load/latest) readable above
    // the live mosaic. The previous seven-line offset painted pane captures
    // over the final inspector fields as soon as they became richer.
    let top = inner.y.saturating_add(10).min(inner.bottom());
    let area = Rect::new(
        inner.x,
        top,
        inner.width,
        inner.bottom().saturating_sub(top),
    );
    if area.width < 10 || area.height < 4 {
        return;
    }
    frame.render_widget(Clear, area);
    let source_width = capture
        .panes
        .iter()
        .map(|pane| u32::from(pane.geometry.left) + u32::from(pane.geometry.width))
        .max()
        .unwrap_or(1);
    let source_height = capture
        .panes
        .iter()
        .map(|pane| u32::from(pane.geometry.top) + u32::from(pane.geometry.height))
        .max()
        .unwrap_or(1);
    for pane in &capture.panes {
        let Some(rect) = scale_geometry(
            pane.geometry.left,
            pane.geometry.top,
            pane.geometry.width,
            pane.geometry.height,
            source_width,
            source_height,
            area,
        ) else {
            continue;
        };
        let theme = crate::watch::watch_theme(watch_theme);
        let border = if pane.geometry.active {
            theme.accent_badge()
        } else {
            theme.border_style()
        };
        let body = pane
            .text
            .as_deref()
            .map_or_else(|| "(capture unavailable)".into(), safe_text);
        frame.render_widget(
            Paragraph::new(body)
                .block(
                    Block::default()
                        .title(format!(" {} ", safe_text(&pane.geometry.pane_id)))
                        .borders(Borders::ALL)
                        .border_type(theme.border_type)
                        .border_style(border),
                )
                .wrap(Wrap { trim: false }),
            rect,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn scale_geometry(
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    source_width: u32,
    source_height: u32,
    area: Rect,
) -> Option<Rect> {
    let scale = |value: u32, source: u32, target: u16| {
        u16::try_from(value.saturating_mul(u32::from(target)) / source.max(1)).unwrap_or(target)
    };
    let x = scale(u32::from(left), source_width, area.width);
    let y = scale(u32::from(top), source_height, area.height);
    let right = scale(u32::from(left) + u32::from(width), source_width, area.width)
        .max(x.saturating_add(1))
        .min(area.width);
    let bottom = scale(
        u32::from(top) + u32::from(height),
        source_height,
        area.height,
    )
    .max(y.saturating_add(1))
    .min(area.height);
    (x < area.width && y < area.height && right > x && bottom > y).then(|| {
        Rect::new(
            area.x.saturating_add(x),
            area.y.saturating_add(y),
            right - x,
            bottom - y,
        )
    })
}

fn render_composer(frame: &mut Frame, area: Rect, app: &App) {
    let theme = crate::watch::watch_theme(app.theme);
    let popup = centered(area, 76, 12);
    frame.render_widget(Clear, popup);
    let (title, footer): (String, String) = match app.mode {
        InputMode::Ask => (
            format!(" ask · {} ", app.ask_agent),
            " Enter ask · Tab agent · Shift-Enter newline · / skills · Esc cancel ".into(),
        ),
        InputMode::Reply => (
            format!(
                " reply · {} ",
                app.reply_request_id
                    .as_deref()
                    .map_or("request", short_request_id)
            ),
            " Enter reply · Shift-Enter newline · / skills · Esc cancel ".into(),
        ),
        InputMode::Message => {
            let path = app.selected_key().map_or_else(|| "agent".into(), key_path);
            (
                format!(
                    " message · {} · {} · {path} ",
                    request_kind_label(app.message_kind),
                    message_mode_label(app.message_mode)
                ),
                " Enter send · Shift-Enter newline · Tab kind · Ctrl-E mode · / skills · Esc cancel ".into(),
            )
        }
        InputMode::Normal | InputMode::Search => return,
    };
    frame.render_widget(
        Paragraph::new(format!("{}_", safe_text(&app.composer)))
            .block(
                Block::default()
                    .title(Span::styled(title, theme.action_badge()))
                    .title_bottom(footer)
                    .borders(Borders::ALL)
                    .border_type(theme.border_type)
                    .border_style(theme.border_style()),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

#[allow(clippy::too_many_lines)] // one modal renderer keeps list/detail layout coherent
fn render_mailbox(frame: &mut Frame, area: Rect, app: &App) {
    let theme = crate::watch::watch_theme(app.theme);
    let popup = centered(area, 92, 82);
    frame.render_widget(Clear, popup);
    let target = match (&app.mailbox.host, &app.mailbox.pane) {
        (Some(host), Some(pane)) => format!("{host} › {}", pane.pane_id),
        _ => "no agent selected".into(),
    };
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" mailbox ", theme.accent_badge()),
            Span::styled(
                format!(" incoming {} ", app.mailbox.incoming.len()),
                if app.mailbox.tab == MailboxTab::Incoming {
                    theme.action_badge()
                } else {
                    theme.dim_style()
                },
            ),
            Span::raw(" "),
            Span::styled(
                format!(" sent {} ", app.mailbox.sent.len()),
                if app.mailbox.tab == MailboxTab::Sent {
                    theme.action_badge()
                } else {
                    theme.dim_style()
                },
            ),
            Span::styled(format!(" · {target} "), theme.table_header_style()),
        ]))
        .title_bottom(" Tab inbox/sent · j/k move · i claim · e reply · r refresh · M/Esc close ")
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.border_style());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if app.mailbox.loading && app.mailbox.incoming.is_empty() && app.mailbox.sent.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("loading mailbox…", theme.dim_style())),
            inner,
        );
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(inner);
    let requests = app.mailbox_requests();
    let visible = usize::from(chunks[0].height).max(1);
    let start = app
        .mailbox
        .selected
        .saturating_add(1)
        .saturating_sub(visible);
    let rows = requests
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, request)| {
            let marker = if index == app.mailbox.selected {
                "> "
            } else {
                "  "
            };
            let peer = match app.mailbox.tab {
                MailboxTab::Incoming => request.from.label(),
                MailboxTab::Sent => request.to.label(),
            };
            Row::new(vec![
                Cell::from(marker),
                Cell::from(short_request_id(&request.id)),
                Cell::from(request_kind_label(request.kind)),
                Cell::from(request_status_label(request.status)),
                Cell::from(safe_text(&peer)),
                Cell::from(safe_text(&request.body).replace('\n', " ")),
            ])
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["", "ID", "KIND", "STATUS", "PEER", "MESSAGE"]).style(theme.table_header_style()),
    );
    frame.render_widget(table, chunks[0]);

    let detail = requests.get(app.mailbox.selected).map_or_else(
        || {
            if app.mailbox.loading {
                "refreshing…".into()
            } else {
                "no requests".into()
            }
        },
        |request| {
            let mut detail = format!(
                "{} · {} · {}\nfrom: {}\nto: {}\nmode: {:?}\ncreated: {}\n\n{}",
                request.id,
                request_kind_label(request.kind),
                request_status_label(request.status),
                request.from.label(),
                request.to.label(),
                request.work_mode,
                request.created_at,
                safe_text(&request.body)
            );
            if let Some(reply) = &request.reply {
                let _ = write!(
                    detail,
                    "\n\nreply · {}\n{}",
                    request_status_label(reply.status),
                    safe_text(&reply.body)
                );
            }
            detail
        },
    );
    frame.render_widget(
        Paragraph::new(safe_text(&detail))
            .block(
                Block::default()
                    .title(Span::styled(" selected request ", theme.dim_style()))
                    .borders(Borders::TOP)
                    .border_style(theme.border_style()),
            )
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
}

fn render_ask_panel(frame: &mut Frame, area: Rect, app: &App) {
    let theme = crate::watch::watch_theme(app.theme);
    let popup = centered(area, 92, 82);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ask ", theme.accent_badge()),
            Span::styled(
                format!(" {} entries ", app.ask_entries.len()),
                theme.table_header_style(),
            ),
        ]))
        .title_bottom(" a new · j/k move · r refresh · A/Esc close ")
        .borders(Borders::ALL)
        .border_type(theme.border_type)
        .border_style(theme.border_style());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if app.ask_entries.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "no questions yet — press a to ask one",
                theme.dim_style(),
            )),
            inner,
        );
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(inner);
    let visible_rows = usize::from(chunks[0].height).max(1);
    let start = app
        .ask_selected
        .saturating_sub(visible_rows.saturating_sub(1));
    let rows = app
        .ask_entries
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, entry)| {
            let status = match entry.status {
                AskStatus::Running => Span::styled("…", theme.state_style(AgentState::Working)),
                AskStatus::Answered => Span::styled("✓", theme.state_style(AgentState::Idle)),
                AskStatus::Failed => Span::styled("✗", theme.state_style(AgentState::Error)),
            };
            let marker = if index == app.ask_selected {
                "> "
            } else {
                "  "
            };
            Row::new(vec![
                Cell::from(Line::from(vec![Span::raw(marker), status])),
                Cell::from(entry.agent.clone()),
                Cell::from(safe_text(&entry.prompt).replace('\n', " ")),
            ])
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .header(Row::new(["", "AGENT", "QUESTION"]).style(theme.table_header_style()));
    frame.render_widget(table, chunks[0]);

    let entry = &app.ask_entries[app.ask_selected.min(app.ask_entries.len() - 1)];
    let answer = if let Some(error) = entry.error.as_deref() {
        format!("ask: {}\n\nerror: {}", entry.prompt, safe_text(error))
    } else if entry.answer.is_empty() {
        format!("ask: {}\n\n(waiting for answer)", entry.prompt)
    } else {
        format!("ask: {}\n\nanswer: {}", entry.prompt, entry.answer)
    };
    frame.render_widget(
        Paragraph::new(safe_text(&answer))
            .block(
                Block::default()
                    .title(Span::styled(" answer ", theme.accent_badge()))
                    .borders(Borders::TOP)
                    .border_style(theme.border_style()),
            )
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
}

fn render_skill_palette(frame: &mut Frame, area: Rect, app: &App) {
    let Some(palette) = app.skill_palette.as_ref() else {
        return;
    };
    let theme = crate::watch::watch_theme(app.theme);
    let matches = crate::message_skill::matching_skills(&app.message_skills, &palette.query);
    let popup = centered(area, 68, 48);
    frame.render_widget(Clear, popup);
    let visible = usize::from(popup.height.saturating_sub(3)).max(1);
    let start = palette.selected.saturating_add(1).saturating_sub(visible);
    let lines = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, (name, prompt))| {
            let marker = if index == palette.selected { ">" } else { " " };
            let summary = prompt.lines().next().unwrap_or_default();
            let line = format!("{marker} /{name:<24}  {}", safe_text(summary));
            if index == palette.selected {
                Line::from(Span::styled(line, theme.selected_style()))
            } else {
                Line::from(line)
            }
        })
        .collect::<Vec<_>>();
    let query = if palette.query.is_empty() {
        "all skills".into()
    } else {
        format!("/{}", palette.query)
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" skills · {query} "))
                    .title_bottom(
                        " type filter · ↑/↓ select · Enter insert · F2/Ctrl-A add · Del/Ctrl-D remove · Esc back ",
                    )
                    .borders(Borders::ALL)
                    .border_type(theme.border_type)
                    .border_style(theme.border_style()),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_skill_editor(frame: &mut Frame, area: Rect, app: &App) {
    let Some(editor) = app.skill_editor.as_ref() else {
        return;
    };
    let theme = crate::watch::watch_theme(app.theme);
    let popup = centered(area, 76, 42);
    frame.render_widget(Clear, popup);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(4)])
        .split(popup);
    let name_style = if editor.field == SkillEditorField::Name {
        theme.selected_style()
    } else {
        Style::default()
    };
    let prompt_style = if editor.field == SkillEditorField::Prompt {
        theme.selected_style()
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(format!("/{}_", safe_text(&editor.name)))
            .style(name_style)
            .block(
                Block::default()
                    .title(Span::styled(" name ", theme.table_header_style()))
                    .borders(Borders::ALL)
                    .border_type(theme.border_type)
                    .border_style(theme.border_style()),
            ),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(format!("{}_", safe_text(&editor.prompt)))
            .style(prompt_style)
            .block(
                Block::default()
                    .title(Span::styled(" prompt ", theme.table_header_style()))
                    .title_bottom(
                        " Tab field · Enter next/save · Shift-Enter newline · Esc cancel ",
                    )
                    .borders(Borders::ALL)
                    .border_type(theme.border_type)
                    .border_style(theme.border_style()),
            )
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
}

fn render_popup(frame: &mut Frame, area: Rect, title: &str, body: &str, watch_theme: WatchTheme) {
    let theme = crate::watch::watch_theme(watch_theme);
    let popup = centered(area, 82, 70);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body.to_string())
            .block(
                Block::default()
                    .title(title.to_string())
                    .title_bottom(" Esc/Enter close ")
                    .borders(Borders::ALL)
                    .border_type(theme.border_type)
                    .border_style(theme.border_style()),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn centered(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(20);
    let height = if height <= 100 {
        area.height
            .saturating_mul(height)
            .saturating_div(100)
            .max(6)
    } else {
        height.min(area.height)
    };
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

fn find_window<'a>(topology: &'a TopologySnapshot, key: &WindowKey) -> Option<&'a WindowNode> {
    topology
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .find(|window| &window.key == key)
}

fn find_pane<'a>(topology: &'a TopologySnapshot, key: &PaneKey) -> Option<&'a PaneNode> {
    topology
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .flat_map(|window| &window.panes)
        .find(|pane| &pane.key == key)
}

fn find_paneless_agent<'a>(
    topology: &'a TopologySnapshot,
    kind: &str,
    session_id: &str,
) -> Option<&'a muxa::Agent> {
    topology
        .unassigned_agents
        .iter()
        .find(|agent| agent.kind.to_string() == kind && agent.session_id == session_id)
}

fn key_path(key: &NodeKey) -> String {
    safe_text(&match key {
        NodeKey::Host(host) => host.clone(),
        NodeKey::Session { host, key } => format!("{host} › {}", key.session_id),
        NodeKey::Window { host, key } => {
            format!("{host} › {} › {}", key.session.session_id, key.window_id)
        }
        NodeKey::Pane { host, key } => format!(
            "{host} › {} › {} › {}",
            key.window.session.session_id, key.window.window_id, key.pane_id
        ),
        NodeKey::PanelessAgent {
            host,
            kind,
            session_id,
        } => format!("{host} › [paneless] {kind} › {session_id}"),
    })
}

fn heading(value: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        safe_text(&value.into()),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:<12}"),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::raw(safe_text(value)),
    ])
}

fn state_marker(state: AgentState) -> &'static str {
    match state {
        AgentState::Starting => "◌",
        AgentState::Working => "●",
        AgentState::Idle => "○",
        AgentState::WaitingInput => "▶",
        AgentState::WaitingChoice => "◆",
        AgentState::Error => "■",
        AgentState::Stopped => "×",
    }
}

fn request_kind_label(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Question => "question",
        RequestKind::Review => "review",
        RequestKind::Task => "task",
        RequestKind::Notice => "notice",
    }
}

fn message_mode_label(mode: WatchCollaborationMode) -> &'static str {
    match mode {
        WatchCollaborationMode::ReadOnly => "read only",
        WatchCollaborationMode::Execute => "execute",
        WatchCollaborationMode::JustSend => "just send",
    }
}

fn request_status_label(status: RequestStatus) -> &'static str {
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

fn short_request_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Starting => "starting",
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::WaitingInput => "waiting input",
        AgentState::WaitingChoice => "waiting choice",
        AgentState::Error => "error",
        AgentState::Stopped => "stopped",
    }
}

fn host_state_label(state: FleetHostState) -> &'static str {
    match state {
        FleetHostState::Disabled => "disabled",
        FleetHostState::Connecting => "connecting",
        FleetHostState::Online => "online",
        FleetHostState::Degraded => "degraded",
        FleetHostState::Offline => "offline",
        FleetHostState::AuthFailed => "auth failed",
        FleetHostState::VersionSkew => "version skew",
    }
}

fn distribution_label(distribution: &StateDistribution) -> String {
    let mut values = Vec::new();
    for (count, label) in [
        (distribution.error, "error"),
        (distribution.waiting_choice, "choice"),
        (distribution.waiting_input, "input"),
        (distribution.working, "working"),
        (distribution.idle, "idle"),
        (distribution.starting, "starting"),
        (distribution.stopped, "stopped"),
    ] {
        if count > 0 {
            values.push(format!("{count} {label}"));
        }
    }
    if values.is_empty() {
        "no agents".into()
    } else {
        values.join(" · ")
    }
}

fn format_map(values: &std::collections::BTreeMap<String, String>) -> String {
    if values.is_empty() {
        "-".into()
    } else {
        values
            .iter()
            .map(|(key, value)| format!("{}={}", safe_text(key), safe_text(value)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn format_age(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let seconds = u64::try_from((now - at).whole_seconds().max(0)).unwrap_or(0);
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// Strip C0 controls and common ANSI CSI/OSC sequences from every remote
/// string before it reaches the terminal renderer. A compromised host may
/// control pane titles and screen contents; it must not emit terminal control
/// sequences through the trusted central UI.
fn safe_text(value: &str) -> String {
    muxa::fleet::sanitize_terminal_text(value)
}

struct TerminalSession {
    terminal: FleetTerminal,
    active: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        terminal.clear()?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    fn suspend(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )?;
        self.terminal.show_cursor()?;
        self.active = false;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        self.terminal.clear()?;
        self.active = true;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(
                self.terminal.backend_mut(),
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            let _ = self.terminal.show_cursor();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::fleet::{FleetBackendInfo, HostAccessMode, NodeId, RemoteSnapshot};
    use muxa::tmux::PaneInfo;
    use ratatui::backend::TestBackend;

    fn host() -> FleetHostSnapshot {
        FleetHostSnapshot {
            alias: "dev".into(),
            local: false,
            ssh_target: "devbox".into(),
            labels: std::collections::BTreeMap::from([("tier".into(), "gpu".into())]),
            annotations: std::collections::BTreeMap::new(),
            mode: HostAccessMode::Control,
            state: FleetHostState::Online,
            node_id: Some(NodeId::generate()),
            hostname: Some("devbox".into()),
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            muxa_version: Some("0.8.34".into()),
            protocol: Some(1),
            capabilities: Vec::new(),
            daemon_generation: Some(0),
            boot_id: Some("boot".into()),
            latency_ms: Some(2),
            last_seen_at: Some(OffsetDateTime::now_utc()),
            received_at: Some(OffsetDateTime::now_utc()),
            error: None,
            remote: Some(RemoteSnapshot {
                revision: 1,
                observed_at: OffsetDateTime::now_utc(),
                agents: Vec::new(),
                panes: vec![PaneInfo {
                    session_group: None,
                    agent_role: None,
                    agent_alias: None,
                    workspace_id: None,
                    work_id: None,
                    pane_id: "%1".into(),
                    session_id: "$1".into(),
                    session: "work".into(),
                    window_id: "@1".into(),
                    window_name: "CAL-1".into(),
                    window_index: "0".into(),
                    pane_index: "0".into(),
                    tty: String::new(),
                    current_command: "codex".into(),
                    title: String::new(),
                    current_path: "/repo".into(),
                    pane_pid: 1,
                    socket: Some("default".into()),
                }],
                sessions: Vec::new(),
                backends: vec![FleetBackendInfo {
                    kind: HostKind::Tmux,
                    current_command: true,
                    pane_pid_map: true,
                    capture_pane: true,
                    focus_pane: true,
                    send_text: true,
                }],
            }),
        }
    }

    fn paneless_agent() -> muxa::Agent {
        serde_json::from_value(serde_json::json!({
            "kind": "codex",
            "agent_session_id": "detached-review",
            "pane": null,
            "cwd": "/repo",
            "state": "idle",
            "last_prompt": "review the changes",
            "last_response": null,
            "last_notification": null,
            "model": "gpt-5",
            "context_used_pct": null,
            "cost_usd": null,
            "started_at": "2026-08-20T00:00:00Z",
            "last_activity_at": "2026-08-20T00:01:00Z",
            "state_entered_at": "2026-08-20T00:01:00Z"
        }))
        .unwrap()
    }

    fn attached_agent(session_id: &str, pane_id: &str) -> muxa::Agent {
        let mut agent = paneless_agent();
        agent.session_id = session_id.into();
        agent.pane = Some(pane_id.into());
        agent.tmux_socket = Some("default".into());
        agent.state = AgentState::Idle;
        agent
    }

    #[test]
    fn focus_tree_keeps_structural_nodes_and_global_jump_targets_panes() {
        let host = host();
        let topology = host_topology(&host).unwrap();
        let mut app = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Window,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![host],
        });
        app.topologies.insert("dev".into(), topology);
        app.rebuild_rows(None);
        assert!(matches!(app.selected_key(), Some(NodeKey::Host(_))));
        app.jump_pane(1);
        assert!(matches!(app.selected_key(), Some(NodeKey::Pane { .. })));
        assert!(app
            .rows
            .iter()
            .any(|row| matches!(row.key, NodeKey::Session { .. })));
        assert!(app
            .rows
            .iter()
            .any(|row| matches!(row.key, NodeKey::Window { .. })));
    }

    #[test]
    fn fleet_message_contract_controls_match_native_watch() {
        assert_eq!(
            next_request_kind(RequestKind::Question),
            RequestKind::Review
        );
        assert_eq!(next_request_kind(RequestKind::Review), RequestKind::Task);
        assert_eq!(next_request_kind(RequestKind::Task), RequestKind::Notice);
        assert_eq!(
            next_request_kind(RequestKind::Notice),
            RequestKind::Question
        );
        assert_eq!(
            next_message_mode(WatchCollaborationMode::ReadOnly),
            WatchCollaborationMode::Execute
        );
        assert_eq!(
            next_message_mode(WatchCollaborationMode::Execute),
            WatchCollaborationMode::JustSend
        );
        assert_eq!(
            next_message_mode(WatchCollaborationMode::JustSend),
            WatchCollaborationMode::ReadOnly
        );

        let mut remote = host();
        let app = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Pane,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        let mut app = app;
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![remote.clone()],
        });
        assert!(!host_supports_collaboration(&app, "dev"));
        remote.capabilities.push("collaboration".into());
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![remote],
        });
        assert!(host_supports_collaboration(&app, "dev"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui]\ntheme = \"classic\"\n").unwrap();
        persist_message_defaults(&path, RequestKind::Task, WatchCollaborationMode::Execute)
            .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("collaboration_kind = \"task\""));
        assert!(saved.contains("collaboration_mode = \"execute\""));

        app.skill_editor = Some(SkillEditor {
            name: "review-plan".into(),
            prompt: "ask another agent to review the plan".into(),
            field: SkillEditorField::Prompt,
        });
        handle_skill_editor_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Some(&path),
            &mut app,
        );
        assert!(app.skill_editor.is_none());
        assert_eq!(
            app.message_skills.get("review-plan").map(String::as_str),
            Some("ask another agent to review the plan")
        );
        assert!(std::fs::read_to_string(path)
            .unwrap()
            .contains("review-plan"));
    }

    #[test]
    fn focus_navigation_matches_native_watch_siblings_and_singleton_fallback() {
        let mut remote_host = host();
        let remote = remote_host.remote.as_mut().unwrap();
        let mut second_window = remote.panes[0].clone();
        second_window.pane_id = "%2".into();
        second_window.window_id = "@2".into();
        second_window.window_name = "CAL-2".into();
        second_window.window_index = "1".into();
        let mut second_session = remote.panes[0].clone();
        second_session.pane_id = "%3".into();
        second_session.session_id = "$2".into();
        second_session.session = "other".into();
        second_session.window_id = "@3".into();
        second_session.window_name = "CAL-3".into();
        remote.panes.extend([second_window, second_session]);

        let mut app = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Pane,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![remote_host],
        });

        app.expand_or_child();
        assert!(matches!(app.selected_key(), Some(NodeKey::Session { .. })));
        let first_session = app.selected_key().cloned().unwrap();
        app.move_vertical(1);
        assert!(matches!(app.selected_key(), Some(NodeKey::Session { .. })));
        assert_ne!(app.selected_key(), Some(&first_session));
        app.move_vertical(-1);
        assert_eq!(app.selected_key(), Some(&first_session));

        let first_pane = all_pane_keys(
            &app.snapshot,
            &app.topologies,
            "",
            false,
            WatchSortKey::Name,
        )
        .into_iter()
        .find(|key| {
            matches!(
                key,
                NodeKey::Pane { key, .. }
                    if key.window.session.session_id == "$1" && key.window.window_id == "@1"
            )
        })
        .unwrap();
        app.reveal_key(first_pane);
        app.move_vertical(1);
        assert!(matches!(
            app.selected_key(),
            Some(NodeKey::Window { key, .. }) if key.window_id == "@2"
        ));
    }

    #[test]
    fn focus_navigation_moves_between_hosts_and_wraps() {
        let first = host();
        let mut second = host();
        second.alias = "prod".into();
        second.ssh_target = "prod-box".into();
        let mut app = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Window,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![second, first],
        });
        assert!(matches!(app.selected_key(), Some(NodeKey::Host(host)) if host == "dev"));
        app.move_vertical(1);
        assert!(matches!(app.selected_key(), Some(NodeKey::Host(host)) if host == "prod"));
        app.move_vertical(1);
        assert!(matches!(app.selected_key(), Some(NodeKey::Host(host)) if host == "dev"));
    }

    #[test]
    fn expansion_policy_respects_view_and_manual_navigation() {
        let snapshot = FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![host()],
        };

        let mut session_view = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Session,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        session_view.apply_snapshot(snapshot.clone());
        let session = session_view
            .rows
            .iter()
            .find_map(|row| matches!(&row.key, NodeKey::Session { .. }).then(|| row.key.clone()))
            .unwrap();
        session_view.focus_key(session);
        assert!(!session_view
            .rows
            .iter()
            .any(|row| matches!(row.key, NodeKey::Window { .. })));

        let mut window_view = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Window,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        window_view.apply_snapshot(snapshot.clone());
        let session = window_view
            .rows
            .iter()
            .find_map(|row| matches!(&row.key, NodeKey::Session { .. }).then(|| row.key.clone()))
            .unwrap();
        window_view.focus_key(session);
        let window = window_view
            .rows
            .iter()
            .find_map(|row| matches!(&row.key, NodeKey::Window { .. }).then(|| row.key.clone()))
            .unwrap();
        window_view.focus_key(window);
        assert!(!window_view
            .rows
            .iter()
            .any(|row| matches!(row.key, NodeKey::Pane { .. })));

        let mut manual = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Pane,
            WatchTreeExpansion::Manual,
            WatchSortKey::Name,
        );
        manual.apply_snapshot(snapshot);
        assert_eq!(manual.rows.len(), 1);
        manual.focus_key(NodeKey::Host("dev".into()));
        assert_eq!(
            manual.rows.len(),
            1,
            "ordinary movement must not expand manual trees"
        );
        manual.jump_pane(1);
        assert!(matches!(manual.selected_key(), Some(NodeKey::Pane { .. })));
        assert!(manual
            .rows
            .iter()
            .any(|row| matches!(row.key, NodeKey::Session { .. })));
    }

    #[test]
    fn parent_message_target_is_the_lowest_live_window_and_pane() {
        let mut remote_host = host();
        let remote = remote_host.remote.as_mut().unwrap();
        remote.panes[0].window_index = "1".into();
        let mut pane_nine = remote.panes[0].clone();
        pane_nine.pane_id = "%9".into();
        pane_nine.window_id = "@0".into();
        pane_nine.window_index = "0".into();
        pane_nine.pane_index = "9".into();
        let mut pane_two = pane_nine.clone();
        pane_two.pane_id = "%2".into();
        pane_two.pane_index = "2".into();
        remote.panes.extend([pane_nine, pane_two]);
        remote.agents.extend([
            attached_agent("agent-one", "%1"),
            attached_agent("agent-nine", "%9"),
            attached_agent("agent-two", "%2"),
        ]);

        let mut app = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Pane,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![remote_host],
        });
        let session = app
            .rows
            .iter()
            .find_map(|row| matches!(&row.key, NodeKey::Session { .. }).then(|| row.key.clone()))
            .unwrap();
        app.reveal_key(session);
        assert_eq!(
            app.selected_message_pane().map(|(_, pane)| pane.pane_id),
            Some("%2".into())
        );
        let window = app
            .rows
            .iter()
            .find_map(|row| match &row.key {
                NodeKey::Window { key, .. } if key.window_id == "@0" => Some(row.key.clone()),
                _ => None,
            })
            .unwrap();
        app.reveal_key(window);
        assert_eq!(
            app.selected_message_pane().map(|(_, pane)| pane.pane_id),
            Some("%2".into())
        );
    }

    #[test]
    fn fleet_message_work_identity_uses_only_a_complete_stamped_pair() {
        let mut remote_host = host();
        let pane = &mut remote_host.remote.as_mut().unwrap().panes[0];
        pane.workspace_id = Some("callabo".into());
        pane.work_id = Some("CAL-7345".into());
        let key = PaneKey::from_pane(HostKind::Tmux, pane);

        let mut app = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Pane,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![remote_host],
        });
        assert_eq!(
            app.work_identity_for_message_pane("dev", &key),
            (Some("callabo".into()), Some("CAL-7345".into()))
        );

        app.snapshot.hosts[0].remote.as_mut().unwrap().panes[0].work_id = None;
        assert_eq!(
            app.work_identity_for_message_pane("dev", &key),
            (None, None)
        );
    }

    #[test]
    fn remote_terminal_sequences_are_removed() {
        assert_eq!(safe_text("ok\u{1b}[31m red\u{1b}[0m"), "ok red");
        assert_eq!(safe_text("title\u{1b}]0;owned\u{7}safe"), "titlesafe");
        assert_eq!(
            key_path(&NodeKey::Pane {
                host: "dev\u{1b}[31m".into(),
                key: PaneKey {
                    window: WindowKey {
                        session: SessionKey {
                            endpoint: muxa::BackendEndpoint {
                                host: HostKind::Tmux,
                                socket: "default".into(),
                            },
                            session_id: "$1".into(),
                        },
                        window_id: "@1".into(),
                    },
                    pane_id: "%1\u{1b}]0;owned\u{7}".into(),
                },
            }),
            "dev › $1 › @1 › %1"
        );
    }

    #[test]
    fn lone_local_host_uses_the_full_native_watch() {
        let mut local = host();
        local.alias = "local".into();
        local.local = true;
        let local_only = FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![local.clone()],
        };
        assert!(uses_native_local_watch(&local_only));

        let mut remote = host();
        remote.alias = "dev".into();
        assert!(!uses_native_local_watch(&FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![local, remote],
        }));
    }

    #[test]
    fn multi_host_tree_pins_local_first_and_swarm_uses_shared_renderer() {
        let mut local = host();
        local.alias = "local".into();
        local.local = true;
        let mut remote = host();
        remote.alias = "aaa-remote".into();

        let snapshot = FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![remote, local],
        };
        let mut app = App::new(
            None,
            WatchTheme::OhMyMuxa,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Window,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        app.apply_snapshot(snapshot.clone());
        assert_eq!(app.snapshot.hosts[0].alias, "local");
        assert!(matches!(app.rows[0].key, NodeKey::Host(ref host) if host == "local"));

        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let tree = terminal.backend().to_string();
        assert!(tree.contains("muxa fleet"));
        assert!(tree.contains("local"));
        assert!(tree.contains("aaa-remote"));

        let mut swarm = App::new(
            None,
            WatchTheme::OhMyMuxa,
            BTreeMap::new(),
            WatchLayout::Swarm,
            WatchView::Pane,
            WatchTreeExpansion::Manual,
            WatchSortKey::Name,
        );
        swarm.apply_snapshot(snapshot);
        terminal.draw(|frame| render(frame, &swarm)).unwrap();
        let swarm_screen = terminal.backend().to_string();
        assert!(swarm_screen.contains("swarm"));
        assert!(swarm_screen.contains("HOST"));
        assert!(swarm_screen.contains("AGENT / PANE"));
    }

    #[test]
    fn paneless_agents_are_explicit_and_only_shown_when_requested() {
        let mut remote = host();
        remote
            .remote
            .as_mut()
            .unwrap()
            .agents
            .push(paneless_agent());
        let snapshot = FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![remote],
        };

        let mut hidden = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Tree,
            WatchView::Window,
            WatchTreeExpansion::Focus,
            WatchSortKey::Name,
        );
        hidden.apply_snapshot(snapshot.clone());
        assert!(!hidden
            .rows
            .iter()
            .any(|row| matches!(row.key, NodeKey::PanelessAgent { .. })));

        let mut shown = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Swarm,
            WatchView::Pane,
            WatchTreeExpansion::Manual,
            WatchSortKey::Name,
        );
        shown.show_paneless = true;
        shown.apply_snapshot(snapshot);
        assert!(matches!(
            shown.selected_key(),
            Some(NodeKey::Pane { .. } | NodeKey::PanelessAgent { .. })
        ));
        assert!(all_swarm_keys(
            &shown.snapshot,
            &shown.topologies,
            "detached-review",
            false,
            WatchSortKey::Name,
            true
        )
        .iter()
        .any(|key| matches!(key, NodeKey::PanelessAgent { .. })));

        shown.layout = WatchLayout::Tree;
        shown.query = "detached-review".into();
        shown.rebuild_rows(None);
        assert!(shown
            .rows
            .iter()
            .any(|row| matches!(row.key, NodeKey::Host(_))));
        assert!(shown
            .rows
            .iter()
            .any(|row| matches!(row.key, NodeKey::PanelessAgent { .. })));
    }

    #[test]
    fn swarm_rebuild_never_leaves_a_structural_action_target_selected() {
        let mut remote = host();
        remote
            .remote
            .as_mut()
            .unwrap()
            .agents
            .push(paneless_agent());
        let mut app = App::new(
            None,
            WatchTheme::Classic,
            BTreeMap::new(),
            WatchLayout::Swarm,
            WatchView::Pane,
            WatchTreeExpansion::Manual,
            WatchSortKey::Name,
        );
        app.show_paneless = true;
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![remote],
        });
        assert!(matches!(
            app.selected_key(),
            Some(NodeKey::Pane { .. } | NodeKey::PanelessAgent { .. })
        ));
        app.query = "detached-review".into();
        app.rebuild_rows(None);
        assert!(matches!(
            app.selected_key(),
            Some(NodeKey::PanelessAgent { .. })
        ));
    }
}
