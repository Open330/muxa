//! Central host → session → window → pane(agent) Fleet TUI.

use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{Frame, Terminal};
use time::OffsetDateTime;
use tokio::sync::mpsc;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const WINDOW_CAPTURE_INTERVAL: Duration = Duration::from_secs(2);
const INPUT_POLL: Duration = Duration::from_millis(50);

type FleetTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NodeKey {
    Host(String),
    Session { host: String, key: SessionKey },
    Window { host: String, key: WindowKey },
    Pane { host: String, key: PaneKey },
}

impl NodeKey {
    fn host(&self) -> &str {
        match self {
            Self::Host(host)
            | Self::Session { host, .. }
            | Self::Window { host, .. }
            | Self::Pane { host, .. } => host,
        }
    }

    fn parent(&self) -> Option<Self> {
        match self {
            Self::Host(_) => None,
            Self::Session { host, .. } => Some(Self::Host(host.clone())),
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
        !matches!(self, Self::Pane { .. })
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
}

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
    window_capture_pending: bool,
    last_window_capture: Option<Instant>,
}

impl App {
    fn new(selector: Option<String>) -> Self {
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
            window_capture_pending: false,
            last_window_capture: None,
        }
    }

    fn apply_snapshot(&mut self, mut snapshot: FleetSnapshot) {
        let selected = self.selected_key().cloned();
        snapshot.hosts.sort_by(|left, right| {
            right
                .needs_attention()
                .cmp(&left.needs_attention())
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
        if self.expanded.is_empty() {
            if let Some(host) = self.snapshot.hosts.first() {
                self.expanded.insert(NodeKey::Host(host.alias.clone()));
            }
        }
        self.rebuild_rows(selected.as_ref());
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

    fn selected_window(&self) -> Option<(String, WindowKey)> {
        match self.selected_key()? {
            NodeKey::Window { host, key } => Some((host.clone(), key.clone())),
            _ => None,
        }
    }

    fn rebuild_rows(&mut self, preserve: Option<&NodeKey>) {
        self.rows = build_rows(
            &self.snapshot,
            &self.topologies,
            &self.expanded,
            &self.query,
            self.attention_only,
        );
        if let Some(key) = preserve {
            if let Some(index) = self.rows.iter().position(|row| &row.key == key) {
                self.selected = index;
                return;
            }
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn focus_key(&mut self, key: NodeKey) {
        self.expanded.clear();
        for ancestor in key.ancestors() {
            self.expanded.insert(ancestor);
        }
        if key.is_parent() {
            self.expanded.insert(key.clone());
        }
        self.rebuild_rows(Some(&key));
    }

    fn move_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
        if let Some(key) = self.selected_key().cloned() {
            self.focus_key(key);
        }
    }

    /// Vim `j/k` jumps between actionable agent panes. Arrow keys retain
    /// access to every host/session/window inspector, so a single-child tree
    /// does not force the common path through four structural rows.
    fn move_pane(&mut self, delta: isize) {
        let panes = all_pane_keys(
            &self.snapshot,
            &self.topologies,
            &self.query,
            self.attention_only,
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
        let next = match (index, delta.is_negative()) {
            (Some(index), false) => (index + 1).min(panes.len() - 1),
            (Some(index), true) => index.saturating_sub(1),
            (None, false) => 0,
            (None, true) => panes.len() - 1,
        };
        self.focus_key(panes[next].clone());
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
}

enum BackgroundResult {
    Window {
        host: String,
        window: WindowKey,
        result: std::result::Result<FleetCommandResult, String>,
    },
    PaneCapture(std::result::Result<FleetCommandResult, String>),
    Command(std::result::Result<FleetCommandResult, String>),
}

pub(crate) async fn run(client: Client, cfg: &Config, selector: Option<String>) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    let mut app = App::new(selector);
    let (background_tx, mut background_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let mut last_refresh = now.checked_sub(REFRESH_INTERVAL).unwrap_or(now);
    let mut quit = false;

    while !quit {
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            match client.fleet_snapshot(app.selector.as_deref()).await {
                Ok(snapshot) => app.apply_snapshot(snapshot),
                Err(error) => app.status(format!("fleet refresh failed: {error}")),
            }
            last_refresh = Instant::now();
        }

        while let Ok(result) = background_rx.try_recv() {
            match result {
                BackgroundResult::Window {
                    host,
                    window,
                    result,
                } => {
                    app.window_capture_pending = false;
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
            }
        }

        maybe_capture_window(&client, &mut app, &background_tx);
        terminal.terminal.draw(|frame| render(frame, &app))?;

        if event::poll(INPUT_POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    quit = handle_key(key, &client, cfg, &mut terminal, &mut app, &background_tx)?;
                }
                Event::Paste(text) => match app.mode {
                    InputMode::Search => {
                        app.query.push_str(&safe_text(&text).replace('\n', " "));
                        app.rebuild_rows(None);
                    }
                    InputMode::Message => app.composer.push_str(&text),
                    InputMode::Normal => {}
                },
                Event::Resize(_, _)
                | Event::Mouse(_)
                | Event::FocusGained
                | Event::FocusLost
                | Event::Key(_) => {}
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // one keymap is easier to audit than scattered state handlers
fn handle_key(
    key: KeyEvent,
    client: &Client,
    cfg: &Config,
    terminal: &mut TerminalSession,
    app: &mut App,
    background: &mpsc::UnboundedSender<BackgroundResult>,
) -> Result<bool> {
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
            match key.code {
                KeyCode::Esc => {
                    app.mode = InputMode::Normal;
                    app.composer.clear();
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    app.composer.push('\n');
                }
                KeyCode::Enter => {
                    let Some((host, pane)) = app.selected_pane() else {
                        app.mode = InputMode::Normal;
                        return Ok(false);
                    };
                    if app.composer.trim().is_empty() {
                        app.status("message is empty");
                        return Ok(false);
                    }
                    let text = std::mem::take(&mut app.composer);
                    app.mode = InputMode::Normal;
                    let client = client.clone();
                    let sender = background.clone();
                    tokio::spawn(async move {
                        let result = client
                            .fleet_execute(
                                &host,
                                &FleetOperation::SendPrompt {
                                    pane,
                                    text,
                                    submit: true,
                                },
                            )
                            .await
                            .map_err(|error| error.to_string());
                        let _ = sender.send(BackgroundResult::Command(result));
                    });
                    app.status("sending prompt…");
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
        InputMode::Normal => {}
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
        KeyCode::Down => app.move_row(1),
        KeyCode::Up => app.move_row(-1),
        KeyCode::Char('j') => app.move_pane(1),
        KeyCode::Char('k') => app.move_pane(-1),
        KeyCode::Home | KeyCode::Char('g') => {
            if let Some(first) = app.rows.first().map(|row| row.key.clone()) {
                app.focus_key(first);
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            if let Some(last) = app.rows.last().map(|row| row.key.clone()) {
                app.focus_key(last);
            }
        }
        KeyCode::Left | KeyCode::Char('h') => app.collapse_or_parent(),
        KeyCode::Right | KeyCode::Char('l') => app.expand_or_child(),
        KeyCode::Char(' ') => app.toggle_selected(),
        KeyCode::Char('/') => app.mode = InputMode::Search,
        KeyCode::Char('a') => {
            app.attention_only = !app.attention_only;
            app.rebuild_rows(None);
        }
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
        KeyCode::Char('p') => {
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
            if app.selected_pane().is_some() {
                app.mode = InputMode::Message;
                app.composer.clear();
            } else {
                app.status("select a pane before sending a prompt");
            }
        }
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
                let result = crate::fleet_cli::attach_exact(cfg, &host_alias, &target);
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
        app.window_capture_pending = false;
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
    if !due || app.window_capture_pending {
        return;
    }
    app.window_capture_pending = true;
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
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for host in &snapshot.hosts {
        let topology = topologies.get(&host.alias);
        if !host_relevant(host, topology, query, attention_only) {
            continue;
        }
        let host_key = NodeKey::Host(host.alias.clone());
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
            attention: host.needs_attention(),
            children: topology.map_or(0, |topology| topology.sessions.len()),
        });
        let show_host = expanded.contains(&host_key) || !query.is_empty() || attention_only;
        if !show_host {
            continue;
        }
        let Some(topology) = topology else {
            continue;
        };
        for session in &topology.sessions {
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
            for window in &session.windows {
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
                for pane in &window.panes {
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
    }
    rows
}

fn all_pane_keys(
    snapshot: &FleetSnapshot,
    topologies: &HashMap<String, TopologySnapshot>,
    query: &str,
    attention_only: bool,
) -> Vec<NodeKey> {
    snapshot
        .hosts
        .iter()
        .filter(|host| host_relevant(host, topologies.get(&host.alias), query, attention_only))
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
        .collect()
}

fn host_relevant(
    host: &FleetHostSnapshot,
    topology: Option<&TopologySnapshot>,
    query: &str,
    attention_only: bool,
) -> bool {
    if attention_only && host.needs_attention() == 0 {
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
        })
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
    render_tree(frame, chunks[0], app);
    render_inspector(frame, chunks[1], app);
    if app.mode == InputMode::Message {
        render_composer(frame, area, app);
    }
    if let Some(capture) = &app.popup {
        render_popup(frame, area, " pane capture ", capture);
    }
    if app.help {
        render_popup(
            frame,
            area,
            " fleet keys ",
            "↑/↓ every node    j/k previous/next agent pane\nh/l collapse/expand    Space toggle\nEnter attach pane      p capture pane\nm send prompt          r refresh host\nc connect/disconnect   a attention only\n/ search               ? help    q quit",
        );
    }
}

fn render_tree(frame: &mut Frame, area: Rect, app: &App) {
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
        let marker = row.state.map_or("○", state_marker);
        let attention = if row.attention > 0 {
            format!(" !{}", row.attention)
        } else {
            String::new()
        };
        Row::new(vec![
            Cell::from(format!("{indent}{branch} {}", safe_text(&row.label))),
            Cell::from(format!("{marker}{attention}")),
            Cell::from(safe_text(&row.detail)),
        ])
    });
    let footer = app.active_status().map_or_else(
        || match app.mode {
            InputMode::Search => format!(" search: {}_ ", app.query),
            _ if !app.query.is_empty() => format!(" filter: {} · Esc/q clear/quit ", app.query),
            _ if app.attention_only => " attention only · a clear ".into(),
            _ => " ↑↓ nodes · j/k agents · Enter attach · m message · ? help ".into(),
        },
        |status| format!(" {} ", safe_text(status)),
    );
    let table = Table::new(rows, widths)
        .header(
            Row::new(["NODE", "STATE", "DETAIL"]).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(
            Block::default()
                .title(title)
                .title_bottom(footer)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(45, 52, 66))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_inspector(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" inspector ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
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
            render_window_mosaic(frame, inner, capture);
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
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

fn session_inspector(
    host: Option<&FleetHostSnapshot>,
    session: &SessionNode,
) -> Vec<Line<'static>> {
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
        for pane in window.panes.iter().take(3) {
            lines.push(Line::from(format!(
                "    {}  {}",
                pane.key.pane_id,
                pane.agent.as_ref().map_or_else(
                    || safe_text(&pane.current_command),
                    |agent| format!("{} {}", agent.kind, state_label(agent.state))
                )
            )));
        }
    }
    lines
}

fn window_inspector(host: Option<&FleetHostSnapshot>, window: &WindowNode) -> Vec<Line<'static>> {
    vec![
        heading(format!(
            "{} › {}",
            host.map_or("?", |host| host.alias.as_str()),
            safe_text(&window.name)
        )),
        kv("window id", &window.key.window_id),
        kv("index", &window.index),
        kv("panes", &window.panes.len().to_string()),
        kv("states", &distribution_label(&window.states)),
        kv("preview", "live layout · selected window only"),
    ]
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
        lines.extend([
            kv("agent", &agent.kind.to_string()),
            kv("state", state_label(agent.state)),
            kv("agent session", &agent.session_id),
            kv("model", agent.model.as_deref().unwrap_or("-")),
            Line::from(""),
            heading("latest"),
            Line::from(safe_text(
                agent
                    .last_response
                    .as_deref()
                    .or(agent.last_prompt.as_deref())
                    .or(agent.last_notification.as_deref())
                    .unwrap_or("(no prompt or response recorded)"),
            )),
        ]);
    }
    lines
}

fn render_window_mosaic(frame: &mut Frame, inner: Rect, capture: &FleetWindowCapture) {
    if capture.panes.is_empty() || inner.height < 12 || inner.width < 32 {
        return;
    }
    let top = inner.y.saturating_add(7).min(inner.bottom());
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
        let border = if pane.geometry.active {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let body = pane
            .text
            .as_deref()
            .map_or_else(|| "(capture unavailable)".into(), safe_text);
        frame.render_widget(
            Paragraph::new(body)
                .block(
                    Block::default()
                        .title(format!(" {} ", pane.geometry.pane_id))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border)),
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
    let popup = centered(area, 76, 12);
    frame.render_widget(Clear, popup);
    let title = app.selected_key().map_or_else(
        || " message ".into(),
        |key| format!(" message · {} ", key_path(key)),
    );
    frame.render_widget(
        Paragraph::new(format!("{}_", app.composer))
            .block(
                Block::default()
                    .title(title)
                    .title_bottom(" Enter send · Shift-Enter newline · Esc cancel ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_popup(frame: &mut Frame, area: Rect, title: &str, body: &str) {
    let popup = centered(area, 82, 70);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body.to_string())
            .block(
                Block::default()
                    .title(title.to_string())
                    .title_bottom(" Esc/Enter close ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan)),
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

fn key_path(key: &NodeKey) -> String {
    match key {
        NodeKey::Host(host) => host.clone(),
        NodeKey::Session { host, key } => format!("{host} › {}", key.session_id),
        NodeKey::Window { host, key } => {
            format!("{host} › {} › {}", key.session.session_id, key.window_id)
        }
        NodeKey::Pane { host, key } => format!(
            "{host} › {} › {} › {}",
            key.window.session.session_id, key.window.window_id, key.pane_id
        ),
    }
}

fn heading(value: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        safe_text(&value.into()),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<12}"), Style::default().fg(Color::DarkGray)),
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

    #[test]
    fn focus_tree_keeps_structural_nodes_but_j_targets_panes() {
        let host = host();
        let topology = host_topology(&host).unwrap();
        let mut app = App::new(None);
        app.apply_snapshot(FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts: vec![host],
        });
        app.topologies.insert("dev".into(), topology);
        app.rebuild_rows(None);
        assert!(matches!(app.selected_key(), Some(NodeKey::Host(_))));
        app.move_pane(1);
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
    fn remote_terminal_sequences_are_removed() {
        assert_eq!(safe_text("ok\u{1b}[31m red\u{1b}[0m"), "ok red");
        assert_eq!(safe_text("title\u{1b}]0;owned\u{7}safe"), "titlesafe");
    }
}
