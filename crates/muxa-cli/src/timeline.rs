use anyhow::{Context, Result};
use clap::ValueEnum;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa::config::WatchTheme;
use muxa::ipc::Client;
use muxa::timeline::{
    self as core_timeline, TimelineBuildInput, TimelineDocument, TimelineFilters, TimelineInterval,
    TimelineIntervalSource, TimelineLane, TimelineLaneKind, TimelineTotals,
};
use muxa::{AgentKind, AgentState, Config};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};
use time::{OffsetDateTime, UtcOffset};

use crate::theme::ThemeArg;
use crate::use_colors;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const INPUT_POLL: Duration = Duration::from_millis(120);
const INITIAL_VIEWPORT_SECS: i64 = 6 * 60 * 60;
const MIN_WINDOW_SECS: i64 = 60;

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Time window to include: today, yesterday, week, 24h, 7d, RFC3339 timestamp, or all.
    #[arg(long, default_value = "today")]
    since: String,

    /// Focus a tmux session by name, session id, or pane id.
    #[arg(long)]
    session: Option<String>,

    /// Filter agent lanes by kind.
    #[arg(long, value_enum)]
    agent: Option<AgentKindArg>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Tui)]
    format: OutputFormat,

    /// Group lanes in the TUI overview.
    #[arg(long, value_enum, default_value_t = TimelineGroupBy::Session)]
    group_by: TimelineGroupBy,

    /// One-shot visual theme override for TUI output.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,
}

impl Args {
    #[cfg(test)]
    pub(crate) fn theme(&self) -> Option<ThemeArg> {
        self.theme
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OutputFormat {
    Tui,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum TimelineGroupBy {
    Session,
    Kind,
    #[value(alias = "none")]
    Flat,
}

impl TimelineGroupBy {
    fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Kind => "kind",
            Self::Flat => "flat",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Session => Self::Kind,
            Self::Kind => Self::Flat,
            Self::Flat => Self::Session,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum AgentKindArg {
    #[value(alias = "claude_code")]
    ClaudeCode,
    Codex,
    #[value(alias = "gemini_cli")]
    GeminiCli,
    Opencode,
    Unknown,
}

impl From<AgentKindArg> for AgentKind {
    fn from(value: AgentKindArg) -> Self {
        match value {
            AgentKindArg::ClaudeCode => Self::ClaudeCode,
            AgentKindArg::Codex => Self::Codex,
            AgentKindArg::GeminiCli => Self::GeminiCli,
            AgentKindArg::Opencode => Self::Opencode,
            AgentKindArg::Unknown => Self::Unknown,
        }
    }
}

pub async fn run(client: &Client, cfg: &Config, args: Args) -> Result<()> {
    let doc = load_document(client, cfg, &args).await?;
    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&doc)?);
            Ok(())
        }
        OutputFormat::Tui => run_tui(client, cfg, args, doc).await,
    }
}

async fn load_document(client: &Client, cfg: &Config, args: &Args) -> Result<TimelineDocument> {
    let now = OffsetDateTime::now_utc();
    let range = core_timeline::parse_since(&args.since, now, "all retained activity")
        .map_err(anyhow::Error::msg)?;
    let mut notes = Vec::new();

    let activity_entries = if cfg.activity.enabled {
        if let Some(path) = cfg
            .activity
            .path
            .clone()
            .or_else(muxa::paths::default_activity_file)
        {
            muxa::activity::load(&path)
                .await
                .with_context(|| format!("loading activity ledger {}", path.display()))?
        } else {
            notes.push("activity ledger path could not be resolved".to_string());
            Vec::new()
        }
    } else {
        notes.push("activity ledger is disabled".to_string());
        Vec::new()
    };

    let agents = client
        .snapshot()
        .await
        .context("querying daemon agent snapshot")?;
    let session_activities = load_session_activities(cfg).await;
    let pane_sessions = muxa::default_backend()
        .list_panes()
        .into_iter()
        .map(|pane| (pane.pane_id, pane.session))
        .collect::<HashMap<_, _>>();

    Ok(core_timeline::build_document(TimelineBuildInput {
        now,
        range,
        activity_entries: &activity_entries,
        agents: &agents,
        session_activities: &session_activities,
        pane_sessions: &pane_sessions,
        filters: TimelineFilters {
            session: args.session.clone(),
            agent_kind: args.agent.map(AgentKind::from),
        },
        notes,
    }))
}

async fn load_session_activities(cfg: &Config) -> Vec<muxa::SessionActivity> {
    if !cfg.session_activity.enabled {
        return Vec::new();
    }
    let Some(path) = cfg
        .session_activity
        .path
        .clone()
        .or_else(muxa::paths::default_session_activity_file)
    else {
        return Vec::new();
    };
    muxa::session_activity::load(&path).await
}

async fn run_tui(
    client: &Client,
    cfg: &Config,
    args: Args,
    initial_doc: TimelineDocument,
) -> Result<()> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    let theme = args.theme.map_or(cfg.ui.theme, WatchTheme::from);
    let mut app = TimelineApp::new(initial_doc, theme, use_colors(), args.group_by);
    let mut last_refresh = Instant::now();

    loop {
        guard
            .terminal_mut()
            .draw(|f| render(f, &mut app))
            .map_err(anyhow::Error::from)?;

        if event::poll(INPUT_POLL)? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if handle_key(&mut app, key.code, key.modifiers) {
                break;
            }
            if matches!(key.code, KeyCode::Char('r')) {
                match load_document(client, cfg, &args).await {
                    Ok(doc) => app.replace_doc(doc),
                    Err(e) => app.last_error = Some(e.to_string()),
                }
                last_refresh = Instant::now();
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            match load_document(client, cfg, &args).await {
                Ok(doc) => app.replace_doc(doc),
                Err(e) => app.last_error = Some(e.to_string()),
            }
            last_refresh = Instant::now();
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelineView {
    Overview,
    Focus,
}

#[derive(Debug)]
struct TimelineApp {
    doc: TimelineDocument,
    view: TimelineView,
    selected_lane: usize,
    selected_interval: usize,
    window_started_at: OffsetDateTime,
    window_ended_at: OffsetDateTime,
    theme: TimelineTheme,
    colors: bool,
    help_open: bool,
    last_error: Option<String>,
    status: Option<String>,
    group_by: TimelineGroupBy,
}

impl TimelineApp {
    fn new(
        doc: TimelineDocument,
        theme: WatchTheme,
        colors: bool,
        group_by: TimelineGroupBy,
    ) -> Self {
        let (window_started_at, window_ended_at) = latest_window(&doc);
        let selected_interval = doc
            .lanes
            .first()
            .map_or(0, |lane| lane.intervals.len().saturating_sub(1));
        Self {
            doc,
            view: TimelineView::Overview,
            selected_lane: 0,
            selected_interval,
            window_started_at,
            window_ended_at,
            theme: TimelineTheme::from_watch(theme),
            colors,
            help_open: false,
            last_error: None,
            status: None,
            group_by,
        }
    }

    fn replace_doc(&mut self, doc: TimelineDocument) {
        let follow_live = (self.doc.window_ended_at - self.window_ended_at)
            .whole_seconds()
            .abs()
            <= 3;
        let current_span = window_span_secs(self.window_started_at, self.window_ended_at);
        self.doc = doc;
        self.selected_lane = self
            .selected_lane
            .min(self.doc.lanes.len().saturating_sub(1));
        self.selected_interval = self.selected_interval.min(
            self.selected_lane_ref()
                .map_or(0, |lane| lane.intervals.len().saturating_sub(1)),
        );
        if follow_live {
            let (doc_start, doc_end) = document_bounds(&self.doc);
            let span = current_span.min(window_span_secs(doc_start, doc_end));
            self.window_ended_at = doc_end;
            self.window_started_at = self.window_ended_at - time::Duration::seconds(span);
        }
        self.clamp_window();
        self.last_error = None;
    }

    fn selected_lane_ref(&self) -> Option<&TimelineLane> {
        self.doc.lanes.get(self.selected_lane)
    }

    fn selected_interval_ref(&self) -> Option<&TimelineInterval> {
        self.selected_lane_ref()
            .and_then(|lane| lane.intervals.get(self.selected_interval))
            .or_else(|| {
                self.selected_lane_ref()
                    .and_then(|lane| lane.intervals.first())
            })
    }

    fn select_lane_delta(&mut self, delta: isize) {
        if self.doc.lanes.is_empty() {
            return;
        }
        self.selected_lane = add_delta(self.selected_lane, delta, self.doc.lanes.len());
        self.selected_interval = self
            .selected_lane_ref()
            .map_or(0, |lane| lane.intervals.len().saturating_sub(1));
    }

    fn select_interval_delta(&mut self, delta: isize) {
        let Some(lane) = self.selected_lane_ref() else {
            return;
        };
        if lane.intervals.is_empty() {
            self.selected_interval = 0;
            return;
        }
        self.selected_interval = add_delta(self.selected_interval, delta, lane.intervals.len());
    }

    fn reset_window(&mut self) {
        let (window_started_at, window_ended_at) = latest_window(&self.doc);
        self.window_started_at = window_started_at;
        self.window_ended_at = window_ended_at;
        self.status = Some("latest view".to_string());
        self.clamp_window();
    }

    fn fit_window(&mut self) {
        let (doc_start, doc_end) = document_bounds(&self.doc);
        self.window_started_at = doc_start;
        self.window_ended_at = doc_end;
        self.status = Some("full range".to_string());
        self.clamp_window();
    }

    fn pan(&mut self, direction: i32) {
        let before = (self.window_started_at, self.window_ended_at);
        let span = window_span_secs(self.window_started_at, self.window_ended_at);
        let step = (span / 5).max(1);
        let delta = time::Duration::seconds(step.saturating_mul(i64::from(direction)));
        self.window_started_at += delta;
        self.window_ended_at += delta;
        self.clamp_window();
        if before == (self.window_started_at, self.window_ended_at) {
            self.status = Some(if direction < 0 {
                "at start of timeline".to_string()
            } else {
                "at latest edge".to_string()
            });
        } else {
            self.status = None;
        }
    }

    fn zoom(&mut self, divisor: i64) {
        let span =
            window_span_secs(self.window_started_at, self.window_ended_at).max(MIN_WINDOW_SECS);
        let center = self.window_started_at + time::Duration::seconds(span / 2);
        let new_span = if divisor > 0 {
            (span / divisor).max(MIN_WINDOW_SECS)
        } else {
            span.saturating_mul(2)
        };
        self.window_started_at = center - time::Duration::seconds(new_span / 2);
        self.window_ended_at = self.window_started_at + time::Duration::seconds(new_span);
        self.clamp_window();
        self.status = None;
    }

    fn cycle_group_by(&mut self) {
        self.group_by = self.group_by.next();
        self.status = Some(format!("group by {}", self.group_by.label()));
    }

    fn clamp_window(&mut self) {
        let (doc_start, doc_end) = document_bounds(&self.doc);
        let doc_span = window_span_secs(doc_start, doc_end);
        let span = window_span_secs(self.window_started_at, self.window_ended_at);
        if span >= doc_span {
            self.window_started_at = doc_start;
            self.window_ended_at = doc_end;
            return;
        }
        if self.window_started_at < doc_start {
            let delta = doc_start - self.window_started_at;
            self.window_started_at += delta;
            self.window_ended_at += delta;
        }
        if self.window_ended_at > doc_end {
            let delta = self.window_ended_at - doc_end;
            self.window_started_at -= delta;
            self.window_ended_at -= delta;
        }
        if self.window_ended_at <= self.window_started_at {
            self.window_started_at = doc_start;
            self.window_ended_at = doc_end;
        }
    }
}

fn latest_window(doc: &TimelineDocument) -> (OffsetDateTime, OffsetDateTime) {
    let (doc_start, doc_end) = document_bounds(doc);
    let doc_span = window_span_secs(doc_start, doc_end);
    let span = doc_span.min(INITIAL_VIEWPORT_SECS);
    (
        (doc_end - time::Duration::seconds(span)).max(doc_start),
        doc_end,
    )
}

fn document_bounds(doc: &TimelineDocument) -> (OffsetDateTime, OffsetDateTime) {
    let doc_start = doc.window_started_at;
    let doc_end = doc
        .window_ended_at
        .max(doc_start + time::Duration::seconds(1));
    (doc_start, doc_end)
}

fn window_span_secs(start: OffsetDateTime, end: OffsetDateTime) -> i64 {
    (end - start).whole_seconds().max(1)
}

#[derive(Debug, Clone, Copy)]
struct TimelineTheme {
    title: &'static str,
    accent: Color,
    border: Color,
    dim: Color,
    selected_bg: Color,
    working: Color,
    waiting: Color,
    choice: Color,
    error: Color,
    idle: Color,
    starting: Color,
    human: Color,
    tmux: Color,
    border_type: BorderType,
}

impl TimelineTheme {
    fn from_watch(theme: WatchTheme) -> Self {
        match theme {
            WatchTheme::OhMyMuxa => Self {
                title: " muxa timeline ",
                accent: Color::Rgb(94, 234, 212),
                border: Color::Rgb(94, 234, 212),
                dim: Color::Gray,
                selected_bg: Color::Rgb(52, 45, 67),
                working: Color::Rgb(93, 230, 138),
                waiting: Color::Rgb(255, 176, 86),
                choice: Color::Rgb(219, 181, 255),
                error: Color::Rgb(255, 91, 107),
                idle: Color::DarkGray,
                starting: Color::Rgb(94, 234, 212),
                human: Color::Rgb(196, 181, 253),
                tmux: Color::Rgb(125, 211, 252),
                border_type: BorderType::Rounded,
            },
            WatchTheme::Focus => Self {
                title: " muxa timeline ",
                accent: Color::Rgb(125, 211, 252),
                border: Color::DarkGray,
                dim: Color::DarkGray,
                selected_bg: Color::Rgb(30, 58, 90),
                working: Color::Rgb(134, 239, 172),
                waiting: Color::Rgb(250, 204, 21),
                choice: Color::Rgb(216, 180, 254),
                error: Color::Rgb(248, 113, 113),
                idle: Color::DarkGray,
                starting: Color::Rgb(125, 211, 252),
                human: Color::Rgb(244, 114, 182),
                tmux: Color::Rgb(56, 189, 248),
                border_type: BorderType::Rounded,
            },
            WatchTheme::HighContrast => Self {
                title: " muxa timeline ",
                accent: Color::White,
                border: Color::White,
                dim: Color::Gray,
                selected_bg: Color::White,
                working: Color::Green,
                waiting: Color::Yellow,
                choice: Color::Magenta,
                error: Color::Red,
                idle: Color::DarkGray,
                starting: Color::Cyan,
                human: Color::LightMagenta,
                tmux: Color::LightCyan,
                border_type: BorderType::Plain,
            },
            WatchTheme::Mono | WatchTheme::Minimal => Self {
                title: " muxa timeline ",
                accent: Color::White,
                border: Color::DarkGray,
                dim: Color::DarkGray,
                selected_bg: Color::DarkGray,
                working: Color::White,
                waiting: Color::Gray,
                choice: Color::Gray,
                error: Color::White,
                idle: Color::DarkGray,
                starting: Color::Gray,
                human: Color::White,
                tmux: Color::Gray,
                border_type: BorderType::Plain,
            },
            WatchTheme::Classic | WatchTheme::Ops => Self {
                title: " muxa timeline ",
                accent: Color::Cyan,
                border: Color::DarkGray,
                dim: Color::DarkGray,
                selected_bg: Color::DarkGray,
                working: Color::Green,
                waiting: Color::Yellow,
                choice: Color::LightYellow,
                error: Color::Red,
                idle: Color::DarkGray,
                starting: Color::Cyan,
                human: Color::Magenta,
                tmux: Color::Cyan,
                border_type: BorderType::Plain,
            },
        }
    }

    #[allow(clippy::unused_self)] // call-site reads naturally as `theme.color(...)`
    fn color(self, color: Color, colors: bool) -> Style {
        if colors {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }

    fn border_style(self, colors: bool) -> Style {
        self.color(self.border, colors)
    }

    fn accent_style(self, colors: bool) -> Style {
        self.color(self.accent, colors).add_modifier(Modifier::BOLD)
    }
}

fn handle_key(app: &mut TimelineApp, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if app.help_open {
        match code {
            KeyCode::Char('?' | 'q') | KeyCode::Esc => app.help_open = false,
            _ => {}
        }
        return false;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('?') => app.help_open = true,
        KeyCode::Char('o') | KeyCode::Enter => {
            app.view = match app.view {
                TimelineView::Overview => TimelineView::Focus,
                TimelineView::Focus => TimelineView::Overview,
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.view == TimelineView::Focus {
                app.select_interval_delta(1);
            } else {
                app.select_lane_delta(1);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.view == TimelineView::Focus {
                app.select_interval_delta(-1);
            } else {
                app.select_lane_delta(-1);
            }
        }
        KeyCode::Left | KeyCode::Char('h') => app.pan(-1),
        KeyCode::Right | KeyCode::Char('l') => app.pan(1),
        KeyCode::Char('+' | '=') => app.zoom(2),
        KeyCode::Char('-') => app.zoom(0),
        KeyCode::Char('0') => app.reset_window(),
        KeyCode::Char('f') => app.fit_window(),
        KeyCode::Char('g') => app.cycle_group_by(),
        KeyCode::Tab => app.select_interval_delta(1),
        KeyCode::BackTab => app.select_interval_delta(-1),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return true,
        _ => {}
    }
    false
}

fn render(f: &mut Frame, app: &mut TimelineApp) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, chunks[0], app);
    match app.view {
        TimelineView::Overview => render_overview(f, chunks[1], app),
        TimelineView::Focus => render_focus(f, chunks[1], app),
    }
    render_detail(f, chunks[2], app);
    render_footer(f, chunks[3], app);

    if app.help_open {
        let popup = centered_rect(64, 82, chunks[1]);
        f.render_widget(Clear, popup);
        render_help(f, popup, app);
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &TimelineApp) {
    let title = format!(
        "{} · {} · {} lanes · group {}",
        app.theme.title.trim(),
        app.doc.range.label,
        app.doc.lanes.len(),
        app.group_by.label()
    );
    let body = vec![
        Line::from(vec![
            Span::styled(title, app.theme.accent_style(app.colors)),
            Span::raw("  "),
            Span::styled(
                format!(
                    "{} - {}",
                    format_short_time(app.window_started_at),
                    format_short_time(app.window_ended_at)
                ),
                app.theme.color(app.theme.dim, app.colors),
            ),
        ]),
        Line::from(legend_spans(app.theme, app.colors)),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(app.colors))
        .border_type(app.theme.border_type);
    f.render_widget(Paragraph::new(body).block(block), area);
}

fn render_overview(f: &mut Frame, area: Rect, app: &TimelineApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(app.colors))
        .border_type(app.theme.border_type)
        .title(Span::styled(
            " overview ",
            app.theme.accent_style(app.colors),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 32 || inner.height < 3 {
        f.render_widget(Paragraph::new("terminal too small"), inner);
        return;
    }
    if app.doc.lanes.is_empty() {
        render_empty(f, inner, app);
        return;
    }

    let label_width = inner.width.clamp(14, 26);
    let track_width = inner.width.saturating_sub(label_width).saturating_sub(1);
    let axis = Line::from(vec![
        Span::raw(" ".repeat(usize::from(label_width + 1))),
        Span::styled(
            axis_line(app.window_started_at, app.window_ended_at, track_width),
            app.theme.color(app.theme.dim, app.colors),
        ),
    ]);

    let rows = overview_rows(app);
    let selected_row = selected_overview_row(&rows, app.selected_lane).unwrap_or(0);
    let lane_slots = inner.height.saturating_sub(1);
    let start = visible_start(selected_row, usize::from(lane_slots), rows.len());
    let mut lines = Vec::with_capacity(usize::from(inner.height));
    lines.push(axis);
    for row in rows.iter().skip(start).take(usize::from(lane_slots)) {
        match row {
            OverviewRow::Group {
                label,
                lane_count,
                totals,
            } => lines.push(render_group_line(
                label,
                *lane_count,
                totals,
                label_width,
                track_width,
                app,
            )),
            OverviewRow::Lane { lane_index } => {
                let Some(lane) = app.doc.lanes.get(*lane_index) else {
                    continue;
                };
                lines.push(render_lane_line(
                    lane,
                    *lane_index == app.selected_lane,
                    label_width,
                    track_width,
                    app,
                ));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_focus(f: &mut Frame, area: Rect, app: &TimelineApp) {
    let title = app.selected_lane_ref().map_or_else(
        || " focus ".to_string(),
        |lane| format!(" focus · {} ", lane.label),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(app.colors))
        .border_type(app.theme.border_type)
        .title(Span::styled(title, app.theme.accent_style(app.colors)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(lane) = app.selected_lane_ref() else {
        render_empty(f, inner, app);
        return;
    };
    if lane.intervals.is_empty() {
        render_empty(f, inner, app);
        return;
    }

    let visible = usize::from(inner.height);
    let start = visible_start(app.selected_interval, visible, lane.intervals.len());
    let lines = lane
        .intervals
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(idx, interval)| focus_line(interval, idx == app.selected_interval, app))
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_detail(f: &mut Frame, area: Rect, app: &TimelineApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(app.colors))
        .border_type(app.theme.border_type)
        .title(Span::styled(" detail ", app.theme.accent_style(app.colors)));
    let mut lines = Vec::new();
    if let Some(err) = app.last_error.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("refresh error: {err}"),
            app.theme
                .color(app.theme.error, app.colors)
                .add_modifier(Modifier::BOLD),
        )));
    }
    if let (Some(lane), Some(interval)) = (app.selected_lane_ref(), app.selected_interval_ref()) {
        lines.push(Line::from(vec![
            Span::styled(lane.label.clone(), app.theme.accent_style(app.colors)),
            Span::raw("  "),
            Span::styled(interval_label(interval), interval_style(interval, app)),
            Span::raw("  "),
            Span::raw(format_duration(interval.duration_secs)),
            Span::raw(if interval.open { "  open" } else { "" }),
        ]));
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{} - {}",
                    format_full_time(interval.started_at),
                    format_full_time(interval.ended_at)
                ),
                app.theme.color(app.theme.dim, app.colors),
            ),
            Span::raw("  "),
            Span::raw(interval.detail.clone()),
        ]));
        let mut meta = Vec::new();
        if let Some(session) = interval.session_name.as_deref() {
            meta.push(format!("session {session}"));
        }
        if let Some(pane) = interval.pane.as_deref() {
            meta.push(format!("pane {pane}"));
        }
        if let Some(cwd) = interval.cwd.as_deref() {
            meta.push(format!("cwd {cwd}"));
        }
        if !meta.is_empty() {
            lines.push(Line::from(Span::styled(
                meta.join("  ·  "),
                app.theme.color(app.theme.dim, app.colors),
            )));
        }
    } else {
        lines.push(Line::from("no interval selected"));
    }
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn render_footer(f: &mut Frame, area: Rect, app: &TimelineApp) {
    let mode = match app.view {
        TimelineView::Overview => "overview",
        TimelineView::Focus => "focus",
    };
    let mut body = format!(
        " {mode}  j/k select  h/l pan  +/- zoom  0 latest  f fit  g group  tab interval  enter/o toggle  r refresh  ? help  q quit"
    );
    if let Some(status) = app.status.as_deref() {
        body.push_str("  ·  ");
        body.push_str(status);
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            body,
            app.theme.color(app.theme.dim, app.colors),
        )),
        area,
    );
}

fn render_help(f: &mut Frame, area: Rect, app: &TimelineApp) {
    let lines = [
        "Keybindings",
        "",
        "  j/k or arrows     select lane in overview, interval in focus",
        "  tab / shift-tab   cycle intervals on the selected lane",
        "  h/l or arrows     pan the visible time window",
        "  + / -             zoom in / zoom out",
        "  0                 jump back to the latest view",
        "  f                 fit the full selected time range",
        "  g                 cycle grouping: session, kind, flat",
        "  enter / o         toggle overview and focus views",
        "  r                 reload activity and live agent state",
        "  q / Esc           quit",
        "",
        "The colored bars are clipped intervals. Agent transitions render",
        "the state they left, so working -> waiting draws a working span.",
    ];
    let body = lines
        .into_iter()
        .map(|line| {
            if line.is_empty() {
                Line::from("")
            } else if line.starts_with("  ") {
                Line::from(line)
            } else {
                Line::from(Span::styled(
                    line,
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            }
        })
        .collect::<Vec<_>>();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(app.colors))
        .border_type(app.theme.border_type)
        .title(Span::styled(" help ", app.theme.accent_style(app.colors)));
    f.render_widget(Paragraph::new(body).block(block), area);
}

fn render_empty(f: &mut Frame, area: Rect, app: &TimelineApp) {
    let mut lines = if app.doc.notes.is_empty() {
        vec![Line::from("no timeline intervals in this view")]
    } else {
        app.doc
            .notes
            .iter()
            .map(|note| Line::from(note.clone()))
            .collect::<Vec<_>>()
    };
    lines.push(Line::from(""));
    lines.push(Line::from("Try a wider --since range or remove filters."));
    f.render_widget(
        Paragraph::new(lines)
            .style(app.theme.color(app.theme.dim, app.colors))
            .wrap(Wrap { trim: true }),
        area,
    );
}

#[derive(Debug)]
enum OverviewRow {
    Group {
        label: String,
        lane_count: usize,
        totals: TimelineTotals,
    },
    Lane {
        lane_index: usize,
    },
}

fn overview_rows(app: &TimelineApp) -> Vec<OverviewRow> {
    if app.group_by == TimelineGroupBy::Flat {
        return (0..app.doc.lanes.len())
            .map(|lane_index| OverviewRow::Lane { lane_index })
            .collect();
    }

    let mut groups: BTreeMap<String, OverviewGroup> = BTreeMap::new();
    for (lane_index, lane) in app.doc.lanes.iter().enumerate() {
        let (key, label) = lane_group_key_label(lane, app.group_by);
        let group = groups.entry(key).or_insert_with(|| OverviewGroup {
            label,
            lane_indices: Vec::new(),
            totals: TimelineTotals::default(),
        });
        group.lane_indices.push(lane_index);
        add_timeline_totals(&mut group.totals, &lane.totals);
    }

    let mut rows = Vec::new();
    for mut group in groups.into_values() {
        group.lane_indices.sort_by(|a, b| {
            compare_lanes_in_group(&app.doc.lanes[*a], &app.doc.lanes[*b], app.group_by)
        });
        rows.push(OverviewRow::Group {
            label: group.label,
            lane_count: group.lane_indices.len(),
            totals: group.totals,
        });
        rows.extend(
            group
                .lane_indices
                .into_iter()
                .map(|lane_index| OverviewRow::Lane { lane_index }),
        );
    }
    rows
}

#[derive(Debug)]
struct OverviewGroup {
    label: String,
    lane_indices: Vec<usize>,
    totals: TimelineTotals,
}

fn selected_overview_row(rows: &[OverviewRow], selected_lane: usize) -> Option<usize> {
    rows.iter().position(|row| match row {
        OverviewRow::Group { .. } => false,
        OverviewRow::Lane { lane_index } => *lane_index == selected_lane,
    })
}

fn lane_group_key_label(lane: &TimelineLane, group_by: TimelineGroupBy) -> (String, String) {
    match group_by {
        TimelineGroupBy::Session => {
            let label = lane
                .session_name
                .as_deref()
                .or(lane.session_id.as_deref())
                .unwrap_or("no session")
                .to_string();
            let key = if label == "no session" {
                "zzzz:no-session".to_string()
            } else {
                format!("session:{}", label.to_ascii_lowercase())
            };
            (key, label)
        }
        TimelineGroupBy::Kind => {
            let label = lane_kind_label(lane.kind).to_string();
            (
                format!("kind:{:02}:{label}", cli_lane_rank(lane.kind)),
                label,
            )
        }
        TimelineGroupBy::Flat => ("flat".to_string(), "flat".to_string()),
    }
}

fn compare_lanes_in_group(
    a: &TimelineLane,
    b: &TimelineLane,
    group_by: TimelineGroupBy,
) -> std::cmp::Ordering {
    let rank = cli_lane_rank(a.kind).cmp(&cli_lane_rank(b.kind));
    if rank != std::cmp::Ordering::Equal {
        return rank;
    }
    overview_lane_label(a, group_by).cmp(&overview_lane_label(b, group_by))
}

fn add_timeline_totals(total: &mut TimelineTotals, next: &TimelineTotals) {
    total.working_secs = total.working_secs.saturating_add(next.working_secs);
    total.waiting_secs = total.waiting_secs.saturating_add(next.waiting_secs);
    total.error_secs = total.error_secs.saturating_add(next.error_secs);
    total.idle_secs = total.idle_secs.saturating_add(next.idle_secs);
    total.starting_secs = total.starting_secs.saturating_add(next.starting_secs);
    total.stopped_secs = total.stopped_secs.saturating_add(next.stopped_secs);
    total.human_secs = total.human_secs.saturating_add(next.human_secs);
    total.foreground_secs = total.foreground_secs.saturating_add(next.foreground_secs);
}

fn render_group_line(
    label: &str,
    lane_count: usize,
    totals: &TimelineTotals,
    label_width: u16,
    track_width: u16,
    app: &TimelineApp,
) -> Line<'static> {
    let label = truncate_pad(&format!("▾ {label}"), usize::from(label_width));
    let summary = truncate_pad(
        &format!("{lane_count} lanes · {}", timeline_totals_label(totals)),
        usize::from(track_width),
    );
    Line::from(vec![
        Span::styled(label, app.theme.accent_style(app.colors)),
        Span::raw(" "),
        Span::styled(summary, app.theme.color(app.theme.dim, app.colors)),
    ])
}

fn render_lane_line(
    lane: &TimelineLane,
    selected: bool,
    label_width: u16,
    track_width: u16,
    app: &TimelineApp,
) -> Line<'static> {
    let mut spans = Vec::new();
    let label = truncate_pad(
        &overview_lane_label(lane, app.group_by),
        usize::from(label_width),
    );
    let label_style = if selected {
        Style::default()
            .bg(app.theme.selected_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        app.theme.color(app.theme.dim, app.colors)
    };
    spans.push(Span::styled(label, label_style));
    spans.push(Span::raw(" "));
    spans.extend(track_spans(
        lane,
        app.window_started_at,
        app.window_ended_at,
        track_width,
        app,
    ));
    Line::from(spans)
}

fn overview_lane_label(lane: &TimelineLane, group_by: TimelineGroupBy) -> String {
    if group_by != TimelineGroupBy::Session {
        return lane.label.clone();
    }

    let short = match lane.kind {
        TimelineLaneKind::Agent => lane
            .agent_kind
            .map_or_else(|| "agent".to_string(), |kind| kind.to_string()),
        TimelineLaneKind::Human => "human".to_string(),
        TimelineLaneKind::Tmux => "tmux".to_string(),
    };
    format!("  {short}")
}

fn lane_kind_label(kind: TimelineLaneKind) -> &'static str {
    match kind {
        TimelineLaneKind::Agent => "agent",
        TimelineLaneKind::Human => "human",
        TimelineLaneKind::Tmux => "tmux",
    }
}

fn cli_lane_rank(kind: TimelineLaneKind) -> u8 {
    match kind {
        TimelineLaneKind::Agent => 0,
        TimelineLaneKind::Human => 1,
        TimelineLaneKind::Tmux => 2,
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn track_spans(
    lane: &TimelineLane,
    start: OffsetDateTime,
    end: OffsetDateTime,
    width: u16,
    app: &TimelineApp,
) -> Vec<Span<'static>> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let mut cells = vec![TrackCell::empty(); width];
    for interval in &lane.intervals {
        if interval.ended_at <= start || interval.started_at >= end {
            continue;
        }
        let left = timeline_pos(interval.started_at.max(start), start, end, width).floor() as usize;
        let right = timeline_pos(interval.ended_at.min(end), start, end, width).ceil() as usize;
        let right = right.max(left + 1).min(width);
        for cell in &mut cells[left.min(width)..right] {
            *cell = TrackCell::from_interval(interval);
        }
    }

    let mut spans = Vec::new();
    let mut idx = 0;
    while idx < cells.len() {
        let cell = cells[idx];
        let mut end_idx = idx + 1;
        while end_idx < cells.len() && cells[end_idx] == cell {
            end_idx += 1;
        }
        let len = end_idx - idx;
        spans.push(Span::styled(
            cell.symbol().repeat(len),
            cell.style(app.theme, app.colors),
        ));
        idx = end_idx;
    }
    spans
}

fn focus_line(interval: &TimelineInterval, selected: bool, app: &TimelineApp) -> Line<'static> {
    let glyph = match interval.source {
        TimelineIntervalSource::AgentState => match interval.state {
            Some(AgentState::Error) => "◆",
            Some(AgentState::WaitingInput | AgentState::WaitingChoice) => "◇",
            Some(AgentState::Working) => "●",
            Some(AgentState::Starting) => "◌",
            Some(AgentState::Idle | AgentState::Stopped) | None => "○",
        },
        TimelineIntervalSource::HumanInteraction => "◇",
        TimelineIntervalSource::SessionForeground => "╞",
    };
    let selected_style = if selected {
        Style::default()
            .bg(app.theme.selected_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{glyph} "), interval_style(interval, app)),
        Span::styled(
            format!("{:<8}", format_short_time(interval.started_at)),
            app.theme.color(app.theme.dim, app.colors),
        ),
        Span::styled(format!("{:<18}", interval_label(interval)), selected_style),
        Span::styled(
            format!("{:>8}", format_duration(interval.duration_secs)),
            app.theme.color(app.theme.dim, app.colors),
        ),
        Span::raw("  "),
        Span::styled(
            interval.detail.clone(),
            app.theme.color(app.theme.dim, app.colors),
        ),
    ])
}

fn legend_spans(theme: TimelineTheme, colors: bool) -> Vec<Span<'static>> {
    [
        ("█ working", TrackCell::Agent(AgentState::Working)),
        ("░ waiting", TrackCell::Agent(AgentState::WaitingInput)),
        ("■ error", TrackCell::Agent(AgentState::Error)),
        ("▁ idle", TrackCell::Agent(AgentState::Idle)),
        ("▓ human", TrackCell::Human),
        ("═ tmux", TrackCell::Tmux),
    ]
    .into_iter()
    .flat_map(|(label, cell)| {
        [
            Span::styled(label.to_string(), cell.style(theme, colors)),
            Span::raw("  ".to_string()),
        ]
    })
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackCell {
    Empty,
    Agent(AgentState),
    Human,
    Tmux,
}

impl TrackCell {
    fn empty() -> Self {
        Self::Empty
    }

    fn from_interval(interval: &TimelineInterval) -> Self {
        match interval.source {
            TimelineIntervalSource::AgentState => interval.state.map_or(Self::Empty, Self::Agent),
            TimelineIntervalSource::HumanInteraction => Self::Human,
            TimelineIntervalSource::SessionForeground => Self::Tmux,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Empty => " ",
            Self::Agent(AgentState::Working) => "█",
            Self::Agent(AgentState::WaitingInput | AgentState::WaitingChoice) => "░",
            Self::Agent(AgentState::Error) => "■",
            Self::Agent(AgentState::Starting) => "▒",
            Self::Agent(AgentState::Idle | AgentState::Stopped) => "▁",
            Self::Human => "▓",
            Self::Tmux => "═",
        }
    }

    fn style(self, theme: TimelineTheme, colors: bool) -> Style {
        let color = match self {
            Self::Empty => theme.dim,
            Self::Agent(AgentState::Working) => theme.working,
            Self::Agent(AgentState::WaitingInput) => theme.waiting,
            Self::Agent(AgentState::WaitingChoice) => theme.choice,
            Self::Agent(AgentState::Error) => theme.error,
            Self::Agent(AgentState::Starting) => theme.starting,
            Self::Agent(AgentState::Idle | AgentState::Stopped) => theme.idle,
            Self::Human => theme.human,
            Self::Tmux => theme.tmux,
        };
        theme.color(color, colors)
    }
}

fn interval_style(interval: &TimelineInterval, app: &TimelineApp) -> Style {
    TrackCell::from_interval(interval)
        .style(app.theme, app.colors)
        .add_modifier(Modifier::BOLD)
}

fn interval_label(interval: &TimelineInterval) -> String {
    match interval.source {
        TimelineIntervalSource::AgentState => interval
            .state
            .map_or_else(|| "agent".to_string(), |state| state.to_string()),
        TimelineIntervalSource::HumanInteraction => interval
            .human_kind
            .map_or_else(|| "human".to_string(), |kind| format!("{kind:?}")),
        TimelineIntervalSource::SessionForeground => "tmux foreground".to_string(),
    }
}

fn timeline_totals_label(totals: &TimelineTotals) -> String {
    let parts = [
        ("work", totals.working_secs),
        ("wait", totals.waiting_secs),
        ("err", totals.error_secs),
        ("human", totals.human_secs),
        ("tmux", totals.foreground_secs),
    ]
    .into_iter()
    .filter(|(_, secs)| *secs > 0)
    .take(3)
    .map(|(label, secs)| format!("{label} {}", format_duration(secs)))
    .collect::<Vec<_>>();

    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" · ")
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn axis_line(start: OffsetDateTime, end: OffsetDateTime, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let mut chars = vec![' '; width];
    let ticks = if width >= 72 { 5 } else { 3 };
    for i in 0..=ticks {
        let ratio = f64::from(i) / f64::from(ticks);
        let pos = ((width.saturating_sub(1)) as f64 * ratio).round() as usize;
        let at =
            start + time::Duration::seconds(((end - start).whole_seconds() as f64 * ratio) as i64);
        let label = format_short_time(at);
        for (offset, ch) in label.chars().enumerate() {
            let idx = pos.saturating_add(offset).min(width.saturating_sub(1));
            chars[idx] = ch;
        }
    }
    chars.into_iter().collect()
}

#[allow(clippy::cast_precision_loss)]
fn timeline_pos(
    at: OffsetDateTime,
    start: OffsetDateTime,
    end: OffsetDateTime,
    width: usize,
) -> f64 {
    let total = (end - start).whole_milliseconds().max(1) as f64;
    let offset = (at - start).whole_milliseconds().max(0) as f64;
    (offset / total * width as f64).clamp(0.0, width as f64)
}

fn visible_start(selected: usize, visible: usize, len: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    selected
        .saturating_sub(visible / 2)
        .min(len.saturating_sub(visible))
}

fn add_delta(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta.unsigned_abs()).min(len - 1)
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn truncate_pad(s: &str, width: usize) -> String {
    let mut out = s.chars().take(width).collect::<String>();
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

fn format_short_time(at: OffsetDateTime) -> String {
    local_time(at)
        .format(time::macros::format_description!("[hour]:[minute]"))
        .unwrap_or_else(|_| at.to_string())
}

fn format_full_time(at: OffsetDateTime) -> String {
    local_time(at)
        .format(time::macros::format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second]"
        ))
        .unwrap_or_else(|_| at.to_string())
}

fn local_time(at: OffsetDateTime) -> OffsetDateTime {
    at.to_offset(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC))
}

fn format_duration(total_secs: u64) -> String {
    if total_secs == 0 {
        return "-".to_string();
    }
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let minutes = total_secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let mins = minutes % 60;
    if hours < 24 {
        return format!("{hours}h{mins:02}m");
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    format!("{days}d{rem_hours:02}h")
}

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
        if let Some(mut terminal) = self.terminal.take() {
            let _ = disable_raw_mode();
            let _ = execute!(
                terminal.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture
            );
            let _ = terminal.show_cursor();
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

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn empty_doc(start: OffsetDateTime, end: OffsetDateTime) -> TimelineDocument {
        TimelineDocument {
            generated_at: end,
            range: core_timeline::TimelineRange {
                label: "test".to_string(),
                since_at: Some(start),
                until_at: Some(end),
            },
            window_started_at: start,
            window_ended_at: end,
            lanes: Vec::new(),
            totals: core_timeline::TimelineTotals::default(),
            notes: Vec::new(),
        }
    }

    fn lane(
        label: &str,
        kind: TimelineLaneKind,
        session_name: Option<&str>,
        totals: TimelineTotals,
    ) -> TimelineLane {
        TimelineLane {
            id: label.to_string(),
            label: label.to_string(),
            kind,
            agent_kind: (kind == TimelineLaneKind::Agent).then_some(AgentKind::Codex),
            session_id: session_name.map(|name| format!("id-{name}")),
            session_name: session_name.map(str::to_string),
            totals,
            intervals: Vec::new(),
        }
    }

    #[test]
    fn add_delta_clamps() {
        assert_eq!(add_delta(0, -1, 3), 0);
        assert_eq!(add_delta(0, 1, 3), 1);
        assert_eq!(add_delta(2, 1, 3), 2);
    }

    #[test]
    fn timeline_pos_maps_bounds() {
        let start = datetime!(2026-06-05 10:00:00 UTC);
        let end = datetime!(2026-06-05 11:00:00 UTC);
        assert!((timeline_pos(start, start, end, 100) - 0.0).abs() < f64::EPSILON);
        assert!((timeline_pos(end, start, end, 100) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn app_starts_on_latest_view_when_range_is_wide() {
        let start = datetime!(2026-06-05 00:00:00 UTC);
        let end = datetime!(2026-06-05 12:00:00 UTC);
        let app = TimelineApp::new(
            empty_doc(start, end),
            WatchTheme::Classic,
            false,
            TimelineGroupBy::Session,
        );

        assert_eq!(app.window_started_at, end - time::Duration::hours(6));
        assert_eq!(app.window_ended_at, end);
    }

    #[test]
    fn h_pan_moves_latest_view_back() {
        let start = datetime!(2026-06-05 00:00:00 UTC);
        let end = datetime!(2026-06-05 12:00:00 UTC);
        let mut app = TimelineApp::new(
            empty_doc(start, end),
            WatchTheme::Classic,
            false,
            TimelineGroupBy::Session,
        );
        let before = (app.window_started_at, app.window_ended_at);

        app.pan(-1);

        assert!(app.window_started_at < before.0);
        assert!(app.window_ended_at < before.1);
        assert_eq!(app.status, None);
    }

    #[test]
    fn l_pan_at_latest_edge_reports_status() {
        let start = datetime!(2026-06-05 00:00:00 UTC);
        let end = datetime!(2026-06-05 12:00:00 UTC);
        let mut app = TimelineApp::new(
            empty_doc(start, end),
            WatchTheme::Classic,
            false,
            TimelineGroupBy::Session,
        );
        let before = (app.window_started_at, app.window_ended_at);

        app.pan(1);

        assert_eq!((app.window_started_at, app.window_ended_at), before);
        assert_eq!(app.status.as_deref(), Some("at latest edge"));
    }

    #[test]
    fn reset_latest_and_fit_full_are_distinct() {
        let start = datetime!(2026-06-05 00:00:00 UTC);
        let end = datetime!(2026-06-05 12:00:00 UTC);
        let mut app = TimelineApp::new(
            empty_doc(start, end),
            WatchTheme::Classic,
            false,
            TimelineGroupBy::Session,
        );

        app.fit_window();
        assert_eq!(app.window_started_at, start);
        assert_eq!(app.window_ended_at, end);
        assert_eq!(app.status.as_deref(), Some("full range"));

        app.reset_window();
        assert_eq!(app.window_started_at, end - time::Duration::hours(6));
        assert_eq!(app.window_ended_at, end);
        assert_eq!(app.status.as_deref(), Some("latest view"));
    }

    #[test]
    fn overview_rows_group_by_session_by_default() {
        let start = datetime!(2026-06-05 00:00:00 UTC);
        let end = datetime!(2026-06-05 12:00:00 UTC);
        let mut doc = empty_doc(start, end);
        doc.lanes = vec![
            lane(
                "tmux/main",
                TimelineLaneKind::Tmux,
                Some("main"),
                TimelineTotals {
                    foreground_secs: 60,
                    ..TimelineTotals::default()
                },
            ),
            lane(
                "codex/side",
                TimelineLaneKind::Agent,
                Some("side"),
                TimelineTotals {
                    working_secs: 120,
                    ..TimelineTotals::default()
                },
            ),
            lane(
                "human/main",
                TimelineLaneKind::Human,
                Some("main"),
                TimelineTotals {
                    human_secs: 30,
                    ..TimelineTotals::default()
                },
            ),
        ];
        let app = TimelineApp::new(doc, WatchTheme::Classic, false, TimelineGroupBy::Session);
        let rows = overview_rows(&app);

        assert!(matches!(
            &rows[0],
            OverviewRow::Group { label, lane_count, .. }
                if label == "main" && *lane_count == 2
        ));
        assert!(matches!(&rows[1], OverviewRow::Lane { lane_index } if *lane_index == 2));
        assert!(matches!(&rows[2], OverviewRow::Lane { lane_index } if *lane_index == 0));
        assert!(matches!(
            &rows[3],
            OverviewRow::Group { label, lane_count, .. }
                if label == "side" && *lane_count == 1
        ));
        assert!(matches!(&rows[4], OverviewRow::Lane { lane_index } if *lane_index == 1));
    }
}
