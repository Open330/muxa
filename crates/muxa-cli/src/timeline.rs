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
use muxa::{AgentKind, AgentState, Config, ScopeExclusions};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};
use time::{Date, OffsetDateTime, UtcOffset, Weekday};

use crate::theme::ThemeArg;
use crate::use_colors;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const INPUT_POLL: Duration = Duration::from_millis(120);
const INITIAL_VIEWPORT_SECS: i64 = 6 * 60 * 60;
const MIN_WINDOW_SECS: i64 = 60;

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Time window to include: today, yesterday, week, month, last-week, last-month, 24h, 7d, RFC3339 timestamp, or all.
    #[arg(long, default_value = "today")]
    since: String,

    /// Focus one local calendar day, e.g. 2026-06-06.
    #[arg(long, value_name = "YYYY-MM-DD")]
    day: Option<String>,

    /// Focus a tmux session by name, session id, or pane id.
    #[arg(long)]
    session: Option<String>,

    /// Exclude pane ids matching a glob. Repeat or comma-separate values.
    #[arg(long = "exclude-pane", value_name = "GLOB", value_delimiter = ',')]
    exclude_pane: Vec<String>,

    /// Exclude tmux session names or ids matching a glob. Repeat or comma-separate values.
    #[arg(long = "exclude-session", value_name = "GLOB", value_delimiter = ',')]
    exclude_session: Vec<String>,

    /// Filter agent lanes by kind.
    #[arg(long, value_enum)]
    agent: Option<AgentKindArg>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Tui)]
    format: OutputFormat,

    /// Timeline presentation.
    #[arg(long, value_enum, default_value_t = TimelineCliView::Timeline)]
    view: TimelineCliView,

    /// Group lanes in the TUI overview.
    #[arg(long, value_enum, default_value_t = TimelineGroupBy::Session)]
    group_by: TimelineGroupBy,

    /// Sort groups and lanes.
    #[arg(long, value_enum, default_value_t = TimelineSort::Latest)]
    sort: TimelineSort,

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
enum TimelineCliView {
    Timeline,
    #[value(alias = "calendar", alias = "contrib", alias = "contribution")]
    Heatmap,
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
enum TimelineSort {
    Latest,
    Name,
    #[value(alias = "dur", alias = "total")]
    Duration,
    #[value(alias = "work")]
    Working,
    #[value(alias = "wait")]
    Waiting,
    #[value(alias = "err")]
    Error,
    Human,
    #[value(alias = "tmux")]
    Foreground,
}

impl TimelineSort {
    fn label(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Name => "name",
            Self::Duration => "duration",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Error => "error",
            Self::Human => "human",
            Self::Foreground => "foreground",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Latest => Self::Duration,
            Self::Duration => Self::Working,
            Self::Working => Self::Waiting,
            Self::Waiting => Self::Error,
            Self::Error => Self::Human,
            Self::Human => Self::Foreground,
            Self::Foreground => Self::Name,
            Self::Name => Self::Latest,
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
        OutputFormat::Tui => match args.view {
            TimelineCliView::Timeline => run_tui(client, cfg, args, doc).await,
            TimelineCliView::Heatmap => {
                print_heatmap(&doc);
                Ok(())
            }
        },
    }
}

async fn load_document(client: &Client, cfg: &Config, args: &Args) -> Result<TimelineDocument> {
    let now = OffsetDateTime::now_utc();
    let since = args.day.as_deref().unwrap_or(&args.since);
    let range = core_timeline::parse_since(since, now, "all retained activity")
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
    let prompt_entries = client
        .recent_prompts(None, Some(0))
        .await
        .context("querying daemon prompt history")?;
    let session_activities = load_session_activities(cfg).await;
    let pane_sessions = muxa::default_backend()
        .list_panes()
        .into_iter()
        .map(|pane| (pane.pane_id, pane.session))
        .collect::<HashMap<_, _>>();

    let mut doc = core_timeline::build_document(TimelineBuildInput {
        now,
        range,
        prompt_entries: &prompt_entries,
        activity_entries: &activity_entries,
        agents: &agents,
        session_activities: &session_activities,
        pane_sessions: &pane_sessions,
        active_lookback_secs: cfg.stats.active_lookback_secs,
        active_timeout_secs: cfg.stats.active_timeout_secs,
        active_tick_timeout_secs: cfg.stats.active_tick_timeout_secs,
        filters: TimelineFilters {
            session: args.session.clone(),
            agent_kind: args.agent.map(AgentKind::from),
            exclusions: ScopeExclusions::new(
                args.exclude_pane.clone(),
                args.exclude_session.clone(),
            ),
        },
        notes,
    });
    sort_document(&mut doc, args.sort);
    Ok(doc)
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
    let mut app = TimelineApp::new(initial_doc, theme, use_colors(), args.group_by, args.sort);
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
    sort: TimelineSort,
}

impl TimelineApp {
    fn new(
        mut doc: TimelineDocument,
        theme: WatchTheme,
        colors: bool,
        group_by: TimelineGroupBy,
        sort: TimelineSort,
    ) -> Self {
        sort_document(&mut doc, sort);
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
            sort,
        }
    }

    fn replace_doc(&mut self, doc: TimelineDocument) {
        let selected_lane_id = self.selected_lane_ref().map(|lane| lane.id.clone());
        let selected_lane = self.selected_lane;
        let mut doc = doc;
        sort_document(&mut doc, self.sort);
        let follow_live = (self.doc.window_ended_at - self.window_ended_at)
            .whole_seconds()
            .abs()
            <= 3;
        let current_span = window_span_secs(self.window_started_at, self.window_ended_at);
        self.doc = doc;
        self.restore_selected_lane(selected_lane_id.as_deref(), selected_lane);
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

    fn restore_selected_lane(&mut self, selected_lane_id: Option<&str>, fallback_lane: usize) {
        let fallback = fallback_lane.min(self.doc.lanes.len().saturating_sub(1));
        self.selected_lane = selected_lane_id
            .and_then(|id| self.doc.lanes.iter().position(|lane| lane.id == id))
            .unwrap_or(fallback);
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

    fn cycle_sort(&mut self) {
        let selected_lane_id = self.selected_lane_ref().map(|lane| lane.id.clone());
        let selected_lane = self.selected_lane;
        self.sort = self.sort.next();
        sort_document(&mut self.doc, self.sort);
        self.restore_selected_lane(selected_lane_id.as_deref(), selected_lane);
        self.status = Some(format!("sort {}", self.sort.label()));
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
        KeyCode::Char('s') => app.cycle_sort(),
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
        "{} · {} · {} lanes · group {} · sort {}",
        app.theme.title.trim(),
        app.doc.range.label,
        app.doc.lanes.len(),
        app.group_by.label(),
        app.sort.label()
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
        " {mode}  j/k select  h/l pan  +/- zoom  0 latest  f fit  g group  s sort  tab interval  enter/o toggle  r refresh  ? help  q quit"
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
        "  s                 cycle sorting: latest, duration, working, waiting, error, human, foreground, name",
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
            latest_at: None,
        });
        group.lane_indices.push(lane_index);
        add_timeline_totals(&mut group.totals, &lane.totals);
        group.latest_at = group.latest_at.max(lane_latest_at(lane));
    }

    let mut rows = Vec::new();
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|a, b| compare_groups(a, b, app.sort, app.group_by));
    for mut group in groups {
        group.lane_indices.sort_by(|a, b| {
            compare_lanes_in_group(
                &app.doc.lanes[*a],
                &app.doc.lanes[*b],
                app.group_by,
                app.sort,
            )
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
    latest_at: Option<OffsetDateTime>,
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
    sort: TimelineSort,
) -> Ordering {
    compare_lanes(a, b, sort, group_by)
}

fn sort_document(doc: &mut TimelineDocument, sort: TimelineSort) {
    doc.lanes
        .sort_by(|a, b| compare_lanes(a, b, sort, TimelineGroupBy::Flat));
}

fn compare_lanes(
    a: &TimelineLane,
    b: &TimelineLane,
    sort: TimelineSort,
    group_by: TimelineGroupBy,
) -> Ordering {
    compare_lane_sort(a, b, sort)
        .then_with(|| cli_lane_rank(a.kind).cmp(&cli_lane_rank(b.kind)))
        .then_with(|| overview_lane_label(a, group_by).cmp(&overview_lane_label(b, group_by)))
        .then_with(|| a.id.cmp(&b.id))
}

fn compare_groups(
    a: &OverviewGroup,
    b: &OverviewGroup,
    sort: TimelineSort,
    group_by: TimelineGroupBy,
) -> Ordering {
    compare_group_sort(a, b, sort)
        .then_with(|| compare_group_fallback(a, b, group_by))
        .then_with(|| a.label.cmp(&b.label))
}

fn compare_lane_sort(a: &TimelineLane, b: &TimelineLane, sort: TimelineSort) -> Ordering {
    match sort {
        TimelineSort::Latest => compare_desc(lane_latest_at(a), lane_latest_at(b)),
        TimelineSort::Name => normalized_cmp(&a.label, &b.label),
        TimelineSort::Duration => compare_desc(
            total_duration_secs(&a.totals),
            total_duration_secs(&b.totals),
        ),
        TimelineSort::Working => compare_desc(a.totals.working_secs, b.totals.working_secs),
        TimelineSort::Waiting => compare_desc(a.totals.waiting_secs, b.totals.waiting_secs),
        TimelineSort::Error => compare_desc(a.totals.error_secs, b.totals.error_secs),
        TimelineSort::Human => compare_desc(a.totals.human_secs, b.totals.human_secs),
        TimelineSort::Foreground => {
            compare_desc(a.totals.foreground_secs, b.totals.foreground_secs)
        }
    }
}

fn compare_group_sort(a: &OverviewGroup, b: &OverviewGroup, sort: TimelineSort) -> Ordering {
    match sort {
        TimelineSort::Latest => compare_desc(a.latest_at, b.latest_at),
        TimelineSort::Name => normalized_cmp(&a.label, &b.label),
        TimelineSort::Duration => compare_desc(
            total_duration_secs(&a.totals),
            total_duration_secs(&b.totals),
        ),
        TimelineSort::Working => compare_desc(a.totals.working_secs, b.totals.working_secs),
        TimelineSort::Waiting => compare_desc(a.totals.waiting_secs, b.totals.waiting_secs),
        TimelineSort::Error => compare_desc(a.totals.error_secs, b.totals.error_secs),
        TimelineSort::Human => compare_desc(a.totals.human_secs, b.totals.human_secs),
        TimelineSort::Foreground => {
            compare_desc(a.totals.foreground_secs, b.totals.foreground_secs)
        }
    }
}

fn compare_group_fallback(
    a: &OverviewGroup,
    b: &OverviewGroup,
    group_by: TimelineGroupBy,
) -> Ordering {
    if group_by == TimelineGroupBy::Kind {
        group_kind_rank(&a.label).cmp(&group_kind_rank(&b.label))
    } else {
        normalized_cmp(&a.label, &b.label)
    }
}

fn compare_desc<T: Ord>(a: T, b: T) -> Ordering {
    b.cmp(&a)
}

fn normalized_cmp(a: &str, b: &str) -> Ordering {
    a.to_ascii_lowercase()
        .cmp(&b.to_ascii_lowercase())
        .then_with(|| a.cmp(b))
}

fn lane_latest_at(lane: &TimelineLane) -> Option<OffsetDateTime> {
    lane.intervals
        .iter()
        .map(|interval| interval.ended_at)
        .max()
}

fn total_duration_secs(totals: &TimelineTotals) -> u64 {
    totals
        .working_secs
        .saturating_add(totals.waiting_secs)
        .saturating_add(totals.error_secs)
        .saturating_add(totals.idle_secs)
        .saturating_add(totals.starting_secs)
        .saturating_add(totals.stopped_secs)
        .saturating_add(totals.human_secs)
        .saturating_add(totals.foreground_secs)
}

fn group_kind_rank(label: &str) -> u8 {
    match label {
        "agent" => cli_lane_rank(TimelineLaneKind::Agent),
        "human" => cli_lane_rank(TimelineLaneKind::Human),
        "tmux" => cli_lane_rank(TimelineLaneKind::Tmux),
        _ => u8::MAX,
    }
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

#[derive(Debug, Clone)]
struct TimelineDayBucket {
    date: Date,
    totals: TimelineTotals,
    session_secs: BTreeMap<String, u64>,
}

impl TimelineDayBucket {
    fn new(date: Date) -> Self {
        Self {
            date,
            totals: TimelineTotals::default(),
            session_secs: BTreeMap::new(),
        }
    }

    fn active_secs(&self) -> u64 {
        self.totals
            .working_secs
            .saturating_add(self.totals.waiting_secs)
            .saturating_add(self.totals.error_secs)
            .saturating_add(self.totals.human_secs)
            .saturating_add(self.totals.foreground_secs)
    }
}

fn print_heatmap(doc: &TimelineDocument) {
    let buckets = timeline_day_buckets(doc);
    let start = buckets.first().map_or_else(
        || local_time(doc.window_started_at).date(),
        |bucket| bucket.date,
    );
    let end = buckets.last().map_or_else(
        || local_time(doc.window_ended_at).date(),
        |bucket| bucket.date,
    );
    println!(
        "muxa timeline heatmap · {} · {} → {}",
        doc.range.label, start, end
    );
    println!(
        "{} lanes · {}",
        doc.lanes.len(),
        timeline_totals_label(&doc.totals)
    );
    if buckets.is_empty() {
        println!("no timeline intervals in this view");
        return;
    }
    println!();
    print_heatmap_grid(&buckets);
    println!();
    println!("legend  · none  ░ low  ▒ medium  ▓ high  █ peak");
    println!();
    print_top_days(&buckets);
    if buckets.len() <= 2 {
        println!();
        print_day_sessions(&buckets[0]);
    }
}

fn print_heatmap_grid(buckets: &[TimelineDayBucket]) {
    let max_secs = buckets
        .iter()
        .map(TimelineDayBucket::active_secs)
        .max()
        .unwrap_or(0);
    let leading = weekday_index(buckets[0].date.weekday());
    let mut cells = Vec::with_capacity(leading + buckets.len() + 6);
    cells.extend(std::iter::repeat_n(None, leading));
    cells.extend(buckets.iter().map(Some));
    while cells.len() % 7 != 0 {
        cells.push(None);
    }
    let weeks = cells.chunks(7).collect::<Vec<_>>();
    println!("{}", heatmap_month_header(&weeks));
    for weekday in [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ] {
        let row = weekday_index(weekday);
        let mut line = format!("{:>3} ", weekday_short_label(weekday));
        for week in &weeks {
            let cell = week[row].map_or(' ', |bucket| heatmap_char(bucket.active_secs(), max_secs));
            line.push(cell);
            line.push(' ');
        }
        println!("{line}");
    }
}

fn heatmap_month_header(weeks: &[&[Option<&TimelineDayBucket>]]) -> String {
    let mut line = "    ".to_string();
    let mut last_month = None;
    for week in weeks {
        let month = week
            .iter()
            .flatten()
            .map(|bucket| bucket.date.month())
            .next();
        if month.is_some() && month != last_month {
            let label = month
                .map(|month| format!("{month:?}"))
                .unwrap_or_default()
                .chars()
                .take(3)
                .collect::<String>();
            line.push_str(&label);
            last_month = month;
        } else {
            line.push_str("  ");
        }
    }
    line
}

fn print_top_days(buckets: &[TimelineDayBucket]) {
    let mut days = buckets
        .iter()
        .filter(|bucket| bucket.active_secs() > 0)
        .collect::<Vec<_>>();
    days.sort_by(|a, b| {
        b.active_secs()
            .cmp(&a.active_secs())
            .then_with(|| b.date.cmp(&a.date))
    });
    if days.is_empty() {
        println!("top days: no recorded activity");
        return;
    }
    println!("top days");
    for bucket in days.into_iter().take(8) {
        println!("  {}  {}", bucket.date, day_totals_label(&bucket.totals));
    }
}

fn print_day_sessions(bucket: &TimelineDayBucket) {
    let mut sessions = bucket.session_secs.iter().collect::<Vec<_>>();
    sessions.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    if sessions.is_empty() {
        println!("sessions: no active sessions on {}", bucket.date);
        return;
    }
    println!("sessions on {}", bucket.date);
    for (session, secs) in sessions.into_iter().take(10) {
        println!("  {session:<28} {}", format_duration(*secs));
    }
}

fn timeline_day_buckets(doc: &TimelineDocument) -> Vec<TimelineDayBucket> {
    if doc.window_ended_at <= doc.window_started_at {
        return Vec::new();
    }
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let start_date = doc.window_started_at.to_offset(offset).date();
    let end_anchor = (doc.window_ended_at - time::Duration::seconds(1)).max(doc.window_started_at);
    let end_date = end_anchor.to_offset(offset).date();
    let mut buckets = Vec::new();
    let mut date = start_date;
    loop {
        buckets.push(TimelineDayBucket::new(date));
        if date >= end_date {
            break;
        }
        let Some(next) = date.next_day() else {
            break;
        };
        date = next;
    }
    let positions = buckets
        .iter()
        .enumerate()
        .map(|(idx, bucket)| (bucket.date, idx))
        .collect::<BTreeMap<_, _>>();
    for lane in &doc.lanes {
        for interval in &lane.intervals {
            add_interval_to_day_buckets(
                &mut buckets,
                &positions,
                lane,
                interval,
                offset,
                doc.window_started_at,
                doc.window_ended_at,
            );
        }
    }
    buckets
}

fn add_interval_to_day_buckets(
    buckets: &mut [TimelineDayBucket],
    positions: &BTreeMap<Date, usize>,
    lane: &TimelineLane,
    interval: &TimelineInterval,
    offset: UtcOffset,
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
) {
    let mut cursor = interval.started_at.max(window_start);
    let ended_at = interval.ended_at.min(window_end);
    if ended_at <= cursor {
        return;
    }
    while cursor < ended_at {
        let date = cursor.to_offset(offset).date();
        let next_day_start = date
            .next_day()
            .map_or(ended_at, |next| local_day_start(next, offset));
        let segment_end = ended_at.min(next_day_start);
        let secs = u64::try_from((segment_end - cursor).whole_seconds().max(0)).unwrap_or(u64::MAX);
        if secs > 0 {
            if let Some(idx) = positions.get(&date).copied() {
                add_interval_secs(&mut buckets[idx].totals, interval, secs);
                let active = active_interval_secs(interval, secs);
                if active > 0 {
                    let session = interval_session_label(lane, interval);
                    *buckets[idx].session_secs.entry(session).or_default() += active;
                }
            }
        }
        cursor = segment_end;
    }
}

fn add_interval_secs(totals: &mut TimelineTotals, interval: &TimelineInterval, secs: u64) {
    match interval.source {
        TimelineIntervalSource::AgentState => match interval.state {
            Some(AgentState::Working) => totals.working_secs += secs,
            Some(AgentState::WaitingInput | AgentState::WaitingChoice) => {
                totals.waiting_secs += secs;
            }
            Some(AgentState::Error) => totals.error_secs += secs,
            Some(AgentState::Idle) => totals.idle_secs += secs,
            Some(AgentState::Starting) => totals.starting_secs += secs,
            Some(AgentState::Stopped) => totals.stopped_secs += secs,
            None => {}
        },
        TimelineIntervalSource::HumanInteraction => totals.human_secs += secs,
        TimelineIntervalSource::SessionForeground => totals.foreground_secs += secs,
    }
}

fn active_interval_secs(interval: &TimelineInterval, secs: u64) -> u64 {
    match interval.source {
        TimelineIntervalSource::AgentState => match interval.state {
            Some(
                AgentState::Working
                | AgentState::WaitingInput
                | AgentState::WaitingChoice
                | AgentState::Error,
            ) => secs,
            _ => 0,
        },
        TimelineIntervalSource::HumanInteraction | TimelineIntervalSource::SessionForeground => {
            secs
        }
    }
}

fn interval_session_label(lane: &TimelineLane, interval: &TimelineInterval) -> String {
    interval
        .session_name
        .as_ref()
        .or(lane.session_name.as_ref())
        .or(interval.session_id.as_ref())
        .or(lane.session_id.as_ref())
        .cloned()
        .unwrap_or_else(|| lane.label.clone())
}

fn day_totals_label(totals: &TimelineTotals) -> String {
    let active = totals
        .working_secs
        .saturating_add(totals.waiting_secs)
        .saturating_add(totals.error_secs)
        .saturating_add(totals.human_secs)
        .saturating_add(totals.foreground_secs);
    if active == 0 {
        return "-".to_string();
    }
    let parts = [
        ("active", active),
        ("work", totals.working_secs),
        ("wait", totals.waiting_secs),
        ("err", totals.error_secs),
        ("human", totals.human_secs),
        ("tmux", totals.foreground_secs),
    ]
    .into_iter()
    .filter(|(_, secs)| *secs > 0)
    .map(|(label, secs)| format!("{label} {}", format_duration(secs)))
    .collect::<Vec<_>>();
    parts.join(" · ")
}

fn heatmap_char(secs: u64, max_secs: u64) -> char {
    match heatmap_level(secs, max_secs) {
        0 => '·',
        1 => '░',
        2 => '▒',
        3 => '▓',
        _ => '█',
    }
}

fn heatmap_level(secs: u64, max_secs: u64) -> u8 {
    if secs == 0 {
        return 0;
    }
    if max_secs == 0 {
        return 1;
    }
    match secs.saturating_mul(4).div_ceil(max_secs) {
        0 => 1,
        level => u8::try_from(level.min(4)).unwrap_or(4),
    }
}

fn local_day_start(date: Date, offset: UtcOffset) -> OffsetDateTime {
    date.midnight().assume_offset(offset)
}

fn weekday_index(weekday: Weekday) -> usize {
    match weekday {
        Weekday::Monday => 0,
        Weekday::Tuesday => 1,
        Weekday::Wednesday => 2,
        Weekday::Thursday => 3,
        Weekday::Friday => 4,
        Weekday::Saturday => 5,
        Weekday::Sunday => 6,
    }
}

fn weekday_short_label(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    }
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
    use time::macros::{date, datetime};

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
            active_sessions: Vec::new(),
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

    fn agent_interval(started_at: OffsetDateTime, ended_at: OffsetDateTime) -> TimelineInterval {
        TimelineInterval {
            source: TimelineIntervalSource::AgentState,
            state: Some(AgentState::Working),
            human_kind: None,
            started_at,
            ended_at,
            duration_secs: u64::try_from((ended_at - started_at).whole_seconds()).unwrap_or(0),
            open: false,
            pane: None,
            session_id: None,
            session_name: None,
            cwd: None,
            detail: "test".to_string(),
        }
    }

    #[test]
    fn timeline_day_buckets_split_intervals_across_local_days() {
        let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let start = local_day_start(date!(2026 - 06 - 05), offset);
        let end = start + time::Duration::days(2);
        let mut doc = empty_doc(start, end);
        let mut lane = lane(
            "codex/main",
            TimelineLaneKind::Agent,
            Some("main"),
            TimelineTotals::default(),
        );
        lane.intervals.push(agent_interval(
            start + time::Duration::hours(23),
            start + time::Duration::hours(25),
        ));
        doc.lanes = vec![lane];

        let buckets = timeline_day_buckets(&doc);

        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].active_secs(), 3600);
        assert_eq!(buckets[1].active_secs(), 3600);
        assert_eq!(buckets[0].session_secs.get("main"), Some(&3600));
        assert_eq!(buckets[1].session_secs.get("main"), Some(&3600));
    }

    #[test]
    fn heatmap_level_handles_zero_and_relative_intensity() {
        assert_eq!(heatmap_level(0, 100), 0);
        assert_eq!(heatmap_level(1, 100), 1);
        assert_eq!(heatmap_level(50, 100), 2);
        assert_eq!(heatmap_level(100, 100), 4);
    }

    #[test]
    fn heatmap_weekday_index_is_monday_first() {
        assert_eq!(weekday_index(Weekday::Monday), 0);
        assert_eq!(weekday_index(Weekday::Tuesday), 1);
        assert_eq!(weekday_index(Weekday::Sunday), 6);
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
            TimelineSort::Latest,
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
            TimelineSort::Latest,
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
            TimelineSort::Latest,
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
            TimelineSort::Latest,
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
        let app = TimelineApp::new(
            doc,
            WatchTheme::Classic,
            false,
            TimelineGroupBy::Session,
            TimelineSort::Latest,
        );
        let rows = overview_rows(&app);

        assert!(matches!(
            &rows[0],
            OverviewRow::Group { label, lane_count, .. }
                if label == "main" && *lane_count == 2
        ));
        assert!(
            matches!(&rows[1], OverviewRow::Lane { lane_index } if app.doc.lanes[*lane_index].label == "human/main")
        );
        assert!(
            matches!(&rows[2], OverviewRow::Lane { lane_index } if app.doc.lanes[*lane_index].label == "tmux/main")
        );
        assert!(matches!(
            &rows[3],
            OverviewRow::Group { label, lane_count, .. }
                if label == "side" && *lane_count == 1
        ));
        assert!(
            matches!(&rows[4], OverviewRow::Lane { lane_index } if app.doc.lanes[*lane_index].label == "codex/side")
        );
    }

    #[test]
    fn sort_document_orders_lanes_by_latest_interval() {
        let start = datetime!(2026-06-05 00:00:00 UTC);
        let end = datetime!(2026-06-05 12:00:00 UTC);
        let mut doc = empty_doc(start, end);
        let mut older = lane(
            "codex/older",
            TimelineLaneKind::Agent,
            Some("older"),
            TimelineTotals::default(),
        );
        older.intervals.push(agent_interval(
            datetime!(2026-06-05 09:00:00 UTC),
            datetime!(2026-06-05 09:30:00 UTC),
        ));
        let mut newer = lane(
            "codex/newer",
            TimelineLaneKind::Agent,
            Some("newer"),
            TimelineTotals::default(),
        );
        newer.intervals.push(agent_interval(
            datetime!(2026-06-05 11:00:00 UTC),
            datetime!(2026-06-05 11:30:00 UTC),
        ));
        doc.lanes = vec![
            lane(
                "codex/empty",
                TimelineLaneKind::Agent,
                Some("empty"),
                TimelineTotals::default(),
            ),
            older,
            newer,
        ];

        sort_document(&mut doc, TimelineSort::Latest);

        let labels = doc
            .lanes
            .iter()
            .map(|lane| lane.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["codex/newer", "codex/older", "codex/empty"]);
    }

    #[test]
    fn overview_groups_follow_metric_sort() {
        let start = datetime!(2026-06-05 00:00:00 UTC);
        let end = datetime!(2026-06-05 12:00:00 UTC);
        let mut doc = empty_doc(start, end);
        doc.lanes = vec![
            lane(
                "codex/main",
                TimelineLaneKind::Agent,
                Some("main"),
                TimelineTotals {
                    waiting_secs: 60,
                    ..TimelineTotals::default()
                },
            ),
            lane(
                "codex/side",
                TimelineLaneKind::Agent,
                Some("side"),
                TimelineTotals {
                    waiting_secs: 180,
                    ..TimelineTotals::default()
                },
            ),
        ];
        let app = TimelineApp::new(
            doc,
            WatchTheme::Classic,
            false,
            TimelineGroupBy::Session,
            TimelineSort::Waiting,
        );
        let rows = overview_rows(&app);

        assert!(matches!(
            &rows[0],
            OverviewRow::Group { label, .. } if label == "side"
        ));
        assert!(matches!(
            &rows[2],
            OverviewRow::Group { label, .. } if label == "main"
        ));
    }
}
