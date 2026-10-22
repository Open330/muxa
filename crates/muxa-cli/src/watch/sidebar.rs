//! Compact, read-only watch surface embedded in a native side pane.
use super::{
    apply_outcome, compute_refresh, refresh_task, setup_terminal, state_marker, tree_node_label,
    watch_theme, App, RefreshOutcome, Spinner, TerminalGuard, TopologyNodeKey, TopologyNodeRef,
    WatchConfig, WatchLayout, WatchTreeExpansion, WatchView,
};
use crate::sidebar::UiLaunch;
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use muxa::ipc::Client;
use muxa::state::Agent;
use muxa::{BackendEndpoint, HostKind};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};
use std::{
    io,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::mpsc;

struct SidebarContext {
    endpoint: BackendEndpoint,
    own: String,
    launch: UiLaunch,
}

#[derive(Default)]
struct Active {
    session: String,
    window: String,
    pane: String,
}
impl Active {
    fn read(context: &SidebarContext) -> Option<Self> {
        let control = crate::mux_control::Control::Endpoint(context.endpoint.clone());
        let source = context.own.strip_prefix("rmux:").unwrap_or(&context.own);
        let row = control
            .output(&[
                "display-message",
                "-p",
                "-t",
                source,
                "#{session_id}\t#{window_id}\t#{@muxa_sidebar_last_pane}",
            ])
            .ok()?;
        let mut fields = row.trim_end().split('\t');
        Some(Self {
            session: fields.next()?.into(),
            window: fields.next()?.into(),
            pane: fields.next().unwrap_or_default().into(),
        })
    }
    fn marker(&self, key: &TopologyNodeKey) -> &'static str {
        match key {
            TopologyNodeKey::Pane(pane)
                if pane.window.session.session_id == self.session
                    && pane.window.window_id == self.window
                    && pane.pane_id.strip_prefix("rmux:").unwrap_or(&pane.pane_id) == self.pane =>
            {
                "▶ "
            }
            TopologyNodeKey::Window(window)
                if window.session.session_id == self.session && window.window_id == self.window =>
            {
                "◆ "
            }
            TopologyNodeKey::Session(session) if session.session_id == self.session => "◆ ",
            _ => "  ",
        }
    }
}

fn sync_sort(app: &mut App, launch: &UiLaunch, modified: &mut Option<(SystemTime, u64)>) {
    let Some(path) = &launch.config else {
        return;
    };
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let Ok(time) = metadata.modified() else {
        return;
    };
    let stamp = (time, metadata.len());
    if *modified == Some(stamp) {
        return;
    }
    if let Ok(config) = muxa::config::Config::load(path) {
        *modified = Some(stamp);
        if app.watch_cfg.sort != config.watch.sort {
            let selected = app.selection_identity_for_rebuild();
            app.watch_cfg.sort = config.watch.sort;
            app.resort_rows_restoring_selection(selected);
        }
    }
}

fn toggle_branch(app: &mut App, key: &TopologyNodeKey) {
    if !app.expanded_nodes.remove(key) {
        app.expanded_nodes.insert(key.clone());
    }
    app.pin_filtered_tree_selection();
}

fn open_watch(context: &SidebarContext, app: &App) -> Result<()> {
    let control = crate::mux_control::Control::Endpoint(context.endpoint.clone());
    let source = context.own.strip_prefix("rmux:").unwrap_or(&context.own);
    let client = crate::sidebar::focused_client(&control, source)?;
    let selected = match app.selected_node_key() {
        Some(TopologyNodeKey::Pane(pane)) => pane.pane_id.clone(),
        _ => crate::sidebar::watch_source(&control, source).unwrap_or_else(|| source.into()),
    };
    let mut words = context.launch.words()?;
    words.extend([
        "watch".into(),
        "--caller-client".into(),
        client.clone(),
        "--caller-pane".into(),
        selected,
    ]);
    let command = muxa::shell::Dialect::native().command_line(&words);
    control.run(&[
        "display-popup",
        "-c",
        &client,
        "-B",
        "-E",
        "-w",
        "100%",
        "-h",
        "99%",
        "-x",
        "0",
        "-y",
        "0",
        &command,
    ])
}

fn on_server(agent: &Agent, endpoint: &BackendEndpoint) -> bool {
    agent.pane.as_deref().is_some_and(|pane| {
        muxa::backend::pane_id_host_kind(pane).unwrap_or(HostKind::Tmux) == endpoint.host
            && agent.tmux_socket.as_deref().is_some_and(|socket| {
                muxa::backend::pane_endpoints_match(Some(pane), socket, &endpoint.socket)
            })
    })
}
fn filter(
    outcome: RefreshOutcome,
    endpoint: &BackendEndpoint,
    own: &str,
) -> Option<RefreshOutcome> {
    match outcome {
        RefreshOutcome::Full(mut full) => {
            full.agents
                .retain(|agent| on_server(agent, endpoint) && agent.pane.as_deref() != Some(own));
            // A sidebar is infrastructure, never an agent/tree child. Limit the
            // compact surface to panes that actually have tracked agents.
            full.panes.retain(|pane| {
                full.agents
                    .iter()
                    .any(|agent| agent.pane.as_deref() == Some(&pane.pane_id))
            });
            full.sessions.clear();
            Some(RefreshOutcome::Full(full))
        }
        RefreshOutcome::Agents(mut agents) => {
            agents.retain(|agent| on_server(agent, endpoint) && agent.pane.as_deref() != Some(own));
            Some(RefreshOutcome::Agents(agents))
        }
        RefreshOutcome::SingleAgent(agent) => (on_server(&agent, endpoint)
            && agent.pane.as_deref() != Some(own))
        .then_some(RefreshOutcome::SingleAgent(agent)),
    }
}
fn draw(frame: &mut Frame, app: &mut App, message: &str, state: &mut ListState, active: &Active) {
    let area = frame.area();
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let targets = app.tree_targets();
    let spin = Spinner {
        frame: 0,
        enabled: false,
    };
    let items: Vec<ListItem> = targets
        .iter()
        .filter_map(|target| {
            let node = app.topology.find(&target.key)?;
            let mut spans = vec![Span::styled(
                active.marker(&target.key),
                theme
                    .dim_style()
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )];
            if let TopologyNodeRef::Pane(pane) = node {
                let (glyph, style) = pane.agent.as_ref().map_or_else(
                    || ("○", theme.dim_style()),
                    |agent| state_marker(agent.state, theme, spin),
                );
                spans.push(Span::styled(format!("{glyph} "), style));
            }
            spans.extend(
                tree_node_label(target, node, false, theme)
                    .lines
                    .into_iter()
                    .flat_map(|line| line.spans),
            );
            Some(ListItem::new(Line::from(spans)))
        })
        .collect();
    state.select(app.table_state.selected());
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Muxa agents · s watch ")
                .border_style(theme.border_style()),
        )
        .highlight_style(theme.selected_style())
        .highlight_symbol("> ");
    let body = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    frame.render_stateful_widget(list, body, state);
    if area.height > 0 {
        let footer = if message.is_empty() {
            "s watch · Enter focus · ▶ active"
        } else {
            message
        };
        frame.render_widget(
            Paragraph::new(footer).style(theme.dim_style()),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

// The abort guard cancels the subscription task on errors and normal exits.
struct Abort(tokio::task::JoinHandle<()>);
impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Shares watch's topology, state merge and coalesced subscription refresh.
/// This surface has no request dispatch, composer or agent lifecycle actions.
pub(crate) async fn run(
    client: &Client,
    config: &muxa::config::Config,
    endpoint: BackendEndpoint,
    own: String,
    launch: UiLaunch,
) -> Result<()> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    let mut app = App::with_config(WatchConfig {
        view: WatchView::Pane,
        layout: WatchLayout::Tree,
        tree_expansion: WatchTreeExpansion::Always,
        spinner: false,
        hide_paneless: true,
        theme: config.watch.theme.or(Some(config.ui.theme)),
        ..config.watch.clone()
    });
    let backend: muxa::SharedBackend = match endpoint.host {
        HostKind::Rmux => Arc::new(muxa::RmuxBackend::with_endpoint(&endpoint.socket)),
        HostKind::Tmux => Arc::new(muxa::TmuxBackend::new()),
        _ => anyhow::bail!("sidebar requires tmux or rmux"),
    };
    let bg_client = client.clone();
    let activity_client = client.clone();
    let sub_client = client.clone();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (out_tx, mut out_rx) = mpsc::channel(2);
    let background = tokio::spawn(refresh_task(
        move || {
            let client = bg_client.clone();
            let backend = backend.clone();
            async move { compute_refresh(&client, &[backend], None).await }
        },
        move || {
            let client = activity_client.clone();
            async move { client.snapshot().await.ok() }
        },
        wake_rx,
        out_tx,
        async move { sub_client.subscribe_agent_changes().await.ok() },
    ));
    let _ = wake_tx.try_send(());
    let context = SidebarContext {
        endpoint: endpoint.clone(),
        own: own.clone(),
        launch,
    };
    let mut message = String::new();
    let mut list_state = ListState::default();
    let _background = Abort(background);
    let mut dirty = true;
    let mut last_draw = Instant::now();
    let mut last_sync: Option<Instant> = None;
    let mut modified = None;
    let mut active = Active::default();
    loop {
        while let Ok(outcome) = out_rx.try_recv() {
            if let Some(outcome) = filter(outcome, &endpoint, &own) {
                apply_outcome(&mut app, outcome);
                dirty = true;
            }
        }
        if last_sync.is_none_or(|last| last.elapsed() >= Duration::from_secs(1)) {
            if let Some(current) = Active::read(&context) {
                active = current;
            }
            sync_sort(&mut app, &context.launch, &mut modified);
            last_sync = Some(Instant::now());
            dirty = true;
        }
        if dirty || last_draw.elapsed() >= Duration::from_secs(1) {
            guard
                .terminal_mut()
                .draw(|frame| draw(frame, &mut app, &message, &mut list_state, &active))?;
            last_draw = Instant::now();
            dirty = false;
        }
        let event = tokio::task::spawn_blocking(|| -> io::Result<Option<Event>> {
            if crossterm::event::poll(Duration::from_millis(250))? {
                Ok(Some(crossterm::event::read()?))
            } else {
                Ok(None)
            }
        })
        .await??;
        dirty |= event.is_some();
        if let Some(event) = event {
            input(
                event,
                &mut app,
                &context,
                &list_state,
                &mut message,
                &wake_tx,
                guard.terminal_mut().size()?.height,
            );
        }
    }
}

fn input(
    event: Event,
    app: &mut App,
    context: &SidebarContext,
    list_state: &ListState,
    message: &mut String,
    wake_tx: &mpsc::Sender<()>,
    pane_height: u16,
) {
    match event {
        Event::Mouse(mouse) => match mouse.kind {
            crossterm::event::MouseEventKind::ScrollUp => app.move_up(),
            crossterm::event::MouseEventKind::ScrollDown => app.move_down(),
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
                if mouse.row == 0 =>
            {
                *message = open_watch(context, app)
                    .err()
                    .map_or_else(String::new, |error| error.to_string());
            }
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
                if mouse.row > 0 && mouse.row < pane_height.saturating_sub(2) =>
            {
                let index = list_state.offset() + usize::from(mouse.row - 1);
                if let Some(target) = app.tree_targets().get(index) {
                    app.table_state.select(Some(index));
                    if let TopologyNodeKey::Pane(pane) = &target.key {
                        *message = crate::sidebar::select(
                            &context.endpoint,
                            pane,
                            &context.own,
                            &context.launch,
                        )
                        .err()
                        .map_or_else(String::new, |error| error.to_string());
                    } else {
                        toggle_branch(app, &target.key);
                    }
                }
            }
            _ => {}
        },
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
            KeyCode::Left | KeyCode::Char('h') => app.collapse_or_select_tree_parent(),
            KeyCode::Right | KeyCode::Char('l') => app.expand_or_enter_tree_child(),
            KeyCode::Char('s') => {
                *message = open_watch(context, app)
                    .err()
                    .map_or_else(String::new, |error| error.to_string());
            }
            KeyCode::Char('r') => {
                let _ = wake_tx.try_send(());
            }
            KeyCode::Enter => {
                if let Some(TopologyNodeKey::Pane(pane)) = app.selected_node_key() {
                    *message = crate::sidebar::select(
                        &context.endpoint,
                        &pane,
                        &context.own,
                        &context.launch,
                    )
                    .err()
                    .map_or_else(String::new, |error| error.to_string());
                } else if let Some(key) = app.selected_node_key() {
                    toggle_branch(app, &key);
                }
            }
            KeyCode::Esc => {
                message.clear();
            }
            KeyCode::Char('z') => {
                *message = "Use prefix + z from the agent pane".into();
            }
            KeyCode::Char('q') => {
                *message = "Hide with muxa sidebar hide".into();
            }
            _ => {}
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_window_and_pane_are_independent_of_the_tree_cursor() {
        let session = muxa::SessionKey {
            endpoint: BackendEndpoint {
                host: HostKind::Rmux,
                socket: "/tmp/server".into(),
            },
            session_id: "$1".into(),
        };
        let window = muxa::WindowKey {
            session: session.clone(),
            window_id: "@2".into(),
        };
        let pane = muxa::PaneKey {
            window: window.clone(),
            pane_id: "rmux:%35".into(),
        };
        let active = Active {
            session: "$1".into(),
            window: "@2".into(),
            pane: "%35".into(),
        };
        assert_eq!(active.marker(&TopologyNodeKey::Session(session)), "◆ ");
        assert_eq!(active.marker(&TopologyNodeKey::Window(window)), "◆ ");
        assert_eq!(active.marker(&TopologyNodeKey::Pane(pane.clone())), "▶ ");
        let mut other = pane;
        other.window.window_id = "@3".into();
        assert_eq!(active.marker(&TopologyNodeKey::Pane(other)), "  ");
    }
    #[test]
    fn watch_saved_sort_updates_the_running_sidebar_and_survives_bad_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[watch]\nsort=['name']\n").unwrap();
        let launch = UiLaunch {
            daemon: directory.path().join("socket"),
            config: Some(path.clone()),
        };
        let mut app = App::with_config(WatchConfig::default());
        let mut stamp = None;
        sync_sort(&mut app, &launch, &mut stamp);
        assert_eq!(app.watch_cfg.sort, vec![muxa::config::WatchSortKey::Name]);
        crate::watch::persist_watch_sort(&path, &[muxa::config::WatchSortKey::PaneId]).unwrap();
        sync_sort(&mut app, &launch, &mut stamp);
        assert_eq!(app.watch_cfg.sort, vec![muxa::config::WatchSortKey::PaneId]);
        std::fs::write(&path, "[watch] invalid").unwrap();
        sync_sort(&mut app, &launch, &mut stamp);
        assert_eq!(app.watch_cfg.sort, vec![muxa::config::WatchSortKey::PaneId]);
    }
    #[test]
    fn compact_tree_does_not_list_its_own_infrastructure_pane() {
        let endpoint = BackendEndpoint {
            host: HostKind::Rmux,
            socket: "/tmp/server".into(),
        };
        let agent: Agent = serde_json::from_value(serde_json::json!({
            "kind":"codex", "agent_session_id":"worker", "pane":"rmux:%35", "tmux_socket":endpoint.socket,
            "state":"idle", "started_at":"2026-10-22T00:00:00Z", "last_activity_at":"2026-10-22T00:00:00Z"
        })).unwrap();
        let mut other_server = agent.clone();
        other_server.tmux_socket = Some("/tmp/another-server".into());
        assert!(filter(
            RefreshOutcome::SingleAgent(other_server),
            &endpoint,
            "rmux:%99"
        )
        .is_none());
        assert!(filter(
            RefreshOutcome::SingleAgent(agent.clone()),
            &endpoint,
            "rmux:%35"
        )
        .is_none());
        assert!(matches!(
            filter(
                RefreshOutcome::SingleAgent(agent.clone()),
                &endpoint,
                "rmux:%99"
            ),
            Some(RefreshOutcome::SingleAgent(_))
        ));
        let own = agent.clone();
        let Some(RefreshOutcome::Agents(agents)) = filter(
            RefreshOutcome::Agents(vec![agent, own]),
            &endpoint,
            "rmux:%35",
        ) else {
            unreachable!()
        };
        assert!(agents.is_empty());
    }
}
