//! `muxa dashboard` — Work-first operator console.
//!
//! `muxa watch` remains the compact execution picker. This module shows durable
//! muxa Work as the primary unit, with runs and panes retained as expandable
//! execution detail and control targets.

use anyhow::{Context, Result};
use clap::ValueEnum;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa::collaboration::{
    AirArtifactProfile, AirArtifactReference, CollaborationOrigin, CollaborationRequest,
    NewRequest, Participant, RequestKind, RequestMailbox, RequestStatus, RoomContext, WorkMode,
};
use muxa::config::{IconSet, WatchTheme};
use muxa::event::RateLimitScope;
use muxa::ipc::Client;
#[cfg(test)]
use muxa::session::SessionBackendKind;
use muxa::session_activity::SessionActivity;
use muxa::tmux::PaneInfo;
use muxa::work::{BoardStage, ExternalItemRef, WorkSignal, WorkSnapshot};
#[cfg(test)]
use muxa::SessionRef;
use muxa::{
    Agent, AgentKind, AgentState, BackendEndpoint, Config, HostKind, PaneBackend, PaneKey,
    ScopeExclusions, SessionKey, SurfaceKind, TopologyNodeKey, WindowKey,
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

use crate::message_skill::{
    insert_prompt as insert_message_skill_prompt, matching_skills, Palette as MessageSkillPalette,
};
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
    TopologyPane(PaneKey),
    Pane(String),
    PtySession(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActionTarget {
    TopologyPane(PaneKey),
    Pane(String),
    PtySession(String),
}

impl ActionTarget {
    fn capture_target(&self) -> CaptureTarget {
        match self {
            Self::TopologyPane(pane) => CaptureTarget::TopologyPane(pane.clone()),
            Self::Pane(pane) => CaptureTarget::Pane(pane.clone()),
            Self::PtySession(session) => CaptureTarget::PtySession(session.clone()),
        }
    }

    fn open_target(&self) -> OpenTarget {
        match self {
            Self::TopologyPane(pane) => OpenTarget::TopologyPane(pane.clone()),
            Self::Pane(pane) => OpenTarget::Pane(pane.clone()),
            Self::PtySession(session) => OpenTarget::PtySession(session.clone()),
        }
    }

    fn prompt_target(&self) -> PromptTarget {
        match self {
            Self::TopologyPane(pane) => PromptTarget::TopologyPane(pane.clone()),
            Self::Pane(pane) => PromptTarget::Pane(pane.clone()),
            Self::PtySession(session) => PromptTarget::PtySession(session.clone()),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::TopologyPane(pane) => format!(
                "{} pane {}",
                pane.window.session.endpoint.socket, pane.pane_id
            ),
            Self::Pane(pane) => format!("pane {pane}"),
            Self::PtySession(session) => format!("pty {session}"),
        }
    }

    fn is_pane(&self) -> bool {
        matches!(self, Self::TopologyPane(_) | Self::Pane(_))
    }

    fn pane_id(&self) -> Option<&str> {
        match self {
            Self::TopologyPane(pane) => Some(&pane.pane_id),
            Self::Pane(pane) => Some(pane),
            Self::PtySession(_) => None,
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
    collaboration: CollaborationData,
}

#[derive(Debug, Clone, Default)]
struct CollaborationData {
    origin: Option<CollaborationOrigin>,
    room: Option<RoomContext>,
    /// The agent `incoming` belongs to — the card under the cursor when the
    /// data was loaded. `None` when that card names no resolvable agent.
    ///
    /// The dashboard sends as the operator console, which is never a
    /// recipient, so claiming and replying have to speak for this agent
    /// instead. Keeping it beside the requests guarantees the keys act on the
    /// mailbox actually on screen.
    inbox: Option<CollaborationAnchor>,
    incoming: Vec<CollaborationRequest>,
    sent: Vec<CollaborationRequest>,
    unavailable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CollaborationAnchor {
    origin: CollaborationOrigin,
    label: String,
}

impl CollaborationData {
    fn participant_for_pane(&self, pane: &str) -> Option<&Participant> {
        let room = self.room.as_ref()?;
        std::iter::once(&room.current)
            .chain(room.peers.iter())
            .find(|participant| participant.pane == pane)
    }

    fn peer_for_pane(&self, pane: &str) -> Option<&Participant> {
        self.room
            .as_ref()?
            .peers
            .iter()
            .find(|participant| participant.pane == pane)
    }
}

#[derive(Debug, Clone, Default)]
struct DashboardTotals {
    works: usize,
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
    pane_keys: Vec<PaneKey>,
    pane_labels: Vec<String>,
    primary_pane: Option<String>,
    primary_pane_key: Option<PaneKey>,
    pty_session_id: Option<String>,
    cwd: Option<String>,
    agents: Vec<Agent>,
    status: CardStatus,
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
    workspace: Option<String>,
    work_id: Option<String>,
    stage: Option<BoardStage>,
    signals: Vec<WorkSignal>,
    external_item: Option<ExternalItemRef>,
    run_count: usize,
}

impl SessionCard {
    fn action_targets(&self) -> Vec<ActionTarget> {
        let mut targets = Vec::new();
        if let Some(pane) = self.primary_pane_key.as_ref() {
            push_action_target(&mut targets, ActionTarget::TopologyPane(pane.clone()));
        } else if let Some(pane) = self.primary_pane.as_ref() {
            push_action_target(&mut targets, ActionTarget::Pane(pane.clone()));
        }
        if let Some(session) = self.pty_session_id.as_ref() {
            push_action_target(&mut targets, ActionTarget::PtySession(session.clone()));
        }
        if self.pane_keys.is_empty() {
            for agent in &self.agents {
                if let Some(pane) = agent.pane.as_ref() {
                    push_action_target(&mut targets, ActionTarget::Pane(pane.clone()));
                } else if let Some(surface) = agent.surface.as_ref() {
                    if surface.kind == SurfaceKind::Pty {
                        push_action_target(
                            &mut targets,
                            ActionTarget::PtySession(surface.id.clone()),
                        );
                    }
                }
            }
            for pane in &self.pane_ids {
                push_action_target(&mut targets, ActionTarget::Pane(pane.clone()));
            }
        } else {
            for pane in &self.pane_keys {
                push_action_target(&mut targets, ActionTarget::TopologyPane(pane.clone()));
            }
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
        if self.signals.contains(&WorkSignal::Error) {
            return 7;
        }
        if self.signals.contains(&WorkSignal::Blocked)
            || self.signals.contains(&WorkSignal::Attention)
        {
            return 6;
        }
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

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardHost {
    Tmux,
    Rmux,
    Zellij,
    Herdr,
    Pty,
    Pane,
    Agent,
}

impl CardHost {
    #[cfg(test)]
    fn label(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Rmux => "rmux",
            Self::Zellij => "zellij",
            Self::Herdr => "herdr",
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

fn board_stage_label(stage: BoardStage) -> &'static str {
    match stage {
        BoardStage::Queued => "queued",
        BoardStage::InProgress => "in progress",
        BoardStage::Review => "review",
        BoardStage::Done => "done",
    }
}

fn work_signal_label(signal: WorkSignal) -> &'static str {
    match signal {
        WorkSignal::Attention => "attention",
        WorkSignal::Blocked => "blocked",
        WorkSignal::Error => "error",
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
    message_skills: BTreeMap<String, String>,
    collaboration_mailbox: CollaborationMailbox,
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
            message_skills: BTreeMap::new(),
            collaboration_mailbox: CollaborationMailbox::default(),
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
        self.clamp_collaboration_request();
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

    fn selected_collaboration_peer(&self) -> Option<&Participant> {
        let target = self.selected_action_target()?;
        self.data.collaboration.peer_for_pane(target.pane_id()?)
    }

    fn collaboration_requests(&self) -> &[CollaborationRequest] {
        match self.collaboration_mailbox.tab {
            CollaborationTab::Incoming => &self.data.collaboration.incoming,
            CollaborationTab::Sent => &self.data.collaboration.sent,
        }
    }

    fn selected_collaboration_request(&self) -> Option<&CollaborationRequest> {
        self.collaboration_requests()
            .get(self.collaboration_mailbox.selected)
    }

    fn clamp_collaboration_request(&mut self) {
        let len = self.collaboration_requests().len();
        self.collaboration_mailbox.selected = if len == 0 {
            0
        } else {
            self.collaboration_mailbox.selected.min(len - 1)
        };
    }

    fn move_collaboration_request(&mut self, delta: isize) {
        let len = self.collaboration_requests().len();
        if len == 0 {
            return;
        }
        self.collaboration_mailbox.selected = self
            .collaboration_mailbox
            .selected
            .saturating_add_signed(delta)
            .min(len - 1);
    }

    fn toggle_collaboration_mailbox(&mut self) {
        self.collaboration_mailbox.tab = match self.collaboration_mailbox.tab {
            CollaborationTab::Incoming => CollaborationTab::Sent,
            CollaborationTab::Sent => CollaborationTab::Incoming,
        };
        self.collaboration_mailbox.selected = 0;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Overlay {
    #[default]
    None,
    Help,
    Notes,
    CaptureFullscreen,
    Collaboration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CollaborationTab {
    #[default]
    Incoming,
    Sent,
}

#[derive(Debug, Clone, Default)]
struct CollaborationMailbox {
    tab: CollaborationTab,
    selected: usize,
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
    PanePrompt {
        pane: PaneKey,
        text: String,
    },
    PaneAbort(PaneKey),
    WorkPrompt {
        panes: Vec<PaneKey>,
        text: String,
    },
    WorkAbort {
        panes: Vec<PaneKey>,
    },
    PtyPrompt {
        session_id: String,
        text: String,
    },
    PtyCtrlC(String),
    TerminatePty(String),
    CollaborationInbox {
        origin: CollaborationOrigin,
    },
    CollaborationSend {
        origin: CollaborationOrigin,
        target: String,
        kind: RequestKind,
        body: String,
        work_mode: WorkMode,
    },
    CollaborationReply {
        origin: CollaborationOrigin,
        request_id: String,
        status: RequestStatus,
        body: String,
    },
    CollaborationCancel {
        origin: CollaborationOrigin,
        request_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptComposer {
    target: PromptTarget,
    label: String,
    input: String,
    cursor: usize,
    skill_palette: Option<MessageSkillPalette>,
}

impl PromptComposer {
    fn new(target: PromptTarget, label: String) -> Self {
        Self {
            target,
            label,
            input: String::new(),
            cursor: 0,
            skill_palette: None,
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
    TopologyPane(PaneKey),
    Pane(String),
    PtySession(String),
    WorkPanes(Vec<PaneKey>),
    CollaborationSend {
        origin: CollaborationOrigin,
        target: String,
        kind: RequestKind,
        work_mode: WorkMode,
    },
    CollaborationReply {
        origin: CollaborationOrigin,
        request_id: String,
        status: RequestStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CaptureTarget {
    TopologyPane(PaneKey),
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
    // No app, therefore no cursor, therefore no anchor on the very first load.
    // The first `b` fills it in.
    let initial = load_dashboard_data(client, cfg, &args, None).await?;
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    let theme = args.theme.map_or(cfg.ui.theme, WatchTheme::from);
    let mut app = DashboardApp::new(initial, theme);
    app.message_skills = cfg.message.skills.clone();
    let mut last_refresh = Instant::now();
    let mut refresh_task: Option<DashboardRefresh> = None;

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
                        refresh_task = Some(spawn_refresh(
                            client,
                            cfg,
                            &args,
                            RefreshSource::Manual,
                            dashboard_mailbox_anchor(&app),
                        ));
                        last_refresh = Instant::now();
                        app.set_hint("refreshing", HintLevel::Info);
                    }
                }
                UiAction::RefreshCollaboration => {
                    refresh_collaboration_data(client, &mut app).await;
                }
                UiAction::Open(target) => {
                    if let Some(refresh) = refresh_task.take() {
                        refresh.task.abort();
                    }
                    return Ok(Some(target));
                }
                UiAction::Run(action) => {
                    if let Some(refresh) = refresh_task.take() {
                        refresh.task.abort();
                    }
                    let outcome = run_pending_action(client, action).await;
                    apply_outcome(&mut app, outcome);
                    refresh_collaboration_data(client, &mut app).await;
                    refresh_capture(client, &mut app).await;
                    last_refresh = Instant::now();
                }
            }
        }

        if refresh_task
            .as_ref()
            .is_some_and(|refresh| refresh.task.is_finished())
        {
            let DashboardRefresh {
                task,
                source,
                anchor,
            } = refresh_task.take().expect("checked above");
            match task.await {
                Ok(Ok(data)) => apply_refresh_data(&mut app, data, source, anchor.as_ref()),
                Ok(Err(e)) => app.set_hint(format!("refresh failed: {e}"), HintLevel::Err),
                Err(e) => app.set_hint(format!("refresh task failed: {e}"), HintLevel::Err),
            }
            refresh_capture(client, &mut app).await;
            last_refresh = Instant::now();
        } else if last_refresh.elapsed() >= REFRESH_INTERVAL && refresh_task.is_none() {
            refresh_task = Some(spawn_refresh(
                client,
                cfg,
                &args,
                RefreshSource::Automatic,
                dashboard_mailbox_anchor(&app),
            ));
            last_refresh = Instant::now();
        } else {
            refresh_capture(client, &mut app).await;
        }
    }

    if let Some(refresh) = refresh_task.take() {
        refresh.task.abort();
    }

    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshSource {
    Manual,
    Automatic,
}

struct DashboardRefresh {
    task: tokio::task::JoinHandle<Result<DashboardData>>,
    source: RefreshSource,
    anchor: Option<CollaborationAnchor>,
}

/// The background refresh cannot see the cursor once it is running, so the
/// mailbox anchor is captured at spawn time. A refresh that lands after the
/// cursor moved shows the previous card's inbox for one cycle; `b` re-reads it
/// on open, which is when it matters.
fn spawn_refresh(
    client: &Client,
    cfg: &Config,
    args: &Args,
    source: RefreshSource,
    anchor: Option<CollaborationAnchor>,
) -> DashboardRefresh {
    let client = client.clone();
    let cfg = cfg.clone();
    let args = args.clone();
    let task_anchor = anchor.clone();
    DashboardRefresh {
        task: tokio::spawn(
            async move { load_dashboard_data(&client, &cfg, &args, task_anchor).await },
        ),
        source,
        anchor,
    }
}

fn apply_refresh_data(
    app: &mut DashboardApp,
    data: DashboardData,
    source: RefreshSource,
    loaded_anchor: Option<&CollaborationAnchor>,
) {
    // The mailbox anchor was captured when this refresh was spawned, and
    // refreshes land every second. If the cursor has moved since — or `b`
    // re-read the mailbox on open — this snapshot's inbox belongs to a
    // different card, and applying it would swap the mailbox out from under
    // the overlay: the list changes while the selection index stays put, so
    // `e` would reply to a request the operator never looked at. Keep what is
    // on screen and let the next refresh, which carries the current anchor,
    // bring it forward.
    let current_anchor = dashboard_mailbox_anchor(app);
    let stale_inbox = current_anchor.as_ref().map(|anchor| &anchor.origin)
        != loaded_anchor.map(|anchor| &anchor.origin);
    let on_screen = stale_inbox.then(|| {
        (
            app.data.collaboration.inbox.clone(),
            app.data.collaboration.incoming.clone(),
        )
    });
    app.replace_data(data);
    if let Some((inbox, incoming)) = on_screen {
        // Only these fields belong to the cursor captured by the refresh.
        // Room, sent mail, and availability describe the console itself and
        // must continue updating even while an older inbox result is ignored.
        app.data.collaboration.inbox = inbox;
        app.data.collaboration.incoming = incoming;
        app.clamp_collaboration_request();
    }
    if source == RefreshSource::Manual {
        app.set_hint("refreshed", HintLevel::Ok);
    }
}

async fn load_dashboard_data(
    client: &Client,
    cfg: &Config,
    args: &Args,
    anchor: Option<CollaborationAnchor>,
) -> Result<DashboardData> {
    let now = OffsetDateTime::now_utc();
    let agents = client
        .snapshot()
        .await
        .context("querying daemon agent snapshot")?;

    let backend = muxa::default_backend();
    let panes = backend.list_panes();
    let mut notes = Vec::new();
    let scan = muxa::tmux::scanner::scan().await;
    notes.extend(
        scan.errors
            .iter()
            .map(|error| format!("tmux scan {}: {}", error.socket.display(), error.message)),
    );
    let records = muxa::dashboard::load_work_records(muxa::paths::default_dashboard_work_file());
    let work_snapshot = muxa::work::build_snapshot(&scan.panes, &agents, &records, now);
    if args.include_paneless {
        notes.push(
            "paneless agents are execution diagnostics; use `muxa watch` to inspect them".into(),
        );
    }
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

    let mut data = build_work_dashboard_data(
        now,
        work_snapshot,
        panes,
        session_activities,
        active_stats,
        args.sort,
        notes,
    );
    data.collaboration = load_collaboration_data(client, anchor).await;
    Ok(data)
}

async fn load_collaboration_data(
    client: &Client,
    anchor: Option<CollaborationAnchor>,
) -> CollaborationData {
    let origin = dashboard_collaboration_origin();
    let room = match client.collaboration_context(&origin).await {
        Ok(room) => room,
        Err(error) => {
            return CollaborationData {
                origin: Some(origin),
                unavailable: Some(friendly_collaboration_error(&error.to_string())),
                ..CollaborationData::default()
            };
        }
    };
    // Incoming belongs to the selected card, not to the console: the console
    // dispatches and never receives, so a reply lives in the mailbox of the
    // agent that was commanded. Sent stays the console's own dispatch log.
    let (incoming, sent) = tokio::join!(
        async {
            match anchor.as_ref() {
                Some(anchor) => {
                    client
                        .collaboration_list(&anchor.origin, RequestMailbox::Incoming)
                        .await
                }
                None => Ok(Vec::new()),
            }
        },
        client.collaboration_list(&origin, RequestMailbox::Sent),
    );
    // A card can name a pane that is not a collaboration participant — PTY
    // sessions, `muxa register` task rows, agents that just stopped. That is a
    // fact about the cursor, so it costs that card its inbox and nothing else.
    let (inbox, incoming) = match incoming {
        Ok(incoming) => (anchor, incoming),
        Err(_) => (None, Vec::new()),
    };
    match sent {
        Ok(sent) => CollaborationData {
            origin: Some(origin),
            room: Some(room),
            inbox,
            incoming,
            sent,
            unavailable: None,
        },
        Err(error) => CollaborationData {
            origin: Some(origin),
            room: Some(room),
            inbox,
            incoming,
            unavailable: Some(format!("mailbox unavailable: {error}")),
            ..CollaborationData::default()
        },
    }
}

/// Matched against the substring the daemon error actually carries — it reads
/// "origin is not a **hook-correlated** tracked pane agent", so the old
/// pattern never fired. A console origin needs no tracked agent, so reaching
/// this now means the daemon predates the console.
fn friendly_collaboration_error(error: &str) -> String {
    if error.contains("origin is not") && error.contains("tracked pane agent") {
        return "muxad is too old to accept console messages — restart it after `muxa upgrade`"
            .into();
    }
    error.into()
}

async fn refresh_collaboration_data(client: &Client, app: &mut DashboardApp) {
    let anchor = dashboard_mailbox_anchor(app);
    app.data.collaboration = load_collaboration_data(client, anchor).await;
    app.clamp_collaboration_request();
}

fn dashboard_collaboration_origin() -> CollaborationOrigin {
    let pane = muxa::default_backend().current_pane();
    dashboard_collaboration_origin_from(
        pane,
        std::env::var("RMUX").ok(),
        std::env::var("TMUX").ok(),
    )
}

/// `muxa dashboard` sends as the operator console, for the same reason
/// `muxa watch` does: the human at the keyboard is the sender, not whichever
/// agent occupies the pane the dashboard was launched from. The pane still
/// rides along as the room it is looking at and as audit provenance, so a
/// pane we cannot resolve degrades to a pane-less console rather than to no
/// collaboration at all — the dashboard runs from anywhere, including outside
/// tmux entirely.
fn dashboard_collaboration_origin_from(
    pane: Option<String>,
    rmux: Option<String>,
    tmux: Option<String>,
) -> CollaborationOrigin {
    let pane = pane.filter(|pane| !pane.is_empty());
    let socket = pane.as_deref().and_then(|pane| {
        let endpoint = match muxa::backend::pane_id_host_kind(pane)? {
            HostKind::Rmux => rmux,
            HostKind::Tmux => tmux,
            HostKind::Zellij | HostKind::Herdr => None,
        }?;
        let path = endpoint.split(',').next()?.trim();
        (!path.is_empty()).then(|| muxa::backend::pane_endpoint_identity(Some(pane), path))
    });
    CollaborationOrigin {
        pane: pane.unwrap_or_default(),
        socket,
        console: true,
    }
}

/// Whose inbox the mailbox overlay shows: the agent on the selected card.
///
/// Preserve the endpoint whenever it is known. Tmux pane ids repeat on every
/// server, so `%1` alone is not enough to identify an inbox. Prefer the
/// daemon's participant because it is already normalized; cards outside the
/// launch room fall back to the selected agent's recorded endpoint.
fn dashboard_mailbox_anchor(app: &DashboardApp) -> Option<CollaborationAnchor> {
    let target = app.selected_action_target()?;
    let pane = target.pane_id()?.to_string();
    let participant = app.data.collaboration.peer_for_pane(&pane);
    let agent = app.selected_card().and_then(|card| {
        card.agents
            .iter()
            .filter(|agent| agent.pane.as_deref() == Some(&pane))
            .max_by_key(|agent| agent.last_activity_at)
    });
    let label = participant
        .map(Participant::label)
        .or_else(|| agent.map(|agent| format!("{}@{pane}", agent.kind)))
        .unwrap_or_else(|| pane.clone());
    let socket = participant
        .and_then(|participant| participant.socket.clone())
        .or_else(|| {
            agent.and_then(|agent| {
                agent
                    .tmux_socket
                    .as_deref()
                    .map(|endpoint| muxa::backend::pane_endpoint_identity(Some(&pane), endpoint))
            })
        })
        .or_else(|| match &target {
            ActionTarget::TopologyPane(key) => Some(key.window.session.endpoint.socket.clone()),
            ActionTarget::Pane(_) | ActionTarget::PtySession(_) => None,
        });
    Some(CollaborationAnchor {
        origin: CollaborationOrigin {
            pane,
            socket,
            console: false,
        },
        label,
    })
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

fn build_work_dashboard_data(
    now: OffsetDateTime,
    snapshot: WorkSnapshot,
    panes: Vec<PaneInfo>,
    session_activities: Vec<SessionActivity>,
    active_stats: SessionActiveStats,
    sort: DashboardSort,
    mut notes: Vec<String>,
) -> DashboardData {
    let pane_by_id = panes
        .iter()
        .map(|pane| (pane.pane_id.clone(), pane.clone()))
        .collect::<HashMap<_, _>>();
    let activity_by_name = session_activities
        .into_iter()
        .map(|activity| (activity.name.clone(), activity))
        .collect::<HashMap<_, _>>();
    let mut cards = Vec::with_capacity(snapshot.works.len());

    for work in snapshot.works {
        let host = work
            .runs
            .first()
            .map_or(CardHost::Tmux, |run| card_host(run.execution.host));
        let mut builder = CardBuilder::new(
            format!(
                "work:{}:{}",
                work.identity.workspace_id, work.identity.work_id
            ),
            work.title,
            host,
        );
        builder.workspace = Some(work.identity.workspace_id);
        builder.work_id = Some(work.identity.work_id);
        builder.stage = Some(work.stage);
        builder.signals = work.signals;
        builder.external_item = work.external_items.into_iter().next();
        builder.run_count = work.runs.len();
        builder.cwd = work.runs.iter().find_map(|run| run.cwd.clone());

        let mut seen_agents = BTreeSet::new();
        for run in work.runs {
            let window = WindowKey {
                session: SessionKey {
                    endpoint: BackendEndpoint {
                        host: run.execution.host,
                        socket: run.execution.socket.clone(),
                    },
                    session_id: run.execution.session_id.clone(),
                },
                window_id: run.execution.window_id.clone(),
            };
            for pane in run.panes {
                builder.pane_ids.insert(pane.pane_id.clone());
                builder.pane_keys.insert(PaneKey {
                    window: window.clone(),
                    pane_id: pane.pane_id,
                });
                if let Some(agent) = pane.agent {
                    if seen_agents.insert(agent.session_id.clone()) {
                        builder.agents.push(agent);
                    }
                }
            }
        }
        cards.push(finalize_card(
            builder,
            now,
            &pane_by_id,
            &activity_by_name,
            &active_stats.by_session,
        ));
    }
    sort_cards(&mut cards, sort);

    if !snapshot.unlinked_executions.is_empty() {
        notes.push(format!(
            "{} unlinked executions hidden from Work board; use `muxa watch` for topology",
            snapshot.unlinked_executions.len()
        ));
    }
    let totals = DashboardTotals {
        works: cards.len(),
        tracked_agents: cards.iter().map(|card| card.agents.len()).sum(),
        attention: cards
            .iter()
            .filter(|card| {
                !card.signals.is_empty()
                    || matches!(
                        card.status,
                        CardStatus::Error | CardStatus::WaitingChoice | CardStatus::WaitingInput
                    )
            })
            .count(),
        working: cards
            .iter()
            .filter(|card| card.stage == Some(BoardStage::InProgress))
            .count(),
        active: active_stats.totals,
    };

    DashboardData {
        generated_at: now,
        cards,
        totals,
        notes,
        collaboration: CollaborationData::default(),
    }
}

fn card_host(host: HostKind) -> CardHost {
    match host {
        HostKind::Tmux => CardHost::Tmux,
        HostKind::Rmux => CardHost::Rmux,
        HostKind::Zellij => CardHost::Zellij,
        HostKind::Herdr => CardHost::Herdr,
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
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
            HostKind::Rmux => CardHost::Rmux,
            HostKind::Zellij => CardHost::Zellij,
            HostKind::Herdr => CardHost::Herdr,
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
        works: cards.len(),
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
        collaboration: CollaborationData::default(),
    }
}

#[derive(Debug, Clone)]
struct CardBuilder {
    key: String,
    label: String,
    host: CardHost,
    pane_ids: BTreeSet<String>,
    pane_keys: BTreeSet<PaneKey>,
    pty_session_id: Option<String>,
    cwd: Option<String>,
    agents: Vec<Agent>,
    workspace: Option<String>,
    work_id: Option<String>,
    stage: Option<BoardStage>,
    signals: Vec<WorkSignal>,
    external_item: Option<ExternalItemRef>,
    run_count: usize,
}

impl CardBuilder {
    fn new(key: String, label: String, host: CardHost) -> Self {
        Self {
            key,
            label,
            host,
            pane_ids: BTreeSet::new(),
            pane_keys: BTreeSet::new(),
            pty_session_id: None,
            cwd: None,
            agents: Vec::new(),
            workspace: None,
            work_id: None,
            stage: None,
            signals: Vec::new(),
            external_item: None,
            run_count: 0,
        }
    }
}

#[cfg(test)]
struct CardIdentity {
    key: String,
    label: String,
    host: CardHost,
    pty_session_id: Option<String>,
}

#[cfg(test)]
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
                HostKind::Rmux => CardHost::Rmux,
                HostKind::Zellij => CardHost::Zellij,
                HostKind::Herdr => CardHost::Herdr,
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
    let pane_keys = builder.pane_keys.into_iter().collect::<Vec<_>>();
    let primary_pane = choose_primary_pane(&builder.agents, &pane_ids);
    let primary_pane_key = primary_pane
        .as_ref()
        .and_then(|pane_id| {
            pane_keys
                .iter()
                .find(|pane| &pane.pane_id == pane_id)
                .cloned()
        })
        .or_else(|| pane_keys.first().cloned());
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
        pane_keys,
        pane_labels,
        primary_pane,
        primary_pane_key,
        pty_session_id: builder.pty_session_id,
        cwd: builder.cwd.or(fallback_cwd),
        agents: builder.agents,
        status,
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
        workspace: builder.workspace,
        work_id: builder.work_id,
        stage: builder.stage,
        signals: builder.signals,
        external_item: builder.external_item,
        run_count: builder.run_count,
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
    /// Re-read the mailbox before showing it. The overlay is anchored to the
    /// selected card, and the cursor moves between refreshes, so opening it
    /// has to fetch rather than render whatever the last tick happened to
    /// capture.
    RefreshCollaboration,
    Open(OpenTarget),
    Run(PendingAction),
}

fn handle_key(app: &mut DashboardApp, key: KeyEvent) -> UiAction {
    if app.confirm.is_some() {
        return handle_confirm_key(app, key);
    }

    if app.composer.is_some() {
        return handle_composer_key(app, key);
    }

    if app.overlay != Overlay::None {
        return handle_overlay_key(app, key);
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
        Overlay::Collaboration => return handle_collaboration_overlay_key(app, key),
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
        KeyCode::Char('P') => open_work_composer(app),
        KeyCode::Char('m') => open_collaboration_composer(app),
        KeyCode::Char('b') => {
            app.overlay = Overlay::Collaboration;
            UiAction::RefreshCollaboration
        }
        KeyCode::Char('i') => claim_collaboration_inbox(app),
        KeyCode::Char('o') => app.selected_action_target().map_or_else(
            || {
                app.set_hint("no pane or PTY session to open", HintLevel::Err);
                UiAction::None
            },
            |target| UiAction::Open(target.open_target()),
        ),
        KeyCode::Char('c') => copy_selected_prompt(app),
        KeyCode::Char('R') => confirm_abort(app),
        KeyCode::Char('A') => confirm_work_abort(app),
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
    if app
        .composer
        .as_ref()
        .is_some_and(|composer| composer.skill_palette.is_some())
    {
        return handle_message_skill_key(app, key);
    }
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
                PromptTarget::TopologyPane(pane) => PendingAction::PanePrompt { pane, text },
                PromptTarget::Pane(pane_id) => {
                    PendingAction::Quick(QuickAction::SendPrompt { pane_id, text })
                }
                PromptTarget::PtySession(session_id) => {
                    PendingAction::PtyPrompt { session_id, text }
                }
                PromptTarget::WorkPanes(panes) => PendingAction::WorkPrompt { panes, text },
                PromptTarget::CollaborationSend {
                    origin,
                    target,
                    kind,
                    work_mode,
                } => PendingAction::CollaborationSend {
                    origin,
                    target,
                    kind,
                    body: text,
                    work_mode,
                },
                PromptTarget::CollaborationReply {
                    origin,
                    request_id,
                    status,
                } => PendingAction::CollaborationReply {
                    origin,
                    request_id,
                    status,
                    body: text,
                },
            })
        }
        KeyCode::Tab => {
            cycle_composer_option(composer);
            UiAction::None
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            toggle_composer_execute(composer);
            UiAction::None
        }
        KeyCode::Char('/') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            composer.skill_palette = Some(MessageSkillPalette::default());
            UiAction::None
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

fn move_message_skill(app: &mut DashboardApp, delta: isize) {
    if let Some(palette) = app
        .composer
        .as_mut()
        .and_then(|composer| composer.skill_palette.as_mut())
    {
        palette.move_selection(delta, &app.message_skills);
    }
}

fn handle_message_skill_key(app: &mut DashboardApp, key: KeyEvent) -> UiAction {
    match key.code {
        KeyCode::Esc => {
            if let Some(composer) = app.composer.as_mut() {
                composer.skill_palette = None;
            }
        }
        KeyCode::Backspace => {
            let close = app
                .composer
                .as_ref()
                .and_then(|composer| composer.skill_palette.as_ref())
                .is_some_and(|palette| palette.query.is_empty());
            if let Some(composer) = app.composer.as_mut() {
                if close {
                    composer.skill_palette = None;
                } else if let Some(palette) = composer.skill_palette.as_mut() {
                    palette.backspace();
                }
            }
        }
        KeyCode::Enter => {
            let prompt = app
                .composer
                .as_ref()
                .and_then(|composer| composer.skill_palette.as_ref())
                .and_then(|palette| palette.selected_prompt(&app.message_skills));
            if let Some(prompt) = prompt {
                if let Some(composer) = app.composer.as_mut() {
                    insert_message_skill_prompt(&mut composer.input, &mut composer.cursor, &prompt);
                    composer.skill_palette = None;
                }
                app.set_hint(
                    "skill inserted — edit or press Enter to send",
                    HintLevel::Ok,
                );
            } else if app.message_skills.is_empty() {
                app.set_hint(
                    "no message skills — run: muxa skill add <name> <prompt>",
                    HintLevel::Info,
                );
            } else {
                app.set_hint("no matching message skill", HintLevel::Info);
            }
        }
        KeyCode::Up | KeyCode::BackTab => move_message_skill(app, -1),
        KeyCode::Down | KeyCode::Tab => move_message_skill(app, 1),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            move_message_skill(app, -1);
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            move_message_skill(app, 1);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(palette) = app
                .composer
                .as_mut()
                .and_then(|composer| composer.skill_palette.as_mut())
            {
                palette.insert(c);
            }
        }
        _ => {}
    }
    UiAction::None
}

fn cycle_composer_option(composer: &mut PromptComposer) {
    match &mut composer.target {
        PromptTarget::CollaborationSend { kind, .. } => {
            *kind = match *kind {
                RequestKind::Question => RequestKind::Review,
                RequestKind::Review => RequestKind::Task,
                RequestKind::Task => RequestKind::Notice,
                RequestKind::Notice => RequestKind::Question,
            };
        }
        PromptTarget::CollaborationReply { status, .. } => {
            *status = match *status {
                RequestStatus::Completed => RequestStatus::Blocked,
                RequestStatus::Blocked => RequestStatus::Declined,
                RequestStatus::Declined => RequestStatus::Failed,
                RequestStatus::Failed
                | RequestStatus::Queued
                | RequestStatus::Claimed
                | RequestStatus::Expired
                | RequestStatus::Cancelled => RequestStatus::Completed,
            };
        }
        PromptTarget::TopologyPane(_)
        | PromptTarget::Pane(_)
        | PromptTarget::PtySession(_)
        | PromptTarget::WorkPanes(_) => {}
    }
}

fn toggle_composer_execute(composer: &mut PromptComposer) {
    if let PromptTarget::CollaborationSend { work_mode, .. } = &mut composer.target {
        *work_mode = match *work_mode {
            WorkMode::ReadOnly => WorkMode::Execute,
            WorkMode::Execute => WorkMode::ReadOnly,
        };
    }
}

fn handle_collaboration_overlay_key(app: &mut DashboardApp, key: KeyEvent) -> UiAction {
    match key.code {
        KeyCode::Esc | KeyCode::Char('b' | 'q') => {
            app.overlay = Overlay::None;
            UiAction::None
        }
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('[' | ']') => {
            app.toggle_collaboration_mailbox();
            UiAction::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_collaboration_request(-1);
            UiAction::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_collaboration_request(1);
            UiAction::None
        }
        KeyCode::Char('i') => claim_collaboration_inbox(app),
        KeyCode::Char('e') => open_collaboration_reply_composer(app),
        KeyCode::Char('x') => confirm_collaboration_cancel(app),
        _ => UiAction::None,
    }
}

fn collaboration_origin_for_action(app: &mut DashboardApp) -> Option<CollaborationOrigin> {
    if app.data.collaboration.room.is_none() {
        let message = app
            .data
            .collaboration
            .unavailable
            .clone()
            .unwrap_or_else(|| "collaboration is unavailable".into());
        app.set_hint(message, HintLevel::Err);
        return None;
    }
    app.data.collaboration.origin.clone()
}

/// Claiming is the recipient's move and the console is only ever a sender, so
/// `i` speaks for the agent on the selected card. It reads the cursor rather
/// than the stored anchor because it also opens the overlay: the refresh that
/// follows re-anchors to the same cursor, so the two agree.
fn claim_collaboration_inbox(app: &mut DashboardApp) -> UiAction {
    if collaboration_origin_for_action(app).is_none() {
        return UiAction::None;
    }
    let Some(anchor) = dashboard_mailbox_anchor(app) else {
        app.set_hint(
            "select a card with an agent to claim its inbox",
            HintLevel::Err,
        );
        return UiAction::None;
    };
    app.overlay = Overlay::Collaboration;
    app.collaboration_mailbox.tab = CollaborationTab::Incoming;
    app.collaboration_mailbox.selected = 0;
    UiAction::Run(PendingAction::CollaborationInbox {
        origin: anchor.origin,
    })
}

fn open_collaboration_composer(app: &mut DashboardApp) -> UiAction {
    let Some(origin) = collaboration_origin_for_action(app) else {
        return UiAction::None;
    };
    if app
        .data
        .collaboration
        .room
        .as_ref()
        .is_some_and(|room| room.peers.is_empty())
    {
        app.set_hint(
            "no agent in this tmux window — the room is the window the dashboard was opened from",
            HintLevel::Err,
        );
        return UiAction::None;
    }
    let Some(peer) = app.selected_collaboration_peer() else {
        app.set_hint(
            "select an agent in this window with Tab, [ or ], then press m",
            HintLevel::Err,
        );
        return UiAction::None;
    };
    let target = format!("pane:{}", peer.pane);
    let label = peer.label();
    app.composer = Some(PromptComposer::new(
        PromptTarget::CollaborationSend {
            origin,
            target,
            kind: RequestKind::Question,
            work_mode: WorkMode::ReadOnly,
        },
        label,
    ));
    UiAction::None
}

fn open_collaboration_reply_composer(app: &mut DashboardApp) -> UiAction {
    if app.collaboration_mailbox.tab != CollaborationTab::Incoming {
        app.set_hint("switch to incoming requests to reply", HintLevel::Err);
        return UiAction::None;
    }
    if collaboration_origin_for_action(app).is_none() {
        return UiAction::None;
    }
    // The *stored* anchor, not the cursor: the listed requests were fetched
    // for it, and replying as anyone else is rejected as a non-participant.
    let Some(anchor) = app.data.collaboration.inbox.clone() else {
        app.set_hint("this mailbox has no agent to reply as", HintLevel::Err);
        return UiAction::None;
    };
    let Some(request) = app.selected_collaboration_request() else {
        app.set_hint("no incoming request selected", HintLevel::Err);
        return UiAction::None;
    };
    if request.status == RequestStatus::Queued {
        app.set_hint(
            "press i to claim the request before replying",
            HintLevel::Err,
        );
        return UiAction::None;
    }
    if request.status.is_terminal() {
        app.set_hint("selected request is already terminal", HintLevel::Err);
        return UiAction::None;
    }
    let request_id = request.id.clone();
    let label = format!(
        "{} → {} · {}",
        request.from.label(),
        anchor.label,
        short_request_id(&request_id)
    );
    app.composer = Some(PromptComposer::new(
        PromptTarget::CollaborationReply {
            origin: anchor.origin,
            request_id,
            status: RequestStatus::Completed,
        },
        label,
    ));
    UiAction::None
}

fn confirm_collaboration_cancel(app: &mut DashboardApp) -> UiAction {
    if app.collaboration_mailbox.tab != CollaborationTab::Sent {
        app.set_hint("switch to sent requests to cancel", HintLevel::Err);
        return UiAction::None;
    }
    let Some(origin) = collaboration_origin_for_action(app) else {
        return UiAction::None;
    };
    let Some(request) = app.selected_collaboration_request() else {
        app.set_hint("no sent request selected", HintLevel::Err);
        return UiAction::None;
    };
    if request.status != RequestStatus::Queued {
        app.set_hint("only queued requests can be cancelled", HintLevel::Err);
        return UiAction::None;
    }
    let request_id = request.id.clone();
    app.confirm = Some(ConfirmPopup {
        message: format!(
            "Cancel {} to {}?",
            short_request_id(&request_id),
            request.to.label()
        ),
        on_confirm: PendingAction::CollaborationCancel { origin, request_id },
    });
    UiAction::None
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
    if target.is_pane() && !pane_write_supported(host) {
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

fn live_work_panes(card: &SessionCard) -> Vec<PaneKey> {
    card.pane_keys
        .iter()
        .filter(|pane| {
            card.agents.iter().any(|agent| {
                agent.state != AgentState::Stopped && agent_targets_pane_key(agent, pane, card)
            })
        })
        .cloned()
        .collect()
}

fn agent_targets_pane_key(agent: &Agent, key: &PaneKey, card: &SessionCard) -> bool {
    if agent.pane.as_deref() != Some(&key.pane_id) {
        return false;
    }
    if let Some(socket) = agent.tmux_socket.as_deref() {
        let endpoint = muxa::backend::pane_endpoint_identity(agent.pane.as_deref(), socket);
        return endpoint == key.window.session.endpoint.socket
            && muxa::backend::pane_id_host_kind(&key.pane_id)
                .is_none_or(|host| host == key.window.session.endpoint.host);
    }
    card.pane_keys
        .iter()
        .filter(|pane| pane.pane_id == key.pane_id)
        .count()
        == 1
}

fn open_work_composer(app: &mut DashboardApp) -> UiAction {
    let Some(card) = app.selected_card() else {
        app.set_hint("no Work selected", HintLevel::Err);
        return UiAction::None;
    };
    if !pane_write_supported(card.host) {
        let message = unsupported_pane_action(card.host, "prompting")
            .unwrap_or_else(|| "prompt unsupported".into());
        app.set_hint(message, HintLevel::Err);
        return UiAction::None;
    }
    let panes = live_work_panes(card);
    if panes.is_empty() {
        app.set_hint("selected Work has no live agent panes", HintLevel::Err);
        return UiAction::None;
    }
    let label = format!("{} · all {} agents", card.label, panes.len());
    app.composer = Some(PromptComposer::new(PromptTarget::WorkPanes(panes), label));
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
    if target.is_pane() && !pane_write_supported(host) {
        let message =
            unsupported_pane_action(host, "abort").unwrap_or_else(|| "abort unsupported".into());
        app.set_hint(message, HintLevel::Err);
        return UiAction::None;
    }
    let action = match target.clone() {
        ActionTarget::TopologyPane(pane) => PendingAction::PaneAbort(pane),
        ActionTarget::Pane(pane_id) => PendingAction::Quick(QuickAction::AbortTurn(pane_id)),
        ActionTarget::PtySession(session_id) => PendingAction::PtyCtrlC(session_id),
    };
    app.confirm = Some(ConfirmPopup {
        message: format!("Abort current turn on {} ({label})?", target.label()),
        on_confirm: action,
    });
    UiAction::None
}

fn confirm_work_abort(app: &mut DashboardApp) -> UiAction {
    let Some(card) = app.selected_card() else {
        return UiAction::None;
    };
    if !pane_write_supported(card.host) {
        let message = unsupported_pane_action(card.host, "abort")
            .unwrap_or_else(|| "abort unsupported".into());
        app.set_hint(message, HintLevel::Err);
        return UiAction::None;
    }
    let panes = live_work_panes(card);
    if panes.is_empty() {
        app.set_hint("selected Work has no live agent panes", HintLevel::Err);
        return UiAction::None;
    }
    app.confirm = Some(ConfirmPopup {
        message: format!(
            "Abort current turns on all {} agents in {}?",
            panes.len(),
            card.label
        ),
        on_confirm: PendingAction::WorkAbort { panes },
    });
    UiAction::None
}

fn confirm_kill(app: &mut DashboardApp) -> UiAction {
    let Some((host, label, target)) = selected_target_context(app) else {
        return UiAction::None;
    };
    if target.is_pane() && !pane_write_supported(host) {
        let message = unsupported_pane_action(host, "termination")
            .unwrap_or_else(|| "terminate unsupported".into());
        app.set_hint(message, HintLevel::Err);
        return UiAction::None;
    }
    let action = match target.clone() {
        ActionTarget::TopologyPane(pane) => {
            PendingAction::Quick(QuickAction::TerminateNode(TopologyNodeKey::Pane(pane)))
        }
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
        PendingAction::PanePrompt { pane, text } => {
            run_exact_pane_action(&pane, &text, true, "prompted")
        }
        PendingAction::PaneAbort(pane) => run_exact_pane_action(&pane, "\u{3}", false, "aborted"),
        PendingAction::WorkPrompt { panes, text } => {
            run_work_pane_actions(panes, &text, true, "prompted")
        }
        PendingAction::WorkAbort { panes } => {
            run_work_pane_actions(panes, "\u{3}", false, "aborted")
        }
        PendingAction::PtyPrompt { session_id, text } => {
            write_pty(
                client,
                session_id,
                format!("{text}\r"),
                "sent prompt to",
                "send",
            )
            .await
        }
        PendingAction::PtyCtrlC(session_id) => {
            write_pty(
                client,
                session_id,
                "\u{3}".into(),
                "sent Ctrl-C to",
                "abort",
            )
            .await
        }
        PendingAction::TerminatePty(session_id) => terminate_pty(client, session_id).await,
        PendingAction::CollaborationInbox { origin } => collaboration_inbox(client, origin).await,
        PendingAction::CollaborationSend {
            origin,
            target,
            kind,
            body,
            work_mode,
        } => collaboration_send(client, origin, target, kind, body, work_mode).await,
        PendingAction::CollaborationReply {
            origin,
            request_id,
            status,
            body,
        } => collaboration_reply(client, origin, request_id, status, body).await,
        PendingAction::CollaborationCancel { origin, request_id } => {
            collaboration_cancel(client, origin, request_id).await
        }
    }
}

async fn collaboration_inbox(client: &Client, origin: CollaborationOrigin) -> ActionOutcome {
    match client.collaboration_inbox(&origin).await {
        Ok(requests) if requests.is_empty() => {
            ActionOutcome::Ok("collaboration inbox is empty".into())
        }
        Ok(requests) => ActionOutcome::Ok(format!(
            "claimed {} collaboration request{}",
            requests.len(),
            if requests.len() == 1 { "" } else { "s" }
        )),
        Err(error) => ActionOutcome::Err(format!("inbox failed: {error}")),
    }
}

async fn collaboration_send(
    client: &Client,
    origin: CollaborationOrigin,
    target: String,
    kind: RequestKind,
    body: String,
    work_mode: WorkMode,
) -> ActionOutcome {
    let request = NewRequest {
        kind,
        body,
        expects_reply: kind != RequestKind::Notice,
        work_mode,
        paths: Vec::new(),
        air_artifacts: Vec::new(),
    };
    match client.collaboration_send(&origin, &target, &request).await {
        Ok(request) => ActionOutcome::Ok(format!(
            "sent {} to {} ({})",
            short_request_id(&request.id),
            request.to.label(),
            request_kind_label(kind)
        )),
        Err(error) => ActionOutcome::Err(format!("collaboration send failed: {error}")),
    }
}

async fn collaboration_reply(
    client: &Client,
    origin: CollaborationOrigin,
    request_id: String,
    status: RequestStatus,
    body: String,
) -> ActionOutcome {
    match client
        .collaboration_reply(&origin, &request_id, status, &body, &[], &[])
        .await
    {
        Ok(request) => ActionOutcome::Ok(format!(
            "replied to {} ({})",
            short_request_id(&request.id),
            request_status_label(status)
        )),
        Err(error) => ActionOutcome::Err(format!("collaboration reply failed: {error}")),
    }
}

async fn collaboration_cancel(
    client: &Client,
    origin: CollaborationOrigin,
    request_id: String,
) -> ActionOutcome {
    match client.collaboration_cancel(&origin, &request_id).await {
        Ok(request) => ActionOutcome::Ok(format!("cancelled {}", short_request_id(&request.id))),
        Err(error) => ActionOutcome::Err(format!("collaboration cancel failed: {error}")),
    }
}

async fn write_pty(
    client: &Client,
    session_id: String,
    data: String,
    success: &str,
    operation: &str,
) -> ActionOutcome {
    match client.write_session(&session_id, &data).await {
        Ok(()) => ActionOutcome::Ok(format!("{success} {session_id}")),
        Err(error) => ActionOutcome::Err(format!("{operation} failed: {error}")),
    }
}

async fn terminate_pty(client: &Client, session_id: String) -> ActionOutcome {
    match client.terminate_session(&session_id).await {
        Ok(()) => ActionOutcome::Ok(format!("terminated {session_id}")),
        Err(error) => ActionOutcome::Err(format!("terminate failed: {error}")),
    }
}

fn run_exact_pane_action(pane: &PaneKey, text: &str, submit: bool, verb: &str) -> ActionOutcome {
    match send_to_exact_pane(pane, text, submit) {
        Ok(()) => ActionOutcome::Ok(format!("{verb} pane {}", pane.pane_id)),
        Err(error) => ActionOutcome::Err(format!("{verb} failed: {error}")),
    }
}

fn run_work_pane_actions(
    panes: Vec<PaneKey>,
    text: &str,
    submit: bool,
    verb: &str,
) -> ActionOutcome {
    let attempted = panes.len();
    let mut succeeded = 0;
    let mut errors = Vec::new();
    for pane in panes {
        match send_to_exact_pane(&pane, text, submit) {
            Ok(()) => succeeded += 1,
            Err(error) => errors.push(format!("{}: {error}", pane.pane_id)),
        }
    }
    if errors.is_empty() {
        ActionOutcome::Ok(format!("{verb} {succeeded}/{attempted} Work agents"))
    } else {
        ActionOutcome::Err(format!(
            "{verb} {succeeded}/{attempted} Work agents · {}",
            errors.join("; ")
        ))
    }
}

fn send_to_exact_pane(pane: &PaneKey, text: &str, submit: bool) -> std::result::Result<(), String> {
    let endpoint = &pane.window.session.endpoint;
    let backend = muxa::active_backends()
        .into_iter()
        .find(|backend| backend.kind() == endpoint.host)
        .ok_or_else(|| format!("{} backend is not active", endpoint.host))?;
    if !backend.caps().send_text {
        return Err(format!("{} pane input is not supported", endpoint.host));
    }
    if !backend.send_text_on(Some(&endpoint.socket), &pane.pane_id, text) {
        return Err("input was rejected".into());
    }
    if submit {
        std::thread::sleep(muxa::backend::PROMPT_SUBMIT_GRACE);
        if !backend.send_text_on(Some(&endpoint.socket), &pane.pane_id, "\r") {
            return Err("prompt text was sent but submit failed".into());
        }
    }
    Ok(())
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

    if matches!(
        (&target, host),
        (
            CaptureTarget::TopologyPane(_) | CaptureTarget::Pane(_),
            CardHost::Zellij
        )
    ) {
        app.capture = CaptureCache {
            target: Some(target),
            text: None,
            message: Some("capture unsupported for zellij panes".into()),
            fetched_at: Some(Instant::now()),
        };
        return;
    }

    let text = match target.clone() {
        CaptureTarget::TopologyPane(pane) => tokio::task::spawn_blocking(move || {
            let endpoint = &pane.window.session.endpoint;
            muxa::active_backends()
                .into_iter()
                .find(|backend| backend.kind() == endpoint.host)
                .and_then(|backend| backend.capture_pane_on(Some(&endpoint.socket), &pane.pane_id))
        })
        .await
        .ok()
        .flatten(),
        CaptureTarget::Pane(pane_id) => {
            // Resolve the backend by the pane id's namespace (like the jump
            // path) so a herdr pane captures via herdr even when the
            // process-global host is tmux, and vice versa.
            tokio::task::spawn_blocking(move || {
                crate::backend_for_pane(&pane_id).capture_pane(&pane_id)
            })
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
        let popup = centered_rect_by_size(76, 25, area);
        f.render_widget(Clear, popup);
        render_help(f, popup, app.theme);
    }
    if app.overlay == Overlay::Collaboration {
        let popup = centered_rect_by_size(104, 24, area);
        f.render_widget(Clear, popup);
        render_collaboration_mailbox(f, popup, app);
    }
    if app.confirm.is_some() {
        let popup = centered_rect_by_size(60, 7, area);
        f.render_widget(Clear, popup);
        render_confirm(f, popup, app);
    }
    if app.composer.is_some() {
        let skills_open = app
            .composer
            .as_ref()
            .is_some_and(|composer| composer.skill_palette.is_some());
        let popup = message_composer_rect(area, skills_open);
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
        subtle_pill(format!("{} works", totals.works), app.theme),
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
    if let Some(room) = app.data.collaboration.room.as_ref() {
        spans.push(Span::raw("  "));
        spans.push(subtle_pill(
            format!("room {}", room.current.room.window_id),
            app.theme,
        ));
        if room.unread > 0 || room.unread_replies > 0 {
            spans.push(Span::raw(" "));
            spans.push(pill(
                format!("mail {}/{}", room.unread, room.unread_replies),
                Color::Black,
                app.theme.warn,
            ));
        }
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
        .title(Span::styled(" Work board ", app.theme.title_style()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.data.cards.is_empty() {
        let text = Text::from(Line::from(Span::styled(
            "No managed or persisted Work found. Start one with `muxa work up`.",
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
    let title_spans = vec![
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
    ];
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
    if let Some(work_id) = card.work_id.as_deref() {
        if card.label.eq_ignore_ascii_case(work_id) {
            return work_id.to_string();
        }
        return format!("{work_id} · {}", card.label);
    }
    let prefix = match card.host {
        CardHost::Tmux | CardHost::Rmux | CardHost::Zellij | CardHost::Herdr => "",
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
        subtle_pill(card.stage.map_or("execution", board_stage_label), theme),
        Span::raw(" "),
        Span::styled(
            format!("{} {}", icon_agent(), card.agents.len()),
            Style::default().fg(theme.panel),
        ),
    ];
    if !card.signals.is_empty() {
        spans.extend([
            Span::raw("  "),
            Span::styled(
                card.signals
                    .iter()
                    .copied()
                    .map(work_signal_label)
                    .collect::<Vec<_>>()
                    .join(","),
                Style::default().fg(if card.signals.contains(&WorkSignal::Error) {
                    theme.error
                } else {
                    theme.warn
                }),
            ),
        ]);
    }
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
    if let Some(item) = card.external_item.as_ref() {
        parts.push(format!(
            "{}:{}{}",
            item.source,
            item.display_key,
            item.status
                .as_deref()
                .map_or_else(String::new, |status| format!("/{status}"))
        ));
    } else if card.work_id.is_some() {
        parts.push("local work".into());
    }
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
        ActionTarget::TopologyPane(pane) => agent.pane.as_deref() == Some(&pane.pane_id),
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
        format!("{} work", icon_session()),
        vec![
            status_pill(card.status, app.theme),
            Span::raw(" "),
            Span::styled(card.label.clone(), app.theme.title_style()),
        ],
        vec![
            format!(
                "{} · {} runs",
                card.workspace.as_deref().unwrap_or("-"),
                card.run_count
            ),
            format!(
                "{} agents · {} panes",
                card.agents.len(),
                card.pane_ids.len()
            ),
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
                agent
                    .pane
                    .as_deref()
                    .and_then(|pane| app.data.collaboration.participant_for_pane(pane)),
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
    ];
    lines.extend(collaboration_inspector_lines(app, width));
    lines.push(Line::from(truncate_width(
        &format!("cwd {}", card.cwd.as_deref().unwrap_or("-")),
        width,
    )));
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
                agent
                    .pane
                    .as_deref()
                    .and_then(|pane| app.data.collaboration.participant_for_pane(pane)),
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

/// Who the dashboard is speaking as, and how much mail is waiting for them.
///
/// The unread pair counts what was addressed to the sender and what it has not
/// read back. Nothing is ever addressed to an operator console and it reads
/// replies off the selected card rather than through the daemon, so both are
/// permanently zero there — a fixed `unread 0/0` reads as "no mail anywhere".
fn room_identity(room: &RoomContext) -> String {
    if room.current.console {
        return format!("room {} · self console", room.current.room.window_id);
    }
    format!(
        "room {} · self {} · unread {}/{}",
        room.current.room.window_id,
        room.current.label(),
        room.unread,
        room.unread_replies
    )
}

fn collaboration_inspector_lines(app: &DashboardApp, width: usize) -> Vec<Line<'static>> {
    let Some(room) = app.data.collaboration.room.as_ref() else {
        return Vec::new();
    };
    let mut lines = vec![Line::from(truncate_width(&room_identity(room), width))];
    if let Some(peer) = app.selected_collaboration_peer() {
        lines.push(Line::from(truncate_width(
            &format!(
                "peer {} · roles {}",
                peer.label(),
                if peer.roles.is_empty() {
                    "-".into()
                } else {
                    peer.roles.join(",")
                }
            ),
            width,
        )));
    }
    lines
}

fn agent_roster_line(
    agent: &Agent,
    now: OffsetDateTime,
    width: usize,
    theme: DashboardTheme,
    primary: bool,
    collaboration_participant: Option<&Participant>,
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
    let collaboration = collaboration_participant.map_or_else(String::new, |participant| {
        let roles = if participant.roles.is_empty() {
            String::new()
        } else {
            format!(" [{}]", participant.roles.join(","))
        };
        format!(" · {}{roles}", participant.label())
    });
    let text = truncate_width(
        &format!(
            "{} {} · {}{} · {} · {}",
            agent.kind,
            agent.state,
            target,
            collaboration,
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
        key("m", app.theme),
        Span::raw(" message  "),
        key("b", app.theme),
        Span::raw(" mailbox  "),
        key("i", app.theme),
        Span::raw(" inbox  "),
        key("p/P", app.theme),
        Span::raw(" prompt target/all  "),
        key("R/A", app.theme),
        Span::raw(" abort target/all  "),
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
        Line::from("  P                 compose one prompt for all live Work agents"),
        Line::from("  m                 message selected same-room peer"),
        Line::from("  b                 open collaboration mailbox"),
        Line::from("  i                 claim collaboration inbox"),
        Line::from("  c                 copy last prompt"),
        Line::from("  R                 abort current turn"),
        Line::from("  A                 abort all live turns in selected Work"),
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

#[allow(clippy::too_many_lines)]
fn render_collaboration_mailbox(f: &mut Frame, area: Rect, app: &DashboardApp) {
    let tab = app.collaboration_mailbox.tab;
    let incoming_count = app.data.collaboration.incoming.len();
    let sent_count = app.data.collaboration.sent.len();
    let title = Line::from(vec![
        Span::styled(" collaboration ", app.theme.title_style()),
        Span::styled(
            format!(" incoming {incoming_count} "),
            if tab == CollaborationTab::Incoming {
                app.theme.key_style()
            } else {
                app.theme.dim_style()
            },
        ),
        Span::raw(" "),
        Span::styled(
            format!(" sent {sent_count} "),
            if tab == CollaborationTab::Sent {
                app.theme.key_style()
            } else {
                app.theme.dim_style()
            },
        ),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.selected_border())
        .border_type(BorderType::Plain)
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.data.collaboration.room.is_none() {
        let message = app
            .data
            .collaboration
            .unavailable
            .as_deref()
            .unwrap_or("collaboration is unavailable");
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    message.to_string(),
                    Style::default()
                        .fg(app.theme.error)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled("Esc/b closes", app.theme.dim_style())),
            ]),
            inner,
        );
        return;
    }

    let has_air = app.selected_collaboration_request().is_some_and(|request| {
        !request.air_artifacts.is_empty()
            || request
                .reply
                .as_ref()
                .is_some_and(|reply| !reply.air_artifacts.is_empty())
    });
    let detail_height = if has_air { 9 } else { 7 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(detail_height),
            Constraint::Length(1),
        ])
        .split(inner);
    let requests = app.collaboration_requests();
    let width = usize::from(chunks[0].width).max(1);
    let max_lines = usize::from(chunks[0].height).max(1);
    let selected = app.collaboration_mailbox.selected;
    let start = selected
        .saturating_add(1)
        .saturating_sub(max_lines)
        .min(requests.len().saturating_sub(max_lines));
    let mut lines = requests
        .iter()
        .enumerate()
        .skip(start)
        .take(max_lines)
        .map(|(index, request)| {
            let focused = index == selected;
            let peer = match tab {
                CollaborationTab::Incoming => request.from.label(),
                CollaborationTab::Sent => request.to.label(),
            };
            let air_badge = request.air_artifacts.first();
            let air_width = air_badge.map_or(0, |reference| {
                reference.profile.label().len().saturating_add(3)
            });
            let text = truncate_width(
                &format!(
                    "{} {:<11} {:<9} {:<12} {:<9} {}",
                    short_request_id(&request.id),
                    request_kind_label(request.kind),
                    request_status_label(request.status),
                    peer,
                    work_mode_label(request.work_mode),
                    squash_ws(&request.body)
                ),
                width.saturating_sub(2).saturating_sub(air_width),
            );
            let mut spans = vec![Span::styled(
                if focused { "> " } else { "  " },
                if focused {
                    app.theme.selected_border()
                } else {
                    app.theme.dim_style()
                },
            )];
            if let Some(reference) = air_badge {
                spans.push(dashboard_air_artifact_badge(reference));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                text,
                if focused {
                    Style::default()
                        .fg(app.theme.title)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(app.theme.panel)
                },
            ));
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            match tab {
                CollaborationTab::Incoming => "no incoming requests",
                CollaborationTab::Sent => "no sent requests",
            },
            app.theme.dim_style(),
        )));
    }
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let detail_block = Block::default()
        .borders(Borders::TOP)
        .border_style(app.theme.border_style())
        .title(Span::styled(" selected request ", app.theme.dim_style()));
    let detail_width = usize::from(detail_block.inner(chunks[1]).width).max(1);
    let detail_lines = app.selected_collaboration_request().map_or_else(
        || vec![Line::from(Span::styled("-", app.theme.dim_style()))],
        |request| collaboration_request_detail(request, detail_width, app.theme),
    );
    f.render_widget(Paragraph::new(detail_lines).block(detail_block), chunks[1]);

    let help = app.data.collaboration.unavailable.as_deref().map_or_else(
        || match tab {
            CollaborationTab::Incoming => {
                "Tab mailbox · ↑↓ select · i claim inbox · e reply · Esc/b close"
            }
            CollaborationTab::Sent => "Tab mailbox · ↑↓ select · x cancel queued · Esc/b close",
        },
        |error| error,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(help, app.theme.dim_style()))),
        chunks[2],
    );
}

fn collaboration_request_detail(
    request: &CollaborationRequest,
    width: usize,
    theme: DashboardTheme,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(truncate_width(
            &format!(
                "{} · {} → {}",
                request.id,
                request.from.label(),
                request.to.label()
            ),
            width,
        )),
        Line::from(truncate_width(
            &format!(
                "{} · {} · reply {} · {}",
                request_kind_label(request.kind),
                request_status_label(request.status),
                if request.expects_reply { "yes" } else { "no" },
                work_mode_label(request.work_mode)
            ),
            width,
        )),
        Line::from(truncate_width(
            &format!("body: {}", squash_ws(&request.body)),
            width,
        )),
    ];
    if !request.paths.is_empty() {
        lines.push(Line::from(truncate_width(
            &format!("paths: {}", request.paths.join(", ")),
            width,
        )));
    }
    lines.extend(
        request
            .air_artifacts
            .iter()
            .map(|reference| dashboard_air_artifact_detail_line("input", reference, width)),
    );
    if let Some(reply) = request.reply.as_ref() {
        lines.push(Line::from(Span::styled(
            truncate_width(
                &format!(
                    "reply [{}]: {}",
                    request_status_label(reply.status),
                    squash_ws(&reply.body)
                ),
                width,
            ),
            Style::default().fg(theme.ok),
        )));
        lines.extend(
            reply
                .air_artifacts
                .iter()
                .map(|reference| dashboard_air_artifact_detail_line("output", reference, width)),
        );
    }
    lines.truncate(8);
    lines
}

fn dashboard_air_artifact_badge(reference: &AirArtifactReference) -> Span<'static> {
    let (foreground, background) = match reference.profile {
        AirArtifactProfile::WorkflowSkill => (Color::White, Color::Blue),
        AirArtifactProfile::PlanNativeCli => (Color::White, Color::Magenta),
        AirArtifactProfile::TraceNativeRun => (Color::Black, Color::Cyan),
        AirArtifactProfile::TraceSessionSnapshot => (Color::Black, Color::LightCyan),
    };
    Span::styled(
        format!(" {} ", reference.profile.label()),
        Style::default()
            .fg(foreground)
            .bg(background)
            .add_modifier(Modifier::BOLD),
    )
}

fn dashboard_air_artifact_detail_line(
    direction: &str,
    reference: &AirArtifactReference,
    width: usize,
) -> Line<'static> {
    let short_id = reference
        .artifact_id
        .strip_prefix("urn:air:sha256:")
        .unwrap_or(&reference.artifact_id)
        .chars()
        .take(12)
        .collect::<String>();
    let label = reference.label.as_deref().unwrap_or("-");
    let locator = reference
        .locator
        .as_ref()
        .map_or("", |locator| locator.display.as_str());
    Line::from(vec![
        dashboard_air_artifact_badge(reference),
        Span::raw(truncate_width(
            &format!(" {direction} · {short_id} · {label} · {locator}"),
            width.saturating_sub(reference.profile.label().len() + 2),
        )),
    ])
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
    if let Some(palette) = composer.skill_palette.as_ref() {
        render_message_skill_palette(f, area, app, composer, palette);
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.selected_border())
        .border_type(BorderType::Plain)
        .title(Span::styled(
            format!(
                " {} → {} · / skills ",
                composer_title(composer),
                composer.label
            ),
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

fn render_message_skill_palette(
    f: &mut Frame,
    area: Rect,
    app: &DashboardApp,
    composer: &PromptComposer,
    palette: &MessageSkillPalette,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.selected_border())
        .border_type(BorderType::Plain)
        .title(Span::styled(
            format!(
                " skills → {} · Enter insert · ↑/↓ select · Esc back ",
                composer.label
            ),
            app.theme.title_style(),
        ));
    let inner = block.inner(area);
    let visible = visible_input(
        &palette.query,
        palette.query.chars().count(),
        inner.width.saturating_sub(2),
    );
    let matches = matching_skills(&app.message_skills, &palette.query);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("/ ", app.theme.key_style()),
            Span::raw(visible.text.clone()),
        ]),
        Line::from(""),
    ];
    let available = usize::from(inner.height).saturating_sub(lines.len());
    if app.message_skills.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no skills · muxa skill add <name> <prompt>",
            app.theme.dim_style(),
        )));
    } else if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching skill",
            app.theme.dim_style(),
        )));
    } else {
        let start = palette
            .selected
            .saturating_add(1)
            .saturating_sub(available.max(1));
        for (offset, (name, prompt)) in matches.into_iter().skip(start).take(available).enumerate()
        {
            let index = start + offset;
            let name_style = if index == palette.selected {
                app.theme.key_style()
            } else {
                app.theme.title_style()
            };
            let prefix = format!("  /{name}");
            let prompt_width = usize::from(inner.width)
                .saturating_sub(prefix.width())
                .saturating_sub(3);
            lines.push(Line::from(vec![
                Span::styled(prefix, name_style),
                Span::raw("  "),
                Span::styled(
                    truncate_width(prompt.lines().next().unwrap_or_default(), prompt_width),
                    app.theme.dim_style(),
                ),
            ]));
        }
    }
    f.render_widget(Paragraph::new(lines).block(block), area);

    let cursor_x = inner
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(visible.text.width()).unwrap_or(u16::MAX));
    if inner.height > 0 && cursor_x < inner.x.saturating_add(inner.width) {
        f.set_cursor_position((cursor_x, inner.y));
    }
}

fn composer_title(composer: &PromptComposer) -> String {
    match &composer.target {
        PromptTarget::TopologyPane(_) | PromptTarget::Pane(_) | PromptTarget::PtySession(_) => {
            "prompt".into()
        }
        PromptTarget::WorkPanes(panes) => format!("Work prompt · {} live agents", panes.len()),
        PromptTarget::CollaborationSend {
            kind, work_mode, ..
        } => format!(
            "message · {} · {} · Tab kind · Ctrl-E mode",
            request_kind_label(*kind),
            work_mode_label(*work_mode)
        ),
        PromptTarget::CollaborationReply { status, .. } => {
            format!("reply · {} · Tab status", request_status_label(*status))
        }
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

fn message_composer_rect(area: Rect, skills_open: bool) -> Rect {
    if !skills_open {
        return bottom_prompt_rect(area);
    }
    let height = area.height.min(12);
    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height),
        width: area.width,
        height,
    }
}

fn capture_target_label(target: &CaptureTarget) -> String {
    match target {
        CaptureTarget::TopologyPane(pane) => format!(
            "{} pane {}",
            pane.window.session.endpoint.socket, pane.pane_id
        ),
        CaptureTarget::Pane(pane) => format!("pane {pane}"),
        CaptureTarget::PtySession(session) => format!("pty {session}"),
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

fn work_mode_label(mode: WorkMode) -> &'static str {
    match mode {
        WorkMode::ReadOnly => "read-only",
        WorkMode::Execute => "execute",
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

fn short_request_id(request_id: &str) -> String {
    const MAX_CHARS: usize = 18;
    let chars = request_id.chars().collect::<Vec<_>>();
    if chars.len() <= MAX_CHARS {
        request_id.to_string()
    } else {
        chars[..MAX_CHARS].iter().collect()
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
    use muxa::collaboration::RoomId;
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
            subagents: Vec::new(),
            state,
            last_prompt: prompt.map(str::to_string),
            last_response: None,
            recap: None,
            ai_title: None,
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
            agent_role: None,
            agent_alias: None,
            socket: None,
            pane_id: pane_id.to_string(),
            session_id: String::new(),
            session: session.to_string(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: "1".into(),
            pane_index: "0".into(),
            tty: "/dev/pts/1".into(),
            current_command: "zsh".into(),
            title: "shell".into(),
            pane_pid: 123,
            current_path: String::new(),
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

    fn fake_participant(
        pane: &str,
        session_id: &str,
        alias: Option<&str>,
        roles: &[&str],
    ) -> Participant {
        Participant {
            agent_kind: AgentKind::Codex,
            agent_session_id: session_id.into(),
            pane: pane.into(),
            socket: Some("default".into()),
            room: RoomId {
                host: "tmux".into(),
                socket: Some("default".into()),
                window_id: "@1".into(),
            },
            tmux_session_id: Some("$1".into()),
            tmux_session_name: Some("main".into()),
            window_name: Some("agents".into()),
            state: AgentState::Idle,
            cwd: Some("/tmp/project".into()),
            alias: alias.map(str::to_string),
            roles: roles.iter().map(|role| (*role).to_string()).collect(),
            console: false,
        }
    }

    fn fake_collaboration_request(
        id: &str,
        from: Participant,
        to: Participant,
        status: RequestStatus,
        now: OffsetDateTime,
    ) -> CollaborationRequest {
        CollaborationRequest {
            id: id.into(),
            from,
            to,
            provenance: None,
            kind: RequestKind::Review,
            body: "review the auth change".into(),
            expects_reply: true,
            work_mode: WorkMode::ReadOnly,
            paths: Vec::new(),
            air_artifacts: Vec::new(),
            status,
            created_at: now,
            claimed_at: (status == RequestStatus::Claimed).then_some(now),
            wake_delivery: None,
            notified_at: None,
            reply_notified_at: None,
            reply_read_at: None,
            reply: None,
        }
    }

    fn attach_collaboration(
        data: &mut DashboardData,
        current: Participant,
        peers: Vec<Participant>,
    ) {
        // The console dispatches; the mailbox on screen belongs to the card
        // under the cursor, which these fixtures park on `current`.
        let inbox = CollaborationAnchor {
            origin: CollaborationOrigin {
                pane: current.pane.clone(),
                socket: None,
                console: false,
            },
            label: current.label(),
        };
        data.collaboration = CollaborationData {
            origin: Some(CollaborationOrigin {
                pane: current.pane.clone(),
                socket: current.socket.clone(),
                console: true,
            }),
            room: Some(RoomContext {
                current,
                peers,
                unread: 0,
                unread_replies: 0,
            }),
            inbox: Some(inbox),
            ..CollaborationData::default()
        };
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
    fn dashboard_slash_palette_inserts_without_sending() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let mut app = DashboardApp::new(data, WatchTheme::Classic);
        app.message_skills
            .insert("review".into(), "ask codex to review our changes".into());
        app.composer = Some(PromptComposer::new(
            PromptTarget::Pane("%1".into()),
            "pane %1".into(),
        ));

        assert!(matches!(
            handle_composer_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)
            ),
            UiAction::None
        ));
        assert!(matches!(
            handle_composer_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiAction::None
        ));
        assert_eq!(
            app.composer
                .as_ref()
                .map(|composer| composer.input.as_str()),
            Some("ask codex to review our changes")
        );
        assert!(matches!(
            handle_composer_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            UiAction::Run(PendingAction::Quick(QuickAction::SendPrompt { .. }))
        ));
    }

    #[test]
    fn dashboard_skill_insertion_preserves_the_existing_draft() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let mut app = DashboardApp::new(data, WatchTheme::Classic);
        app.message_skills
            .insert("review".into(), "review the current changes".into());
        let mut composer = PromptComposer::new(PromptTarget::Pane("%1".into()), "pane %1".into());
        composer.input = "Keep this context.".into();
        composer.cursor = composer.input.chars().count();
        app.composer = Some(composer);

        let _ = handle_composer_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        );
        let _ = handle_composer_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            app.composer
                .as_ref()
                .map(|composer| composer.input.as_str()),
            Some("Keep this context.\n\nreview the current changes")
        );
    }

    #[test]
    fn dashboard_slash_palette_renders_registered_skill() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let mut app = DashboardApp::new(data, WatchTheme::Classic);
        app.message_skills
            .insert("agent-review".into(), "ask codex for a review".into());
        let mut composer = PromptComposer::new(PromptTarget::Pane("%1".into()), "pane %1".into());
        composer.skill_palette = Some(MessageSkillPalette::default());
        app.composer = Some(composer);
        let backend = TestBackend::new(96, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(screen.contains("skills"));
        assert!(screen.contains("/agent-review"));
        assert!(screen.contains("Enter insert"));
    }

    #[test]
    fn automatic_refresh_preserves_existing_footer_hint() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let data = build_dashboard_data(
            now,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let mut app = DashboardApp::new(data.clone(), WatchTheme::Classic);
        app.set_hint("request sent", HintLevel::Ok);

        apply_refresh_data(&mut app, data.clone(), RefreshSource::Automatic, None);
        assert_eq!(
            app.hint.as_ref().map(|hint| hint.message.as_str()),
            Some("request sent")
        );

        apply_refresh_data(&mut app, data, RefreshSource::Manual, None);
        assert_eq!(
            app.hint.as_ref().map(|hint| hint.message.as_str()),
            Some("refreshed")
        );
    }

    /// The dashboard sends as the console, so the pane it was opened from is
    /// an ordinary peer rather than "self", and a launch pane the backend
    /// cannot resolve still yields a console instead of killing collaboration.
    #[test]
    fn collaboration_origin_is_a_console_that_carries_the_launch_pane() {
        let origin = dashboard_collaboration_origin_from(
            Some("%9".into()),
            None,
            Some("/tmp/tmux-1000/custom,42,7".into()),
        );
        assert!(origin.console);
        assert_eq!(origin.pane, "%9");

        let paneless = dashboard_collaboration_origin_from(None, None, None);
        assert!(paneless.console);
        assert!(paneless.pane.is_empty());
    }

    /// A refresh spawned before the cursor moved carries the previous card's
    /// inbox. Refreshes land every second, so applying one blindly would swap
    /// the open mailbox for another agent's while the selection index stays
    /// put — and `e` would then reply to a request the operator never saw.
    #[test]
    fn a_refresh_anchored_to_another_card_does_not_replace_the_open_mailbox() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let build = || {
            build_dashboard_data(
                now,
                vec![fake_agent("self", Some("%1"), AgentState::Idle, None, now)],
                vec![fake_pane("%1", "main")],
                Vec::new(),
                Vec::new(),
                SessionActiveStats::default(),
                false,
                DashboardSort::Attention,
                HostKind::Tmux,
                Vec::new(),
            )
        };
        let current = fake_participant("%1", "self", Some("builder"), &[]);
        let peer = fake_participant("%2", "peer", Some("reviewer"), &[]);

        let mut on_screen = build();
        attach_collaboration(&mut on_screen, current.clone(), vec![peer.clone()]);
        on_screen
            .collaboration
            .incoming
            .push(fake_collaboration_request(
                "req_on_screen_1234",
                peer.clone(),
                current.clone(),
                RequestStatus::Claimed,
                now,
            ));
        let mut app = DashboardApp::new(on_screen, WatchTheme::Classic);

        // A refresh that was spawned while the cursor sat on another card.
        let mut stale = build();
        attach_collaboration(&mut stale, current.clone(), vec![peer.clone()]);
        stale.collaboration.inbox = Some(CollaborationAnchor {
            origin: CollaborationOrigin {
                pane: "%9".into(),
                socket: None,
                console: false,
            },
            label: "codex@%9".into(),
        });
        stale
            .collaboration
            .incoming
            .push(fake_collaboration_request(
                "req_other_card_1234",
                peer.clone(),
                current.clone(),
                RequestStatus::Claimed,
                now,
            ));
        stale.collaboration.sent.push(fake_collaboration_request(
            "req_new_sent_123456",
            current,
            peer,
            RequestStatus::Queued,
            now,
        ));

        let loaded_anchor = stale.collaboration.inbox.clone();
        apply_refresh_data(
            &mut app,
            stale,
            RefreshSource::Automatic,
            loaded_anchor.as_ref(),
        );

        assert_eq!(
            app.data
                .collaboration
                .incoming
                .iter()
                .map(|request| request.id.as_str())
                .collect::<Vec<_>>(),
            vec!["req_on_screen_1234"],
            "a refresh anchored elsewhere must not replace the open mailbox"
        );
        assert_eq!(
            app.data
                .collaboration
                .inbox
                .as_ref()
                .map(|anchor| anchor.origin.pane.as_str()),
            Some("%1")
        );
        assert_eq!(
            app.data
                .collaboration
                .sent
                .iter()
                .map(|request| request.id.as_str())
                .collect::<Vec<_>>(),
            vec!["req_new_sent_123456"],
            "a stale inbox must not freeze console-wide sent mail"
        );
    }

    #[test]
    fn a_failed_current_inbox_load_clears_the_previous_cards_mailbox() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let build = || {
            build_dashboard_data(
                now,
                vec![fake_agent("self", Some("%1"), AgentState::Idle, None, now)],
                vec![fake_pane("%1", "main")],
                Vec::new(),
                Vec::new(),
                SessionActiveStats::default(),
                false,
                DashboardSort::Attention,
                HostKind::Tmux,
                Vec::new(),
            )
        };
        let current = fake_participant("%1", "self", Some("builder"), &[]);
        let peer = fake_participant("%2", "peer", Some("reviewer"), &[]);
        let mut on_screen = build();
        attach_collaboration(&mut on_screen, current.clone(), vec![peer.clone()]);
        on_screen
            .collaboration
            .incoming
            .push(fake_collaboration_request(
                "req_previous_card_1234",
                peer,
                current,
                RequestStatus::Claimed,
                now,
            ));
        let mut app = DashboardApp::new(on_screen, WatchTheme::Classic);

        // The request was made for the current card, but that pane stopped
        // being a collaboration participant before the daemon answered.
        let requested = dashboard_mailbox_anchor(&app).expect("current pane anchor");
        let mut refreshed = build();
        refreshed.collaboration.origin = app.data.collaboration.origin.clone();
        refreshed.collaboration.room = app.data.collaboration.room.clone();
        refreshed.collaboration.inbox = None;
        refreshed.collaboration.incoming.clear();

        apply_refresh_data(
            &mut app,
            refreshed,
            RefreshSource::Automatic,
            Some(&requested),
        );

        assert!(app.data.collaboration.inbox.is_none());
        assert!(app.data.collaboration.incoming.is_empty());
    }

    #[test]
    fn dashboard_mailbox_anchor_preserves_the_selected_agents_socket() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut agent = fake_agent("self", Some("%1"), AgentState::Idle, None, now);
        agent.tmux_socket = Some("/tmp/tmux-1000/custom".into());
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
        let app = DashboardApp::new(data, WatchTheme::Classic);

        let anchor = dashboard_mailbox_anchor(&app).expect("selected agent anchor");
        assert_eq!(anchor.origin.pane, "%1");
        assert_eq!(anchor.origin.socket.as_deref(), Some("custom"));
    }

    /// Replying is the recipient's move. The console can never be one, so `e`
    /// must speak for the agent whose inbox produced the listed request —
    /// sourcing it from the origin would be rejected as a non-participant.
    #[test]
    fn collaboration_reply_speaks_for_the_anchored_agent_not_the_console() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut data = build_dashboard_data(
            now,
            vec![fake_agent("self", Some("%1"), AgentState::Idle, None, now)],
            vec![fake_pane("%1", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let current = fake_participant("%1", "self", Some("builder"), &[]);
        let peer = fake_participant("%2", "peer", Some("reviewer"), &[]);
        attach_collaboration(&mut data, current.clone(), vec![peer.clone()]);
        data.collaboration.incoming.push(fake_collaboration_request(
            "req_incoming_123456",
            peer,
            current,
            RequestStatus::Claimed,
            now,
        ));
        let mut app = DashboardApp::new(data, WatchTheme::Classic);
        app.collaboration_mailbox.tab = CollaborationTab::Incoming;

        assert!(matches!(
            open_collaboration_reply_composer(&mut app),
            UiAction::None
        ));
        assert!(
            matches!(
                app.composer.as_ref().map(|composer| &composer.target),
                Some(PromptTarget::CollaborationReply { origin, .. })
                    if origin.pane == "%1" && !origin.console
            ),
            "reply must come from the mailbox's agent, got {:?}",
            app.composer.as_ref().map(|composer| &composer.target)
        );
    }

    #[test]
    fn collaboration_origin_uses_tmux_pane_and_short_socket() {
        let origin = dashboard_collaboration_origin_from(
            Some("%9".into()),
            None,
            Some("/tmp/tmux-1000/custom,42,7".into()),
        );

        assert_eq!(origin.pane, "%9");
        assert_eq!(origin.socket.as_deref(), Some("custom"));
    }

    #[test]
    fn collaboration_origin_prefers_nonempty_tmux_pane_env() {
        let origin = dashboard_collaboration_origin_from(
            Some("%3".into()),
            None,
            Some("/tmp/tmux-1000/default,42,7".into()),
        );

        assert_eq!(origin.pane, "%3");
        assert_eq!(origin.socket.as_deref(), Some("default"));
    }

    #[test]
    fn collaboration_origin_preserves_rmux_endpoint() {
        let origin = dashboard_collaboration_origin_from(
            Some("rmux:%3".into()),
            Some("/tmp/rmux-1000/default,42,7".into()),
            Some("/tmp/rmux-compat,42,7".into()),
        );

        assert_eq!(origin.pane, "rmux:%3");
        assert_eq!(origin.socket.as_deref(), Some("/tmp/rmux-1000/default"));
    }

    #[test]
    fn collaboration_error_explains_the_single_user_action() {
        assert_eq!(
            friendly_collaboration_error(
                "collaboration origin is not a hook-correlated tracked pane agent: %12"
            ),
            "muxad is too old to accept console messages — restart it after `muxa upgrade`"
        );
        assert_eq!(
            friendly_collaboration_error("agent collaboration is disabled"),
            "agent collaboration is disabled"
        );
    }

    #[test]
    fn collaboration_composer_targets_selected_same_room_peer() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut data = build_dashboard_data(
            now,
            vec![
                fake_agent("self", Some("%1"), AgentState::Idle, None, now),
                fake_agent("peer", Some("%2"), AgentState::WaitingInput, None, now),
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
        attach_collaboration(
            &mut data,
            fake_participant("%1", "self", Some("builder"), &["rust"]),
            vec![fake_participant(
                "%2",
                "peer",
                Some("reviewer"),
                &["review"],
            )],
        );
        let mut app = DashboardApp::new(data, WatchTheme::Classic);

        assert!(matches!(
            open_collaboration_composer(&mut app),
            UiAction::None
        ));
        let composer = app.composer.as_mut().unwrap();
        assert_eq!(composer.label, "reviewer@%2");
        assert!(matches!(
            composer.target,
            PromptTarget::CollaborationSend {
                ref target,
                kind: RequestKind::Question,
                work_mode: WorkMode::ReadOnly,
                ..
            } if target == "pane:%2"
        ));
        cycle_composer_option(composer);
        toggle_composer_execute(composer);
        assert!(matches!(
            composer.target,
            PromptTarget::CollaborationSend {
                kind: RequestKind::Review,
                work_mode: WorkMode::Execute,
                ..
            }
        ));
    }

    #[test]
    fn collaboration_composer_explains_when_the_room_has_no_peer() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut data = build_dashboard_data(
            now,
            vec![fake_agent("self", Some("%1"), AgentState::Idle, None, now)],
            vec![fake_pane("%1", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        attach_collaboration(
            &mut data,
            fake_participant("%1", "self", Some("builder"), &[]),
            Vec::new(),
        );
        let mut app = DashboardApp::new(data, WatchTheme::Classic);

        assert!(matches!(
            open_collaboration_composer(&mut app),
            UiAction::None
        ));
        assert_eq!(
            app.hint.as_ref().map(|hint| hint.message.as_str()),
            Some("no agent in this tmux window — the room is the window the dashboard was opened from")
        );
    }

    #[test]
    fn collaboration_inbox_reply_and_cancel_actions_preserve_origin() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut data = build_dashboard_data(
            now,
            vec![fake_agent("self", Some("%1"), AgentState::Idle, None, now)],
            vec![fake_pane("%1", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let current = fake_participant("%1", "self", Some("builder"), &["rust"]);
        let peer = fake_participant("%2", "peer", Some("reviewer"), &["review"]);
        attach_collaboration(&mut data, current.clone(), vec![peer.clone()]);
        data.collaboration.incoming.push(fake_collaboration_request(
            "req_incoming_123456",
            peer.clone(),
            current.clone(),
            RequestStatus::Claimed,
            now,
        ));
        data.collaboration.sent.push(fake_collaboration_request(
            "req_sent_123456",
            current,
            peer,
            RequestStatus::Queued,
            now,
        ));
        let mut app = DashboardApp::new(data, WatchTheme::Classic);

        assert!(matches!(
            claim_collaboration_inbox(&mut app),
            UiAction::Run(PendingAction::CollaborationInbox {
                origin: CollaborationOrigin { ref pane, .. }
            }) if pane == "%1"
        ));
        assert_eq!(app.overlay, Overlay::Collaboration);
        assert!(matches!(
            open_collaboration_reply_composer(&mut app),
            UiAction::None
        ));
        assert!(matches!(
            app.composer.as_ref().map(|composer| &composer.target),
            Some(PromptTarget::CollaborationReply { request_id, .. })
                if request_id == "req_incoming_123456"
        ));

        app.composer = None;
        app.collaboration_mailbox.tab = CollaborationTab::Sent;
        assert!(matches!(
            confirm_collaboration_cancel(&mut app),
            UiAction::None
        ));
        assert!(matches!(
            app.confirm.as_ref().map(|popup| &popup.on_confirm),
            Some(PendingAction::CollaborationCancel { request_id, .. })
                if request_id == "req_sent_123456"
        ));
    }

    #[test]
    fn collaboration_mailbox_renders_request_and_peer_identity() {
        let now = datetime!(2026-06-16 00:00 UTC);
        let mut data = build_dashboard_data(
            now,
            vec![fake_agent("self", Some("%1"), AgentState::Idle, None, now)],
            vec![fake_pane("%1", "main")],
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            false,
            DashboardSort::Attention,
            HostKind::Tmux,
            Vec::new(),
        );
        let current = fake_participant("%1", "self", Some("builder"), &["rust"]);
        let peer = fake_participant("%2", "peer", Some("reviewer"), &["review"]);
        attach_collaboration(&mut data, current.clone(), vec![peer.clone()]);
        let mut request = fake_collaboration_request(
            "req_render_123456",
            peer,
            current,
            RequestStatus::Claimed,
            now,
        );
        request.air_artifacts.push(AirArtifactReference {
            artifact_id: format!("urn:air:sha256:{}", "a".repeat(64)),
            profile: AirArtifactProfile::PlanNativeCli,
            label: Some("CAL-6924 execution plan".into()),
            locator: None,
        });
        data.collaboration.incoming.push(request);
        let backend = TestBackend::new(104, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = DashboardApp::new(data, WatchTheme::Classic);

        terminal
            .draw(|f| render_collaboration_mailbox(f, f.area(), &app))
            .unwrap();
        let dump = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(dump.contains("incoming 1"));
        assert!(dump.contains("reviewer@%2"));
        assert!(dump.contains("AIR PLAN"));
        assert!(dump.contains("aaaaaaaaaaaa"));
        assert!(dump.contains("review the auth change"));
        assert!(dump.contains("e reply"));
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
    fn selected_card_preserves_text_layout_and_renders_rate_limit_hint() {
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

        let backend = TestBackend::new(96, 8);
        let mut unselected_terminal = Terminal::new(backend).unwrap();
        unselected_terminal
            .draw(|f| {
                render_card(f, f.area(), app.selected_card().unwrap(), false, &app);
            })
            .unwrap();
        let unselected_dump = unselected_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(!dump.contains("FOCUS"));
        assert_eq!(dump, unselected_dump);
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
        let mut data = build_dashboard_data(
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
        attach_collaboration(
            &mut data,
            fake_participant("%1", "a", Some("builder"), &["rust"]),
            vec![fake_participant("%2", "b", Some("reviewer"), &["review"])],
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
        assert!(dump.contains("reviewer@%2"));
        assert!(dump.contains("roles review"));
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
    fn work_dashboard_keeps_work_external_issue_and_execution_separate() {
        let now = datetime!(2026-08-24 00:00 UTC);
        let identity = muxa::work::WorkIdentity::new("muxa", "dashboard-v2");
        let snapshot = WorkSnapshot {
            schema_version: muxa::work::WORK_SCHEMA_VERSION,
            generated_at: now,
            workspaces: Vec::new(),
            works: vec![muxa::work::WorkSnapshotItem {
                identity: identity.clone(),
                title: "Rebuild dashboard".into(),
                goal: None,
                next_action: None,
                stage: BoardStage::InProgress,
                signals: vec![WorkSignal::Attention],
                external_items: vec![ExternalItemRef {
                    source: "linear".into(),
                    scope: Some("CAL".into()),
                    stable_id: Some("linear-1".into()),
                    display_key: "CAL-7093".into(),
                    title: Some("Dashboard".into()),
                    url: Some("https://linear.app/example/CAL-7093".into()),
                    status: Some("started".into()),
                    item_type: Some("issue".into()),
                    synced_at: now,
                }],
                runs: Vec::new(),
                participants: 0,
                latest_at: None,
                source: muxa::work::WorkSource::Persisted,
                metadata: muxa::work::WorkMetadata {
                    title: Some("Rebuild dashboard".into()),
                    goal: None,
                    next_action: None,
                    stage: muxa::work::WorkStage::InProgress,
                    updated_at: now,
                },
            }],
            unlinked_executions: vec![muxa::work::RunSnapshot {
                id: "tmux:@9".into(),
                state: muxa::work::RunState::Idle,
                linked: false,
                work: None,
                execution: muxa::work::ExecutionIdentity {
                    host: HostKind::Tmux,
                    socket: "default".into(),
                    session_id: "$9".into(),
                    window_id: "@9".into(),
                },
                session_name: "scratch".into(),
                window_name: "shell".into(),
                window_index: "0".into(),
                cwd: Some("/tmp".into()),
                panes: Vec::new(),
                latest_at: None,
            }],
        };

        let data = build_work_dashboard_data(
            now,
            snapshot,
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            DashboardSort::Attention,
            Vec::new(),
        );

        assert_eq!(data.cards.len(), 1);
        assert_eq!(data.cards[0].workspace.as_deref(), Some("muxa"));
        assert_eq!(data.cards[0].work_id.as_deref(), Some("dashboard-v2"));
        assert_eq!(data.cards[0].stage, Some(BoardStage::InProgress));
        assert_eq!(
            data.cards[0]
                .external_item
                .as_ref()
                .map(|item| item.display_key.as_str()),
            Some("CAL-7093")
        );
        assert_eq!(data.totals.works, 1);
        assert_eq!(data.totals.attention, 1);
        assert!(data.notes[0].contains("1 unlinked executions hidden"));
    }

    #[test]
    fn work_dashboard_controls_keep_the_exact_execution_endpoint() {
        let now = datetime!(2026-08-24 00:00 UTC);
        let identity = muxa::work::WorkIdentity::new("muxa", "dashboard-v2");
        let mut agent = fake_agent(
            "agent-1",
            Some("%1"),
            AgentState::Working,
            Some("review controls"),
            now,
        );
        agent.tmux_socket = Some("alpha".into());
        let snapshot = WorkSnapshot {
            schema_version: muxa::work::WORK_SCHEMA_VERSION,
            generated_at: now,
            workspaces: Vec::new(),
            works: vec![muxa::work::WorkSnapshotItem {
                identity: identity.clone(),
                title: "Rebuild dashboard".into(),
                goal: None,
                next_action: None,
                stage: BoardStage::InProgress,
                signals: Vec::new(),
                external_items: Vec::new(),
                runs: vec![muxa::work::RunSnapshot {
                    id: "tmux:alpha:$1:@1".into(),
                    state: muxa::work::RunState::Running,
                    linked: true,
                    work: Some(identity),
                    execution: muxa::work::ExecutionIdentity {
                        host: HostKind::Tmux,
                        socket: "alpha".into(),
                        session_id: "$1".into(),
                        window_id: "@1".into(),
                    },
                    session_name: "muxa".into(),
                    window_name: "dashboard-v2".into(),
                    window_index: "0".into(),
                    cwd: Some("/tmp/muxa".into()),
                    panes: vec![muxa::work::RunPaneSnapshot {
                        pane_id: "%1".into(),
                        pane_index: "0".into(),
                        current_command: "codex".into(),
                        title: "review controls".into(),
                        current_path: "/tmp/muxa".into(),
                        attach_command: "tmux -L alpha attach".into(),
                        role: Some("reviewer".into()),
                        task: Some("review controls".into()),
                        agent: Some(agent),
                    }],
                    latest_at: Some(now),
                }],
                participants: 1,
                latest_at: Some(now),
                source: muxa::work::WorkSource::Managed,
                metadata: muxa::work::WorkMetadata {
                    title: Some("Rebuild dashboard".into()),
                    goal: None,
                    next_action: None,
                    stage: muxa::work::WorkStage::InProgress,
                    updated_at: now,
                },
            }],
            unlinked_executions: Vec::new(),
        };

        let data = build_work_dashboard_data(
            now,
            snapshot,
            Vec::new(),
            Vec::new(),
            SessionActiveStats::default(),
            DashboardSort::Attention,
            Vec::new(),
        );
        let card = &data.cards[0];
        let panes = live_work_panes(card);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].window.session.endpoint.socket, "alpha");
        assert!(matches!(
            card.action_targets().first(),
            Some(ActionTarget::TopologyPane(key))
                if key.window.session.endpoint.socket == "alpha" && key.pane_id == "%1"
        ));
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
