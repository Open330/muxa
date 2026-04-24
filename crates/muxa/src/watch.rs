//! `muxa watch` — fullscreen ratatui dashboard.
//!
//! Polls the daemon via `Client::snapshot()` every 500 ms and renders a
//! live-updating table of tracked agents. Input is handled via crossterm
//! events (`q`/`Esc`/`Ctrl-C` to quit, `r` to force-refresh, `↑/↓` or
//! `j/k` for selection, `Enter` to attach into the selected pane).
//!
//! Terminal lifecycle is managed by a RAII `TerminalGuard` so raw mode and
//! the alternate screen are always restored — even on panic.

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa_core::state::Agent;
use muxa_core::AgentState;
use muxa_runtime::ipc::Client;
use muxa_runtime::tmux;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};
use time::OffsetDateTime;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const INPUT_POLL: Duration = Duration::from_millis(50);

/// State held by the TUI.
///
/// Kept separate from rendering so the smoke test can construct it
/// directly without touching a real terminal.
pub(crate) struct App {
    pub agents: Vec<Agent>,
    pub table_state: TableState,
    pub last_error: Option<String>,
    pub last_refresh: OffsetDateTime,
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            agents: Vec::new(),
            table_state: TableState::default(),
            last_error: None,
            last_refresh: OffsetDateTime::now_utc(),
        }
    }

    pub(crate) fn set_agents(&mut self, mut agents: Vec<Agent>) {
        // Stable order so the cursor doesn't jump around between polls.
        agents.sort_by(|a, b| {
            a.pane
                .as_deref()
                .unwrap_or("")
                .cmp(b.pane.as_deref().unwrap_or(""))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        self.agents = agents;
        self.last_refresh = OffsetDateTime::now_utc();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.agents.is_empty() {
            self.table_state.select(None);
            return;
        }
        match self.table_state.selected() {
            Some(i) if i >= self.agents.len() => {
                self.table_state.select(Some(self.agents.len() - 1));
            }
            None => self.table_state.select(Some(0)),
            Some(_) => {}
        }
    }

    pub(crate) fn move_down(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i + 1 < self.agents.len() => i + 1,
            Some(_) => self.agents.len() - 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub(crate) fn move_up(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i > 0 => i - 1,
            _ => 0,
        };
        self.table_state.select(Some(i));
    }

    /// `pane_id` of the currently selected row, if any.
    pub(crate) fn selected_pane(&self) -> Option<String> {
        let i = self.table_state.selected()?;
        self.agents.get(i)?.pane.clone()
    }
}

/// Render a `pane_id` as `session:window.pane` when we can resolve it via
/// tmux, falling back to the raw id (e.g. `%1618`) so this still degrades
/// gracefully outside tmux.
fn pane_display(pane_id: Option<&str>) -> String {
    let Some(id) = pane_id else {
        return "-".into();
    };
    match tmux::resolve_pane(id) {
        Some(p) => format!("{}:{}.{}", p.session, p.window_index, p.pane_index),
        None => id.to_string(),
    }
}

/// Restore the terminal to a sane state on drop.
///
/// We take ownership of the `Terminal` here so that panics during the
/// render loop still run this destructor via stack unwinding.
struct TerminalGuard<B: Backend + io::Write> {
    terminal: Option<Terminal<B>>,
}

impl<B: Backend + io::Write> TerminalGuard<B> {
    fn new(terminal: Terminal<B>) -> Self {
        Self {
            terminal: Some(terminal),
        }
    }

    fn terminal_mut(&mut self) -> &mut Terminal<B> {
        self.terminal.as_mut().expect("terminal present")
    }
}

impl<B: Backend + io::Write> Drop for TerminalGuard<B> {
    fn drop(&mut self) {
        if let Some(mut t) = self.terminal.take() {
            let _ = disable_raw_mode();
            let _ = execute!(t.backend_mut(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = t.show_cursor();
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// Entry point for `muxa watch`.
///
/// Returns `Some(pane_id)` if the user pressed Enter on a selected agent,
/// meaning they want to attach to that pane. The caller (`main.rs`) runs
/// the actual tmux switch-client invocation *after* this returns so the
/// terminal is already restored by the time we hand off control.
pub async fn run(client: &Client) -> Result<Option<String>> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);

    let mut app = App::new();

    // Prime the initial snapshot so we don't paint an empty frame first.
    match client.snapshot().await {
        Ok(agents) => app.set_agents(agents),
        Err(e) => app.last_error = Some(e.to_string()),
    }

    let mut last_poll = tokio::time::Instant::now();
    let mut jump_target: Option<String> = None;

    loop {
        guard
            .terminal_mut()
            .draw(|f| render(f, &mut app))
            .map_err(anyhow::Error::from)?;

        // Drain a batch of input events. `crossterm::event::poll` is
        // blocking, so we run it on a blocking thread to not starve the
        // tokio runtime. A short timeout keeps UI latency low.
        let got_event = tokio::task::spawn_blocking(|| -> io::Result<Option<Event>> {
            if crossterm::event::poll(INPUT_POLL)? {
                Ok(Some(crossterm::event::read()?))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(anyhow::Error::from)??;

        if let Some(ev) = got_event {
            match handle_event(ev, &mut app) {
                Action::Quit => break,
                Action::Attach => {
                    if let Some(pane) = app.selected_pane() {
                        jump_target = Some(pane);
                        break;
                    }
                }
                Action::Refresh => {
                    refresh(client, &mut app).await;
                    last_poll = tokio::time::Instant::now();
                }
                Action::None => {}
            }
        }

        if last_poll.elapsed() >= POLL_INTERVAL {
            refresh(client, &mut app).await;
            last_poll = tokio::time::Instant::now();
        }
    }

    Ok(jump_target)
}

async fn refresh(client: &Client, app: &mut App) {
    match client.snapshot().await {
        Ok(agents) => {
            app.last_error = None;
            app.set_agents(agents);
        }
        Err(e) => {
            app.last_error = Some(e.to_string());
        }
    }
}

enum Action {
    None,
    Quit,
    Refresh,
    /// Attach to the currently-selected pane.
    Attach,
}

fn handle_event(ev: Event, app: &mut App) -> Action {
    let Event::Key(KeyEvent {
        code,
        modifiers,
        kind,
        ..
    }) = ev
    else {
        return Action::None;
    };

    // On terminals that emit `Release` events too (crossterm 0.29 on
    // Windows / kitty protocol) we only react to initial presses so
    // actions don't fire twice.
    if kind != KeyEventKind::Press && kind != KeyEventKind::Repeat {
        return Action::None;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        return Action::Quit;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Enter => Action::Attach,
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_down();
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_up();
            Action::None
        }
        _ => Action::None,
    }
}

// ---- rendering ------------------------------------------------------------

pub(crate) fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, chunks[0], app);
    render_table(f, chunks[1], app);
    render_footer(f, chunks[2], app);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let count = app.agents.len();
    let now = app.last_refresh;
    let clock = format!(
        "{:02}:{:02}:{:02} UTC",
        now.hour(),
        now.minute(),
        now.second()
    );

    let title = Line::from(vec![
        Span::styled(
            " muxa watch ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{count} agent{}", if count == 1 { "" } else { "s" }),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(clock, Style::default().fg(Color::DarkGray)),
    ]);

    let err_line = app
        .last_error
        .as_ref()
        .map(|e| {
            Line::from(Span::styled(
                format!("daemon error: {e}"),
                Style::default().fg(Color::Red),
            ))
        })
        .unwrap_or_default();

    let header = Paragraph::new(vec![title, err_line]).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(header, area);
}

fn render_table(f: &mut Frame, area: Rect, app: &mut App) {
    let header_cells = [
        "PANE",
        "KIND",
        "STATE",
        "MODEL",
        "CTX%",
        "COST$",
        "LAST PROMPT",
        "ACTIVITY",
    ]
    .iter()
    .map(|h| {
        Cell::from(*h).style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header = Row::new(header_cells).height(1);

    let now = OffsetDateTime::now_utc();
    let rows: Vec<Row> = app.agents.iter().map(|a| agent_row(a, now)).collect();

    let widths = [
        // PANE — "session:window.pane" can run long; 22 covers most.
        Constraint::Length(22),
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Length(16),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Min(20),
        Constraint::Length(10),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" Agents "),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn agent_row(a: &Agent, now: OffsetDateTime) -> Row<'_> {
    let state_style = match a.state {
        AgentState::Working => Style::default().fg(Color::Green),
        AgentState::WaitingInput => Style::default().fg(Color::Yellow),
        AgentState::Error => Style::default().fg(Color::Red),
        AgentState::Idle | AgentState::Stopped => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
        AgentState::Starting => Style::default().fg(Color::Cyan),
    };

    let pane = pane_display(a.pane.as_deref());
    let kind = kind_label(a.kind);
    let state = state_label(a.state);
    let model = a.model.as_deref().unwrap_or("-").to_string();
    let ctx = a
        .context_used_pct
        .map_or_else(|| "-".into(), |p| format!("{p:>3.0}%"));
    let cost = a
        .cost_usd
        .map_or_else(|| "-".into(), |c| format!("${c:.2}"));
    let prompt = a
        .last_prompt
        .as_deref()
        .unwrap_or("-")
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(80)
        .collect::<String>();
    let activity = relative_time(a.last_activity_at, now);

    Row::new(vec![
        Cell::from(pane),
        Cell::from(kind),
        Cell::from(state).style(state_style),
        Cell::from(model),
        Cell::from(ctx),
        Cell::from(cost),
        Cell::from(prompt),
        Cell::from(activity),
    ])
}

fn kind_label(kind: muxa_core::AgentKind) -> String {
    match kind {
        muxa_core::AgentKind::ClaudeCode => "claude_code",
        muxa_core::AgentKind::Codex => "codex",
        muxa_core::AgentKind::GeminiCli => "gemini_cli",
        muxa_core::AgentKind::Opencode => "opencode",
        muxa_core::AgentKind::Unknown => "unknown",
    }
    .to_string()
}

fn state_label(state: AgentState) -> String {
    match state {
        AgentState::Starting => "starting",
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::WaitingInput => "waiting_input",
        AgentState::Error => "error",
        AgentState::Stopped => "stopped",
    }
    .to_string()
}

fn relative_time(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let delta = now - at;
    let secs = delta.whole_seconds();
    if secs < 0 {
        // Clock skew — show absolute.
        return format!("{:02}:{:02}", at.hour(), at.minute());
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = delta.whole_minutes();
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = delta.whole_hours();
    if hours < 48 {
        return format!("{hours}h ago");
    }
    let days = delta.whole_days();
    format!("{days}d ago")
}

fn render_footer(f: &mut Frame, area: Rect, _app: &App) {
    let hint = Line::from(vec![
        Span::styled(" ↑/↓ ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" move  "),
        Span::styled(" ⏎ ", Style::default().fg(Color::Black).bg(Color::Green)),
        Span::raw(" attach  "),
        Span::styled(" r ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" refresh  "),
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit"),
    ]);
    f.render_widget(Paragraph::new(hint), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa_core::event::{AgentKind, AgentState};
    use ratatui::backend::TestBackend;
    use time::OffsetDateTime;

    #[allow(clippy::too_many_arguments)]
    fn fake_agent(
        session: &str,
        pane: Option<&str>,
        kind: AgentKind,
        state: AgentState,
        prompt: Option<&str>,
        model: Option<&str>,
        ctx: Option<f32>,
        cost: Option<f64>,
    ) -> Agent {
        let now = OffsetDateTime::now_utc();
        Agent {
            kind,
            session_id: session.into(),
            pane: pane.map(Into::into),
            cwd: None,
            state,
            last_prompt: prompt.map(Into::into),
            last_notification: None,
            model: model.map(Into::into),
            context_used_pct: ctx,
            cost_usd: cost,
            started_at: now,
            last_activity_at: now,
        }
    }

    #[test]
    fn render_does_not_panic_on_test_backend() {
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = App::new();
        app.set_agents(vec![
            fake_agent(
                "sess-a",
                Some("%10"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some(
                    "refactor the ipc module to use generics across multiple lines\nand keep going",
                ),
                Some("Opus"),
                Some(34.0),
                Some(0.12),
            ),
            fake_agent(
                "sess-b",
                Some("%11"),
                AgentKind::Codex,
                AgentState::WaitingInput,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "sess-c",
                Some("%12"),
                AgentKind::GeminiCli,
                AgentState::Idle,
                Some("summarize this PR"),
                Some("Gemini"),
                Some(12.5),
                Some(0.01),
            ),
            fake_agent(
                "sess-d",
                None,
                AgentKind::Unknown,
                AgentState::Error,
                None,
                None,
                None,
                None,
            ),
        ]);

        terminal.draw(|f| render(f, &mut app)).unwrap();
    }

    #[test]
    fn render_handles_empty_state() {
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal.draw(|f| render(f, &mut app)).unwrap();
    }

    #[test]
    fn selected_pane_returns_pane_id_for_selected_row() {
        let mut app = App::new();
        app.set_agents(vec![
            fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "s2",
                Some("%22"),
                AgentKind::Codex,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
        ]);
        // Selection starts at 0 after set_agents.
        assert_eq!(app.selected_pane().as_deref(), Some("%1"));
        app.move_down();
        assert_eq!(app.selected_pane().as_deref(), Some("%22"));
    }

    #[test]
    fn pane_display_falls_back_to_raw_id_outside_tmux() {
        // Outside a tmux server, resolve_pane returns None, and we expect
        // the raw pane id back.
        assert_eq!(pane_display(Some("%9999")), "%9999");
        assert_eq!(pane_display(None), "-");
    }

    #[test]
    fn selection_movement_is_bounded() {
        let mut app = App::new();
        app.set_agents(vec![
            fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "s2",
                Some("%2"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
        ]);
        assert_eq!(app.table_state.selected(), Some(0));
        app.move_down();
        assert_eq!(app.table_state.selected(), Some(1));
        app.move_down();
        assert_eq!(app.table_state.selected(), Some(1));
        app.move_up();
        assert_eq!(app.table_state.selected(), Some(0));
        app.move_up();
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn relative_time_buckets() {
        let now = OffsetDateTime::now_utc();
        assert!(relative_time(now, now).ends_with("s ago"));
        assert!(relative_time(now - time::Duration::minutes(5), now).ends_with("m ago"));
        assert!(relative_time(now - time::Duration::hours(3), now).ends_with("h ago"));
        assert!(relative_time(now - time::Duration::days(3), now).ends_with("d ago"));
    }
}
