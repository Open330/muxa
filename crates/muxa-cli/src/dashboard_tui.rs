//! `muxa dashboard` — session-first operator console.
//!
//! `muxa watch` is a compact picker. This module keeps the same trusted side
//! effects for tmux actions, but presents a richer card board where the user can
//! inspect panes and send prompts without attaching to the underlying session.

use anyhow::{Context, Result};
use clap::ValueEnum;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa::config::{IconSet, WatchTheme};
use muxa::event::RateLimitScope;
use muxa::ipc::Client;
use muxa::session::SessionBackendKind;
use muxa::session_activity::SessionActivity;
use muxa::tmux::PaneInfo;
use muxa::{
    Agent, AgentKind, AgentState, Config, HostKind, PaneBackend, ScopeExclusions, SessionRef,
    SurfaceKind,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::stats::{self, ActiveDuration, SessionActiveStats};
use crate::theme::ThemeArg;
use crate::watch::{self, ActionOutcome, QuickAction, RealEffects};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const CAPTURE_INTERVAL: Duration = Duration::from_millis(700);
const INPUT_POLL: Duration = Duration::from_millis(80);
const HINT_TTL: Duration = Duration::from_secs(3);
const CARD_HEIGHT: u16 = 6;
const MIN_CARD_HEIGHT: u16 = 5;
const DESKTOP_SESSION_PERCENT: u16 = 48;
const DESKTOP_INSPECTOR_PERCENT: u16 = 52;

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Include agents that are not attached to a tmux/zellij pane or muxa PTY.
    #[arg(long)]
    include_paneless: bool,

    /// ACT/WACT time window. Accepts the same values as `muxa stats --since`.
    #[arg(long, default_value = "today")]
    since: String,

    /// Card sort order.
    #[arg(long, value_enum, default_value_t = DashboardSort::Attention)]
    sort: DashboardSort,

    /// One-shot visual theme override.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DashboardSort {
    Attention,
    Activity,
    Active,
    Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    Pane(String),
    PtySession(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActionTarget {
    Pane(String),
    PtySession(String),
}

impl ActionTarget {
    fn capture_target(&self) -> CaptureTarget {
        match self {
            Self::Pane(pane) => CaptureTarget::Pane(pane.clone()),
            Self::PtySession(session) => CaptureTarget::PtySession(session.clone()),
        }
    }

    fn open_target(&self) -> OpenTarget {
        match self {
            Self::Pane(pane) => OpenTarget::Pane(pane.clone()),
            Self::PtySession(session) => OpenTarget::PtySession(session.clone()),
        }
    }

    fn prompt_target(&self) -> PromptTarget {
        match self {
            Self::Pane(pane) => PromptTarget::Pane(pane.clone()),
            Self::PtySession(session) => PromptTarget::PtySession(session.clone()),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Pane(pane) => format!("pane {pane}"),
            Self::PtySession(session) => format!("pty {session}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DashboardTheme {
    accent: Color,
    border: Color,
    selected: Color,
    selected_fg: Color,
    selected_bg: Color,
    dim: Color,
    panel: Color,
    surface: Color,
    surface_alt: Color,
    ok: Color,
    warn: Color,
    error: Color,
    working: Color,
    title: Color,
}

impl DashboardTheme {
    fn border_style(self) -> Style {
        Style::default().fg(self.border)
    }

    fn selected_border(self) -> Style {
        Style::default()
            .fg(self.selected)
            .add_modifier(Modifier::BOLD)
    }

    fn title_style(self) -> Style {
        Style::default().fg(self.title).add_modifier(Modifier::BOLD)
    }

    fn dim_style(self) -> Style {
        Style::default().fg(self.dim).add_modifier(Modifier::DIM)
    }

    fn key_style(self) -> Style {
        Style::default()
            .fg(self.selected_fg)
            .bg(self.selected)
            .add_modifier(Modifier::BOLD)
    }

    fn state_style(self, state: AgentState) -> Style {
        match state {
            AgentState::Error => Style::default().fg(self.error).add_modifier(Modifier::BOLD),
            AgentState::WaitingChoice | AgentState::WaitingInput => {
                Style::default().fg(self.warn).add_modifier(Modifier::BOLD)
            }
            AgentState::Working => Style::default()
                .fg(self.working)
                .add_modifier(Modifier::BOLD),
            AgentState::Starting => Style::default().fg(self.accent),
            AgentState::Idle => Style::default().fg(self.dim),
            AgentState::Stopped => Style::default().fg(self.dim).add_modifier(Modifier::DIM),
        }
    }

    fn status_bg(self, status: CardStatus) -> Color {
        match status {
            CardStatus::Error => self.error,
            CardStatus::WaitingChoice | CardStatus::WaitingInput => self.warn,
            CardStatus::Working => self.working,
            CardStatus::Starting => self.accent,
            CardStatus::Idle | CardStatus::Stopped | CardStatus::Empty => self.border,
        }
    }

    fn card_style(self, selected: bool) -> Style {
        if selected {
            Style::default().bg(self.selected_bg)
        } else {
            Style::default().bg(self.surface)
        }
    }
}

fn rich_icon(rich: &'static str, ascii: &'static str) -> &'static str {
    match crate::icon_set() {
        IconSet::Unicode => rich,
        IconSet::Ascii => ascii,
    }
}

fn icon_session() -> &'static str {
    rich_icon("", "S")
}

fn icon_agent() -> &'static str {
    rich_icon("󰒋", "A")
}

fn icon_time() -> &'static str {
    rich_icon("󰥔", "T")
}

fn icon_prompt() -> &'static str {
    rich_icon("󰍩", ">")
}

fn icon_target() -> &'static str {
    rich_icon("󰓾", "@")
}

fn icon_model() -> &'static str {
    rich_icon("󰚩", "M")
}

fn icon_activity() -> &'static str {
    rich_icon("󰃰", "*")
}

fn icon_capture() -> &'static str {
    rich_icon("󰆍", "$")
}

fn pill(text: impl Into<String>, fg: Color, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", text.into()),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

fn subtle_pill(text: impl Into<String>, theme: DashboardTheme) -> Span<'static> {
    Span::styled(
        format!(" {} ", text.into()),
        Style::default()
            .fg(theme.panel)
            .bg(theme.border)
            .add_modifier(Modifier::BOLD),
    )
}

fn status_pill(status: CardStatus, theme: DashboardTheme) -> Span<'static> {
    let fg = match status {
        CardStatus::Idle | CardStatus::Stopped | CardStatus::Empty => theme.panel,
        _ => Color::Black,
    };
    pill(status.label(), fg, theme.status_bg(status))
}

fn dashboard_theme(theme: WatchTheme) -> DashboardTheme {
    match theme {
        WatchTheme::Classic => DashboardTheme {
            accent: Color::Cyan,
            border: Color::DarkGray,
            selected: Color::Cyan,
            selected_fg: Color::Black,
            selected_bg: Color::Rgb(14, 38, 44),
            dim: Color::DarkGray,
            panel: Color::Gray,
            surface: Color::Reset,
            surface_alt: Color::Rgb(20, 20, 24),
            ok: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            working: Color::Green,
            title: Color::White,
        },
        WatchTheme::OhMyMuxa => DashboardTheme {
            accent: Color::LightMagenta,
            border: Color::DarkGray,
            selected: Color::LightMagenta,
            selected_fg: Color::Black,
            selected_bg: Color::Rgb(42, 22, 48),
            dim: Color::DarkGray,
            panel: Color::Gray,
            surface: Color::Reset,
            surface_alt: Color::Rgb(24, 20, 30),
            ok: Color::LightGreen,
            warn: Color::LightYellow,
            error: Color::LightRed,
            working: Color::LightCyan,
            title: Color::White,
        },
        WatchTheme::Focus => DashboardTheme {
            accent: Color::Blue,
            border: Color::DarkGray,
            selected: Color::Blue,
            selected_fg: Color::White,
            selected_bg: Color::Rgb(18, 28, 50),
            dim: Color::DarkGray,
            panel: Color::Gray,
            surface: Color::Reset,
            surface_alt: Color::Rgb(20, 22, 30),
            ok: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            working: Color::Cyan,
            title: Color::White,
        },
        WatchTheme::Ops => DashboardTheme {
            accent: Color::LightCyan,
            border: Color::Gray,
            selected: Color::LightCyan,
            selected_fg: Color::Black,
            selected_bg: Color::Rgb(18, 42, 44),
            dim: Color::DarkGray,
            panel: Color::White,
            surface: Color::Reset,
            surface_alt: Color::Rgb(18, 26, 26),
            ok: Color::LightGreen,
            warn: Color::LightYellow,
            error: Color::LightRed,
            working: Color::LightGreen,
            title: Color::White,
        },
        WatchTheme::Mono | WatchTheme::Minimal => DashboardTheme {
            accent: Color::White,
            border: Color::DarkGray,
            selected: Color::White,
            selected_fg: Color::Black,
            selected_bg: Color::DarkGray,
            dim: Color::DarkGray,
            panel: Color::Gray,
            surface: Color::Reset,
            surface_alt: Color::Black,
            ok: Color::White,
            warn: Color::White,
            error: Color::White,
            working: Color::White,
            title: Color::White,
        },
        WatchTheme::HighContrast => DashboardTheme {
            accent: Color::Yellow,
            border: Color::White,
            selected: Color::Yellow,
            selected_fg: Color::Black,
            selected_bg: Color::Blue,
            dim: Color::Gray,
            panel: Color::White,
            surface: Color::Reset,
            surface_alt: Color::Black,
            ok: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            working: Color::Cyan,
            title: Color::White,
        },
    }
}

#[derive(Debug, Clone)]
struct DashboardData {
    generated_at: OffsetDateTime,
    cards: Vec<SessionCard>,
    totals: DashboardTotals,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct DashboardTotals {
    sessions: usize,
    tracked_agents: usize,
    attention: usize,
    working: usize,
    active: ActiveDuration,
}

#[derive(Debug, Clone)]
struct SessionCard {
    key: String,
    label: String,
    host: CardHost,
    pane_ids: Vec<String>,
    pane_labels: Vec<String>,
    primary_pane: Option<String>,
    pty_session_id: Option<String>,
    cwd: Option<String>,
    agents: Vec<Agent>,
    status: CardStatus,
    counts: StateCounts,
    last_activity_at: Option<OffsetDateTime>,
    active: ActiveDuration,
    foreground_secs: Option<u64>,
    foreground_attached: bool,
    last_prompt: Option<String>,
    last_response: Option<String>,
    last_notification: Option<String>,
    model: Option<String>,
    cost_usd: Option<f64>,
    context_used_pct: Option<f32>,
    kinds: Vec<AgentKind>,
}

impl SessionCard {
    fn action_targets(&self) -> Vec<ActionTarget> {
        let mut targets = Vec::new();
        if let Some(pane) = self.primary_pane.as_ref() {
            push_action_target(&mut targets, ActionTarget::Pane(pane.clone()));
        }
        if let Some(session) = self.pty_session_id.as_ref() {
            push_action_target(&mut targets, ActionTarget::PtySession(session.clone()));
        }
        for agent in &self.agents {
            if let Some(pane) = agent.pane.as_ref() {
                push_action_target(&mut targets, ActionTarget::Pane(pane.clone()));
            } else if let Some(surface) = agent.surface.as_ref() {
                if surface.kind == SurfaceKind::Pty {
                    push_action_target(&mut targets, ActionTarget::PtySession(surface.id.clone()));
                }
            }
        }
        for pane in &self.pane_ids {
            push_action_target(&mut targets, ActionTarget::Pane(pane.clone()));
        }
        targets
    }

    fn capture_target(&self) -> Option<CaptureTarget> {
        self.action_targets()
            .into_iter()
            .next()
            .map(|target| target.capture_target())
    }

    fn attention_score(&self) -> u8 {
        match self.status {
            CardStatus::Error => 6,
            CardStatus::WaitingChoice => 5,
            CardStatus::WaitingInput => 4,
            CardStatus::Working => 3,
            CardStatus::Starting => 2,
            CardStatus::Idle | CardStatus::Empty => 1,
            CardStatus::Stopped => 0,
        }
    }
}

fn push_action_target(targets: &mut Vec<ActionTarget>, target: ActionTarget) {
    if !targets.contains(&target) {
        targets.push(target);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardHost {
    Tmux,
    Zellij,
    Pty,
    Pane,
    Agent,
}

impl CardHost {
    fn label(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Zellij => "zellij",
            Self::Pty => "pty",
            Self::Pane => "pane",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardStatus {
    Error,
    WaitingChoice,
    WaitingInput,
    Working,
    Starting,
    Idle,
    Stopped,
    Empty,
}

impl CardStatus {
    fn state(self) -> Option<AgentState> {
        match self {
            Self::Error => Some(AgentState::Error),
            Self::WaitingChoice => Some(AgentState::WaitingChoice),
            Self::WaitingInput => Some(AgentState::WaitingInput),
            Self::Working => Some(AgentState::Working),
            Self::Starting => Some(AgentState::Starting),
            Self::Idle => Some(AgentState::Idle),
            Self::Stopped => Some(AgentState::Stopped),
            Self::Empty => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::WaitingChoice => "choice",
            Self::WaitingInput => "input",
            Self::Working => "working",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Stopped => "stopped",
            Self::Empty => "untracked",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct StateCounts {
    starting: usize,
    working: usize,
    idle: usize,
    waiting_input: usize,
    waiting_choice: usize,
    error: usize,
    stopped: usize,
}

impl StateCounts {
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

    fn total(self) -> usize {
        self.starting
            + self.working
            + self.idle
            + self.waiting_input
            + self.waiting_choice
            + self.error
            + self.stopped
    }

    fn status(self) -> CardStatus {
        if self.total() == 0 {
            return CardStatus::Empty;
        }
        if self.error > 0 {
            CardStatus::Error
        } else if self.waiting_choice > 0 {
            CardStatus::WaitingChoice
        } else if self.waiting_input > 0 {
            CardStatus::WaitingInput
        } else if self.working > 0 {
            CardStatus::Working
        } else if self.starting > 0 {
            CardStatus::Starting
        } else if self.idle > 0 {
            CardStatus::Idle
        } else {
            CardStatus::Stopped
        }
    }

    fn compact(self) -> String {
        let mut parts = Vec::new();
        if self.error > 0 {
            parts.push(format!("{} err", self.error));
        }
        if self.waiting_choice > 0 {
            parts.push(format!("{} choice", self.waiting_choice));
        }
        if self.waiting_input > 0 {
            parts.push(format!("{} input", self.waiting_input));
        }
        if self.working > 0 {
            parts.push(format!("{} work", self.working));
        }
        if self.starting > 0 {
            parts.push(format!("{} start", self.starting));
        }
        if self.idle > 0 {
            parts.push(format!("{} idle", self.idle));
        }
        if self.stopped > 0 {
            parts.push(format!("{} stop", self.stopped));
        }
        if parts.is_empty() {
            "no agents".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

#[derive(Debug, Clone)]
struct DashboardApp {
    data: DashboardData,
    selected: usize,
    columns: usize,
    target_indices: BTreeMap<String, usize>,
    inspector_open: bool,
    overlay: Overlay,
    hint: Option<FooterHint>,
    confirm: Option<ConfirmPopup>,
    composer: Option<PromptComposer>,
    capture: CaptureCache,
    capture_scroll: usize,
    theme: DashboardTheme,
}

impl DashboardApp {
    fn new(data: DashboardData, theme: WatchTheme) -> Self {
        Self {
            data,
            selected: 0,
            columns: 1,
            target_indices: BTreeMap::new(),
            inspector_open: true,
            overlay: Overlay::None,
            hint: None,
            confirm: None,
            composer: None,
            capture: CaptureCache::default(),
            capture_scroll: 0,
            theme: dashboard_theme(theme),
        }
    }

    fn replace_data(&mut self, data: DashboardData) {
        let selected_key = self.selected_card().map(|card| card.key.clone());
        self.data = data;
        let next_selected = selected_key
            .and_then(|key| self.data.cards.iter().position(|card| card.key == key))
            .unwrap_or(self.selected);
        if next_selected != self.selected {
            self.reset_capture_view();
        }
        self.selected = next_selected;
        self.clamp_selected();
        self.clamp_selected_target();
    }

    fn selected_card(&self) -> Option<&SessionCard> {
        self.data.cards.get(self.selected)
    }

    fn action_target_for(&self, card: &SessionCard) -> Option<ActionTarget> {
        let targets = card.action_targets();
        if targets.is_empty() {
            return None;
        }
        let idx = self
            .target_indices
            .get(&card.key)
            .copied()
            .unwrap_or(0)
            .min(targets.len() - 1);
        targets.get(idx).cloned()
    }

    fn selected_action_target(&self) -> Option<ActionTarget> {
        self.selected_card()
            .and_then(|card| self.action_target_for(card))
    }

    fn selected_target_position(&self) -> Option<(usize, usize)> {
        let card = self.selected_card()?;
        let count = card.action_targets().len();
        if count == 0 {
            return None;
        }
        let idx = self
            .target_indices
            .get(&card.key)
            .copied()
            .unwrap_or(0)
            .min(count - 1);
        Some((idx + 1, count))
    }

    fn clamp_selected(&mut self) {
        if self.data.cards.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.data.cards.len() {
            self.selected = self.data.cards.len() - 1;
        }
    }

    fn clamp_selected_target(&mut self) {
        let Some(card) = self.selected_card() else {
            return;
        };
        let count = card.action_targets().len();
        if count == 0 {
            return;
        }
        let key = card.key.clone();
        let current = self.target_indices.get(&key).copied().unwrap_or(0);
        if current >= count {
            self.target_indices.insert(key, count - 1);
            self.reset_capture_view();
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.data.cards.is_empty() {
            return;
        }
        let max = self.data.cards.len() - 1;
        let next = self.selected.saturating_add_signed(delta).min(max);
        if next != self.selected {
            self.selected = next;
            self.reset_capture_view();
            self.clamp_selected_target();
        }
    }

    fn move_next(&mut self) {
        self.move_by(1);
    }

    fn move_prev(&mut self) {
        self.move_by(-1);
    }

    fn move_down(&mut self) {
        self.move_by(isize::try_from(self.columns.max(1)).unwrap_or(1));
    }

    fn move_up(&mut self) {
        self.move_by(-isize::try_from(self.columns.max(1)).unwrap_or(1));
    }

    fn set_hint(&mut self, message: impl Into<String>, level: HintLevel) {
        self.hint = Some(FooterHint {
            message: message.into(),
            level,
            set_at: Instant::now(),
        });
    }

    fn cycle_target(&mut self, delta: isize) {
        let Some(card) = self.selected_card() else {
            self.set_hint("no session selected", HintLevel::Err);
            return;
        };
        let targets = card.action_targets();
        let count = targets.len();
        if count <= 1 {
            self.set_hint("selected session has no alternate target", HintLevel::Info);
            return;
        }
        let key = card.key.clone();
        let current = self
            .target_indices
            .get(&key)
            .copied()
            .unwrap_or(0)
            .min(count - 1);
        let count_i = isize::try_from(count).unwrap_or(isize::MAX);
        let current_i = isize::try_from(current).unwrap_or(0);
        let next_i = (current_i + delta).rem_euclid(count_i);
        let next = usize::try_from(next_i).unwrap_or(0);
        let label = targets[next].label();
        self.target_indices.insert(key, next);
        self.reset_capture_view();
        self.set_hint(format!("target {label}"), HintLevel::Info);
    }

    fn scroll_capture(&mut self, delta: isize) {
        if delta < 0 {
            self.capture_scroll = self.capture_scroll.saturating_add(delta.unsigned_abs());
        } else {
            self.capture_scroll = self
                .capture_scroll
                .saturating_sub(usize::try_from(delta).unwrap_or(0));
        }
    }

    fn reset_capture_view(&mut self) {
        self.capture_scroll = 0;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Overlay {
    #[default]
    None,
    Help,
    Notes,
    CaptureFullscreen,
}

#[derive(Debug, Clone)]
struct FooterHint {
    message: String,
    level: HintLevel,
    set_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintLevel {
    Ok,
    Err,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfirmPopup {
    message: String,
    on_confirm: PendingAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingAction {
    Quick(QuickAction),
    PtyPrompt { session_id: String, text: String },
    PtyCtrlC(String),
    TerminatePty(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptComposer {
    target: PromptTarget,
    label: String,
    input: String,
    cursor: usize,
}

impl PromptComposer {
    fn new(target: PromptTarget, label: String) -> Self {
        Self {
            target,
            label,
            input: String::new(),
            cursor: 0,
        }
    }

    fn insert(&mut self, c: char) {
        let idx = char_to_byte_idx(&self.input, self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = char_to_byte_idx(&self.input, self.cursor - 1);
        let end = char_to_byte_idx(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let start = char_to_byte_idx(&self.input, self.cursor);
        let end = char_to_byte_idx(&self.input, self.cursor + 1);
        self.input.replace_range(start..end, "");
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptTarget {
    Pane(String),
    PtySession(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CaptureTarget {
    Pane(String),
    PtySession(String),
}

#[derive(Debug, Clone, Default)]
struct CaptureCache {
    target: Option<CaptureTarget>,
    text: Option<String>,
    message: Option<String>,
    fetched_at: Option<Instant>,
}

impl CaptureCache {
    fn is_fresh_for(&self, target: &CaptureTarget) -> bool {
        self.target.as_ref() == Some(target)
            && self
                .fetched_at
                .is_some_and(|at| at.elapsed() < CAPTURE_INTERVAL)
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
            let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
            let _ = terminal.show_cursor();
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enabling raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)
        .map_err(|e| {
            let _ = disable_raw_mode();
            e
        })
        .context("entering alternate terminal screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
        .inspect_err(|_| {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen);
            let _ = disable_raw_mode();
        })
        .context("initializing terminal")
}

pub async fn run(client: &Client, cfg: &Config, args: Args) -> Result<Option<OpenTarget>> {
    let initial = load_dashboard_data(client, cfg, &args).await?;
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    let theme = args.theme.map_or(cfg.ui.theme, WatchTheme::from);
    let mut app = DashboardApp::new(initial, theme);
    let mut last_refresh = Instant::now();
    let mut refresh_task: Option<tokio::task::JoinHandle<Result<DashboardData>>> = None;

    refresh_capture(client, &mut app).await;

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

            match handle_key(&mut app, key) {
                UiAction::None => {}
                UiAction::Quit => break,
                UiAction::Refresh => {
                    if refresh_task.is_some() {
                        app.set_hint("refresh already running", HintLevel::Info);
                    } else {
                        refresh_task = Some(spawn_refresh(client, cfg, &args));
                        last_refresh = Instant::now();
                        app.set_hint("refreshing", HintLevel::Info);
                    }
                }
                UiAction::Open(target) => {
                    if let Some(task) = refresh_task.take() {
                        task.abort();
                    }
                    return Ok(Some(target));
                }
                UiAction::Run(action) => {
                    let outcome = run_pending_action(client, action).await;
                    apply_outcome(&mut app, outcome);
                    refresh_capture(client, &mut app).await;
                    last_refresh = Instant::now();
                }
            }
        }

        if refresh_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            let task = refresh_task.take().expect("checked above");
            match task.await {
                Ok(Ok(data)) => {
                    app.replace_data(data);
                    app.set_hint("refreshed", HintLevel::Ok);
                }
                Ok(Err(e)) => app.set_hint(format!("refresh failed: {e}"), HintLevel::Err),
                Err(e) => app.set_hint(format!("refresh task failed: {e}"), HintLevel::Err),
            }
            refresh_capture(client, &mut app).await;
            last_refresh = Instant::now();
        } else if last_refresh.elapsed() >= REFRESH_INTERVAL && refresh_task.is_none() {
            refresh_task = Some(spawn_refresh(client, cfg, &args));
            last_refresh = Instant::now();
        } else {
            refresh_capture(client, &mut app).await;
        }
    }

    if let Some(task) = refresh_task.take() {
        task.abort();
    }

    Ok(None)
}

fn spawn_refresh(
    client: &Client,
    cfg: &Config,
    args: &Args,
) -> tokio::task::JoinHandle<Result<DashboardData>> {
    let client = client.clone();
    let cfg = cfg.clone();
    let args = args.clone();
    tokio::spawn(async move { load_dashboard_data(&client, &cfg, &args).await })
}

async fn load_dashboard_data(client: &Client, cfg: &Config, args: &Args) -> Result<DashboardData> {
    let now = OffsetDateTime::now_utc();
    let agents = client
        .snapshot()
        .await
        .context("querying daemon agent snapshot")?;

    let backend = muxa::default_backend();
    let host = backend.kind();
    let panes = backend.list_panes();
    let mut notes = Vec::new();
    let sessions = match client.list_sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            notes.push(format!("terminal session list unavailable: {e}"));
            Vec::new()
        }
    };
    let session_activities = load_session_activities(cfg).await;
    let active_stats =
        match stats::session_active_stats(client, cfg, &args.since, &ScopeExclusions::default())
            .await
        {
            Ok(stats) => stats,
            Err(e) => {
                notes.push(format!("ACT/WACT unavailable: {e}"));
                SessionActiveStats::default()
            }
        };

    Ok(build_dashboard_data(
        now,
        agents,
        panes,
        sessions,
        session_activities,
        active_stats,
        args.include_paneless,
        args.sort,
        host,
        notes,
    ))
}

async fn load_session_activities(cfg: &Config) -> Vec<SessionActivity> {
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

#[allow(clippy::too_many_arguments)]
fn build_dashboard_data(
    now: OffsetDateTime,
    agents: Vec<Agent>,
    panes: Vec<PaneInfo>,
    sessions: Vec<SessionRef>,
    session_activities: Vec<SessionActivity>,
    active_stats: SessionActiveStats,
    include_paneless: bool,
    sort: DashboardSort,
    host: HostKind,
    notes: Vec<String>,
) -> DashboardData {
    let pane_by_id = panes
        .iter()
        .map(|pane| (pane.pane_id.clone(), pane.clone()))
        .collect::<HashMap<_, _>>();
    let pty_by_id = sessions
        .iter()
        .map(|session| (session.id.clone(), session.clone()))
        .collect::<HashMap<_, _>>();
    let activity_by_name = session_activities
        .into_iter()
        .map(|activity| (activity.name.clone(), activity))
        .collect::<HashMap<_, _>>();
    let mut builders = BTreeMap::<String, CardBuilder>::new();

    for pane in &panes {
        let card_host = match host {
            HostKind::Tmux => CardHost::Tmux,
            HostKind::Zellij => CardHost::Zellij,
        };
        let key = format!("{}:{}", card_host.label(), pane.session);
        let builder = builders
            .entry(key.clone())
            .or_insert_with(|| CardBuilder::new(key, pane.session.clone(), card_host));
        builder.pane_ids.insert(pane.pane_id.clone());
    }

    for session in &sessions {
        if session.backend != SessionBackendKind::Pty || (session.exited && !include_paneless) {
            continue;
        }
        let label = session
            .display_name
            .clone()
            .unwrap_or_else(|| session.id.clone());
        let key = format!("pty:{}", session.id);
        let builder = builders
            .entry(key.clone())
            .or_insert_with(|| CardBuilder::new(key, label, CardHost::Pty));
        builder.pty_session_id = Some(session.id.clone());
        builder.cwd = builder.cwd.clone().or_else(|| session.cwd.clone());
    }

    for agent in agents {
        let identity = card_identity(&agent, &pane_by_id, &pty_by_id, host);
        if !include_paneless
            && matches!(identity.host, CardHost::Agent)
            && agent.pane.is_none()
            && agent.surface.is_none()
        {
            continue;
        }

        let builder = builders
            .entry(identity.key.clone())
            .or_insert_with(|| CardBuilder::new(identity.key, identity.label, identity.host));
        if let Some(pty_session_id) = identity.pty_session_id {
            builder.pty_session_id = Some(pty_session_id);
        }
        if let Some(pane) = agent.pane.as_ref() {
            builder.pane_ids.insert(pane.clone());
        }
        builder.cwd = builder.cwd.clone().or_else(|| agent.cwd.clone());
        builder.agents.push(agent);
    }

    let mut cards = builders
        .into_values()
        .map(|builder| {
            finalize_card(
                builder,
                now,
                &pane_by_id,
                &activity_by_name,
                &active_stats.by_session,
            )
        })
        .collect::<Vec<_>>();
    sort_cards(&mut cards, sort);

    let totals = DashboardTotals {
        sessions: cards.len(),
        tracked_agents: cards.iter().map(|card| card.agents.len()).sum(),
        attention: cards
            .iter()
            .filter(|card| {
                matches!(
                    card.status,
                    CardStatus::Error | CardStatus::WaitingChoice | CardStatus::WaitingInput
                )
            })
            .count(),
        working: cards
            .iter()
            .filter(|card| card.status == CardStatus::Working)
            .count(),
        active: active_stats.totals,
    };

    DashboardData {
        generated_at: now,
        cards,
        totals,
        notes,
    }
}

#[derive(Debug, Clone)]
struct CardBuilder {
    key: String,
    label: String,
    host: CardHost,
    pane_ids: BTreeSet<String>,
    pty_session_id: Option<String>,
    cwd: Option<String>,
    agents: Vec<Agent>,
}

impl CardBuilder {
    fn new(key: String, label: String, host: CardHost) -> Self {
        Self {
            key,
            label,
            host,
            pane_ids: BTreeSet::new(),
            pty_session_id: None,
            cwd: None,
            agents: Vec::new(),
        }
    }
}

struct CardIdentity {
    key: String,
    label: String,
    host: CardHost,
    pty_session_id: Option<String>,
}

fn card_identity(
    agent: &Agent,
    pane_by_id: &HashMap<String, PaneInfo>,
    pty_by_id: &HashMap<String, SessionRef>,
    host: HostKind,
) -> CardIdentity {
    if let Some(surface) = agent.surface.as_ref() {
        if surface.kind == SurfaceKind::Pty {
            let label = pty_by_id
                .get(&surface.id)
                .and_then(|session| session.display_name.clone())
                .unwrap_or_else(|| surface.id.clone());
            return CardIdentity {
                key: format!("pty:{}", surface.id),
                label,
                host: CardHost::Pty,
                pty_session_id: Some(surface.id.clone()),
            };
        }
    }

    if let Some(pane) = agent.pane.as_ref() {
        if let Some(info) = pane_by_id.get(pane) {
            let card_host = match host {
                HostKind::Tmux => CardHost::Tmux,
                HostKind::Zellij => CardHost::Zellij,
            };
            return CardIdentity {
                key: format!("{}:{}", card_host.label(), info.session),
                label: info.session.clone(),
                host: card_host,
                pty_session_id: None,
            };
        }
        return CardIdentity {
            key: format!("pane:{pane}"),
            label: pane.clone(),
            host: CardHost::Pane,
            pty_session_id: None,
        };
    }

    CardIdentity {
        key: format!("agent:{}", agent.session_id),
        label: agent.session_id.clone(),
        host: CardHost::Agent,
        pty_session_id: None,
    }
}

fn finalize_card(
    builder: CardBuilder,
    now: OffsetDateTime,
    pane_by_id: &HashMap<String, PaneInfo>,
    activity_by_name: &HashMap<String, SessionActivity>,
    active_by_session: &BTreeMap<String, ActiveDuration>,
) -> SessionCard {
    let mut counts = StateCounts::default();
    let mut kinds = Vec::new();
    for agent in &builder.agents {
        counts.add(agent.state);
        if !kinds.contains(&agent.kind) {
            kinds.push(agent.kind);
        }
    }
    let status = counts.status();
    let active = active_for_card(&builder, active_by_session);
    let pane_ids = builder.pane_ids.into_iter().collect::<Vec<_>>();
    let primary_pane = choose_primary_pane(&builder.agents, &pane_ids);
    let pane_labels = pane_ids
        .iter()
        .map(|pane| pane_label(pane, pane_by_id))
        .collect::<Vec<_>>();
    let latest_agent = builder
        .agents
        .iter()
        .max_by_key(|agent| agent.last_activity_at);
    let last_activity_at = latest_agent
        .map(|agent| agent.last_activity_at)
        .or_else(|| activity_by_name.get(&builder.label).map(|a| a.last_seen_at));
    let last_prompt = latest_agent.and_then(|agent| agent.last_prompt.clone());
    let last_response = latest_agent.and_then(|agent| agent.last_response.clone());
    let last_notification = latest_agent.and_then(|agent| agent.last_notification.clone());
    let model = latest_agent.and_then(|agent| agent.model.clone());
    let fallback_cwd = latest_agent.and_then(|agent| agent.cwd.clone());
    let session_activity = activity_by_name.get(&builder.label);
    let foreground_secs = session_activity.map(|activity| activity.effective_total_secs(now));
    let cost_usd = sum_cost(&builder.agents);
    let context_used_pct = builder
        .agents
        .iter()
        .filter_map(|agent| agent.context_used_pct)
        .max_by(f32::total_cmp);

    SessionCard {
        key: builder.key,
        label: builder.label,
        host: builder.host,
        pane_ids,
        pane_labels,
        primary_pane,
        pty_session_id: builder.pty_session_id,
        cwd: builder.cwd.or(fallback_cwd),
        agents: builder.agents,
        status,
        counts,
        last_activity_at,
        active,
        foreground_secs,
        foreground_attached: session_activity.is_some_and(SessionActivity::is_attached),
        last_prompt,
        last_response,
        last_notification,
        model,
        cost_usd,
        context_used_pct,
        kinds,
    }
}

fn choose_primary_pane(agents: &[Agent], pane_ids: &[String]) -> Option<String> {
    agents
        .iter()
        .filter_map(|agent| agent.pane.as_ref().map(|pane| (agent, pane)))
        .max_by(|(a, _), (b, _)| {
            state_rank(a.state)
                .cmp(&state_rank(b.state))
                .then_with(|| a.last_activity_at.cmp(&b.last_activity_at))
        })
        .map(|(_, pane)| pane.clone())
        .or_else(|| pane_ids.first().cloned())
}

fn state_rank(state: AgentState) -> u8 {
    match state {
        AgentState::Error => 7,
        AgentState::WaitingChoice => 6,
        AgentState::WaitingInput => 5,
        AgentState::Working => 4,
        AgentState::Starting => 3,
        AgentState::Idle => 2,
        AgentState::Stopped => 1,
    }
}

fn pane_label(pane: &str, pane_by_id: &HashMap<String, PaneInfo>) -> String {
    pane_by_id.get(pane).map_or_else(
        || pane.to_string(),
        |info| format!("{}:{}.{}", info.session, info.window_index, info.pane_index),
    )
}

fn active_for_card(
    builder: &CardBuilder,
    active_by_session: &BTreeMap<String, ActiveDuration>,
) -> ActiveDuration {
    if let Some(row) = active_by_session.get(&builder.label) {
        return *row;
    }
    if let Some(session_id) = builder.pty_session_id.as_ref() {
        if let Some(row) = active_by_session.get(session_id) {
            return *row;
        }
    }
    let mut out = ActiveDuration::default();
    let mut seen = BTreeSet::new();
    for agent in &builder.agents {
        if seen.insert(agent.session_id.clone()) {
            if let Some(row) = active_by_session.get(&agent.session_id) {
                out.active_secs = out.active_secs.saturating_add(row.active_secs);
                out.work_active_secs = out.work_active_secs.saturating_add(row.work_active_secs);
            }
        }
    }
    out
}

fn sum_cost(agents: &[Agent]) -> Option<f64> {
    let total = agents
        .iter()
        .filter_map(|agent| agent.cost_usd)
        .sum::<f64>();
    (total > 0.0).then_some(total)
}

fn sort_cards(cards: &mut [SessionCard], sort: DashboardSort) {
    cards.sort_by(|a, b| {
        let ordering = match sort {
            DashboardSort::Attention => b
                .attention_score()
                .cmp(&a.attention_score())
                .then_with(|| b.last_activity_at.cmp(&a.last_activity_at))
                .then_with(|| b.active.active_secs.cmp(&a.active.active_secs)),
            DashboardSort::Activity => b
                .last_activity_at
                .cmp(&a.last_activity_at)
                .then_with(|| b.attention_score().cmp(&a.attention_score())),
            DashboardSort::Active => b
                .active
                .active_secs
                .cmp(&a.active.active_secs)
                .then_with(|| b.last_activity_at.cmp(&a.last_activity_at)),
            DashboardSort::Name => a.label.cmp(&b.label),
        };
        ordering.then_with(|| a.label.cmp(&b.label))
    });
}

#[derive(Debug, Clone)]
enum UiAction {
    None,
    Quit,
    Refresh,
    Open(OpenTarget),
    Run(PendingAction),
}

fn handle_key(app: &mut DashboardApp, key: KeyEvent) -> UiAction {
    if app.overlay != Overlay::None {
        return handle_overlay_key(app, key);
    }

    if app.confirm.is_some() {
        return handle_confirm_key(app, key);
    }

    if app.composer.is_some() {
        return handle_composer_key(app, key);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return UiAction::Quit;
    }

    handle_normal_key(app, key)
}

fn handle_overlay_key(app: &mut DashboardApp, key: KeyEvent) -> UiAction {
    match app.overlay {
        Overlay::Help => match key.code {
            KeyCode::Esc | KeyCode::Char('?' | 'q') => app.overlay = Overlay::None,
            _ => {}
        },
        Overlay::Notes => match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'q') => app.overlay = Overlay::None,
            _ => {}
        },
        Overlay::CaptureFullscreen => match key.code {
            KeyCode::Esc | KeyCode::Char('f') => app.overlay = Overlay::None,
            KeyCode::PageUp => app.scroll_capture(-5),
            KeyCode::PageDown => app.scroll_capture(5),
            KeyCode::Up | KeyCode::Char('k') => app.scroll_capture(-1),
            KeyCode::Down | KeyCode::Char('j') => app.scroll_capture(1),
            KeyCode::End | KeyCode::Char('G') => app.reset_capture_view(),
            _ => {}
        },
        Overlay::None => {}
    }
    UiAction::None
}

fn handle_normal_key(app: &mut DashboardApp, key: KeyEvent) -> UiAction {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => UiAction::Quit,
        KeyCode::Char('?') => {
            app.overlay = Overlay::Help;
            UiAction::None
        }
        KeyCode::Char('n') => {
            if app.data.notes.is_empty() {
                app.set_hint("no dashboard notes", HintLevel::Info);
            } else {
                app.overlay = Overlay::Notes;
            }
            UiAction::None
        }
        KeyCode::Char('r') => UiAction::Refresh,
        KeyCode::Tab | KeyCode::Char(']') => {
            app.cycle_target(1);
            UiAction::None
        }
        KeyCode::BackTab | KeyCode::Char('[') => {
            app.cycle_target(-1);
            UiAction::None
        }
        KeyCode::PageUp => {
            app.scroll_capture(-5);
            UiAction::None
        }
        KeyCode::PageDown => {
            app.scroll_capture(5);
            UiAction::None
        }
        KeyCode::End | KeyCode::Char('G') => {
            app.reset_capture_view();
            UiAction::None
        }
        KeyCode::Char('f') => {
            app.overlay = Overlay::CaptureFullscreen;
            UiAction::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_down();
            UiAction::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_up();
            UiAction::None
        }
        KeyCode::Right | KeyCode::Char('l') => {
            app.move_next();
            UiAction::None
        }
        KeyCode::Left | KeyCode::Char('h') => {
            app.move_prev();
            UiAction::None
        }
        KeyCode::Enter => {
            app.inspector_open = !app.inspector_open;
            UiAction::None
        }
        KeyCode::Char('p') => open_composer(app),
        KeyCode::Char('o') => app.selected_action_target().map_or_else(
            || {
                app.set_hint("no pane or PTY session to open", HintLevel::Err);
                UiAction::None
            },
            |target| UiAction::Open(target.open_target()),
        ),
        KeyCode::Char('c') => copy_selected_prompt(app),
        KeyCode::Char('R') => confirm_abort(app),
        KeyCode::Char('K') => confirm_kill(app),
        _ => UiAction::None,
    }
}

fn handle_confirm_key(app: &mut DashboardApp, key: KeyEvent) -> UiAction {
    match key.code {
        KeyCode::Char('y' | 'Y') => {
            let action = app
                .confirm
                .take()
                .expect("confirm checked above")
                .on_confirm;
            UiAction::Run(action)
        }
        KeyCode::Esc | KeyCode::Char('n' | 'N' | 'q') | KeyCode::Enter => {
            app.confirm = None;
            app.set_hint("cancelled", HintLevel::Info);
            UiAction::None
        }
        _ => UiAction::None,
    }
}

fn handle_composer_key(app: &mut DashboardApp, key: KeyEvent) -> UiAction {
    let composer = app.composer.as_mut().expect("composer checked above");
    match key.code {
        KeyCode::Esc => {
            app.composer = None;
            UiAction::None
        }
        KeyCode::Enter => {
            let composer = app.composer.take().expect("composer checked above");
            let text = composer.input.trim().to_string();
            if text.is_empty() {
                return UiAction::None;
            }
            UiAction::Run(match composer.target {
                PromptTarget::Pane(pane_id) => {
                    PendingAction::Quick(QuickAction::SendPrompt { pane_id, text })
                }
                PromptTarget::PtySession(session_id) => {
                    PendingAction::PtyPrompt { session_id, text }
                }
            })
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            composer.insert(c);
            UiAction::None
        }
        KeyCode::Backspace => {
            composer.backspace();
            UiAction::None
        }
        KeyCode::Delete => {
            composer.delete();
            UiAction::None
        }
        KeyCode::Left => {
            composer.move_left();
            UiAction::None
        }
        KeyCode::Right => {
            composer.move_right();
            UiAction::None
        }
        KeyCode::Home => {
            composer.move_home();
            UiAction::None
        }
        KeyCode::End => {
            composer.move_end();
            UiAction::None
        }
        _ => UiAction::None,
    }
}

fn selected_target_context(app: &DashboardApp) -> Option<(CardHost, String, ActionTarget)> {
    let card = app.selected_card()?;
    let target = app.action_target_for(card)?;
    Some((card.host, card.label.clone(), target))
}

fn pane_write_supported(host: CardHost) -> bool {
    !matches!(host, CardHost::Zellij)
}

fn unsupported_pane_action(host: CardHost, action: &str) -> Option<String> {
    match host {
        CardHost::Zellij => Some(format!("zellij pane {action} is not supported yet")),
        _ => None,
    }
}

fn open_composer(app: &mut DashboardApp) -> UiAction {
    let Some((host, label, target)) = selected_target_context(app) else {
        app.set_hint("no session selected", HintLevel::Err);
        return UiAction::None;
    };
    if matches!(target, ActionTarget::Pane(_)) && !pane_write_supported(host) {
        let message = unsupported_pane_action(host, "prompting")
            .unwrap_or_else(|| "prompt unsupported".into());
        app.set_hint(message, HintLevel::Err);
        return UiAction::None;
    }
    app.composer = Some(PromptComposer::new(
        target.prompt_target(),
        format!("{label} · {}", target.label()),
    ));
    UiAction::None
}

fn copy_selected_prompt(app: &mut DashboardApp) -> UiAction {
    let Some(prompt) = app
        .selected_card()
        .and_then(|card| card.last_prompt.clone())
        .filter(|prompt| !prompt.is_empty())
    else {
        app.set_hint("selected session has no prompt to copy", HintLevel::Err);
        return UiAction::None;
    };
    UiAction::Run(PendingAction::Quick(QuickAction::CopyPrompt(prompt)))
}

fn confirm_abort(app: &mut DashboardApp) -> UiAction {
    let Some((host, label, target)) = selected_target_context(app) else {
        return UiAction::None;
    };
    if matches!(target, ActionTarget::Pane(_)) && !pane_write_supported(host) {
        let message =
            unsupported_pane_action(host, "abort").unwrap_or_else(|| "abort unsupported".into());
        app.set_hint(message, HintLevel::Err);
        return UiAction::None;
    }
    let action = match target.clone() {
        ActionTarget::Pane(pane_id) => PendingAction::Quick(QuickAction::AbortTurn(pane_id)),
        ActionTarget::PtySession(session_id) => PendingAction::PtyCtrlC(session_id),
    };
    app.confirm = Some(ConfirmPopup {
        message: format!("Abort current turn on {} ({label})?", target.label()),
        on_confirm: action,
    });
    UiAction::None
}

fn confirm_kill(app: &mut DashboardApp) -> UiAction {
    let Some((host, label, target)) = selected_target_context(app) else {
        return UiAction::None;
    };
    if matches!(target, ActionTarget::Pane(_)) && !pane_write_supported(host) {
        let message = unsupported_pane_action(host, "termination")
            .unwrap_or_else(|| "terminate unsupported".into());
        app.set_hint(message, HintLevel::Err);
        return UiAction::None;
    }
    let action = match target.clone() {
        ActionTarget::Pane(pane_id) => PendingAction::Quick(QuickAction::KillPane(pane_id)),
        ActionTarget::PtySession(session_id) => PendingAction::TerminatePty(session_id),
    };
    app.confirm = Some(ConfirmPopup {
        message: format!("Terminate {} ({label})?", target.label()),
        on_confirm: action,
    });
    UiAction::None
}

async fn run_pending_action(client: &Client, action: PendingAction) -> ActionOutcome {
    match action {
        PendingAction::Quick(action) => {
            let mut fx = RealEffects;
            watch::dispatch_quick_action(action, &mut fx)
        }
        PendingAction::PtyPrompt { session_id, text } => {
            match client
                .write_session(&session_id, &format!("{text}\r"))
                .await
            {
                Ok(()) => ActionOutcome::Ok(format!("sent prompt to {session_id}")),
                Err(e) => ActionOutcome::Err(format!("send failed: {e}")),
            }
        }
        PendingAction::PtyCtrlC(session_id) => {
            match client.write_session(&session_id, "\u{3}").await {
                Ok(()) => ActionOutcome::Ok(format!("sent Ctrl-C to {session_id}")),
                Err(e) => ActionOutcome::Err(format!("abort failed: {e}")),
            }
        }
        PendingAction::TerminatePty(session_id) => {
            match client.terminate_session(&session_id).await {
                Ok(()) => ActionOutcome::Ok(format!("terminated {session_id}")),
                Err(e) => ActionOutcome::Err(format!("terminate failed: {e}")),
            }
        }
    }
}

fn apply_outcome(app: &mut DashboardApp, outcome: ActionOutcome) {
    match outcome {
        ActionOutcome::Ok(message) => app.set_hint(message, HintLevel::Ok),
        ActionOutcome::Err(message) => app.set_hint(message, HintLevel::Err),
        ActionOutcome::HelpToggled => {
            app.overlay = if app.overlay == Overlay::Help {
                Overlay::None
            } else {
                Overlay::Help
            };
        }
    }
}

async fn refresh_capture(client: &Client, app: &mut DashboardApp) {
    let Some((host, target)) = app.selected_card().and_then(|card| {
        app.action_target_for(card)
            .map(|target| (card.host, target.capture_target()))
    }) else {
        app.capture = CaptureCache::default();
        return;
    };
    if app.capture.is_fresh_for(&target) {
        return;
    }

    if matches!((&target, host), (CaptureTarget::Pane(_), CardHost::Zellij)) {
        app.capture = CaptureCache {
            target: Some(target),
            text: None,
            message: Some("capture unsupported for zellij panes".into()),
            fetched_at: Some(Instant::now()),
        };
        return;
    }

    let text = match target.clone() {
        CaptureTarget::Pane(pane_id) => {
            tokio::task::spawn_blocking(move || muxa::default_backend().capture_pane(&pane_id))
                .await
                .ok()
                .flatten()
        }
        CaptureTarget::PtySession(session_id) => client
            .capture_session(&session_id)
            .await
            .ok()
            .map(|snapshot| snapshot.lines.join("\n")),
    };

    app.capture = CaptureCache {
        target: Some(target),
        text,
        message: None,
        fetched_at: Some(Instant::now()),
    };
}

#[allow(clippy::too_many_lines)]
fn render(f: &mut Frame, app: &mut DashboardApp) {
    let area = f.area();
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);
    render_header(f, shell[0], app);

    if app.inspector_open && shell[1].width >= 96 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(DESKTOP_SESSION_PERCENT),
                Constraint::Percentage(DESKTOP_INSPECTOR_PERCENT),
            ])
            .split(shell[1]);
        render_cards(f, columns[0], app);
        render_inspector(f, columns[1], app);
    } else if app.inspector_open && shell[1].height >= 14 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(shell[1]);
        render_cards(f, rows[0], app);
        render_inspector(f, rows[1], app);
    } else {
        render_cards(f, shell[1], app);
    }

    render_footer(f, shell[2], app);

    if app.overlay == Overlay::CaptureFullscreen {
        if let Some(card) = app.selected_card() {
            let popup = centered_rect_by_size(
                area.width.saturating_sub(4).max(20),
                area.height.saturating_sub(4).max(8),
                area,
            );
            f.render_widget(Clear, popup);
            render_capture_panel(f, popup, card, app);
        }
    }
    if app.overlay == Overlay::Notes {
        let popup = centered_rect_by_size(76, 14, area);
        f.render_widget(Clear, popup);
        render_notes(f, popup, app);
    }
    if app.overlay == Overlay::Help {
        let popup = centered_rect_by_size(76, 22, area);
        f.render_widget(Clear, popup);
        render_help(f, popup, app.theme);
    }
    if app.confirm.is_some() {
        let popup = centered_rect_by_size(60, 7, area);
        f.render_widget(Clear, popup);
        render_confirm(f, popup, app);
    }
    if app.composer.is_some() {
        let popup = bottom_prompt_rect(area);
        f.render_widget(Clear, popup);
        render_composer(f, popup, app);
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &DashboardApp) {
    let totals = &app.data.totals;
    let updated = format_clock(app.data.generated_at);
    let mut spans = vec![
        Span::styled(
            format!(" {} muxa dashboard ", icon_session()),
            app.theme.key_style(),
        ),
        Span::raw("  "),
        subtle_pill(format!("{} sessions", totals.sessions), app.theme),
        Span::raw("  "),
        subtle_pill(format!("{} agents", totals.tracked_agents), app.theme),
        Span::raw("  "),
        pill(
            format!("{} attention", totals.attention),
            if totals.attention > 0 {
                Color::Black
            } else {
                app.theme.panel
            },
            if totals.attention > 0 {
                app.theme.warn
            } else {
                app.theme.border
            },
        ),
        Span::raw("  "),
        pill(
            format!("{} working", totals.working),
            Color::Black,
            app.theme.working,
        ),
        Span::raw("  "),
        Span::styled(
            format!(
                "{} ACT {}",
                icon_time(),
                format_duration(totals.active.active_secs)
            ),
            Style::default().fg(app.theme.accent),
        ),
        Span::raw("  "),
        Span::styled(
            format!("WACT {}", format_duration(totals.active.work_active_secs)),
            Style::default().fg(app.theme.ok),
        ),
        Span::raw("  "),
        Span::styled(format!("updated {updated}"), app.theme.dim_style()),
    ];
    if !app.data.notes.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(pill(
            format!("{} notes", app.data.notes.len()),
            Color::Black,
            app.theme.warn,
        ));
    }

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(app.theme.border_style());
    let width = usize::from(area.width);
    f.render_widget(
        Paragraph::new(Line::from(fit_spans(spans, width))).block(block),
        area,
    );
}

fn render_cards(f: &mut Frame, area: Rect, app: &mut DashboardApp) {
    let columns = card_columns(area.width);
    app.columns = columns;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style())
        .border_type(BorderType::Plain)
        .title(Span::styled(" sessions ", app.theme.title_style()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.data.cards.is_empty() {
        let text = Text::from(Line::from(Span::styled(
            "No sessions or tracked agents found.",
            app.theme.dim_style(),
        )));
        f.render_widget(Paragraph::new(text), inner);
        return;
    }

    let row_heights = card_row_heights(inner.height);
    if row_heights.is_empty() {
        return;
    }
    let rows_per_page = row_heights.len();
    let page_size = rows_per_page.saturating_mul(columns).max(1);
    let page_start = app.selected / page_size * page_size;
    let visible_end = (page_start + page_size).min(app.data.cards.len());
    let visible = &app.data.cards[page_start..visible_end];
    let row_constraints = row_heights
        .into_iter()
        .map(Constraint::Length)
        .collect::<Vec<_>>();
    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .spacing(1)
        .constraints(row_constraints)
        .split(inner);

    for row in 0..rows_per_page {
        let col_constraints =
            vec![Constraint::Ratio(1, u32::try_from(columns).unwrap_or(1)); columns];
        let col_rects = Layout::default()
            .direction(Direction::Horizontal)
            .spacing(1)
            .constraints(col_constraints)
            .split(row_rects[row]);
        for (col, rect) in col_rects.iter().enumerate().take(columns) {
            let visible_idx = row * columns + col;
            let card_idx = page_start + visible_idx;
            if visible_idx >= visible.len() {
                continue;
            }
            render_card(
                f,
                *rect,
                &app.data.cards[card_idx],
                card_idx == app.selected,
                app,
            );
        }
    }
}

fn render_card(f: &mut Frame, area: Rect, card: &SessionCard, selected: bool, app: &DashboardApp) {
    let border_style = if selected {
        app.theme.selected_border()
    } else if card.status == CardStatus::Error {
        Style::default().fg(app.theme.error)
    } else {
        app.theme.border_style()
    };
    let status_span = card.status.state().map_or_else(
        || Span::styled("?", app.theme.dim_style()),
        |state| Span::styled(crate::state_icon(state), app.theme.state_style(state)),
    );
    let mut title_spans = Vec::new();
    if selected {
        title_spans.push(pill("FOCUS", app.theme.selected_fg, app.theme.selected));
        title_spans.push(Span::raw(" "));
    }
    title_spans.extend([
        Span::styled(
            icon_session(),
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        status_span,
        Span::raw(" "),
        Span::styled(card_title(card), app.theme.title_style()),
    ]);
    let title = Line::from(title_spans);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .border_type(BorderType::Rounded)
        .title(title)
        .style(app.theme.card_style(selected));
    let inner_width = usize::from(area.width.saturating_sub(2)).max(1);
    let inner_height = usize::from(area.height.saturating_sub(2));
    let action_target = app.action_target_for(card);
    let mut lines = if inner_height <= 3 {
        compact_card_lines(
            card,
            action_target.as_ref(),
            app.data.generated_at,
            inner_width,
            app.theme,
        )
    } else {
        full_card_lines(
            card,
            action_target.as_ref(),
            app.data.generated_at,
            inner_width,
            app.theme,
        )
    };
    lines.truncate(inner_height);

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(app.theme.card_style(selected)),
        area,
    );
}

fn full_card_lines(
    card: &SessionCard,
    action_target: Option<&ActionTarget>,
    now: OffsetDateTime,
    width: usize,
    theme: DashboardTheme,
) -> Vec<Line<'static>> {
    vec![
        card_status_line(card, width, theme),
        card_reason_line(card, action_target, now, width, theme),
        card_activity_bar_line(card, width, theme),
        card_resource_line(card, action_target, now, width, theme),
        card_prompt_line(card, width, theme),
    ]
}

fn compact_card_lines(
    card: &SessionCard,
    action_target: Option<&ActionTarget>,
    now: OffsetDateTime,
    width: usize,
    theme: DashboardTheme,
) -> Vec<Line<'static>> {
    vec![
        card_status_line(card, width, theme),
        card_reason_line(card, action_target, now, width, theme),
        card_activity_bar_line(card, width, theme),
    ]
}

fn card_title(card: &SessionCard) -> String {
    let prefix = match card.host {
        CardHost::Tmux | CardHost::Zellij => "",
        CardHost::Pty => "pty:",
        CardHost::Pane => "pane:",
        CardHost::Agent => "agent:",
    };
    format!("{prefix}{}", card.label)
}

fn card_status_line(card: &SessionCard, width: usize, theme: DashboardTheme) -> Line<'static> {
    let mut spans = vec![
        status_pill(card.status, theme),
        Span::raw(" "),
        subtle_pill(card.host.label(), theme),
        Span::raw(" "),
        Span::styled(
            format!("{} {}", icon_agent(), card.agents.len()),
            Style::default().fg(theme.panel),
        ),
    ];
    if let Some(label) = card.pane_labels.first() {
        spans.extend([
            Span::raw("  "),
            Span::styled(
                format!("{} {}", icon_target(), label),
                Style::default().fg(theme.panel),
            ),
        ]);
    } else if !card.pane_ids.is_empty() {
        spans.extend([
            Span::raw("  "),
            Span::styled(
                format!("{} {}", icon_target(), card.pane_ids.len()),
                Style::default().fg(theme.panel),
            ),
        ]);
    }
    Line::from(fit_spans(spans, width))
}

fn card_reason_line(
    card: &SessionCard,
    action_target: Option<&ActionTarget>,
    now: OffsetDateTime,
    width: usize,
    theme: DashboardTheme,
) -> Line<'static> {
    let Some(agent) = action_target
        .and_then(|target| agent_for_action_target(card, target))
        .or_else(|| primary_agent(card))
    else {
        return Line::from(Span::styled("no tracked agent", theme.dim_style()));
    };
    let age = relative_time(agent.state_entered_at, now);
    let reason = agent
        .last_notification
        .as_deref()
        .or(agent.last_prompt.as_deref())
        .map_or_else(|| agent.state.to_string(), squash_ws);
    let prefix = match agent.state {
        AgentState::Error => "error",
        AgentState::WaitingChoice => "needs choice",
        AgentState::WaitingInput => "needs input",
        AgentState::Working => "working",
        AgentState::Starting => "starting",
        AgentState::Idle => "idle",
        AgentState::Stopped => "stopped",
    };
    Line::from(fit_spans(
        vec![
            Span::styled(icon_activity(), theme.state_style(agent.state)),
            Span::raw(" "),
            Span::styled(format!("{prefix} {age}"), theme.state_style(agent.state)),
            Span::raw("  "),
            Span::styled(
                truncate_width(&reason, width.saturating_sub(18)),
                theme.dim_style(),
            ),
        ],
        width,
    ))
}

fn card_resource_line(
    card: &SessionCard,
    action_target: Option<&ActionTarget>,
    now: OffsetDateTime,
    width: usize,
    theme: DashboardTheme,
) -> Line<'static> {
    let mut parts = Vec::new();
    if let Some(model) = card.model.as_deref() {
        parts.push(model.to_string());
    }
    if let Some(ctx) = card.context_used_pct {
        parts.push(format!("ctx {:.0}%", ctx.clamp(0.0, 100.0)));
    }
    if let Some(cost) = card.cost_usd {
        parts.push(format!("${cost:.2}"));
    }
    if let Some(hint) = action_target
        .and_then(|target| agent_for_action_target(card, target))
        .or_else(|| primary_agent(card))
        .and_then(|agent| rate_limit_hint(agent, now))
    {
        parts.push(hint);
    }
    if !card.kinds.is_empty() {
        parts.push(
            card.kinds
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    let text = if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" · ")
    };
    Line::from(fit_spans(
        vec![
            Span::styled(icon_target(), Style::default().fg(theme.panel)),
            Span::raw(" "),
            Span::styled(
                action_target_label(card, action_target),
                Style::default().fg(theme.panel),
            ),
            Span::raw("  "),
            Span::styled(icon_model(), theme.dim_style()),
            Span::raw(" "),
            Span::raw(text),
        ],
        width,
    ))
}

fn card_activity_bar_line(
    card: &SessionCard,
    width: usize,
    theme: DashboardTheme,
) -> Line<'static> {
    let foreground = card.foreground_secs.map_or_else(
        || "fg -".to_string(),
        |secs| {
            if card.foreground_attached {
                format!("fg {} live", format_duration(secs))
            } else {
                format!("fg {}", format_duration(secs))
            }
        },
    );
    let mut spans = vec![
        Span::styled(icon_time(), Style::default().fg(theme.accent)),
        Span::raw(" "),
        Span::styled(
            format!("ACT {}", format_duration(card.active.active_secs)),
            Style::default().fg(theme.accent),
        ),
        Span::raw(" "),
    ];
    spans.extend(ratio_bar_spans(
        card.active.work_active_secs,
        card.active.active_secs,
        10,
        theme,
    ));
    spans.extend([
        Span::raw(" "),
        Span::styled(
            format!("WACT {}", format_duration(card.active.work_active_secs)),
            Style::default().fg(theme.ok),
        ),
        Span::raw("  "),
        Span::styled(foreground, theme.dim_style()),
    ]);
    Line::from(fit_spans(spans, width))
}

fn card_prompt_line(card: &SessionCard, width: usize, theme: DashboardTheme) -> Line<'static> {
    let body = card
        .last_notification
        .as_deref()
        .or(card.last_prompt.as_deref())
        .unwrap_or("-");
    Line::from(fit_spans(
        vec![
            Span::styled(icon_prompt(), Style::default().fg(theme.warn)),
            Span::raw(" "),
            Span::raw(squash_ws(body)),
        ],
        width,
    ))
}

fn primary_agent(card: &SessionCard) -> Option<&Agent> {
    if let Some(primary_pane) = card.primary_pane.as_deref() {
        if let Some(agent) = card
            .agents
            .iter()
            .find(|agent| agent.pane.as_deref() == Some(primary_pane))
        {
            return Some(agent);
        }
    }

    if let Some(session_id) = card.pty_session_id.as_deref() {
        if let Some(agent) = card.agents.iter().find(|agent| {
            agent.surface.as_ref().is_some_and(|surface| {
                surface.kind == SurfaceKind::Pty && surface.id.as_str() == session_id
            })
        }) {
            return Some(agent);
        }
    }

    card.agents.iter().max_by(|a, b| {
        state_rank(a.state)
            .cmp(&state_rank(b.state))
            .then_with(|| a.last_activity_at.cmp(&b.last_activity_at))
    })
}

fn agent_for_action_target<'a>(card: &'a SessionCard, target: &ActionTarget) -> Option<&'a Agent> {
    card.agents
        .iter()
        .find(|agent| agent_matches_action_target(agent, target))
}

fn agent_matches_action_target(agent: &Agent, target: &ActionTarget) -> bool {
    match target {
        ActionTarget::Pane(pane) => agent.pane.as_deref() == Some(pane.as_str()),
        ActionTarget::PtySession(session_id) => agent.surface.as_ref().is_some_and(|surface| {
            surface.kind == SurfaceKind::Pty && surface.id.as_str() == session_id.as_str()
        }),
    }
}

fn action_target_label(card: &SessionCard, target: Option<&ActionTarget>) -> String {
    if let Some((target, agent)) =
        target.and_then(|target| agent_for_action_target(card, target).map(|agent| (target, agent)))
    {
        return format!("{} {}", agent.kind, target.label());
    }
    if let Some(target) = target {
        return target.label();
    }
    if let Some(agent) = primary_agent(card) {
        return agent_target_label(agent);
    }
    card.capture_target()
        .as_ref()
        .map_or_else(|| "-".to_string(), capture_target_label)
}

fn agent_target_label(agent: &Agent) -> String {
    let target = agent
        .pane
        .as_deref()
        .or_else(|| agent.surface.as_ref().map(|surface| surface.id.as_str()))
        .unwrap_or("-");
    format!("{} {target}", agent.kind)
}

fn rate_limit_hint(agent: &Agent, now: OffsetDateTime) -> Option<String> {
    if is_currently_capped(agent, now) {
        return Some(format!(
            "cap {}",
            format_cap_body(agent.rate_limit_scope, agent.rate_limited_until, now)
        ));
    }

    match (agent.rate_limit_5h_pct, agent.rate_limit_7d_pct) {
        (Some(five), Some(seven)) => {
            if seven > five {
                Some(format!("7d {:.0}%", seven.clamp(0.0, 100.0)))
            } else {
                Some(format!("5h {:.0}%", five.clamp(0.0, 100.0)))
            }
        }
        (Some(pct), None) => Some(format!("5h {:.0}%", pct.clamp(0.0, 100.0))),
        (None, Some(pct)) => Some(format!("7d {:.0}%", pct.clamp(0.0, 100.0))),
        (None, None) => None,
    }
}

fn is_currently_capped(agent: &Agent, now: OffsetDateTime) -> bool {
    agent.rate_limit_scope.is_some() && agent.rate_limited_until.is_none_or(|until| until > now)
}

fn format_cap_body(
    scope: Option<RateLimitScope>,
    until: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> String {
    let scope_prefix = rate_scope_label(scope);
    match until {
        Some(until) => {
            let reset = format_relative_until(until, now);
            if scope_prefix.is_empty() {
                reset
            } else {
                format!("{scope_prefix} {reset}")
            }
        }
        None if scope_prefix.is_empty() => "rate limited".into(),
        None => format!("{scope_prefix} capped"),
    }
}

fn rate_scope_label(scope: Option<RateLimitScope>) -> &'static str {
    match scope {
        Some(RateLimitScope::FiveHour) => "5h",
        Some(RateLimitScope::SevenDay) => "7d",
        Some(RateLimitScope::Unknown) | None => "",
    }
}

fn format_relative_until(until: OffsetDateTime, now: OffsetDateTime) -> String {
    let total_secs = (until - now).whole_seconds();
    if total_secs <= 0 {
        return "now".into();
    }
    let hours = total_secs / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("in {hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("in {minutes}m")
    } else {
        format!("in {seconds}s")
    }
}

fn ratio_bar_spans(
    numerator: u64,
    denominator: u64,
    width: usize,
    theme: DashboardTheme,
) -> Vec<Span<'static>> {
    let width = width.max(1);
    let width_u64 = u64::try_from(width).unwrap_or(u64::MAX);
    let rounded = numerator
        .min(denominator)
        .saturating_mul(width_u64)
        .saturating_add(denominator / 2)
        .checked_div(denominator)
        .unwrap_or(0);
    let filled = usize::try_from(rounded).unwrap_or(width).min(width);
    let empty = width.saturating_sub(filled);
    let (full_cell, empty_cell) = match crate::icon_set() {
        IconSet::Unicode => ("▰", "▱"),
        IconSet::Ascii => ("#", "-"),
    };
    vec![
        Span::styled(full_cell.repeat(filled), Style::default().fg(theme.ok)),
        Span::styled(empty_cell.repeat(empty), theme.dim_style()),
    ]
}

fn render_inspector(f: &mut Frame, area: Rect, app: &DashboardApp) {
    let Some(card) = app.selected_card() else {
        return;
    };
    if area.height >= 22 {
        let roster_height = if card.agents.is_empty() { 0 } else { 8 };
        let constraints = if roster_height == 0 {
            vec![Constraint::Length(6), Constraint::Min(6)]
        } else {
            vec![
                Constraint::Length(6),
                Constraint::Length(roster_height),
                Constraint::Min(6),
            ]
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        render_summary_strip(f, chunks[0], card, app);
        if roster_height == 0 {
            render_capture_panel(f, chunks[1], card, app);
        } else {
            render_agent_roster_panel(f, chunks[1], card, app);
            render_capture_panel(f, chunks[2], card, app);
        }
    } else {
        let detail_height = area.height.saturating_div(2).clamp(4, 8);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(detail_height), Constraint::Min(3)])
            .split(area);
        render_detail_panel(f, chunks[0], card, app);
        render_capture_panel(f, chunks[1], card, app);
    }
}

fn render_summary_strip(f: &mut Frame, area: Rect, card: &SessionCard, app: &DashboardApp) {
    let action_target = app.action_target_for(card);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);
    render_metric_tile(
        f,
        columns[0],
        format!("{} session", icon_session()),
        vec![
            status_pill(card.status, app.theme),
            Span::raw(" "),
            Span::styled(card.label.clone(), app.theme.title_style()),
        ],
        vec![
            format!(
                "{} agents · {} panes",
                card.agents.len(),
                card.pane_ids.len()
            ),
            card.counts.compact(),
        ],
        app.theme,
    );
    render_metric_tile(
        f,
        columns[1],
        format!("{} activity", icon_activity()),
        vec![
            Span::styled(
                format!("ACT {}", format_duration(card.active.active_secs)),
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("WACT {}", format_duration(card.active.work_active_secs)),
                Style::default()
                    .fg(app.theme.ok)
                    .add_modifier(Modifier::BOLD),
            ),
        ],
        vec![
            card.last_activity_at.map_or_else(
                || "last -".to_string(),
                |at| format!("last {}", relative_time(at, app.data.generated_at)),
            ),
            card.foreground_secs.map_or_else(
                || "foreground -".to_string(),
                |secs| format!("foreground {}", format_duration(secs)),
            ),
        ],
        app.theme,
    );
    render_metric_tile(
        f,
        columns[2],
        format!("{} target", icon_target()),
        vec![Span::styled(
            action_target
                .as_ref()
                .map_or_else(|| "-".to_string(), ActionTarget::label),
            Style::default().fg(app.theme.panel),
        )],
        vec![
            card.model
                .as_deref()
                .map_or_else(|| "model -".to_string(), |model| format!("model {model}")),
            card.cwd.as_deref().map_or_else(
                || "cwd -".to_string(),
                |cwd| format!("cwd {}", short_path(cwd)),
            ),
        ],
        app.theme,
    );
}

fn render_metric_tile(
    f: &mut Frame,
    area: Rect,
    title: String,
    headline: Vec<Span<'static>>,
    details: Vec<String>,
    theme: DashboardTheme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(BorderType::Rounded)
        .title(Span::styled(format!(" {title} "), theme.title_style()))
        .style(Style::default().bg(theme.surface_alt));
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let mut lines = vec![Line::from(fit_spans(headline, width))];
    lines.extend(details.into_iter().take(2).map(|detail| {
        Line::from(Span::styled(
            truncate_width(&detail, width),
            theme.dim_style(),
        ))
    }));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::default().bg(theme.surface_alt)),
        area,
    );
}

fn render_agent_roster_panel(f: &mut Frame, area: Rect, card: &SessionCard, app: &DashboardApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style())
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            format!(" {} agents ", icon_agent()),
            app.theme.title_style(),
        ));
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let max_lines = usize::from(area.height.saturating_sub(2));
    let action_target = app.action_target_for(card);
    let mut lines = card
        .agents
        .iter()
        .take(max_lines)
        .map(|agent| {
            agent_roster_line(
                agent,
                app.data.generated_at,
                width,
                app.theme,
                action_target
                    .as_ref()
                    .is_some_and(|target| agent_matches_action_target(agent, target)),
            )
        })
        .collect::<Vec<_>>();
    if card.agents.len() > max_lines && !lines.is_empty() {
        let last = lines.len() - 1;
        lines[last] = Line::from(Span::styled(
            format!("+{} more", card.agents.len() - max_lines + 1),
            app.theme.dim_style(),
        ));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_detail_panel(f: &mut Frame, area: Rect, card: &SessionCard, app: &DashboardApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.selected_border())
        .border_type(BorderType::Plain)
        .title(Span::styled(" inspector ", app.theme.title_style()));
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let max_lines = usize::from(area.height.saturating_sub(2)).max(1);
    let action_target = app.action_target_for(card);
    let mut lines = vec![
        Line::from(truncate_width(
            &format!("{} · {}", card.label, card.status.label()),
            width,
        )),
        Line::from(truncate_width(
            &format!(
                "ACT {} · WACT {} · foreground {}",
                format_duration(card.active.active_secs),
                format_duration(card.active.work_active_secs),
                card.foreground_secs
                    .map_or_else(|| "-".to_string(), format_duration)
            ),
            width,
        )),
        Line::from(truncate_width(
            &format!("agents {}", agent_summary(card)),
            width,
        )),
        Line::from(truncate_width(
            &format!(
                "target {}",
                action_target
                    .as_ref()
                    .map_or_else(|| "-".to_string(), ActionTarget::label)
            ),
            width,
        )),
        Line::from(truncate_width(
            &format!("cwd {}", card.cwd.as_deref().unwrap_or("-")),
            width,
        )),
    ];
    if let Some(prompt) = card.last_prompt.as_deref() {
        lines.push(Line::from(truncate_width(
            &format!("prompt {}", squash_ws(prompt)),
            width,
        )));
    }
    if let Some(response) = card.last_response.as_deref() {
        lines.push(Line::from(truncate_width(
            &format!("response {}", squash_ws(response)),
            width,
        )));
    }
    if !card.agents.is_empty() && lines.len() < max_lines {
        lines.push(Line::from(Span::styled("agents", app.theme.dim_style())));
        let remaining = max_lines.saturating_sub(lines.len());
        let visible_agents = remaining.min(card.agents.len());
        for agent in card.agents.iter().take(visible_agents) {
            lines.push(agent_roster_line(
                agent,
                app.data.generated_at,
                width,
                app.theme,
                action_target
                    .as_ref()
                    .is_some_and(|target| agent_matches_action_target(agent, target)),
            ));
        }
        if visible_agents < card.agents.len() && lines.len() < max_lines {
            lines.push(Line::from(Span::styled(
                format!("+{} more", card.agents.len() - visible_agents),
                app.theme.dim_style(),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn agent_roster_line(
    agent: &Agent,
    now: OffsetDateTime,
    width: usize,
    theme: DashboardTheme,
    primary: bool,
) -> Line<'static> {
    let target = agent
        .pane
        .clone()
        .or_else(|| agent.surface.as_ref().map(|surface| surface.id.clone()))
        .unwrap_or_else(|| "-".to_string());
    let message = agent
        .last_notification
        .as_deref()
        .or(agent.last_prompt.as_deref())
        .map_or_else(|| "-".to_string(), squash_ws);
    let text = truncate_width(
        &format!(
            "{} {} · {} · {} · {}",
            agent.kind,
            agent.state,
            target,
            relative_time(agent.last_activity_at, now),
            message
        ),
        width.saturating_sub(2),
    );
    let marker = if primary {
        rich_icon("▶", ">")
    } else {
        crate::state_icon(agent.state)
    };
    let marker_style = if primary {
        theme.selected_border()
    } else {
        theme.state_style(agent.state)
    };
    let text_style = if primary {
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(marker, marker_style),
        Span::raw(" "),
        Span::styled(text, text_style),
    ])
}

fn render_capture_panel(f: &mut Frame, area: Rect, card: &SessionCard, app: &DashboardApp) {
    let target = app
        .action_target_for(card)
        .map(|target| target.capture_target());
    let title = target
        .as_ref()
        .map_or_else(|| "capture".to_string(), capture_target_label);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style())
        .border_type(BorderType::Plain)
        .title(Span::styled(
            format!(" {} {title} ", icon_capture()),
            app.theme.title_style(),
        ));
    let inner_height = usize::from(block.inner(area).height).max(1);
    let body = capture_text(
        &app.capture,
        target.as_ref(),
        app.theme,
        inner_height,
        app.capture_scroll,
    );
    f.render_widget(Paragraph::new(body).block(block), area);
}

fn capture_text<'a>(
    capture: &'a CaptureCache,
    target: Option<&CaptureTarget>,
    theme: DashboardTheme,
    visible_lines: usize,
    scroll: usize,
) -> Text<'a> {
    use ansi_to_tui::IntoText;

    let placeholder = |msg: &'static str| {
        Text::from(Line::from(Span::styled(
            msg,
            theme.dim_style().add_modifier(Modifier::ITALIC),
        )))
    };
    let Some(target) = target else {
        return placeholder("(no capture target)");
    };
    if capture.target.as_ref() != Some(target) {
        return placeholder("(capturing...)");
    }
    if let Some(message) = capture.message.as_deref() {
        return Text::from(Line::from(Span::styled(
            message.to_string(),
            theme.dim_style().add_modifier(Modifier::ITALIC),
        )));
    }
    let Some(text) = capture.text.as_ref() else {
        return placeholder("(capture unavailable)");
    };
    if text.is_empty() {
        return placeholder("(empty screen)");
    }
    let text = capture_tail(text, visible_lines, scroll);
    text.as_bytes()
        .into_text()
        .unwrap_or_else(|_| Text::from(text))
}

fn capture_tail(text: &str, visible_lines: usize, scroll: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    let visible_lines = visible_lines.max(1);
    let max_scroll = lines.len().saturating_sub(1);
    let scroll = scroll.min(max_scroll);
    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(visible_lines);
    lines[start..end].join("\n")
}

fn render_footer(f: &mut Frame, area: Rect, app: &DashboardApp) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(app.theme.border_style());
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(hint) = app
        .hint
        .as_ref()
        .filter(|hint| hint.set_at.elapsed() < HINT_TTL)
    {
        let style = match hint.level {
            HintLevel::Ok => Style::default().fg(app.theme.ok),
            HintLevel::Err => Style::default()
                .fg(app.theme.error)
                .add_modifier(Modifier::BOLD),
            HintLevel::Info => app.theme.dim_style(),
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(hint.message.clone(), style))),
            inner,
        );
        return;
    }

    let page = page_text(app);
    let target = app.selected_target_position().map_or_else(
        || "target -".to_string(),
        |(idx, count)| format!("target {idx}/{count}"),
    );
    let spans = vec![
        Span::styled(page, app.theme.dim_style()),
        Span::raw("  "),
        Span::styled(target, app.theme.dim_style()),
        Span::raw("  "),
        key("↑↓←→/hjkl", app.theme),
        Span::raw(" move  "),
        key("Tab", app.theme),
        Span::raw(" target  "),
        key("Pg", app.theme),
        Span::raw(" capture  "),
        key("Enter", app.theme),
        Span::raw(" inspect  "),
        key("p", app.theme),
        Span::raw(" prompt  "),
        key("R", app.theme),
        Span::raw(" abort  "),
        key("K", app.theme),
        Span::raw(" terminate  "),
        key("o", app.theme),
        Span::raw(" open  "),
        key("f", app.theme),
        Span::raw(" full  "),
        key("n", app.theme),
        Span::raw(" notes  "),
        key("r", app.theme),
        Span::raw(" refresh  "),
        key("?", app.theme),
        Span::raw(" help"),
    ];
    let width = usize::from(inner.width);
    f.render_widget(Paragraph::new(Line::from(fit_spans(spans, width))), inner);
}

fn key(label: &'static str, theme: DashboardTheme) -> Span<'static> {
    Span::styled(format!(" {label} "), theme.key_style())
}

fn page_text(app: &DashboardApp) -> String {
    if app.data.cards.is_empty() {
        return "0/0".to_string();
    }
    format!("{}/{}", app.selected + 1, app.data.cards.len())
}

fn render_help(f: &mut Frame, area: Rect, theme: DashboardTheme) {
    let lines = vec![
        Line::from(Span::styled("Keybindings", theme.title_style())),
        Line::from(""),
        Line::from("  arrows / hjkl     move card selection"),
        Line::from("  Tab / [ / ]       cycle action target in selected card"),
        Line::from("  PageUp/PageDown   scroll capture history"),
        Line::from("  f                 toggle capture fullscreen"),
        Line::from("  G / End           jump capture to latest output"),
        Line::from("  n                 show dashboard notes"),
        Line::from("  Enter             toggle inspector"),
        Line::from("  p                 compose prompt for selected target"),
        Line::from("  c                 copy last prompt"),
        Line::from("  R                 abort current turn"),
        Line::from("  K                 terminate pane or PTY session"),
        Line::from("  o                 open target explicitly"),
        Line::from("  r                 refresh now"),
        Line::from("  ?                 close this help"),
        Line::from("  q / Esc           quit"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.selected_border())
        .border_type(BorderType::Plain)
        .title(Span::styled(" help ", theme.title_style()));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_notes(f: &mut Frame, area: Rect, app: &DashboardApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.warn))
        .border_type(BorderType::Plain)
        .title(Span::styled(" notes ", app.theme.title_style()));
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let max_lines = usize::from(area.height.saturating_sub(2)).max(1);
    let mut lines = app
        .data
        .notes
        .iter()
        .take(max_lines.saturating_sub(1))
        .map(|note| {
            Line::from(Span::styled(
                truncate_width(note, width),
                Style::default().fg(app.theme.warn),
            ))
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("no notes", app.theme.dim_style())));
    }
    lines.push(Line::from(Span::styled(
        "Esc/n closes",
        app.theme.dim_style(),
    )));
    lines.truncate(max_lines);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_confirm(f: &mut Frame, area: Rect, app: &DashboardApp) {
    let popup = app.confirm.as_ref().expect("confirm checked by caller");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.warn))
        .border_type(BorderType::Plain)
        .title(Span::styled(
            " confirm ",
            Style::default()
                .fg(Color::Black)
                .bg(app.theme.warn)
                .add_modifier(Modifier::BOLD),
        ));
    let lines = vec![
        Line::from(popup.message.clone()),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y]", Style::default().fg(app.theme.ok)),
            Span::raw("es / "),
            Span::styled("[N]", Style::default().fg(app.theme.panel)),
            Span::raw("o"),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_composer(f: &mut Frame, area: Rect, app: &DashboardApp) {
    let composer = app.composer.as_ref().expect("composer checked by caller");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.selected_border())
        .border_type(BorderType::Plain)
        .title(Span::styled(
            format!(" prompt → {} ", composer.label),
            app.theme.title_style(),
        ));
    let inner = block.inner(area);
    let visible = visible_input(
        &composer.input,
        composer.cursor,
        inner.width.saturating_sub(2),
    );
    let line = Line::from(vec![Span::raw("> "), Span::raw(visible.text.clone())]);
    f.render_widget(Paragraph::new(line).block(block), area);

    let cursor_visible = composer.cursor.saturating_sub(visible.skipped_chars);
    let before_cursor = visible
        .text
        .chars()
        .take(cursor_visible)
        .collect::<String>();
    let cursor_x = inner
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(before_cursor.width()).unwrap_or(u16::MAX));
    if inner.height > 0 && cursor_x < inner.x.saturating_add(inner.width) {
        f.set_cursor_position((cursor_x, inner.y));
    }
}

struct VisibleInput {
    text: String,
    skipped_chars: usize,
}

fn visible_input(input: &str, cursor: usize, width: u16) -> VisibleInput {
    let width = usize::from(width.max(1));
    let chars = input.chars().collect::<Vec<_>>();
    let mut start = 0usize;
    while start < cursor && char_width(&chars[start..cursor]) >= width {
        start += 1;
    }
    let mut end = start;
    let mut used = 0usize;
    while end < chars.len() {
        let ch_width = UnicodeWidthChar::width(chars[end]).unwrap_or(0);
        if used.saturating_add(ch_width) > width {
            break;
        }
        used += ch_width;
        end += 1;
    }
    VisibleInput {
        text: chars[start..end].iter().collect(),
        skipped_chars: start,
    }
}

fn char_width(chars: &[char]) -> usize {
    chars
        .iter()
        .map(|ch| UnicodeWidthChar::width(*ch).unwrap_or(0))
        .sum()
}

fn card_columns(width: u16) -> usize {
    if width >= 150 {
        3
    } else if width >= 76 {
        2
    } else {
        1
    }
}

fn card_row_heights(height: u16) -> Vec<u16> {
    if height == 0 {
        return Vec::new();
    }

    let mut remaining = height;
    let mut rows = Vec::new();
    loop {
        if rows.is_empty() {
            let row = remaining.min(CARD_HEIGHT);
            rows.push(row);
            remaining = remaining.saturating_sub(row);
        } else {
            let after_gutter = remaining.saturating_sub(1);
            if after_gutter < MIN_CARD_HEIGHT {
                break;
            }
            remaining = after_gutter;
            let row = remaining.min(CARD_HEIGHT);
            rows.push(row);
            remaining = remaining.saturating_sub(row);
        }

        if remaining == 0 {
            break;
        }
    }
    rows
}

fn centered_rect_by_size(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn bottom_prompt_rect(area: Rect) -> Rect {
    let height = area.height.min(3);
    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height),
        width: area.width,
        height,
    }
}

fn capture_target_label(target: &CaptureTarget) -> String {
    match target {
        CaptureTarget::Pane(pane) => format!("pane {pane}"),
        CaptureTarget::PtySession(session) => format!("pty {session}"),
    }
}

fn agent_summary(card: &SessionCard) -> String {
    if card.agents.is_empty() {
        return "none".to_string();
    }
    card.agents
        .iter()
        .map(|agent| format!("{}:{}", agent.kind, agent.state))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn format_clock(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC))
        .format(time::macros::format_description!(
            "[hour]:[minute]:[second]"
        ))
        .unwrap_or_else(|_| at.to_string())
}

fn relative_time(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let secs = (now - at).whole_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
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
    format!("{}d{:02}h", hours / 24, hours % 24)
}

fn truncate_width(value: &str, max_width: usize) -> String {
    crate::truncate_cell(value, max_width)
}

fn fit_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    if max_width == 0 {
        return Vec::new();
    }
    let mut remaining = max_width;
    let mut out = Vec::new();
    for span in spans {
        let content = span.content.into_owned();
        let content_width = content.width();
        if content_width <= remaining {
            remaining = remaining.saturating_sub(content_width);
            out.push(Span::styled(content, span.style));
            continue;
        }
        if remaining > 0 {
            out.push(Span::styled(
                truncate_width(&content, remaining),
                span.style,
            ));
        }
        break;
    }
    out
}

fn squash_ws(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").ok();
    let path = home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).map(|rest| format!("~{rest}")))
        .unwrap_or_else(|| path.to_string());
    truncate_middle(&path, 42)
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars || max_chars < 8 {
        return value.to_string();
    }
    let left = (max_chars - 3) / 2;
    let right = max_chars - 3 - left;
    format!(
        "{}...{}",
        chars[..left].iter().collect::<String>(),
        chars[chars.len() - right..].iter().collect::<String>()
    )
}

fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    s.char_indices()
        .nth(char_idx)
        .map_or_else(|| s.len(), |(idx, _)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::event::SurfaceRef;
    use ratatui::backend::TestBackend;
    use time::macros::datetime;

    #[allow(clippy::too_many_arguments)]
    fn fake_agent(
        session: &str,
        pane: Option<&str>,
        state: AgentState,
        prompt: Option<&str>,
        at: OffsetDateTime,
    ) -> Agent {
        Agent {
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::Codex,
            session_id: session.to_string(),
            surface: None,
            pane: pane.map(str::to_string),
            cwd: Some("/tmp/project".into()),
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            state,
            last_prompt: prompt.map(str::to_string),
            last_response: None,
            last_notification: None,
            model: Some("gpt-test".into()),
            context_used_pct: Some(42.0),
            cost_usd: Some(0.25),
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: at,
            last_activity_at: at,
            state_entered_at: at,
        }
    }

    fn fake_pane(pane_id: &str, session: &str) -> PaneInfo {
        PaneInfo {
            socket: None,
            pane_id: pane_id.to_string(),
            session: session.to_string(),
            window_index: "1".into(),
            pane_index: "0".into(),
            tty: "/dev/pts/1".into(),
            current_command: "zsh".into(),
            title: "shell".into(),
            pane_pid: 123,
        }
    }

    fn fake_pty(id: &str, name: &str) -> SessionRef {
        SessionRef {
            id: id.to_string(),
            backend: SessionBackendKind::Pty,
            display_name: Some(name.to_string()),
            cwd: Some("/tmp/pty".into()),
            attached_clients: 0,
            exited: false,
            exit_status: None,
            pid: Some(99),
        }
    }

    #[test]
    fn build_groups_tmux_session_cards_and_sorts_attention_first() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut active = SessionActiveStats::default();
        active.by_session.insert(
            "main".into(),
            ActiveDuration {
                active_secs: 600,
                work_active_secs: 300,
            },
        );
        active.totals = ActiveDuration {
            active_secs: 600,
            work_active_secs: 300,
        };
        let data = build_dashboard_data(
            now,
            vec![
                fake_agent(
                    "s1",
                    Some("%1"),
                    AgentState::Idle,
                    Some("ship it"),
                    now - time::Duration::minutes(10),
                ),
                fake_agent(
                    "s2",
                    Some("%2"),
                    AgentState::WaitingInput,
                    Some("need approval"),
                    now,
                ),
            ],
            vec![fake_pane("%1", "main"), fake_pane("%2", "review")],
            Vec::new(),
            Vec::new(),
            active,
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );

        assert_eq!(data.cards[0].label, "review");
        assert_eq!(data.cards[0].status, CardStatus::WaitingInput);
        assert_eq!(data.cards[1].label, "main");
        assert_eq!(data.cards[1].active.active_secs, 600);
        assert_eq!(data.totals.attention, 1);
    }

    #[test]
    fn pty_session_gets_prompt_target_without_tmux_pane() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut agent = fake_agent("pty-1", None, AgentState::Working, Some("run"), now);
        agent.surface = Some(SurfaceRef {
            kind: SurfaceKind::Pty,
            id: "pty-1".into(),
        });
        let data = build_dashboard_data(
            now,
            vec![agent],
            Vec::new(),
            vec![fake_pty("pty-1", "worker")],
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );

        assert_eq!(data.cards.len(), 1);
        assert_eq!(data.cards[0].label, "worker");
        assert_eq!(
            data.cards[0]
                .action_targets()
                .first()
                .map(ActionTarget::prompt_target),
            Some(PromptTarget::PtySession("pty-1".into()))
        );
    }

    #[test]
    fn composer_editing_is_unicode_safe() {
        let mut composer = PromptComposer::new(PromptTarget::Pane("%1".into()), "main".into());
        composer.insert('한');
        composer.insert('a');
        composer.move_left();
        composer.backspace();
        assert_eq!(composer.input, "a");
        assert_eq!(composer.cursor, 0);
        composer.delete();
        assert_eq!(composer.input, "");
    }

    #[test]
    fn card_row_heights_use_partial_bottom_row_when_room_remains() {
        assert_eq!(card_row_heights(19), vec![6, 6, 5]);
        assert_eq!(card_row_heights(18), vec![6, 6]);
        assert_eq!(card_row_heights(6), vec![6]);
    }

    #[test]
    fn target_cycle_selects_an_alternate_pane_in_session_card() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            vec![
                fake_agent("a", Some("%1"), AgentState::Idle, Some("alpha"), now),
                fake_agent("b", Some("%2"), AgentState::WaitingInput, Some("beta"), now),
            ],
            vec![fake_pane("%1", "main"), fake_pane("%2", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let mut app = DashboardApp::new(data, WatchTheme::Classic);

        assert_eq!(
            app.selected_action_target(),
            Some(ActionTarget::Pane("%2".into()))
        );
        app.cycle_target(1);
        assert_eq!(
            app.selected_action_target(),
            Some(ActionTarget::Pane("%1".into()))
        );
    }

    #[test]
    fn destructive_confirm_names_exact_target() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            vec![
                fake_agent("a", Some("%1"), AgentState::Idle, Some("alpha"), now),
                fake_agent("b", Some("%2"), AgentState::WaitingInput, Some("beta"), now),
            ],
            vec![fake_pane("%1", "main"), fake_pane("%2", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let mut app = DashboardApp::new(data, WatchTheme::Classic);

        assert!(matches!(confirm_kill(&mut app), UiAction::None));
        let popup = app.confirm.as_ref().unwrap();
        assert!(popup.message.contains("pane %2"));
        assert!(popup.message.contains("main"));
    }

    #[test]
    fn zellij_pane_write_actions_are_disabled_instead_of_using_tmux() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            vec![fake_agent(
                "a",
                Some("zj-1"),
                AgentState::WaitingInput,
                Some("approve"),
                now,
            )],
            vec![fake_pane("zj-1", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Zellij,
            Vec::new(),
        );
        let mut app = DashboardApp::new(data, WatchTheme::Classic);

        assert!(matches!(open_composer(&mut app), UiAction::None));
        assert!(app.composer.is_none());
        assert!(app
            .hint
            .as_ref()
            .is_some_and(|hint| hint.message.contains("zellij")));
    }

    #[test]
    fn capture_tail_defaults_to_latest_lines_and_can_scroll_back() {
        let text = "one\ntwo\nthree\nfour";

        assert_eq!(capture_tail(text, 2, 0), "three\nfour");
        assert_eq!(capture_tail(text, 2, 1), "two\nthree");
        assert_eq!(capture_tail(text, 2, 99), "one");
    }

    #[test]
    fn rate_limit_hint_prefers_active_cap() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut agent = fake_agent("s1", Some("%1"), AgentState::Working, Some("run"), now);
        agent.rate_limit_5h_pct = Some(100.0);
        agent.rate_limit_scope = Some(RateLimitScope::FiveHour);
        agent.rate_limited_until =
            Some(now + time::Duration::hours(2) + time::Duration::minutes(14));

        assert_eq!(
            rate_limit_hint(&agent, now),
            Some("cap 5h in 2h 14m".to_string())
        );
    }

    #[test]
    fn selected_card_renders_focus_target_and_rate_limit_hint() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut agent = fake_agent(
            "s1",
            Some("%1"),
            AgentState::WaitingInput,
            Some("approve"),
            now,
        );
        agent.rate_limit_5h_pct = Some(84.0);
        let data = build_dashboard_data(
            now,
            vec![agent],
            vec![fake_pane("%1", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let backend = TestBackend::new(96, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = DashboardApp::new(data, WatchTheme::Classic);
        terminal
            .draw(|f| {
                render_card(f, f.area(), app.selected_card().unwrap(), true, &app);
            })
            .unwrap();
        let dump = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(dump.contains("FOCUS"));
        assert!(dump.contains("codex pane %1"));
        assert!(dump.contains("5h 84%"));
    }

    #[test]
    fn render_cards_uses_partial_bottom_row_for_last_session() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            vec![
                fake_agent("a", Some("%1"), AgentState::Idle, Some("alpha"), now),
                fake_agent("b", Some("%2"), AgentState::Idle, Some("beta"), now),
                fake_agent("c", Some("%3"), AgentState::Idle, Some("gamma"), now),
            ],
            vec![
                fake_pane("%1", "alpha"),
                fake_pane("%2", "beta"),
                fake_pane("%3", "gamma"),
            ],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Name,
            HostKind::Tmux,
            Vec::new(),
        );
        let backend = TestBackend::new(72, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DashboardApp::new(data, WatchTheme::Classic);
        terminal
            .draw(|f| {
                let area = f.area();
                render_cards(f, area, &mut app);
            })
            .unwrap();
        let dump = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(dump.contains("alpha"));
        assert!(dump.contains("beta"));
        assert!(dump.contains("gamma"));
    }

    #[test]
    fn inspector_renders_agent_roster_for_selected_session() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            vec![
                fake_agent("a", Some("%1"), AgentState::Working, Some("build"), now),
                fake_agent(
                    "b",
                    Some("%2"),
                    AgentState::WaitingInput,
                    Some("approve"),
                    now,
                ),
            ],
            vec![fake_pane("%1", "main"), fake_pane("%2", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let backend = TestBackend::new(96, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = DashboardApp::new(data, WatchTheme::Classic);
        terminal
            .draw(|f| {
                let area = f.area();
                render_detail_panel(f, area, app.selected_card().unwrap(), &app);
            })
            .unwrap();
        let dump = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(dump.contains("agents"));
        assert!(dump.contains("working"));
        assert!(dump.contains("waiting_input"));
    }

    #[test]
    fn render_smoke_desktop_and_narrow() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            vec![fake_agent(
                "s1",
                Some("%1"),
                AgentState::Working,
                Some("implement dashboard"),
                now,
            )],
            vec![fake_pane("%1", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );

        for (width, height) in [(120, 32), (72, 22)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = DashboardApp::new(data.clone(), WatchTheme::Classic);
            terminal.draw(|f| render(f, &mut app)).unwrap();
            let dump = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(dump.contains("muxa dashboard"));
            assert!(dump.contains("main"));
        }
    }

    #[test]
    fn narrow_dashboard_keeps_inspector_visible() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            vec![fake_agent(
                "s1",
                Some("%1"),
                AgentState::Working,
                Some("implement dashboard"),
                now,
            )],
            vec![fake_pane("%1", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let backend = TestBackend::new(80, 22);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DashboardApp::new(data, WatchTheme::Classic);
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(dump.contains("inspector"));
        assert!(dump.contains("capture"));
    }
}
