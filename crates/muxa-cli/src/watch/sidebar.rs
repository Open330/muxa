//! Compact, read-only watch surface embedded in a native side pane.
use super::{
    apply_outcome, compute_refresh, refresh_task, setup_terminal, state_marker, tree_node_label,
    watch_theme, App, RefreshOutcome, Spinner, TerminalGuard, TopologyNodeKey, TopologyNodeRef,
    WatchConfig, WatchLayout, WatchTreeExpansion, WatchView,
};
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
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

struct SidebarContext {
    endpoint: BackendEndpoint,
    own: String,
    daemon: PathBuf,
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
fn draw(frame: &mut Frame, app: &mut App, message: &str, state: &mut ListState) {
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
            let mut spans = Vec::new();
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
                .title(" Muxa agents ")
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
            "↑↓ tree · Enter focus · z zoom"
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
        daemon: client.socket().to_path_buf(),
    };
    let mut message = String::new();
    let mut list_state = ListState::default();
    let _background = Abort(background);
    let mut dirty = true;
    let mut last_draw = Instant::now();
    loop {
        while let Ok(outcome) = out_rx.try_recv() {
            if let Some(outcome) = filter(outcome, &endpoint, &own) {
                apply_outcome(&mut app, outcome);
                dirty = true;
            }
        }
        if dirty || last_draw.elapsed() >= Duration::from_secs(1) {
            guard
                .terminal_mut()
                .draw(|frame| draw(frame, &mut app, &message, &mut list_state))?;
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
                            &context.daemon,
                        )
                        .err()
                        .map_or_else(String::new, |error| error.to_string());
                    } else {
                        app.expand_or_enter_tree_child();
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
            KeyCode::Char('r') => {
                let _ = wake_tx.try_send(());
            }
            KeyCode::Enter => {
                if let Some(TopologyNodeKey::Pane(pane)) = app.selected_node_key() {
                    *message = crate::sidebar::select(
                        &context.endpoint,
                        &pane,
                        &context.own,
                        &context.daemon,
                    )
                    .err()
                    .map_or_else(String::new, |error| error.to_string());
                } else {
                    app.expand_or_enter_tree_child();
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
