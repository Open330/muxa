//! `muxa swarm` — a fleet command center.
//!
//! `muxa watch` is a compact picker and `muxa dashboard` is a session-card
//! console. `muxa swarm` steps up a level: it treats every tracked agent as
//! one node in a *swarm*, groups them into clusters (by project or tmux
//! session), and lets the operator dispatch work across the fleet — send a
//! prompt to one agent, or **broadcast** the same instruction to a whole
//! marked squad in a single keystroke.
//!
//! Dispatch reuses the exact same trusted side effects as the other TUIs:
//! tmux `send-keys` for pane agents (via [`crate::watch::dispatch_quick_action`])
//! and IPC `write_session` / `terminate_session` for muxa-owned PTY sessions.
//! Nothing new touches the agents; this module is a router over machinery
//! that already ships.

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa::config::{IconSet, WatchTheme};
use muxa::ipc::Client;
use muxa::{Agent, AgentKind, AgentState, Config, SurfaceKind};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::BTreeSet;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr;

use crate::theme::ThemeArg;
use crate::watch::{self, ActionOutcome, QuickAction, RealEffects};

const INPUT_POLL: Duration = Duration::from_millis(150);
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const HINT_TTL: Duration = Duration::from_secs(4);
/// Fixed cell label width so clusters read as an aligned grid.
const CELL_LABEL_WIDTH: usize = 12;

// ─────────────────────────── CLI surface ───────────────────────────

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Include agents that have no tmux pane or muxa PTY surface. They can't
    /// be dispatched to, so they're hidden by default.
    #[arg(long)]
    include_paneless: bool,

    /// How to cluster the swarm.
    #[arg(long, value_enum, default_value_t = SwarmGroupBy::Project)]
    group_by: SwarmGroupBy,

    /// One-shot visual theme override.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SwarmGroupBy {
    /// Cluster by working-directory project (the cwd basename).
    Project,
    /// Cluster by tmux session (falls back to the PTY/surface id).
    Session,
}

impl SwarmGroupBy {
    fn toggled(self) -> Self {
        match self {
            Self::Project => Self::Session,
            Self::Session => Self::Project,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Session => "session",
        }
    }
}

/// What the parent command should do after the TUI exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    Pane(String),
}

// ─────────────────────────── theme ───────────────────────────

/// muxa's signature brand accent — a violet that unifies the CLI with the
/// `BarShelf` menu-bar widget and sits clear of every state color.
const VIOLET: Color = Color::Rgb(166, 116, 255);

#[derive(Debug, Clone, Copy)]
struct SwarmTheme {
    accent: Color,
    border: Color,
    dim: Color,
    fg: Color,
    selected_fg: Color,
    working: Color,
    waiting: Color,
    choice: Color,
    error: Color,
    idle: Color,
    starting: Color,
    stopped: Color,
}

impl SwarmTheme {
    fn state_color(self, state: AgentState) -> Color {
        match state {
            AgentState::Working => self.working,
            AgentState::WaitingInput => self.waiting,
            AgentState::WaitingChoice => self.choice,
            AgentState::Error => self.error,
            AgentState::Idle => self.idle,
            AgentState::Starting => self.starting,
            AgentState::Stopped => self.stopped,
        }
    }

    fn state_style(self, state: AgentState) -> Style {
        let base = Style::default().fg(self.state_color(state));
        match state {
            AgentState::Error | AgentState::WaitingChoice | AgentState::WaitingInput => {
                base.add_modifier(Modifier::BOLD)
            }
            AgentState::Stopped => base.add_modifier(Modifier::DIM),
            _ => base,
        }
    }
}

fn swarm_theme(theme: WatchTheme) -> SwarmTheme {
    match theme {
        WatchTheme::Mono | WatchTheme::Minimal => SwarmTheme {
            accent: Color::White,
            border: Color::DarkGray,
            dim: Color::DarkGray,
            fg: Color::Gray,
            selected_fg: Color::Black,
            working: Color::White,
            waiting: Color::White,
            choice: Color::White,
            error: Color::White,
            idle: Color::DarkGray,
            starting: Color::Gray,
            stopped: Color::DarkGray,
        },
        WatchTheme::HighContrast => SwarmTheme {
            accent: Color::Yellow,
            border: Color::White,
            dim: Color::Gray,
            fg: Color::White,
            selected_fg: Color::Black,
            working: Color::Green,
            waiting: Color::Yellow,
            choice: Color::Magenta,
            error: Color::Red,
            idle: Color::Gray,
            starting: Color::Cyan,
            stopped: Color::Gray,
        },
        // Every color theme shares the violet brand accent; only the base
        // greys differ, which we intentionally keep neutral here.
        _ => SwarmTheme {
            accent: VIOLET,
            border: Color::Rgb(74, 62, 104),
            dim: Color::DarkGray,
            fg: Color::Gray,
            selected_fg: Color::Black,
            working: Color::Green,
            waiting: Color::Yellow,
            choice: Color::Magenta,
            error: Color::Red,
            idle: Color::DarkGray,
            starting: Color::Cyan,
            stopped: Color::DarkGray,
        },
    }
}

// ─────────────────────────── domain model ───────────────────────────

/// A resolved, actionable destination for a dispatch command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DispatchTarget {
    /// A tmux pane id (e.g. `%42`). Driven by `tmux send-keys` / `kill-pane`.
    Pane(String),
    /// A muxa-owned PTY session id. Driven by IPC `write_session` /
    /// `terminate_session`.
    Pty(String),
}

impl DispatchTarget {
    /// Resolve the best dispatch route for an agent, if any. PTY surfaces win
    /// over the legacy `pane` field so a `pty:*` agent isn't driven as a tmux
    /// pane it doesn't own.
    fn for_agent(agent: &Agent) -> Option<Self> {
        if let Some(surface) = &agent.surface {
            if surface.kind == SurfaceKind::Pty {
                return Some(Self::Pty(surface.id.clone()));
            }
        }
        agent.pane.clone().map(Self::Pane)
    }

    fn label(&self) -> String {
        match self {
            Self::Pane(pane) => pane.clone(),
            Self::Pty(id) => format!("pty:{}", short_id(id)),
        }
    }
}

/// One agent rendered as a swarm node.
#[derive(Debug, Clone)]
struct SwarmCell {
    session_id: String,
    kind: AgentKind,
    state: AgentState,
    label: String,
    target: Option<DispatchTarget>,
    last_prompt: Option<String>,
    last_response: Option<String>,
    model: Option<String>,
    state_since_secs: i64,
    idle_secs: i64,
}

impl SwarmCell {
    fn from_agent(agent: &Agent, now: OffsetDateTime) -> Self {
        let target = DispatchTarget::for_agent(agent);
        let label = target
            .as_ref()
            .map_or_else(|| short_id(&agent.session_id), DispatchTarget::label);
        Self {
            session_id: agent.session_id.clone(),
            kind: agent.kind,
            state: agent.state,
            label,
            target,
            last_prompt: agent.last_prompt.clone(),
            last_response: agent.last_response.clone(),
            model: agent.model.clone(),
            state_since_secs: (now - agent.state_entered_at).whole_seconds().max(0),
            idle_secs: (now - agent.last_activity_at).whole_seconds().max(0),
        }
    }
}

/// A cluster of agents (a "squad").
#[derive(Debug, Clone)]
struct SwarmGroup {
    label: String,
    cells: Vec<SwarmCell>,
}

#[derive(Debug, Clone, Default)]
struct FleetStats {
    total: usize,
    working: usize,
    waiting_input: usize,
    waiting_choice: usize,
    error: usize,
    idle: usize,
    starting: usize,
}

impl FleetStats {
    fn tally(&mut self, state: AgentState) {
        self.total += 1;
        match state {
            AgentState::Working => self.working += 1,
            AgentState::WaitingInput => self.waiting_input += 1,
            AgentState::WaitingChoice => self.waiting_choice += 1,
            AgentState::Error => self.error += 1,
            AgentState::Idle => self.idle += 1,
            AgentState::Starting => self.starting += 1,
            AgentState::Stopped => {}
        }
    }

    fn attention(&self) -> usize {
        self.error + self.waiting_input + self.waiting_choice
    }
}

#[derive(Debug, Clone, Default)]
struct SwarmData {
    groups: Vec<SwarmGroup>,
    fleet: FleetStats,
}

/// Lower is more urgent. Used to float attention-needing agents to the top of
/// their cluster and the most urgent cluster to the top of the board.
fn state_priority(state: AgentState) -> u8 {
    match state {
        AgentState::Error => 0,
        AgentState::WaitingChoice => 1,
        AgentState::WaitingInput => 2,
        AgentState::Working => 3,
        AgentState::Starting => 4,
        AgentState::Idle => 5,
        AgentState::Stopped => 6,
    }
}

/// Build the swarm board from a raw agent snapshot. Pure so it can be tested
/// without a daemon or a terminal.
fn build_swarm_data(agents: &[Agent], group_by: SwarmGroupBy, include_paneless: bool) -> SwarmData {
    let now = OffsetDateTime::now_utc();
    let mut fleet = FleetStats::default();
    let mut buckets: Vec<(String, Vec<SwarmCell>)> = Vec::new();

    for agent in agents {
        let cell = SwarmCell::from_agent(agent, now);
        if cell.target.is_none() && !include_paneless {
            continue;
        }
        fleet.tally(cell.state);
        let key = group_key(agent, group_by);
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, cells)) => cells.push(cell),
            None => buckets.push((key, vec![cell])),
        }
    }

    let mut groups: Vec<SwarmGroup> = buckets
        .into_iter()
        .map(|(label, mut cells)| {
            cells.sort_by(|a, b| {
                state_priority(a.state)
                    .cmp(&state_priority(b.state))
                    .then(a.idle_secs.cmp(&b.idle_secs))
                    .then(a.label.cmp(&b.label))
            });
            SwarmGroup { label, cells }
        })
        .collect();

    groups.sort_by(|a, b| {
        let a_urgency = a.cells.iter().map(|c| state_priority(c.state)).min();
        let b_urgency = b.cells.iter().map(|c| state_priority(c.state)).min();
        a_urgency.cmp(&b_urgency).then(a.label.cmp(&b.label))
    });

    SwarmData { groups, fleet }
}

fn group_key(agent: &Agent, group_by: SwarmGroupBy) -> String {
    match group_by {
        SwarmGroupBy::Project => agent
            .cwd
            .as_deref()
            .map_or_else(|| "(no project)".to_string(), project_name),
        SwarmGroupBy::Session => agent
            .tmux_session
            .clone()
            .or_else(|| {
                agent
                    .surface
                    .as_ref()
                    .map(|s| format!("pty:{}", short_id(&s.id)))
            })
            .unwrap_or_else(|| "(detached)".to_string()),
    }
}

fn project_name(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

/// Last 6 chars of an id — enough to disambiguate at a glance without
/// spending a whole cell on a uuid.
fn short_id(id: &str) -> String {
    let n = id.chars().count();
    if n <= 6 {
        id.to_string()
    } else {
        id.chars().skip(n - 6).collect()
    }
}

// ─────────────────────────── dispatch ───────────────────────────

/// A pending, operator-confirmed command. `Prompt` carries a whole target
/// set so a broadcast is a single action.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingAction {
    Prompt {
        targets: Vec<DispatchTarget>,
        text: String,
    },
    Abort(DispatchTarget),
    Kill(DispatchTarget),
}

/// Resolve the destinations for a broadcast: the marked squad if any agent is
/// marked, otherwise every dispatchable agent currently blocked on human
/// input (the "ready for a fresh instruction" set).
fn broadcast_targets(data: &SwarmData, marked: &BTreeSet<String>) -> Vec<DispatchTarget> {
    data.groups
        .iter()
        .flat_map(|g| g.cells.iter())
        .filter(|c| {
            if marked.is_empty() {
                matches!(
                    c.state,
                    AgentState::WaitingInput | AgentState::WaitingChoice
                )
            } else {
                marked.contains(&c.session_id)
            }
        })
        .filter_map(|c| c.target.clone())
        .collect()
}

async fn run_pending_action(client: &Client, action: PendingAction) -> ActionOutcome {
    match action {
        PendingAction::Prompt { targets, text } => {
            let total = targets.len();
            let mut ok = 0usize;
            let mut last_err = None;
            for target in targets {
                let outcome = match target {
                    DispatchTarget::Pane(pane) => {
                        let mut fx = RealEffects;
                        watch::dispatch_quick_action(
                            QuickAction::SendPrompt {
                                pane_id: pane,
                                text: text.clone(),
                            },
                            &mut fx,
                        )
                    }
                    DispatchTarget::Pty(session) => pty_prompt(client, &session, &text).await,
                };
                match outcome {
                    ActionOutcome::Ok(_) => ok += 1,
                    ActionOutcome::Err(e) => last_err = Some(e),
                    ActionOutcome::HelpToggled => {}
                }
            }
            summarize("prompt", ok, total, last_err)
        }
        PendingAction::Abort(target) => match target {
            DispatchTarget::Pane(pane) => {
                let mut fx = RealEffects;
                watch::dispatch_quick_action(QuickAction::AbortTurn(pane), &mut fx)
            }
            DispatchTarget::Pty(session) => match client.write_session(&session, "\u{3}").await {
                Ok(()) => ActionOutcome::Ok(format!("✔ sent Ctrl-C to pty:{}", short_id(&session))),
                Err(e) => ActionOutcome::Err(format!("✗ abort failed: {e}")),
            },
        },
        PendingAction::Kill(target) => match target {
            DispatchTarget::Pane(pane) => {
                let mut fx = RealEffects;
                watch::dispatch_quick_action(QuickAction::KillPane(pane), &mut fx)
            }
            DispatchTarget::Pty(session) => match client.terminate_session(&session).await {
                Ok(()) => ActionOutcome::Ok(format!("✔ terminated pty:{}", short_id(&session))),
                Err(e) => ActionOutcome::Err(format!("✗ terminate failed: {e}")),
            },
        },
    }
}

async fn pty_prompt(client: &Client, session: &str, text: &str) -> ActionOutcome {
    match client.write_session(session, &format!("{text}\r")).await {
        Ok(()) => ActionOutcome::Ok(format!("✔ sent prompt to pty:{}", short_id(session))),
        Err(e) => ActionOutcome::Err(format!("✗ send failed: {e}")),
    }
}

fn summarize(verb: &str, ok: usize, total: usize, last_err: Option<String>) -> ActionOutcome {
    if ok == total {
        if total == 1 {
            ActionOutcome::Ok(format!("✔ {verb} sent"))
        } else {
            ActionOutcome::Ok(format!("✔ {verb} broadcast to {ok} agents"))
        }
    } else if ok == 0 {
        ActionOutcome::Err(format!(
            "✗ {verb} failed for all {total}: {}",
            last_err.unwrap_or_else(|| "unknown error".to_string())
        ))
    } else {
        ActionOutcome::Err(format!("⚠ {verb}: {ok}/{total} ok, {} failed", total - ok))
    }
}

// ─────────────────────────── app state ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Composer,
    Confirm,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintLevel {
    Ok,
    Err,
    Info,
}

struct Composer {
    buffer: String,
    title: String,
    targets: Vec<DispatchTarget>,
}

struct ConfirmPrompt {
    message: String,
    action: PendingAction,
}

struct SwarmApp {
    data: SwarmData,
    theme: SwarmTheme,
    group_by: SwarmGroupBy,
    /// Flat display order as `(group_idx, cell_idx)` for linear navigation.
    order: Vec<(usize, usize)>,
    selected: usize,
    marked: BTreeSet<String>,
    mode: Mode,
    composer: Composer,
    confirm: Option<ConfirmPrompt>,
    hint: Option<(String, HintLevel, Instant)>,
}

/// What the run loop should do after a key press.
enum Signal {
    None,
    Quit,
    Open(OpenTarget),
    Run(PendingAction),
    Refresh,
}

impl SwarmApp {
    fn new(data: SwarmData, theme: SwarmTheme, group_by: SwarmGroupBy) -> Self {
        let mut app = Self {
            data,
            theme,
            group_by,
            order: Vec::new(),
            selected: 0,
            marked: BTreeSet::new(),
            mode: Mode::Normal,
            composer: Composer {
                buffer: String::new(),
                title: String::new(),
                targets: Vec::new(),
            },
            confirm: None,
            hint: None,
        };
        app.rebuild_order(None);
        app
    }

    /// Recompute the flat navigation order, keeping the cursor on the same
    /// agent across a data refresh where possible.
    fn rebuild_order(&mut self, keep: Option<&str>) {
        let order: Vec<(usize, usize)> = self
            .data
            .groups
            .iter()
            .enumerate()
            .flat_map(|(gi, g)| (0..g.cells.len()).map(move |ci| (gi, ci)))
            .collect();
        self.order = order;
        // Drop marks for agents that vanished.
        let live: BTreeSet<String> = self
            .data
            .groups
            .iter()
            .flat_map(|g| g.cells.iter().map(|c| c.session_id.clone()))
            .collect();
        self.marked.retain(|id| live.contains(id));
        if let Some(id) = keep {
            if let Some(idx) = self
                .order
                .iter()
                .position(|&(gi, ci)| self.data.groups[gi].cells[ci].session_id == id)
            {
                self.selected = idx;
                return;
            }
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.order.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.order.len() {
            self.selected = self.order.len() - 1;
        }
    }

    fn selected_cell(&self) -> Option<&SwarmCell> {
        self.order
            .get(self.selected)
            .map(|&(gi, ci)| &self.data.groups[gi].cells[ci])
    }

    fn selected_session_id(&self) -> Option<String> {
        self.selected_cell().map(|c| c.session_id.clone())
    }

    fn select_step(&mut self, forward: bool) {
        if self.order.is_empty() {
            return;
        }
        let len = self.order.len();
        self.selected = if forward {
            (self.selected + 1) % len
        } else {
            (self.selected + len - 1) % len
        };
    }

    /// Jump to the first cell of the next / previous group.
    fn move_group(&mut self, forward: bool) {
        let Some(&(cur_group, _)) = self.order.get(self.selected) else {
            return;
        };
        let group_count = self.data.groups.len();
        if group_count == 0 {
            return;
        }
        let target_group = if forward {
            (cur_group + 1) % group_count
        } else {
            (cur_group + group_count - 1) % group_count
        };
        if let Some(idx) = self.order.iter().position(|&(gi, _)| gi == target_group) {
            self.selected = idx;
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(id) = self.selected_session_id() {
            if !self.marked.remove(&id) {
                self.marked.insert(id);
            }
        }
    }

    fn mark_group(&mut self) {
        let Some(&(gi, _)) = self.order.get(self.selected) else {
            return;
        };
        for cell in &self.data.groups[gi].cells {
            if cell.target.is_some() {
                self.marked.insert(cell.session_id.clone());
            }
        }
    }

    fn set_hint(&mut self, msg: impl Into<String>, level: HintLevel) {
        self.hint = Some((msg.into(), level, Instant::now()));
    }

    fn apply_outcome(&mut self, outcome: ActionOutcome) {
        match outcome {
            ActionOutcome::Ok(m) => self.set_hint(m, HintLevel::Ok),
            ActionOutcome::Err(m) => self.set_hint(m, HintLevel::Err),
            ActionOutcome::HelpToggled => {}
        }
    }

    fn open_composer(&mut self, targets: Vec<DispatchTarget>, title: String) {
        self.composer.buffer.clear();
        self.composer.title = title;
        self.composer.targets = targets;
        self.mode = Mode::Composer;
    }
}

// ─────────────────────────── input handling ───────────────────────────

fn handle_key(app: &mut SwarmApp, key: KeyEvent) -> Signal {
    match app.mode {
        Mode::Composer => handle_composer_key(app, key),
        Mode::Confirm => handle_confirm_key(app, key),
        Mode::Help => {
            app.mode = Mode::Normal;
            Signal::None
        }
        Mode::Normal => handle_normal_key(app, key),
    }
}

fn handle_normal_key(app: &mut SwarmApp, key: KeyEvent) -> Signal {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Signal::Quit,
        KeyCode::Char('j') | KeyCode::Down | KeyCode::Right => app.select_step(true),
        KeyCode::Char('k') | KeyCode::Up | KeyCode::Left => app.select_step(false),
        KeyCode::Tab => app.move_group(true),
        KeyCode::BackTab => app.move_group(false),
        KeyCode::Char(' ') => app.toggle_mark(),
        KeyCode::Char('m') => app.mark_group(),
        KeyCode::Char('c') => {
            app.marked.clear();
            app.set_hint("cleared marks", HintLevel::Info);
        }
        KeyCode::Char('g') => {
            app.group_by = app.group_by.toggled();
            app.set_hint(
                format!("grouping by {}", app.group_by.label()),
                HintLevel::Info,
            );
            return Signal::Refresh;
        }
        KeyCode::Char('r') => return Signal::Refresh,
        KeyCode::Char('?') => app.mode = Mode::Help,
        KeyCode::Char('p') | KeyCode::Enter => return compose_single(app),
        KeyCode::Char('b') => return compose_broadcast(app),
        KeyCode::Char('o') => return open_selected(app),
        KeyCode::Char('x') => return confirm_selected(app, false),
        KeyCode::Char('K') => return confirm_selected(app, true),
        _ => {}
    }
    Signal::None
}

fn compose_single(app: &mut SwarmApp) -> Signal {
    let Some(target) = app.selected_cell().and_then(|c| c.target.clone()) else {
        app.set_hint("selected agent has no dispatch target", HintLevel::Err);
        return Signal::None;
    };
    let title = format!("prompt → {}", target.label());
    app.open_composer(vec![target], title);
    Signal::None
}

fn compose_broadcast(app: &mut SwarmApp) -> Signal {
    let targets = broadcast_targets(&app.data, &app.marked);
    if targets.is_empty() {
        app.set_hint(
            "no broadcast targets — mark agents with Space, or wait for input-blocked agents",
            HintLevel::Err,
        );
        return Signal::None;
    }
    let scope = if app.marked.is_empty() {
        "input-blocked".to_string()
    } else {
        format!("{} marked", app.marked.len())
    };
    let title = format!("broadcast → {} agents ({scope})", targets.len());
    app.open_composer(targets, title);
    Signal::None
}

fn open_selected(app: &mut SwarmApp) -> Signal {
    let target = app.selected_cell().and_then(|c| c.target.clone());
    match target {
        Some(DispatchTarget::Pane(pane)) => Signal::Open(OpenTarget::Pane(pane)),
        Some(DispatchTarget::Pty(_)) => {
            app.set_hint(
                "jump is tmux-only; PTY sessions use `muxa attach`",
                HintLevel::Info,
            );
            Signal::None
        }
        None => {
            app.set_hint("selected agent has no pane to jump to", HintLevel::Err);
            Signal::None
        }
    }
}

fn confirm_selected(app: &mut SwarmApp, kill: bool) -> Signal {
    let Some(target) = app.selected_cell().and_then(|c| c.target.clone()) else {
        app.set_hint("selected agent has no dispatch target", HintLevel::Err);
        return Signal::None;
    };
    let label = target.label();
    let (message, action) = if kill {
        (
            format!("Terminate {label}? This kills the pane/process."),
            PendingAction::Kill(target),
        )
    } else {
        (
            format!("Abort current turn on {label} (send Ctrl-C)?"),
            PendingAction::Abort(target),
        )
    };
    app.confirm = Some(ConfirmPrompt { message, action });
    app.mode = Mode::Confirm;
    Signal::None
}

fn handle_composer_key(app: &mut SwarmApp, key: KeyEvent) -> Signal {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            Signal::None
        }
        KeyCode::Enter => {
            let text = app.composer.buffer.trim().to_string();
            if text.is_empty() {
                app.set_hint("empty prompt — nothing sent", HintLevel::Info);
                app.mode = Mode::Normal;
                return Signal::None;
            }
            let targets = std::mem::take(&mut app.composer.targets);
            app.mode = Mode::Normal;
            Signal::Run(PendingAction::Prompt { targets, text })
        }
        KeyCode::Backspace => {
            app.composer.buffer.pop();
            Signal::None
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.composer.buffer.clear();
            Signal::None
        }
        KeyCode::Char(ch) => {
            app.composer.buffer.push(ch);
            Signal::None
        }
        _ => Signal::None,
    }
}

fn handle_confirm_key(app: &mut SwarmApp, key: KeyEvent) -> Signal {
    match key.code {
        KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
            app.mode = Mode::Normal;
            if let Some(confirm) = app.confirm.take() {
                Signal::Run(confirm.action)
            } else {
                Signal::None
            }
        }
        _ => {
            app.confirm = None;
            app.mode = Mode::Normal;
            Signal::None
        }
    }
}

// ─────────────────────────── glyphs ───────────────────────────

fn state_glyph(state: AgentState) -> &'static str {
    match crate::icon_set() {
        IconSet::Unicode => match state {
            AgentState::Working => "●",
            AgentState::WaitingInput => "▶",
            AgentState::WaitingChoice => "◆",
            AgentState::Error => "■",
            AgentState::Idle => "○",
            AgentState::Starting => "◌",
            AgentState::Stopped => "×",
        },
        IconSet::Ascii => match state {
            AgentState::Working => "*",
            AgentState::WaitingInput => ">",
            AgentState::WaitingChoice => "?",
            AgentState::Error => "!",
            AgentState::Idle => "o",
            AgentState::Starting => "~",
            AgentState::Stopped => "x",
        },
    }
}

fn brand_glyph() -> &'static str {
    match crate::icon_set() {
        IconSet::Unicode => "▦",
        IconSet::Ascii => "#",
    }
}

fn mark_glyph() -> &'static str {
    match crate::icon_set() {
        IconSet::Unicode => "◍",
        IconSet::Ascii => "+",
    }
}

fn kind_tag(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::GeminiCli => "gemini",
        AgentKind::Opencode => "opencode",
        AgentKind::Task => "task",
        AgentKind::Unknown => "agent",
    }
}

// ─────────────────────────── rendering ───────────────────────────

fn render(f: &mut Frame, app: &SwarmApp) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header + fleet
            Constraint::Min(3),    // swarm board
            Constraint::Length(4), // selection detail
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_header(f, chunks[0], app);
    render_board(f, chunks[1], app);
    render_detail(f, chunks[2], app);
    render_footer(f, chunks[3], app);

    match app.mode {
        Mode::Composer => render_composer(f, area, app),
        Mode::Confirm => render_confirm(f, area, app),
        Mode::Help => render_help(f, area, app),
        Mode::Normal => {}
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &SwarmApp) {
    let t = app.theme;
    let brand = Line::from(vec![
        Span::styled(
            format!(" {} ", brand_glyph()),
            Style::default()
                .fg(t.selected_fg)
                .bg(t.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " muxa ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "swarm",
            Style::default().fg(t.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  ·  {} agents in {} clusters",
                app.data.fleet.total,
                app.data.groups.len()
            ),
            Style::default().fg(t.dim),
        ),
    ]);

    let fleet = &app.data.fleet;
    let mut spans = vec![Span::styled(" fleet  ", Style::default().fg(t.dim))];
    for (state, count) in [
        (AgentState::Working, fleet.working),
        (AgentState::WaitingInput, fleet.waiting_input),
        (AgentState::WaitingChoice, fleet.waiting_choice),
        (AgentState::Error, fleet.error),
        (AgentState::Starting, fleet.starting),
        (AgentState::Idle, fleet.idle),
    ] {
        spans.push(Span::styled(
            format!("{} {}  ", state_glyph(state), count),
            t.state_style(state),
        ));
    }
    let attention = fleet.attention();
    if attention > 0 {
        spans.push(Span::styled(
            format!(" {attention} need you "),
            Style::default()
                .fg(Color::Black)
                .bg(t.waiting)
                .add_modifier(Modifier::BOLD),
        ));
    }

    f.render_widget(Paragraph::new(vec![brand, Line::from(spans)]), area);
}

fn render_board(f: &mut Frame, area: Rect, app: &SwarmApp) {
    let t = app.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.border))
        .title(Span::styled(
            format!(" swarm map · by {} ", app.group_by.label()),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.order.is_empty() {
        f.render_widget(
            Paragraph::new(Text::from(vec![Line::from(Span::styled(
                "  no dispatchable agents — start an agent, or pass --include-paneless",
                Style::default().fg(t.dim),
            ))])),
            inner,
        );
        return;
    }

    let width = inner.width.max(1) as usize;
    // chip = " " + mark + glyph + " " + label(padded) + " " → CELL_LABEL_WIDTH + 5,
    // plus a one-column gap between chips.
    let chip_w = CELL_LABEL_WIDTH + 5;
    let per_row = (width.saturating_sub(2) / (chip_w + 1)).max(1);

    let mut lines: Vec<Line> = Vec::new();
    let mut selected_row: usize = 0;

    for (gi, group) in app.data.groups.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("▸ {}", group.label),
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({})", group.cells.len()),
                Style::default().fg(t.dim),
            ),
        ]));

        for chunk_start in (0..group.cells.len()).step_by(per_row) {
            let mut spans: Vec<Span> = vec![Span::raw("  ")];
            for ci in chunk_start..(chunk_start + per_row).min(group.cells.len()) {
                let cell = &group.cells[ci];
                let is_selected = app.order.get(app.selected) == Some(&(gi, ci));
                if is_selected {
                    selected_row = lines.len();
                }
                spans.extend(chip_spans(cell, app, is_selected));
                spans.push(Span::raw(" "));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
    }

    let height = inner.height as usize;
    let scroll =
        u16::try_from(selected_row.saturating_sub(height.saturating_sub(1))).unwrap_or(u16::MAX);
    f.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), inner);
}

fn chip_spans(cell: &SwarmCell, app: &SwarmApp, selected: bool) -> Vec<Span<'static>> {
    let t = app.theme;
    let marked = app.marked.contains(&cell.session_id);
    let mark = if marked { mark_glyph() } else { " " };
    let label = pad_label(&cell.label, CELL_LABEL_WIDTH);
    let text = format!("{mark}{} {label}", state_glyph(cell.state));

    let style = if selected {
        Style::default()
            .fg(t.selected_fg)
            .bg(t.accent)
            .add_modifier(Modifier::BOLD)
    } else if marked {
        t.state_style(cell.state).add_modifier(Modifier::UNDERLINED)
    } else {
        t.state_style(cell.state)
    };
    vec![Span::styled(format!(" {text} "), style)]
}

fn render_detail(f: &mut Frame, area: Rect, app: &SwarmApp) {
    let t = app.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.border))
        .title(Span::styled(" selected ", Style::default().fg(t.dim)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(cell) = app.selected_cell() else {
        return;
    };

    let head = Line::from(vec![
        Span::styled(
            format!("{} ", state_glyph(cell.state)),
            t.state_style(cell.state),
        ),
        Span::styled(
            cell.target
                .as_ref()
                .map_or_else(|| short_id(&cell.session_id), DispatchTarget::label),
            Style::default().fg(t.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", kind_tag(cell.kind)),
            Style::default().fg(t.accent),
        ),
        Span::styled(format!("  {}", cell.state), t.state_style(cell.state)),
        Span::styled(
            format!("  {}", fmt_since(cell.state_since_secs)),
            Style::default().fg(t.dim),
        ),
        Span::styled(
            cell.model
                .as_deref()
                .map_or_else(String::new, |m| format!("  {m}")),
            Style::default().fg(t.dim),
        ),
    ]);

    let detail_text = cell
        .last_response
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or(cell.last_prompt.as_deref())
        .unwrap_or("(no prompt captured)");
    let body = Line::from(Span::styled(
        format!(
            "  ↳ {}",
            truncate(detail_text.trim(), inner.width.saturating_sub(4) as usize)
        ),
        Style::default().fg(t.dim),
    ));

    f.render_widget(Paragraph::new(Text::from(vec![head, body])), inner);
}

fn render_footer(f: &mut Frame, area: Rect, app: &SwarmApp) {
    let t = app.theme;
    if let Some((msg, level, at)) = &app.hint {
        if at.elapsed() < HINT_TTL {
            let color = match level {
                HintLevel::Ok => t.working,
                HintLevel::Err => t.error,
                HintLevel::Info => t.accent,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {msg}"),
                    Style::default().fg(color),
                ))),
                area,
            );
            return;
        }
    }
    let marked = app.marked.len();
    let keys = format!(
        " enter/p prompt · b broadcast{} · space mark · x abort · K kill · o jump · g group · ? help · q quit",
        if marked > 0 { format!(" ({marked})") } else { String::new() }
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(keys, Style::default().fg(t.dim)))),
        area,
    );
}

fn render_composer(f: &mut Frame, area: Rect, app: &SwarmApp) {
    let t = app.theme;
    let popup = centered_rect_by_size(area.width.saturating_sub(8).min(90), 7, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(Span::styled(
            format!(" {} ", app.composer.title),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let text = Text::from(vec![
        Line::from(Span::styled(
            format!("{}▏", app.composer.buffer),
            Style::default().fg(t.fg),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter send · Ctrl-U clear · Esc cancel",
            Style::default().fg(t.dim),
        )),
    ]);
    f.render_widget(Paragraph::new(text), inner);
}

fn render_confirm(f: &mut Frame, area: Rect, app: &SwarmApp) {
    let t = app.theme;
    let Some(confirm) = &app.confirm else {
        return;
    };
    let popup = centered_rect_by_size(70, 6, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.error))
        .title(Span::styled(
            " confirm ",
            Style::default().fg(t.error).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    f.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                confirm.message.clone(),
                Style::default().fg(t.fg),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "y confirm · any other key cancel",
                Style::default().fg(t.dim),
            )),
        ])),
        inner,
    );
}

fn render_help(f: &mut Frame, area: Rect, app: &SwarmApp) {
    let t = app.theme;
    let popup = centered_rect_by_size(64, 20, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(Span::styled(
            " muxa swarm — keys ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let lines: Vec<Line> = help_text()
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::default().fg(t.fg))))
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn help_text() -> Vec<&'static str> {
    vec![
        "  Navigation",
        "    j/k ↑/↓ ←/→   move between agents",
        "    Tab / S-Tab    jump between clusters",
        "",
        "  Dispatch",
        "    Enter / p      compose a prompt for the selected agent",
        "    b              broadcast a prompt to the squad",
        "    Space          mark / unmark the selected agent",
        "    m              mark every agent in this cluster",
        "    c              clear all marks",
        "    x              abort the current turn (Ctrl-C, confirm)",
        "    K              terminate the pane / process (confirm)",
        "    o              jump to the selected tmux pane",
        "",
        "  View",
        "    g              toggle grouping (project ↔ session)",
        "    r              refresh · ? help · q quit",
    ]
}

// ─────────────────────────── small helpers ───────────────────────────

fn pad_label(label: &str, width: usize) -> String {
    let truncated = truncate(label, width);
    let w = truncated.width();
    if w >= width {
        truncated
    } else {
        format!("{truncated}{}", " ".repeat(width - w))
    }
}

/// Width-aware truncation with a trailing ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.to_string().width();
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

fn fmt_since(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
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

// ─────────────────────────── terminal + run loop ───────────────────────────

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
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
            let _ = terminal.show_cursor();
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enabling raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
        .map_err(|e| {
            let _ = disable_raw_mode();
            e
        })
        .context("entering alternate terminal screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
        .inspect_err(|_| {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
            let _ = disable_raw_mode();
        })
        .context("initializing terminal")
}

async fn load_swarm_data(client: &Client, args: &Args) -> Result<SwarmData> {
    let agents = client
        .snapshot()
        .await
        .context("querying daemon agent snapshot")?;
    Ok(build_swarm_data(
        &agents,
        args.group_by,
        args.include_paneless,
    ))
}

pub async fn run(client: &Client, cfg: &Config, mut args: Args) -> Result<Option<OpenTarget>> {
    let initial = load_swarm_data(client, &args).await?;
    let theme = swarm_theme(args.theme.map_or(cfg.ui.theme, WatchTheme::from));
    let mut app = SwarmApp::new(initial, theme, args.group_by);

    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    let mut last_refresh = Instant::now();

    let result = loop {
        guard
            .terminal_mut()
            .draw(|f| render(f, &app))
            .map_err(anyhow::Error::from)?;

        if event::poll(INPUT_POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match handle_key(&mut app, key) {
                        Signal::None => {}
                        Signal::Quit => break Ok(None),
                        Signal::Open(target) => break Ok(Some(target)),
                        Signal::Refresh => {
                            args.group_by = app.group_by;
                            refresh(&mut app, client, &args).await;
                            last_refresh = Instant::now();
                        }
                        Signal::Run(action) => {
                            let outcome = run_pending_action(client, action).await;
                            app.apply_outcome(outcome);
                            refresh(&mut app, client, &args).await;
                            last_refresh = Instant::now();
                        }
                    }
                }
                Event::Paste(data) if app.mode == Mode::Composer => {
                    app.composer
                        .buffer
                        .push_str(data.trim_end_matches(['\n', '\r']));
                }
                _ => {}
            }
        }

        if app.mode == Mode::Normal && last_refresh.elapsed() >= REFRESH_INTERVAL {
            refresh(&mut app, client, &args).await;
            last_refresh = Instant::now();
        }
    };

    drop(guard);
    result
}

async fn refresh(app: &mut SwarmApp, client: &Client, args: &Args) {
    match load_swarm_data(client, args).await {
        Ok(data) => {
            let keep = app.selected_session_id();
            app.data = data;
            app.rebuild_order(keep.as_deref());
        }
        Err(e) => app.set_hint(format!("refresh failed: {e}"), HintLevel::Err),
    }
}

// ─────────────────────────── tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::event::SurfaceRef;

    fn agent(session: &str, state: AgentState, pane: Option<&str>, cwd: Option<&str>) -> Agent {
        let now = OffsetDateTime::now_utc();
        Agent {
            kind: AgentKind::ClaudeCode,
            session_id: session.to_string(),
            surface: None,
            pane: pane.map(str::to_string),
            tmux_socket: None,
            tmux_session: None,
            cwd: cwd.map(str::to_string),
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            state,
            last_prompt: None,
            last_response: None,
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

    #[test]
    fn fleet_stats_tally_and_attention() {
        let agents = vec![
            agent("a", AgentState::Working, Some("%1"), Some("/home/u/proj")),
            agent(
                "b",
                AgentState::WaitingInput,
                Some("%2"),
                Some("/home/u/proj"),
            ),
            agent("c", AgentState::Error, Some("%3"), Some("/home/u/other")),
            agent("d", AgentState::Idle, Some("%4"), Some("/home/u/other")),
        ];
        let data = build_swarm_data(&agents, SwarmGroupBy::Project, false);
        assert_eq!(data.fleet.total, 4);
        assert_eq!(data.fleet.working, 1);
        assert_eq!(data.fleet.attention(), 2); // waiting_input + error
    }

    #[test]
    fn paneless_agents_hidden_unless_requested() {
        let agents = vec![
            agent("a", AgentState::Working, Some("%1"), None),
            agent("b", AgentState::Working, None, None), // paneless
        ];
        assert_eq!(
            build_swarm_data(&agents, SwarmGroupBy::Project, false)
                .fleet
                .total,
            1
        );
        assert_eq!(
            build_swarm_data(&agents, SwarmGroupBy::Project, true)
                .fleet
                .total,
            2
        );
    }

    #[test]
    fn groups_by_project_basename() {
        let agents = vec![
            agent("a", AgentState::Idle, Some("%1"), Some("/home/u/alpha")),
            agent("b", AgentState::Idle, Some("%2"), Some("/home/u/alpha")),
            agent("c", AgentState::Idle, Some("%3"), Some("/home/u/beta")),
        ];
        let data = build_swarm_data(&agents, SwarmGroupBy::Project, false);
        assert_eq!(data.groups.len(), 2);
        let alpha = data.groups.iter().find(|g| g.label == "alpha").unwrap();
        assert_eq!(alpha.cells.len(), 2);
    }

    #[test]
    fn most_urgent_group_sorts_first() {
        let agents = vec![
            agent("a", AgentState::Idle, Some("%1"), Some("/home/u/calm")),
            agent("b", AgentState::Error, Some("%2"), Some("/home/u/onfire")),
        ];
        let data = build_swarm_data(&agents, SwarmGroupBy::Project, false);
        assert_eq!(data.groups[0].label, "onfire");
    }

    #[test]
    fn broadcast_defaults_to_input_blocked_when_no_marks() {
        let agents = vec![
            agent("a", AgentState::Working, Some("%1"), Some("/p")),
            agent("b", AgentState::WaitingInput, Some("%2"), Some("/p")),
            agent("c", AgentState::WaitingChoice, Some("%3"), Some("/p")),
        ];
        let data = build_swarm_data(&agents, SwarmGroupBy::Project, false);
        let targets = broadcast_targets(&data, &BTreeSet::new());
        assert_eq!(targets.len(), 2); // b + c, not the working one
    }

    #[test]
    fn broadcast_uses_marked_set_when_present() {
        let agents = vec![
            agent("a", AgentState::Working, Some("%1"), Some("/p")),
            agent("b", AgentState::WaitingInput, Some("%2"), Some("/p")),
        ];
        let data = build_swarm_data(&agents, SwarmGroupBy::Project, false);
        let mut marked = BTreeSet::new();
        marked.insert("a".to_string());
        let targets = broadcast_targets(&data, &marked);
        assert_eq!(targets, vec![DispatchTarget::Pane("%1".to_string())]);
    }

    #[test]
    fn pty_surface_beats_pane_for_dispatch() {
        let mut a = agent("a", AgentState::Working, Some("%1"), None);
        a.surface = Some(SurfaceRef {
            kind: SurfaceKind::Pty,
            id: "pty:abc123".to_string(),
        });
        assert_eq!(
            DispatchTarget::for_agent(&a),
            Some(DispatchTarget::Pty("pty:abc123".to_string()))
        );
    }

    #[test]
    fn truncate_is_width_aware() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn fmt_since_scales_units() {
        assert_eq!(fmt_since(5), "5s");
        assert_eq!(fmt_since(120), "2m");
        assert_eq!(fmt_since(7200), "2h");
        assert_eq!(fmt_since(172_800), "2d");
    }
}
